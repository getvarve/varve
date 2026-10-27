//! Integration tests against the dev database matrix (`dev/docker-compose.yml`).
//!
//! Start the servers with
//! `docker compose -p <project> -f dev/docker-compose.yml up -d --wait mysql-8.0 mysql-9.7 mariadb-10.6 mariadb-12.3`
//! and run `cargo test -p varve-driver-mysql -- --include-ignored`.
//!
//! Endpoints come from environment variables so CI can point them elsewhere:
//! `VARVE_MYSQL_HOST` (default `127.0.0.1`), `VARVE_MYSQL_USER`,
//! `VARVE_MYSQL_PASSWORD`, `VARVE_MYSQL_DATABASE`, and one port per server:
//! `VARVE_MYSQL_8_0_PORT` (33080), `VARVE_MYSQL_9_7_PORT` (33097),
//! `VARVE_MARIADB_10_6_PORT` (33106), `VARVE_MARIADB_12_3_PORT` (33123).
//! `VARVE_ONLY_SERVERS` (compose service names such as `mysql-9.7,mariadb-12.3`)
//! limits the run to those servers; CI uses it to test only the newest versions.
//! `VerifyCa` against a server with a known CA is tested in `tests/tls.rs`.

use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use varve_core::Engine;
use varve_driver::{
    ConnectParams, DriverError, ErrorKind, ExecOptions, ExecOutput, Session, StatementResult,
    TlsMode, Value,
};
use varve_driver_mysql::{MysqlDriver, MysqlSession, SessionOverrides, normalize_dump_boilerplate};
use varve_secrets::Secret;

mod proxy;

type TestResult = Result<(), Box<dyn Error>>;

#[derive(Debug, Clone)]
struct Target {
    name: &'static str,
    engine: Engine,
    port: u16,
    /// The server offers TLS with its auto-generated certificate.
    tls: bool,
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

/// The servers to test (see [`selected`]), never none.
fn targets() -> Vec<Target> {
    let targets: Vec<Target> = every_target()
        .into_iter()
        .filter(|target| selected(&target.name.to_lowercase().replace(' ', "-")))
        .collect();
    assert!(
        !targets.is_empty(),
        "VARVE_ONLY_SERVERS selects no MySQL or MariaDB server"
    );
    targets
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
            tls: true,
        },
        Target {
            name: "MySQL 9.7",
            engine: Engine::Mysql,
            port: port("VARVE_MYSQL_9_7_PORT", 33097),
            tls: true,
        },
        Target {
            name: "MariaDB 10.6",
            engine: Engine::Mariadb,
            port: port("VARVE_MARIADB_10_6_PORT", 33106),
            tls: false,
        },
        Target {
            name: "MariaDB 12.3",
            engine: Engine::Mariadb,
            port: port("VARVE_MARIADB_12_3_PORT", 33123),
            tls: true,
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

/// Adds the server name to an error.
fn on(target: &Target, error: impl std::fmt::Display) -> Box<dyn Error> {
    format!("{}: {error}", target.name).into()
}

async fn open(target: &Target, params: &ConnectParams) -> Result<MysqlSession, Box<dyn Error>> {
    MysqlDriver::new()
        .open(params)
        .await
        .map_err(|error| on(target, error))
}

async fn run(
    target: &Target,
    session: &MysqlSession,
    sql: &str,
) -> Result<ExecOutput, Box<dyn Error>> {
    session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|error| on(target, format!("{sql}: {error:?}")))
}

fn expect_error(
    target: &Target,
    result: Result<impl std::fmt::Debug, DriverError>,
) -> Result<DriverError, Box<dyn Error>> {
    match result {
        Ok(value) => Err(on(target, format!("expected an error, got {value:?}"))),
        Err(error) => Ok(error),
    }
}

/// The rows of result `index`.
fn rows(output: &ExecOutput, index: usize) -> Result<&Vec<Vec<Value>>, Box<dyn Error>> {
    match output.results.get(index) {
        Some(StatementResult::Rows { rows, .. }) => Ok(rows),
        other => Err(format!("result {index} is not a row set: {other:?}").into()),
    }
}

/// The single value of a one-row, one-column first result.
async fn scalar(
    target: &Target,
    session: &MysqlSession,
    sql: &str,
) -> Result<Value, Box<dyn Error>> {
    let output = run(target, session, sql).await?;
    rows(&output, 0)?
        .first()
        .and_then(|row| row.first())
        .cloned()
        .ok_or_else(|| on(target, format!("{sql} returned no value")))
}

fn check(target: &Target, ok: bool, what: impl std::fmt::Display) -> TestResult {
    if ok { Ok(()) } else { Err(on(target, what)) }
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn connects_and_detects_the_engine() -> TestResult {
    for target in targets() {
        // The profile claims the other engine; detection must win.
        let mut params = params(&target);
        params.engine = match target.engine {
            Engine::Mysql => Engine::Mariadb,
            _ => Engine::Mysql,
        };
        params.init_sql = Some("SET @a = 1; SET @b = 2".into());
        let session = open(&target, &params).await?;
        let info = session.server_info().clone();
        check(
            &target,
            info.engine == target.engine,
            format!("detected {info:?}"),
        )?;
        check(&target, info.major >= 8, format!("version {info:?}"))?;
        check(&target, !session.uses_tls(), "TLS was disabled")?;

        let output = run(
            &target,
            &session,
            "SELECT DATABASE(), @@session.time_zone, @a + @b, CONNECTION_ID()",
        )
        .await?;
        let row = rows(&output, 0)?.first().cloned().unwrap_or_default();
        check(
            &target,
            row.first() == Some(&Value::Text("varve".into())),
            format!("default schema {row:?}"),
        )?;
        check(
            &target,
            row.get(1) == Some(&Value::Text("+00:00".into())),
            format!("time zone {row:?}"),
        )?;
        check(
            &target,
            row.get(2) == Some(&Value::Int(3)),
            format!("init SQL {row:?}"),
        )?;
        check(
            &target,
            i64::try_from(session.connection_id())
                .is_ok_and(|id| row.get(3) == Some(&Value::Int(id))),
            format!("connection id {row:?}"),
        )?;

        if target.engine == Engine::Mysql {
            let program = scalar(
                &target,
                &session,
                "SELECT ATTR_VALUE FROM performance_schema.session_account_connect_attrs \
                 WHERE PROCESSLIST_ID = CONNECTION_ID() AND ATTR_NAME = 'program_name'",
            )
            .await?;
            check(
                &target,
                program == Value::Text("varve-tests".into()),
                format!("program_name {program:?}"),
            )?;
        }

        session.ping().await.map_err(|error| on(&target, error))?;
        let any = Session::as_any(&session);
        check(
            &target,
            any.downcast_ref::<MysqlSession>().is_some(),
            "as_any",
        )?;
        Box::new(session)
            .close()
            .await
            .map_err(|error| on(&target, error))?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn wrong_password_is_an_auth_error() -> TestResult {
    for target in targets() {
        let mut params = params(&target);
        params.password = Some(Secret::new("not-the-password-x9".into()));
        let error = expect_error(&target, MysqlDriver::new().open(&params).await)?;
        check(&target, error.kind == ErrorKind::Auth, format!("{error:?}"))?;
        check(
            &target,
            error.code.as_deref() == Some("1045 (28000)"),
            format!("{error:?}"),
        )?;
        check(
            &target,
            !error.message.contains("not-the-password-x9"),
            "password in the message",
        )?;
    }
    // Nothing listens on port 1.
    let mut refused = params(&targets()[0]);
    refused.port = 1;
    let error = expect_error(&targets()[0], MysqlDriver::new().open(&refused).await)?;
    check(
        &targets()[0],
        error.kind == ErrorKind::Connect,
        format!("{error:?}"),
    )?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn read_only_sessions_reject_writes() -> TestResult {
    for target in targets() {
        let writer = open(&target, &params(&target)).await?;
        run(
            &target,
            &writer,
            "CREATE TABLE IF NOT EXISTS varve_read_only (id INT PRIMARY KEY)",
        )
        .await?;

        let mut read_only = params(&target);
        read_only.read_only = true;
        let session = open(&target, &read_only).await?;
        run(&target, &session, "SELECT COUNT(*) FROM varve_read_only").await?;
        let error = expect_error(
            &target,
            session
                .execute(
                    "INSERT INTO varve_read_only VALUES (1)",
                    &ExecOptions::default(),
                )
                .await,
        )?;
        check(
            &target,
            error.kind == ErrorKind::ReadOnly,
            format!("{error:?}"),
        )?;
        check(
            &target,
            error
                .code
                .as_deref()
                .is_some_and(|code| code.starts_with("1792 ")),
            format!("{error:?}"),
        )?;
        run(&target, &writer, "DROP TABLE varve_read_only").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn scripts_return_every_result_in_order() -> TestResult {
    for target in targets() {
        let session = open(&target, &params(&target)).await?;
        let output = run(
            &target,
            &session,
            "CREATE TEMPORARY TABLE script (id INT AUTO_INCREMENT PRIMARY KEY, v INT);
             INSERT INTO script (v) VALUES (1), (2), (3);
             SELECT v FROM script ORDER BY v;
             UPDATE script SET v = v + 10 WHERE v > 1;
             SELECT COUNT(*) AS n FROM script;
             SELECT 'x' AS a, 2 AS b",
        )
        .await?;
        let results = &output.results;
        check(&target, results.len() == 6, format!("{results:?}"))?;
        check(
            &target,
            results[0]
                == StatementResult::Command {
                    tag: "OK, 0 rows affected".into(),
                    rows_affected: Some(0),
                },
            format!("{:?}", results[0]),
        )?;
        check(
            &target,
            results[1]
                == StatementResult::Command {
                    tag: "OK, 3 rows affected, last insert id 1".into(),
                    rows_affected: Some(3),
                },
            format!("{:?}", results[1]),
        )?;
        check(
            &target,
            *rows(&output, 2)?
                == vec![
                    vec![Value::Int(1)],
                    vec![Value::Int(2)],
                    vec![Value::Int(3)],
                ],
            format!("{:?}", results[2]),
        )?;
        check(
            &target,
            results[3]
                == StatementResult::Command {
                    tag: "OK, 2 rows affected".into(),
                    rows_affected: Some(2),
                },
            format!("{:?}", results[3]),
        )?;
        check(
            &target,
            *rows(&output, 4)? == vec![vec![Value::Int(3)]],
            format!("{:?}", results[4]),
        )?;
        match &results[5] {
            StatementResult::Rows { columns, rows, .. } => {
                let names: Vec<_> = columns.iter().map(|c| c.name.as_str()).collect();
                check(&target, names == ["a", "b"], format!("{columns:?}"))?;
                check(
                    &target,
                    *rows == vec![vec![Value::Text("x".into()), Value::Int(2)]],
                    format!("{rows:?}"),
                )?;
            }
            other => return Err(on(&target, format!("{other:?}"))),
        }

        // An error stops the script and carries number and SQLSTATE.
        let error = expect_error(
            &target,
            session
                .execute("SELECT 1; SELEC 2; SELECT 3", &ExecOptions::default())
                .await,
        )?;
        check(
            &target,
            error.kind == ErrorKind::Query,
            format!("{error:?}"),
        )?;
        check(
            &target,
            error.code.as_deref() == Some("1064 (42000)"),
            format!("{error:?}"),
        )?;
        // A failing first statement, and the session stays usable.
        let error = expect_error(
            &target,
            session
                .execute("SELECT * FROM no_such_table", &ExecOptions::default())
                .await,
        )?;
        check(
            &target,
            error.code.as_deref() == Some("1146 (42S02)"),
            format!("{error:?}"),
        )?;
        check(
            &target,
            scalar(&target, &session, "SELECT 7").await? == Value::Int(7),
            "session unusable after an error",
        )?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn maps_every_type() -> TestResult {
    for target in targets() {
        let session = open(&target, &params(&target)).await?;
        run(
            &target,
            &session,
            "DROP TABLE IF EXISTS varve_types;
             CREATE TABLE varve_types (
               ti TINYINT, ui INT UNSIGNED, ub BIGINT UNSIGNED, sb BIGINT, y YEAR,
               d DECIMAL(40,10), f FLOAT, dbl DOUBLE,
               dt DATE, dtm DATETIME(6), ts TIMESTAMP NULL, tm TIME, tm2 TIME,
               b8 BIT(8), b64 BIT(64), j JSON,
               e ENUM('red','green'), s SET('a','b','c'),
               bin BINARY(4), vb VARBINARY(8), bl BLOB, tx TEXT, vc VARCHAR(20),
               g GEOMETRY, n INT
             );
             INSERT INTO varve_types VALUES (
               -5, 4000000000, 18446744073709551615, -9223372036854775808, 2026,
               123456789012345678901234567890.0123456789, 1.5, 2.25,
               '2026-10-19', '2026-10-19 13:45:00.123456', '2026-10-19 11:45:00',
               '-838:59:59', '100:30:00',
               b'10100101', b'1111111111111111111111111111111111111111111111111111111111111111',
               '{\"a\": [1, 2]}',
               'green', 'a,c',
               X'00010203', X'ff00', X'deadbeef', 'héllo', 'varchar',
               ST_GeomFromText('POINT(1 2)'), NULL
             )",
        )
        .await?;
        let output = run(&target, &session, "SELECT * FROM varve_types").await?;
        let (columns, row) = match output.results.first() {
            Some(StatementResult::Rows { columns, rows, .. }) => {
                (columns.clone(), rows.first().cloned().unwrap_or_default())
            }
            other => return Err(on(&target, format!("{other:?}"))),
        };
        let mariadb = target.engine == Engine::Mariadb;
        let json_type = if mariadb { "longtext" } else { "json" };
        let json_value = if mariadb {
            Value::Text("{\"a\": [1, 2]}".into())
        } else {
            Value::json("{\"a\": [1, 2]}")
        };
        let expected: Vec<(&str, &str, Value)> = vec![
            ("ti", "tinyint", Value::Int(-5)),
            ("ui", "int unsigned", Value::Int(4_000_000_000)),
            ("ub", "bigint unsigned", Value::UInt(u64::MAX)),
            ("sb", "bigint", Value::Int(i64::MIN)),
            ("y", "year", Value::Int(2026)),
            (
                "d",
                "decimal",
                Value::Decimal("123456789012345678901234567890.0123456789".into()),
            ),
            ("f", "float", Value::Float(1.5)),
            ("dbl", "double", Value::Float(2.25)),
            ("dt", "date", Value::Date("2026-10-19".into())),
            (
                "dtm",
                "datetime",
                Value::Timestamp("2026-10-19T13:45:00.123456".into()),
            ),
            (
                "ts",
                "timestamp",
                Value::TimestampTz("2026-10-19T11:45:00Z".into()),
            ),
            ("tm", "time", Value::Time("-838:59:59".into())),
            ("tm2", "time", Value::Time("100:30:00".into())),
            ("b8", "bit", Value::Int(0b1010_0101)),
            ("b64", "bit", Value::Bytes(vec![0xff; 8])),
            ("j", json_type, json_value),
            ("e", "enum", Value::Text("green".into())),
            ("s", "set", Value::Text("a,c".into())),
            ("bin", "binary", Value::Bytes(vec![0, 1, 2, 3])),
            ("vb", "varbinary", Value::Bytes(vec![0xff, 0x00])),
            ("bl", "blob", Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])),
            ("tx", "text", Value::Text("héllo".into())),
            ("vc", "varchar", Value::Text("varchar".into())),
        ];
        for (index, (name, type_name, value)) in expected.iter().enumerate() {
            let column = columns.get(index);
            check(
                &target,
                column.is_some_and(|c| c.name == *name && c.type_name == *type_name),
                format!("column {index}: expected {name} {type_name}, got {column:?}"),
            )?;
            check(
                &target,
                row.get(index) == Some(value),
                format!("{name}: expected {value:?}, got {:?}", row.get(index)),
            )?;
        }
        let geometry = expected.len();
        check(
            &target,
            columns.get(geometry).map(|c| c.type_name.as_str()) == Some("geometry"),
            format!("{:?}", columns.get(geometry)),
        )?;
        // SRID (4 bytes) + WKB point (21 bytes).
        check(
            &target,
            matches!(row.get(geometry), Some(Value::Bytes(bytes)) if bytes.len() == 25),
            format!("geometry {:?}", row.get(geometry)),
        )?;
        check(
            &target,
            row.get(geometry + 1) == Some(&Value::Null),
            format!("null {:?}", row.get(geometry + 1)),
        )?;
        check(&target, row.len() == geometry + 2, format!("{row:?}"))?;
        run(&target, &session, "DROP TABLE varve_types").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn max_rows_truncates_each_result() -> TestResult {
    for target in targets() {
        let session = open(&target, &params(&target)).await?;
        let output = session
            .execute(
                "WITH RECURSIVE seq (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 100)
                 SELECT n FROM seq;
                 SELECT 'after'",
                &ExecOptions { max_rows: 10 },
            )
            .await
            .map_err(|error| on(&target, error))?;
        match output.results.first() {
            Some(StatementResult::Rows {
                rows, truncated, ..
            }) => {
                check(&target, rows.len() == 10 && *truncated, format!("{rows:?}"))?;
                check(
                    &target,
                    rows[9] == vec![Value::Int(10)],
                    format!("{rows:?}"),
                )?;
            }
            other => return Err(on(&target, format!("{other:?}"))),
        }
        match output.results.get(1) {
            Some(StatementResult::Rows {
                rows, truncated, ..
            }) => check(
                &target,
                !*truncated && *rows == vec![vec![Value::Text("after".into())]],
                format!("{rows:?}"),
            )?,
            other => return Err(on(&target, format!("{other:?}"))),
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn warnings_become_notices() -> TestResult {
    for target in targets() {
        let session = open(&target, &params(&target)).await?;
        let output = run(
            &target,
            &session,
            "SET SESSION sql_mode = '';
             CREATE TEMPORARY TABLE warn (c VARCHAR(3));
             INSERT INTO warn VALUES ('too long')",
        )
        .await?;
        let notice = output.notices.first();
        check(
            &target,
            output.notices.len() == 1
                && notice.is_some_and(|n| {
                    n.severity == "Warning"
                        && n.message.starts_with("1265: Data truncated for column 'c'")
                }),
            format!("{:?}", output.notices),
        )?;

        // Earlier statements only report their count.
        let output = run(
            &target,
            &session,
            "INSERT INTO warn VALUES ('again too long'); SELECT 1",
        )
        .await?;
        check(
            &target,
            output.notices.len() == 1
                && output.notices[0]
                    .message
                    .starts_with("Statement 1 raised 1 warning(s)"),
            format!("{:?}", output.notices),
        )?;

        let output = run(&target, &session, "SELECT 1").await?;
        check(
            &target,
            output.notices.is_empty(),
            format!("{:?}", output.notices),
        )?;
    }
    Ok(())
}

/// A statement that runs for minutes and fails with error 1317 when killed.
const HEAVY: &str = "SELECT COUNT(*) FROM information_schema.columns a, \
                     information_schema.columns b, information_schema.columns c";

/// [`HEAVY`], then a statement that must not run.
const HEAVY_FIRST: &str = "SELECT COUNT(*) FROM information_schema.columns a, \
                           information_schema.columns b, information_schema.columns c; \
                           SELECT 'not run'";

/// A SLEEP, then [`HEAVY`], then a last statement.
const HEAVY_THEN_DONE: &str = "SELECT SLEEP(30); SELECT COUNT(*) FROM information_schema.columns a, \
                               information_schema.columns b, information_schema.columns c; \
                               SELECT 'done'";

/// A cancelled `SLEEP()` stops quickly. MySQL ends it without an error, so
/// the call returns its result with the too-late note; MariaDB may report
/// it as interrupted.
async fn cancel_sleep(target: &Target) -> TestResult {
    let session = Arc::new(open(target, &params(target)).await?);
    let handle = session.cancel_handle();
    let running = Arc::clone(&session);
    let task = tokio::spawn(async move {
        running
            .execute("SELECT SLEEP(30)", &ExecOptions::default())
            .await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    handle.cancel().await.map_err(|error| on(target, error))?;
    let outcome = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .map_err(|_| on(target, "SLEEP did not stop within 2 seconds"))??;
    let fine = match &outcome {
        Ok(output) => output
            .notices
            .iter()
            .any(|notice| notice.message.contains("came too late")),
        Err(error) => error.kind == ErrorKind::Cancelled,
    };
    check(target, fine, format!("{outcome:?}"))
}

async fn cancel_while_running(target: &Target, sql: &'static str) -> TestResult {
    cancel_while_running_with(target, &params(target), sql).await
}

async fn cancel_while_running_with(
    target: &Target,
    params: &ConnectParams,
    sql: &'static str,
) -> TestResult {
    let session = Arc::new(open(target, params).await?);
    let handle = session.cancel_handle();
    // Cancelling an idle session succeeds and changes nothing.
    handle.cancel().await.map_err(|error| on(target, error))?;

    let running = Arc::clone(&session);
    let task = tokio::spawn(async move { running.execute(sql, &ExecOptions::default()).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let cancelled_at = Instant::now();
    handle.cancel().await.map_err(|error| on(target, error))?;
    let outcome = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .map_err(|_| on(target, format!("{sql} did not stop within 2 seconds")))?
        .map_err(|error| on(target, error))?;
    let error = expect_error(target, outcome)?;
    check(
        target,
        error.kind == ErrorKind::Cancelled,
        format!("{error:?}"),
    )?;
    check(
        target,
        cancelled_at.elapsed() < Duration::from_secs(2),
        "cancel took too long",
    )?;

    // The session stays usable, and quickly.
    let started = Instant::now();
    check(
        target,
        scalar(target, &session, "SELECT 'still here'").await? == Value::Text("still here".into()),
        "session unusable after cancel",
    )?;
    check(
        target,
        started.elapsed() < Duration::from_secs(2),
        "the next statement waited for the cancelled script",
    )?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn cancel_stops_a_running_statement() -> TestResult {
    for target in targets() {
        cancel_while_running(&target, HEAVY).await?;
        // The killed SLEEP() ends without an error on MySQL; the repeated
        // kill then stops the next statement.
        cancel_while_running(&target, HEAVY_THEN_DONE).await?;
        cancel_sleep(&target).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn statement_timeout_is_a_timeout_error() -> TestResult {
    for target in targets() {
        let mut params = params(&target);
        params.statement_timeout = Some(Duration::from_millis(500));
        let session = open(&target, &params).await?;
        // MySQL applies max_execution_time to SELECT and ends an
        // interrupted SLEEP() without an error, so a real query is needed.
        let sql = match target.engine {
            Engine::Mariadb => "SELECT SLEEP(5)",
            _ => {
                "SELECT COUNT(*) FROM information_schema.columns a, \
                 information_schema.columns b, information_schema.columns c"
            }
        };
        let started = Instant::now();
        let error = expect_error(&target, session.execute(sql, &ExecOptions::default()).await)?;
        check(
            &target,
            error.kind == ErrorKind::Timeout,
            format!("{error:?}"),
        )?;
        let expected = if target.engine == Engine::Mariadb {
            "1969 "
        } else {
            "3024 "
        };
        check(
            &target,
            error
                .code
                .as_deref()
                .is_some_and(|code| code.starts_with(expected)),
            format!("{error:?}"),
        )?;
        check(
            &target,
            started.elapsed() < Duration::from_secs(4),
            "timeout took too long",
        )?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn json_text_is_passed_on_verbatim() -> TestResult {
    for target in targets() {
        let output = run(
            &target,
            &session_for(&target).await?,
            "CREATE TEMPORARY TABLE json_text (j JSON);
             INSERT INTO json_text VALUES ('{\"b\": 1, \"a\": 1.10, \"n\": 12345678901234567890123}');
             SELECT j, CAST(j AS CHAR) FROM json_text",
        )
        .await?;
        let row = rows(&output, 2)?.first().cloned().unwrap_or_default();
        let text = match row.get(1) {
            Some(Value::Text(text)) => text.clone(),
            other => return Err(on(&target, format!("text {other:?}"))),
        };
        // MariaDB has no JSON type on the wire: the value is text there.
        let expected = if target.engine == Engine::Mariadb {
            Value::Text(text)
        } else {
            Value::json(text)
        };
        check(
            &target,
            row.first() == Some(&expected),
            format!("{row:?} expected {expected:?}"),
        )?;
        check(
            &target,
            matches!(&expected, Value::Json(_)) == (target.engine == Engine::Mysql),
            format!("{expected:?}"),
        )?;
    }
    Ok(())
}

async fn session_for(target: &Target) -> Result<MysqlSession, Box<dyn Error>> {
    open(target, &params(target)).await
}

/// Rows in `varve_ro_bypass`, read through a writable session.
async fn bypass_rows(
    target: &Target,
    writer: &MysqlSession,
    name: &str,
) -> Result<Value, Box<dyn Error>> {
    scalar(target, writer, &format!("SELECT COUNT(*) FROM {name}")).await
}

/// A session as the server's root user (`VARVE_MYSQL_ROOT_USER`,
/// `VARVE_MYSQL_ROOT_PASSWORD`): privileged, so a write route is not
/// masked by missing privileges.
fn root_params(target: &Target) -> ConnectParams {
    let mut params = params(target);
    params.username = Some(env_or("VARVE_MYSQL_ROOT_USER", "root"));
    params.password = Some(Secret::new(env_or(
        "VARVE_MYSQL_ROOT_PASSWORD",
        "varve_dev",
    )));
    params
}

/// The variable that holds the access mode on `target`.
fn read_only_variable(target: &Target) -> &'static str {
    match target.engine {
        Engine::Mariadb => "tx_read_only",
        _ => "transaction_read_only",
    }
}

/// Root creates the table and the stored routines the read-only tests use.
async fn create_routines(target: &Target, root: &MysqlSession, name: &str) -> TestResult {
    let variable = read_only_variable(target);
    for sql in [
        format!("DROP TABLE IF EXISTS {name}"),
        format!("CREATE TABLE {name} (id INT PRIMARY KEY)"),
        format!("DROP PROCEDURE IF EXISTS {name}_switch"),
        format!("CREATE PROCEDURE {name}_switch() BEGIN SET @@session.{variable} = 0; END"),
        format!("DROP PROCEDURE IF EXISTS {name}_write"),
        format!(
            "CREATE PROCEDURE {name}_write() BEGIN SET @@session.{variable} = 0; \
             INSERT INTO {name} VALUES (100); END"
        ),
        format!("DROP FUNCTION IF EXISTS {name}_insert"),
        format!(
            "CREATE FUNCTION {name}_insert() RETURNS INT DETERMINISTIC MODIFIES SQL DATA \
             BEGIN INSERT INTO {name} VALUES (101); RETURN 1; END"
        ),
        format!("DROP PROCEDURE IF EXISTS {name}_rw"),
        format!("CREATE PROCEDURE {name}_rw() BEGIN START TRANSACTION READ WRITE; END"),
        format!("DROP PROCEDURE IF EXISTS {name}_untrack"),
        format!(
            "CREATE PROCEDURE {name}_untrack() BEGIN \
             SET SESSION session_track_system_variables = ''; \
             SET SESSION session_track_transaction_info = 'OFF'; \
             SET @@session.{variable} = 0; END"
        ),
        format!("DROP PROCEDURE IF EXISTS {name}_narrow"),
        format!(
            "CREATE PROCEDURE {name}_narrow() BEGIN \
             SET SESSION session_track_system_variables = 'autocommit'; \
             SET @@session.{variable} = 0; END"
        ),
        format!("DROP PROCEDURE IF EXISTS {name}_charset"),
        format!("CREATE PROCEDURE {name}_charset() BEGIN SET NAMES latin1; END"),
        format!("DROP FUNCTION IF EXISTS {name}_switch_fn"),
        format!(
            "CREATE FUNCTION {name}_switch_fn() RETURNS INT DETERMINISTIC MODIFIES SQL DATA \
             BEGIN SET @@session.{variable} = 0; RETURN 1; END"
        ),
    ] {
        run(target, root, &sql).await?;
    }
    Ok(())
}

async fn drop_routines(target: &Target, root: &MysqlSession, name: &str) -> TestResult {
    run(
        target,
        root,
        &format!(
            "DROP PROCEDURE {name}_switch; DROP PROCEDURE {name}_write; \
             DROP PROCEDURE {name}_rw; DROP PROCEDURE {name}_untrack; \
             DROP PROCEDURE {name}_narrow; DROP PROCEDURE {name}_charset; \
             DROP FUNCTION {name}_insert; DROP FUNCTION {name}_switch_fn; DROP TABLE {name}"
        ),
    )
    .await
    .map(|_| ())
}

/// Read-only mode is on, and an INSERT fails with error 1792.
async fn still_read_only(
    target: &Target,
    session: &MysqlSession,
    name: &str,
    what: &str,
) -> TestResult {
    let variable = read_only_variable(target);
    let mode = scalar(target, session, &format!("SELECT @@session.{variable}")).await?;
    check(
        target,
        mode == Value::Int(1),
        format!("{what}: mode {mode:?}"),
    )?;
    let error = expect_error(
        target,
        session
            .execute(
                &format!("INSERT INTO {name} VALUES (99)"),
                &ExecOptions::default(),
            )
            .await,
    )?;
    check(
        target,
        error.kind == ErrorKind::ReadOnly
            && error
                .code
                .as_deref()
                .is_some_and(|code| code.starts_with("1792 ")),
        format!("{what}: INSERT after it: {error:?}"),
    )
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn read_only_mode_cannot_be_switched_off() -> TestResult {
    for target in targets() {
        let root = open(&target, &root_params(&target)).await?;
        create_routines(&target, &root, "varve_ro_modes").await?;
        let mut read_only = params(&target);
        read_only.read_only = true;
        let session = open(&target, &read_only).await?;
        check(&target, session.is_read_only(), "is_read_only")?;
        let variable = read_only_variable(&target);

        // Statements that switch the mode off are refused before anything
        // runs, also with a backquoted variable name.
        let backquoted = format!(
            "SET @marker = 1; SET @@session.`{variable}` = 0; INSERT INTO varve_ro_modes VALUES (5)"
        );
        let mut refused = vec![
            "SET @marker = 1; SET SESSION TRANSACTION READ WRITE; INSERT INTO varve_ro_modes VALUES (1)".to_owned(),
            "SET @marker = 1; START TRANSACTION READ WRITE; INSERT INTO varve_ro_modes VALUES (2); COMMIT".to_owned(),
            "SET @marker = 1; SET @@session.transaction_read_only = 0; INSERT INTO varve_ro_modes VALUES (3)".to_owned(),
            "SET @marker = 1; SET SESSION tx_read_only = OFF; INSERT INTO varve_ro_modes VALUES (4)".to_owned(),
            backquoted,
            // Dynamic SQL from a variable cannot be checked.
            "SET @marker = 1;
             SET @q = CONCAT('START TRANSACTION ', 'READ WRITE');
             PREPARE s FROM @q;
             EXECUTE s;
             INSERT INTO varve_ro_modes VALUES (6);
             COMMIT"
                .to_owned(),
            "SET @marker = 1; PREPARE s FROM 'START TRANSACTION ' 'READ WRITE'".to_owned(),
        ];
        if target.engine == Engine::Mariadb {
            refused.push(
                "SET @marker = 1; EXECUTE IMMEDIATE CONCAT('START TRANSACTION ', 'READ WRITE')"
                    .to_owned(),
            );
        }
        for sql in &refused {
            let error = expect_error(&target, session.execute(sql, &ExecOptions::default()).await)?;
            check(
                &target,
                error.kind == ErrorKind::ReadOnly && error.message.contains("Nothing was run"),
                format!("{sql}: {error:?}"),
            )?;
            check(
                &target,
                scalar(&target, &session, "SELECT @marker IS NULL").await? == Value::Int(1),
                format!("{sql}: the first statement ran"),
            )?;
        }
        let dynamic = expect_error(
            &target,
            session
                .execute("PREPARE s FROM @q", &ExecOptions::default())
                .await,
        )?;
        check(
            &target,
            dynamic
                .message
                .contains("dynamic SQL cannot be checked in a read-only session"),
            format!("{dynamic:?}"),
        )?;

        // Stored routines the check cannot see into: the server reports the
        // switch, the mode is restored and the script stops before the next
        // statement, so nothing is written.
        for (sql, statement, what) in [
            (
                "SELECT varve_ro_modes_switch_fn(); INSERT INTO varve_ro_modes VALUES (10)",
                1,
                "switched read-only mode off",
            ),
            (
                "CALL varve_ro_modes_switch(); INSERT INTO varve_ro_modes VALUES (11)",
                1,
                "switched read-only mode off",
            ),
            (
                "SELECT 1; SET @x = varve_ro_modes_switch_fn(); INSERT INTO varve_ro_modes VALUES (12)",
                2,
                "switched read-only mode off",
            ),
            (
                "CALL varve_ro_modes_rw(); INSERT INTO varve_ro_modes VALUES (13); COMMIT",
                1,
                "started a READ WRITE transaction",
            ),
        ] {
            let error = expect_error(&target, session.execute(sql, &ExecOptions::default()).await)?;
            check(
                &target,
                error.kind == ErrorKind::ReadOnly
                    && error
                        .message
                        .starts_with(&format!("Statement {statement} {what}")),
                format!("{sql}: {error:?}"),
            )?;
            check(
                &target,
                bypass_rows(&target, &root, "varve_ro_modes").await? == Value::Int(0),
                format!("{sql} wrote"),
            )?;
            still_read_only(&target, &session, "varve_ro_modes", sql).await?;
        }

        // A routine that switches tracking off, or narrows it, is not seen
        // while the script runs; the check after the script finds it,
        // restores tracking and read-only mode, and the call reports it.
        for procedure in ["varve_ro_modes_untrack", "varve_ro_modes_narrow"] {
            let error = expect_error(
                &target,
                session
                    .execute(&format!("CALL {procedure}()"), &ExecOptions::default())
                    .await,
            )?;
            check(
                &target,
                error.kind == ErrorKind::ReadOnly
                    && error.message.starts_with("After the script")
                    && error
                        .message
                        .contains("tracking was narrowed or switched off"),
                format!("{procedure}: {error:?}"),
            )?;
            still_read_only(&target, &session, "varve_ro_modes", procedure).await?;
            let list = scalar(
                &target,
                &session,
                "SELECT @@session.session_track_system_variables",
            )
            .await?;
            check(
                &target,
                matches!(&list, Value::Text(list) if list.contains("time_zone")
                    && (list.contains("tx_read_only") || list.contains("transaction_read_only"))),
                format!("{procedure}: tracking not restored: {list:?}"),
            )?;
        }
        // Tracking works again: the next switch is caught.
        let error = expect_error(
            &target,
            session
                .execute(
                    "CALL varve_ro_modes_switch(); INSERT INTO varve_ro_modes VALUES (14)",
                    &ExecOptions::default(),
                )
                .await,
        )?;
        check(
            &target,
            error.kind == ErrorKind::ReadOnly,
            format!("{error:?}"),
        )?;

        // Harmless prepared statements still work.
        let output = run(
            &target,
            &session,
            "PREPARE s FROM 'select ?'; SET @v = 5; EXECUTE s USING @v; DEALLOCATE PREPARE s",
        )
        .await?;
        check(
            &target,
            output.results.len() == 4 && *rows(&output, 2)? == vec![vec![Value::Int(5)]],
            format!("{:?}", output.results),
        )?;

        // DDL is refused by the server.
        for sql in [
            "CREATE TABLE varve_ro_ddl (id INT)",
            "ALTER TABLE varve_ro_modes ADD COLUMN extra INT",
            "DROP TABLE varve_ro_modes",
            "CREATE DATABASE varve_ro_db",
        ] {
            let error = expect_error(&target, session.execute(sql, &ExecOptions::default()).await)?;
            check(
                &target,
                error.kind == ErrorKind::ReadOnly
                    && error
                        .code
                        .as_deref()
                        .is_some_and(|c| c.starts_with("1792 ")),
                format!("{sql}: {error:?}"),
            )?;
        }

        // Cancel works in read-only sessions. A statement killed with an
        // error is Cancelled; a SLEEP() that ends early without one leaves
        // the statements so far, and the rest is not run.
        cancel_while_running_with(&target, &read_only, HEAVY_FIRST).await?;
        let stopped = Arc::new(open(&target, &read_only).await?);
        let running = Arc::clone(&stopped);
        let task = tokio::spawn(async move {
            running
                .execute(
                    "SELECT SLEEP(30); SELECT 'not run'",
                    &ExecOptions::default(),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        stopped
            .cancel_handle()
            .cancel()
            .await
            .map_err(|error| on(&target, error))?;
        let outcome = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .map_err(|_| on(&target, "the cancelled script hung"))??;
        let fine = match &outcome {
            Ok(output) => {
                output.results.len() == 1
                    && output
                        .notices
                        .iter()
                        .any(|notice| notice.message.contains("cancelled after statement 1"))
            }
            Err(error) => error.kind == ErrorKind::Cancelled,
        };
        check(
            &target,
            fine,
            format!("SLEEP then a statement: {outcome:?}"),
        )?;

        // An init SQL that switches the mode off is refused at connect.
        for init_sql in [
            "SET SESSION TRANSACTION READ WRITE",
            "SET @q = CONCAT('SET SESSION TRANSACTION ', 'READ WRITE'); PREPARE s FROM @q; EXECUTE s",
        ] {
            let mut bad = read_only.clone();
            bad.init_sql = Some(init_sql.into());
            let error = expect_error(&target, MysqlDriver::new().open(&bad).await)?;
            check(
                &target,
                error.kind == ErrorKind::ReadOnly,
                format!("{init_sql}: {error:?}"),
            )?;
        }

        check(
            &target,
            bypass_rows(&target, &root, "varve_ro_modes").await? == Value::Int(0),
            "a write got through",
        )?;
        drop_routines(&target, &root, "varve_ro_modes").await?;
    }
    Ok(())
}

/// The scripts whose meaning depends on nothing running between their
/// statements.
const SEMANTICS: [&str; 4] = [
    "SELECT SQL_CALC_FOUND_ROWS n FROM (SELECT 1 n UNION SELECT 2 UNION SELECT 3) t LIMIT 1; \
     SELECT FOUND_ROWS()",
    "SELECT CAST('abc' AS SIGNED); SHOW WARNINGS",
    "SELECT CAST('abc' AS SIGNED); SELECT @@warning_count",
    "CREATE TEMPORARY TABLE IF NOT EXISTS varve_rc (a INT); SELECT 1; SELECT ROW_COUNT()",
];

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn read_only_sessions_keep_script_semantics() -> TestResult {
    for target in targets() {
        let normal = session_for(&target).await?;
        let mut read_only = params(&target);
        read_only.read_only = true;
        let session = open(&target, &read_only).await?;

        // FOUND_ROWS() sees the SELECT before it.
        let output = run(&target, &session, SEMANTICS[0]).await?;
        check(
            &target,
            *rows(&output, 1)? == vec![vec![Value::Int(3)]],
            format!("FOUND_ROWS: {:?}", output.results),
        )?;
        // SHOW WARNINGS and @@warning_count see the cast warning.
        let output = run(&target, &session, SEMANTICS[1]).await?;
        let warnings = rows(&output, 1)?;
        check(
            &target,
            warnings.len() == 1 && warnings[0].get(1) == Some(&Value::Int(1292)),
            format!("SHOW WARNINGS: {warnings:?}"),
        )?;
        let output = run(&target, &session, SEMANTICS[2]).await?;
        check(
            &target,
            *rows(&output, 1)? == vec![vec![Value::Int(1)]],
            format!("@@warning_count: {:?}", output.results),
        )?;

        // Every script gives what a normal session gives.
        for sql in [
            SEMANTICS[0],
            SEMANTICS[1],
            SEMANTICS[2],
            "SELECT 1;\nSELECT 2;\nSELEC 3",
            "-- only a comment",
            "/* only a comment */",
        ] {
            let expected = normal.execute(sql, &ExecOptions::default()).await;
            let actual = session.execute(sql, &ExecOptions::default()).await;
            let same = match (&expected, &actual) {
                (Ok(expected), Ok(actual)) => {
                    expected.results == actual.results && expected.notices == actual.notices
                }
                (Err(expected), Err(actual)) => {
                    expected.kind == actual.kind
                        && expected.code == actual.code
                        && expected.message == actual.message
                }
                _ => false,
            };
            check(
                &target,
                same,
                format!("{sql}: normal {expected:?}, read-only {actual:?}"),
            )?;
        }
    }
    Ok(())
}

/// Where `SELECT ... INTO OUTFILE` may write on `target`, from root's view.
async fn outfile_dir(target: &Target, root: &MysqlSession) -> Result<String, Box<dyn Error>> {
    Ok(
        match scalar(target, root, "SELECT @@secure_file_priv").await? {
            Value::Text(dir) if !dir.is_empty() => dir,
            _ => "/tmp/".to_owned(),
        },
    )
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn read_only_sessions_cannot_write() -> TestResult {
    for target in targets() {
        let root = open(&target, &root_params(&target)).await?;
        create_routines(&target, &root, "varve_ro_bypass").await?;
        let dir = outfile_dir(&target, &root).await?;
        let outfile = format!("{dir}varve_ro_outfile_{}.txt", std::process::id());
        let dumpfile = format!("{dir}varve_ro_dumpfile_{}.txt", std::process::id());
        let max_connections = scalar(&target, &root, "SELECT @@global.max_connections").await?;
        let other_value = match &max_connections {
            Value::Int(n) => n + 7,
            other => return Err(on(&target, format!("max_connections {other:?}"))),
        };

        let routes: Vec<String> = vec![
            // Executable comments.
            "/*! INSERT INTO varve_ro_bypass VALUES (1) */".into(),
            "/*!50000 SET SESSION TRANSACTION READ WRITE */; INSERT INTO varve_ro_bypass VALUES (1)".into(),
            "SELECT 1 /*M!, (SELECT 1) */".into(),
            // sql_mode changes.
            "SET sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 'a\\'; SET SESSION TRANSACTION READ WRITE; \
             INSERT INTO varve_ro_bypass VALUES (2); SELECT '"
                .into(),
            "SET sql_mode = 'ANSI_QUOTES'; INSERT INTO \"varve_ro_bypass\" VALUES (3)".into(),
            // Several statements packed into one.
            "PREPARE s FROM 'SET SESSION TRANSACTION READ WRITE; INSERT INTO varve_ro_bypass VALUES (4)'; EXECUTE s".into(),
            "SELECT 1 --x; SET SESSION TRANSACTION READ WRITE; INSERT INTO varve_ro_bypass VALUES (4)".into(),
            // HANDLER only reads.
            "HANDLER varve_ro_bypass OPEN; HANDLER varve_ro_bypass READ FIRST; HANDLER varve_ro_bypass CLOSE".into(),
            // LOAD DATA, local and server side.
            "LOAD DATA LOCAL INFILE '/etc/hosts' INTO TABLE varve_ro_bypass".into(),
            "LOAD DATA INFILE '/etc/hostname' INTO TABLE varve_ro_bypass".into(),
            // Files on the server.
            format!("SELECT 1 INTO OUTFILE '{outfile}'"),
            format!("SELECT 'x' INTO DUMPFILE '{dumpfile}'"),
            // Server settings.
            format!("SET GLOBAL max_connections = {other_value}"),
            format!("SET @@global.max_connections = {other_value}"),
            format!("SET PERSIST max_connections = {other_value}"),
            // XA, table locks, temporary tables.
            "XA START 'varve_ro'; INSERT INTO varve_ro_bypass VALUES (5); XA END 'varve_ro'; \
             XA PREPARE 'varve_ro'; XA COMMIT 'varve_ro'"
                .into(),
            "LOCK TABLES varve_ro_bypass WRITE; INSERT INTO varve_ro_bypass VALUES (6)".into(),
            "CREATE TEMPORARY TABLE varve_ro_bypass (id INT); INSERT INTO varve_ro_bypass VALUES (7)".into(),
            // A function that writes.
            "DO varve_ro_bypass_insert()".into(),
            "SELECT varve_ro_bypass_insert()".into(),
            // Prepared from literals.
            "PREPARE s FROM 'INSERT INTO varve_ro_bypass VALUES (8)'; EXECUTE s".into(),
            "PREPARE s FROM 'SET SESSION TRANSACTION READ WRITE'; EXECUTE s".into(),
        ];
        let cleanup = "XA END 'varve_ro'; XA ROLLBACK 'varve_ro'; UNLOCK TABLES; \
                       DROP TEMPORARY TABLE IF EXISTS varve_ro_bypass";

        for user in ["varve", "root"] {
            let mut read_only = if user == "root" {
                root_params(&target)
            } else {
                params(&target)
            };
            read_only.read_only = true;
            for route in &routes {
                let session = open(&target, &read_only).await?;
                let outcome = session.execute(route, &ExecOptions::default()).await;
                for statement in cleanup.split("; ") {
                    // Most of these fail; they only undo what a route left.
                    let _ = session.execute(statement, &ExecOptions::default()).await;
                }
                let what = format!("{user}: {route}: {outcome:?}");
                check(
                    &target,
                    bypass_rows(&target, &root, "varve_ro_bypass").await? == Value::Int(0),
                    format!("table changed by {what}"),
                )?;
                for file in [&outfile, &dumpfile] {
                    let absent = scalar(
                        &target,
                        &root,
                        &format!("SELECT LOAD_FILE('{file}') IS NULL"),
                    )
                    .await?;
                    check(
                        &target,
                        absent == Value::Int(1),
                        format!("{file} written by {what}"),
                    )?;
                }
                check(
                    &target,
                    scalar(&target, &root, "SELECT @@global.max_connections").await?
                        == max_connections,
                    format!("max_connections changed by {what}"),
                )?;
                still_read_only(&target, &session, "varve_ro_bypass", &what).await?;
            }

            // A procedure that switches the mode off and writes in its own
            // body (autocommitted before control returns) is the remaining
            // gap; the mode is restored and the script stopped after it.
            let session = open(&target, &read_only).await?;
            let error = expect_error(
                &target,
                session
                    .execute("CALL varve_ro_bypass_write()", &ExecOptions::default())
                    .await,
            )?;
            check(
                &target,
                error.kind == ErrorKind::ReadOnly,
                format!("CALL: {error:?}"),
            )?;
            still_read_only(
                &target,
                &session,
                "varve_ro_bypass",
                "after CALL of a writing procedure",
            )
            .await?;
            let output = session
                .execute(
                    "SELECT varve_ro_bypass_switch_fn()",
                    &ExecOptions::default(),
                )
                .await;
            check(
                &target,
                matches!(&output, Err(error) if error.kind == ErrorKind::ReadOnly),
                format!("function: {output:?}"),
            )?;
            still_read_only(
                &target,
                &session,
                "varve_ro_bypass",
                "after a switching function",
            )
            .await?;
            run(&target, &root, "DELETE FROM varve_ro_bypass").await?;
        }
        drop_routines(&target, &root, "varve_ro_bypass").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn timestamps_follow_the_session_time_zone() -> TestResult {
    // 1792410300 is 2026-10-19 11:45:00 UTC.
    let utc = Value::TimestampTz("2026-10-19T11:45:00Z".into());
    let plus_two = Value::Timestamp("2026-10-19T13:45:00".into());
    for target in targets() {
        // An init SQL that sets another zone does not win.
        let mut params = params(&target);
        params.init_sql = Some("SET time_zone = '+02:00'".into());
        let session = open(&target, &params).await?;
        check(
            &target,
            scalar(&target, &session, "SELECT @@session.time_zone").await?
                == Value::Text("+00:00".into()),
            "init SQL time zone",
        )?;
        run(
            &target,
            &session,
            "CREATE TEMPORARY TABLE tz (ts TIMESTAMP NULL);
             INSERT INTO tz VALUES (FROM_UNIXTIME(1792410300))",
        )
        .await?;
        check(
            &target,
            scalar(&target, &session, "SELECT ts FROM tz").await? == utc,
            "UTC session",
        )?;

        // A SET in the script: results after it have no zone.
        let output = run(
            &target,
            &session,
            "SELECT ts FROM tz; SET time_zone = '+02:00'; SELECT ts FROM tz",
        )
        .await?;
        check(
            &target,
            *rows(&output, 0)? == vec![vec![utc.clone()]]
                && *rows(&output, 2)? == vec![vec![plus_two.clone()]],
            format!("{:?}", output.results),
        )?;
        // The server reports each change, so a zone set back to UTC in the
        // same script labels the results after it as UTC again.
        let output = run(
            &target,
            &session,
            "SET time_zone = '+02:00'; SELECT ts FROM tz; SET time_zone = '+00:00'; SELECT ts FROM tz;
             SET time_zone = '+02:00'",
        )
        .await?;
        check(
            &target,
            *rows(&output, 1)? == vec![vec![plus_two.clone()]]
                && *rows(&output, 3)? == vec![vec![utc.clone()]],
            format!("tracked changes: {:?}", output.results),
        )?;
        // The next call knows the zone.
        check(
            &target,
            scalar(&target, &session, "SELECT ts FROM tz").await? == plus_two,
            "tracked +02:00",
        )?;
        run(&target, &session, "SET @@session.time_zone = '+00:00'").await?;
        check(
            &target,
            scalar(&target, &session, "SELECT ts FROM tz").await? == utc,
            "back to UTC",
        )?;
    }
    Ok(())
}

/// The header block of a `mysqldump` file (MySQL 8 and later).
const DUMP_HEADER: &str = "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
/*!40101 SET @OLD_CHARACTER_SET_RESULTS=@@CHARACTER_SET_RESULTS */;
/*!40101 SET @OLD_COLLATION_CONNECTION=@@COLLATION_CONNECTION */;
/*!50503 SET NAMES utf8mb4 */;
/*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */;
/*!40103 SET TIME_ZONE='+00:00' */;
/*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */;
/*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */;
/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */;
/*!40111 SET @OLD_SQL_NOTES=@@SQL_NOTES, SQL_NOTES=0 */;";

/// The footer lines of a dump that restore what the header saved, minus
/// the client character set ones (`SET CHARACTER_SET_CLIENT =
/// @OLD_CHARACTER_SET_CLIENT` names no literal value and is refused).
const DUMP_FOOTER_SAFE: &str = "/*!40103 SET TIME_ZONE=@OLD_TIME_ZONE */;
/*!40101 SET SQL_MODE=@OLD_SQL_MODE */;
/*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */;
/*!40014 SET UNIQUE_CHECKS=@OLD_UNIQUE_CHECKS */;
/*!40111 SET SQL_NOTES=@OLD_SQL_NOTES */;";

/// A dump made with `mysqldump` 8.0 (`--skip-dump-date`).
const MYSQL_DUMP: &str = include_str!("fixtures/mysqldump-8.0.sql");

/// A dump made with `mariadb-dump` 12.3 (`--skip-dump-date`).
const MARIADB_DUMP: &str = include_str!("fixtures/mariadb-dump-12.3.sql");

/// The session's `character_set_client`.
async fn client_charset(target: &Target, session: &MysqlSession) -> Result<Value, Box<dyn Error>> {
    scalar(target, session, "SELECT @@session.character_set_client").await
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn the_client_character_set_stays_utf8mb4() -> TestResult {
    let utf8mb4 = Value::Text("utf8mb4".into());
    for target in targets() {
        let root = open(&target, &root_params(&target)).await?;
        create_routines(&target, &root, "varve_charset").await?;
        let normal = session_for(&target).await?;
        let mut read_only = params(&target);
        read_only.read_only = true;
        let guarded = open(&target, &read_only).await?;

        // Refused in every session, before anything runs.
        for session in [&normal, &guarded] {
            for sql in [
                "SET @marker = 1; SET NAMES gbk".to_owned(),
                "SET @marker = 1; SET CHARACTER SET gbk".to_owned(),
                "SET @marker = 1; SET @@session.character_set_client = latin1".to_owned(),
                "SET @marker = 1; SET collation_connection = latin1_swedish_ci".to_owned(),
                "SET @marker = 1; SET @a = 1, NAMES latin1".to_owned(),
                "SET @marker = 1; /*!40101 SET NAMES latin1 */".to_owned(),
                // Executable comments, read as code and as comments, with
                // five or six version digits.
                "SET @marker = 1; /*!99999 ; SELECT */ SET NAMES latin1".to_owned(),
                "SET @marker = 1; SET @a = /*!800001 , NAMES latin1 */".to_owned(),
                "SET @marker = 1; SET @a = /*!1006001 , NAMES latin1 */".to_owned(),
                // Other sql_mode readings of the same text.
                "SET @marker = 1; SET sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 'a\\'; \
                 SET NAMES latin1; SELECT '"
                    .to_owned(),
                "SET @marker = 1; SET sql_mode = 'ANSI_QUOTES'; SELECT \"a\\\"; \
                 SET NAMES latin1; SELECT \""
                    .to_owned(),
                // A variable as the value.
                "SET @marker = 1; SET @v = @@character_set_client; SET character_set_client = @v"
                    .to_owned(),
                // In GBK the backtick ends a character, so the server would
                // see the next statements differently from the lexer.
                "SET @marker = 1; SET NAMES gbk; SELECT 1 AS \u{20ac}`; SET SESSION TRANSACTION \
                 READ WRITE; INSERT INTO varve_charset VALUES (1)"
                    .to_owned(),
            ] {
                let error = expect_error(
                    &target,
                    session.execute(&sql, &ExecOptions::default()).await,
                )?;
                check(
                    &target,
                    error.kind == ErrorKind::Unsupported
                        && error.message.contains("Varve talks utf8mb4 to the server"),
                    format!("{sql}: {error:?}"),
                )?;
                check(
                    &target,
                    scalar(&target, session, "SELECT @marker IS NULL").await? == Value::Int(1),
                    format!("{sql}: the first statement ran"),
                )?;
            }
            // Converting text is fine.
            let output = run(
                &target,
                session,
                "SELECT CONVERT('caf\u{e9}' USING latin1), CAST('a' AS CHAR CHARACTER SET latin1)",
            )
            .await?;
            check(&target, output.results.len() == 1, "CONVERT")?;
        }

        // A mysqldump header sets utf8mb4, which changes nothing for Varve:
        // it runs in a normal session, and another charset is refused.
        let output = run(&target, &normal, DUMP_HEADER).await?;
        check(
            &target,
            !output
                .notices
                .iter()
                .any(|notice| notice.message.contains("utf8mb4")),
            format!("dump header: {:?}", output.notices),
        )?;
        check(
            &target,
            client_charset(&target, &normal).await? == utf8mb4,
            "dump header changed the charset",
        )?;
        run(&target, &normal, DUMP_FOOTER_SAFE).await?;
        let latin1_header = DUMP_HEADER.replace("SET NAMES utf8mb4", "SET NAMES latin1");
        let error = expect_error(
            &target,
            normal
                .execute(&latin1_header, &ExecOptions::default())
                .await,
        )?;
        check(
            &target,
            error.kind == ErrorKind::Unsupported
                && error.message.contains("Varve talks utf8mb4 to the server"),
            format!("SET NAMES latin1: {error:?}"),
        )?;

        // A complete dump (made with the engine's own dump tool: header, a
        // table, footer) is refused as it is: its footer sets the charset
        // from a variable. After normalize_dump_boilerplate it runs, and
        // the charset stays utf8mb4 without a warning.
        let dump = match target.engine {
            Engine::Mariadb => MARIADB_DUMP,
            _ => MYSQL_DUMP,
        };
        let error = expect_error(&target, normal.execute(dump, &ExecOptions::default()).await)?;
        check(
            &target,
            error.kind == ErrorKind::Unsupported
                && error.message.contains("Varve talks utf8mb4 to the server"),
            format!("raw dump: {error:?}"),
        )?;
        check(
            &target,
            scalar(
                &target,
                &normal,
                "SELECT COUNT(*) FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'varve_dump'",
            )
            .await?
                == Value::Int(0),
            "the raw dump ran",
        )?;
        let normalized = normalize_dump_boilerplate(dump);
        let output = run(&target, &normal, &normalized).await?;
        check(
            &target,
            !output
                .notices
                .iter()
                .any(|notice| notice.message.contains("character set")),
            format!("dump: {:?}", output.notices),
        )?;
        check(
            &target,
            client_charset(&target, &normal).await? == utf8mb4,
            "the dump changed the charset",
        )?;
        let loaded = run(&target, &normal, "SELECT name FROM varve_dump ORDER BY id").await?;
        check(
            &target,
            *rows(&loaded, 0)?
                == vec![
                    vec![Value::Text("caf\u{e9}".into())],
                    vec![Value::Text("O'Brien".into())],
                ],
            format!("loaded rows: {:?}", loaded.results),
        )?;
        run(&target, &normal, "DROP TABLE varve_dump").await?;

        // A routine changes it: a read-only session restores it and stops.
        let error = expect_error(
            &target,
            guarded
                .execute(
                    "CALL varve_charset_charset(); INSERT INTO varve_charset VALUES (2)",
                    &ExecOptions::default(),
                )
                .await,
        )?;
        check(
            &target,
            error.kind == ErrorKind::ReadOnly
                && error
                    .message
                    .starts_with("Statement 1 changed the client character set"),
            format!("read-only routine: {error:?}"),
        )?;
        check(
            &target,
            client_charset(&target, &guarded).await? == utf8mb4,
            "read-only charset not restored",
        )?;
        // A normal session restores it with a warning.
        let output = run(&target, &normal, "CALL varve_charset_charset()").await?;
        check(
            &target,
            output
                .notices
                .iter()
                .any(|notice| notice.message.contains("set back to utf8mb4")),
            format!("normal routine: {:?}", output.notices),
        )?;
        check(
            &target,
            client_charset(&target, &normal).await? == utf8mb4,
            "normal charset not restored",
        )?;
        check(
            &target,
            bypass_rows(&target, &root, "varve_charset").await? == Value::Int(0),
            "a write got through",
        )?;
        drop_routines(&target, &root, "varve_charset").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn a_failed_restore_closes_the_session() -> TestResult {
    for target in targets() {
        let root = open(&target, &root_params(&target)).await?;
        create_routines(&target, &root, "varve_poison").await?;
        // The proxy breaks the driver's ROLLBACK, so restoring fails while
        // the connection itself stays fine.
        let proxy = proxy::start(
            upstream(&target),
            proxy::ProxyOptions {
                rewrite: Some((b"ROLLBACK", b"ROLLBACX")),
                ..proxy::ProxyOptions::default()
            },
        )
        .await?;
        let mut read_only = through(&target, proxy.port);
        read_only.read_only = true;
        let session = open(&target, &read_only).await?;
        let error = expect_error(
            &target,
            session
                .execute(
                    "CALL varve_poison_switch(); INSERT INTO varve_poison VALUES (1)",
                    &ExecOptions::default(),
                )
                .await,
        )?;
        check(
            &target,
            error.kind == ErrorKind::ConnectionLost && error.message.contains("restoring"),
            format!("failed restore: {error:?}"),
        )?;
        // Never a half-working session with writes enabled.
        for sql in ["SELECT 1", "INSERT INTO varve_poison VALUES (2)"] {
            let error = expect_error(&target, session.execute(sql, &ExecOptions::default()).await)?;
            check(
                &target,
                error.kind == ErrorKind::ConnectionLost,
                format!("{sql} after a failed restore: {error:?}"),
            )?;
        }
        let ping = session.ping().await;
        check(
            &target,
            matches!(&ping, Err(error) if error.kind == ErrorKind::ConnectionLost),
            format!("ping: {ping:?}"),
        )?;
        check(
            &target,
            session.in_transaction().is_none(),
            "a closed session knows no transaction",
        )?;
        check(
            &target,
            bypass_rows(&target, &root, "varve_poison").await? == Value::Int(0),
            "a write got through",
        )?;
        drop_routines(&target, &root, "varve_poison").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn normal_sessions_restore_tracking() -> TestResult {
    let plus_two = Value::Timestamp("2026-10-19T13:45:00".into());
    let utc = Value::TimestampTz("2026-10-19T11:45:00Z".into());
    for target in targets() {
        let root = open(&target, &root_params(&target)).await?;
        run(
            &target,
            &root,
            "DROP PROCEDURE IF EXISTS varve_tz_narrow; CREATE PROCEDURE varve_tz_narrow() BEGIN \
             SET SESSION session_track_system_variables = 'autocommit'; \
             SET time_zone = '+02:00'; END",
        )
        .await?;
        let session = session_for(&target).await?;
        run(
            &target,
            &session,
            "CREATE TEMPORARY TABLE tz (ts TIMESTAMP NULL);
             INSERT INTO tz VALUES (FROM_UNIXTIME(1792410300))",
        )
        .await?;
        // The routine blinds tracking and changes the zone: the results of
        // the call have no zone, and tracking is restored with a warning.
        let output = run(
            &target,
            &session,
            "CALL varve_tz_narrow(); SELECT ts FROM tz",
        )
        .await?;
        let last = output.results.len().saturating_sub(1);
        check(
            &target,
            *rows(&output, last)? == vec![vec![plus_two.clone()]]
                && output
                    .notices
                    .iter()
                    .any(|notice| notice.message.contains("tracking")),
            format!("{:?} {:?}", output.results, output.notices),
        )?;
        check(
            &target,
            scalar(&target, &session, "SELECT ts FROM tz").await? == plus_two,
            "zone after the restore",
        )?;
        // Tracking works again: a change back to UTC is seen at once.
        let output = run(
            &target,
            &session,
            "SET time_zone = '+00:00'; SELECT ts FROM tz",
        )
        .await?;
        check(
            &target,
            *rows(&output, 1)? == vec![vec![utc.clone()]],
            format!("after restore: {:?}", output.results),
        )?;
        run(&target, &root, "DROP PROCEDURE varve_tz_narrow").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn execute_with_session_applies_and_restores_settings() -> TestResult {
    for target in targets() {
        let session = session_for(&target).await?;
        let before = run(
            &target,
            &session,
            "SELECT @@session.sql_mode, @@session.time_zone, @@session.character_set_client, \
             @@session.collation_connection",
        )
        .await?;
        let before = rows(&before, 0)?.first().cloned().unwrap_or_default();
        let latin1 = SessionOverrides {
            character_set_client: Some("latin1".into()),
            collation_connection: Some("latin1_swedish_ci".into()),
            sql_mode: Some("STRICT_ALL_TABLES".into()),
            time_zone: Some("+05:00".into()),
        };
        let output = session
            .execute_with_session(
                &latin1,
                "DROP PROCEDURE IF EXISTS varve_latin1_proc;
                 CREATE PROCEDURE varve_latin1_proc() SELECT 1;
                 SELECT @@session.character_set_client, @@session.time_zone, @@session.sql_mode",
                &ExecOptions::default(),
            )
            .await
            .map_err(|error| on(&target, format!("execute_with_session: {error:?}")))?;
        check(
            &target,
            *rows(&output, 2)?
                == vec![vec![
                    Value::Text("latin1".into()),
                    Value::Text("+05:00".into()),
                    Value::Text("STRICT_ALL_TABLES".into()),
                ]],
            format!("inside the call: {:?}", output.results),
        )?;
        let routine = run(
            &target,
            &session,
            "SELECT CHARACTER_SET_CLIENT, COLLATION_CONNECTION, SQL_MODE FROM \
             information_schema.ROUTINES WHERE ROUTINE_SCHEMA = DATABASE() \
             AND ROUTINE_NAME = 'varve_latin1_proc'",
        )
        .await?;
        check(
            &target,
            *rows(&routine, 0)?
                == vec![vec![
                    Value::Text("latin1".into()),
                    Value::Text("latin1_swedish_ci".into()),
                    Value::Text("STRICT_ALL_TABLES".into()),
                ]],
            format!("routine metadata: {:?}", routine.results),
        )?;
        let after = run(
            &target,
            &session,
            "SELECT @@session.sql_mode, @@session.time_zone, @@session.character_set_client, \
             @@session.collation_connection",
        )
        .await?;
        check(
            &target,
            rows(&after, 0)?.first() == Some(&before),
            format!("restored: {:?} vs {before:?}", after.results),
        )?;

        // Refused before anything changes.
        for (bad, reason) in [
            (
                SessionOverrides {
                    character_set_client: Some("gbk".into()),
                    ..SessionOverrides::default()
                },
                "multibyte",
            ),
            (
                SessionOverrides {
                    character_set_client: Some("nosuchcharset".into()),
                    ..SessionOverrides::default()
                },
                "no character set",
            ),
            (
                SessionOverrides {
                    collation_connection: Some("latin1_nosuch_ci".into()),
                    ..SessionOverrides::default()
                },
                "no collation",
            ),
            (
                SessionOverrides {
                    sql_mode: Some("STRICT_ALL_TABLES,NOPE".into()),
                    ..SessionOverrides::default()
                },
                "not an sql_mode",
            ),
            (
                SessionOverrides {
                    time_zone: Some("Mars/Olympus".into()),
                    ..SessionOverrides::default()
                },
                "time zone",
            ),
        ] {
            let error = expect_error(
                &target,
                session
                    .execute_with_session(&bad, "SET @marker = 1", &ExecOptions::default())
                    .await,
            )?;
            check(
                &target,
                error.kind == ErrorKind::Unsupported && error.message.contains(reason),
                format!("{bad:?}: {error:?}"),
            )?;
            check(
                &target,
                scalar(&target, &session, "SELECT @marker IS NULL").await? == Value::Int(1),
                format!("{bad:?}: the SQL ran"),
            )?;
        }
        // A TIMESTAMP under the overridden zone is that zone's local time,
        // without a zone.
        let output = session
            .execute_with_session(
                &SessionOverrides {
                    time_zone: Some("+05:00".into()),
                    ..SessionOverrides::default()
                },
                "CREATE TEMPORARY TABLE tz_override (ts TIMESTAMP NULL);
                 INSERT INTO tz_override VALUES (FROM_UNIXTIME(1792410300));
                 SELECT ts FROM tz_override",
                &ExecOptions::default(),
            )
            .await
            .map_err(|error| on(&target, format!("+05:00: {error:?}")))?;
        check(
            &target,
            *rows(&output, 2)? == vec![vec![Value::Timestamp("2026-10-19T16:45:00".into())]],
            format!("+05:00 timestamp: {:?}", output.results),
        )?;
        check(
            &target,
            scalar(&target, &session, "SELECT ts FROM tz_override").await?
                == Value::TimestampTz("2026-10-19T11:45:00Z".into()),
            "zone after the override",
        )?;
        // utf8 has another name on these servers.
        let error = expect_error(
            &target,
            session
                .execute_with_session(
                    &SessionOverrides {
                        character_set_client: Some("utf8".into()),
                        ..SessionOverrides::default()
                    },
                    "SELECT 1",
                    &ExecOptions::default(),
                )
                .await,
        )?;
        check(
            &target,
            error.kind == ErrorKind::Unsupported && error.message.contains("utf8mb3"),
            format!("utf8: {error:?}"),
        )?;
        // A call dropped halfway is put right by the next one.
        let dropped = tokio::time::timeout(
            Duration::from_millis(300),
            session.execute_with_session(&latin1, "SELECT SLEEP(1)", &ExecOptions::default()),
        )
        .await;
        check(&target, dropped.is_err(), "the call was not dropped")?;
        let after = run(
            &target,
            &session,
            "SELECT @@session.sql_mode, @@session.time_zone, @@session.character_set_client, \
             @@session.collation_connection",
        )
        .await?;
        check(
            &target,
            rows(&after, 0)?.first() == Some(&before),
            format!("after a dropped call: {:?} vs {before:?}", after.results),
        )?;

        // The guards still apply.
        let error = expect_error(
            &target,
            session
                .execute_with_session(&latin1, "SET NAMES gbk", &ExecOptions::default())
                .await,
        )?;
        check(
            &target,
            error.kind == ErrorKind::Unsupported,
            format!("{error:?}"),
        )?;
        run(&target, &session, "DROP PROCEDURE varve_latin1_proc").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn sessions_are_utf8mb4_whatever_the_server_sets() -> TestResult {
    let utf8mb4 = Value::Text("utf8mb4".into());
    for target in targets() {
        let root = open(&target, &root_params(&target)).await?;
        // init_connect runs for every login of a user without SUPER.
        run(&target, &root, "SET GLOBAL init_connect = 'SET NAMES gbk'").await?;
        let outcome = async {
            let mut read_only = params(&target);
            read_only.read_only = true;
            for params in [params(&target), read_only] {
                let session = open(&target, &params).await?;
                let state = run(
                    &target,
                    &session,
                    "SELECT @@session.character_set_client, @@session.character_set_connection, \
                     @@session.character_set_results, HEX('\u{e9}')",
                )
                .await?;
                let row = rows(&state, 0)?.first().cloned().unwrap_or_default();
                check(
                    &target,
                    row == vec![
                        utf8mb4.clone(),
                        utf8mb4.clone(),
                        utf8mb4.clone(),
                        Value::Text("C3A9".into()),
                    ],
                    format!("read_only={}: {row:?}", params.read_only),
                )?;
            }
            // The auditor's init SQL is refused.
            for init_sql in [
                "SET @\u{e9} = @@character_set_client; SET @e = 28; SET character_set_client = @\u{e9}",
                "SET @a = /*!1006001 , NAMES gbk */",
            ] {
                let mut bad = params(&target);
                bad.init_sql = Some(init_sql.into());
                let error = expect_error(&target, MysqlDriver::new().open(&bad).await)?;
                check(
                    &target,
                    error.kind == ErrorKind::Unsupported,
                    format!("{init_sql}: {error:?}"),
                )?;
            }
            Ok::<(), Box<dyn Error>>(())
        }
        .await;
        run(&target, &root, "SET GLOBAL init_connect = ''").await?;
        outcome?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn sessions_without_tracking_reread_their_state() -> TestResult {
    for target in targets() {
        let root = open(&target, &root_params(&target)).await?;
        create_routines(&target, &root, "varve_notrack").await?;
        run(
            &target,
            &root,
            "DROP PROCEDURE IF EXISTS varve_notrack_tz; \
             CREATE PROCEDURE varve_notrack_tz() SET time_zone = '+02:00'",
        )
        .await?;
        let proxy = proxy::start(
            upstream(&target),
            proxy::ProxyOptions {
                strip_session_track: true,
                ..proxy::ProxyOptions::default()
            },
        )
        .await?;
        let params = through(&target, proxy.port);
        // Read-only needs tracking.
        let mut read_only = params.clone();
        read_only.read_only = true;
        let error = expect_error(&target, MysqlDriver::new().open(&read_only).await)?;
        check(
            &target,
            error.kind == ErrorKind::Unsupported && error.message.contains("session tracking"),
            format!("read-only without tracking: {error:?}"),
        )?;
        let session = open(&target, &params).await?;
        run(
            &target,
            &session,
            "CREATE TEMPORARY TABLE tz (ts TIMESTAMP NULL);
             INSERT INTO tz VALUES (FROM_UNIXTIME(1792410300))",
        )
        .await?;
        // A routine changes the charset: restored with a warning.
        let output = run(&target, &session, "CALL varve_notrack_charset()").await?;
        check(
            &target,
            output
                .notices
                .iter()
                .any(|notice| notice.message.contains("set back to utf8mb4")),
            format!("charset: {:?}", output.notices),
        )?;
        check(
            &target,
            client_charset(&target, &session).await? == Value::Text("utf8mb4".into()),
            "charset not restored",
        )?;
        // A routine changes the zone: it is read again.
        run(&target, &session, "CALL varve_notrack_tz()").await?;
        check(
            &target,
            scalar(&target, &session, "SELECT ts FROM tz").await?
                == Value::Timestamp("2026-10-19T13:45:00".into()),
            "zone after the routine",
        )?;
        run(&target, &root, "DROP PROCEDURE varve_notrack_tz").await?;
        drop_routines(&target, &root, "varve_notrack").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn dropped_calls_are_put_right() -> TestResult {
    for target in targets() {
        let mut read_only = params(&target);
        read_only.read_only = true;
        let session = open(&target, &read_only).await?;
        let dropped = tokio::time::timeout(
            Duration::from_millis(300),
            session.execute("SELECT SLEEP(1)", &ExecOptions::default()),
        )
        .await;
        check(&target, dropped.is_err(), "the call was not dropped")?;
        let variable = read_only_variable(&target);
        check(
            &target,
            scalar(&target, &session, &format!("SELECT @@session.{variable}")).await?
                == Value::Int(1),
            "read-only after a dropped call",
        )?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn latin1_replay_round_trips() -> TestResult {
    let latin1 = SessionOverrides {
        character_set_client: Some("latin1".into()),
        collation_connection: Some("latin1_swedish_ci".into()),
        ..SessionOverrides::default()
    };
    for target in targets() {
        let root = open(&target, &root_params(&target)).await?;
        root.execute_with_session(
            &latin1,
            "DROP DATABASE IF EXISTS `varve_b\u{e4}ck`;
             CREATE DATABASE `varve_b\u{e4}ck`;
             CREATE PROCEDURE `varve_b\u{e4}ck`.p() SELECT 'caf\u{e9}' AS v",
            &ExecOptions::default(),
        )
        .await
        .map_err(|error| on(&target, format!("latin1 replay: {error:?}")))?;
        check(
            &target,
            scalar(&target, &root, "CALL `varve_b\u{e4}ck`.p()").await?
                == Value::Text("caf\u{e9}".into()),
            "procedure result",
        )?;
        let routine = run(
            &target,
            &root,
            "SELECT ROUTINE_SCHEMA, CHARACTER_SET_CLIENT FROM information_schema.ROUTINES \
             WHERE ROUTINE_NAME = 'p' AND ROUTINE_SCHEMA = 'varve_b\u{e4}ck'",
        )
        .await?;
        check(
            &target,
            *rows(&routine, 0)?
                == vec![vec![
                    Value::Text("varve_b\u{e4}ck".into()),
                    Value::Text("latin1".into()),
                ]],
            format!("routine: {:?}", routine.results),
        )?;
        // A character latin1 cannot hold is refused before anything runs.
        let error = expect_error(
            &target,
            root.execute_with_session(&latin1, "SELECT '\u{2713}'", &ExecOptions::default())
                .await,
        )?;
        check(
            &target,
            error.kind == ErrorKind::Unsupported && error.message.contains("cannot represent"),
            format!("check mark: {error:?}"),
        )?;
        run(&target, &root, "DROP DATABASE `varve_b\u{e4}ck`").await?;
    }
    Ok(())
}

/// Runs `sql` and checks [`Session::in_transaction`] afterwards.
async fn transaction_after(
    target: &Target,
    session: &MysqlSession,
    sql: &str,
    expected: Option<bool>,
) -> TestResult {
    // Errors are part of some steps; the state after them is what counts.
    let outcome = session.execute(sql, &ExecOptions::default()).await;
    let state = session.in_transaction();
    check(
        target,
        state == expected,
        format!("{sql}: in_transaction {state:?}, expected {expected:?} ({outcome:?})"),
    )
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn in_transaction_follows_the_server() -> TestResult {
    for target in targets() {
        // The test account has no PROCESS privilege: the status comes from
        // the replies, not from a processlist query.
        let session = Arc::new(session_for(&target).await?);
        check(
            &target,
            session.in_transaction() == Some(false),
            format!("after connect: {:?}", session.in_transaction()),
        )?;
        run(
            &target,
            &session,
            "DROP TABLE IF EXISTS varve_trx; CREATE TABLE varve_trx (id INT)",
        )
        .await?;
        let steps: [(&str, Option<bool>); 10] = [
            ("SELECT 1", Some(false)),
            ("START TRANSACTION", Some(true)),
            ("INSERT INTO varve_trx VALUES (1)", Some(true)),
            ("COMMIT", Some(false)),
            ("BEGIN; INSERT INTO varve_trx VALUES (2)", Some(true)),
            ("ROLLBACK", Some(false)),
            // DDL commits implicitly.
            (
                "START TRANSACTION; INSERT INTO varve_trx VALUES (3); \
                 CREATE TABLE IF NOT EXISTS varve_trx2 (id INT)",
                Some(false),
            ),
            // An error inside a transaction leaves it open.
            ("START TRANSACTION", Some(true)),
            ("SELECT * FROM varve_no_such_table", Some(true)),
            ("ROLLBACK", Some(false)),
        ];
        for (sql, expected) in steps {
            transaction_after(&target, &session, sql, expected).await?;
        }
        // The error stays visible to SHOW ERRORS in the next call.
        let failed = session
            .execute("SELECT * FROM varve_no_such_table", &ExecOptions::default())
            .await;
        check(&target, failed.is_err(), "the query should fail")?;
        let errors = run(&target, &session, "SHOW ERRORS").await?;
        check(
            &target,
            rows(&errors, 0)?
                .first()
                .is_some_and(|row| row.get(1) == Some(&Value::Int(1146))),
            format!("SHOW ERRORS after an error: {:?}", errors.results),
        )?;

        // A cancelled statement inside a transaction.
        transaction_after(&target, &session, "START TRANSACTION", Some(true)).await?;
        let running = Arc::clone(&session);
        let task =
            tokio::spawn(async move { running.execute(HEAVY, &ExecOptions::default()).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        session
            .cancel_handle()
            .cancel()
            .await
            .map_err(|error| on(&target, error))?;
        let outcome = tokio::time::timeout(Duration::from_secs(3), task).await??;
        check(
            &target,
            matches!(&outcome, Err(error) if error.kind == ErrorKind::Cancelled)
                && session.in_transaction() == Some(true),
            format!("after a cancel: {:?} {outcome:?}", session.in_transaction()),
        )?;
        transaction_after(&target, &session, "ROLLBACK", Some(false)).await?;

        // execute_with_session keeps the state its SQL left.
        session
            .execute_with_session(
                &SessionOverrides {
                    time_zone: Some("+01:00".into()),
                    ..SessionOverrides::default()
                },
                "START TRANSACTION",
                &ExecOptions::default(),
            )
            .await
            .map_err(|error| on(&target, error))?;
        check(
            &target,
            session.in_transaction() == Some(true),
            "after execute_with_session",
        )?;
        transaction_after(&target, &session, "ROLLBACK", Some(false)).await?;
        session.ping().await.map_err(|error| on(&target, error))?;
        check(
            &target,
            session.in_transaction() == Some(false),
            "after ping",
        )?;

        // A read-only session.
        let mut read_only = params(&target);
        read_only.read_only = true;
        let guarded = open(&target, &read_only).await?;
        transaction_after(&target, &guarded, "START TRANSACTION READ ONLY", Some(true)).await?;
        transaction_after(
            &target,
            &guarded,
            "SELECT COUNT(*) FROM varve_trx",
            Some(true),
        )
        .await?;
        transaction_after(&target, &guarded, "COMMIT", Some(false)).await?;

        run(
            &target,
            &session,
            "DROP TABLE varve_trx; DROP TABLE IF EXISTS varve_trx2",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn connect_timeout_covers_init_sql() -> TestResult {
    for target in targets() {
        let mut params = params(&target);
        params.init_sql = Some("select sleep(10)".into());
        params.connect_timeout = Duration::from_secs(2);
        let started = Instant::now();
        let error = expect_error(&target, MysqlDriver::new().open(&params).await)?;
        check(
            &target,
            error.kind == ErrorKind::Timeout,
            format!("{error:?}"),
        )?;
        check(
            &target,
            started.elapsed() < Duration::from_secs(4),
            format!("took {:?}", started.elapsed()),
        )?;
    }
    Ok(())
}

/// Parameters that reach `target` through a proxy on `port`.
fn through(target: &Target, port: u16) -> ConnectParams {
    let mut params = params(target);
    params.host = "127.0.0.1".into();
    params.port = port;
    params
}

fn upstream(target: &Target) -> String {
    format!(
        "{}:{}",
        env_or("VARVE_MYSQL_HOST", "127.0.0.1"),
        target.port
    )
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn late_kills_never_hit_the_next_call() -> TestResult {
    for target in targets() {
        // Every KILL QUERY reaches the server 800 ms after it was sent.
        let proxy = proxy::start(
            upstream(&target),
            proxy::ProxyOptions {
                kill_delay: Some(Duration::from_millis(800)),
                ..proxy::ProxyOptions::default()
            },
        )
        .await?;
        let session = Arc::new(open(&target, &through(&target, proxy.port)).await?);
        let handle = session.cancel_handle();
        let running = Arc::clone(&session);
        let first = tokio::spawn(async move {
            running
                .execute("SELECT SLEEP(0.6)", &ExecOptions::default())
                .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        let cancel = tokio::spawn(async move { handle.cancel().await });

        // The script finished before the kill arrived: its results come
        // back, with a note, and it is not reported as cancelled.
        let output = tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .map_err(|_| on(&target, "the first call hung"))??
            .map_err(|error| on(&target, format!("first call: {error:?}")))?;
        check(
            &target,
            *rows(&output, 0)? == vec![vec![Value::Int(0)]],
            format!("{output:?}"),
        )?;
        check(
            &target,
            output
                .notices
                .iter()
                .any(|notice| notice.message.contains("came too late")),
            format!("{:?}", output.notices),
        )?;

        // The late KILL must not interrupt the next call.
        let started = Instant::now();
        let slept = scalar(&target, &session, "SELECT SLEEP(1)").await?;
        check(
            &target,
            slept == Value::Int(0) && started.elapsed() >= Duration::from_millis(950),
            format!("next call: {slept:?} after {:?}", started.elapsed()),
        )?;
        cancel
            .await?
            .map_err(|error| on(&target, format!("cancel: {error:?}")))?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn a_cancel_after_the_server_finished_returns_the_results() -> TestResult {
    for target in targets() {
        let writer = session_for(&target).await?;
        run(
            &target,
            &writer,
            "DROP TABLE IF EXISTS varve_late_cancel; CREATE TABLE varve_late_cancel (id INT)",
        )
        .await?;
        // The server sends everything at once; the client gets it over
        // about three seconds.
        let proxy = proxy::start(
            upstream(&target),
            proxy::ProxyOptions {
                slow_delivery: Some(Duration::from_millis(20)),
                ..proxy::ProxyOptions::default()
            },
        )
        .await?;
        let session = Arc::new(open(&target, &through(&target, proxy.port)).await?);
        let running = Arc::clone(&session);
        let task = tokio::spawn(async move {
            running
                .execute(
                    "INSERT INTO varve_late_cancel VALUES (1);
                     SELECT REPEAT('x', 1000) FROM information_schema.columns a,
                            information_schema.columns b LIMIT 1500",
                    &ExecOptions { max_rows: 10 },
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // The precondition: the server is idle, the client still reading.
        let command = scalar(
            &target,
            &writer,
            &format!(
                "SELECT COMMAND FROM information_schema.processlist WHERE ID = {}",
                session.connection_id()
            ),
        )
        .await?;
        check(
            &target,
            command == Value::Text("Sleep".into()) && !task.is_finished(),
            format!(
                "server thread {command:?}, call finished: {}",
                task.is_finished()
            ),
        )?;
        session
            .cancel_handle()
            .cancel()
            .await
            .map_err(|error| on(&target, error))?;
        let output = tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .map_err(|_| on(&target, "the call hung"))??
            .map_err(|error| on(&target, format!("reported as {error:?}")))?;
        check(
            &target,
            output.results.len() == 2
                && output
                    .notices
                    .iter()
                    .any(|notice| notice.message.contains("came too late")),
            format!(
                "{} results, notices {:?}",
                output.results.len(),
                output.notices
            ),
        )?;
        check(
            &target,
            matches!(output.results.get(1), Some(StatementResult::Rows { rows, truncated: true, .. }) if rows.len() == 10),
            "the row set is incomplete",
        )?;
        check(
            &target,
            scalar(&target, &writer, "SELECT COUNT(*) FROM varve_late_cancel").await?
                == Value::Int(1),
            "the INSERT did not commit",
        )?;
        run(&target, &writer, "DROP TABLE varve_late_cancel").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn cancel_uses_the_server_connection_id() -> TestResult {
    for target in targets() {
        // Like a pooling proxy: the handshake carries another id.
        let fake = 4_000_000_000_u32;
        let proxy = proxy::start(
            upstream(&target),
            proxy::ProxyOptions {
                fake_connection_id: Some(fake),
                ..proxy::ProxyOptions::default()
            },
        )
        .await?;
        let params = through(&target, proxy.port);
        let session = open(&target, &params).await?;
        let id = scalar(&target, &session, "SELECT CONNECTION_ID()").await?;
        check(
            &target,
            session.connection_id() != u64::from(fake)
                && i64::try_from(session.connection_id()).is_ok_and(|own| id == Value::Int(own)),
            format!("{} vs {id:?}", session.connection_id()),
        )?;
        cancel_while_running_with(&target, &params, HEAVY).await?;
    }
    Ok(())
}

/// A self-signed CA that signed none of the servers' certificates.
fn unrelated_ca() -> Result<std::path::PathBuf, Box<dyn Error>> {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new())?;
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate()?;
    let cert = ca.self_signed(&key)?;
    let path = std::env::temp_dir().join(format!("varve-mysql-test-ca-{}.pem", std::process::id()));
    std::fs::write(&path, cert.pem())?;
    Ok(path)
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn tls_modes() -> TestResult {
    let unrelated = unrelated_ca()?;
    for target in targets() {
        let mut params = params(&target);

        // Prefer: TLS when offered, plain otherwise.
        params.tls.mode = TlsMode::Prefer;
        let session = open(&target, &params).await?;
        check(&target, session.uses_tls() == target.tls, "Prefer")?;
        let cipher = run(&target, &session, "SHOW SESSION STATUS LIKE 'Ssl_cipher'").await?;
        let cipher = rows(&cipher, 0)?
            .first()
            .and_then(|row| row.get(1))
            .cloned()
            .unwrap_or(Value::Null);
        check(
            &target,
            (cipher != Value::Text(String::new())) == target.tls,
            format!("Ssl_cipher {cipher:?}"),
        )?;

        // Require: encrypted without certificate checks.
        params.tls.mode = TlsMode::Require;
        let result = MysqlDriver::new().open(&params).await;
        if target.tls {
            let session = result.map_err(|error| on(&target, error))?;
            check(&target, session.uses_tls(), "Require")?;
            // Cancel goes through TLS too.
            session
                .cancel_handle()
                .cancel()
                .await
                .map_err(|error| on(&target, error))?;
        } else {
            let error = expect_error(&target, result)?;
            check(&target, error.kind == ErrorKind::Tls, format!("{error:?}"))?;
        }

        // VerifyFull and VerifyCa: the auto-generated certificates are not
        // trusted by the OS store or by an unrelated CA.
        for mode in [TlsMode::VerifyFull, TlsMode::VerifyCa] {
            params.tls.mode = mode;
            params.tls.ca_path = None;
            // Servers without TLS fail before any certificate is seen.
            let reason = if target.tls {
                "UnknownIssuer"
            } else {
                "does not support TLS"
            };
            let error = expect_error(&target, MysqlDriver::new().open(&params).await)?;
            check(
                &target,
                error.kind == ErrorKind::Tls && error.message.contains(reason),
                format!("{mode:?}: {error:?}"),
            )?;
            params.tls.ca_path = Some(unrelated.display().to_string());
            let error = expect_error(&target, MysqlDriver::new().open(&params).await)?;
            check(
                &target,
                error.kind == ErrorKind::Tls && error.message.contains(reason),
                format!("{mode:?}: {error:?}"),
            )?;
        }
        params.tls.ca_path = None;
    }

    let _ = std::fs::remove_file(unrelated);
    Ok(())
}
