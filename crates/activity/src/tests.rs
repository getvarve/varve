//! Recorder tests against an in-memory `app.db`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;
use varve_store::{Store, core_migrations};

use crate::query::page_sql;
use crate::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const DAY: i64 = 86_400_000;
/// 2026-10-19T12:00:00Z.
const NOON: i64 = 1_792_411_200_000;

fn recorder() -> Result<(Arc<Store>, Recorder), Box<dyn std::error::Error>> {
    let store = Arc::new(Store::open_in_memory()?);
    store.migrate(&[core_migrations()])?;
    let settings = RecorderSettings {
        actor_id: "user-1".into(),
        device_id: "device-1".into(),
        text_level: TextLevel::Normalized,
    };
    Ok((Arc::clone(&store), Recorder::new(store, settings)))
}

fn event_at(action: &str, category: Category, at: i64) -> Result<ActivityEvent, ActivityError> {
    ActivityEvent::builder(category, action)
        .occurred_at(at)
        .build()
}

fn count_rows(store: &Store) -> Result<i64, Box<dyn std::error::Error>> {
    Ok(store
        .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM core_activity", [], |r| r.get(0))?))?)
}

#[test]
fn records_and_reads_back_an_event() -> TestResult {
    let (store, rec) = recorder()?;
    let event = ActivityEvent::connection_opened(&ConnectionDetails {
        engine: varve_core::Engine::Mysql,
        server_version: Some("8.4.3".into()),
        host: "db".into(),
        port: Some(3306),
        database: None,
        username: Some("root".into()),
        via_ssh: false,
        tls_mode: Some("required".into()),
    })
    .connection("c1")
    .server_key("db:3306")
    .workspace("personal")
    .build()?;
    let id = event.id;
    rec.record(event)?;

    let page = rec.list(&ActivityFilter::default())?;
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.next_cursor, None);
    let stored = &page.events[0];
    assert_eq!(stored.id, id);
    assert_eq!(stored.actor_id, "user-1");
    assert_eq!(stored.device_id.as_deref(), Some("device-1"));
    assert_eq!(stored.connection_id.as_deref(), Some("c1"));
    assert_eq!(stored.server_key.as_deref(), Some("db:3306"));
    assert_eq!(stored.workspace_id.as_deref(), Some("personal"));
    assert_eq!(stored.category, Category::Connection);
    assert_eq!(stored.details["engine"], "mysql");
    assert_eq!(stored.details["port"], 3306);

    let state: String = store.with_conn(|c| {
        Ok(c.query_row("SELECT upload_state FROM core_activity", [], |r| r.get(0))?)
    })?;
    assert_eq!(state, "local_only");
    Ok(())
}

#[test]
fn explicit_actor_and_device_are_kept() -> TestResult {
    let (_, rec) = recorder()?;
    rec.record(
        ActivityEvent::app_started("1.0.0")
            .actor("other")
            .device("dev-9")
            .build()?,
    )?;
    let e = &rec.list(&ActivityFilter::default())?.events[0];
    assert_eq!(e.actor_id, "other");
    assert_eq!(e.device_id.as_deref(), Some("dev-9"));
    Ok(())
}

#[test]
fn invalid_events_are_rejected() -> TestResult {
    let (store, rec) = recorder()?;
    let mut e = ActivityEvent::app_started("1").build()?;
    e.action = "Not Valid".into();
    assert!(matches!(
        rec.record(e),
        Err(ActivityError::InvalidAction(_))
    ));

    let mut e = ActivityEvent::app_started("1").build()?;
    e.details = json!([1, 2]);
    assert!(matches!(rec.record(e), Err(ActivityError::InvalidEvent(_))));

    rec.set_settings(RecorderSettings {
        actor_id: " ".into(),
        device_id: "d".into(),
        text_level: TextLevel::Full,
    });
    let e = ActivityEvent::app_started("1").build()?;
    assert!(matches!(rec.record(e), Err(ActivityError::InvalidEvent(_))));

    // A batch with one invalid event stores nothing.
    rec.set_settings(RecorderSettings::local("d"));
    let good = ActivityEvent::app_started("1").build()?;
    let mut bad = ActivityEvent::app_started("1").build()?;
    bad.action = "x".into();
    assert!(rec.record_batch(vec![good, bad]).is_err());
    assert_eq!(count_rows(&store)?, 0);
    Ok(())
}

fn record_query(
    rec: &Recorder,
    text: &str,
    dialect: SqlDialect,
) -> Result<ActivityEvent, Box<dyn std::error::Error>> {
    rec.record(
        ActivityEvent::query_executed(text, dialect, "ddl", RowCounts::default(), 2)
            .connection("c1")
            .build()?,
    )?;
    Ok(rec.query_history("c1", 1)?.remove(0))
}

#[test]
fn statement_text_follows_the_level_and_never_holds_secrets() -> TestResult {
    let (store, rec) = recorder()?;
    let text = "ALTER ROLE app WITH PASSWORD 'Tr0ub4dor' VALID UNTIL '2030-01-01'";

    rec.set_text_level(TextLevel::None);
    let e = record_query(&rec, text, SqlDialect::Postgres)?;
    assert!(e.details.get("statement").is_none());
    assert_eq!(e.details["text_level"], "none");
    assert_eq!(e.details["kind"], "ddl");

    rec.set_text_level(TextLevel::Normalized);
    let e = record_query(&rec, text, SqlDialect::Postgres)?;
    assert_eq!(
        e.details["statement"],
        "ALTER ROLE ? WITH PASSWORD ? VALID UNTIL ?"
    );

    rec.set_text_level(TextLevel::Full);
    let e = record_query(&rec, text, SqlDialect::Postgres)?;
    assert_eq!(
        e.details["statement"],
        "ALTER ROLE app WITH PASSWORD '[redacted]' VALID UNTIL '2030-01-01'"
    );

    let e = record_query(&rec, "CONFIG SET requirepass Tr0ub4dor", SqlDialect::Redis)?;
    assert_eq!(e.details["statement"], "CONFIG SET requirepass [redacted]");

    let raw: Vec<String> = store.with_conn(|c| {
        let mut stmt = c.prepare("SELECT details || coalesce(target, '') FROM core_activity")?;
        let rows = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    })?;
    assert_eq!(raw.len(), 4);
    assert!(raw.iter().all(|d| !d.contains("Tr0ub4dor")));
    Ok(())
}

#[test]
fn caller_supplied_statement_details_are_replaced() -> TestResult {
    let (_, rec) = recorder()?;
    rec.record(
        ActivityEvent::builder(Category::Query, "query.executed")
            .detail("statement", "ALTER ROLE a PASSWORD 'Tr0ub4dor'")
            .detail("text_level", "full")
            .build()?,
    )?;
    let e = &rec.list(&ActivityFilter::default())?.events[0];
    assert!(e.details.get("statement").is_none());
    assert!(e.details.get("text_level").is_none());
    Ok(())
}

#[test]
fn long_statements_are_truncated() -> TestResult {
    let (_, rec) = recorder()?;
    rec.set_text_level(TextLevel::Full);
    let text = format!("SELECT '{}'", "é".repeat(MAX_STATEMENT_BYTES));
    let e = record_query(&rec, &text, SqlDialect::Postgres)?;
    let stored = e.details["statement"].as_str().unwrap_or_default();
    assert!(stored.len() <= MAX_STATEMENT_BYTES);
    assert!(stored.len() > MAX_STATEMENT_BYTES - 4);
    assert_eq!(e.details["statement_truncated"], true);
    Ok(())
}

#[test]
fn credentials_in_details_and_target_are_redacted() -> TestResult {
    let (_, rec) = recorder()?;
    rec.record(
        ActivityEvent::connection_failed(
            &ConnectionDetails {
                engine: varve_core::Engine::Postgres,
                server_version: None,
                host: "db".into(),
                port: None,
                database: None,
                username: None,
                via_ssh: false,
                tls_mode: None,
            },
            &ErrorDetails {
                code: None,
                message: "invalid dsn postgres://a:Tr0ub4dor@db/x (password=Tr0ub4dor)".into(),
            },
        )
        .target("redis://:Tr0ub4dor@cache")
        .detail("nested", json!({ "list": ["password=Tr0ub4dor"] }))
        .build()?,
    )?;
    let e = &rec.list(&ActivityFilter::default())?.events[0];
    let all = serde_json::to_string(e)?;
    assert!(!all.contains("Tr0ub4dor"), "{all}");
    assert_eq!(e.target.as_deref(), Some("redis://:[redacted]@cache"));
    Ok(())
}

/// The three messages from the review: each server echoes `hunter2`.
const ECHOING_ERRORS: [(SqlDialect, &str, &str, &str); 3] = [
    (
        SqlDialect::Mysql,
        "CREATE USER u IDENTIFED BY 'hunter2'",
        "1064 (42000)",
        "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'IDENTIFED BY 'hunter2'' at line 1",
    ),
    (
        SqlDialect::Postgres,
        "ALTER ROLE a PASSWORD hunter2",
        "42601",
        "syntax error at or near \"hunter2\"",
    ),
    (
        SqlDialect::Redis,
        "AUHT hunter2",
        "ERR",
        "ERR unknown command 'AUHT', with args beginning with: 'hunter2' ",
    ),
];

fn record_failed(
    rec: &Recorder,
    dialect: SqlDialect,
    statement: &str,
    code: &str,
    message: &str,
) -> Result<ActivityEvent, Box<dyn std::error::Error>> {
    rec.record(
        ActivityEvent::query_failed(
            statement,
            dialect,
            "other",
            &ErrorDetails {
                code: Some(code.into()),
                message: message.into(),
            },
            1,
        )
        .connection("c1")
        .build()?,
    )?;
    Ok(rec.query_history("c1", 1)?.remove(0))
}

#[test]
fn error_messages_follow_the_text_level() -> TestResult {
    let (_, rec) = recorder()?;
    for (dialect, statement, code, message) in ECHOING_ERRORS {
        rec.set_text_level(TextLevel::None);
        let e = record_failed(&rec, dialect, statement, code, message)?;
        assert_eq!(e.details["error"]["code"], code);
        assert_eq!(e.details["error"]["message"], ERROR_MESSAGE_HIDDEN);
        assert_eq!(e.details["text_level"], "none");
        assert!(!serde_json::to_string(&e)?.contains("hunter2"));

        rec.set_text_level(TextLevel::Normalized);
        let e = record_failed(&rec, dialect, statement, code, message)?;
        assert_eq!(e.details["error"]["message"], SYNTAX_ERROR_HIDDEN);
        assert!(!serde_json::to_string(&e)?.contains("hunter2"));

        rec.set_text_level(TextLevel::Full);
        let e = record_failed(&rec, dialect, statement, code, message)?;
        let stored = e.details["error"]["message"].as_str().unwrap_or_default();
        assert_eq!(
            stored,
            prepare_error_message_for_test(statement, code, message, dialect)
        );
        if dialect != SqlDialect::Redis {
            // The statement redactor removed `hunter2`, so the echo goes too.
            assert!(!serde_json::to_string(&e)?.contains("hunter2"), "{stored}");
            assert!(stored.contains(REDACTED), "{stored}");
        }
    }
    // Full keeps a message that echoes nothing secret.
    let e = record_failed(
        &rec,
        SqlDialect::Postgres,
        "SELECT * FROM missing",
        "42P01",
        "relation \"missing\" does not exist",
    )?;
    assert_eq!(
        e.details["error"]["message"],
        "relation \"missing\" does not exist"
    );
    Ok(())
}

/// What the recorder stores at `full` for a failed `statement`.
fn prepare_error_message_for_test(
    statement: &str,
    code: &str,
    message: &str,
    dialect: SqlDialect,
) -> String {
    let (_, secrets) = crate::redact::redact_secrets_tracked(statement, dialect);
    crate::redact::prepare_error_message_with(
        message,
        Some(code),
        Some(dialect),
        TextLevel::Full,
        Some(&secrets),
        &[],
        false,
    )
}

#[test]
fn errors_set_as_plain_details_are_replaced() -> TestResult {
    let (_, rec) = recorder()?;
    rec.record(
        ActivityEvent::builder(Category::Query, "query.failed")
            .detail("error", json!({ "message": "near 'hunter2'" }))
            .build()?,
    )?;
    let e = &rec.list(&ActivityFilter::default())?.events[0];
    assert!(e.details.get("error").is_none());

    rec.record(
        ActivityEvent::builder(Category::Query, "query.failed")
            .detail("error", "near 'hunter2'")
            .error(
                &ErrorDetails {
                    code: Some("1105".into()),
                    message: "Unknown error near 'hunter2' at line 1".into(),
                },
                Some(SqlDialect::Mysql),
            )
            .build()?,
    )?;
    let e = &rec.list(&ActivityFilter::default())?.events[0];
    assert_eq!(
        e.details["error"],
        json!({ "code": "1105", "message": "Unknown error near ? at line 1" })
    );
    Ok(())
}

#[test]
fn connection_and_tunnel_errors_follow_their_source() -> TestResult {
    let (_, rec) = recorder()?;
    rec.set_text_level(TextLevel::None);
    let details = ConnectionDetails {
        engine: varve_core::Engine::Mariadb,
        server_version: None,
        host: "db".into(),
        port: None,
        database: None,
        username: None,
        via_ssh: true,
        tls_mode: None,
    };
    // `init_sql` errors come from the server and can echo it.
    rec.record(
        ActivityEvent::connection_failed(
            &details,
            &ErrorDetails {
                code: Some("1064 (42000)".into()),
                message: "... near 'IDENTIFIED BY 'hunter2'' at line 1".into(),
            },
        )
        .build()?,
    )?;
    let e = &rec.list(&ActivityFilter::default())?.events[0];
    assert_eq!(e.details["error"]["message"], ERROR_MESSAGE_HIDDEN);
    assert_eq!(e.details["error"]["code"], "1064 (42000)");

    // SSH messages cannot echo statements: kept, without credentials.
    rec.record(
        ActivityEvent::tunnel_failed(
            &TunnelDetails {
                ssh_host: "bastion".into(),
                ssh_port: 22,
                ssh_username: None,
                auth_method: None,
            },
            &ErrorDetails {
                code: None,
                message: "host key for bastion changed (ssh://u:hunter2@bastion)".into(),
            },
        )
        .build()?,
    )?;
    let e = &rec.list(&ActivityFilter::default())?.events[0];
    assert_eq!(
        e.details["error"]["message"],
        "host key for bastion changed (ssh://u:[redacted]@bastion)"
    );
    Ok(())
}

#[test]
fn statements_over_the_cap_keep_only_their_length() -> TestResult {
    let (_, rec) = recorder()?;
    let text = format!("SELECT '{}'", "x".repeat(MAX_REDACTED_BYTES));
    for level in [TextLevel::None, TextLevel::Normalized, TextLevel::Full] {
        rec.set_text_level(level);
        let e = record_query(&rec, &text, SqlDialect::Postgres)?;
        assert!(e.details.get("statement").is_none());
        assert_eq!(e.details["statement_bytes"], text.len());
        assert_eq!(e.details["statement_omitted"], "too_large");
    }
    Ok(())
}

#[test]
fn a_hidden_statement_hides_its_error_message_too() -> TestResult {
    let (_, rec) = recorder()?;
    // SQL nested in strings past the work budget: the statement is hidden
    // whole, so its secrets are unknown to the message filter.
    let mut pad = String::from("; SELECT 1; ");
    while pad.len() < 200_000 {
        pad.push_str("SET init_connect = (");
    }
    let cases = [
        (
            SqlDialect::Postgres,
            format!("ALTER ROLE a PASSWORD Zq9wSecret1{pad}"),
            "42601",
            "syntax error at or near \"Zq9wSecret1\"",
        ),
        (
            SqlDialect::Postgres,
            format!("ALTER ROLE a PASSWORD 'Zq9wSecret1' LOGIN{pad}"),
            "42710",
            "role \"a\" already exists; password Zq9wSecret1 not applied",
        ),
        (
            SqlDialect::Mysql,
            format!("ALTER USER u IDENTIFIED BY Zq9wSecret1{pad}"),
            "1064 (42000)",
            "You have an error in your SQL syntax; check the manual near 'Zq9wSecret1; SELECT 1' at line 1",
        ),
        (
            SqlDialect::Mysql,
            format!("CREATE USER u IDENTIFIED BY 'Zq9wSecret1'{pad}"),
            "1396 (HY000)",
            "Operation CREATE USER failed for 'u'@'%' (password Zq9wSecret1)",
        ),
        // Over the size cap: not read at all.
        (
            SqlDialect::Postgres,
            format!(
                "ALTER ROLE a PASSWORD 'Zq9wSecret1' {}",
                "x".repeat(MAX_REDACTED_BYTES)
            ),
            "42710",
            "role \"a\" already exists; password Zq9wSecret1 not applied",
        ),
    ];
    for (dialect, statement, code, message) in cases {
        for level in [TextLevel::Normalized, TextLevel::Full] {
            rec.set_text_level(level);
            let e = record_failed(&rec, dialect, &statement, code, message)?;
            let stored = e.details.to_string();
            assert!(!stored.contains("Zq9wSecret1"), "{level}: {stored}");
            assert_eq!(e.details["error"]["message"], ERROR_MESSAGE_WITHHELD);
            assert_eq!(e.details["error"]["code"], code);
        }
        rec.set_text_level(TextLevel::None);
        let e = record_failed(&rec, dialect, &statement, code, message)?;
        assert_eq!(e.details["error"]["message"], ERROR_MESSAGE_HIDDEN);
        // The same statement without the padding keeps its message.
        let short = statement
            .split(pad.as_str())
            .next()
            .unwrap_or_default()
            .split(" xxx")
            .next()
            .unwrap_or_default()
            .to_string();
        rec.set_text_level(TextLevel::Full);
        let e = record_failed(&rec, dialect, &short, code, message)?;
        assert_ne!(
            e.details["error"]["message"], ERROR_MESSAGE_WITHHELD,
            "{short}"
        );
        assert!(!e.details.to_string().contains("Zq9wSecret1"));
    }
    Ok(())
}

#[test]
fn normalized_messages_do_not_show_statement_literals() -> TestResult {
    let (_, rec) = recorder()?;
    let cases = [
        (
            SqlDialect::Postgres,
            "DO $$ BEGIN RAISE EXCEPTION 'customer % not found', 'Zq9wLiteral'; END $$",
            "P0001",
            "customer Zq9wLiteral not found",
            "customer ? not found",
        ),
        (
            SqlDialect::Mysql,
            "SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Zq9wLiteral'",
            "1644",
            "Zq9wLiteral",
            "?",
        ),
        (
            SqlDialect::Postgres,
            "SELECT * FROM orders WHERE id = 1234567",
            "P0001",
            "order 1234567 is locked",
            "order ? is locked",
        ),
        (
            SqlDialect::Redis,
            "SET session:1 Zq9wLiteral",
            "ERR",
            "ERR value Zq9wLiteral is not an integer",
            "ERR value ? is not an integer",
        ),
    ];
    for (dialect, statement, code, message, expected) in cases {
        rec.set_text_level(TextLevel::Normalized);
        let e = record_failed(&rec, dialect, statement, code, message)?;
        assert_eq!(e.details["error"]["message"], expected, "{statement}");
        // At full the statement shows the literal, so the message may too.
        rec.set_text_level(TextLevel::Full);
        let e = record_failed(&rec, dialect, statement, code, message)?;
        assert_eq!(e.details["error"]["message"], message, "{statement}");
    }
    Ok(())
}

#[test]
fn broken_quoting_hides_the_rest_of_a_script() -> TestResult {
    // A pg_dump-like script with an unescaped quote in one row: from that
    // statement on the lexer would read data as code and code as data.
    let mut script = String::new();
    for id in 0..40 {
        let name = if id == 21 { "D'Angelo" } else { "Bob" };
        script.push_str(&format!(
            "INSERT INTO users (id, name, password_hash) VALUES ({id}, '{name}', '$2b$12$Zq9wHash{id:04}');\n"
        ));
    }
    let (_, rec) = recorder()?;
    for level in [TextLevel::Normalized, TextLevel::Full] {
        rec.set_text_level(level);
        let e = record_failed(
            &rec,
            SqlDialect::Postgres,
            &script,
            "42601",
            "syntax error at or near \"Angelo\"",
        )?;
        let stored = e.details.to_string();
        assert!(!stored.contains("Zq9wHash"), "{level}: {stored}");
        let statement = e.details["statement"].as_str().unwrap_or_default();
        assert!(
            statement.starts_with("INSERT INTO ") && statement.contains("VALUES"),
            "{level}: {statement}"
        );
        assert_eq!(e.details["error"]["message"], ERROR_MESSAGE_WITHHELD);
    }
    rec.set_text_level(TextLevel::Full);
    let e = record_failed(&rec, SqlDialect::Postgres, &script, "42601", "x")?;
    let statement = e.details["statement"].as_str().unwrap_or_default();
    // The statements before the broken one are kept, redacted as usual.
    assert!(
        statement.contains("VALUES (20, 'Bob', '[redacted]');"),
        "{statement}"
    );
    // The hidden part keeps its line breaks around the marker.
    assert!(statement.trim_end().ends_with("[redacted]"), "{statement}");
    assert!(!statement.contains("(21,"), "{statement}");
    // Clean SQL with quotes in values is not affected.
    let clean = "INSERT INTO users (name) VALUES ('O''Brien'), (E'it\\'s'), ($$D'Angelo$$)";
    for level in [TextLevel::Normalized, TextLevel::Full] {
        assert_ne!(
            prepare_statement_text(clean, SqlDialect::Postgres, level).as_deref(),
            Some(REDACTED)
        );
    }
    assert_eq!(
        prepare_statement_text(clean, SqlDialect::Postgres, TextLevel::Full).as_deref(),
        Some(clean)
    );
    Ok(())
}

#[test]
fn normalized_messages_read_literals_as_the_server_does() -> TestResult {
    let (_, rec) = recorder()?;
    rec.set_text_level(TextLevel::Normalized);
    let pg = SqlDialect::Postgres;
    let my = SqlDialect::Mysql;
    for (dialect, statement, code, message, expected) in [
        // A doubled quote earlier in a dollar-quoted body does not shift
        // the reading of the rest of it.
        (
            pg,
            "DO $$ BEGIN RAISE NOTICE 'it''s'; RAISE EXCEPTION 'order % rejected', 'Zq9wLiteral'; END $$",
            "P0001",
            "order Zq9wLiteral rejected",
            "order ? rejected",
        ),
        (
            pg,
            "CREATE FUNCTION f() RETURNS void LANGUAGE plpgsql AS $f$ BEGIN RAISE NOTICE 'can''t'; EXECUTE 'SELECT 1'; RAISE EXCEPTION 'bad %', 'Zq9wLiteral'; END $f$",
            "P0001",
            "bad Zq9wLiteral",
            "bad ?",
        ),
        // Escape and Unicode strings are decoded before they are matched.
        (
            pg,
            "DO $$ BEGIN RAISE EXCEPTION E'bad\\tZq9wLiteral'; END $$",
            "P0001",
            "bad\tZq9wLiteral",
            "?",
        ),
        (
            pg,
            "SELECT E'\\x5Aq9wLiteral'::int",
            "P0001",
            "value Zq9wLiteral is bad",
            "value ? is bad",
        ),
        (
            pg,
            "SELECT U&'\\005Aq9wLiteral'::int",
            "P0001",
            "value Zq9wLiteral is bad",
            "value ? is bad",
        ),
        // A literal that is a word of the server's message stays.
        (
            pg,
            "INSERT INTO t (kind, name) VALUES ('null', NULL)",
            "23502",
            "null value in column \"name\" of relation \"t\" violates not-null constraint",
            "null value in column ? of relation ? violates not-null constraint",
        ),
        (
            my,
            "INSERT INTO t (a) VALUES ('Data')",
            "1406",
            "Data too long for column 'a' at row 1",
            "Data too long for column ? at row 1",
        ),
    ] {
        let e = record_failed(&rec, dialect, statement, code, message)?;
        assert_eq!(e.details["error"]["message"], expected, "{statement}");
    }
    Ok(())
}

#[test]
fn comment_rules_hold_round_eleven_forms() {
    let my = SqlDialect::Mysql;
    for (text, dialect) in [
        ("SELECT 1 -- mysql appdb -pZq9wSecret1", my),
        (
            "SELECT 1 /* mysql -h db -u admin \\\n   -pzqsecretword appdb */",
            my,
        ),
        (
            "SELECT 1 -- docker exec db sh -c \"mysql -uroot -pZq9wSecret1 -e 'show databases'\"",
            my,
        ),
        (
            "SELECT 1 -- ssh db1 'mysqldump -uroot -pzqsecretword appdb' > dump.sql",
            my,
        ),
        (
            "SELECT 1 -- n=$(mysql -uroot -pzqsecretword -N -e 'select 1')",
            my,
        ),
        (
            "SELECT 1 -- run `mysql -uroot -pzqsecretword appdb` first",
            my,
        ),
        ("SELECT 1 -- cd /tmp&&mysql -uroot -pzqsecretword", my),
        (
            "SELECT 1 -- C:\\mysql\\bin\\mysql.exe -uroot -pzqsecretword",
            my,
        ),
        ("SELECT 1 -- SET PASSWORD FOR app TO 'zqsecretword'", my),
        (
            "SELECT 1 -- SET PASSWORD FOR app = ENCRYPT('zqsecretword')",
            my,
        ),
        (
            "SELECT 1 -- SET PASSWORD FOR app = PASSWORD(CONCAT('zqsecret', 'word'))",
            my,
        ),
        (
            "# redis-cli -h cache -a zqsecretword\nPING",
            SqlDialect::Redis,
        ),
        ("# mysql -uroot -pzqsecretword\nPING", SqlDialect::Redis),
        ("GET k # redis-cli -a zqsecretword", SqlDialect::Redis),
    ] {
        let out = redact_secrets(text, dialect);
        assert!(
            !out.contains("zqsecret") && !out.contains("Zq9wSecret1"),
            "{text} => {out}"
        );
        assert_eq!(redact_secrets(&out, dialect), out, "{text}");
    }
    for (text, dialect) in [
        ("SELECT 1 -- set password for the app user = later", my),
        ("SELECT 1 -- MySQL docs -performance tips", my),
        ("SELECT 1 -- psql -h db -W appdb", SqlDialect::Postgres),
        (
            "SELECT jsonb_build_object('password length', 12, 'password policy', 'strong', 'password changed at', '2024-01-01')",
            SqlDialect::Postgres,
        ),
        ("SELECT 'a'AS x, 'b'FROM t", SqlDialect::Postgres),
    ] {
        assert_eq!(redact_secrets(text, dialect), text);
        assert_eq!(
            prepare_statement_text(text, dialect, TextLevel::Full).as_deref(),
            Some(text)
        );
    }
    // Quoting that is genuinely broken still fails closed.
    for text in [
        "SELECT 'D'Or' , 'Zq9wSecret1'",
        "INSERT INTO t VALUES ('5 o'9, 'Zq9wSecret1')",
    ] {
        assert_eq!(
            prepare_statement_text(text, SqlDialect::Postgres, TextLevel::Full).as_deref(),
            Some(REDACTED),
            "{text}"
        );
    }
}

#[test]
fn round_twelve_patterns() -> TestResult {
    let (_, rec) = recorder()?;
    rec.set_text_level(TextLevel::Normalized);
    let pg = SqlDialect::Postgres;
    let my = SqlDialect::Mysql;
    // Template words stay in server messages; a message the statement
    // raised itself loses every literal of the statement.
    for (dialect, statement, code, message, expected) in [
        (
            pg,
            "SELECT 1 / 0 WHERE 'division' = 'division'",
            "22012",
            "division by zero",
            "division by zero",
        ),
        (
            my,
            "INSERT INTO t (a) VALUES ('truncated')",
            "1265 (01000)",
            "Data truncated for column 'a' at row 1",
            "Data truncated for column ? at row 1",
        ),
        (
            pg,
            "DO $$ BEGIN RAISE EXCEPTION 'customer % blocked', 'Grant'; END $$",
            "P0001",
            "customer Grant blocked",
            "customer ? blocked",
        ),
        (
            my,
            "SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'Cross'",
            "1644 (45000)",
            "customer Cross blocked",
            "customer ? blocked",
        ),
        (
            pg,
            "DO $$ BEGIN RAISE EXCEPTION U&'!005Aq9wLiteral' UESCAPE '!'; END $$",
            "P0001",
            "Zq9wLiteral",
            "?",
        ),
    ] {
        let e = record_failed(&rec, dialect, statement, code, message)?;
        assert_eq!(e.details["error"]["message"], expected, "{statement}");
    }

    // pg_dump data rows are data: the rest of the dump keeps its normal
    // redaction, and the secret columns of the COPY column list go.
    let dump = "COPY public.users (id, name, password) FROM stdin;\n1\tO'Brien\t$2b$12$Zq9wHashOne\n2\tit's\t\\N\n\\.\n\nALTER ROLE app PASSWORD 'Zq9wSecret1';\n";
    assert_eq!(
        prepare_statement_text(dump, pg, TextLevel::Full).as_deref(),
        Some(
            "COPY public.users (id, name, password) FROM stdin;\n1\tO'Brien\t[redacted]\n2\tit's\t\\N\n\\.\n\nALTER ROLE app PASSWORD '[redacted]';\n"
        )
    );
    let normalized = prepare_statement_text(dump, pg, TextLevel::Normalized).unwrap_or_default();
    assert!(
        !normalized.contains("Zq9w") && !normalized.contains("Brien"),
        "{normalized}"
    );
    // A `stdin;` inside a literal starts no data section.
    let fake = "SELECT 'COPY t FROM stdin;\n'; ALTER ROLE app PASSWORD 'Zq9wSecret1'";
    let full = prepare_statement_text(fake, pg, TextLevel::Full).unwrap_or_default();
    assert!(!full.contains("Zq9w"), "{full}");

    // redis-cli arguments, client environment variables, mysqladmin.
    for (text, dialect) in [
        ("SELECT 1 -- redis-cli -h cache AUTH app zqsecretword", my),
        (
            "SELECT 1 -- redis-cli -h cache CONFIG SET requirepass zqsecretword",
            my,
        ),
        (
            "SELECT 1 -- REDISCLI_AUTH=zqsecretword redis-cli -h cache",
            my,
        ),
        ("SELECT 1 -- mysqladmin -uroot password zqsecretword", my),
        (
            "SELECT 1 -- sh -c \"mysql -uroot -p\\\"zqsecret word\\\" appdb\"",
            my,
        ),
        ("SELECT 1 -- bash -c $'mysql appdb -pzqsecretword'", my),
        (
            "SELECT 1 -- & \"C:\\Program Files\\MySQL\\MySQL Server 8.0\\bin\\mysql.exe\" appdb -pzqsecretword",
            my,
        ),
        (
            "SELECT 1 -- C:\\xampp\\mysql\\bin\\mysql.exe appdb -pzqsecretword",
            my,
        ),
        (
            "SELECT 1 -- 0 3 * * * mysqldump appdb -pzqsecretword > /b/x.sql",
            my,
        ),
        ("SELECT 1 -- $ mysql appdb -pzqsecretword", my),
        ("SELECT 1 -- exec 2>&1 mysql appdb -pzqsecretword", my),
        (
            "CREATE FUNCTION f() RETURNS int AS $$\n-- mysqldump appdb -pzqsecretword > x.sql\nSELECT 1 $$ LANGUAGE sql",
            pg,
        ),
    ] {
        let out = redact_secrets(text, dialect);
        assert!(!out.contains("zqsecret"), "{text} => {out}");
    }
    for text in [
        "SELECT 1 -- Mysql -prefixed tables are legacy",
        "SELECT 'a'AS\"b\"",
    ] {
        assert_eq!(redact_secrets(text, pg), text);
    }
    Ok(())
}

#[test]
fn statements_that_raise_their_own_errors_hide_every_literal() -> TestResult {
    let (_, rec) = recorder()?;
    rec.set_text_level(TextLevel::Normalized);
    for (dialect, statement, code, message, expected) in [
        (
            SqlDialect::Postgres,
            "DO $$ BEGIN RAISE EXCEPTION 'customer % blocked', 'Grant' USING ERRCODE = '42501'; END $$",
            "42501",
            "customer Grant blocked",
            "customer ? blocked",
        ),
        (
            SqlDialect::Mysql,
            "SIGNAL SQLSTATE '23000' SET MYSQL_ERRNO = 1062, MESSAGE_TEXT = CONCAT('user ', 'admin', ' exists')",
            "1062 (23000)",
            "user admin exists",
            // Every literal of the statement: the message is built from them.
            "? ? ?",
        ),
        // Without RAISE or SIGNAL a template word stays.
        (
            SqlDialect::Postgres,
            "SELECT 1 / 0 WHERE 'division' <> ''",
            "22012",
            "division by zero",
            "division by zero",
        ),
    ] {
        let e = record_failed(&rec, dialect, statement, code, message)?;
        assert_eq!(e.details["error"]["message"], expected, "{statement}");
    }
    Ok(())
}

#[test]
fn round_thirteen_patterns() {
    let pg = SqlDialect::Postgres;
    let my = SqlDialect::Mysql;
    for (text, dialect) in [
        // Comments in the body's own language.
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\n# mysql -uroot -pzqsecretword appdb\nreturn 1\n$$",
            pg,
        ),
        (
            "CREATE FUNCTION f() RETURNS int AS $$\n// mysql -uroot -pzqsecretword appdb\nreturn 1;\n$$ LANGUAGE plv8",
            pg,
        ),
        // Prompts, cron with a user field, bullets.
        ("SELECT 1 -- root@db1:~$ mysqldump appdb -pzqsecretword", my),
        (
            "SELECT 1 -- [root@db1 ~]# mysqldump appdb -pzqsecretword",
            my,
        ),
        (
            "SELECT 1 -- */5 * * * * root mysqldump appdb -pzqsecretword > /b/x.sql",
            my,
        ),
        ("SELECT 1 -- PS> mysqldump appdb -pzqsecretword", my),
        ("SELECT 1 -- mysql> \\! mysqldump appdb -pzqsecretword", my),
        ("SELECT 1 -- - mysqldump appdb -pzqsecretword", my),
        // Shell keywords on a shell line.
        (
            "SELECT 1 -- for db in a b; do mysqldump $db -pzqsecretword > $db.sql; done",
            my,
        ),
        // redis-cli option table and commands after unknown options.
        (
            "SELECT 1 -- redis-cli --askpass CONFIG SET requirepass zqsecretword",
            my,
        ),
        (
            "SELECT 1 -- redis-cli -t 5 CONFIG SET requirepass zqsecretword",
            my,
        ),
        (
            "SELECT 1 -- redis-cli --unknown-opt 1 ACL SETUSER app on >zqsecretword",
            my,
        ),
        ("SELECT 1 -- mysqladmin password 'zqsecretword'", my),
    ] {
        let out = redact_secrets(text, dialect);
        assert!(!out.contains("zqsecret"), "{text} => {out}");
    }
    for (text, dialect) in [
        ("SELECT 1 -- do not use mysql -performance_schema here", my),
        ("SELECT 1 -- then mysql -prompt changes", my),
        ("SELECT 1 -- note ! mysql -preloads", my),
        ("SELECT 1 -- this time mysql -prefers hash joins", my),
        ("SELECT 1 -- redis-cli supports AUTH since forever", my),
        ("SELECT 1 -- mysqladmin password reset is documented", my),
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE plpython3u AS $$\nx = {'host': 'db'}\nreturn 1\n$$",
            pg,
        ),
    ] {
        assert_eq!(redact_secrets(text, dialect), text);
    }
}

#[test]
fn round_fourteen_patterns() -> TestResult {
    let pg = SqlDialect::Postgres;
    // Framework credential columns, and times and counts about secrets.
    assert_eq!(
        redact_secrets(
            "INSERT INTO users (id, remember_token, confirmation_token, two_factor_recovery_codes) VALUES (1, 'zqa', 'zqb', 'zqc')",
            pg
        ),
        "INSERT INTO users (id, remember_token, confirmation_token, two_factor_recovery_codes) VALUES (1, '[redacted]', '[redacted]', '[redacted]')"
    );
    for text in [
        "UPDATE users SET password_changed_at = '2024-01-01', token_count = 3, max_token = 4096",
        "COPY t (id, password_changed_at, token_count) FROM stdin;\n1\t2024-01-01\t3\n\\.\n",
    ] {
        assert_eq!(
            prepare_statement_text(text, pg, TextLevel::Full).as_deref(),
            Some(text)
        );
    }
    // COPY data values are statement literals for the message.
    let (_, rec) = recorder()?;
    rec.set_text_level(TextLevel::Normalized);
    let e = record_failed(
        &rec,
        pg,
        "COPY public.users (id, email, note) FROM stdin;\nzqbadid\tbob@example.com\tnote\n\\.\n",
        "22P02",
        "invalid input syntax for type integer: zqbadid",
    )?;
    assert_eq!(
        e.details["error"]["message"],
        "invalid input syntax for type integer: ?"
    );
    Ok(())
}

#[test]
fn round_fifteen_patterns() -> TestResult {
    let pg = SqlDialect::Postgres;
    // A psql meta-command line ends a statement: the COPY or INSERT after
    // it is read from a fresh start.
    for text in [
        "\\connect appdb\nCOPY public.users (id, email, password_hash) FROM stdin;\n1\tb@x\tZq9wHash\n\\.\n",
        "\\restrict aBcDeF123\nCOPY public.users (id, password_hash) FROM stdin;\n1\tZq9wHash\n\\.\n",
        "\\set ON_ERROR_STOP on\nINSERT INTO users (name, password) VALUES ('bob', 'Zq9wHash');",
        "\\copy users (id, password) from pstdin\nINSERT INTO users (name, password) VALUES ('bob', 'Zq9wHash');",
        "SELECT 1\nCOPY public.users (id, password_hash) FROM stdin;\n1\tZq9wHash\n\\.\n",
    ] {
        let full = prepare_statement_text(text, pg, TextLevel::Full).unwrap_or_default();
        assert!(!full.contains("Zq9w"), "{text} => {full}");
    }
    // Double-quoted strings of a non-SQL body are literals for the message.
    let (_, rec) = recorder()?;
    rec.set_text_level(TextLevel::Normalized);
    for (statement, message) in [
        (
            "DO LANGUAGE plpython3u $$ plpy.error(\"zqliteral\") $$",
            "zqliteral",
        ),
        (
            "DO LANGUAGE plv8 $$ plv8.elog(ERROR, \"zqliteral\"); $$",
            "zqliteral",
        ),
        (
            "DO LANGUAGE plperl $$ die \"zqliteral\\n\"; $$",
            "zqliteral",
        ),
    ] {
        let e = record_failed(&rec, pg, statement, "XX000", message)?;
        assert_eq!(e.details["error"]["message"], "?", "{statement}");
    }
    Ok(())
}

#[test]
fn failed_statements_with_many_secrets_record_fast() -> TestResult {
    let (_, rec) = recorder()?;
    let statement = (0..32_000)
        .map(|i| format!("u{i} IDENTIFIED BY 'pw{i:06}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let statement = format!("CREATE USER {statement}");
    assert!(statement.len() < MAX_REDACTED_BYTES);
    let echo = format!(
        "You have an error near '{}' at line 1",
        &statement[60_000..120_000]
    );
    for message in ["boom".to_string(), echo] {
        rec.set_text_level(TextLevel::Full);
        let start = std::time::Instant::now();
        rec.record(
            ActivityEvent::query_failed(
                &statement,
                SqlDialect::Mysql,
                "ddl",
                &ErrorDetails {
                    code: Some("1105".into()),
                    message,
                },
                1,
            )
            .connection("c-many")
            .build()?,
        )?;
        let took = start.elapsed();
        let bound = if cfg!(debug_assertions) { 30 } else { 2 };
        assert!(took.as_secs() < bound, "record took {took:?}");
        let e = rec.query_history("c-many", 1)?.remove(0);
        assert!(!serde_json::to_string(&e)?.contains("pw000123"));
    }
    Ok(())
}

#[test]
fn subscribers_are_called_and_a_panic_does_not_lose_the_write() -> TestResult {
    let (store, rec) = recorder()?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    rec.on_recorded(|_| panic!("subscriber failure"));
    let seen2 = Arc::clone(&seen);
    let sub = rec.on_recorded(move |e| {
        if let Ok(mut s) = seen2.lock() {
            s.push((e.action.clone(), e.details.get("statement").cloned()));
        }
    });
    let calls2 = Arc::clone(&calls);
    rec.on_recorded(move |_| {
        calls2.fetch_add(1, Ordering::SeqCst);
    });

    rec.record(
        ActivityEvent::query_executed(
            "AUTH Tr0ub4dor",
            SqlDialect::Redis,
            "AUTH",
            RowCounts::default(),
            1,
        )
        .build()?,
    )?;
    assert_eq!(count_rows(&store)?, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    {
        let seen = seen.lock().map_err(|_| "poisoned")?;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "query.executed");
        // Subscribers see the prepared statement, never the raw text.
        assert_eq!(seen[0].1, Some(json!("AUTH ?")));
    }

    assert!(rec.unsubscribe(sub));
    assert!(!rec.unsubscribe(sub));
    rec.record(ActivityEvent::app_started("1").build()?)?;
    assert_eq!(seen.lock().map_err(|_| "poisoned")?.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(count_rows(&store)?, 2);
    Ok(())
}

#[test]
fn subscriber_may_record_without_deadlock() -> TestResult {
    let (store, rec) = recorder()?;
    let rec = Arc::new(rec);
    let weak = Arc::downgrade(&rec);
    rec.on_recorded(move |e| {
        if e.action == "app.started"
            && let Some(rec) = weak.upgrade()
            && let Ok(follow_up) = ActivityEvent::builder(Category::App, "app.follow_up").build()
        {
            let _ = rec.record(follow_up);
        }
    });
    rec.record(ActivityEvent::app_started("1").build()?)?;
    assert_eq!(count_rows(&store)?, 2);
    Ok(())
}

fn seed(rec: &Recorder) -> TestResult {
    let mut events = Vec::new();
    for i in 0..10 {
        events.push(
            ActivityEvent::builder(Category::Query, "query.executed")
                .occurred_at(NOON - i * 1000)
                .connection(if i % 2 == 0 { "c1" } else { "c2" })
                .detail("kind", if i == 3 { "drop" } else { "select" })
                .build()?,
        );
    }
    events.push(
        ActivityEvent::builder(Category::Query, "query.failed")
            .outcome(Outcome::Failure)
            .occurred_at(NOON - 20_000)
            .connection("c1")
            .target("public.orders_100%")
            .build()?,
    );
    events.push(
        ActivityEvent::builder(Category::Connection, "connection.opened")
            .occurred_at(NOON - DAY)
            .connection("c1")
            .workspace("team")
            .build()?,
    );
    events.push(
        ActivityEvent::builder(Category::Tool, "transfer.started")
            .occurred_at(NOON - 2 * DAY)
            .build()?,
    );
    rec.record_batch(events)?;
    Ok(())
}

#[test]
fn list_filters() -> TestResult {
    let (_, rec) = recorder()?;
    seed(&rec)?;
    let count =
        |f: ActivityFilter| -> Result<usize, ActivityError> { Ok(rec.list(&f)?.events.len()) };

    assert_eq!(count(ActivityFilter::default())?, 13);
    assert_eq!(
        count(ActivityFilter {
            connection_id: Some("c1".into()),
            ..Default::default()
        })?,
        7
    );
    assert_eq!(
        count(ActivityFilter {
            categories: vec![Category::Connection, Category::Tool],
            ..Default::default()
        })?,
        2
    );
    assert_eq!(
        count(ActivityFilter {
            actions: vec!["query.failed".into(), "transfer.*".into()],
            ..Default::default()
        })?,
        2
    );
    assert_eq!(
        count(ActivityFilter {
            outcomes: vec![Outcome::Failure],
            ..Default::default()
        })?,
        1
    );
    assert_eq!(
        count(ActivityFilter {
            workspace_id: Some("team".into()),
            ..Default::default()
        })?,
        1
    );
    assert_eq!(
        count(ActivityFilter {
            from: Some(NOON - DAY),
            to: Some(NOON),
            ..Default::default()
        })?,
        11
    );
    // Text search over action, target and details, with LIKE wildcards
    // taken literally.
    assert_eq!(
        count(ActivityFilter {
            text: Some("drop".into()),
            ..Default::default()
        })?,
        1
    );
    assert_eq!(
        count(ActivityFilter {
            text: Some("orders_100%".into()),
            ..Default::default()
        })?,
        1
    );
    assert_eq!(
        count(ActivityFilter {
            text: Some("100%%".into()),
            ..Default::default()
        })?,
        0
    );
    assert_eq!(
        count(ActivityFilter {
            text: Some("TRANSFER".into()),
            ..Default::default()
        })?,
        1
    );
    assert_eq!(
        count(ActivityFilter {
            actor_id: Some("user-1".into()),
            device_id: Some("device-1".into()),
            ..Default::default()
        })?,
        13
    );
    Ok(())
}

#[test]
fn pages_are_newest_first_with_a_cursor() -> TestResult {
    let (_, rec) = recorder()?;
    seed(&rec)?;
    // Two events at the same millisecond are ordered by id.
    rec.record(event_at("app.tie", Category::App, NOON)?)?;

    let mut filter = ActivityFilter {
        limit: 5,
        ..Default::default()
    };
    let mut seen = Vec::new();
    loop {
        let page = rec.list(&filter)?;
        assert!(page.events.len() <= 5);
        seen.extend(page.events.iter().map(|e| (e.occurred_at, e.id)));
        match page.next_cursor {
            Some(cursor) => filter.before_id = Some(cursor),
            None => break,
        }
    }
    assert_eq!(seen.len(), 14);
    let mut sorted = seen.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(seen, sorted);

    let unknown = ActivityFilter {
        before_id: Some("no-such-id".into()),
        ..Default::default()
    };
    assert!(matches!(
        rec.list(&unknown),
        Err(ActivityError::UnknownCursor(_))
    ));
    Ok(())
}

#[test]
fn query_history_returns_query_events_of_one_connection() -> TestResult {
    let (_, rec) = recorder()?;
    seed(&rec)?;
    let history = rec.query_history("c1", 100)?;
    assert_eq!(history.len(), 6);
    assert!(history.iter().all(|e| e.category == Category::Query
        && e.action.starts_with("query.")
        && e.connection_id.as_deref() == Some("c1")));
    assert_eq!(rec.query_history("c1", 2)?.len(), 2);
    Ok(())
}

#[test]
fn counts_by_day() -> TestResult {
    let (_, rec) = recorder()?;
    seed(&rec)?;
    let days = rec.count_by_day(&ActivityFilter::default())?;
    let summary: Vec<(&str, u64)> = days.iter().map(|d| (d.day.as_str(), d.count)).collect();
    assert_eq!(
        summary,
        [("2026-10-17", 1), ("2026-10-18", 1), ("2026-10-19", 11)]
    );
    assert_eq!(days[2].day_start_ms, NOON - 12 * 3_600_000);

    let connects = rec.count_by_day(&ActivityFilter {
        actions: vec!["connection.opened".into()],
        ..Default::default()
    })?;
    assert_eq!(connects.len(), 1);
    assert_eq!(connects[0].day, "2026-10-18");

    // At UTC-13 noon UTC is still the previous day.
    let shifted = rec.count_by_day_with_offset(&ActivityFilter::default(), -13 * 60)?;
    assert_eq!(shifted.last().map(|d| d.day.as_str()), Some("2026-10-18"));
    Ok(())
}

#[test]
fn retention_deletes_old_events_and_records_it() -> TestResult {
    let (store, rec) = recorder()?;
    seed(&rec)?;
    assert_eq!(rec.apply_retention_at(Retention::Forever, NOON)?, 0);
    assert!(matches!(
        rec.apply_retention_at(Retention::Days(0), NOON),
        Err(ActivityError::InvalidRetention(_))
    ));

    let removed = rec.apply_retention_at(Retention::Days(1), NOON + 1)?;
    assert_eq!(removed, 2);
    assert_eq!(count_rows(&store)?, 12);
    let page = rec.list(&ActivityFilter {
        actions: vec![ACTION_PRUNED.into()],
        ..Default::default()
    })?;
    assert_eq!(page.events.len(), 1);
    let pruned = &page.events[0];
    assert_eq!(pruned.category, Category::App);
    assert_eq!(pruned.details["removed"], 2);
    assert_eq!(pruned.details["retention_days"], 1);

    // Nothing to remove: no event.
    assert_eq!(rec.apply_retention_at(Retention::Days(1), NOON + 1)?, 0);
    assert_eq!(count_rows(&store)?, 12);
    Ok(())
}

#[test]
fn clear_one_connection_or_everything() -> TestResult {
    let (store, rec) = recorder()?;
    seed(&rec)?;
    let notified = Arc::new(Mutex::new(Vec::new()));
    let n2 = Arc::clone(&notified);
    rec.on_recorded(move |e| {
        if let Ok(mut n) = n2.lock() {
            n.push(e.details.clone());
        }
    });

    assert_eq!(rec.clear(Some("c2"))?, 5);
    let c2 = rec.list(&ActivityFilter {
        connection_id: Some("c2".into()),
        ..Default::default()
    })?;
    assert_eq!(c2.events.len(), 1);
    assert_eq!(c2.events[0].action, ACTION_CLEARED);
    assert_eq!(c2.events[0].details["scope"], "connection");
    assert_eq!(c2.events[0].details["removed"], 5);

    let total_before = count_rows(&store)?;
    assert_eq!(rec.clear(None)?, u64::try_from(total_before)?);
    let all = rec.list(&ActivityFilter::default())?;
    assert_eq!(all.events.len(), 1);
    assert_eq!(all.events[0].details["scope"], "all");

    let notified = notified.lock().map_err(|_| "poisoned")?;
    assert_eq!(notified.len(), 2);
    assert_eq!(notified[0]["removed"], 5);
    Ok(())
}

#[test]
fn exports_csv_and_jsonl_over_many_pages() -> TestResult {
    let (_, rec) = recorder()?;
    let mut events = Vec::new();
    for i in 0..1203 {
        events.push(
            ActivityEvent::builder(Category::Query, "query.executed")
                .occurred_at(NOON - i)
                .connection("c1")
                .target(if i == 0 {
                    "=HYPERLINK(\"x\")"
                } else {
                    "t, \"q\""
                })
                .detail("n", i)
                .build()?,
        );
    }
    events.push(event_at("app.started", Category::App, NOON)?);
    rec.record_batch(events)?;

    let filter = ActivityFilter {
        connection_id: Some("c1".into()),
        limit: 3,
        ..Default::default()
    };
    let mut jsonl = Vec::new();
    assert_eq!(rec.export_jsonl(&filter, &mut jsonl)?, 1203);
    let text = String::from_utf8(jsonl)?;
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1203);
    let first: ActivityEvent = serde_json::from_str(lines[0])?;
    let last: ActivityEvent = serde_json::from_str(lines[1202])?;
    assert_eq!(first.details["n"], 0);
    assert_eq!(last.details["n"], 1202);

    let mut csv = Vec::new();
    assert_eq!(rec.export_csv(&filter, &mut csv)?, 1203);
    let csv = String::from_utf8(csv)?;
    let rows: Vec<&str> = csv.split("\r\n").filter(|l| !l.is_empty()).collect();
    assert_eq!(rows.len(), 1204);
    assert!(rows[0].starts_with("id,occurred_at,occurred_at_utc,"));
    assert!(rows[1].contains(",2026-10-19T12:00:00.000Z,"));
    assert!(rows[1].contains(",\"'=HYPERLINK(\"\"x\"\")\","));
    assert!(rows[2].contains(",\"t, \"\"q\"\"\","));

    // Starting below a cursor.
    let page = rec.list(&ActivityFilter {
        limit: 3,
        ..filter.clone()
    })?;
    let mut rest = Vec::new();
    let from_cursor = ActivityFilter {
        before_id: page.next_cursor,
        ..filter
    };
    assert_eq!(rec.export_jsonl(&from_cursor, &mut rest)?, 1200);
    Ok(())
}

#[test]
fn list_queries_use_the_activity_indexes() -> TestResult {
    let (store, _) = recorder()?;
    let filters = [
        ActivityFilter::default(),
        ActivityFilter {
            connection_id: Some("c1".into()),
            ..Default::default()
        },
        ActivityFilter {
            categories: vec![Category::Query],
            ..Default::default()
        },
        ActivityFilter {
            from: Some(0),
            to: Some(NOON),
            text: Some("x".into()),
            ..Default::default()
        },
    ];
    for filter in filters {
        let cursor = (NOON, "id".to_string());
        let (sql, params) = page_sql(&filter, Some(&cursor), 10);
        let plan: Vec<String> = store.with_conn(|c| {
            let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params), |r| {
                    r.get::<_, String>(3)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let plan = plan.join(" | ");
        assert!(
            plan.contains("USING INDEX core_activity_"),
            "no index for {filter:?}: {plan}"
        );
        assert!(
            !plan.contains("USE TEMP B-TREE FOR ORDER BY"),
            "full sort: {plan}"
        );
    }
    Ok(())
}

/// A redactor that fails never takes the app down: the event is still
/// recorded, the part it failed on is not stored (fail closed), and the
/// event says so.
#[test]
fn a_failing_redactor_stores_nothing_of_that_part() -> TestResult {
    use crate::recorder::FAILING_REDACTOR;
    let (_store, rec) = recorder()?;
    for level in [TextLevel::Normalized, TextLevel::Full] {
        rec.set_text_level(level);
        for part in ["details", "target", "statement", "error message"] {
            let connection = format!("{part}-{level}");
            FAILING_REDACTOR.with(|p| p.set(Some(part)));
            let result = rec.record(
                ActivityEvent::query_failed(
                    "ALTER ROLE app PASSWORD 'Tr0ub4dor'",
                    SqlDialect::Postgres,
                    "x",
                    &ErrorDetails {
                        code: Some("XX000".into()),
                        message: "role check failed for Tr0ub4dor".into(),
                    },
                    1,
                )
                .connection(connection.as_str())
                .target("redis://:Tr0ub4dor@cache")
                .detail("note", "password=Tr0ub4dor")
                .build()?,
            );
            FAILING_REDACTOR.with(|p| p.set(None));
            result?;
            let filter = ActivityFilter {
                connection_id: Some(connection.clone()),
                ..ActivityFilter::default()
            };
            let events = rec.list(&filter)?.events;
            assert_eq!(events.len(), 1, "{part} at {level}");
            let event = &events[0];
            let all = serde_json::to_string(event)?;
            assert!(!all.contains("Tr0ub4dor"), "{part} at {level}: {all}");
            let details = &event.details;
            match part {
                "details" => {
                    assert_eq!(details["details_omitted"], REDACTION_FAILED, "{all}");
                    assert!(details.get("note").is_none(), "{all}");
                }
                "target" => assert_eq!(event.target, None, "{all}"),
                "statement" => {
                    assert_eq!(details["statement_omitted"], REDACTION_FAILED, "{all}");
                    assert!(details.get("statement").is_none(), "{all}");
                    // Without the statement's secrets the message is
                    // withheld too.
                    assert_eq!(details["error"]["message"], ERROR_MESSAGE_WITHHELD, "{all}");
                }
                _ => {
                    assert_eq!(details["error"]["message"], ERROR_MESSAGE_WITHHELD, "{all}");
                    assert!(details.get("statement").is_some(), "{all}");
                }
            }
        }
    }
    Ok(())
}

/// The public redaction entry points never panic into their callers
/// (`crates/live-stats` redacts other sessions' query text with
/// `redact_secrets`): on a failure they return nothing of the text.
#[test]
fn public_redactors_fail_closed() {
    use crate::recorder::FAILING_REDACTOR;
    let text = "ALTER ROLE app PASSWORD 'Tr0ub4dor'";
    FAILING_REDACTOR.with(|p| p.set(Some("redact_secrets")));
    let redacted = redact_secrets(text, SqlDialect::Postgres);
    FAILING_REDACTOR.with(|p| p.set(Some("prepare_statement_text")));
    let prepared = prepare_statement_text(text, SqlDialect::Postgres, TextLevel::Full);
    FAILING_REDACTOR.with(|p| p.set(Some("prepare_error_message")));
    let message = prepare_error_message(
        "role check failed for Tr0ub4dor",
        Some("XX000"),
        Some(SqlDialect::Postgres),
        TextLevel::Full,
    );
    FAILING_REDACTOR.with(|p| p.set(None));
    assert_eq!(redacted, REDACTED);
    assert_eq!(prepared, None);
    assert_eq!(message, ERROR_MESSAGE_WITHHELD);
    // Without a failure the same calls redact as usual.
    assert_eq!(
        redact_secrets(text, SqlDialect::Postgres),
        "ALTER ROLE app PASSWORD '[redacted]'"
    );
}
