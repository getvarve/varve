//! Partial output of failed scripts (`DriverError::partial`) against the
//! MySQL and MariaDB servers of `dev/docker-compose.yml`.
//!
//! Start the servers with
//! `docker compose -p <project> -f dev/docker-compose.yml up -d --wait mysql-8.0 mysql-9.7 mariadb-10.6 mariadb-12.3`
//! and run `cargo test -p varve-driver-mysql --test partial -- --include-ignored`.
//! Endpoints come from the environment variables described in
//! `tests/servers.rs`, also `VARVE_ONLY_SERVERS`.

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use varve_core::Engine;
use varve_driver::{
    ConnectParams, DriverError, Durability, ErrorKind, ExecOptions, PartialOutput, Session,
    StatementResult, TlsMode, Value,
};
use varve_driver_mysql::MysqlDriver;
use varve_secrets::Secret;

use Durability::{Committed, InTransaction};

type TestResult = Result<(), Box<dyn Error>>;

struct Target {
    name: &'static str,
    engine: Engine,
    port: u16,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// Whether the server with `dev/docker-compose.yml` service name `service`
/// (for example `mysql-9.7`) is tested. `VARVE_ONLY_SERVERS`, a comma
/// separated list of service names, limits a run to those servers (CI tests
/// only the newest version of each engine); without it every server is tested.
fn selected(service: &str) -> bool {
    std::env::var("VARVE_ONLY_SERVERS")
        .ok()
        .is_none_or(|list| list.split(',').any(|name| name.trim() == service))
}

/// `targets` without the servers [`selected`] leaves out, never none.
fn only_selected(targets: Vec<Target>) -> Vec<Target> {
    let targets: Vec<Target> = targets
        .into_iter()
        .filter(|target| selected(&target.name.to_lowercase().replace(' ', "-")))
        .collect();
    assert!(
        !targets.is_empty(),
        "VARVE_ONLY_SERVERS selects no MySQL or MariaDB server"
    );
    targets
}

/// MySQL 8.0 and 9.7, MariaDB 10.6 and 12.3 (see [`selected`]).
fn targets() -> Vec<Target> {
    only_selected(every_target())
}

fn every_target() -> Vec<Target> {
    let port = |name: &str, default: u16| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    vec![
        Target {
            name: "MySQL 8.0",
            engine: Engine::Mysql,
            port: port("VARVE_MYSQL_8_0_PORT", 33080),
        },
        Target {
            name: "MySQL 9.7",
            engine: Engine::Mysql,
            port: port("VARVE_MYSQL_9_7_PORT", 33097),
        },
        Target {
            name: "MariaDB 10.6",
            engine: Engine::Mariadb,
            port: port("VARVE_MARIADB_10_6_PORT", 33106),
        },
        Target {
            name: "MariaDB 12.3",
            engine: Engine::Mariadb,
            port: port("VARVE_MARIADB_12_3_PORT", 33123),
        },
    ]
}

fn params(target: &Target) -> ConnectParams {
    let mut params = ConnectParams::new(
        target.engine,
        env_or("VARVE_MYSQL_HOST", "127.0.0.1"),
        target.port,
    );
    params.username = Some(env_or("VARVE_MYSQL_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_MYSQL_PASSWORD", "varve_dev")));
    params.database = Some(env_or("VARVE_MYSQL_DATABASE", "varve"));
    params.tls.mode = TlsMode::Disable;
    params.application_name = "varve-tests".into();
    params
}

async fn connect(params: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
    varve_driver::Driver::connect(&MysqlDriver, params).await
}

async fn run(session: &dyn Session, sql: &str) -> Result<varve_driver::ExecOutput, DriverError> {
    session.execute(sql, &ExecOptions::default()).await
}

async fn fails(session: &dyn Session, sql: &str) -> Result<DriverError, String> {
    match run(session, sql).await {
        Ok(output) => Err(format!("`{sql}` succeeded: {output:?}")),
        Err(error) => Ok(error),
    }
}

fn partial(error: &DriverError) -> Result<&PartialOutput, String> {
    error
        .partial
        .as_deref()
        .ok_or_else(|| format!("no partial output: {error:?}"))
}

fn kinds(partial: &PartialOutput) -> Vec<&'static str> {
    partial
        .results
        .iter()
        .map(|r| match r {
            StatementResult::Rows { .. } => "rows",
            StatementResult::Command { .. } => "command",
        })
        .collect()
}

async fn count(session: &dyn Session, table: &str) -> Result<i64, Box<dyn Error>> {
    let output = run(session, &format!("SELECT COUNT(*) FROM {table}")).await?;
    match output.results.first() {
        Some(StatementResult::Rows { rows, .. }) => match rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => Ok(*n),
            Some(Value::UInt(n)) => Ok(i64::try_from(*n)?),
            other => Err(format!("count: {other:?}").into()),
        },
        other => Err(format!("count: {other:?}").into()),
    }
}

/// A query that runs for minutes and fails with an error when killed.
const SLOW: &str = "SELECT COUNT(*) FROM information_schema.COLUMNS a \
                    CROSS JOIN information_schema.COLUMNS b CROSS JOIN information_schema.COLUMNS c";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_failing_first_statement_has_no_partial_output() -> TestResult {
    for target in targets() {
        let session = connect(&params(&target)).await?;
        let error = fails(session.as_ref(), "SELECT * FROM partial_missing; SELECT 1").await?;
        assert!(
            error.code.as_deref().is_some_and(|c| c.starts_with("1146")),
            "{}: {error:?}",
            target.name
        );
        assert!(error.partial.is_none(), "{}: {error:?}", target.name);
        // An empty statement is no statement: whether the server stops at it
        // or at the next one, statement 1 is the first that did not run.
        let error = fails(session.as_ref(), "SELECT 1;; SELECT * FROM partial_missing").await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(1), "{}: {error:?}", target.name);
        assert_eq!(kinds(p), ["rows"]);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn earlier_results_and_row_counts_survive_a_failure() -> TestResult {
    for target in targets() {
        let name = target.name;
        let session = connect(&params(&target)).await?;
        let s = session.as_ref();
        run(
            s,
            "DROP TABLE IF EXISTS partial_mid; CREATE TABLE partial_mid (id int)",
        )
        .await?;
        let error = fails(
            s,
            "/* first; */ SELECT 1 AS a; -- note;\nINSERT INTO partial_mid VALUES (1), (2);\n\
             # another note;\nSELECT 'x;y'; SELECT * FROM partial_missing; SELECT 2",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(3), "{name}: {p:?}");
        assert_eq!(kinds(p), ["rows", "command", "rows"], "{name}");
        assert!(matches!(
            &p.results[1],
            StatementResult::Command {
                rows_affected: Some(2),
                ..
            }
        ));
        assert_eq!(p.completed, [Committed, Committed, Committed], "{name}");
        assert_eq!(count(s, "partial_mid").await?, 2, "{name}");
        assert_eq!(session.in_transaction(), Some(false));

        // The last statement fails.
        let error = fails(s, "SELECT 1; SELECT 2; SELECT * FROM partial_missing").await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2), "{name}");
        assert_eq!(kinds(p), ["rows", "rows"], "{name}");
        run(s, "DROP TABLE partial_mid").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn procedures_and_compound_statements_count_as_one_statement() -> TestResult {
    for target in targets() {
        let name = target.name;
        let session = connect(&params(&target)).await?;
        let s = session.as_ref();
        run(
            s,
            "DROP PROCEDURE IF EXISTS partial_two; DROP PROCEDURE IF EXISTS partial_broken",
        )
        .await?;
        // Created without DELIMITER: the body's semicolons stay inside.
        let error = fails(
            s,
            "CREATE PROCEDURE partial_two() BEGIN SELECT 1; SELECT 2; END;\n\
             CALL partial_two(); SELECT 3; SELECT * FROM partial_missing",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(3), "{name}: {p:?}");
        assert_eq!(
            kinds(p),
            ["command", "rows", "rows", "command", "rows"],
            "{name}"
        );
        // A procedure may commit or roll back inside itself.
        assert_eq!(
            p.completed,
            [Committed, Durability::Unknown, Committed],
            "{name}"
        );

        // A procedure that fails after sending a result set: the set is
        // shown, the procedure is the failing statement.
        run(
            s,
            "CREATE PROCEDURE partial_broken() BEGIN SELECT 10; SELECT * FROM partial_missing; END",
        )
        .await?;
        let error = fails(s, "SELECT 0; CALL partial_broken(); SELECT 4").await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(1), "{name}: {p:?}");
        assert_eq!(kinds(p), ["rows", "rows"], "{name}");
        assert_eq!(p.completed, [Committed], "{name}");
        run(
            s,
            "DROP PROCEDURE partial_two; DROP PROCEDURE partial_broken",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn statements_in_an_open_transaction_are_reported_as_open() -> TestResult {
    for target in targets() {
        let name = target.name;
        let session = connect(&params(&target)).await?;
        let s = session.as_ref();
        run(
            s,
            "DROP TABLE IF EXISTS partial_tx; CREATE TABLE partial_tx (id int) ENGINE=InnoDB; \
             DROP TABLE IF EXISTS partial_ddl",
        )
        .await?;
        let error = fails(
            s,
            "START TRANSACTION; INSERT INTO partial_tx VALUES (1); SELECT * FROM partial_missing",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2), "{name}: {p:?}");
        assert_eq!(p.completed, [InTransaction, InTransaction], "{name}");
        assert_eq!(session.in_transaction(), Some(true));
        run(s, "ROLLBACK").await?;
        assert_eq!(count(s, "partial_tx").await?, 0, "{name}");

        // DDL commits the open transaction implicitly.
        let error = fails(
            s,
            "BEGIN; INSERT INTO partial_tx VALUES (1); CREATE TABLE partial_ddl (id int); \
             BEGIN; INSERT INTO partial_tx VALUES (2); SAVEPOINT s; \
             INSERT INTO partial_tx VALUES (3); ROLLBACK TO SAVEPOINT s; \
             SELECT * FROM partial_missing",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(8), "{name}: {p:?}");
        assert_eq!(
            p.completed,
            [
                Committed,
                Committed,
                Committed,
                InTransaction,
                InTransaction,
                InTransaction,
                Durability::RolledBack,
                InTransaction
            ],
            "{name}"
        );
        run(s, "ROLLBACK").await?;
        assert_eq!(count(s, "partial_tx").await?, 1, "{name}");
        run(s, "DROP TABLE partial_tx; DROP TABLE partial_ddl").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_sessions_report_the_failing_statement() -> TestResult {
    for target in targets() {
        let name = target.name;
        let mut params = params(&target);
        params.read_only = true;
        let session = connect(&params).await?;
        let error = fails(
            session.as_ref(),
            "SELECT 1; SELECT 'a;b'; SELECT * FROM partial_missing; SELECT 2",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2), "{name}: {p:?}");
        assert_eq!(kinds(p), ["rows", "rows"], "{name}");
        assert_eq!(p.completed, [Committed, Committed], "{name}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_cancel_mid_script_keeps_what_ran() -> TestResult {
    for target in targets() {
        let name = target.name;
        let session: Arc<dyn Session> = Arc::from(connect(&params(&target)).await?);
        run(
            session.as_ref(),
            "DROP TABLE IF EXISTS partial_cancel; CREATE TABLE partial_cancel (id int)",
        )
        .await?;
        let running = Arc::clone(&session);
        let task = tokio::spawn(async move {
            run(
                running.as_ref(),
                &format!("SELECT 1; INSERT INTO partial_cancel VALUES (1); {SLOW}; SELECT 2"),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(700)).await;
        session.cancel_handle().cancel().await?;
        let error = tokio::time::timeout(Duration::from_secs(10), task)
            .await??
            .err()
            .ok_or("the script finished")?;
        assert_eq!(error.kind, ErrorKind::Cancelled, "{name}: {error:?}");
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2), "{name}: {p:?}");
        assert_eq!(kinds(p), ["rows", "command"], "{name}");
        assert_eq!(p.completed, [Committed, Committed], "{name}");
        assert_eq!(
            count(session.as_ref(), "partial_cancel").await?,
            1,
            "{name}"
        );
        run(session.as_ref(), "DROP TABLE partial_cancel").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_statement_timeout_mid_script_keeps_what_ran() -> TestResult {
    for target in targets() {
        let name = target.name;
        let mut params = params(&target);
        params.statement_timeout = Some(Duration::from_millis(400));
        let session = connect(&params).await?;
        let error = fails(session.as_ref(), &format!("SELECT 1; {SLOW}; SELECT 2")).await?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{name}: {error:?}");
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(1), "{name}: {p:?}");
        assert_eq!(kinds(p), ["rows"], "{name}");
        assert_eq!(p.completed, [Committed], "{name}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn results_that_cannot_be_attributed_name_no_statement() -> TestResult {
    for target in targets() {
        let name = target.name;
        let session = connect(&params(&target)).await?;
        let s = session.as_ref();
        // EXECUTE of a prepared SELECT: its result set could also be a
        // procedure's, so which statement failed is not known.
        let error = fails(
            s,
            "PREPARE partial_s FROM 'SELECT 1'; EXECUTE partial_s; SELECT * FROM partial_missing",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, None, "{name}: {p:?}");
        assert!(p.completed.is_empty());
        assert_eq!(kinds(p), ["command", "rows"], "{name}");
        // EXECUTE of a statement without rows is attributed.
        let error = fails(
            s,
            "PREPARE partial_s FROM 'DO 1'; EXECUTE partial_s; SELECT * FROM partial_missing",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2), "{name}: {p:?}");
        assert_eq!(p.completed.len(), 2);
        // Boundaries that depend on NO_BACKSLASH_ESCAPES follow the
        // session's sql_mode.
        let error = fails(s, "SELECT 'a\\';b'; SELECT * FROM partial_missing").await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(1), "{name}: {p:?}");
        assert_eq!(kinds(p), ["rows"], "{name}");
        // A statement that changes sql_mode in the same request: the rest
        // is attributed only when it splits the same in both modes.
        let error = fails(
            s,
            "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES'); \
             SELECT 1; SELECT * FROM partial_missing",
        )
        .await?;
        assert_eq!(partial(&error)?.failed_statement, Some(2), "{name}");
        run(s, "SET SESSION sql_mode = DEFAULT").await?;
        let error = fails(
            s,
            "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES'); \
             SELECT 'a\\';b'; SELECT * FROM partial_missing",
        )
        .await?;
        assert_eq!(partial(&error)?.failed_statement, None, "{name}");
        run(s, "SET SESSION sql_mode = DEFAULT").await?;
    }
    Ok(())
}

/// The probes of the partial results audit (PC-1, PC-2, PC-5): Varve's
/// split must agree with the server's, executable comments are read as
/// the server reads them, and the statements that ran are reported
/// exactly or not at all. Run as `root` (`VARVE_MYSQL_ROOT_USER`,
/// `VARVE_MYSQL_ROOT_PASSWORD`) because they grant privileges.
fn root_params(target: &Target) -> ConnectParams {
    let mut params = params(target);
    params.username = Some(env_or("VARVE_MYSQL_ROOT_USER", "root"));
    params.password = Some(Secret::new(env_or(
        "VARVE_MYSQL_ROOT_PASSWORD",
        "varve_dev",
    )));
    params
}

/// `SHOW GRANTS FOR user`, as one text.
async fn grants(session: &dyn Session, user: &str) -> Result<String, Box<dyn Error>> {
    let output = run(session, &format!("SHOW GRANTS FOR {user}")).await?;
    Ok(format!("{:?}", output.results))
}

fn ran_kinds(session: &dyn Session) -> Option<Vec<varve_driver::StatementKind>> {
    session
        .last_statements()
        .map(|ran| ran.iter().map(|r| r.kind).collect())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn audit_probes_never_report_a_grant_that_did_not_run() -> TestResult {
    use varve_driver::StatementKind::{Ddl, Grant, Select};
    for target in all_targets() {
        let name = target.name;
        let session = connect(&root_params(&target)).await?;
        let s = session.as_ref();
        run(
            s,
            "DROP USER IF EXISTS pc_u; CREATE USER pc_u IDENTIFIED BY 'x'; \
             DROP PROCEDURE IF EXISTS varve.pc_p; DROP PROCEDURE IF EXISTS varve.pc_p2; \
             DROP TABLE IF EXISTS varve.pc_m; CREATE TABLE varve.pc_m (x int) ENGINE=InnoDB",
        )
        .await?;

        // P-M1: a routine whose body is one IF statement.
        let output = run(
            s,
            "CREATE PROCEDURE varve.pc_p() IF FALSE THEN DO 0; GRANT SELECT ON varve.* TO pc_u; END IF",
        )
        .await?;
        assert_eq!(output.results.len(), 1, "{name}");
        assert_eq!(ran_kinds(s), Some(vec![Ddl]), "{name}");

        // P-M2: the same, then a failure: the statements are named right.
        let error = fails(
            s,
            "CREATE PROCEDURE varve.pc_p2() IF FALSE THEN DO 0; GRANT SELECT ON varve.* TO pc_u; \
             END IF; SELECT 1; SELECT * FROM varve.pc_nonexist",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2), "{name}: {p:?}");
        assert_eq!(p.completed, [Committed, Committed], "{name}");
        assert_eq!(ran_kinds(s), Some(vec![Ddl, Select]), "{name}");

        // P-N1: under NO_BACKSLASH_ESCAPES the GRANT is inside a string.
        run(
            s,
            "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES')",
        )
        .await?;
        let output = run(
            s,
            "SELECT 'a\\'' AS c; GRANT SELECT ON varve.* TO pc_u; SELECT '",
        )
        .await?;
        assert_eq!(output.results.len(), 1, "{name}");
        // Read in the session's mode the GRANT is string text: no statement
        // of the script grants anything.
        assert!(!ran_kinds(s).unwrap_or_default().contains(&Grant), "{name}");
        run(s, "SET SESSION sql_mode = DEFAULT").await?;
        // A script that changes the mode midway is split again after it.
        let output = run(
            s,
            "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES'); \
             SELECT 'a\\'' AS c; GRANT SELECT ON varve.* TO pc_u; SELECT '",
        )
        .await?;
        assert_eq!(output.results.len(), 2, "{name}");
        assert_eq!(
            ran_kinds(s),
            Some(vec![varve_driver::StatementKind::Session, Select]),
            "{name}"
        );
        run(s, "SET SESSION sql_mode = DEFAULT").await?;
        assert!(!grants(s, "pc_u").await?.contains("SELECT ON"), "{name}");

        // A GRANT that runs is reported, also before a failure.
        let error = fails(
            s,
            "GRANT SELECT ON varve.pc_m TO pc_u; SELECT * FROM varve.pc_nonexist",
        )
        .await?;
        assert_eq!(partial(&error)?.failed_statement, Some(1));
        let ran = s.last_statements().ok_or("no exact report")?;
        assert_eq!(ran.len(), 1, "{name}");
        assert_eq!((ran[0].kind, ran[0].durability), (Grant, Committed));
        assert!(grants(s, "pc_u").await?.contains("`pc_m`"), "{name}");

        // P-M3, M3b: ROLLBACK in executable comments rolls back.
        for rollback in ["/*!ROLLBACK*/", "/*!50000 ROLLBACK */"] {
            let error = fails(
                s,
                &format!(
                    "START TRANSACTION; INSERT INTO varve.pc_m VALUES (1); {rollback}; \
                     SELECT * FROM varve.pc_nonexist"
                ),
            )
            .await?;
            let p = partial(&error)?;
            assert_eq!(p.failed_statement, Some(3), "{name}: {p:?}");
            assert_eq!(
                p.completed,
                [Durability::RolledBack, Durability::RolledBack, Committed],
                "{name} {rollback}"
            );
        }
        if target.engine == Engine::Mariadb {
            // P-M4b and P-M4.
            let error = fails(
                s,
                "START TRANSACTION; INSERT INTO varve.pc_m VALUES (1); /*M!ROLLBACK*/; \
                 SELECT * FROM varve.pc_nonexist",
            )
            .await?;
            assert_eq!(
                partial(&error)?.completed,
                [Durability::RolledBack, Durability::RolledBack, Committed],
                "{name}"
            );
            run(s, "IF 0 THEN DO 0; GRANT SELECT ON varve.* TO pc_u; END IF").await?;
            assert!(
                !ran_kinds(s).unwrap_or_default().contains(&Grant),
                "{name}: {:?}",
                s.last_statements()
            );
        }
        let count = run(s, "SELECT COUNT(*) FROM varve.pc_m").await?;
        assert!(format!("{:?}", count.results).contains("Int(0)"), "{name}");

        // PC-5: the server rejects the empty statement: the error points
        // at it, and statement 1 did not run.
        let error = fails(
            s,
            "SELECT 1;; -- c\n; /* x; */ SELECT 2; SELECT * FROM varve.pc_nonexist",
        )
        .await?;
        assert_eq!(error.position, Some(10), "{name}: {error:?}");
        assert_eq!(partial(&error)?.failed_statement, Some(1));

        run(
            s,
            "DROP PROCEDURE varve.pc_p; DROP PROCEDURE varve.pc_p2; DROP TABLE varve.pc_m; \
             DROP USER pc_u",
        )
        .await?;
    }
    Ok(())
}

/// MySQL 8.0, 8.4 and 9.7, MariaDB 10.6 and 12.3 (see [`selected`]).
fn all_targets() -> Vec<Target> {
    let port = |name: &str, default: u16| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    let mut targets = every_target();
    targets.insert(
        1,
        Target {
            name: "MySQL 8.4",
            engine: Engine::Mysql,
            port: port("VARVE_MYSQL_8_4_PORT", 33084),
        },
    );
    only_selected(targets)
}

/// Runs `sql` as one plain multi-statement request, as any client would:
/// the server's own behaviour, the baseline Varve must match. Returns the
/// number of results and the error, if any.
async fn plain(target: &Target, sql: &str) -> Result<(usize, Option<String>), Box<dyn Error>> {
    use mysql_async::prelude::Queryable;
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname(env_or("VARVE_MYSQL_HOST", "127.0.0.1"))
        .tcp_port(target.port)
        .user(Some(env_or("VARVE_MYSQL_ROOT_USER", "root")))
        .pass(Some(env_or("VARVE_MYSQL_ROOT_PASSWORD", "varve_dev")));
    let mut conn = mysql_async::Conn::new(opts).await?;
    let mut results = 0;
    // Read like any client: each OK packet and each result set is one result.
    let error = match conn.query_iter(sql).await {
        Err(error) => Some(error.to_string()),
        Ok(mut query) => 'read: loop {
            let Some(columns) = query.columns() else {
                break query.next().await.err().map(|e| e.to_string());
            };
            results += 1;
            if columns.is_empty() {
                if let Err(error) = query.next().await {
                    break Some(error.to_string());
                }
                continue;
            }
            loop {
                match query.next().await {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(error) => break 'read Some(error.to_string()),
                }
            }
        },
    };
    conn.disconnect().await?;
    Ok((results, error))
}

/// What a script left behind for one side: the grants of its user and the
/// definitions of the routines in its database.
async fn end_state(session: &dyn Session, db: &str, user: &str) -> Result<String, Box<dyn Error>> {
    let grants = grants(session, user)
        .await?
        .replace(user, "USER")
        .replace(db, "DB");
    let routines = run(
        session,
        &format!(
            "SELECT ROUTINE_NAME, ROUTINE_DEFINITION FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = '{db}' ORDER BY ROUTINE_NAME"
        ),
    )
    .await?;
    let log = run(
        session,
        &format!("SELECT k, v FROM {db}.log5 ORDER BY k, v"),
    )
    .await?;
    Ok(
        format!("{grants}\n{:?}\n{:?}", routines.results, log.results)
            .replace(db, "DB")
            .replace(user, "USER"),
    )
}

/// Audit round 2 (R2-1 to R2-5): for each script, Varve does what one plain
/// multi-statement request does: the same error or success, the same
/// grants, the same stored routine definitions. The scripts mention GRANT,
/// so Varve runs them one statement per request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn scripts_behave_as_one_plain_request() -> TestResult {
    for target in all_targets() {
        let name = target.name;
        let mariadb = target.engine == Engine::Mariadb;
        let (version_on, version_off) = if mariadb {
            ("/*!100000 ", "/*!999999 ")
        } else {
            ("/*!50000 ", "/*!99999 ")
        };
        let mut scripts: Vec<String> = vec![
            // R2-1: empty and comment-only statements.
            "GRANT SELECT ON {db}.* TO {user};; GRANT INSERT ON {db}.* TO {user}".into(),
            "GRANT SELECT ON {db}.* TO {user}; /* c */ ; GRANT INSERT ON {db}.* TO {user}".into(),
            "; GRANT SELECT ON {db}.* TO {user}".into(),
            "/* only a comment */; GRANT SELECT ON {db}.* TO {user}".into(),
            "GRANT SELECT ON {db}.* TO {user};; -- c\n;".into(),
            format!("{version_off}GRANT SELECT ON {{db}}.* TO {{user}} */; GRANT INSERT ON {{db}}.* TO {{user}}"),
            format!("{version_on}GRANT SELECT ON {{db}}.* TO {{user}} */; GRANT INSERT ON {{db}}.* TO {{user}}"),
            "/*M!100000 GRANT SELECT ON {db}.* TO {user} */; GRANT INSERT ON {db}.* TO {user}".into(),
            // R2-2: `end` and `start` as column names.
            "CREATE PROCEDURE {db}.p14() BEGIN SELECT end, start FROM {db}.t7 WHERE end > 0; \
             GRANT INSERT ON {db}.* TO {user}; END; GRANT SELECT ON {db}.* TO {user}"
                .into(),
            "CREATE PROCEDURE {db}.p15() BEGIN REPEAT SET @a = 1; UNTIL (SELECT MAX(end) FROM {db}.t7) > 0 \
             END REPEAT; END; GRANT SELECT ON {db}.* TO {user}"
                .into(),
            // R2-4: a single-statement body keeps its `;`.
            "CREATE PROCEDURE {db}.p10() SELECT CASE WHEN 1 THEN 2 END; GRANT SELECT ON {db}.* TO {user}"
                .into(),
            // R2-5: version digits.
            "SELECT /*!100000 +1 */ 5; GRANT SELECT ON {db}.* TO {user}".into(),
            "SELECT /*!8000001 +1 */ 5; GRANT SELECT ON {db}.* TO {user}".into(),
            // CI case on MySQL 9.7: a trigger, then a grant in an
            // executable comment that does not run on the server.
            "CREATE TRIGGER {db}.tr1 BEFORE INSERT ON {db}.t7 FOR EACH ROW CASE WHEN NEW.end > 1 \
             THEN SET NEW.end = 1; ELSE SET NEW.end = 0; END CASE; GRANT SELECT ON {db}.* TO {user}; \
             /*!99999 GRANT INSERT ON {db}.* TO {user} */"
                .into(),
            "CREATE TRIGGER {db}.tr2 BEFORE INSERT ON {db}.t7 FOR EACH ROW CASE WHEN NEW.end > 1 \
             THEN SET NEW.end = 1; ELSE SET NEW.end = 0; END CASE; \
             /*!99999 GRANT INSERT ON {db}.* TO {user} */; GRANT SELECT ON {db}.* TO {user}"
                .into(),
            // Round 3, Medium 1: the statement after a definition sees its
            // diagnostics as in the whole script; `event` as a table name.
            "CREATE TABLE {db}.event AS SELECT 1 AS x UNION SELECT 2; \
             INSERT INTO {db}.log5 VALUES ('rc', ROW_COUNT()); GRANT SELECT ON {db}.* TO {user}"
                .into(),
            "CREATE TABLE {db}.event (x int); CREATE TABLE IF NOT EXISTS {db}.event (x int); \
             INSERT INTO {db}.log5 VALUES ('wc', @@warning_count); GRANT SELECT ON {db}.* TO {user}"
                .into(),
            "CREATE PROCEDURE {db}.p1() SELECT 1; CREATE PROCEDURE IF NOT EXISTS {db}.p1() SELECT 1; \
             INSERT INTO {db}.log5 VALUES ('wc', @@warning_count); GRANT SELECT ON {db}.* TO {user}"
                .into(),
            "CREATE PROCEDURE {db}.p2() SELECT 1; INSERT INTO {db}.log5 VALUES ('rc', ROW_COUNT()), \
             ('id', LAST_INSERT_ID()); GRANT SELECT ON {db}.* TO {user}"
                .into(),
            // Round 3, Low 2: trailing comments after the last statement.
            "GRANT SELECT ON {db}.* TO {user}; -- c".into(),
            "GRANT SELECT ON {db}.* TO {user}; CREATE PROCEDURE IF NOT EXISTS {db}.p1() SELECT 1; /* c */"
                .into(),
            // A failure in the middle.
            "GRANT SELECT ON {db}.* TO {user}; SELECT * FROM {db}.missing; GRANT INSERT ON {db}.* TO {user}"
                .into(),
        ];
        if mariadb {
            // R2-3 (MariaDB 10.6 rejects the syntax on both sides).
            scripts.push(
                "CREATE FUNCTION {db}.f1() RETURNS TYPE OF {db}.t7.end BEGIN DECLARE x INT DEFAULT 1; \
                 RETURN x; END; GRANT SELECT ON {db}.* TO {user}"
                    .into(),
            );
            scripts.push("IF 1 THEN GRANT SELECT ON {db}.* TO {user}; END IF".into());
        }
        let root = connect(&root_params(&target)).await?;
        let r = root.as_ref();
        for script in &scripts {
            let mut states = Vec::new();
            for side in ["v", "b"] {
                let db = format!("cmp_{side}");
                let user = format!("cmp_u{side}");
                run(
                    r,
                    &format!(
                        "DROP DATABASE IF EXISTS {db}; CREATE DATABASE {db}; \
                         CREATE TABLE {db}.t7 (`end` int, `start` int); \
                         CREATE TABLE {db}.log5 (k varchar(8), v bigint); \
                         DROP USER IF EXISTS {user}; CREATE USER {user} IDENTIFIED BY 'x'"
                    ),
                )
                .await?;
                let sql = script.replace("{db}", &db).replace("{user}", &user);
                let outcome = if side == "v" {
                    let varve = connect(&root_params(&target)).await?;
                    let outcome = match run(varve.as_ref(), &sql).await {
                        Ok(output) => Some(format!("ok {}", output.results.len())),
                        Err(error) => Some(error.code.unwrap_or_default()),
                    };
                    // Grants the bridge would log must be real.
                    for ran in varve.last_statements().unwrap_or_default() {
                        if ran.kind == varve_driver::StatementKind::Grant
                            && ran.durability == Committed
                        {
                            let text = &sql[ran.offset..ran.offset + ran.len];
                            assert!(text.contains("GRANT"), "{name}: {text}");
                        }
                    }
                    outcome
                } else {
                    let (results, error) = plain(&target, &sql).await?;
                    Some(match error {
                        Some(error) => error.split(' ').nth(3).unwrap_or_default().to_owned(),
                        None => format!("ok {results}"),
                    })
                };
                states.push((
                    outcome.map(|code| {
                        if code.starts_with("ok") {
                            code
                        } else {
                            code.chars().take(4).collect::<String>()
                        }
                    }),
                    end_state(r, &db, &user).await?,
                ));
            }
            assert_eq!(states[0], states[1], "{name}: {script}");
        }
        run(
            r,
            "DROP DATABASE cmp_v; DROP DATABASE cmp_b; DROP USER cmp_uv; DROP USER cmp_ub",
        )
        .await?;
    }
    Ok(())
}
