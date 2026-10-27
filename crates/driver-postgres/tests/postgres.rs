//! Integration tests against the PostgreSQL servers of `dev/docker-compose.yml`:
//!
//! ```sh
//! docker compose -p varve-m1-pg -f dev/docker-compose.yml up -d --wait postgres-13 postgres-18
//! cargo test -p varve-driver-postgres -- --include-ignored
//! ```

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    TRANSLATED_CANCEL, TRANSLATED_TIMEOUT, TestResult, assert_dead, backend_pid, connect,
    cutting_proxy, dead_sessions_fail_fast, delaying_proxy, newest, oldest, params, password,
    proxy, rewriting_proxy, rewriting_proxy_rules, rows, run, scalar, servers, translating_proxy,
    wait_until_dead,
};
use varve_core::Engine;
use varve_driver::{Column, ErrorKind, ExecOptions, Notice, Session, StatementResult, Value};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn connects_and_reads_server_info() -> TestResult {
    for (major, port) in servers() {
        let session = connect(&params(port)).await?;
        let info = session.server_info();
        assert_eq!(info.engine, Engine::Postgres);
        assert_eq!(info.major, major, "{info:?}");
        assert!(info.version.starts_with(&format!("{major}.")), "{info:?}");
        session.ping().await?;
        assert_eq!(
            scalar(session.as_ref(), "SHOW application_name").await?,
            Value::Text("varve-driver-postgres tests".into())
        );
        assert_eq!(
            scalar(session.as_ref(), "SHOW TimeZone").await?,
            Value::Text("UTC".into())
        );
        assert_eq!(
            scalar(session.as_ref(), "SHOW IntervalStyle").await?,
            Value::Text("iso_8601".into())
        );
        assert!(
            session
                .as_any()
                .downcast_ref::<varve_driver_postgres::PostgresSession>()
                .is_some()
        );
        session.close().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn defaults_to_the_postgres_database() -> TestResult {
    let (_, port) = newest();
    let mut params = params(port);
    params.database = None;
    let session = connect(&params).await?;
    assert_eq!(
        scalar(session.as_ref(), "SELECT current_database()").await?,
        Value::Text("postgres".into())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn wrong_password_is_an_auth_error() -> TestResult {
    for (_, port) in servers() {
        let mut params = params(port);
        let wrong = "definitely-not-the-password";
        params.password = Some(varve_secrets::Secret::new(wrong.into()));
        let error = connect(&params).await.err().ok_or("connected")?;
        assert_eq!(error.kind, ErrorKind::Auth, "{error:?}");
        assert_eq!(error.code.as_deref(), Some("28P01"));
        assert!(!error.message.contains(wrong));
        assert!(!error.message.contains(&password()));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn refused_and_unknown_hosts_are_connect_errors() -> TestResult {
    let refused = connect(&params(1)).await.err().ok_or("connected")?;
    assert_eq!(refused.kind, ErrorKind::Connect, "{refused:?}");

    let mut unknown = params(5432);
    unknown.host = "no-such-host.invalid".into();
    let error = connect(&unknown).await.err().ok_or("connected")?;
    assert_eq!(error.kind, ErrorKind::Connect, "{error:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn connect_timeout_is_a_timeout() -> TestResult {
    // A non-routable address: the TCP handshake never completes.
    let mut params = params(5432);
    params.host = "10.255.255.1".into();
    params.connect_timeout = Duration::from_millis(300);
    let started = Instant::now();
    let error = connect(&params).await.err().ok_or("connected")?;
    assert!(
        matches!(error.kind, ErrorKind::Timeout | ErrorKind::Connect),
        "{error:?}"
    );
    if error.kind == ErrorKind::Connect {
        // Some networks reject the address at once instead of dropping it.
        assert!(started.elapsed() < Duration::from_millis(300));
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn connect_timeout_covers_the_init_sql() -> TestResult {
    for (_, port) in servers() {
        let mut params = params(port);
        params.init_sql = Some("select pg_sleep(10)".into());
        params.connect_timeout = Duration::from_millis(1500);
        let started = Instant::now();
        let error = connect(&params).await.err().ok_or("connected")?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_blocks_writes() -> TestResult {
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DROP TABLE IF EXISTS varve_ro_writes; CREATE TABLE varve_ro_writes (id int); \
             INSERT INTO varve_ro_writes VALUES (1)",
        )
        .await?;
        let mut params = params(port);
        params.read_only = true;
        let session = connect(&params).await?;
        for sql in [
            "CREATE TABLE varve_read_only_probe (id int)",
            "CREATE TEMP TABLE varve_read_only_temp (id int)",
            "ALTER TABLE varve_ro_writes ADD COLUMN extra int",
            "DROP TABLE varve_ro_writes",
            "INSERT INTO varve_ro_writes VALUES (2)",
            "UPDATE varve_ro_writes SET id = 3",
            "DELETE FROM varve_ro_writes",
            "TRUNCATE varve_ro_writes",
            "SELECT 1; BEGIN; INSERT INTO varve_ro_writes VALUES (4)",
        ] {
            let error = run(session.as_ref(), sql)
                .await
                .err()
                .ok_or_else(|| format!("{sql} ran on a read-only session"))?;
            assert_eq!(error.kind, ErrorKind::ReadOnly, "{sql}: {error:?}");
            assert_eq!(error.code.as_deref(), Some("25006"), "{sql}");
            run(session.as_ref(), "ROLLBACK").await?;
        }
        assert_eq!(scalar(session.as_ref(), "SELECT 1").await?, Value::Int(1));
        assert_eq!(
            scalar(writer.as_ref(), "SELECT count(*) FROM varve_ro_writes").await?,
            Value::Int(1)
        );
        run(writer.as_ref(), "DROP TABLE varve_ro_writes").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_rejects_statements_that_switch_it_off() -> TestResult {
    for (_, port) in servers() {
        let mut params = params(port);
        params.read_only = true;
        let session = connect(&params).await?;
        for (sql, statement) in [
            (
                "SET default_transaction_read_only = off",
                "SET default_transaction_read_only = off",
            ),
            (
                "SET transaction_read_only TO off",
                "SET transaction_read_only TO off",
            ),
            (
                "RESET default_transaction_read_only",
                "RESET default_transaction_read_only",
            ),
            ("RESET ALL", "RESET ALL"),
            ("DISCARD ALL", "DISCARD ALL"),
            ("BEGIN READ WRITE", "BEGIN READ WRITE"),
            (
                "START TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE",
                "START TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE",
            ),
            (
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
            ),
            (
                "BEGIN; SET TRANSACTION READ WRITE; COMMIT",
                "SET TRANSACTION READ WRITE",
            ),
            (
                "SELECT set_config('default_transaction_read_only', 'off', false)",
                "SELECT set_config('default_transaction_read_only', 'off', false)",
            ),
            // A DO block that would switch the mode off and commit inside
            // itself, before any check after the statement could react.
            (
                "DO $$ BEGIN PERFORM set_config('default_transaction_read_only', 'off', false); \
                 COMMIT; INSERT INTO varve_ro_never VALUES (1); END $$",
                "DO $$ BEGIN PERFORM set_config('default_transaction_read_only', 'off', false); \
                 COMMIT; INSERT INTO varve_ro_never VALUES (1); END $$",
            ),
            // Nothing runs, not even the statements before the violation.
            (
                "SET application_name = 'varve ran this'; SET default_transaction_read_only = off",
                "SET default_transaction_read_only = off",
            ),
        ] {
            let error = run(session.as_ref(), sql)
                .await
                .err()
                .ok_or_else(|| format!("{sql} ran on a read-only session"))?;
            assert_eq!(error.kind, ErrorKind::ReadOnly, "{sql}: {error:?}");
            assert!(
                error.message.contains(&format!("\"{statement}\"")),
                "{sql}: {error:?}"
            );
            let byte = sql.find(statement).ok_or("statement not in script")?;
            assert_eq!(
                error.position,
                Some(u32::try_from(sql[..byte].chars().count() + 1)?),
                "{sql}"
            );
        }
        assert_eq!(
            scalar(session.as_ref(), "SHOW application_name").await?,
            Value::Text("varve-driver-postgres tests".into())
        );
        assert_eq!(
            scalar(session.as_ref(), "SHOW default_transaction_read_only").await?,
            Value::Text("on".into())
        );
        assert_eq!(
            scalar(session.as_ref(), "SHOW transaction_read_only").await?,
            Value::Text("on".into())
        );

        // The init SQL is held to the same rule.
        let mut init = params.clone();
        init.init_sql = Some("SET default_transaction_read_only = off".into());
        let error = connect(&init)
            .await
            .err()
            .ok_or("init SQL switched read-only mode off")?;
        assert_eq!(error.kind, ErrorKind::ReadOnly, "{error:?}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_survives_a_function_that_switches_it_off() -> TestResult {
    for (_, port) in servers() {
        // The function hides set_config from the script check.
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DROP TABLE IF EXISTS varve_ro_target; CREATE TABLE varve_ro_target (id int);\n\
             CREATE OR REPLACE FUNCTION varve_ro_flip() RETURNS text LANGUAGE sql AS \
             $$ SELECT set_config('default_transaction_read_only', 'off', false) $$;\n\
             DROP SCHEMA IF EXISTS varve_ro_fake CASCADE; CREATE SCHEMA varve_ro_fake;\n\
             CREATE FUNCTION varve_ro_fake.current_setting(text) RETURNS text \
             LANGUAGE sql AS $$ SELECT 'on'::text $$;\n\
             CREATE OR REPLACE FUNCTION varve_ro_spoof() RETURNS text LANGUAGE sql AS \
             $$ SELECT set_config('search_path', 'varve_ro_fake, pg_catalog', false) || \
             set_config('default_transaction_read_only', 'off', false) $$",
        )
        .await?;
        let mut params = params(port);
        params.read_only = true;
        let session = connect(&params).await?;
        let warning = Notice {
            severity: "WARNING".into(),
            message: "statement 1 switched read-only mode off; Varve switched it back on".into(),
        };

        let output = run(session.as_ref(), "SELECT varve_ro_flip()").await?;
        assert_eq!(output.notices, vec![warning.clone()]);
        assert_eq!(
            scalar(session.as_ref(), "SHOW default_transaction_read_only").await?,
            Value::Text("on".into())
        );

        // The write right after the function is still refused.
        let error = run(
            session.as_ref(),
            "SELECT varve_ro_flip(); INSERT INTO varve_ro_target VALUES (1)",
        )
        .await
        .err()
        .ok_or("the INSERT after the function ran")?;
        assert_eq!(error.kind, ErrorKind::ReadOnly, "{error:?}");

        // A warning still reaches the user when the script then fails: it
        // is added to the error's detail.
        let error = run(session.as_ref(), "SELECT varve_ro_flip(); SELECT 1/0")
            .await
            .err()
            .ok_or("division by zero")?;
        assert_eq!(error.code.as_deref(), Some("22012"));
        assert_eq!(
            error.detail.as_deref(),
            Some(format!("Warning: {}", warning.message).as_str())
        );

        // A function that also hides current_setting behind its own
        // search_path: the check reads pg_catalog directly.
        let error = run(
            session.as_ref(),
            "SELECT varve_ro_spoof(); INSERT INTO public.varve_ro_target VALUES (3)",
        )
        .await
        .err()
        .ok_or("the INSERT after the spoofing function ran")?;
        assert_eq!(error.kind, ErrorKind::ReadOnly, "{error:?}");
        run(session.as_ref(), "SET search_path = public").await?;

        // Inside a transaction block, and across its COMMIT.
        let error = run(
            session.as_ref(),
            "BEGIN; SELECT varve_ro_flip(); COMMIT; INSERT INTO varve_ro_target VALUES (2)",
        )
        .await
        .err()
        .ok_or("the INSERT after COMMIT ran")?;
        assert_eq!(error.kind, ErrorKind::ReadOnly, "{error:?}");
        let output = run(
            session.as_ref(),
            "BEGIN; SELECT varve_ro_flip(); SHOW default_transaction_read_only; \
             SHOW transaction_read_only; COMMIT; SHOW default_transaction_read_only",
        )
        .await?;
        // Nothing is sent inside the transaction block: the default stays
        // off there, while the transaction itself stays read-only. Once it
        // has ended, read-only mode is back on before anything else runs.
        assert_eq!(rows(&output, 2)?, &vec![vec![Value::Text("off".into())]]);
        assert_eq!(rows(&output, 3)?, &vec![vec![Value::Text("on".into())]]);
        assert_eq!(rows(&output, 5)?, &vec![vec![Value::Text("on".into())]]);
        assert_eq!(
            output.notices,
            vec![Notice {
                message: "the transaction that ended with statement 5 switched read-only mode \
                          off; Varve switched it back on"
                    .into(),
                ..warning.clone()
            }]
        );
        // ROLLBACK undoes the function's change itself: nothing to restore.
        let output = run(
            session.as_ref(),
            "BEGIN; SELECT varve_ro_flip(); ROLLBACK; SHOW default_transaction_read_only",
        )
        .await?;
        assert_eq!(rows(&output, 3)?, &vec![vec![Value::Text("on".into())]]);
        assert!(output.notices.is_empty(), "{:?}", output.notices);

        // An init SQL that calls the function: switched back on at connect,
        // and the warning comes with the first run.
        let mut init = params.clone();
        init.init_sql = Some("SELECT varve_ro_flip()".into());
        let session = connect(&init).await?;
        let output = run(session.as_ref(), "SHOW default_transaction_read_only").await?;
        assert_eq!(rows(&output, 0)?, &vec![vec![Value::Text("on".into())]]);
        assert_eq!(output.notices.len(), 1, "{:?}", output.notices);
        assert!(output.notices[0].message.contains("init SQL"));

        assert_eq!(
            scalar(writer.as_ref(), "SELECT count(*) FROM varve_ro_target").await?,
            Value::Int(0)
        );
        run(
            writer.as_ref(),
            "DROP TABLE varve_ro_target; DROP FUNCTION varve_ro_flip(); \
             DROP FUNCTION varve_ro_spoof(); DROP SCHEMA varve_ro_fake CASCADE",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn init_sql_and_statement_timeout() -> TestResult {
    for (_, port) in servers() {
        let mut params = params(port);
        params.init_sql = Some("SET search_path = pg_catalog; SET work_mem = '8MB'".into());
        params.statement_timeout = Some(Duration::from_millis(200));
        let session = connect(&params).await?;
        assert_eq!(
            scalar(session.as_ref(), "SHOW search_path").await?,
            Value::Text("pg_catalog".into())
        );
        assert_eq!(
            scalar(session.as_ref(), "SHOW statement_timeout").await?,
            Value::Text("200ms".into())
        );
        let error = run(session.as_ref(), "SELECT pg_sleep(5)")
            .await
            .err()
            .ok_or("statement timeout did not fire")?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
        assert_eq!(error.code.as_deref(), Some("57014"));

        let mut broken = params.clone();
        broken.init_sql = Some("SELECT * FROM varve_no_such_table".into());
        let error = connect(&broken)
            .await
            .err()
            .ok_or("init SQL error ignored")?;
        assert_eq!(error.kind, ErrorKind::Query);
        assert_eq!(error.code.as_deref(), Some("42P01"));
        assert!(error.message.starts_with("init SQL failed"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn script_with_mixed_statements() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        let output = run(
            session.as_ref(),
            "CREATE TEMP TABLE mixed (id int PRIMARY KEY, name text);\n\
             INSERT INTO mixed VALUES (1, 'a'), (2, 'b'), (3, 'c');\n\
             -- a comment between statements\n\
             SELECT id, name FROM mixed ORDER BY id;\n\
             UPDATE mixed SET name = 'z' WHERE id >= 2;\n\
             SELECT count(*) AS n FROM mixed WHERE name = 'z';\n\
             DELETE FROM mixed WHERE id = 1;\n\
             BEGIN; SAVEPOINT s; ROLLBACK TO SAVEPOINT s; COMMIT;\n\
             DROP TABLE mixed;",
        )
        .await?;
        let command = |tag: &str, rows_affected: Option<u64>| StatementResult::Command {
            tag: tag.into(),
            rows_affected,
        };
        let results = &output.results;
        assert_eq!(results.len(), 11, "{results:#?}");
        assert_eq!(results[0], command("CREATE TABLE", None));
        assert_eq!(results[1], command("INSERT 0 3", Some(3)));
        assert_eq!(
            results[2],
            StatementResult::Rows {
                columns: vec![
                    Column {
                        name: "id".into(),
                        type_name: "int4".into()
                    },
                    Column {
                        name: "name".into(),
                        type_name: "text".into()
                    },
                ],
                rows: vec![
                    vec![Value::Int(1), Value::Text("a".into())],
                    vec![Value::Int(2), Value::Text("b".into())],
                    vec![Value::Int(3), Value::Text("c".into())],
                ],
                truncated: false,
            }
        );
        assert_eq!(results[3], command("UPDATE 2", Some(2)));
        assert_eq!(
            results[4],
            StatementResult::Rows {
                columns: vec![Column {
                    name: "n".into(),
                    type_name: "int8".into()
                }],
                rows: vec![vec![Value::Int(2)]],
                truncated: false,
            }
        );
        assert_eq!(results[5], command("DELETE 1", Some(1)));
        assert_eq!(results[6], command("BEGIN", None));
        assert_eq!(results[7], command("SAVEPOINT", None));
        assert_eq!(results[8], command("ROLLBACK", None));
        assert_eq!(results[9], command("COMMIT", None));
        assert_eq!(results[10], command("DROP TABLE", None));
        assert!(output.notices.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn utility_statements_and_create_table_as() -> TestResult {
    let (_, port) = newest();
    let session = connect(&params(port)).await?;
    let output = run(
        session.as_ref(),
        "CREATE TEMP TABLE ctas AS SELECT g FROM generate_series(1, 4) g;\n\
         VACUUM ctas; ANALYZE ctas; EXPLAIN SELECT * FROM ctas; SHOW DateStyle;\n\
         SELECT; VALUES (1, 'x')",
    )
    .await?;
    let results = &output.results;
    assert_eq!(
        results[0],
        StatementResult::Command {
            tag: "SELECT 4".into(),
            rows_affected: Some(4)
        }
    );
    assert_eq!(
        results[1],
        StatementResult::Command {
            tag: "VACUUM".into(),
            rows_affected: None
        }
    );
    assert!(
        matches!(&results[3], StatementResult::Rows { columns, .. } if columns[0].name == "QUERY PLAN")
    );
    assert_eq!(
        rows(&output, 4)?,
        &vec![vec![Value::Text("ISO, YMD".into())]]
    );
    assert!(
        matches!(&results[5], StatementResult::Rows { columns, rows, .. } if columns.is_empty() && rows.len() == 1)
    );
    assert_eq!(
        rows(&output, 6)?,
        &vec![vec![Value::Int(1), Value::Text("x".into())]]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn every_type_round_trips() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        let schema = "varve_driver_pg_types";
        run(
            session.as_ref(),
            &format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};\n\
                 CREATE EXTENSION IF NOT EXISTS citext SCHEMA {schema};\n\
                 CREATE TYPE {schema}.mood AS ENUM ('sad', 'ok', 'happy');\n\
                 CREATE DOMAIN {schema}.positive AS int4 CHECK (VALUE > 0);"
            ),
        )
        .await?;
        let output = run(
            session.as_ref(),
            &format!(
                r#"SET search_path = {schema}, public;
CREATE TEMP TABLE all_types (
    b bool, i2 int2, i4 int4, i8 int8, o oid,
    f4 float4, f8 float8, f_nan float8, f_inf float8,
    n numeric, m money,
    t text, vc varchar(10), c char(3), nm name, ci citext, x xml,
    by bytea, d date, tm time, ttz timetz, ts timestamp, tstz timestamptz,
    iv interval, u uuid, j json, jb jsonb,
    ai int4[], at text[], a2 int4[], ae mood[], ajb jsonb[], aby bytea[],
    e mood, dom positive, pt point, nothing text
);
INSERT INTO all_types VALUES (
    true, -32768, 2147483647, -9223372036854775808, 12345,
    1.5, 0.1, 'NaN', '-Infinity',
    '1234567890123456789012345678901234567890.0123456789', 12.34,
    'tekst ✓', 'varchar', 'ab', 'a_name', 'MiXeD', '<a>1</a>',
    '\xdeadbeef', '2026-10-19', '13:45:00.5', '13:45:00+05:30',
    '2026-10-19 13:45:00', '2026-10-19 13:45:00.123456+02',
    '1 year 2 months 3 days 04:05:06.5', 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11',
    '{{"b": [1, 2.5], "a": 12345678901234567890123, "c": 1.10}}', '{{"b": null, "a": true}}',
    '{{1,NULL,3}}', ARRAY['plain', 'with space', 'quote " and \ slash', NULL, 'NULL', ''],
    '{{{{1,2}},{{3,4}}}}', '{{happy,sad}}', ARRAY['{{"k": 1}}'::jsonb, NULL], ARRAY['\x00ff'::bytea],
    'ok', 42, '(1,2)', NULL
);
SELECT * FROM all_types;
RESET search_path"#
            ),
        )
        .await?;
        let Some(StatementResult::Rows { columns, rows, .. }) = output.results.get(3) else {
            return Err(format!("no rows: {:#?}", output.results).into());
        };
        let row = rows.first().ok_or("no row")?;
        let value = |name: &str| -> Result<(&str, &Value), String> {
            let index = columns
                .iter()
                .position(|c| c.name == name)
                .ok_or(format!("no column {name}"))?;
            Ok((columns[index].type_name.as_str(), &row[index]))
        };
        let expect = |name: &str, type_name: &str, expected: Value| -> TestResult {
            let (actual_type, actual) = value(name)?;
            assert_eq!(actual_type, type_name, "type of {name}");
            assert_eq!(actual, &expected, "value of {name}");
            Ok(())
        };
        expect("b", "bool", Value::Bool(true))?;
        expect("i2", "int2", Value::Int(-32768))?;
        expect("i4", "int4", Value::Int(2_147_483_647))?;
        expect("i8", "int8", Value::Int(i64::MIN))?;
        expect("o", "oid", Value::Int(12345))?;
        expect("f4", "float4", Value::Float(1.5))?;
        expect("f8", "float8", Value::Float(0.1))?;
        expect("f_nan", "float8", Value::Other("NaN".into()))?;
        expect("f_inf", "float8", Value::Other("-Infinity".into()))?;
        expect(
            "n",
            "numeric",
            Value::Decimal("1234567890123456789012345678901234567890.0123456789".into()),
        )?;
        let (money_type, money) = value("m")?;
        assert_eq!(money_type, "money");
        assert!(
            matches!(money, Value::Other(text) if text.contains("12.34")),
            "{money:?}"
        );
        expect("t", "text", Value::Text("tekst ✓".into()))?;
        expect("vc", "varchar", Value::Text("varchar".into()))?;
        expect("c", "bpchar", Value::Text("ab ".into()))?;
        expect("nm", "name", Value::Text("a_name".into()))?;
        expect("ci", "citext", Value::Text("MiXeD".into()))?;
        expect("x", "xml", Value::Text("<a>1</a>".into()))?;
        expect("by", "bytea", Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]))?;
        expect("d", "date", Value::Date("2026-10-19".into()))?;
        expect("tm", "time", Value::Time("13:45:00.5".into()))?;
        expect("ttz", "timetz", Value::Time("13:45:00+05:30".into()))?;
        expect(
            "ts",
            "timestamp",
            Value::Timestamp("2026-10-19T13:45:00".into()),
        )?;
        expect(
            "tstz",
            "timestamptz",
            Value::TimestampTz("2026-10-19T11:45:00.123456Z".into()),
        )?;
        expect("iv", "interval", Value::Interval("P1Y2M3DT4H5M6.5S".into()))?;
        expect(
            "u",
            "uuid",
            Value::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
        )?;
        // json keeps the text as written: key order and number precision.
        expect(
            "j",
            "json",
            Value::json(r#"{"b": [1, 2.5], "a": 12345678901234567890123, "c": 1.10}"#),
        )?;
        // jsonb normalizes key order on the server; the driver keeps that.
        expect("jb", "jsonb", Value::json(r#"{"a": true, "b": null}"#))?;
        expect(
            "ai",
            "int4[]",
            Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]),
        )?;
        expect(
            "at",
            "text[]",
            Value::Array(vec![
                Value::Text("plain".into()),
                Value::Text("with space".into()),
                Value::Text("quote \" and \\ slash".into()),
                Value::Null,
                Value::Text("NULL".into()),
                Value::Text(String::new()),
            ]),
        )?;
        expect(
            "a2",
            "int4[]",
            Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![Value::Int(3), Value::Int(4)]),
            ]),
        )?;
        expect(
            "ae",
            "mood[]",
            Value::Array(vec![Value::Text("happy".into()), Value::Text("sad".into())]),
        )?;
        expect(
            "ajb",
            "jsonb[]",
            Value::Array(vec![Value::json(r#"{"k": 1}"#), Value::Null]),
        )?;
        expect(
            "aby",
            "bytea[]",
            Value::Array(vec![Value::Bytes(vec![0, 255])]),
        )?;
        expect("e", "mood", Value::Text("ok".into()))?;
        // PostgreSQL describes domain columns by their base type.
        expect("dom", "int4", Value::Int(42))?;
        expect("pt", "point", Value::Other("(1,2)".into()))?;
        expect("nothing", "text", Value::Null)?;

        run(session.as_ref(), &format!("DROP SCHEMA {schema} CASCADE")).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn max_rows_truncates_and_keeps_reading() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        let output = session
            .execute(
                "SELECT g FROM generate_series(1, 1000) g; SELECT 'after'",
                &ExecOptions { max_rows: 10 },
            )
            .await?;
        let Some(StatementResult::Rows {
            rows, truncated, ..
        }) = output.results.first()
        else {
            return Err("no rows".into());
        };
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[9], vec![Value::Int(10)]);
        assert!(*truncated);
        assert_eq!(
            output.results.get(1),
            Some(&StatementResult::Rows {
                columns: vec![Column {
                    name: "?column?".into(),
                    type_name: "text".into()
                }],
                rows: vec![vec![Value::Text("after".into())]],
                truncated: false,
            })
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn captures_notices() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        let output = run(
            session.as_ref(),
            "DO $$ BEGIN RAISE NOTICE 'hi'; END $$;\n\
             DO $$ BEGIN RAISE WARNING 'careful'; RAISE INFO 'fyi'; END $$",
        )
        .await?;
        assert_eq!(
            output.notices,
            vec![
                Notice {
                    severity: "NOTICE".into(),
                    message: "hi".into()
                },
                Notice {
                    severity: "WARNING".into(),
                    message: "careful".into()
                },
                Notice {
                    severity: "INFO".into(),
                    message: "fyi".into()
                },
            ]
        );
        assert_eq!(
            output.results[0],
            StatementResult::Command {
                tag: "DO".into(),
                rows_affected: None
            }
        );
        // Notices do not leak into the next execution.
        assert!(run(session.as_ref(), "SELECT 1").await?.notices.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn error_positions_map_to_the_script() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        let sql = "SELECT 'héllo, wörld';\nSELECT 2;\n  SELECT nope FROM varve_missing_table;";
        let error = run(session.as_ref(), sql)
            .await
            .err()
            .ok_or("script succeeded")?;
        assert_eq!(error.kind, ErrorKind::Query);
        assert_eq!(error.code.as_deref(), Some("42P01"));
        assert!(error.message.contains("varve_missing_table"), "{error:?}");
        let byte = sql.find("varve_missing_table").ok_or("missing")?;
        let expected = u32::try_from(sql[..byte].chars().count() + 1)?;
        assert_eq!(error.position, Some(expected));

        let error = run(
            session.as_ref(),
            "SELECT 1; CREATE TEMP TABLE pos (id int UNIQUE); INSERT INTO pos VALUES (1), (1)",
        )
        .await
        .err()
        .ok_or("duplicate accepted")?;
        assert_eq!(error.code.as_deref(), Some("23505"));
        assert!(error.detail.is_some(), "{error:?}");

        let error = run(session.as_ref(), "SELECT * FROM pg_clas")
            .await
            .err()
            .ok_or("typo accepted")?;
        assert_eq!(error.position, Some(15));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn failed_transaction_state_is_kept() -> TestResult {
    let (_, port) = oldest();
    let session = connect(&params(port)).await?;
    let error = run(session.as_ref(), "BEGIN; SELECT 1/0; SELECT 1")
        .await
        .err()
        .ok_or("division by zero")?;
    assert_eq!(error.code.as_deref(), Some("22012"));
    // The statement cannot be prepared in an aborted transaction; the simple
    // protocol reports the server's error.
    let error = run(session.as_ref(), "SELECT 1")
        .await
        .err()
        .ok_or("aborted transaction accepted a query")?;
    assert_eq!(error.code.as_deref(), Some("25P02"));
    let output = run(session.as_ref(), "ROLLBACK; SELECT 2").await?;
    assert_eq!(
        output.results[0],
        StatementResult::Command {
            tag: "ROLLBACK".into(),
            rows_affected: None
        }
    );
    assert_eq!(rows(&output, 1)?, &vec![vec![Value::Int(2)]]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn copy_through_the_client_is_unsupported() -> TestResult {
    let (_, port) = newest();
    let session = connect(&params(port)).await?;
    for sql in [
        "COPY (SELECT 1) TO STDOUT",
        "CREATE TEMP TABLE c (a int); COPY c FROM stdin",
    ] {
        let error = run(session.as_ref(), sql).await.err().ok_or("COPY ran")?;
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert!(error.message.contains("Run SQL file"));
    }
    session.ping().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn cancel_stops_a_running_statement() -> TestResult {
    for (_, port) in servers() {
        let session: Arc<dyn Session> = Arc::from(connect(&params(port)).await?);
        // Cancelling an idle session succeeds and changes nothing.
        session.cancel_handle().cancel().await?;
        // The server signals the backend after answering the cancel request;
        // give the (ignored) signal time to arrive before the next statement.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(scalar(session.as_ref(), "SELECT 1").await?, Value::Int(1));

        let running = Arc::clone(&session);
        let task = tokio::spawn(async move { run(running.as_ref(), "SELECT pg_sleep(30)").await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let cancelled_at = Instant::now();
        session.cancel_handle().cancel().await?;
        let error = tokio::time::timeout(Duration::from_secs(2), task)
            .await??
            .err()
            .ok_or("pg_sleep finished")?;
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
        assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
        assert_eq!(error.code.as_deref(), Some("57014"));
        session.ping().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn executions_are_serialized() -> TestResult {
    let (_, port) = newest();
    let session: Arc<dyn Session> = Arc::from(connect(&params(port)).await?);
    let tasks: Vec<_> = (0..8)
        .map(|i| {
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                run(
                    session.as_ref(),
                    &format!("SELECT {i}; DO $$ BEGIN PERFORM pg_sleep(0.02); END $$"),
                )
                .await
            })
        })
        .collect();
    for (i, task) in tasks.into_iter().enumerate() {
        let output = task.await??;
        assert_eq!(
            rows(&output, 0)?,
            &vec![vec![Value::Int(i64::try_from(i)?)]]
        );
        assert_eq!(
            output.results[1],
            StatementResult::Command {
                tag: "DO".into(),
                rows_affected: None
            }
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn cancel_stops_a_script_between_statements() -> TestResult {
    const STATEMENTS: usize = 5000;
    for (_, port) in servers() {
        let session: Arc<dyn Session> = Arc::from(connect(&params(port)).await?);
        run(session.as_ref(), "CREATE TEMP TABLE cancel_probe (id int)").await?;
        let script: String = (0..STATEMENTS)
            .map(|i| format!("INSERT INTO cancel_probe VALUES ({i});\n"))
            .collect();
        // Each INSERT takes microseconds on the server, so the backend is
        // idle (and ignores the cancel request) most of the time. Several
        // rounds make a lucky hit on a running INSERT irrelevant.
        for _ in 0..3 {
            run(session.as_ref(), "TRUNCATE cancel_probe").await?;
            let running = Arc::clone(&session);
            let owned = script.clone();
            let task = tokio::spawn(async move { run(running.as_ref(), &owned).await });
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!task.is_finished(), "the script finished before the cancel");
            let cancelled_at = Instant::now();
            session.cancel_handle().cancel().await?;
            let error = tokio::time::timeout(Duration::from_secs(5), task)
                .await??
                .err()
                .ok_or("the whole script ran")?;
            assert!(cancelled_at.elapsed() < Duration::from_secs(1));
            assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
            let Value::Int(done) =
                scalar(session.as_ref(), "SELECT count(*) FROM cancel_probe").await?
            else {
                return Err("count(*) is not an integer".into());
            };
            assert!(done > 0, "nothing ran before the cancel");
            assert!(
                done < i64::try_from(STATEMENTS)?,
                "all {done} statements ran"
            );
            // The error points at the first statement that did not run.
            if error.code.is_none() {
                let next = format!("INSERT INTO cancel_probe VALUES ({done});");
                let byte = script.find(&next).ok_or("statement not found")?;
                assert_eq!(error.position, Some(u32::try_from(byte + 1)?));
            }
        }
        session.ping().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn notices_are_capped() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        let output = run(
            session.as_ref(),
            "DO $$ BEGIN FOR i IN 1..5000 LOOP RAISE NOTICE 'notice %', i; END LOOP; END $$",
        )
        .await?;
        // 1000 notices are kept, then one says how many were dropped.
        assert_eq!(output.notices.len(), 1001);
        assert_eq!(output.notices[999].message, "notice 1000");
        assert_eq!(output.notices[1000].severity, "WARNING");
        assert!(
            output.notices[1000]
                .message
                .starts_with("4000 more notices were dropped"),
            "{:?}",
            output.notices[1000]
        );
        // Big notices are capped by size (1 MiB of text per run).
        let output = run(
            session.as_ref(),
            "DO $$ BEGIN FOR i IN 1..20 LOOP RAISE NOTICE '%', repeat('x', 100000); END LOOP; END $$",
        )
        .await?;
        assert_eq!(output.notices.len(), 11, "{}", output.notices.len());
        assert!(
            output.notices[10]
                .message
                .starts_with("10 more notices were dropped")
        );
        assert!(run(session.as_ref(), "SELECT 1").await?.notices.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn command_tags_with_enum_and_domain_columns() -> TestResult {
    for (_, port) in servers() {
        // A fresh session: every new type makes tokio-postgres run its own
        // type lookup queries between the statements of the script.
        let session = connect(&params(port)).await?;
        let schema = "varve_driver_pg_tags";
        let output = run(
            session.as_ref(),
            &format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE;\n\
                 CREATE SCHEMA {schema};\n\
                 CREATE TYPE {schema}.level AS ENUM ('low', 'mid', 'high');\n\
                 CREATE DOMAIN {schema}.score AS int4 CHECK (VALUE >= 0);\n\
                 CREATE TABLE {schema}.items (id int PRIMARY KEY, lvl {schema}.level, s {schema}.score);\n\
                 INSERT INTO {schema}.items VALUES (1, 'low', 1), (2, 'mid', 2), (3, 'high', 3);\n\
                 SELECT lvl, s FROM {schema}.items ORDER BY id;\n\
                 UPDATE {schema}.items SET lvl = 'high' WHERE lvl <> 'high';\n\
                 SELECT array_agg(lvl ORDER BY id) AS levels FROM {schema}.items;\n\
                 DELETE FROM {schema}.items WHERE s > 1;\n\
                 SELECT 'mid'::{schema}.level AS m, 5::{schema}.score AS sc;\n\
                 DROP SCHEMA {schema} CASCADE"
            ),
        )
        .await?;
        let command = |tag: &str, rows_affected: Option<u64>| StatementResult::Command {
            tag: tag.into(),
            rows_affected,
        };
        let column = |name: &str, type_name: &str| Column {
            name: name.into(),
            type_name: type_name.into(),
        };
        let results = &output.results;
        assert_eq!(results.len(), 12, "{results:#?}");
        assert_eq!(results[0], command("DROP SCHEMA", None));
        assert_eq!(results[1], command("CREATE SCHEMA", None));
        assert_eq!(results[2], command("CREATE TYPE", None));
        assert_eq!(results[3], command("CREATE DOMAIN", None));
        assert_eq!(results[4], command("CREATE TABLE", None));
        assert_eq!(results[5], command("INSERT 0 3", Some(3)));
        assert_eq!(
            results[6],
            StatementResult::Rows {
                columns: vec![column("lvl", "level"), column("s", "int4")],
                rows: vec![
                    vec![Value::Text("low".into()), Value::Int(1)],
                    vec![Value::Text("mid".into()), Value::Int(2)],
                    vec![Value::Text("high".into()), Value::Int(3)],
                ],
                truncated: false,
            }
        );
        assert_eq!(results[7], command("UPDATE 2", Some(2)));
        assert_eq!(
            results[8],
            StatementResult::Rows {
                columns: vec![column("levels", "level[]")],
                rows: vec![vec![Value::Array(vec![Value::Text("high".into()); 3])]],
                truncated: false,
            }
        );
        assert_eq!(results[9], command("DELETE 2", Some(2)));
        assert_eq!(
            results[10],
            StatementResult::Rows {
                columns: vec![column("m", "level"), column("sc", "int4")],
                rows: vec![vec![Value::Text("mid".into()), Value::Int(5)]],
                truncated: false,
            }
        );
        assert_eq!(results[11], command("DROP SCHEMA", None));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_session_is_closed_when_the_check_fails() -> TestResult {
    for (_, port) in servers() {
        let admin = connect(&params(port)).await?;
        let role = "varve_ro_lost";
        run(
            admin.as_ref(),
            &format!(
                "DROP ROLE IF EXISTS {role}; CREATE ROLE {role} LOGIN PASSWORD 'varve_dev';\n\
                 REVOKE EXECUTE ON FUNCTION pg_catalog.current_setting(text) FROM PUBLIC"
            ),
        )
        .await?;
        let outcome = async {
            let mut params = params(port);
            params.username = Some(role.into());
            params.read_only = true;
            let session = connect(&params).await?;
            let failed = run(session.as_ref(), "SELECT 1/0").await;
            let session = connect(&params).await?;
            let first = run(session.as_ref(), "SELECT 1").await;
            let second = run(session.as_ref(), "SELECT 2").await;
            let ping = session.ping().await;
            Ok::<_, varve_driver::DriverError>((failed, first, second, ping))
        }
        .await;
        run(
            admin.as_ref(),
            &format!(
                "GRANT EXECUTE ON FUNCTION pg_catalog.current_setting(text) TO PUBLIC;\n\
                 DROP ROLE {role}"
            ),
        )
        .await?;
        let (failed, first, second, ping) = outcome?;
        // A failed statement keeps its own error; the closing comes with it.
        let error = failed.err().ok_or("SELECT 1/0 passed")?;
        assert_eq!(error.code.as_deref(), Some("22012"), "{error:?}");
        assert!(
            error
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("the connection was closed")),
            "{error:?}"
        );
        let error = first
            .err()
            .ok_or("the check could not run, yet SELECT 1 passed")?;
        assert_eq!(error.kind, ErrorKind::ConnectionLost, "{error:?}");
        assert!(
            error
                .message
                .contains("statement 1 may have changed read-only mode"),
            "{error:?}"
        );
        assert!(error.message.contains("permission denied"), "{error:?}");
        let error = second.err().ok_or("a lost session ran SELECT 2")?;
        assert_eq!(error.kind, ErrorKind::ConnectionLost, "{error:?}");
        let error = ping.err().ok_or("a lost session answered a ping")?;
        assert_eq!(error.kind, ErrorKind::ConnectionLost, "{error:?}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn cancel_returns_once_the_server_has_handled_it() -> TestResult {
    // The proxy keeps the cancel connection open for `hold` after the server
    // closed it: cancel() must wait for that close, which the server sends
    // only after it has signalled the backend.
    let hold = Duration::from_millis(800);
    let (_, port) = newest();
    let proxy = proxy(port, hold).await?;
    let session = connect(&params(proxy.port)).await?;
    let started = Instant::now();
    session.cancel_handle().cancel().await?;
    assert!(started.elapsed() >= hold, "{:?}", started.elapsed());
    assert_eq!(scalar(session.as_ref(), "SELECT 1").await?, Value::Int(1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn execute_waits_for_a_cancel_in_flight() -> TestResult {
    let hold = Duration::from_millis(800);
    let (_, port) = newest();
    let proxy = proxy(port, hold).await?;
    let session: Arc<dyn Session> = Arc::from(connect(&params(proxy.port)).await?);
    let handle = session.cancel_handle();
    let cancel = tokio::spawn(async move { handle.cancel().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!cancel.is_finished());
    // The run starts only after the cancel request is done, and is not
    // hit by it.
    let started = Instant::now();
    assert_eq!(scalar(session.as_ref(), "SELECT 1").await?, Value::Int(1));
    assert!(cancel.is_finished());
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
    cancel.await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn session_settings_are_set_back() -> TestResult {
    for (_, port) in servers() {
        let mut params = params(port);
        params.statement_timeout = Some(Duration::from_millis(1500));
        let session = connect(&params).await?;
        let expect_kept = |output: &varve_driver::ExecOutput, names: &[&str]| {
            let notice = output
                .notices
                .iter()
                .find(|n| n.message.contains("changed session settings"))
                .ok_or_else(|| format!("no warning: {:?}", output.notices))?;
            assert_eq!(notice.severity, "WARNING");
            for name in names {
                assert!(notice.message.contains(name), "{name}: {notice:?}");
            }
            Ok::<_, String>(())
        };
        let shows =
            async |session: &dyn Session| -> Result<Vec<Value>, Box<dyn std::error::Error>> {
                let output = run(
                    session,
                    "SHOW DateStyle; SHOW IntervalStyle; SHOW TimeZone; SHOW extra_float_digits; \
                 SHOW bytea_output; SHOW client_encoding; SHOW standard_conforming_strings; \
                 SHOW statement_timeout",
                )
                .await?;
                assert!(output.notices.is_empty(), "{:?}", output.notices);
                (0..8)
                    .map(|i| Ok(rows(&output, i)?[0][0].clone()))
                    .collect()
            };
        let wanted: Vec<Value> = [
            "ISO, YMD", "iso_8601", "UTC", "3", "hex", "UTF8", "on", "1500ms",
        ]
        .into_iter()
        .map(|v| Value::Text(v.into()))
        .collect();
        assert_eq!(shows(session.as_ref()).await?, wanted);

        for reset in ["RESET ALL", "RESET IntervalStyle"] {
            let output = run(session.as_ref(), reset).await?;
            expect_kept(&output, &["IntervalStyle = iso_8601"])?;
            assert_eq!(shows(session.as_ref()).await?, wanted, "{reset}");
        }

        let output = run(
            session.as_ref(),
            "SET TimeZone = 'Asia/Tokyo'; SELECT '2026-01-01 12:00:00+00'::timestamptz AS t",
        )
        .await?;
        expect_kept(&output, &["TimeZone = UTC"])?;
        assert_eq!(
            rows(&output, 1)?,
            &vec![vec![Value::TimestampTz("2026-01-01T12:00:00Z".into())]]
        );

        let output = run(
            session.as_ref(),
            "SELECT set_config('bytea_output', 'escape', false); SELECT '\\x00ff'::bytea AS b",
        )
        .await?;
        expect_kept(&output, &["bytea_output = hex"])?;
        assert_eq!(rows(&output, 1)?, &vec![vec![Value::Bytes(vec![0, 255])]]);

        let output = run(
            session.as_ref(),
            "SET extra_float_digits = 0; SELECT 1::float8 / 3 AS f",
        )
        .await?;
        expect_kept(&output, &["extra_float_digits = 3"])?;
        assert_eq!(rows(&output, 1)?, &vec![vec![Value::Float(1.0 / 3.0)]]);

        // A function called from a plain SELECT: the server reports the
        // change, no extra query is needed to see it.
        run(
            session.as_ref(),
            "CREATE OR REPLACE FUNCTION pg_temp.varve_style() RETURNS text LANGUAGE sql AS \
             $$ SELECT set_config('IntervalStyle', 'postgres', false) $$",
        )
        .await?;
        let output = run(
            session.as_ref(),
            "SELECT pg_temp.varve_style(); SELECT interval '1 day' AS i",
        )
        .await?;
        expect_kept(&output, &["IntervalStyle = iso_8601"])?;
        assert_eq!(
            rows(&output, 1)?,
            &vec![vec![Value::Interval("P1D".into())]]
        );

        // SET LOCAL inside a transaction, then the transaction ends.
        let output = run(
            session.as_ref(),
            "BEGIN; SET LOCAL TimeZone = 'Asia/Tokyo'; \
             SELECT '2026-01-01 12:00:00+00'::timestamptz AS t; COMMIT",
        )
        .await?;
        // COMMIT ends SET LOCAL by itself; the timestamp carries its offset.
        assert!(output.notices.is_empty(), "{:?}", output.notices);
        assert_eq!(
            rows(&output, 2)?,
            &vec![vec![Value::TimestampTz("2026-01-01T12:00:00Z".into())]]
        );
        assert_eq!(shows(session.as_ref()).await?, wanted);

        // Inside a transaction nothing is set back (ROLLBACK would undo
        // it); after COMMIT it is, before the next statement.
        let output = run(
            session.as_ref(),
            "BEGIN; SET TimeZone = 'Asia/Tokyo'; ROLLBACK; SHOW TimeZone",
        )
        .await?;
        assert!(output.notices.is_empty(), "{:?}", output.notices);
        assert_eq!(rows(&output, 3)?, &vec![vec![Value::Text("UTC".into())]]);
        let output = run(
            session.as_ref(),
            "BEGIN; SET TimeZone = 'Asia/Tokyo'; COMMIT; SHOW TimeZone",
        )
        .await?;
        assert_eq!(rows(&output, 3)?, &vec![vec![Value::Text("UTC".into())]]);
        assert_eq!(output.notices.len(), 1, "{:?}", output.notices);
        assert!(
            output.notices[0]
                .message
                .starts_with("the transaction that ended with statement 3 changed"),
            "{:?}",
            output.notices
        );

        // A function called from a plain SELECT changes an unreported
        // setting: checked before a later statement reads bytea values,
        // and at the end of every run.
        run(
            session.as_ref(),
            "CREATE OR REPLACE FUNCTION pg_temp.varve_escape() RETURNS text LANGUAGE sql AS \
             $$ SELECT set_config('bytea_output', 'escape', false) $$",
        )
        .await?;
        let output = run(
            session.as_ref(),
            "SELECT pg_temp.varve_escape(); SELECT '\\x00ff'::bytea AS b",
        )
        .await?;
        assert_eq!(rows(&output, 1)?, &vec![vec![Value::Bytes(vec![0, 255])]]);
        assert!(
            output.notices.iter().any(|n| n
                .message
                .starts_with("a statement before statement 2 changed")),
            "{:?}",
            output.notices
        );
        let output = run(session.as_ref(), "SELECT pg_temp.varve_escape()").await?;
        assert!(
            output
                .notices
                .iter()
                .any(|n| n.message.starts_with("a statement in this run changed")
                    && n.message.contains("bytea_output = hex")),
            "{:?}",
            output.notices
        );
        assert_eq!(shows(session.as_ref()).await?, wanted);

        // The warning also comes with an error.
        let error = run(session.as_ref(), "SET TimeZone = 'Asia/Tokyo'; SELECT 1/0")
            .await
            .err()
            .ok_or("division by zero")?;
        assert!(
            error
                .detail
                .as_deref()
                .is_some_and(|d| d.starts_with("Warning: statement 1 changed session settings")),
            "{error:?}"
        );
        assert_eq!(shows(session.as_ref()).await?, wanted);

        // Ordinary statements cost nothing extra and warn about nothing.
        let output = run(session.as_ref(), "SELECT 1; SET work_mem = '8MB'").await?;
        assert!(output.notices.is_empty(), "{:?}", output.notices);

        // The init SQL is checked too.
        let mut init = params.clone();
        init.init_sql = Some("SET IntervalStyle = postgres; SET extra_float_digits = 1".into());
        let session = connect(&init).await?;
        let output = run(session.as_ref(), "SELECT interval '1 day' AS i").await?;
        assert_eq!(
            rows(&output, 0)?,
            &vec![vec![Value::Interval("P1D".into())]]
        );
        assert_eq!(output.notices.len(), 1, "{:?}", output.notices);
        assert!(
            output.notices[0]
                .message
                .starts_with("the init SQL changed")
        );
        assert!(output.notices[0].message.contains("extra_float_digits = 3"));
    }
    Ok(())
}

const DISCARD_ALL_NOTICE: &str =
    "DISCARD ALL: Varve reconnected the session so type information stays correct";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn discard_all_reconnects_the_session() -> TestResult {
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DROP TYPE IF EXISTS varve_discard_a; DROP TYPE IF EXISTS varve_discard_b;\n\
             CREATE TYPE varve_discard_a AS ENUM ('x'); CREATE TYPE varve_discard_b AS ENUM ('y')",
        )
        .await?;
        let mut params = params(port);
        // Longer than the cancel test below waits, so only a cancel that
        // reaches the new backend stops pg_sleep in time.
        params.statement_timeout = Some(Duration::from_secs(10));
        params.init_sql = Some("SET work_mem = '8MB'".into());
        let session: Arc<dyn Session> = Arc::from(connect(&params).await?);
        let Value::Int(first_pid) = scalar(session.as_ref(), "SELECT pg_backend_pid()").await?
        else {
            return Err("pid is not an integer".into());
        };
        let output = run(
            session.as_ref(),
            "CREATE TEMP TABLE varve_discard_temp (a int);\n\
             SELECT 'x'::varve_discard_a AS a;\n\
             DISCARD ALL;\n\
             SELECT 'y'::varve_discard_b AS b;\n\
             SELECT pg_backend_pid() AS pid, to_regclass('varve_discard_temp') IS NULL AS gone;\n\
             SHOW IntervalStyle; SHOW statement_timeout; SHOW work_mem",
        )
        .await?;
        let column = |name: &str, type_name: &str| Column {
            name: name.into(),
            type_name: type_name.into(),
        };
        assert_eq!(
            output.notices,
            vec![Notice {
                severity: "NOTICE".into(),
                message: DISCARD_ALL_NOTICE.into(),
            }]
        );
        assert_eq!(
            output.results[2],
            StatementResult::Command {
                tag: "DISCARD ALL".into(),
                rows_affected: None
            }
        );
        // The type of a column looked up after DISCARD ALL is known.
        assert_eq!(
            output.results[3],
            StatementResult::Rows {
                columns: vec![column("b", "varve_discard_b")],
                rows: vec![vec![Value::Text("y".into())]],
                truncated: false,
            }
        );
        let row = &rows(&output, 4)?[0];
        assert_ne!(row[0], Value::Int(first_pid), "the backend did not change");
        assert_eq!(row[1], Value::Bool(true));
        assert_eq!(
            rows(&output, 5)?,
            &vec![vec![Value::Text("iso_8601".into())]]
        );
        assert_eq!(rows(&output, 6)?, &vec![vec![Value::Text("10s".into())]]);
        assert_eq!(rows(&output, 7)?, &vec![vec![Value::Text("8MB".into())]]);

        // The cancel handle follows the new backend.
        let running = Arc::clone(&session);
        let task = tokio::spawn(async move { run(running.as_ref(), "SELECT pg_sleep(30)").await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let cancelled_at = Instant::now();
        session.cancel_handle().cancel().await?;
        let error = tokio::time::timeout(Duration::from_secs(3), task)
            .await??
            .err()
            .ok_or("pg_sleep finished")?;
        assert!(cancelled_at.elapsed() < Duration::from_secs(1));
        assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
        assert_eq!(error.code.as_deref(), Some("57014"));
        assert_eq!(scalar(session.as_ref(), "SELECT 1").await?, Value::Int(1));

        run(
            writer.as_ref(),
            "DROP TYPE varve_discard_a; DROP TYPE varve_discard_b",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn deallocate_all_is_refused() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        for (sql, statement) in [
            (
                "SET application_name = 'varve ran this'; DEALLOCATE ALL",
                "DEALLOCATE ALL",
            ),
            (
                "SET application_name = 'varve ran this';\n  deallocate prepare all",
                "deallocate prepare all",
            ),
        ] {
            let error = run(session.as_ref(), sql)
                .await
                .err()
                .ok_or_else(|| format!("{sql} ran"))?;
            assert_eq!(error.kind, ErrorKind::Unsupported, "{error:?}");
            assert!(error.message.contains("DEALLOCATE <name>"), "{error:?}");
            assert!(error.message.contains("DISCARD ALL"), "{error:?}");
            let byte = sql.find(statement).ok_or("statement not in script")?;
            assert_eq!(
                error.position,
                Some(u32::try_from(sql[..byte].chars().count() + 1)?)
            );
        }
        assert_eq!(
            scalar(session.as_ref(), "SHOW application_name").await?,
            Value::Text("varve-driver-postgres tests".into())
        );
        // Named statements, and the words inside literals, are fine.
        let output = run(
            session.as_ref(),
            "PREPARE varve_p AS SELECT 1; DEALLOCATE varve_p; SELECT 'DEALLOCATE ALL' AS t",
        )
        .await?;
        assert_eq!(
            rows(&output, 2)?,
            &vec![vec![Value::Text("DEALLOCATE ALL".into())]]
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn prepare_time_errors_in_a_transaction_keep_their_sqlstate() -> TestResult {
    for (_, port) in servers() {
        for read_only in [false, true] {
            let mut params = params(port);
            params.read_only = read_only;
            let session = connect(&params).await?;
            for (statement, code, column) in [
                ("SELECT CAST('abc' AS uuid)", "22P02", "'abc'"),
                (
                    "SELECT no_such_column FROM pg_class",
                    "42703",
                    "no_such_column",
                ),
            ] {
                let sql = format!("BEGIN;\nSELECT 1;\n{statement};\nSELECT 2");
                let error = run(session.as_ref(), &sql)
                    .await
                    .err()
                    .ok_or_else(|| format!("{statement} succeeded"))?;
                assert_eq!(error.code.as_deref(), Some(code), "{error:?}");
                assert_eq!(error.kind, ErrorKind::Query, "{error:?}");
                let byte = sql.find(column).ok_or("not in script")?;
                assert_eq!(
                    error.position,
                    Some(u32::try_from(sql[..byte].chars().count() + 1)?),
                    "{error:?}"
                );
                assert!(error.detail.is_none(), "{error:?}");
                // The transaction is now aborted, as the server says.
                let error = run(session.as_ref(), "SELECT 3")
                    .await
                    .err()
                    .ok_or("an aborted transaction ran SELECT 3")?;
                assert_eq!(error.code.as_deref(), Some("25P02"), "{error:?}");
                run(session.as_ref(), "ROLLBACK").await?;
                assert_eq!(scalar(session.as_ref(), "SELECT 4").await?, Value::Int(4));
            }
            // A run-time error keeps its detail.
            let error = run(
                session.as_ref(),
                "BEGIN; CREATE TEMP TABLE varve_dup (id int UNIQUE); \
                 INSERT INTO varve_dup VALUES (1), (1)",
            )
            .await
            .err()
            .ok_or("duplicate accepted")?;
            if read_only {
                assert_eq!(error.code.as_deref(), Some("25006"), "{error:?}");
            } else {
                assert_eq!(error.code.as_deref(), Some("23505"), "{error:?}");
                assert!(error.detail.is_some(), "{error:?}");
            }
            run(session.as_ref(), "ROLLBACK").await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn the_guard_leaves_transactions_alone() -> TestResult {
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DROP TABLE IF EXISTS varve_snap; CREATE TABLE varve_snap (id int)",
        )
        .await?;
        for read_only in [false, true] {
            let mut params = params(port);
            params.read_only = read_only;
            let session = connect(&params).await?;
            // The isolation level can still be set after BEGIN and a SET.
            let output = run(
                session.as_ref(),
                "BEGIN; SET work_mem = '8MB'; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; \
                 SHOW transaction_isolation; COMMIT",
            )
            .await?;
            assert_eq!(
                rows(&output, 3)?,
                &vec![vec![Value::Text("repeatable read".into())]]
            );

            // The snapshot is taken by the user's first query, not before.
            run(writer.as_ref(), "TRUNCATE varve_snap").await?;
            run(
                session.as_ref(),
                "BEGIN ISOLATION LEVEL REPEATABLE READ; SET work_mem = '8MB'",
            )
            .await?;
            run(writer.as_ref(), "INSERT INTO varve_snap VALUES (1)").await?;
            let output = run(session.as_ref(), "SELECT count(*) FROM varve_snap; COMMIT").await?;
            assert_eq!(
                rows(&output, 0)?,
                &vec![vec![Value::Int(1)]],
                "read_only {read_only}"
            );

            // SET TRANSACTION SNAPSHOT must come first in its transaction.
            let exporter = connect(&common::params(port)).await?;
            let output = run(
                exporter.as_ref(),
                "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT pg_export_snapshot()",
            )
            .await?;
            let Value::Text(snapshot) = rows(&output, 1)?[0][0].clone() else {
                return Err("no snapshot id".into());
            };
            let output = run(
                session.as_ref(),
                &format!(
                    "BEGIN ISOLATION LEVEL REPEATABLE READ; SET TRANSACTION SNAPSHOT '{snapshot}'; \
                     SELECT 1; COMMIT"
                ),
            )
            .await?;
            assert_eq!(rows(&output, 2)?, &vec![vec![Value::Int(1)]]);
            run(exporter.as_ref(), "COMMIT").await?;
        }
        run(writer.as_ref(), "DROP TABLE varve_snap").await?;
    }
    Ok(())
}

const TYPE_STATEMENTS_NOTICE: &str = "Varve reconnected the session because a statement dropped \
     the prepared statements it uses to look up column types (DEALLOCATE ALL); temporary \
     tables and session settings were reset";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn deallocate_all_inside_do_reconnects() -> TestResult {
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DROP TYPE IF EXISTS varve_dealloc_a; DROP TYPE IF EXISTS varve_dealloc_b;\n\
             CREATE TYPE varve_dealloc_a AS ENUM ('x'); CREATE TYPE varve_dealloc_b AS ENUM ('y')",
        )
        .await?;
        let session = connect(&params(port)).await?;
        // A custom type makes tokio-postgres prepare its type lookups.
        let output = run(session.as_ref(), "SELECT 'x'::varve_dealloc_a AS a").await?;
        assert!(matches!(
            &output.results[0],
            StatementResult::Rows { columns, .. } if columns[0].type_name == "varve_dealloc_a"
        ));
        let output = run(
            session.as_ref(),
            "DO $$ BEGIN EXECUTE 'DEALLOCATE ALL'; END $$; SELECT 'y'::varve_dealloc_b AS b",
        )
        .await?;
        assert_eq!(
            output.notices,
            vec![Notice {
                severity: "NOTICE".into(),
                message: TYPE_STATEMENTS_NOTICE.into(),
            }]
        );
        assert_eq!(
            output.results[1],
            StatementResult::Rows {
                columns: vec![Column {
                    name: "b".into(),
                    type_name: "varve_dealloc_b".into(),
                }],
                rows: vec![vec![Value::Text("y".into())]],
                truncated: false,
            }
        );
        run(
            writer.as_ref(),
            "DROP TYPE varve_dealloc_a; DROP TYPE varve_dealloc_b",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn cancel_never_hits_the_guard_as_a_failure() -> TestResult {
    // In a read-only session every autocommitted statement is followed by a
    // guard query, so a cancel often arrives while one runs.
    for (_, port) in servers() {
        let mut params = params(port);
        params.read_only = true;
        let session: Arc<dyn Session> = Arc::from(connect(&params).await?);
        let script: String = (0..3000).map(|i| format!("SELECT {i};\n")).collect();
        for round in 0..5 {
            let running = Arc::clone(&session);
            let owned = script.clone();
            let task = tokio::spawn(async move { run(running.as_ref(), &owned).await });
            tokio::time::sleep(Duration::from_millis(100 + 37 * round)).await;
            session.cancel_handle().cancel().await?;
            let error = tokio::time::timeout(Duration::from_secs(5), task)
                .await??
                .err()
                .ok_or("the whole script ran")?;
            assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
            assert!(error.detail.is_none(), "{error:?}");
            // The session is fine and nothing is left aborted.
            let output = run(session.as_ref(), "SELECT 1").await?;
            assert!(output.notices.is_empty(), "{:?}", output.notices);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn the_guard_waits_for_a_cancel_in_flight() -> TestResult {
    // The proxy delivers the cancel request 600 ms late. The script stops at
    // once (the cancel flag), but the guard's check at the end of the run
    // must wait until the request is through, so its query cannot be the
    // one the late signal interrupts.
    let delay = Duration::from_millis(600);
    let (_, port) = newest();
    let proxy = delaying_proxy(port, delay, Duration::ZERO).await?;
    let mut params = params(proxy.port);
    params.read_only = true;
    let session: Arc<dyn Session> = Arc::from(connect(&params).await?);
    let script: String = (0..20000).map(|i| format!("SELECT {i};\n")).collect();
    let running = Arc::clone(&session);
    let task = tokio::spawn(async move { run(running.as_ref(), &script).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let handle = session.cancel_handle();
    let cancelled_at = Instant::now();
    let cancel = tokio::spawn(async move { handle.cancel().await });
    let error = tokio::time::timeout(Duration::from_secs(5), task)
        .await??
        .err()
        .ok_or("the whole script ran")?;
    let returned_after = cancelled_at.elapsed();
    assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
    assert!(error.detail.is_none(), "{error:?}");
    assert!(
        returned_after >= delay - Duration::from_millis(50),
        "{returned_after:?}"
    );
    cancel.await??;
    let output = run(session.as_ref(), "SELECT 1").await?;
    assert!(output.notices.is_empty(), "{:?}", output.notices);
    Ok(())
}

/// Creates the enum types `varve_lk1..3` and `public.varve_dealloc()`,
/// which runs DEALLOCATE ALL out of sight of the script check.
async fn lookup_fixtures(writer: &dyn Session) -> Result<(), varve_driver::DriverError> {
    run(
        writer,
        "DROP TYPE IF EXISTS varve_lk1; DROP TYPE IF EXISTS varve_lk2; DROP TYPE IF EXISTS varve_lk3;\n\
         CREATE TYPE varve_lk1 AS ENUM ('a'); CREATE TYPE varve_lk2 AS ENUM ('b');\n\
         CREATE TYPE varve_lk3 AS ENUM ('c');\n\
         CREATE OR REPLACE FUNCTION public.varve_dealloc() RETURNS int LANGUAGE plpgsql AS \
         $$ BEGIN EXECUTE 'DEALLOCATE ALL'; RETURN 1; END $$",
    )
    .await
    .map(drop)
}

fn type_of(output: &varve_driver::ExecOutput, index: usize) -> Option<&str> {
    match output.results.get(index) {
        Some(StatementResult::Rows { columns, .. }) => {
            columns.first().map(|c| c.type_name.as_str())
        }
        _ => None,
    }
}

async fn protocol_statements(session: &dyn Session) -> Result<i64, Box<dyn std::error::Error>> {
    match scalar(
        session,
        "SELECT count(*) FROM pg_prepared_statements WHERE NOT from_sql",
    )
    .await?
    {
        Value::Int(n) => Ok(n),
        other => Err(format!("count is {other:?}").into()),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn lost_type_lookups_are_recovered() -> TestResult {
    let reconnected = Notice {
        severity: "NOTICE".into(),
        message: TYPE_STATEMENTS_NOTICE.into(),
    };
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        lookup_fixtures(writer.as_ref()).await?;

        // A function in a plain SELECT drops the lookups; the next new type
        // in the same script still gets its name.
        let session = connect(&params(port)).await?;
        let output = run(session.as_ref(), "SELECT 'a'::varve_lk1 AS e").await?;
        assert_eq!(type_of(&output, 0), Some("varve_lk1"));
        let output = run(
            session.as_ref(),
            "SELECT public.varve_dealloc(); SELECT 'b'::varve_lk2 AS e",
        )
        .await?;
        assert_eq!(type_of(&output, 1), Some("varve_lk2"), "{output:?}");
        assert_eq!(output.notices, vec![reconnected.clone()]);
        let output = run(session.as_ref(), "SELECT 'c'::varve_lk3 AS e").await?;
        assert_eq!(type_of(&output, 0), Some("varve_lk3"));
        assert!(output.notices.is_empty(), "{:?}", output.notices);
        // Only the lookups remain on the server: nothing leaked.
        assert!(protocol_statements(session.as_ref()).await? <= 3);

        // Inside a transaction: the failed lookup aborts it; the error says
        // why, the abort stays, and the reconnect follows the ROLLBACK.
        let session = connect(&params(port)).await?;
        run(session.as_ref(), "SELECT 'a'::varve_lk1 AS e").await?;
        let error = run(
            session.as_ref(),
            "BEGIN; SELECT public.varve_dealloc(); SELECT 'b'::varve_lk2 AS e",
        )
        .await
        .err()
        .ok_or("the lookup inside the transaction passed")?;
        assert_eq!(error.code.as_deref(), Some("26000"), "{error:?}");
        assert!(error.message.contains("Roll it back"), "{error:?}");
        let error = run(session.as_ref(), "SELECT 1")
            .await
            .err()
            .ok_or("the aborted transaction ran SELECT 1")?;
        assert_eq!(error.code.as_deref(), Some("25P02"), "{error:?}");
        let output = run(session.as_ref(), "ROLLBACK; SELECT 'c'::varve_lk3 AS e").await?;
        assert_eq!(type_of(&output, 1), Some("varve_lk3"), "{output:?}");
        assert_eq!(output.notices, vec![reconnected.clone()]);

        // Some of the lookups dropped by name.
        let session = connect(&params(port)).await?;
        run(session.as_ref(), "SELECT 'a'::varve_lk1 AS e").await?;
        assert!(protocol_statements(session.as_ref()).await? >= 2);
        // The check after the DO block finds a lookup missing by its name
        // (a count of the remaining ones is still above 0) and reconnects
        // at once: no lookup ever fails with 26000.
        let output = run(
            session.as_ref(),
            "DO $$ DECLARE r record; n int := 0; BEGIN \
             FOR r IN SELECT name FROM pg_prepared_statements WHERE NOT from_sql ORDER BY name \
             LOOP n := n + 1; IF n > 1 THEN EXECUTE format('DEALLOCATE %I', r.name); END IF; \
             END LOOP; END $$",
        )
        .await?;
        assert_eq!(output.notices, vec![reconnected.clone()]);
        let output = run(session.as_ref(), "SELECT 'b'::varve_lk2 AS e").await?;
        assert_eq!(type_of(&output, 0), Some("varve_lk2"), "{output:?}");
        assert!(output.notices.is_empty(), "{:?}", output.notices);
        let output = run(
            session.as_ref(),
            "BEGIN; SELECT ARRAY['c'::varve_lk3] AS e; COMMIT",
        )
        .await?;
        assert_eq!(type_of(&output, 1), Some("varve_lk3[]"), "{output:?}");

        run(
            writer.as_ref(),
            "DROP TYPE varve_lk1; DROP TYPE varve_lk2; DROP TYPE varve_lk3; \
             DROP FUNCTION public.varve_dealloc()",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn failed_lookups_leak_no_statements() -> TestResult {
    for (_, port) in servers() {
        let admin = connect(&params(port)).await?;
        let role = "varve_leak";
        run(
            admin.as_ref(),
            &format!(
                "DROP TYPE IF EXISTS varve_leak_e; CREATE TYPE varve_leak_e AS ENUM ('x');\n\
                 DROP ROLE IF EXISTS {role}; CREATE ROLE {role} LOGIN PASSWORD 'varve_dev';\n\
                 REVOKE SELECT ON pg_catalog.pg_enum FROM PUBLIC"
            ),
        )
        .await?;
        let outcome = async {
            let mut params = params(port);
            params.username = Some(role.into());
            let session = connect(&params).await?;
            let mut values = Vec::new();
            for i in 0..8 {
                // The enum lookup fails (no access to pg_enum) after the
                // statement was parsed; it still runs, as text.
                let output = run(
                    session.as_ref(),
                    &format!("SELECT 'x'::varve_leak_e AS e{i}"),
                )
                .await?;
                values.push(
                    rows(&output, 0)
                        .map(|r| r[0][0].clone())
                        .unwrap_or(Value::Null),
                );
            }
            let count = protocol_statements(session.as_ref()).await;
            Ok::<_, Box<dyn std::error::Error>>((values, count))
        }
        .await;
        run(
            admin.as_ref(),
            &format!(
                "GRANT SELECT ON pg_catalog.pg_enum TO PUBLIC; DROP ROLE {role}; \
                 DROP TYPE varve_leak_e"
            ),
        )
        .await?;
        let (values, count) = outcome?;
        assert!(
            values.iter().all(|v| *v == Value::Text("x".into())),
            "{values:?}"
        );
        // The type lookups only (pg_type and pg_enum), not one per run.
        assert!(count? <= 2);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_holds_under_external_cancels() -> TestResult {
    for (_, port) in servers() {
        let admin = connect(&params(port)).await?;
        run(
            admin.as_ref(),
            "DROP TABLE IF EXISTS varve_ro_cancel; CREATE TABLE varve_ro_cancel (id int);\n\
             CREATE OR REPLACE FUNCTION public.varve_ro_evil() RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM set_config('default_transaction_read_only', 'off', false); \
             RETURN 1; END $$",
        )
        .await?;
        let mut params = params(port);
        params.read_only = true;
        let session: Arc<dyn Session> = Arc::from(connect(&params).await?);
        let Value::Int(pid) = scalar(session.as_ref(), "SELECT pg_backend_pid()").await? else {
            return Err("pid is not an integer".into());
        };
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let canceller = {
            let stop = Arc::clone(&stop);
            let spammer = connect(&common::params(port)).await?;
            tokio::spawn(async move {
                let mut sent = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = run(
                        spammer.as_ref(),
                        &format!("SELECT pg_cancel_backend({pid})"),
                    )
                    .await;
                    sent += 1;
                }
                sent
            })
        };
        let started = Instant::now();
        let mut runs = 0;
        while started.elapsed() < Duration::from_secs(4) {
            let _ = run(
                session.as_ref(),
                "SELECT public.varve_ro_evil(); INSERT INTO varve_ro_cancel VALUES (1)",
            )
            .await;
            runs += 1;
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let sent = canceller.await?;
        assert!(sent > 100 && runs > 100, "sent {sent}, runs {runs}");
        assert_eq!(
            scalar(admin.as_ref(), "SELECT count(*) FROM varve_ro_cancel").await?,
            Value::Int(0),
            "a read-only session wrote"
        );
        run(
            admin.as_ref(),
            "DROP TABLE varve_ro_cancel; DROP FUNCTION public.varve_ro_evil()",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn init_sql_leaving_a_transaction_open_owes_the_check() -> TestResult {
    for (_, port) in servers() {
        let mut params = params(port);
        params.init_sql = Some("BEGIN; SET extra_float_digits = 0".into());
        let session = connect(&params).await?;
        let output = run(session.as_ref(), "COMMIT; SELECT 1::float8 / 3 AS f").await?;
        assert_eq!(rows(&output, 1)?, &vec![vec![Value::Float(1.0 / 3.0)]]);
        assert!(
            output
                .notices
                .iter()
                .any(|n| n.message.contains("extra_float_digits = 3")),
            "{:?}",
            output.notices
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_call_and_do_cannot_commit() -> TestResult {
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DROP TABLE IF EXISTS varve_pr_w; CREATE TABLE varve_pr_w (a int);\n\
             CREATE OR REPLACE PROCEDURE public.varve_pr_rw() LANGUAGE plpgsql AS $$ BEGIN \
             PERFORM set_config('default_transaction_read_only', 'off', false); COMMIT; \
             INSERT INTO varve_pr_w VALUES (42); END $$;\n\
             CREATE OR REPLACE PROCEDURE public.varve_pr_rw2() LANGUAGE plpgsql AS $$ BEGIN \
             SET default_transaction_read_only = off; COMMIT; \
             INSERT INTO varve_pr_w VALUES (43); END $$;\n\
             CREATE OR REPLACE PROCEDURE public.varve_pr_ok() LANGUAGE plpgsql AS $$ BEGIN \
             RAISE NOTICE 'ok'; END $$",
        )
        .await?;
        let mut params = params(port);
        params.read_only = true;
        let session = connect(&params).await?;
        for sql in ["CALL public.varve_pr_rw()", "CALL public.varve_pr_rw2()"] {
            let error = run(session.as_ref(), sql)
                .await
                .err()
                .ok_or_else(|| format!("{sql} passed"))?;
            assert_eq!(error.code.as_deref(), Some("2D000"), "{error:?}");
            assert!(
                error
                    .hint
                    .as_deref()
                    .is_some_and(|h| h.contains("read-only transaction")),
                "{error:?}"
            );
            assert_eq!(
                scalar(session.as_ref(), "SHOW default_transaction_read_only").await?,
                Value::Text("on".into())
            );
        }
        // Error positions still point into the user's text.
        let sql = "SELECT 1;\n  CALL public.varve_no_such_proc()";
        let error = run(session.as_ref(), sql)
            .await
            .err()
            .ok_or("unknown procedure ran")?;
        assert_eq!(error.code.as_deref(), Some("42883"), "{error:?}");
        let byte = sql.find("public.varve_no_such_proc").ok_or("missing")?;
        assert_eq!(
            error.position,
            Some(u32::try_from(sql[..byte].chars().count() + 1)?)
        );
        // Harmless procedures and DO blocks still run, alone and in a block.
        let output = run(
            session.as_ref(),
            "CALL public.varve_pr_ok(); DO $$ BEGIN PERFORM 1; END $$; \
             BEGIN; CALL public.varve_pr_ok(); COMMIT",
        )
        .await?;
        assert_eq!(output.results.len(), 5);
        assert_eq!(output.notices.len(), 2, "{:?}", output.notices);
        assert_eq!(
            scalar(writer.as_ref(), "SELECT count(*) FROM varve_pr_w").await?,
            Value::Int(0),
            "a read-only session wrote"
        );
        run(
            writer.as_ref(),
            "DROP TABLE varve_pr_w; DROP PROCEDURE public.varve_pr_rw(); \
             DROP PROCEDURE public.varve_pr_rw2(); DROP PROCEDURE public.varve_pr_ok()",
        )
        .await?;
    }
    Ok(())
}

const UNCHECKED_READS_NOTICE: &str = "float or bytea values read inside that transaction after \
     the change may have been rounded or shown in another format";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn user_code_cannot_leave_float_or_bytea_settings_changed() -> TestResult {
    let third = Value::Float(1.0 / 3.0);
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DROP SCHEMA IF EXISTS varve_bp CASCADE; CREATE SCHEMA varve_bp;\n\
             CREATE FUNCTION varve_bp.efd() RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM set_config('extra_float_digits', '0', false); RETURN 1; END $$;\n\
             CREATE FUNCTION varve_bp.bo() RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM set_config('bytea_output', 'escape', false); RETURN 1; END $$;\n\
             CREATE FUNCTION varve_bp.op(int, int) RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM varve_bp.efd(); RETURN $1 + $2; END $$;\n\
             CREATE OPERATOR varve_bp.### (LEFTARG = int, RIGHTARG = int, FUNCTION = varve_bp.op);\n\
             CREATE FUNCTION varve_bp.chk(int) RETURNS bool LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM varve_bp.efd(); RETURN true; END $$;\n\
             CREATE DOMAIN varve_bp.d AS int CHECK (varve_bp.chk(VALUE));\n\
             CREATE TYPE varve_bp.ct AS (a int);\n\
             CREATE FUNCTION varve_bp.to_ct(int) RETURNS varve_bp.ct LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM varve_bp.efd(); RETURN ROW($1)::varve_bp.ct; END $$;\n\
             CREATE CAST (int AS varve_bp.ct) WITH FUNCTION varve_bp.to_ct(int);\n\
             CREATE VIEW varve_bp.v AS SELECT varve_bp.efd() AS x;\n\
             CREATE VIEW varve_bp.vb AS SELECT varve_bp.bo() AS x;\n\
             CREATE TABLE varve_bp.t (a int);\n\
             CREATE FUNCTION varve_bp.trg() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM varve_bp.efd(); RETURN NEW; END $$;\n\
             CREATE TRIGGER trg BEFORE INSERT OR UPDATE ON varve_bp.t \
             FOR EACH ROW EXECUTE FUNCTION varve_bp.trg();\n\
             CREATE TABLE varve_bp.def (a int DEFAULT varve_bp.efd());\n\
             CREATE TYPE varve_bp.cf AS (f float8)",
        )
        .await?;
        for (label, sql) in [
            ("view", "SELECT * FROM varve_bp.v"),
            ("operator", "SELECT 1 OPERATOR(varve_bp.###) 2 AS o"),
            ("domain", "SELECT 1::varve_bp.d AS dv"),
            ("cast", "SELECT (1::varve_bp.ct)::text AS c"),
            ("trigger insert", "INSERT INTO varve_bp.t VALUES (1)"),
            ("trigger update", "UPDATE varve_bp.t SET a = 2"),
            ("default", "INSERT INTO varve_bp.def DEFAULT VALUES"),
            ("copy from program", "COPY varve_bp.t FROM PROGRAM 'echo 5'"),
        ] {
            let session = connect(&params(port)).await?;
            let output = run(
                session.as_ref(),
                &format!("{sql}; SELECT 1::float8 / 3 AS f"),
            )
            .await?;
            assert_eq!(rows(&output, 1)?, &vec![vec![third.clone()]], "{label}");
            assert!(
                output
                    .notices
                    .iter()
                    .any(|n| n.message.contains("extra_float_digits = 3")),
                "{label}: {:?}",
                output.notices
            );
            // Alone in a run, the next run's float read is checked first.
            run(session.as_ref(), sql).await?;
            let output = run(session.as_ref(), "SELECT 1::float8 / 3 AS f").await?;
            assert_eq!(
                rows(&output, 0)?,
                &vec![vec![third.clone()]],
                "{label} next run"
            );
        }
        let session = connect(&params(port)).await?;
        // A composite with a float field, and bytea.
        let output = run(
            session.as_ref(),
            "SELECT * FROM varve_bp.v; SELECT ROW(1::float8 / 3)::varve_bp.cf AS c",
        )
        .await?;
        assert_eq!(
            rows(&output, 1)?,
            &vec![vec![Value::Other("(0.3333333333333333)".into())]]
        );
        let output = run(
            session.as_ref(),
            "SELECT * FROM varve_bp.vb; SELECT '\\x00ff'::bytea AS b",
        )
        .await?;
        assert_eq!(rows(&output, 1)?, &vec![vec![Value::Bytes(vec![0, 255])]]);
        // Inside a transaction no check can run: the value may be rounded,
        // and a notice says so once the transaction has ended.
        let output = run(
            session.as_ref(),
            "BEGIN; SELECT * FROM varve_bp.v; SELECT 1::float8 / 3 AS f; COMMIT",
        )
        .await?;
        assert!(
            output
                .notices
                .iter()
                .any(|n| n.severity == "NOTICE" && n.message == UNCHECKED_READS_NOTICE),
            "{:?}",
            output.notices
        );
        let output = run(session.as_ref(), "SELECT 1::float8 / 3 AS f").await?;
        assert_eq!(rows(&output, 0)?, &vec![vec![third.clone()]]);
        run(writer.as_ref(), "DROP SCHEMA varve_bp CASCADE").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn other_missing_statements_do_not_reconnect() -> TestResult {
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DROP TYPE IF EXISTS varve_nosuch_e; CREATE TYPE varve_nosuch_e AS ENUM ('a')",
        )
        .await?;
        let session = connect(&params(port)).await?;
        run(
            session.as_ref(),
            "SELECT 'a'::varve_nosuch_e AS e; CREATE TEMP TABLE varve_keep (a int); \
             SET work_mem = '7MB'",
        )
        .await?;
        for sql in [
            "EXECUTE nosuch",
            "EXPLAIN EXECUTE nosuch",
            "DEALLOCATE nosuch",
        ] {
            let error = run(session.as_ref(), sql).await.err().ok_or("nosuch ran")?;
            assert_eq!(error.code.as_deref(), Some("26000"), "{error:?}");
            assert!(!error.message.contains("Roll it back"), "{error:?}");
        }
        let output = run(
            session.as_ref(),
            "SELECT count(*) FROM pg_temp.varve_keep; SHOW work_mem",
        )
        .await?;
        assert!(output.notices.is_empty(), "{:?}", output.notices);
        assert_eq!(rows(&output, 1)?, &vec![vec![Value::Text("7MB".into())]]);
        run(writer.as_ref(), "DROP TYPE varve_nosuch_e").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn an_outside_cancel_is_not_a_timeout() -> TestResult {
    for (_, port) in servers() {
        let admin = connect(&params(port)).await?;
        let mut params = params(port);
        params.statement_timeout = Some(Duration::from_secs(20));
        let session: Arc<dyn Session> = Arc::from(connect(&params).await?);
        let Value::Int(pid) = scalar(session.as_ref(), "SELECT pg_backend_pid()").await? else {
            return Err("pid is not an integer".into());
        };
        let running = Arc::clone(&session);
        let task = tokio::spawn(async move { run(running.as_ref(), "SELECT pg_sleep(10)").await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        run(admin.as_ref(), &format!("SELECT pg_cancel_backend({pid})")).await?;
        let error = tokio::time::timeout(Duration::from_secs(3), task)
            .await??
            .err()
            .ok_or("pg_sleep finished")?;
        assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
        assert!(
            error
                .message
                .starts_with("cancelled by the server or another session"),
            "{error:?}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_gives_up_when_it_cannot_confirm() -> TestResult {
    // A SERIALIZABLE READ ONLY DEFERRABLE transaction waits for a safe
    // snapshot while a serializable read-write transaction is open; with a
    // short statement_timeout every check is cancelled by the server.
    for (_, port) in servers() {
        let admin = connect(&params(port)).await?;
        run(
            admin.as_ref(),
            "DROP TABLE IF EXISTS varve_ro_wait; CREATE TABLE varve_ro_wait (a int)",
        )
        .await?;
        let blocker = connect(&common::params(port)).await?;
        run(
            blocker.as_ref(),
            "BEGIN ISOLATION LEVEL SERIALIZABLE; INSERT INTO varve_ro_wait VALUES (1)",
        )
        .await?;
        let mut params = params(port);
        params.read_only = true;
        let session = connect(&params).await?;
        let error = run(
            session.as_ref(),
            "SET statement_timeout = 200; SET default_transaction_isolation = serializable; \
             SET default_transaction_deferrable = on; SELECT 1",
        )
        .await
        .err()
        .ok_or("SELECT 1 ran without read-only mode confirmed")?;
        assert_eq!(error.kind, ErrorKind::ReadOnly, "{error:?}");
        assert!(error.message.contains("could not confirm"), "{error:?}");
        run(blocker.as_ref(), "ROLLBACK").await?;
        // Once the check can run again, the session works.
        assert_eq!(scalar(session.as_ref(), "SELECT 2").await?, Value::Int(2));
        run(admin.as_ref(), "DROP TABLE varve_ro_wait").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_block_never_leaves_the_session_stuck() -> TestResult {
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "CREATE OR REPLACE PROCEDURE public.varve_pr_quick() LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM 1; END $$",
        )
        .await?;
        let mut params = params(port);
        params.read_only = true;
        let session: Arc<dyn Session> = Arc::from(connect(&params).await?);
        let Value::Int(pid) = scalar(session.as_ref(), "SELECT pg_backend_pid()").await? else {
            return Err("pid is not an integer".into());
        };
        // Phase 1: cancels from another session; phase 2: Varve's own.
        for outside in [true, false] {
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let canceller = {
                let stop = Arc::clone(&stop);
                let spammer = connect(&common::params(port)).await?;
                let handle = session.cancel_handle();
                tokio::spawn(async move {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        if outside {
                            let _ = run(
                                spammer.as_ref(),
                                &format!("SELECT pg_cancel_backend({pid})"),
                            )
                            .await;
                        } else {
                            let _ = handle.cancel().await;
                            tokio::time::sleep(Duration::from_micros(300)).await;
                        }
                    }
                })
            };
            let started = Instant::now();
            let mut stuck = 0;
            let mut runs = 0;
            let mut closed = false;
            while started.elapsed() < Duration::from_secs(3) {
                runs += 1;
                match run(session.as_ref(), "CALL public.varve_pr_quick()").await {
                    Err(e) if e.code.as_deref() == Some("25P02") => stuck += 1,
                    Err(e) if e.kind == ErrorKind::ConnectionLost => {
                        closed = true;
                        break;
                    }
                    Err(e) if !outside => {
                        // Varve's own cancel is reported as such.
                        assert!(!e.message.contains("another session"), "{e:?}");
                    }
                    _ => {}
                }
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            canceller.await?;
            assert_eq!(
                stuck, 0,
                "{stuck} of {runs} runs found an aborted transaction"
            );
            if closed {
                break;
            }
            // Idle again: a plain statement runs.
            assert_eq!(scalar(session.as_ref(), "SELECT 1").await?, Value::Int(1));
        }
        run(writer.as_ref(), "DROP PROCEDURE public.varve_pr_quick()").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn multiranges_over_floats_are_checked() -> TestResult {
    for (major, port) in servers() {
        if major < 14 {
            continue;
        }
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DROP SCHEMA IF EXISTS varve_mr CASCADE; CREATE SCHEMA varve_mr;\n\
             CREATE TYPE varve_mr.fr AS RANGE (subtype = float8, multirange_type_name = varve_mr.frm);\n\
             CREATE FUNCTION varve_mr.efd() RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM set_config('extra_float_digits', '0', false); RETURN 1; END $$;\n\
             CREATE VIEW varve_mr.v AS SELECT varve_mr.efd() AS x",
        )
        .await?;
        let session = connect(&params(port)).await?;
        let output = run(
            session.as_ref(),
            "SELECT * FROM varve_mr.v; \
             SELECT varve_mr.frm(varve_mr.fr(0, 1::float8 / 3)) AS m, 'x'::text AS t",
        )
        .await?;
        let row = &rows(&output, 1)?[0];
        assert!(
            matches!(&row[0], Value::Other(text) if text.contains("0.3333333333333333")),
            "{row:?}"
        );
        run(writer.as_ref(), "DROP SCHEMA varve_mr CASCADE").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_statement_timeout_the_user_sets_is_kept_and_reported() -> TestResult {
    for (_, port) in servers() {
        // Set in the script, without a profile timeout.
        let session = connect(&params(port)).await?;
        let error = run(
            session.as_ref(),
            "SET statement_timeout = 200; SELECT pg_sleep(3)",
        )
        .await
        .err()
        .ok_or("pg_sleep finished")?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
        assert!(!error.message.contains("another session"), "{error:?}");

        // Set by the init SQL.
        let mut init = params(port);
        init.init_sql = Some("SET statement_timeout = 200".into());
        let session = connect(&init).await?;
        let error = run(session.as_ref(), "SELECT pg_sleep(3)")
            .await
            .err()
            .ok_or("finished")?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");

        // A profile timeout is a default: the user's value stays.
        let mut profile = params(port);
        profile.statement_timeout = Some(Duration::from_secs(20));
        let session = connect(&profile).await?;
        let output = run(session.as_ref(), "SET statement_timeout = 300").await?;
        assert!(output.notices.is_empty(), "{:?}", output.notices);
        assert_eq!(
            scalar(session.as_ref(), "SHOW statement_timeout").await?,
            Value::Text("300ms".into())
        );
        let error = run(session.as_ref(), "SELECT pg_sleep(3)")
            .await
            .err()
            .ok_or("finished")?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
        // RESET brings the profile's value back.
        run(session.as_ref(), "RESET statement_timeout").await?;
        assert_eq!(
            scalar(session.as_ref(), "SHOW statement_timeout").await?,
            Value::Text("20s".into())
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_block_failures_leave_the_session_idle() -> TestResult {
    // A proxy turns the driver's own BEGIN or COMMIT around a CALL into a
    // failing one that leaves the transaction aborted, as a cancel landing
    // on it would.
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "CREATE OR REPLACE PROCEDURE public.varve_pr_idle() LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM 1; END $$",
        )
        .await?;
        for (from, to) in [
            ("BEGIN READ ONLY", "BEGIN READ ONLY; SELECT 1/0"),
            ("COMMIT", "SELECT 1/0; COMMIT"),
        ] {
            let proxy = rewriting_proxy(port, from, to).await?;
            let mut params = params(proxy.port);
            params.read_only = true;
            let session = connect(&params).await?;
            let error = run(session.as_ref(), "CALL public.varve_pr_idle()")
                .await
                .err()
                .ok_or_else(|| format!("the failing {from} passed"))?;
            assert_eq!(error.code.as_deref(), Some("22012"), "{from}: {error:?}");
            // Not stuck in an aborted transaction.
            assert_eq!(
                scalar(session.as_ref(), "SELECT 1").await?,
                Value::Int(1),
                "{from}"
            );
            assert_eq!(
                scalar(session.as_ref(), "SHOW default_transaction_read_only").await?,
                Value::Text("on".into())
            );
        }

        // Varve's own cancel landing on the BEGIN is reported as a cancel.
        let proxy = rewriting_proxy(
            port,
            "BEGIN READ ONLY",
            "BEGIN READ ONLY; SELECT pg_sleep(10)",
        )
        .await?;
        let mut params = params(proxy.port);
        params.read_only = true;
        let session: Arc<dyn Session> = Arc::from(connect(&params).await?);
        let running = Arc::clone(&session);
        let task =
            tokio::spawn(async move { run(running.as_ref(), "CALL public.varve_pr_idle()").await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        session.cancel_handle().cancel().await?;
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await??
            .err()
            .ok_or("the CALL finished")?;
        assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
        assert!(!error.message.contains("another session"), "{error:?}");
        assert_eq!(scalar(session.as_ref(), "SELECT 1").await?, Value::Int(1));
        run(writer.as_ref(), "DROP PROCEDURE public.varve_pr_idle()").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn reports_whether_a_transaction_is_open() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        assert_eq!(session.in_transaction(), Some(false), "after connect");
        run(session.as_ref(), "BEGIN").await?;
        assert_eq!(session.in_transaction(), Some(true), "after BEGIN");
        assert!(run(session.as_ref(), "SELECT 1/0").await.is_err());
        assert_eq!(session.in_transaction(), Some(true), "aborted");
        run(session.as_ref(), "ROLLBACK").await?;
        assert_eq!(session.in_transaction(), Some(false), "after ROLLBACK");
        run(session.as_ref(), "BEGIN; SELECT 1; COMMIT").await?;
        assert_eq!(session.in_transaction(), Some(false), "after COMMIT");
        // A failing prepare inside a transaction, then the run ends.
        assert!(
            run(
                session.as_ref(),
                "BEGIN; SELECT no_such_column FROM pg_class"
            )
            .await
            .is_err()
        );
        assert_eq!(session.in_transaction(), Some(true), "aborted at prepare");
        run(session.as_ref(), "ROLLBACK").await?;
        run(session.as_ref(), "DISCARD ALL").await?;
        assert_eq!(
            session.in_transaction(),
            Some(false),
            "after the DISCARD ALL reconnect"
        );

        // The read-only wrapper's own BEGIN/COMMIT around a CALL or DO.
        let mut params = params(port);
        params.read_only = true;
        let session = connect(&params).await?;
        run(session.as_ref(), "DO $$ BEGIN PERFORM 1; END $$").await?;
        assert_eq!(session.in_transaction(), Some(false), "after a wrapped DO");
        assert!(
            run(session.as_ref(), "DO $$ BEGIN PERFORM 1/0; END $$")
                .await
                .is_err()
        );
        assert_eq!(
            session.in_transaction(),
            Some(false),
            "after a failed wrapped DO"
        );
        run(session.as_ref(), "BEGIN; DO $$ BEGIN PERFORM 1; END $$").await?;
        assert_eq!(
            session.in_transaction(),
            Some(true),
            "DO inside the user's transaction"
        );
        run(session.as_ref(), "COMMIT").await?;
        assert_eq!(session.in_transaction(), Some(false));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_block_rollback_failures() -> TestResult {
    for (_, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "CREATE OR REPLACE PROCEDURE public.varve_pr_rb() LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM 1; END $$",
        )
        .await?;
        for failures in 1..=3 {
            // The BEGIN leaves the block aborted; the ROLLBACKs fail.
            let proxy = rewriting_proxy_rules(
                port,
                &[
                    ("BEGIN READ ONLY", "BEGIN READ ONLY; SELECT 1/0", 1),
                    ("ROLLBACK", "SELECT 1/0", failures),
                ],
            )
            .await?;
            let mut params = params(proxy.port);
            params.read_only = true;
            let session = connect(&params).await?;
            let error = run(session.as_ref(), "CALL public.varve_pr_rb()")
                .await
                .err()
                .ok_or("the failing BEGIN passed")?;
            if failures < 3 {
                assert_eq!(
                    error.code.as_deref(),
                    Some("22012"),
                    "{failures}: {error:?}"
                );
                assert_eq!(scalar(session.as_ref(), "SELECT 1").await?, Value::Int(1));
            } else {
                // Still in the aborted transaction after 3 tries: closed.
                assert_eq!(error.kind, ErrorKind::ConnectionLost, "{error:?}");
                let error = run(session.as_ref(), "SELECT 1").await.err().ok_or("ran")?;
                assert_eq!(error.kind, ErrorKind::ConnectionLost, "{error:?}");
            }
        }
        run(writer.as_ref(), "DROP PROCEDURE public.varve_pr_rb()").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn the_users_statement_timeout_survives_other_checks() -> TestResult {
    for (_, port) in servers() {
        let mut profile = params(port);
        profile.statement_timeout = Some(Duration::from_secs(20));
        let session = connect(&profile).await?;
        run(
            session.as_ref(),
            "CREATE FUNCTION pg_temp.varve_f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        )
        .await?;
        run(session.as_ref(), "SET statement_timeout = 0").await?;
        for sql in [
            "SELECT now()",
            "SELECT count(*) FROM pg_class",
            "SELECT pg_temp.varve_f()",
            "SELECT 1::float8 / 3",
        ] {
            let output = run(session.as_ref(), sql).await?;
            assert!(output.notices.is_empty(), "{sql}: {:?}", output.notices);
            assert_eq!(
                scalar(session.as_ref(), "SHOW statement_timeout").await?,
                Value::Text("0".into()),
                "{sql}"
            );
        }
        // SET ... TO DEFAULT behaves like RESET: the profile's value.
        run(session.as_ref(), "SET statement_timeout TO DEFAULT").await?;
        assert_eq!(
            scalar(session.as_ref(), "SHOW statement_timeout").await?,
            Value::Text("20s".into())
        );

        // The same from the init SQL.
        let mut init = profile.clone();
        init.init_sql = Some("SET statement_timeout = 0".into());
        let session = connect(&init).await?;
        run(session.as_ref(), "SELECT count(*) FROM pg_class").await?;
        assert_eq!(
            scalar(session.as_ref(), "SHOW statement_timeout").await?,
            Value::Text("0".into())
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn timeouts_from_any_source_are_timeouts() -> TestResult {
    for (_, port) in servers() {
        let admin = connect(&params(port)).await?;
        let role = "varve_to_role";
        run(
            admin.as_ref(),
            &format!(
                "DROP ROLE IF EXISTS {role}; CREATE ROLE {role} LOGIN PASSWORD 'varve_dev';\n\
                 ALTER ROLE {role} SET statement_timeout = 200"
            ),
        )
        .await?;
        let mut as_role = params(port);
        as_role.username = Some(role.into());
        let outcome = async {
            let session = connect(&as_role).await?;
            run(session.as_ref(), "SELECT pg_sleep(3)")
                .await
                .err()
                .ok_or("finished".into())
        }
        .await;
        run(admin.as_ref(), &format!("DROP ROLE {role}")).await?;
        let error: varve_driver::DriverError =
            outcome.map_err(|e: Box<dyn std::error::Error>| e)?;
        assert_eq!(error.kind, ErrorKind::Timeout, "ALTER ROLE: {error:?}");

        let session = connect(&params(port)).await?;
        for sql in [
            "BEGIN; SET LOCAL statement_timeout = 200; SELECT pg_sleep(3)",
            "BEGIN; SET statement_timeout = 200; SELECT pg_sleep(3)",
            "SELECT set_config('statement_timeout', '200', false); SELECT pg_sleep(3)",
        ] {
            let error = run(session.as_ref(), sql).await.err().ok_or("finished")?;
            assert_eq!(error.kind, ErrorKind::Timeout, "{sql}: {error:?}");
            run(session.as_ref(), "ROLLBACK; SET statement_timeout = 0").await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn geometric_values_keep_their_digits() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        run(
            session.as_ref(),
            "CREATE FUNCTION pg_temp.varve_efd() RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM set_config('extra_float_digits', '0', false); RETURN 1; END $$;\n\
             CREATE TEMP VIEW varve_geo_v AS SELECT pg_temp.varve_efd() AS x",
        )
        .await?;
        for sql in [
            "SELECT point(1::float8 / 3, 0) AS g",
            "SELECT ARRAY[circle(point(0, 0), 1::float8 / 3)] AS g",
        ] {
            let output = run(
                session.as_ref(),
                &format!("SELECT * FROM varve_geo_v; {sql}"),
            )
            .await?;
            let value = &rows(&output, 1)?[0][0];
            assert!(
                format!("{value:?}").contains("0.3333333333333333"),
                "{sql}: {value:?}"
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_dead_session_fails_fast() -> TestResult {
    for (_, port) in servers() {
        dead_sessions_fail_fast(params(port), params(port)).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_cut_connection_has_no_transaction_status() -> TestResult {
    for (_, port) in servers() {
        for read_only in [false, true] {
            for (setup, in_tx) in [("", Some(false)), ("BEGIN", Some(true))] {
                let case = format!("read_only={read_only} cut after {setup:?}");
                let cutter = cutting_proxy(port).await?;
                let mut params = params(cutter.proxy.port);
                params.read_only = read_only;
                let session = connect(&params).await?;
                if !setup.is_empty() {
                    run(session.as_ref(), setup).await?;
                }
                assert_eq!(session.in_transaction(), in_tx, "{case}");
                // No message from the server: the socket just closes.
                cutter.cut();
                wait_until_dead(session.as_ref(), &case).await?;
                assert_dead(session.as_ref(), &case).await?;
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_cut_during_a_call_has_no_transaction_status() -> TestResult {
    for (_, port) in servers() {
        for sql in ["SELECT pg_sleep(10)", "BEGIN; SELECT pg_sleep(10)"] {
            let cutter = cutting_proxy(port).await?;
            let session: Arc<dyn Session> = Arc::from(connect(&params(cutter.proxy.port)).await?);
            let running = Arc::clone(&session);
            let task = tokio::spawn(async move { run(running.as_ref(), sql).await });
            tokio::time::sleep(Duration::from_millis(300)).await;
            let cut = Instant::now();
            cutter.cut();
            let error = tokio::time::timeout(Duration::from_secs(5), task)
                .await??
                .err()
                .ok_or("finished")?;
            assert_eq!(error.kind, ErrorKind::ConnectionLost, "{sql}: {error:?}");
            assert!(
                cut.elapsed() < Duration::from_secs(1),
                "{sql}: {:?}",
                cut.elapsed()
            );
            assert_dead(session.as_ref(), sql).await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_reset_that_was_rolled_back_keeps_the_users_value() -> TestResult {
    for (_, port) in servers() {
        let mut profile = params(port);
        profile.statement_timeout = Some(Duration::from_secs(20));
        let session = connect(&profile).await?;
        for (script, want) in [
            // The user's 0 equals the server default: a rolled-back RESET
            // must still leave it alone.
            (
                "SET statement_timeout = 0; BEGIN; RESET statement_timeout; ROLLBACK",
                "0",
            ),
            ("SET statement_timeout = 0; BEGIN; RESET ALL; ROLLBACK", "0"),
            (
                "SET statement_timeout = 0; BEGIN; SET statement_timeout TO DEFAULT; ROLLBACK",
                "0",
            ),
            (
                "SET statement_timeout = 0; BEGIN; SAVEPOINT a; RESET statement_timeout; \
                 ROLLBACK TO a; COMMIT",
                "0",
            ),
            (
                "SET statement_timeout = 0; BEGIN; SAVEPOINT a; RESET statement_timeout; \
                 RELEASE a; COMMIT",
                "20s",
            ),
            (
                "SET statement_timeout = 0; BEGIN; RESET statement_timeout; COMMIT",
                "20s",
            ),
            ("SET statement_timeout = 0; RESET statement_timeout", "20s"),
            (
                "SET statement_timeout = 5000; BEGIN; RESET statement_timeout; ROLLBACK",
                "5s",
            ),
        ] {
            run(session.as_ref(), script).await?;
            assert_eq!(
                scalar(session.as_ref(), "SHOW statement_timeout").await?,
                Value::Text(want.into()),
                "{script}"
            );
            run(session.as_ref(), "RESET statement_timeout").await?;
        }
        // The same as separate runs.
        run(session.as_ref(), "SET statement_timeout = 0").await?;
        run(session.as_ref(), "BEGIN").await?;
        run(session.as_ref(), "RESET statement_timeout").await?;
        run(session.as_ref(), "ROLLBACK").await?;
        assert_eq!(
            scalar(session.as_ref(), "SHOW statement_timeout").await?,
            Value::Text("0".into())
        );

        // A role default of 300 ms, and the user sets 300 ms too.
        let admin = connect(&params(port)).await?;
        let role = "varve_reset_role";
        run(
            admin.as_ref(),
            &format!(
                "DROP ROLE IF EXISTS {role}; CREATE ROLE {role} LOGIN PASSWORD 'varve_dev';\n\
                 ALTER ROLE {role} SET statement_timeout = 300"
            ),
        )
        .await?;
        let mut as_role = profile.clone();
        as_role.username = Some(role.into());
        let outcome = async {
            let session = connect(&as_role).await?;
            let mut seen = Vec::new();
            seen.push(scalar(session.as_ref(), "SHOW statement_timeout").await?);
            run(
                session.as_ref(),
                "SET statement_timeout = 300; BEGIN; RESET statement_timeout; ROLLBACK",
            )
            .await?;
            seen.push(scalar(session.as_ref(), "SHOW statement_timeout").await?);
            run(session.as_ref(), "RESET statement_timeout").await?;
            seen.push(scalar(session.as_ref(), "SHOW statement_timeout").await?);
            Ok::<_, Box<dyn std::error::Error>>(seen)
        }
        .await;
        run(admin.as_ref(), &format!("DROP ROLE {role}")).await?;
        assert_eq!(
            outcome?,
            vec![
                Value::Text("20s".into()),
                Value::Text("300ms".into()),
                Value::Text("20s".into())
            ]
        );

        // In the init SQL.
        for (init_sql, want) in [
            ("RESET statement_timeout", "20s"),
            ("SET statement_timeout TO DEFAULT", "20s"),
            ("RESET ALL", "20s"),
            ("SET statement_timeout = 300", "300ms"),
            // The init SQL is one query: the BEGIN takes the SET before it
            // into the transaction, and the ROLLBACK undoes both.
            (
                "SET statement_timeout = 0; BEGIN; RESET statement_timeout; ROLLBACK",
                "20s",
            ),
            (
                "SET statement_timeout = 0; BEGIN; SAVEPOINT a; RESET statement_timeout; \
                 ROLLBACK TO a; COMMIT",
                "0",
            ),
            ("SET statement_timeout = 0; RESET statement_timeout", "20s"),
        ] {
            let mut init = profile.clone();
            init.init_sql = Some(init_sql.into());
            let session = connect(&init).await?;
            assert_eq!(
                scalar(session.as_ref(), "SHOW statement_timeout").await?,
                Value::Text(want.into()),
                "{init_sql}"
            );
        }
    }
    Ok(())
}

/// The error of `sql` on `session`, which must fail.
async fn failure(session: &dyn Session, sql: &str) -> Result<varve_driver::DriverError, String> {
    run(session, sql)
        .await
        .err()
        .ok_or_else(|| format!("{sql}: finished"))
}

/// Statements that time out, each after the timeout in force when it ran,
/// however that was set, with a profile timeout of 1 s. `ROLLBACK` follows
/// each.
const TIMEOUT_CASES: [&str; 9] = [
    // A session SET after a SET LOCAL in the same transaction wins.
    "BEGIN; SET LOCAL statement_timeout = 5000; SET statement_timeout = 300; SELECT pg_sleep(3)",
    // The SET LOCAL was rolled back with its savepoint.
    "BEGIN; SAVEPOINT a; SET LOCAL statement_timeout = 5000; ROLLBACK TO a; SELECT pg_sleep(1.5)",
    // A set_config that only appears in a comment or a string changes nothing.
    "SELECT 1 /* set_config('statement_timeout', '100000', false) */; SELECT pg_sleep(1.5)",
    "SELECT 'set_config(''statement_timeout'', ''100000'', false)'; SELECT pg_sleep(1.5)",
    // A function changes it, for the rest of the run.
    "SELECT pg_temp.varve_short_timeout(); SELECT pg_sleep(3)",
    "BEGIN; SELECT pg_temp.varve_short_timeout(); SELECT pg_sleep(3)",
    // SET LOCAL ends with its transaction.
    "BEGIN; SET LOCAL statement_timeout = 5000; COMMIT; SELECT pg_sleep(1.5)",
    "BEGIN; SET LOCAL statement_timeout = 5000; ROLLBACK; SELECT pg_sleep(1.5)",
    "BEGIN; SELECT set_config('statement_timeout', '5000', true); COMMIT; SELECT pg_sleep(1.5)",
];

/// Runs [`TIMEOUT_CASES`] on a fresh session with `params` and checks each
/// is a timeout with message `message`; also that an outside cancel is not.
async fn timeouts_are_timeouts(
    params: &varve_driver::ConnectParams,
    admin: &dyn Session,
    message: &str,
    cancel_message: &str,
    learn_first: bool,
) -> TestResult {
    let session: Arc<dyn Session> = Arc::from(connect(params).await?);
    if learn_first {
        // An autocommitted timeout is told apart without knowing the
        // language, and teaches it.
        let error = failure(session.as_ref(), "SELECT pg_sleep(1.5)").await?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
        assert_eq!(error.message, message);
    }
    run(
        session.as_ref(),
        "CREATE FUNCTION pg_temp.varve_short_timeout() RETURNS int LANGUAGE plpgsql AS \
         $$ BEGIN PERFORM set_config('statement_timeout', '300', false); RETURN 1; END $$",
    )
    .await?;
    for sql in TIMEOUT_CASES {
        let error = failure(session.as_ref(), sql).await?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{sql}: {error:?}");
        assert_eq!(error.message, message, "{sql}");
        run(session.as_ref(), "ROLLBACK; RESET statement_timeout").await?;
        // Nothing of it lasts into the next run.
        let error = failure(session.as_ref(), "SELECT pg_sleep(1.5)").await?;
        assert_eq!(error.kind, ErrorKind::Timeout, "after {sql}: {error:?}");
    }
    // A cancel from another session is not a timeout, even long after the
    // timeout of an earlier transaction would have passed.
    let pid = backend_pid(session.as_ref()).await?;
    for sql in [
        "SELECT pg_sleep(0.5); SET statement_timeout = 100000; SELECT pg_sleep(10)",
        "BEGIN; SET LOCAL statement_timeout = 100000; SELECT pg_sleep(10)",
    ] {
        let running = Arc::clone(&session);
        let task = tokio::spawn(async move { run(running.as_ref(), sql).await });
        tokio::time::sleep(Duration::from_millis(1500)).await;
        run(admin, &format!("SELECT pg_cancel_backend({pid})")).await?;
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await??
            .err()
            .ok_or("pg_sleep finished")?;
        assert_eq!(error.kind, ErrorKind::Cancelled, "{sql}: {error:?}");
        assert_eq!(
            error.message,
            format!("cancelled by the server or another session: {cancel_message}"),
            "{sql}"
        );
        run(session.as_ref(), "ROLLBACK; RESET statement_timeout").await?;
    }
    // Varve's own cancel is a plain cancel.
    let running = Arc::clone(&session);
    let task =
        tokio::spawn(async move { run(running.as_ref(), "BEGIN; SELECT pg_sleep(10)").await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.cancel_handle().cancel().await?;
    let error = tokio::time::timeout(Duration::from_secs(5), task)
        .await??
        .err()
        .ok_or("pg_sleep finished")?;
    assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
    assert_eq!(error.message, cancel_message);
    run(session.as_ref(), "ROLLBACK").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn timeouts_are_told_from_cancels_whatever_set_them() -> TestResult {
    for (_, port) in servers() {
        let admin = connect(&params(port)).await?;
        let mut profile = params(port);
        profile.statement_timeout = Some(Duration::from_secs(1));
        timeouts_are_timeouts(
            &profile,
            admin.as_ref(),
            "canceling statement due to statement timeout",
            "canceling statement due to user request",
            false,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn translated_timeouts_are_told_from_cancels() -> TestResult {
    for (_, port) in servers() {
        let admin = connect(&params(port)).await?;
        let proxy = translating_proxy(port).await?;
        let mut profile = params(proxy.port);
        profile.tls.mode = varve_driver::TlsMode::Disable;
        profile.statement_timeout = Some(Duration::from_secs(1));
        // The server says its messages are German (the test images have no
        // translations; the proxy translates).
        profile.init_sql = Some("SET lc_messages = 'de_DE.UTF-8'".into());
        timeouts_are_timeouts(
            &profile,
            admin.as_ref(),
            TRANSLATED_TIMEOUT,
            TRANSLATED_CANCEL,
            false,
        )
        .await?;
        // A server that says English but translates: learned from the
        // first timeout.
        profile.init_sql = None;
        timeouts_are_timeouts(
            &profile,
            admin.as_ref(),
            TRANSLATED_TIMEOUT,
            TRANSLATED_CANCEL,
            true,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn cube_values_keep_their_digits() -> TestResult {
    for (major, port) in servers() {
        let session = connect(&params(port)).await?;
        if let Err(error) = run(session.as_ref(), "CREATE EXTENSION IF NOT EXISTS cube").await {
            eprintln!("PostgreSQL {major}: the cube extension is not available, skipped: {error}");
            continue;
        }
        run(
            session.as_ref(),
            "CREATE FUNCTION pg_temp.varve_efd() RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM set_config('extra_float_digits', '0', false); RETURN 1; END $$;\n\
             CREATE TEMP VIEW varve_cube_v AS SELECT pg_temp.varve_efd() AS x",
        )
        .await?;
        for sql in [
            "SELECT cube(1::float8 / 3) AS g",
            "SELECT ARRAY[cube(ARRAY[1::float8 / 3, 2])] AS g",
        ] {
            // Same run as the change, and the run after it.
            let output = run(
                session.as_ref(),
                &format!("SELECT * FROM varve_cube_v; {sql}"),
            )
            .await?;
            let value = &rows(&output, 1)?[0][0];
            assert!(
                format!("{value:?}").contains("0.3333333333333333"),
                "{sql}: {value:?}"
            );
            run(session.as_ref(), "SELECT pg_temp.varve_efd()").await?;
            let output = run(session.as_ref(), sql).await?;
            let value = &rows(&output, 0)?[0][0];
            assert!(
                format!("{value:?}").contains("0.3333333333333333"),
                "{sql} next run: {value:?}"
            );
        }
    }
    Ok(())
}
