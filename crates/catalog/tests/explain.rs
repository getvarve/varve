//! EXPLAIN against the M2 database matrix: PostgreSQL 13 and 18, MySQL 8.0
//! and 9.7, MariaDB 10.6 and 12.3. Each test creates its own `m2_explain_*`
//! schema or database, drops it first in case an earlier run failed, and
//! drops it at the end.
//!
//! Run with `cargo test -p varve-catalog --test explain -- --include-ignored`.

use std::error::Error;

use varve_catalog::CatalogError;
use varve_catalog::explain::{
    AnalyzeSafety, ExplainFormat, ExplainOptions, NodeWarning, Plan, PlanFormat, PlanNode,
    ShareBasis, analyze_safety, explain,
};
use varve_catalog::rows;
use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, Session, TlsMode};
use varve_driver_mysql::MysqlDriver;
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn Error>>;

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

async fn postgres(port_var: &str, default: u16) -> Result<Box<dyn Session>, Box<dyn Error>> {
    let mut params = ConnectParams::new(
        Engine::Postgres,
        env_or("VARVE_M2_HOST", "127.0.0.1"),
        port(port_var, default),
    );
    params.username = Some(env_or("VARVE_M2_PG_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_M2_PG_PASSWORD", "varve_dev")));
    params.database = Some(env_or("VARVE_M2_PG_DATABASE", "varve"));
    params.tls.mode = TlsMode::Disable;
    params.application_name = "varve-catalog explain tests".into();
    Ok(PostgresDriver::new().connect(&params).await?)
}

async fn mysql(
    engine: Engine,
    port_var: &str,
    default: u16,
) -> Result<Box<dyn Session>, Box<dyn Error>> {
    let mut params = ConnectParams::new(
        engine,
        env_or("VARVE_M2_HOST", "127.0.0.1"),
        port(port_var, default),
    );
    params.username = Some(env_or("VARVE_M2_MYSQL_USER", "root"));
    params.password = Some(Secret::new(env_or("VARVE_M2_MYSQL_PASSWORD", "varve_dev")));
    params.tls.mode = TlsMode::Disable;
    params.application_name = "varve-catalog explain tests".into();
    Ok(MysqlDriver::new().connect(&params).await?)
}

async fn run(session: &dyn Session, sql: &str) -> Result<(), Box<dyn Error>> {
    rows::run(session, sql).await?;
    Ok(())
}

async fn int(session: &dyn Session, sql: &str) -> Result<i64, Box<dyn Error>> {
    let rows = rows::query(session, sql).await?;
    let row = rows.single("value")?;
    let column = rows
        .columns()
        .first()
        .ok_or("the query returned no column")?
        .clone();
    Ok(row.int(&column)?.ok_or("NULL")?)
}

fn nodes(node: &PlanNode) -> Vec<&PlanNode> {
    let mut out = vec![node];
    for child in &node.children {
        out.extend(nodes(child));
    }
    out
}

fn has_warning(plan: &Plan, check: impl Fn(&NodeWarning) -> bool) -> bool {
    nodes(&plan.root)
        .iter()
        .any(|n| n.warnings.iter().any(&check))
}

/// Shares of a plan with a basis add up to 1, ids are a depth-first walk.
fn check_plan(plan: &Plan) {
    let all = nodes(&plan.root);
    for (i, node) in all.iter().enumerate() {
        assert_eq!(node.id as usize, i);
    }
    if plan.share_basis != ShareBasis::None {
        let total: f64 = all.iter().filter_map(|n| n.share).sum();
        assert!((total - 1.0).abs() < 1e-6, "shares add up to {total}");
    }
    assert!(!plan.raw.is_empty());
}

const ANALYZE: ExplainOptions = ExplainOptions {
    analyze: true,
    buffers: false,
    verbose: false,
    settings: false,
    wal: false,
    format: None,
    confirmed: false,
};

/// `ANALYZE` the user confirmed: statements that change data always need
/// it, they cannot run in a read-only transaction.
const CONFIRMED: ExplainOptions = ExplainOptions {
    confirmed: true,
    ..ANALYZE
};

// PostgreSQL

const PG_SETUP: &str = "
DROP SCHEMA IF EXISTS m2_explain_it CASCADE;
CREATE SCHEMA m2_explain_it;
CREATE TABLE m2_explain_it.customers (id int PRIMARY KEY, name text, country char(2));
CREATE TABLE m2_explain_it.orders (
    id int PRIMARY KEY,
    customer_id int NOT NULL REFERENCES m2_explain_it.customers(id),
    amount numeric(10,2),
    created date
);
CREATE INDEX ON m2_explain_it.orders (customer_id);
CREATE TABLE m2_explain_it.audit (id serial PRIMARY KEY, order_id int);
CREATE FUNCTION m2_explain_it.audit_fn() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN INSERT INTO m2_explain_it.audit(order_id) VALUES (NEW.id); RETURN NEW; END $$;
CREATE TRIGGER orders_audit AFTER UPDATE ON m2_explain_it.orders
    FOR EACH ROW EXECUTE FUNCTION m2_explain_it.audit_fn();
INSERT INTO m2_explain_it.customers
    SELECT n, 'c' || n, CASE WHEN n % 3 = 0 THEN 'NL' ELSE 'DE' END
    FROM generate_series(1, 1000) n;
INSERT INTO m2_explain_it.orders
    SELECT n, 1 + n % 1000, n % 97, date '2026-01-01' + (n % 300)
    FROM generate_series(1, 150000) n;
ANALYZE m2_explain_it.customers;
ANALYZE m2_explain_it.orders;
";

const PG_QUERY: &str = "
WITH big AS MATERIALIZED (
    SELECT customer_id, sum(amount) AS total FROM m2_explain_it.orders GROUP BY customer_id
)
SELECT c.country, count(*) AS n, sum(b.total) AS total,
       (SELECT max(amount) FROM m2_explain_it.orders) AS max_amount
FROM m2_explain_it.customers c
JOIN big b ON b.customer_id = c.id
WHERE c.id IN (SELECT customer_id FROM m2_explain_it.orders WHERE amount > 90)
GROUP BY c.country
ORDER BY total DESC";

async fn postgres_suite(session: &dyn Session) -> TestResult {
    let server = session.server_info().clone();
    run(session, PG_SETUP).await?;

    // Estimates: cost hot spots, no actual values.
    let plan = explain(session, &server, PG_QUERY, &ExplainOptions::default()).await?;
    check_plan(&plan);
    assert_eq!(plan.format, PlanFormat::PostgresJson);
    assert!(!plan.analyzed);
    assert_eq!(plan.share_basis, ShareBasis::Cost);
    assert!(plan.total_cost.is_some_and(|c| c > 0.0));
    let all = nodes(&plan.root);
    assert!(all.iter().any(|n| n.operation == "CTE Scan"));
    assert!(
        all.iter()
            .any(|n| n.detail.subplan.as_deref() == Some("CTE big"))
    );

    // ANALYZE with every option.
    let options = ExplainOptions {
        analyze: true,
        buffers: true,
        verbose: true,
        settings: true,
        wal: false,
        format: Some(ExplainFormat::Json),
        confirmed: false,
    };
    let plan = explain(session, &server, PG_QUERY, &options).await?;
    check_plan(&plan);
    assert!(plan.analyzed);
    assert_eq!(plan.share_basis, ShareBasis::Time);
    assert!(plan.execution_ms.is_some() && plan.planning_ms.is_some());
    assert!(plan.root.buffers.is_some());
    assert!(!plan.root.detail.output.is_empty());
    assert!(
        nodes(&plan.root)
            .iter()
            .any(|n| n.detail.relationship.as_deref() == Some("InitPlan"))
    );

    // A full scan of the 150 000 row table is flagged.
    let plan = explain(
        session,
        &server,
        "SELECT * FROM m2_explain_it.orders WHERE created > date '2026-02-01'",
        &ANALYZE,
    )
    .await?;
    assert!(has_warning(&plan, |w| matches!(
        w,
        NodeWarning::SeqScanLargeTable { relation: Some(r), .. } if r == "orders"
    )));

    // DML under ANALYZE runs, fires its trigger, and is rolled back.
    let before = int(
        session,
        "SELECT sum(amount)::bigint FROM m2_explain_it.orders",
    )
    .await?;
    let update = "UPDATE m2_explain_it.orders SET amount = amount + 1000 WHERE customer_id = 5";
    assert!(matches!(
        analyze_safety(&server, update),
        AnalyzeSafety::RolledBack { .. }
    ));
    // Unconfirmed it is refused and never runs.
    assert!(matches!(
        explain(session, &server, update, &ANALYZE).await,
        Err(CatalogError::NeedsConfirmation { .. })
    ));
    let options = ExplainOptions {
        wal: true,
        buffers: true,
        ..CONFIRMED
    };
    let plan = explain(session, &server, update, &options).await?;
    check_plan(&plan);
    assert_eq!(plan.root.operation, "Update");
    assert_eq!(plan.triggers.len(), 1);
    assert_eq!(plan.triggers[0].name, "orders_audit");
    assert_eq!(plan.triggers[0].calls, Some(150));
    assert!(plan.root.properties.iter().any(|p| p.name == "WAL Records"));
    assert!(!plan.warnings.is_empty());
    assert_eq!(
        int(
            session,
            "SELECT sum(amount)::bigint FROM m2_explain_it.orders"
        )
        .await?,
        before
    );
    assert_eq!(
        int(session, "SELECT count(*) FROM m2_explain_it.audit").await?,
        0
    );
    // The session is not left inside a transaction.
    assert_eq!(
        int(
            session,
            "SELECT (transaction_timestamp() <> statement_timestamp())::int"
        )
        .await?,
        0
    );

    // Inside the user's own transaction a savepoint is used: the user's
    // change stays, the explained one is undone.
    run(session, "BEGIN").await?;
    run(
        session,
        "INSERT INTO m2_explain_it.audit(order_id) VALUES (-1)",
    )
    .await?;
    explain(
        session,
        &server,
        "DELETE FROM m2_explain_it.audit WHERE order_id = -1",
        &CONFIRMED,
    )
    .await?;
    assert_eq!(
        int(
            session,
            "SELECT count(*) FROM m2_explain_it.audit WHERE order_id = -1"
        )
        .await?,
        1
    );
    run(session, "ROLLBACK").await?;
    assert_eq!(
        int(session, "SELECT count(*) FROM m2_explain_it.audit").await?,
        0
    );

    // A failing statement is rolled back too and the session stays usable.
    let failing = "INSERT INTO m2_explain_it.orders VALUES (1, 1, 1, now())";
    assert!(
        explain(session, &server, failing, &CONFIRMED)
            .await
            .is_err()
    );
    assert_eq!(int(session, "SELECT 1").await?, 1);

    refusals(session, &server, "CREATE TABLE m2_explain_it.x (a int)").await?;
    postgres_attacks(session, &server).await?;
    run(session, "DROP SCHEMA m2_explain_it CASCADE").await?;
    Ok(())
}

/// Every way of reaching the server with more than the one statement, with
/// and without ANALYZE: each call is refused and the table is unchanged.
async fn refused_everywhere(
    session: &dyn Session,
    server: &varve_driver::ServerInfo,
    texts: &[&str],
    count_sql: &str,
) -> TestResult {
    let count = int(session, count_sql).await?;
    for text in texts {
        for options in [ExplainOptions::default(), ANALYZE] {
            let result = explain(session, server, text, &options).await;
            assert!(
                matches!(result, Err(CatalogError::Invalid { .. })),
                "{text:?} with analyze={}: {result:?}",
                options.analyze
            );
        }
        assert!(
            matches!(analyze_safety(server, text), AnalyzeSafety::Refused { .. }),
            "{text:?}"
        );
        assert_eq!(
            int(session, count_sql).await?,
            count,
            "{text:?} changed the table"
        );
    }
    Ok(())
}

async fn postgres_attacks(session: &dyn Session, server: &varve_driver::ServerInfo) -> TestResult {
    let count = "SELECT count(*) FROM m2_explain_it.orders";
    refused_everywhere(
        session,
        server,
        &[
            "SELECT 1 --1\n; DELETE FROM m2_explain_it.orders",
            "SELECT 1; DELETE FROM m2_explain_it.orders",
        ],
        count,
    )
    .await?;

    // `a$b$` is an identifier, not a dollar quote: the DELETE in the WITH is
    // seen, and the run is rolled back.
    let before = int(session, count).await?;
    let cte = "WITH a$b$ AS (DELETE FROM m2_explain_it.orders WHERE id <= 10 RETURNING 1) \
               SELECT count(*) FROM a$b$";
    assert!(matches!(
        analyze_safety(server, cte),
        AnalyzeSafety::RolledBack { .. }
    ));
    let plan = explain(session, server, cte, &CONFIRMED).await?;
    assert!(plan.analyzed);
    assert_eq!(int(session, count).await?, before);

    // A side-effecting function needs confirmation; unconfirmed it never
    // runs, so the advisory lock is not taken.
    let lock = "SELECT pg_advisory_lock(424242)";
    let held = "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND objid = 424242 \
                AND pid = pg_backend_pid()";
    assert!(matches!(
        explain(session, server, lock, &ANALYZE).await,
        Err(CatalogError::NeedsConfirmation { .. })
    ));
    assert_eq!(int(session, held).await?, 0);
    let confirmed = ExplainOptions {
        confirmed: true,
        ..ANALYZE
    };
    let plan = explain(session, server, lock, &confirmed).await?;
    assert!(plan.warnings.iter().any(|w| w.contains("pg_advisory_lock")));
    // A session-level advisory lock outlives the rollback, as warned.
    assert_eq!(int(session, held).await?, 1);
    run(session, "SELECT pg_advisory_unlock_all()").await?;
    Ok(())
}

async fn mysql_attacks(session: &dyn Session, server: &varve_driver::ServerInfo) -> TestResult {
    let texts = [
        "SELECT 1 --1; DELETE FROM orders",
        "SELECT 1 /*! ; DELETE FROM orders */",
        "SELECT 1 /*!50000 ; DELETE FROM orders */",
        "SELECT 1 /*M! ; DELETE FROM orders */",
        "SELECT 'a\\'; DELETE FROM orders; SELECT '",
    ];
    refused_everywhere(session, server, &texts, COUNT).await?;
    // The same with the server not treating backslashes as escapes, where the
    // last text really is three statements.
    let mode = rows::query(session, "SELECT @@SESSION.sql_mode AS m").await?;
    let mode = mode.single("sql_mode")?.text("m")?.unwrap_or_default();
    run(
        session,
        "SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, ',NO_BACKSLASH_ESCAPES')",
    )
    .await?;
    let result = refused_everywhere(session, server, &texts, COUNT).await;
    run(
        session,
        &format!(
            "SET SESSION sql_mode = {}",
            varve_catalog::sql::string(varve_driver::Dialect::Mysql, &mode)?
        ),
    )
    .await?;
    result?;

    // GET_LOCK needs confirmation; unconfirmed it never runs.
    let lock = "SELECT GET_LOCK('m2_explain_lock', 0)";
    let held = "SELECT IFNULL(IS_USED_LOCK('m2_explain_lock') = CONNECTION_ID(), 0) AS held";
    let result = explain(session, server, lock, &ANALYZE).await;
    assert!(
        matches!(result, Err(CatalogError::NeedsConfirmation { .. })),
        "{result:?}"
    );
    assert_eq!(int(session, held).await?, 0);
    let confirmed = ExplainOptions {
        confirmed: true,
        ..ANALYZE
    };
    let plan = explain(session, server, lock, &confirmed).await?;
    assert!(plan.warnings.iter().any(|w| w.contains("get_lock")));
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("non-transactional"))
    );
    run(session, "DO RELEASE_ALL_LOCKS()").await?;
    Ok(())
}

async fn refusals(
    session: &dyn Session,
    server: &varve_driver::ServerInfo,
    ddl: &str,
) -> TestResult {
    assert!(matches!(
        explain(session, server, ddl, &ANALYZE).await,
        Err(CatalogError::Invalid { .. })
    ));
    assert!(matches!(
        explain(
            session,
            server,
            "SELECT 1; SELECT 2",
            &ExplainOptions::default()
        )
        .await,
        Err(CatalogError::Invalid { .. })
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn postgres_13() -> TestResult {
    let session = postgres("VARVE_M2_PG_13_PORT", 55513).await?;
    assert_eq!(session.server_info().major, 13);
    let result = postgres_suite(session.as_ref()).await;
    let _ = rows::run(
        session.as_ref(),
        "DROP SCHEMA IF EXISTS m2_explain_it CASCADE",
    )
    .await;
    result
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn postgres_18() -> TestResult {
    let session = postgres("VARVE_M2_PG_18_PORT", 55518).await?;
    assert_eq!(session.server_info().major, 18);
    let result = postgres_suite(session.as_ref()).await;
    let _ = rows::run(
        session.as_ref(),
        "DROP SCHEMA IF EXISTS m2_explain_it CASCADE",
    )
    .await;
    result
}

// MySQL and MariaDB

fn my_setup(engine: Engine) -> String {
    let depth = if engine == Engine::Mariadb {
        "SET SESSION max_recursive_iterations = 100000"
    } else {
        "SET SESSION cte_max_recursion_depth = 100000"
    };
    format!(
        "
DROP DATABASE IF EXISTS m2_explain_it;
CREATE DATABASE m2_explain_it;
USE m2_explain_it;
CREATE TABLE customers (id INT PRIMARY KEY, name VARCHAR(50), country CHAR(2), KEY idx_country (country)) ENGINE=InnoDB;
CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT NOT NULL, amount DECIMAL(10,2), created DATE, KEY idx_customer (customer_id)) ENGINE=InnoDB;
{depth};
INSERT INTO customers WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 200)
    SELECT n, CONCAT('c', n), IF(n % 3 = 0, 'NL', 'DE') FROM s;
INSERT INTO orders WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 3000)
    SELECT n, 1 + n % 200, n % 97, DATE_ADD('2026-01-01', INTERVAL n % 300 DAY) FROM s;
ANALYZE TABLE customers, orders"
    )
}

const MY_QUERY: &str = "
WITH big AS (SELECT customer_id, SUM(amount) AS total FROM orders GROUP BY customer_id)
SELECT c.country, COUNT(*) AS n, SUM(b.total) AS total, (SELECT MAX(amount) FROM orders) AS mx
FROM customers c
JOIN big b ON b.customer_id = c.id
WHERE c.id IN (SELECT customer_id FROM orders WHERE amount > 90)
GROUP BY c.country
ORDER BY total DESC";

const SUM: &str = "SELECT CAST(SUM(amount) AS SIGNED) AS s FROM orders";
const COUNT: &str = "SELECT COUNT(*) AS n FROM orders";

async fn mysql_suite(session: &dyn Session) -> TestResult {
    let server = session.server_info().clone();
    let mariadb = server.engine == Engine::Mariadb;
    run(session, &my_setup(server.engine)).await?;

    // Estimates in JSON (format version 1 on MySQL 8.0, 2 on 9.x).
    let plan = explain(session, &server, MY_QUERY, &ExplainOptions::default()).await?;
    check_plan(&plan);
    assert!(!plan.analyzed);
    assert!(nodes(&plan.root).len() > 4);
    assert!(
        nodes(&plan.root)
            .iter()
            .any(|n| n.detail.relation.as_deref() == Some("orders"))
    );
    if !mariadb {
        assert_eq!(plan.format, PlanFormat::MysqlJson);
        assert_eq!(plan.share_basis, ShareBasis::Cost);
        // The tree format, estimates only.
        let tree = ExplainOptions {
            format: Some(ExplainFormat::Tree),
            ..ExplainOptions::default()
        };
        let plan = explain(session, &server, MY_QUERY, &tree).await?;
        check_plan(&plan);
        assert_eq!(plan.format, PlanFormat::MysqlTree);
        assert!(!plan.analyzed);
        assert!(plan.root.operation.starts_with("Sort"));
    } else {
        assert_eq!(plan.format, PlanFormat::MariadbJson);
    }

    // ANALYZE: tree text on MySQL, JSON on MariaDB.
    let plan = explain(session, &server, MY_QUERY, &ANALYZE).await?;
    check_plan(&plan);
    assert!(plan.analyzed);
    assert_eq!(plan.share_basis, ShareBasis::Time);
    assert!(plan.execution_ms.is_some_and(|t| t > 0.0));
    assert!(
        nodes(&plan.root)
            .iter()
            .any(|n| n.loops.is_some_and(|l| l > 1))
    );
    if server.at_least(8, 3) && !mariadb {
        // EXPLAIN ANALYZE FORMAT=JSON (format version 2, the 9.x default).
        let json = ExplainOptions {
            format: Some(ExplainFormat::Json),
            ..ANALYZE
        };
        let plan = explain(session, &server, MY_QUERY, &json).await?;
        check_plan(&plan);
        assert_eq!(plan.format, PlanFormat::MysqlJson);
        assert!(plan.analyzed);
    }

    // DML under ANALYZE is rolled back. MySQL measures INSERT ... SELECT;
    // MariaDB measures UPDATE.
    let sum = int(session, SUM).await?;
    let count = int(session, COUNT).await?;
    let insert = "INSERT INTO orders SELECT id + 100000, customer_id, amount, created FROM orders WHERE id <= 50";
    let plan = explain(session, &server, insert, &CONFIRMED).await?;
    check_plan(&plan);
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("non-transactional"))
    );
    if !mariadb {
        assert_eq!(plan.root.operation, "Insert into orders");
        assert!(plan.analyzed);
    }
    let update = "UPDATE orders SET amount = amount + 1000 WHERE customer_id = 5";
    let plan = explain(session, &server, update, &CONFIRMED).await?;
    check_plan(&plan);
    if mariadb {
        assert!(plan.analyzed);
        assert!(
            nodes(&plan.root)
                .iter()
                .any(|n| n.operation.starts_with("Update orders") && n.actual_rows == Some(15.0))
        );
    }
    assert_eq!(int(session, COUNT).await?, count);
    assert_eq!(int(session, SUM).await?, sum);

    // Inside the user's transaction a savepoint keeps the user's change.
    run(session, "START TRANSACTION").await?;
    run(session, "DELETE FROM orders WHERE id = 1").await?;
    explain(
        session,
        &server,
        "DELETE FROM orders WHERE id = 2",
        &CONFIRMED,
    )
    .await?;
    explain(session, &server, insert, &CONFIRMED).await?;
    assert_eq!(int(session, COUNT).await?, count - 1);
    assert_eq!(
        int(session, "SELECT COUNT(*) AS n FROM orders WHERE id = 2").await?,
        1
    );
    run(session, "ROLLBACK").await?;
    assert_eq!(int(session, COUNT).await?, count);

    // With autocommit off as well.
    run(session, "SET autocommit = 0").await?;
    run(session, "DELETE FROM orders WHERE id = 1").await?;
    explain(session, &server, insert, &CONFIRMED).await?;
    assert_eq!(int(session, COUNT).await?, count - 1);
    run(session, "ROLLBACK").await?;
    run(session, "SET autocommit = 1").await?;
    assert_eq!(int(session, COUNT).await?, count);

    refusals(session, &server, "CREATE TABLE x (a INT)").await?;
    mysql_attacks(session, &server).await?;
    run(session, "DROP DATABASE m2_explain_it").await?;
    Ok(())
}

async fn mysql_target(engine: Engine, port_var: &str, default: u16, major: u32) -> TestResult {
    let session = mysql(engine, port_var, default).await?;
    assert_eq!(session.server_info().engine, engine);
    assert_eq!(session.server_info().major, major);
    let result = mysql_suite(session.as_ref()).await;
    let _ = rows::run(session.as_ref(), "DROP DATABASE IF EXISTS m2_explain_it").await;
    result
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mysql_8_0() -> TestResult {
    mysql_target(Engine::Mysql, "VARVE_M2_MYSQL_8_0_PORT", 35080, 8).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mysql_9_7() -> TestResult {
    mysql_target(Engine::Mysql, "VARVE_M2_MYSQL_9_7_PORT", 35097, 9).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mariadb_10_6() -> TestResult {
    mysql_target(Engine::Mariadb, "VARVE_M2_MARIADB_10_6_PORT", 35106, 10).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mariadb_12_3() -> TestResult {
    mysql_target(Engine::Mariadb, "VARVE_M2_MARIADB_12_3_PORT", 35123, 12).await
}
