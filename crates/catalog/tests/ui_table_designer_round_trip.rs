//! Table designer round trip through the UI, against the shared M2 database
//! matrix (PostgreSQL 13 and 18, MySQL 8.0, 8.4 and 9.7, MariaDB 10.6 and 12.3):
//!
//! ```sh
//! cargo test -p varve-catalog --test ui_table_designer_round_trip -- --include-ignored
//! ```
//!
//! The UI test `packages/ui/src/components/table/designerRoundTrip.test.ts`
//! builds, with the designer's own code, the changes for new partitioned
//! tables (range, list by an expression, hash, each with its partitions or
//! default partition), partitions attached and detached, every table
//! property on a new and on an existing table (PostgreSQL: owner, UNLOGGED,
//! INHERITS, storage parameters, tablespace, comment, rename, SET SCHEMA;
//! MySQL and MariaDB: engine, row format, AUTO_INCREMENT, character set and
//! collation, CONVERT TO, comment, rename) and a unique index on a
//! materialized view, PostgreSQL constraint comments (on a new table, then
//! removed, changed with a rename and set on a new constraint), and MySQL / MariaDB INVISIBLE columns and ON UPDATE
//! CURRENT_TIMESTAMP set at creation and changed on an existing table, and
//! MySQL / MariaDB range partitioning with HASH subpartitions, then a
//! partition split and a merge with REORGANIZE PARTITION, and a LIST
//! partition split that keeps every row (the reorganize that would drop a
//! value is refused by the designer and planned as destructive). It keeps them in
//! `packages/ui/src/components/table/__roundtrip__/<engine>.json`. Here
//! every step is planned and run in order, and the catalog is read back
//! (`describe`, as the UI reads it) to prove the result.
//!
//! The storage engine list ([`varve_catalog::mysql::engines`]) is checked on
//! every MySQL and MariaDB server too.
//!
//! Everything is named `m2_td*` and dropped at the start and the end of a run.
//! The capped row count check runs in a database of its own per run
//! (`m2_fin_rc_<pid>_<nanoseconds>`), so concurrent runs cannot collide there.

use std::error::Error;
use std::path::PathBuf;

use serde_json::Value as Json;
use varve_catalog::model::{Catalog, ObjectKind, ObjectRef};
use varve_catalog::mysql::edit::MyChange;
use varve_catalog::open_catalog;
use varve_catalog::postgres::edit::PgChange;
use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, ExecOptions, ExecOutput, Session, StatementResult};
use varve_driver_mysql::MysqlDriver;
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const SCHEMA: &str = "m2_td";
const SCHEMA_B: &str = "m2_td_b";
const ROLE: &str = "m2_td_owner";
const MY_DB: &str = "m2_td";

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

/// The steps the UI test wrote for `engine` (`postgres`, `mysql`, `mariadb`).
fn steps(engine: &str) -> TestResult<Vec<(String, Vec<Json>)>> {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/ui/src/components/table/__roundtrip__")
        .join(format!("{engine}.json"));
    let text = std::fs::read_to_string(&file).map_err(|e| {
        format!(
            "{}: {e} (the UI test components/table/designerRoundTrip.test.ts writes it)",
            file.display()
        )
    })?;
    let list: Vec<Json> = serde_json::from_str(&text)?;
    list.into_iter()
        .map(|step| {
            let label = step["label"]
                .as_str()
                .ok_or("a step without a label")?
                .to_owned();
            let changes = step["changes"]
                .as_array()
                .ok_or("a step without changes")?
                .clone();
            Ok((label, changes))
        })
        .collect()
}

async fn exec(session: &dyn Session, sql: &str) -> TestResult<ExecOutput> {
    session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}\n=> {e}").into())
}

/// Runs a planned change as the app does: a transactional plan with more
/// than one statement in one transaction, others statement by statement.
async fn run_plan(session: &dyn Session, statements: &[String], transactional: bool) -> TestResult {
    if transactional && statements.len() > 1 {
        exec(session, "BEGIN").await?;
        for statement in statements {
            if let Err(e) = exec(session, statement).await {
                exec(session, "ROLLBACK").await?;
                return Err(e);
            }
        }
        exec(session, "COMMIT").await?;
    } else {
        for statement in statements {
            exec(session, statement).await?;
        }
    }
    Ok(())
}

fn object(kind: ObjectKind, database: &str, schema: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind,
        database: Some(database.into()),
        schema: schema.map(Into::into),
        parent: None,
        name: name.into(),
        signature: None,
    }
}

/// `describe` as JSON: the object, as the UI receives it.
async fn describe(
    catalog: &dyn Catalog,
    session: &dyn Session,
    object: &ObjectRef,
) -> TestResult<Json> {
    let details = serde_json::to_value(catalog.describe(session, object).await?)?;
    Ok(details["object"].clone())
}

/// The text of column `v` of the single row `sql` returns.
async fn scalar(session: &dyn Session, sql: &str) -> TestResult<String> {
    let rows = varve_catalog::rows::query(session, sql).await?;
    let row = rows.single("the value")?;
    Ok(row.text("v")?.ok_or("NULL")?)
}

/// The column names of the first row set of `output`.
fn column_names(output: &ExecOutput) -> Vec<String> {
    output
        .results
        .iter()
        .find_map(|r| match r {
            StatementResult::Rows { columns, .. } => {
                Some(columns.iter().map(|c| c.name.clone()).collect())
            }
            StatementResult::Command { .. } => None,
        })
        .unwrap_or_default()
}

/// `(name, comment)` of a described table's constraints other than NOT NULL, by name.
fn constraint_comments(table: &Json) -> Vec<(String, Json)> {
    let mut out: Vec<(String, Json)> = table["constraints"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter(|k| k["constraintType"] != "notNull")
                .filter_map(|k| Some((k["name"].as_str()?.to_owned(), k["comment"].clone())))
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// `(partition, number of subpartitions)` of a described MySQL / MariaDB table, in partition order.
fn partition_layout(table: &Json) -> Vec<(String, usize)> {
    table["partitioning"]["partitions"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|p| {
                    Some((
                        p["name"].as_str()?.to_owned(),
                        p["subpartitions"].as_array().map_or(0, Vec::len),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn names(list: &Json, key: &str) -> Vec<String> {
    let mut out: Vec<String> = list
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x[key].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// PostgreSQL

async fn pg_connect(port: u16) -> TestResult<Box<dyn Session>> {
    let mut params = ConnectParams::new(
        Engine::Postgres,
        env_or("VARVE_M2_PG_HOST", "127.0.0.1"),
        port,
    );
    params.database = Some("varve".to_owned());
    params.username = Some(env_or("VARVE_M2_PG_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_M2_PG_PASSWORD", "varve_dev")));
    params.application_name = "varve-catalog ui_table_designer_round_trip tests".into();
    Ok(PostgresDriver::new().connect(&params).await?)
}

async fn pg_cleanup(session: &dyn Session) -> TestResult {
    exec(session, &format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE")).await?;
    exec(
        session,
        &format!("DROP SCHEMA IF EXISTS {SCHEMA_B} CASCADE"),
    )
    .await?;
    exec(session, &format!("DROP ROLE IF EXISTS {ROLE}")).await?;
    Ok(())
}

/// The fixtures the UI test's details mirror.
fn pg_fixtures() -> Vec<String> {
    vec![
        format!("CREATE ROLE {ROLE} NOLOGIN"),
        format!("CREATE SCHEMA {SCHEMA}"),
        format!("CREATE SCHEMA {SCHEMA_B}"),
        format!("CREATE TABLE {SCHEMA}.base (id bigint NOT NULL)"),
        format!(
            "CREATE UNLOGGED TABLE {SCHEMA}.orders (id bigint NOT NULL, note text) \
             WITH (fillfactor = 90, autovacuum_enabled = false)"
        ),
        format!("CREATE TABLE {SCHEMA}.events_old (id bigint NOT NULL, at timestamptz NOT NULL)"),
        format!("INSERT INTO {SCHEMA}.events_old VALUES (1500, '2025-06-01')"),
        format!("CREATE TABLE {SCHEMA}.days (day date)"),
        format!("INSERT INTO {SCHEMA}.days VALUES ('2026-10-01'), ('2026-10-02'), ('2026-10-02')"),
        format!("CREATE MATERIALIZED VIEW {SCHEMA}.mv AS SELECT DISTINCT day FROM {SCHEMA}.days"),
    ]
}

async fn pg_round_trip(port: u16) -> TestResult {
    let session = pg_connect(port).await?;
    let session = session.as_ref();
    pg_cleanup(session).await?;
    let result = pg_run(session).await;
    let cleaned = pg_cleanup(session).await;
    result?;
    cleaned?;
    Ok(())
}

async fn pg_run(session: &dyn Session) -> TestResult {
    for sql in pg_fixtures() {
        exec(session, &sql).await?;
    }
    let catalog = open_catalog(session).await?;
    let catalog = catalog.as_ref();
    let db = "varve";
    let major = session.server_info().major;
    for (label, changes) in steps("postgres")? {
        for change in changes {
            assert_eq!(change["engine"], "postgres", "{label}");
            let change: PgChange = serde_json::from_value(change["change"].clone())?;
            let plan = varve_catalog::postgres::edit::plan(session.server_info(), &change)
                .map_err(|e| format!("{label}: plan: {e}"))?;
            run_plan(session, &plan.statements, plan.transactional)
                .await
                .map_err(|e| format!("{label} on PostgreSQL {major}: {e}"))?;
        }
        let table = |name: &str| object(ObjectKind::Table, db, Some(SCHEMA), name);
        match label.as_str() {
            "create range partitioned events" => {
                let t = describe(catalog, session, &table("events")).await?;
                assert_eq!(t["kind"], "partitionedTable");
                assert_eq!(
                    t["partitioning"]["strategy"]
                        .as_str()
                        .map(str::to_lowercase)
                        .as_deref(),
                    Some("range")
                );
                assert_eq!(t["partitioning"]["key"], "RANGE (id, at)");
                assert_eq!(
                    names(&t["partitioning"]["partitions"], "name"),
                    ["events_default"]
                );
                assert_eq!(
                    t["partitioning"]["defaultPartition"]["name"],
                    "events_default"
                );
                assert_eq!(t["owner"], ROLE);
                assert_eq!(t["comment"], "events by time");
                // The identity column fills rows written through the parent (into the default
                // partition); removed again, so the next partitions do not collide with it.
                exec(
                    session,
                    &format!("INSERT INTO {SCHEMA}.events (at) VALUES ('2026-10-01')"),
                )
                .await?;
                assert_eq!(
                    scalar(
                        session,
                        &format!("SELECT id::text AS v FROM {SCHEMA}.events_default")
                    )
                    .await?,
                    "1"
                );
                exec(session, &format!("DELETE FROM {SCHEMA}.events")).await?;
            }
            "partitions of events" => {
                let t = describe(catalog, session, &table("events")).await?;
                assert_eq!(
                    names(&t["partitioning"]["partitions"], "name"),
                    ["events_default", "events_low", "events_old"]
                );
                let low = describe(
                    catalog,
                    session,
                    &object(ObjectKind::Partition, db, Some(SCHEMA), "events_low"),
                )
                .await?;
                assert_eq!(low["partitionOf"]["parent"]["name"], "events");
                assert!(
                    low["partitionOf"]["bound"].as_str().is_some_and(
                        |b| b.starts_with("FOR VALUES FROM (MINVALUE, MINVALUE) TO ('1000',")
                    ),
                    "{}",
                    low["partitionOf"]["bound"]
                );
            }
            "detach events_old" => {
                let t = describe(catalog, session, &table("events")).await?;
                assert_eq!(
                    names(&t["partitioning"]["partitions"], "name"),
                    ["events_default", "events_low"]
                );
                let old = describe(catalog, session, &table("events_old")).await?;
                assert_eq!(old["kind"], "table");
                assert!(old["partitionOf"].is_null());
            }
            "create list partitioned regions" => {
                let t = describe(catalog, session, &table("regions")).await?;
                assert_eq!(
                    t["partitioning"]["strategy"]
                        .as_str()
                        .map(str::to_lowercase)
                        .as_deref(),
                    Some("list")
                );
                assert_eq!(t["partitioning"]["key"], "LIST (lower(code))");
                assert_eq!(
                    names(&t["partitioning"]["partitions"], "name"),
                    ["regions_default"]
                );
            }
            "partition of regions" => {
                let t = describe(catalog, session, &table("regions")).await?;
                assert_eq!(
                    names(&t["partitioning"]["partitions"], "name"),
                    ["regions_default", "regions_eu"]
                );
                exec(
                    session,
                    &format!("INSERT INTO {SCHEMA}.regions (code) VALUES ('NL')"),
                )
                .await?;
                assert_eq!(
                    scalar(
                        session,
                        &format!("SELECT count(*)::text AS v FROM {SCHEMA}.regions_eu")
                    )
                    .await?,
                    "1"
                );
            }
            "create hash partitioned spread" => {
                let t = describe(catalog, session, &table("spread")).await?;
                assert_eq!(
                    t["partitioning"]["strategy"]
                        .as_str()
                        .map(str::to_lowercase)
                        .as_deref(),
                    Some("hash")
                );
                assert!(t["partitioning"]["defaultPartition"].is_null());
            }
            "partitions of spread" => {
                let t = describe(catalog, session, &table("spread")).await?;
                assert_eq!(
                    names(&t["partitioning"]["partitions"], "name"),
                    ["spread_0", "spread_1"]
                );
            }
            "create plain with properties" => {
                let t = describe(catalog, session, &table("plain")).await?;
                assert_eq!(t["unlogged"], true);
                assert_eq!(t["owner"], ROLE);
                assert_eq!(t["comment"], "plain table");
                assert_eq!(
                    t["inherits"],
                    serde_json::json!([{ "schema": SCHEMA, "name": "base" }])
                );
                assert_eq!(
                    t["storageParameters"],
                    serde_json::json!([{ "name": "fillfactor", "value": "80" }])
                );
                assert!(
                    t["tablespace"].is_null(),
                    "pg_default is the database default"
                );
            }
            "orders properties" => {
                let moved = object(ObjectKind::Table, db, Some(SCHEMA_B), "orders_moved");
                let t = describe(catalog, session, &moved).await?;
                assert_eq!(t["unlogged"], false);
                assert_eq!(t["owner"], ROLE);
                assert_eq!(t["comment"], "orders, renamed");
                assert_eq!(
                    t["inherits"],
                    serde_json::json!([{ "schema": SCHEMA, "name": "base" }])
                );
                assert_eq!(
                    t["storageParameters"],
                    serde_json::json!([{ "name": "fillfactor", "value": "70" }])
                );
                assert!(describe(catalog, session, &table("orders")).await.is_err());
            }
            "events_low to m2_td_b" => {
                let low = describe(
                    catalog,
                    session,
                    &object(ObjectKind::Partition, db, Some(SCHEMA_B), "events_low"),
                )
                .await?;
                assert_eq!(low["schema"], SCHEMA_B);
                assert_eq!(low["partitionOf"]["parent"]["name"], "events");
            }
            "create noted with constraint comments" => {
                let t = describe(catalog, session, &table("noted")).await?;
                assert_eq!(
                    constraint_comments(&t),
                    [
                        ("noted_pkey".to_owned(), Json::from("the key")),
                        ("noted_positive".to_owned(), Json::from("it's positive")),
                    ],
                    "PostgreSQL {major}"
                );
            }
            "noted constraint comments changed" => {
                let t = describe(catalog, session, &table("noted")).await?;
                assert_eq!(
                    constraint_comments(&t),
                    [
                        ("noted_pkey".to_owned(), Json::Null),
                        (
                            "noted_pos".to_owned(),
                            Json::from("renamed, still positive")
                        ),
                        ("noted_small".to_owned(), Json::from("below a million")),
                    ],
                    "PostgreSQL {major}"
                );
            }
            "unique index on mv" => {
                let mv = object(ObjectKind::MaterializedView, db, Some(SCHEMA), "mv");
                let v = describe(catalog, session, &mv).await?;
                assert_eq!(names(&v["indexes"], "name"), ["mv_day"]);
                assert_eq!(v["indexes"][0]["unique"], true);
                assert_eq!(v["refreshConcurrently"], true);
                exec(
                    session,
                    &format!("REFRESH MATERIALIZED VIEW CONCURRENTLY {SCHEMA}.mv"),
                )
                .await?;
            }
            other => return Err(format!("no check for step {other}").into()),
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn table_designer_round_trips_on_postgres_13() -> TestResult {
    pg_round_trip(port("VARVE_M2_PG13_PORT", 55513)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn table_designer_round_trips_on_postgres_18() -> TestResult {
    pg_round_trip(port("VARVE_M2_PG18_PORT", 55518)).await
}

// ---------------------------------------------------------------------------
// MySQL and MariaDB

async fn my_connect(engine: Engine, port: u16) -> TestResult<Box<dyn Session>> {
    let mut params = ConnectParams::new(engine, env_or("VARVE_M2_MYSQL_HOST", "127.0.0.1"), port);
    params.username = Some(env_or("VARVE_M2_MYSQL_USER", "root"));
    params.password = Some(Secret::new(env_or("VARVE_M2_MYSQL_PASSWORD", "varve_dev")));
    params.tls.mode = varve_driver::TlsMode::Disable;
    params.application_name = "varve-catalog ui_table_designer_round_trip tests".into();
    Ok(MysqlDriver::new().connect(&params).await?)
}

async fn my_round_trip(engine: Engine, port: u16) -> TestResult {
    let session = my_connect(engine, port).await?;
    let session = session.as_ref();
    row_count_cap(session).await?;
    exec(session, &format!("DROP DATABASE IF EXISTS {MY_DB}")).await?;
    let result = my_run(session, engine == Engine::Mariadb).await;
    let cleaned = exec(session, &format!("DROP DATABASE IF EXISTS {MY_DB}")).await;
    result?;
    cleaned?;
    Ok(())
}

/// A database name no other run uses at the same time: `m2_fin_rc_<pid>_<nanoseconds>`.
fn unique_db() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    format!("m2_fin_rc_{}_{nanos}", std::process::id())
}

/// TD2-M1: the designer's exact row count gives up (`None`) when counting
/// takes longer than its cap, instead of keeping the review waiting.
///
/// Deterministic on every server: the view `sleepy` sleeps one second per
/// row (ten rows), so the count can never finish within the 200 ms cap, and
/// the cap is long enough that the server's statement timer is armed while
/// the statement executes. (A large cross join is not slow on every server:
/// MySQL 8.4 counts it from the table sizes at once.) The fixtures live in a
/// database of their own per run, dropped on success and on failure.
async fn row_count_cap(session: &dyn Session) -> TestResult {
    let db = unique_db();
    exec(session, &format!("CREATE DATABASE `{db}`")).await?;
    let result = row_count_cap_in(session, &db).await;
    let cleaned = exec(session, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
    result?;
    cleaned?;
    Ok(())
}

async fn row_count_cap_in(session: &dyn Session, db: &str) -> TestResult {
    let server = session.server_info().version.clone();
    exec(
        session,
        &format!("CREATE TABLE `{db}`.nums (id int PRIMARY KEY)"),
    )
    .await?;
    exec(
        session,
        &format!("INSERT INTO `{db}`.nums VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)"),
    )
    .await?;
    exec(
        session,
        &format!("CREATE VIEW `{db}`.sleepy AS SELECT id FROM `{db}`.nums WHERE SLEEP(1) = 0"),
    )
    .await?;
    let started = std::time::Instant::now();
    assert_eq!(
        varve_catalog::mysql::row_count(session, db, "sleepy", 200).await?,
        None,
        "{server}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{server}: {:?}",
        started.elapsed()
    );
    assert_eq!(
        varve_catalog::mysql::row_count(session, db, "nums", 2000).await?,
        Some(10),
        "{server}"
    );
    assert!(
        varve_catalog::mysql::row_count(session, db, "missing", 2000)
            .await
            .is_err()
    );
    Ok(())
}

async fn my_run(session: &dyn Session, mariadb: bool) -> TestResult {
    let server = session.server_info().version.clone();

    // The engine picker's list.
    let engines = varve_catalog::mysql::engines(session).await?;
    let default: Vec<&str> = engines
        .iter()
        .filter(|e| e.default)
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(default, ["InnoDB"], "{server}: {engines:?}");
    assert!(
        engines.iter().any(|e| e.name == "MyISAM"),
        "{server}: {engines:?}"
    );
    assert_eq!(
        engines.iter().any(|e| e.name == "Aria"),
        mariadb,
        "{server}: {engines:?}"
    );
    assert!(
        !engines
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case("PERFORMANCE_SCHEMA"))
    );
    let memory = engines
        .iter()
        .find(|e| e.name == "MEMORY")
        .ok_or("no MEMORY")?;
    assert!(
        memory
            .warning
            .as_deref()
            .is_some_and(|w| w.contains("server restarts")),
        "{server}: {memory:?}"
    );
    if let Some(blackhole) = engines.iter().find(|e| e.name == "BLACKHOLE") {
        assert!(
            blackhole
                .warning
                .as_deref()
                .is_some_and(|w| w.contains("rows will be lost"))
        );
    }
    let innodb = engines
        .iter()
        .find(|e| e.name == "InnoDB")
        .ok_or("no InnoDB")?;
    assert_eq!(innodb.transactions, Some(true));
    let mut sorted = engines.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
    sorted.sort();
    assert_eq!(
        sorted,
        engines.iter().map(|e| e.name.clone()).collect::<Vec<_>>()
    );

    if !mariadb {
        // information_schema.TABLES caches AUTO_INCREMENT and ROW_FORMAT for a day by default.
        exec(session, "SET SESSION information_schema_stats_expiry = 0").await?;
    }
    exec(session, &format!("CREATE DATABASE {MY_DB}")).await?;
    exec(
        session,
        &format!(
            "CREATE TABLE {MY_DB}.orders (id int NOT NULL AUTO_INCREMENT PRIMARY KEY, note varchar(20)) \
             ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin"
        ),
    )
    .await?;
    exec(
        session,
        &format!("INSERT INTO {MY_DB}.orders (note) VALUES ('caf\u{e9}')"),
    )
    .await?;
    exec(
        session,
        &format!(
            "CREATE TABLE {MY_DB}.latin (id int NOT NULL AUTO_INCREMENT PRIMARY KEY, note varchar(20)) \
             ENGINE=InnoDB DEFAULT CHARSET=latin1 COLLATE=latin1_swedish_ci"
        ),
    )
    .await?;
    exec(
        session,
        &format!("INSERT INTO {MY_DB}.latin (note) VALUES ('caf\u{e9}')"),
    )
    .await?;
    exec(
        session,
        &format!(
            "CREATE TABLE {MY_DB}.scratch (id int NOT NULL AUTO_INCREMENT PRIMARY KEY, note varchar(20)) \
             ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin"
        ),
    )
    .await?;
    exec(
        session,
        &format!("INSERT INTO {MY_DB}.scratch (note) VALUES ('a'), ('b')"),
    )
    .await?;
    let catalog = open_catalog(session).await?;
    let catalog = catalog.as_ref();
    let table = |name: &str| object(ObjectKind::Table, MY_DB, None, name);
    let file = if mariadb { "mariadb" } else { "mysql" };
    for (label, changes) in steps(file)? {
        let mut plans = Vec::new();
        for change in changes {
            assert_eq!(change["engine"], "mysql", "{label}");
            let change: MyChange = serde_json::from_value(change["change"].clone())?;
            let plan = varve_catalog::mysql::edit::plan(session.server_info(), &change)
                .map_err(|e| format!("{label}: plan: {e}"))?;
            run_plan(session, &plan.statements, false)
                .await
                .map_err(|e| format!("{label} on {server}: {e}"))?;
            plans.push(plan);
        }
        match label.as_str() {
            "create made with options" => {
                let t = describe(catalog, session, &table("made")).await?;
                assert_eq!(
                    t["engine"],
                    if mariadb { "Aria" } else { "InnoDB" },
                    "{server}"
                );
                assert_eq!(
                    t["rowFormat"].as_str().map(str::to_lowercase).as_deref(),
                    Some(if mariadb { "page" } else { "compact" }),
                    "{server}"
                );
                assert_eq!(t["autoIncrement"], 100, "{server}");
                assert_eq!(t["collation"], "utf8mb4_bin");
                assert_eq!(t["comment"], "made by the designer");
            }
            "orders options" => {
                let t = describe(catalog, session, &table("orders")).await?;
                assert_eq!(
                    t["engine"],
                    if mariadb { "Aria" } else { "MyISAM" },
                    "{server}"
                );
                if mariadb {
                    assert_eq!(
                        t["rowFormat"].as_str().map(str::to_lowercase).as_deref(),
                        Some("page")
                    );
                }
                assert_eq!(t["autoIncrement"], 500, "{server}");
                assert_eq!(t["comment"], "orders, converted");
            }
            "orders charset and rename" => {
                assert!(describe(catalog, session, &table("orders")).await.is_err());
                let t = describe(catalog, session, &table("orders_renamed")).await?;
                assert_eq!(t["collation"], "latin1_swedish_ci", "{server}");
                let note = t["columns"]
                    .as_array()
                    .and_then(|c| c.iter().find(|c| c["name"] == "note"))
                    .ok_or("no note column")?;
                assert_eq!(note["charset"], "latin1", "{server}");
                // The value survived the conversion.
                assert_eq!(
                    scalar(
                        session,
                        &format!("SELECT HEX(note) AS v FROM {MY_DB}.orders_renamed")
                    )
                    .await?,
                    "636166E9"
                );
            }
            "orders_renamed to InnoDB" => {
                let t = describe(catalog, session, &table("orders_renamed")).await?;
                assert_eq!(t["engine"], "InnoDB", "{server}");
                // MariaDB's Aria table had ROW_FORMAT=PAGE; the plan reset it so InnoDB accepts the change.
                if mariadb {
                    assert!(plans.iter().any(|p| {
                        p.statements
                            .iter()
                            .any(|s| s.contains("ROW_FORMAT=DEFAULT"))
                    }));
                }
            }
            "latin to utf8mb4" => {
                let t = describe(catalog, session, &table("latin")).await?;
                let collation = t["collation"].as_str().unwrap_or_default();
                assert!(collation.starts_with("utf8mb4_"), "{server}: {collation}");
                assert!(plans.iter().all(|p| {
                    p.statements
                        .iter()
                        .all(|s| !s.contains("latin1_swedish_ci"))
                }));
                assert_eq!(
                    scalar(
                        session,
                        &format!("SELECT HEX(note) AS v FROM {MY_DB}.latin")
                    )
                    .await?,
                    "636166C3A9"
                );
            }
            "latin comment with backslashes" => {
                let want = "C:\\data\\new\\x */ it's; \u{fc}";
                let t = describe(catalog, session, &table("latin")).await?;
                assert_eq!(t["comment"], want, "{server}");
                // The same plan in a session with NO_BACKSLASH_ESCAPES stores the same text.
                let mode = scalar(session, "SELECT @@SESSION.sql_mode AS v").await?;
                exec(
                    session,
                    &format!("ALTER TABLE {MY_DB}.latin COMMENT='reset'"),
                )
                .await?;
                exec(
                    session,
                    "SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, ',NO_BACKSLASH_ESCAPES')",
                )
                .await?;
                let rerun = async {
                    for plan in &plans {
                        run_plan(session, &plan.statements, false).await?;
                    }
                    describe(catalog, session, &table("latin")).await
                }
                .await;
                exec(session, &format!("SET SESSION sql_mode = '{mode}'")).await?;
                assert_eq!(
                    rerun?["comment"], want,
                    "{server} with NO_BACKSLASH_ESCAPES"
                );
            }
            "scratch to a row-losing engine" => {
                let plan = plans.first().ok_or("no plan")?;
                assert!(plan.destructive, "{server}");
                // The designer's exact count (capped on the server) agrees with the rows left.
                let counted = varve_catalog::mysql::row_count(
                    session,
                    MY_DB,
                    "scratch",
                    varve_catalog::mysql::ROW_COUNT_CAP_MS,
                )
                .await?;
                assert_eq!(counted, Some(if mariadb { 2 } else { 0 }), "{server}");
                let t = describe(catalog, session, &table("scratch")).await?;
                let rows = scalar(
                    session,
                    &format!("SELECT COUNT(*) AS v FROM {MY_DB}.scratch"),
                )
                .await?;
                if mariadb {
                    assert!(
                        plan.warnings
                            .iter()
                            .any(|w| w.contains("lost when the server restarts")),
                        "{:?}",
                        plan.warnings
                    );
                    assert_eq!(t["engine"], "MEMORY");
                    assert_eq!(rows, "2");
                } else {
                    assert!(
                        plan.warnings
                            .iter()
                            .any(|w| w.contains("rows will be lost")),
                        "{:?}",
                        plan.warnings
                    );
                    // The warning is true: BLACKHOLE kept none of the 2 rows.
                    assert_eq!(t["engine"], "BLACKHOLE");
                    assert_eq!(rows, "0");
                }
            }
            "create vis with invisible and on update" => {
                let t = describe(catalog, session, &table("vis")).await?;
                let column = |name: &str| {
                    t["columns"]
                        .as_array()
                        .and_then(|c| c.iter().find(|c| c["name"] == name))
                        .cloned()
                        .ok_or(format!("no column {name}"))
                };
                assert_eq!(column("id")?["invisible"], false, "{server}");
                assert_eq!(column("secret")?["invisible"], true, "{server}");
                assert_eq!(column("changed")?["invisible"], false, "{server}");
                assert_eq!(
                    column("changed")?["onUpdate"]
                        .as_str()
                        .map(str::to_ascii_uppercase)
                        .as_deref(),
                    Some("CURRENT_TIMESTAMP(3)"),
                    "{server}"
                );
                // SELECT * leaves the invisible column out; naming it reads it.
                exec(
                    session,
                    &format!("INSERT INTO {MY_DB}.vis (secret) VALUES ('hidden')"),
                )
                .await?;
                let star = exec(session, &format!("SELECT * FROM {MY_DB}.vis")).await?;
                assert_eq!(column_names(&star), ["id", "changed"], "{server}");
                assert_eq!(
                    scalar(session, &format!("SELECT secret AS v FROM {MY_DB}.vis")).await?,
                    "hidden"
                );
            }
            "vis visibility swapped and on update cleared" => {
                let t = describe(catalog, session, &table("vis")).await?;
                let column = |name: &str| {
                    t["columns"]
                        .as_array()
                        .and_then(|c| c.iter().find(|c| c["name"] == name))
                        .cloned()
                        .ok_or(format!("no column {name}"))
                };
                assert_eq!(column("secret")?["invisible"], false, "{server}");
                assert_eq!(column("secret")?["collation"], "utf8mb4_bin", "{server}");
                assert_eq!(column("changed")?["invisible"], true, "{server}");
                assert_eq!(column("changed")?["onUpdate"], Json::Null, "{server}");
                let star = exec(session, &format!("SELECT * FROM {MY_DB}.vis")).await?;
                assert_eq!(column_names(&star), ["id", "secret"], "{server}");
                assert_eq!(
                    scalar(session, &format!("SELECT secret AS v FROM {MY_DB}.vis")).await?,
                    "hidden"
                );
            }
            "create subp" => {
                describe(catalog, session, &table("subp")).await?;
            }
            "partition subp by range with hash subpartitions" => {
                let t = describe(catalog, session, &table("subp")).await?;
                assert_eq!(t["partitioning"]["method"], "RANGE", "{server}");
                assert_eq!(t["partitioning"]["subpartitionMethod"], "HASH", "{server}");
                assert_eq!(
                    partition_layout(&t),
                    [("p0".to_owned(), 2), ("pmax".to_owned(), 2)],
                    "{server}"
                );
                // Rows in every partition, so the reorganizations below must move them.
                exec(
                    session,
                    &format!("INSERT INTO {MY_DB}.subp (id) VALUES (5), (50), (500), (5000)"),
                )
                .await?;
            }
            "split subp pmax" => {
                let t = describe(catalog, session, &table("subp")).await?;
                assert_eq!(
                    partition_layout(&t),
                    [
                        ("p0".to_owned(), 2),
                        ("p1".to_owned(), 2),
                        ("pmax".to_owned(), 2)
                    ],
                    "{server}"
                );
                assert_eq!(
                    scalar(
                        session,
                        &format!("SELECT COUNT(*) AS v FROM {MY_DB}.subp PARTITION (p1)")
                    )
                    .await?,
                    "1",
                    "{server}"
                );
            }
            "merge subp p0 and p1" => {
                let t = describe(catalog, session, &table("subp")).await?;
                assert_eq!(
                    partition_layout(&t),
                    [("plow".to_owned(), 2), ("pmax".to_owned(), 2)],
                    "{server}"
                );
                assert_eq!(
                    scalar(
                        session,
                        &format!("SELECT COUNT(*) AS v FROM {MY_DB}.subp PARTITION (plow)")
                    )
                    .await?,
                    "3",
                    "{server}"
                );
                assert_eq!(
                    scalar(session, &format!("SELECT COUNT(*) AS v FROM {MY_DB}.subp")).await?,
                    "4",
                    "{server}"
                );
            }
            "create lst" => {
                describe(catalog, session, &table("lst")).await?;
            }
            "partition lst by list" => {
                let t = describe(catalog, session, &table("lst")).await?;
                assert_eq!(t["partitioning"]["method"], "LIST", "{server}");
                assert_eq!(
                    partition_layout(&t),
                    [("a".to_owned(), 0), ("b".to_owned(), 0)],
                    "{server}"
                );
                exec(
                    session,
                    &format!("INSERT INTO {MY_DB}.lst (id) VALUES (1), (2), (3)"),
                )
                .await?;
                // The reorganize the designer refuses (value 2 in no new partition): the
                // backend cannot know the old values, so it always asks for the destructive
                // confirmation, and says why.
                let lossy: MyChange = serde_json::from_value(serde_json::json!({
                    "change": "alterTable", "database": MY_DB, "name": "lst",
                    "operations": [{"op": "reorganizePartitions", "names": ["a"], "into": [
                        {"name": "a1", "values": {"kind": "in", "rows": [[{"kind": "value", "value": {"t": "int", "v": 1}}]]}}
                    ]}]
                }))?;
                let plan = varve_catalog::mysql::edit::plan(session.server_info(), &lossy)?;
                assert!(plan.destructive, "{server}");
                assert!(
                    plan.warnings.iter().any(|w| w.contains("rows whose values no new partition lists are deleted")),
                    "{server}: {:?}",
                    plan.warnings
                );
            }
            "split lst a" => {
                assert!(plans.iter().all(|p| p.destructive), "{server}");
                let t = describe(catalog, session, &table("lst")).await?;
                assert_eq!(
                    partition_layout(&t),
                    [
                        ("a1".to_owned(), 0),
                        ("a2".to_owned(), 0),
                        ("b".to_owned(), 0)
                    ],
                    "{server}"
                );
                // Every row survived the split.
                assert_eq!(
                    scalar(session, &format!("SELECT COUNT(*) AS v FROM {MY_DB}.lst")).await?,
                    "3",
                    "{server}"
                );
                assert_eq!(
                    scalar(
                        session,
                        &format!("SELECT COUNT(*) AS v FROM {MY_DB}.lst PARTITION (a2)")
                    )
                    .await?,
                    "1",
                    "{server}"
                );
            }
            "partition made by hash" => {
                let t = describe(catalog, session, &table("made")).await?;
                assert_eq!(t["partitioning"]["method"], "HASH", "{server}");
                assert_eq!(
                    t["partitioning"]["partitions"].as_array().map(Vec::len),
                    Some(2)
                );
            }
            other => return Err(format!("no check for step {other}").into()),
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn table_designer_round_trips_on_mysql_8_0() -> TestResult {
    my_round_trip(Engine::Mysql, port("VARVE_M2_MYSQL_8_0_PORT", 35080)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn table_designer_round_trips_on_mysql_8_4() -> TestResult {
    my_round_trip(Engine::Mysql, port("VARVE_M2_MYSQL_8_4_PORT", 35084)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn table_designer_round_trips_on_mysql_9_7() -> TestResult {
    my_round_trip(Engine::Mysql, port("VARVE_M2_MYSQL_9_7_PORT", 35097)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn table_designer_round_trips_on_mariadb_10_6() -> TestResult {
    my_round_trip(Engine::Mariadb, port("VARVE_M2_MARIADB_10_6_PORT", 35106)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn table_designer_round_trips_on_mariadb_12_3() -> TestResult {
    my_round_trip(Engine::Mariadb, port("VARVE_M2_MARIADB_12_3_PORT", 35123)).await
}
