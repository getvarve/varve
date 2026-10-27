//! Integration tests against the dev server matrix (`dev/docker-compose.yml`):
//! Redis 6.2, Redis 8.10 and Valkey 9.
//!
//! Run with `cargo test -p varve-driver-redis -- --include-ignored`.
//! Endpoints come from `VARVE_REDIS_62`, `VARVE_REDIS_810` and
//! `VARVE_VALKEY_9` (`host:port`), the password from `VARVE_REDIS_PASSWORD`.
//! Tests use logical database 15 and remove their keys.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, DriverError, ErrorKind, ExecOptions, Session, StatementResult, TlsMode,
    Value,
};
use varve_driver_redis::{
    CommandRisk, KeyType, RedisDriver, RedisSession, ServerFlavor, ValueData, ValuePage,
};
use varve_secrets::Secret;

const TEST_DB: u16 = 15;

/// Tests share database 15, so they run one at a time.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Server {
    label: String,
    valkey: bool,
    major: u32,
    minor: Option<u32>,
    host: String,
    port: u16,
}

impl Server {
    /// Redis 8 ships JSON in the core server; older Redis and Valkey need a module.
    fn has_builtin_json(&self) -> bool {
        !self.valkey && self.major >= 8
    }
}

/// Servers to test. `VARVE_REDIS_SERVERS` takes a comma separated list of
/// `<redis|valkey>-<major>[.<minor>]@host:port`, for example
/// `redis-7.2@127.0.0.1:63072,valkey-8@127.0.0.1:63180`. Without it the
/// oldest Redis, the newest Redis and the newest Valkey run, with the single
/// endpoint overrides `VARVE_REDIS_62`, `VARVE_REDIS_810` and `VARVE_VALKEY_9`.
fn servers() -> Vec<Server> {
    let list = std::env::var("VARVE_REDIS_SERVERS").unwrap_or_else(|_| {
        let endpoint =
            |var: &str, default: &str| std::env::var(var).unwrap_or_else(|_| default.to_string());
        format!(
            "redis-6.2@{},redis-8.10@{},valkey-9@{}",
            endpoint("VARVE_REDIS_62", "127.0.0.1:63062"),
            endpoint("VARVE_REDIS_810", "127.0.0.1:63081"),
            endpoint("VARVE_VALKEY_9", "127.0.0.1:63190"),
        )
    });
    list.split(',')
        .map(|item| {
            let (label, endpoint) = item
                .trim()
                .split_once('@')
                .unwrap_or_else(|| panic!("`{item}` must look like redis-7.2@host:port"));
            let (flavor, version) = label
                .split_once('-')
                .unwrap_or_else(|| panic!("`{label}` must look like redis-7.2"));
            let mut numbers = version.split('.');
            let major = numbers
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("`{label}` has no major version"));
            let minor = numbers.next().and_then(|n| n.parse().ok());
            let (host, port) = endpoint
                .rsplit_once(':')
                .unwrap_or_else(|| panic!("`{endpoint}` must be host:port"));
            Server {
                label: label.to_string(),
                valkey: match flavor {
                    "redis" => false,
                    "valkey" => true,
                    other => panic!("unknown server flavor `{other}`"),
                },
                major,
                minor,
                host: host.to_string(),
                port: port
                    .parse()
                    .unwrap_or_else(|_| panic!("`{endpoint}` has a bad port")),
            }
        })
        .collect()
}

fn password() -> String {
    std::env::var("VARVE_REDIS_PASSWORD").unwrap_or_else(|_| "varve_dev".into())
}

fn params(server: &Server) -> ConnectParams {
    let mut p = ConnectParams::new(Engine::Redis, server.host.clone(), server.port);
    p.password = Some(Secret::new(password()));
    p.redis_db = TEST_DB;
    p.tls.mode = TlsMode::Disable;
    p.connect_timeout = Duration::from_secs(5);
    p.application_name = "Varve Test".into();
    p
}

async fn connect(p: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
    RedisDriver::new().connect(p).await
}

fn redis(session: &dyn Session) -> &RedisSession {
    session
        .as_any()
        .downcast_ref::<RedisSession>()
        .unwrap_or_else(|| panic!("not a RedisSession"))
}

async fn run(session: &dyn Session, text: &str) -> Result<Vec<StatementResult>, DriverError> {
    Ok(session
        .execute(text, &ExecOptions::default())
        .await?
        .results)
}

/// The single cell of a one-cell result.
fn cell(result: &StatementResult) -> Value {
    match result {
        StatementResult::Rows { rows, .. } if rows.len() == 1 && rows[0].len() == 1 => {
            rows[0][0].clone()
        }
        other => panic!("expected one cell, got {other:?}"),
    }
}

fn tag(result: &StatementResult) -> String {
    match result {
        StatementResult::Command { tag, .. } => tag.clone(),
        other => panic!("expected a status, got {other:?}"),
    }
}

async fn one(session: &dyn Session, text: &str) -> Result<StatementResult, DriverError> {
    let mut results = run(session, text).await?;
    assert_eq!(results.len(), 1, "{text}");
    Ok(results.remove(0))
}

/// True when `CLIENT LIST` shows the client id.
async fn client_listed(session: &dyn Session, id: i64) -> Result<bool, DriverError> {
    match one(session, "CLIENT LIST").await? {
        StatementResult::Rows { rows, .. } => {
            Ok(format!("{rows:?}").contains(&format!("id={id} ")))
        }
        other => panic!("{other:?}"),
    }
}

async fn flush_test_db(session: &dyn Session) -> Result<(), DriverError> {
    // Only database 15 is touched; FLUSHDB applies to the selected one.
    let selected = redis(session).current_db().await;
    assert_eq!(selected, TEST_DB);
    run(session, "FLUSHDB").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn connects_and_authenticates() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let p = params(&server);
        let session = connect(&p).await?;
        let info = session.server_info().clone();
        assert_eq!(info.engine, Engine::Redis, "{label}");
        let expected_flavor = if server.valkey {
            ServerFlavor::Valkey
        } else {
            ServerFlavor::Redis
        };
        let rs = redis(session.as_ref());
        assert_eq!(rs.flavor(), expected_flavor, "{label}");
        assert_eq!(info.major, server.major, "{label}: {info:?}");
        if let Some(minor) = server.minor {
            assert_eq!(info.minor, minor, "{label}: {info:?}");
        }
        if server.valkey {
            assert!(info.version.ends_with("-valkey"), "{}", info.version);
        }
        assert_eq!(rs.current_db().await, TEST_DB);
        assert_eq!(
            cell(&one(session.as_ref(), "CLIENT GETNAME").await?),
            Value::Text("Varve-Test".into()),
            "{label}"
        );
        session.ping().await?;

        // An ACL user created for the test.
        run(
            session.as_ref(),
            "ACL SETUSER varve_test_user on >user_secret ~* &* +@all",
        )
        .await?;
        let mut user = p.clone();
        user.username = Some("varve_test_user".into());
        user.password = Some(Secret::new("user_secret".into()));
        let user_session = connect(&user).await?;
        assert_eq!(
            cell(&one(user_session.as_ref(), "ACL WHOAMI").await?),
            Value::Text("varve_test_user".into()),
            "{label}"
        );
        user_session.close().await?;

        let mut wrong_user = user.clone();
        wrong_user.password = Some(Secret::new("nope".into()));
        let error = connect(&wrong_user).await.err();
        assert_eq!(
            error.as_ref().map(|e| e.kind),
            Some(ErrorKind::Auth),
            "{label}: {error:?}"
        );
        run(session.as_ref(), "ACL DELUSER varve_test_user").await?;

        // Wrong password for the default user, and no password at all.
        let mut wrong = p.clone();
        wrong.password = Some(Secret::new("definitely-wrong".into()));
        let error = connect(&wrong).await.err();
        let error = error.unwrap_or_else(|| panic!("{label}: wrong password connected"));
        assert_eq!(error.kind, ErrorKind::Auth, "{label}: {error:?}");
        assert!(!error.message.contains("definitely-wrong"));
        let mut none = p.clone();
        none.password = None;
        let error = connect(&none).await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Auth), "{label}");

        // `prefer` falls back to plain TCP; `require` does not.
        let mut prefer = p.clone();
        prefer.tls.mode = TlsMode::Prefer;
        connect(&prefer).await?.ping().await?;
        let mut require = p.clone();
        require.tls.mode = TlsMode::Require;
        let error = connect(&require).await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Tls), "{label}");

        session.close().await?;
    }

    // Nothing listens on port 1.
    let mut refused = ConnectParams::new(Engine::Redis, "127.0.0.1", 1);
    refused.connect_timeout = Duration::from_secs(2);
    let error = connect(&refused).await.err();
    assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Connect));

    // A non-routable address times out.
    let mut slow = ConnectParams::new(Engine::Redis, "10.255.255.1", 6379);
    slow.connect_timeout = Duration::from_millis(300);
    let error = connect(&slow).await.err();
    assert!(
        matches!(
            error.as_ref().map(|e| e.kind),
            Some(ErrorKind::Timeout | ErrorKind::Connect)
        ),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn console_reply_shapes() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        flush_test_db(s).await?;

        let results = run(
            s,
            "# comment\n\nSET greeting \"hello world\"\r\nGET greeting\n  INCR counter\nGET missing\nPING\n",
        )
        .await?;
        assert_eq!(results.len(), 5, "{label}");
        assert_eq!(tag(&results[0]), "OK");
        assert_eq!(cell(&results[1]), Value::Text("hello world".into()));
        match &results[2] {
            StatementResult::Rows { columns, rows, .. } => {
                assert_eq!(columns[0].name, "integer");
                assert_eq!(rows, &vec![vec![Value::Int(1)]]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(cell(&results[3]), Value::Null);
        assert_eq!(tag(&results[4]), "PONG");

        // Binary values and escapes.
        run(s, r#"SET bin "\xff\x00\x01""#).await?;
        assert_eq!(
            cell(&one(s, "GET bin").await?),
            Value::Bytes(vec![0xff, 0, 1])
        );
        run(s, r"SET quoted 'it\'s \n literal'").await?;
        assert_eq!(
            cell(&one(s, "GET quoted").await?),
            Value::Text(r"it's \n literal".into())
        );

        // Arrays, nested arrays and max_rows.
        run(s, "RPUSH list a b c d e").await?;
        match one(s, "LRANGE list 0 -1").await? {
            StatementResult::Rows {
                columns,
                rows,
                truncated,
            } => {
                assert_eq!(
                    columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                    ["#", "value"]
                );
                assert_eq!(rows.len(), 5);
                assert_eq!(rows[0], vec![Value::Int(1), Value::Text("a".into())]);
                assert!(!truncated);
            }
            other => panic!("{other:?}"),
        }
        let output = s
            .execute("LRANGE list 0 -1", &ExecOptions { max_rows: 3 })
            .await?;
        match &output.results[0] {
            StatementResult::Rows {
                rows, truncated, ..
            } => {
                assert_eq!(rows.len(), 3);
                assert!(*truncated);
            }
            other => panic!("{other:?}"),
        }
        run(s, "XADD events 1-1 type click").await?;
        match one(s, "XRANGE events - +").await? {
            StatementResult::Rows { rows, .. } => assert_eq!(
                rows[0][1],
                Value::Array(vec![
                    Value::Text("1-1".into()),
                    Value::Array(vec![
                        Value::Text("type".into()),
                        Value::Text("click".into())
                    ])
                ])
            ),
            other => panic!("{other:?}"),
        }
        match one(s, "LRANGE nothing 0 -1").await? {
            StatementResult::Rows { rows, .. } => assert!(rows.is_empty()),
            other => panic!("{other:?}"),
        }

        // The console speaks RESP2: HELLO with another version is refused
        // before it is sent, HELLO 2 runs.
        let error = run(s, "GET a\nHELLO 3").await.err();
        assert!(
            error
                .as_ref()
                .is_some_and(|e| e.kind == ErrorKind::Unsupported
                    && e.message
                        .contains("HELLO with these arguments is not available")),
            "{label}: {error:?}"
        );
        run(s, "HELLO 2").await?;
        run(s, "HSET h f1 v1\nZADD z 1.5 m").await?;
        match one(s, "HGETALL h").await? {
            StatementResult::Rows { rows, .. } => {
                assert_eq!(rows.len(), 2, "{label}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            cell(&one(s, "ZSCORE z m").await?),
            Value::Text("1.5".into())
        );
        run(s, "ZADD z inf top -inf bottom").await?;
        assert_eq!(
            cell(&one(s, "ZSCORE z top").await?),
            Value::Text("inf".into()),
            "{label}"
        );

        // An error stops the script; later lines do not run.
        let error = run(s, "SET kind str\nLPUSH kind x\nSET after 1")
            .await
            .err()
            .unwrap_or_else(|| panic!("{label}: expected WRONGTYPE"));
        assert_eq!(error.kind, ErrorKind::Query);
        assert_eq!(error.code.as_deref(), Some("WRONGTYPE"));
        assert_eq!(cell(&one(s, "EXISTS after").await?), Value::Int(0));
        let error = run(s, "NOSUCHCOMMAND x").await.err();
        assert_eq!(
            error.as_ref().and_then(|e| e.code.clone()).as_deref(),
            Some("ERR")
        );

        // Unterminated quotes fail before anything runs.
        let error = run(s, "SET early 1\nSET k \"open").await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Query));
        assert_eq!(cell(&one(s, "EXISTS early").await?), Value::Int(0));

        // Commands that would break the session are refused.
        let error = run(s, "SUBSCRIBE news").await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Unsupported));

        // SELECT is followed across reconnects.
        run(s, "SELECT 14").await?;
        assert_eq!(redis(s).current_db().await, 14);
        run(s, "SELECT 15").await?;

        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn read_only_blocks_writes() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let writer = connect(&params(&server)).await?;
        flush_test_db(writer.as_ref()).await?;
        run(writer.as_ref(), "SET keep me").await?;

        let mut p = params(&server);
        p.read_only = true;
        let reader = connect(&p).await?;
        let r = reader.as_ref();
        assert!(redis(r).is_read_only());
        assert_eq!(cell(&one(r, "GET keep").await?), Value::Text("me".into()));
        for blocked in [
            "SET keep changed",
            "FLUSHDB",
            "FLUSHALL",
            "DEL keep",
            "EVAL \"return 1\" 0",
            "CONFIG SET maxmemory 1",
            "GET keep\nSET keep changed",
        ] {
            let error = run(r, blocked).await.err();
            assert_eq!(
                error.as_ref().map(|e| e.kind),
                Some(ErrorKind::ReadOnly),
                "{label}: {blocked}: {error:?}"
            );
        }
        // Reads keep working, including admin reads.
        run(r, "CONFIG GET maxmemory\nTTL keep\nSCAN 0").await?;
        assert_eq!(
            cell(&one(writer.as_ref(), "GET keep").await?),
            Value::Text("me".into())
        );
        assert_eq!(cell(&one(writer.as_ref(), "DBSIZE").await?), Value::Int(1));

        // Init commands are checked too.
        let mut init = p.clone();
        init.init_sql = Some("SET from_init 1".into());
        let error = connect(&init).await.err();
        assert!(
            error
                .as_ref()
                .is_some_and(|e| e.message.contains("SET cannot be an init command")),
            "{label}: {error:?}"
        );
        // A connection setup command that is not read-only.
        // Connection setup commands are fine in a read-only profile: they
        // change the connection, never data.
        init.init_sql = Some("CLIENT SETNAME ro-from-init\nSELECT 15".into());
        connect(&init).await?.ping().await?;
        let mut init = params(&server);
        init.init_sql = Some("# comment\nCLIENT SETNAME from-init".into());
        let named = connect(&init).await?;
        assert_eq!(
            cell(&one(named.as_ref(), "CLIENT GETNAME").await?),
            Value::Text("from-init".into())
        );

        flush_test_db(writer.as_ref()).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn classifies_command_risk() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let rs = redis(session.as_ref());
        for (command, expected) in [
            ("GET", CommandRisk::Read),
            ("hgetall", CommandRisk::Read),
            ("SCAN", CommandRisk::Read),
            ("CONFIG GET", CommandRisk::Read),
            ("SET", CommandRisk::Write),
            ("DEL", CommandRisk::Write),
            ("EXPIRE", CommandRisk::Write),
            ("EVAL", CommandRisk::Destructive),
            ("EVALSHA", CommandRisk::Destructive),
            ("FCALL", CommandRisk::Destructive),
            ("FUNCTION FLUSH", CommandRisk::Destructive),
            ("function flush", CommandRisk::Destructive),
            ("SCRIPT FLUSH", CommandRisk::Destructive),
            ("script|flush", CommandRisk::Destructive),
            ("SCRIPT EXISTS", CommandRisk::Read),
            ("CLIENT KILL", CommandRisk::Destructive),
            ("CLIENT LIST", CommandRisk::Read),
            ("CONFIG REWRITE", CommandRisk::Destructive),
            ("CONFIG RESETSTAT", CommandRisk::Destructive),
            ("ACL SETUSER", CommandRisk::Destructive),
            ("ACL DELUSER", CommandRisk::Destructive),
            ("MIGRATE", CommandRisk::Destructive),
            ("REPLICAOF", CommandRisk::Destructive),
            ("FLUSHALL", CommandRisk::Destructive),
            ("FLUSHDB", CommandRisk::Destructive),
            ("SWAPDB", CommandRisk::Destructive),
            ("DEBUG", CommandRisk::Destructive),
            ("SHUTDOWN", CommandRisk::Destructive),
            ("KEYS", CommandRisk::Destructive),
            ("RESTORE", CommandRisk::Destructive),
            ("CONFIG SET", CommandRisk::Destructive),
            ("config|set", CommandRisk::Destructive),
            ("NOSUCHCOMMAND", CommandRisk::Write),
            ("SCRIPT LOAD", CommandRisk::Write),
            ("MEMORY USAGE", CommandRisk::Read),
        ] {
            assert_eq!(
                rs.command_risk(command).await?,
                expected,
                "{label}: {command}"
            );
        }
        assert_eq!(rs.script_risk("GET a\nSET a 1").await?, CommandRisk::Write);
        assert_eq!(
            rs.script_risk("GET a\nKEYS *").await?,
            CommandRisk::Destructive
        );
        assert_eq!(rs.script_risk("# nothing").await?, CommandRisk::Read);
        // Any quoting and case of the flushes.
        for script in [
            "'function' \"FLUSH\"",
            "\"SCRIPT\" 'flush' ASYNC",
            "Function Flush SYNC",
            "\"\\x46UNCTION\" flush",
        ] {
            assert_eq!(
                rs.script_risk(script).await?,
                CommandRisk::Destructive,
                "{label}: {script}"
            );
        }
        // Read-only scripts can run non-write admin commands.
        assert_eq!(
            rs.command_risk("EVAL_RO").await?,
            CommandRisk::Write,
            "{label}"
        );
        assert_eq!(
            rs.command_risk("FCALL_RO").await?,
            CommandRisk::Write,
            "{label}"
        );
        // Read-only sessions refuse scripts and flushes before running them.
        let mut p = params(&server);
        p.read_only = true;
        let ro = connect(&p).await?;
        for script in [
            "EVAL \"return 1\" 0",
            "EVAL_RO \"return 1\" 0",
            "SCRIPT FLUSH",
            "'function' 'flush'",
        ] {
            let error = run(ro.as_ref(), script).await.err();
            assert_eq!(
                error.map(|e| e.kind),
                Some(ErrorKind::ReadOnly),
                "{label}: {script}"
            );
        }
        ro.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn scans_keys_with_patterns_and_types() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        flush_test_db(s).await?;

        let mut script = String::new();
        for i in 0..1000 {
            script.push_str(&format!("SET str:{i} v{i}\n"));
        }
        for i in 0..500 {
            script.push_str(&format!("HSET hash:{i} f v\n"));
        }
        for i in 0..300 {
            script.push_str(&format!("RPUSH list:{i} a b\n"));
        }
        for i in 0..199 {
            script.push_str(&format!("SADD set:{i} m\n"));
        }
        script.push_str("SADD \"set:\\xff\" m\n");
        for i in 0..50 {
            script.push_str(&format!("PEXPIRE str:{i} 600000\n"));
        }
        run(s, &script).await?;
        assert_eq!(cell(&one(s, "DBSIZE").await?), Value::Int(2000), "{label}");

        let rs = redis(s);
        let collect = |pattern: &'static str, filter: Option<KeyType>| async move {
            let mut cursor = 0;
            let mut keys = Vec::new();
            loop {
                let page = rs.scan_keys(cursor, pattern, filter.clone(), 200).await?;
                keys.extend(page.keys);
                cursor = page.cursor;
                if cursor == 0 {
                    break;
                }
            }
            Ok::<_, DriverError>(keys)
        };

        let all = collect("*", None).await?;
        let names: HashSet<Vec<u8>> = all.iter().map(|k| k.name.clone()).collect();
        assert_eq!(names.len(), 2000, "{label}");

        let hashes = collect("hash:*", None).await?;
        assert_eq!(hashes.len(), 500, "{label}");
        assert!(
            hashes
                .iter()
                .all(|k| k.key_type == KeyType::Hash && k.ttl_ms.is_none())
        );

        let lists = collect("*", Some(KeyType::List)).await?;
        assert_eq!(lists.len(), 300, "{label}");
        assert!(lists.iter().all(|k| k.display.starts_with("list:")));

        let sets = collect("set:*", Some(KeyType::Set)).await?;
        assert_eq!(sets.len(), 200, "{label}");
        let binary = sets
            .iter()
            .find(|k| k.name == b"set:\xff")
            .unwrap_or_else(|| panic!("{label}: binary key missing"));
        assert_eq!(binary.display, "set:\\xff");

        let strings = collect("str:*", Some(KeyType::String)).await?;
        assert_eq!(strings.len(), 1000, "{label}");
        let expiring = strings.iter().filter(|k| k.ttl_ms.is_some()).count();
        assert_eq!(expiring, 50, "{label}");
        assert!(
            strings
                .iter()
                .filter_map(|k| k.ttl_ms)
                .all(|t| t > 0 && t <= 600_000)
        );

        let none = collect("str:*", Some(KeyType::Hash)).await?;
        assert!(none.is_empty());

        let keyspace = rs.keyspace().await?;
        let db15 = keyspace
            .iter()
            .find(|d| d.db == TEST_DB)
            .unwrap_or_else(|| panic!("{label}: db15 missing"));
        assert_eq!((db15.keys, db15.expires), (2000, 50), "{label}");
        assert!(rs.database_count().await?.is_some_and(|n| n >= 16));

        flush_test_db(s).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn reads_every_value_type() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        let rs = redis(s);
        flush_test_db(s).await?;

        // String: small, binary and larger than one chunk.
        run(s, "SET small \"héllo\"\nPEXPIRE small 100000").await?;
        let info = rs
            .key_info(b"small")
            .await?
            .unwrap_or_else(|| panic!("{label}: no info"));
        assert_eq!(info.key_type, KeyType::String);
        assert_eq!(info.length, Some(6));
        assert!(info.ttl_ms.is_some_and(|t| t > 0));
        assert!(info.memory_bytes.is_some_and(|m| m > 0));
        assert_eq!(info.encoding.as_deref(), Some("embstr"));
        let chunk = rs
            .read_value(b"small", &ValuePage::first(10))
            .await?
            .unwrap_or_else(|| panic!("{label}: no value"));
        assert_eq!(chunk.total, Some(6));
        assert_eq!(chunk.next_cursor, None);
        assert_eq!(
            chunk.data,
            ValueData::String {
                value: Value::Text("héllo".into()),
                offset: 0,
                truncated: false
            }
        );

        // 600 KiB of "é" (2 bytes each) with an odd offset split.
        let big = format!("x{}", "é".repeat(300 * 1024));
        run(s, &format!("SET big \"{big}\"")).await?;
        let first = rs
            .read_value(b"big", &ValuePage::first(1))
            .await?
            .unwrap_or_else(|| panic!("no big"));
        let (text, truncated) = match &first.data {
            ValueData::String {
                value: Value::Text(t),
                truncated,
                ..
            } => (t.clone(), *truncated),
            other => panic!("{label}: {other:?}"),
        };
        assert!(truncated);
        assert!(text.len() as u64 <= varve_driver_redis::STRING_CHUNK_BYTES);
        let cursor = first
            .next_cursor
            .clone()
            .unwrap_or_else(|| panic!("no next cursor"));
        let second = rs
            .read_value(b"big", &ValuePage { cursor, count: 1 })
            .await?
            .unwrap_or_else(|| panic!("no second chunk"));
        let rest = match &second.data {
            ValueData::String {
                value: Value::Text(t),
                truncated,
                ..
            } => {
                assert!(!truncated);
                t.clone()
            }
            other => panic!("{label}: {other:?}"),
        };
        assert_eq!(format!("{text}{rest}"), big, "{label}");
        assert_eq!(second.next_cursor, None);

        run(s, r#"SET raw "\xff\xfe""#).await?;
        let raw = rs.read_value(b"raw", &ValuePage::first(1)).await?;
        assert!(matches!(
            raw.map(|c| c.data),
            Some(ValueData::String {
                value: Value::Bytes(_),
                ..
            })
        ));

        // Hash with 300 fields: hashtable encoding, real HSCAN cursors.
        // One long value forces the hashtable encoding on every version.
        let mut script = format!("HSET hash f300 v{}", "3".repeat(100));
        for i in 0..300 {
            script.push_str(&format!(" f{i} v{i}"));
        }
        run(s, &script).await?;
        let info = rs
            .key_info(b"hash")
            .await?
            .unwrap_or_else(|| panic!("no hash"));
        assert_eq!(
            (info.key_type.clone(), info.length),
            (KeyType::Hash, Some(301))
        );
        assert_eq!(info.encoding.as_deref(), Some("hashtable"), "{label}");
        let mut page = ValuePage::first(50);
        let mut fields = HashSet::new();
        let mut pages = 0;
        loop {
            let chunk = rs
                .read_value(b"hash", &page)
                .await?
                .unwrap_or_else(|| panic!("hash vanished"));
            pages += 1;
            assert_eq!(chunk.total, Some(301));
            if let ValueData::Hash { entries } = &chunk.data {
                for entry in entries {
                    if let (Value::Text(f), Value::Text(v)) = (&entry.field, &entry.value) {
                        if f != "f300" {
                            assert_eq!(v, &format!("v{}", &f[1..]));
                        }
                        fields.insert(f.clone());
                    }
                }
            } else {
                panic!("{label}: not a hash page");
            }
            match chunk.next_cursor {
                Some(next) => page.cursor = next,
                None => break,
            }
        }
        assert_eq!(fields.len(), 301, "{label}");
        assert!(pages > 1, "{label}: expected several HSCAN pages");

        // List paging by offset.
        let mut script = String::from("RPUSH list");
        for i in 0..25 {
            script.push_str(&format!(" item{i}"));
        }
        run(s, &script).await?;
        let mut page = ValuePage::first(10);
        let mut indexes = Vec::new();
        loop {
            let chunk = rs
                .read_value(b"list", &page)
                .await?
                .unwrap_or_else(|| panic!("list vanished"));
            let ValueData::List { items } = chunk.data else {
                panic!("{label}: not a list");
            };
            for item in items {
                assert_eq!(item.value, Value::Text(format!("item{}", item.index)));
                indexes.push(item.index);
            }
            match chunk.next_cursor {
                Some(next) => page.cursor = next,
                None => break,
            }
        }
        assert_eq!(indexes, (0..25).collect::<Vec<u64>>());

        // Set.
        run(s, "SADD set a b c").await?;
        let chunk = rs
            .read_value(b"set", &ValuePage::first(100))
            .await?
            .unwrap_or_else(|| panic!("no set"));
        let ValueData::Set { members } = chunk.data else {
            panic!("{label}: not a set");
        };
        assert_eq!(members.len(), 3);
        assert_eq!(chunk.total, Some(3));
        assert_eq!(chunk.next_cursor, None);

        // Sorted set by rank with scores.
        run(s, "ZADD zset 1 one 2.5 two inf top -inf bottom").await?;
        let chunk = rs
            .read_value(b"zset", &ValuePage::first(3))
            .await?
            .unwrap_or_else(|| panic!("no zset"));
        let ValueData::Zset { members } = &chunk.data else {
            panic!("{label}: not a zset");
        };
        assert_eq!(members.len(), 3);
        assert_eq!(members[0].member, Value::Text("bottom".into()));
        assert_eq!(members[0].score, Value::Other("-inf".into()), "{label}");
        assert_eq!(members[2].score, Value::Float(2.5));
        assert_eq!(chunk.next_cursor.as_deref(), Some("3"));
        let rest = rs
            .read_value(
                b"zset",
                &ValuePage {
                    cursor: "3".into(),
                    count: 3,
                },
            )
            .await?
            .unwrap_or_else(|| panic!("no zset"));
        assert_eq!(rest.next_cursor, None);
        assert!(
            matches!(&rest.data, ValueData::Zset { members } if members.len() == 1 && members[0].score == Value::Other("inf".into())),
            "{label}: {rest:?}"
        );
        // JSON has no infinity: the score must not turn into null.
        let json = serde_json::to_string(&rest)?;
        assert!(
            json.contains(r#""score":{"t":"other","v":"inf"}"#),
            "{label}: {json}"
        );

        // Stream paging by id.
        let mut script = String::new();
        for i in 1..=25 {
            script.push_str(&format!("XADD stream {i}-0 n {i}\n"));
        }
        run(s, &script).await?;
        let info = rs
            .key_info(b"stream")
            .await?
            .unwrap_or_else(|| panic!("no stream"));
        assert_eq!(
            (info.key_type.clone(), info.length),
            (KeyType::Stream, Some(25))
        );
        let mut page = ValuePage::first(10);
        let mut ids = Vec::new();
        loop {
            let chunk = rs
                .read_value(b"stream", &page)
                .await?
                .unwrap_or_else(|| panic!("stream vanished"));
            let ValueData::Stream { entries } = chunk.data else {
                panic!("{label}: not a stream");
            };
            for entry in entries {
                assert_eq!(entry.fields[0].field, Value::Text("n".into()));
                ids.push(entry.id);
            }
            match chunk.next_cursor {
                Some(next) => page.cursor = next,
                None => break,
            }
        }
        assert_eq!(ids.len(), 25, "{label}: {ids:?}");
        assert_eq!(ids.first().map(String::as_str), Some("1-0"));
        assert_eq!(ids.last().map(String::as_str), Some("25-0"));

        // JSON where the server has it (built into Redis 8).
        match run(s, r#"JSON.SET doc $ '{"a":[1,2],"b":"x"}'"#).await {
            Ok(_) => {
                let info = rs
                    .key_info(b"doc")
                    .await?
                    .unwrap_or_else(|| panic!("no doc"));
                assert_eq!(info.key_type, KeyType::Json);
                let chunk = rs
                    .read_value(b"doc", &ValuePage::first(1))
                    .await?
                    .unwrap_or_else(|| panic!("no doc"));
                assert_eq!(
                    chunk.data,
                    ValueData::Json {
                        document: Value::json(r#"{"a":[1,2],"b":"x"}"#)
                    }
                );
                // Key order is the document's, not sorted.
                run(
                    s,
                    r#"JSON.SET ordered $ '{"zeta":1,"alpha":{"y":2,"b":3}}'"#,
                )
                .await?;
                let chunk = rs
                    .read_value(b"ordered", &ValuePage::first(1))
                    .await?
                    .unwrap_or_else(|| panic!("no ordered doc"));
                match &chunk.data {
                    ValueData::Json {
                        document: Value::Json(text),
                    } => assert_eq!(
                        text.as_str(),
                        r#"{"zeta":1,"alpha":{"y":2,"b":3}}"#,
                        "{label}"
                    ),
                    other => panic!("{label}: {other:?}"),
                }
            }
            Err(error) => {
                assert!(
                    !server.has_builtin_json(),
                    "{label}: Redis 8 should have JSON: {error:?}"
                );
                assert_eq!(error.code.as_deref(), Some("ERR"));
            }
        }

        // Missing keys and bad cursors.
        assert_eq!(rs.key_info(b"missing").await?, None);
        assert_eq!(rs.read_value(b"missing", &ValuePage::first(1)).await?, None);
        let error = rs
            .read_value(
                b"list",
                &ValuePage {
                    cursor: "abc".into(),
                    count: 1,
                },
            )
            .await
            .err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Query));

        flush_test_db(s).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn cancels_blocking_commands() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session: std::sync::Arc<dyn Session> = connect(&params(&server)).await?.into();
        let s = session.as_ref();
        flush_test_db(s).await?;
        run(s, "SET marker here").await?;
        let first_id = cell(&one(s, "CLIENT ID").await?);

        // Cancelling an idle session is a no-op.
        session.cancel_handle().cancel().await?;
        assert_eq!(cell(&one(s, "CLIENT ID").await?), first_id);

        let handle = session.cancel_handle();
        let running = {
            let session = std::sync::Arc::clone(&session);
            tokio::spawn(async move {
                session
                    .execute("BLPOP nokey 30", &ExecOptions::default())
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = Instant::now();
        handle.cancel().await?;
        let result = tokio::time::timeout(Duration::from_secs(2), running).await??;
        assert!(started.elapsed() < Duration::from_secs(2), "{label}");
        assert_eq!(
            result.err().map(|e| e.kind),
            Some(ErrorKind::Cancelled),
            "{label}"
        );

        // The session reconnects: new client, same database and name.
        session.ping().await?;
        let second_id = cell(&one(s, "CLIENT ID").await?);
        assert_ne!(second_id, first_id, "{label}");
        assert_eq!(
            cell(&one(s, "GET marker").await?),
            Value::Text("here".into())
        );
        assert_eq!(
            cell(&one(s, "CLIENT GETNAME").await?),
            Value::Text("Varve-Test".into())
        );
        // The old connection is gone on the server.
        if let Value::Int(old) = first_id {
            match one(s, "CLIENT LIST").await? {
                StatementResult::Rows { rows, .. } => {
                    let text = format!("{rows:?}");
                    assert!(!text.contains(&format!("id={old} ")), "{label}");
                }
                other => panic!("{other:?}"),
            }
        }

        // The statement timeout is enforced on the client and the session
        // recovers afterwards.
        let mut timed = params(&server);
        timed.statement_timeout = Some(Duration::from_millis(300));
        let timed = connect(&timed).await?;
        let timed_id = cell(&one(timed.as_ref(), "CLIENT ID").await?);
        let error = run(timed.as_ref(), "BLPOP queue 5").await.err();
        let error = error.unwrap_or_else(|| panic!("{label}: BLPOP did not time out"));
        assert_eq!(error.kind, ErrorKind::Timeout, "{label}");
        assert!(
            error.message.contains("server may still finish"),
            "{label}: {}",
            error.message
        );
        // The timed-out connection is closed at once: the blocked BLPOP is
        // gone, so a value pushed now stays in the list instead of being
        // delivered to a reply nobody reads.
        tokio::time::sleep(Duration::from_millis(200)).await;
        run(s, "RPUSH queue late").await?;
        assert_eq!(cell(&one(s, "LLEN queue").await?), Value::Int(1), "{label}");
        if let Value::Int(old) = timed_id {
            assert!(!client_listed(s, old).await?, "{label}: old client alive");
        }
        timed.ping().await?;
        assert_ne!(cell(&one(timed.as_ref(), "CLIENT ID").await?), timed_id);

        // Explicit database switches are kept for later reconnects.
        redis(s).select_db(14).await?;
        assert_eq!(redis(s).current_db().await, 14);
        redis(s).select_db(TEST_DB).await?;
        flush_test_db(s).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn cancel_without_client_kill_permission() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let admin = connect(&params(&server)).await?;
        let a = admin.as_ref();
        flush_test_db(a).await?;
        // A user that may do everything except CLIENT KILL (and the other
        // CLIENT subcommands, which Redis 6.2 cannot exclude one by one).
        run(
            a,
            "ACL SETUSER varve_nokill reset on >nokill_secret ~* &* +@all -client +client|id +client|setname +client|getname",
        )
        .await?;
        let mut p = params(&server);
        p.username = Some("varve_nokill".into());
        p.password = Some(Secret::new("nokill_secret".into()));
        let session: std::sync::Arc<dyn Session> = connect(&p).await?.into();
        let user_id = cell(&one(session.as_ref(), "CLIENT ID").await?);

        let handle = session.cancel_handle();
        let running = {
            let session = std::sync::Arc::clone(&session);
            tokio::spawn(async move {
                session
                    .execute("BLPOP nokill_queue 30", &ExecOptions::default())
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = Instant::now();
        // The cancel itself succeeds: the call is cancelled client side.
        handle.cancel().await?;
        let result = tokio::time::timeout(Duration::from_secs(5), running).await??;
        assert!(started.elapsed() < Duration::from_secs(3), "{label}");
        let error = result
            .err()
            .unwrap_or_else(|| panic!("{label}: BLPOP was not cancelled"));
        assert_eq!(error.kind, ErrorKind::Cancelled, "{label}");
        assert!(
            error.message.contains("may keep running on the server"),
            "{label}: {}",
            error.message
        );
        assert!(
            error
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("NOPERM")),
            "{label}: {:?}",
            error.detail
        );
        assert!(!error.message.contains("nokill_secret"));

        // Varve closed the connection, so the server dropped the blocked
        // client too, and the session works again on a new connection.
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Value::Int(old) = user_id {
            assert!(!client_listed(a, old).await?, "{label}: old client alive");
        }
        session.ping().await?;
        assert_ne!(cell(&one(session.as_ref(), "CLIENT ID").await?), user_id);

        // A successful kill reports a plain cancel.
        let admin_shared: std::sync::Arc<dyn Session> = connect(&params(&server)).await?.into();
        let handle = admin_shared.cancel_handle();
        let running = {
            let session = std::sync::Arc::clone(&admin_shared);
            tokio::spawn(async move {
                session
                    .execute("BLPOP nokill_queue 30", &ExecOptions::default())
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.cancel().await?;
        let error = tokio::time::timeout(Duration::from_secs(5), running)
            .await??
            .err()
            .unwrap_or_else(|| panic!("{label}: BLPOP was not cancelled"));
        assert_eq!(error.kind, ErrorKind::Cancelled, "{label}");
        assert_eq!(error.message, "The command was cancelled", "{label}");
        assert_eq!(error.detail, None);

        run(a, "ACL DELUSER varve_nokill").await?;
        flush_test_db(a).await?;
    }
    Ok(())
}

/// A value of `len` bytes for console scripts.
fn blob(tag: char, len: usize) -> String {
    tag.to_string().repeat(len)
}

const MIB: usize = 1024 * 1024;

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn bounds_memory_for_large_values() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        let rs = redis(s);
        flush_test_db(s).await?;

        // Twelve 1 MiB elements in every collection type.
        for i in 0..12 {
            let value = blob(char::from(b'a' + i), MIB);
            run(
                s,
                &format!(
                    "RPUSH biglist {value}\nHSET bighash f{i:02} {value}\nSADD bigset {value}\nZADD bigzset {i} {value}\nXADD bigstream {}-0 v {value}",
                    i + 1
                ),
            )
            .await?;
        }
        let limit = varve_driver_redis::MAX_PAGE_BYTES;
        for key in ["biglist", "bighash", "bigset", "bigzset", "bigstream"] {
            let mut page = ValuePage::first(1000);
            let mut seen = 0usize;
            let mut pages = 0;
            loop {
                let chunk = rs
                    .read_value(key.as_bytes(), &page)
                    .await?
                    .unwrap_or_else(|| panic!("{label}: {key} vanished"));
                pages += 1;
                let entries = match &chunk.data {
                    ValueData::List { items } => items.len(),
                    ValueData::Hash { entries } => entries.len(),
                    ValueData::Set { members } => members.len(),
                    ValueData::Zset { members } => members.len(),
                    ValueData::Stream { entries } => entries.len(),
                    other => panic!("{label}: {other:?}"),
                };
                // Bytes of the page: the budget plus at most one batch. A
                // batch of a ranged read is sized to fit; HSCAN and SSCAN
                // steps are sized by the server (COUNT is a hint), but
                // still never over MAX_REPLY_BYTES.
                let bytes = serde_json::to_string(&chunk.data)?.len() as u64;
                let allowed = if matches!(key, "bighash" | "bigset") {
                    limit + varve_driver_redis::MAX_REPLY_BYTES
                } else {
                    2 * limit
                };
                assert!(
                    bytes <= allowed,
                    "{label}: {key} page of {entries} entries has {bytes} bytes"
                );
                // A ranged read never takes all twelve at once; one HSCAN or
                // SSCAN step may (Valkey 9 returns whole buckets) as long
                // as it stays under MAX_REPLY_BYTES.
                let scan_type = matches!(key, "bighash" | "bigset");
                assert!(scan_type || entries < 12, "{label}: {key} came in one page");
                seen += entries;
                match chunk.next_cursor {
                    Some(next) => page.cursor = next,
                    None => break,
                }
            }
            assert_eq!(seen, 12, "{label}: {key}");
            assert!(
                matches!(key, "bighash" | "bigset") || pages >= 2,
                "{label}: {key} took {pages} pages"
            );
        }

        // The console refuses replies over MAX_REPLY_BYTES (16 MiB) and
        // reconnects afterwards.
        // 12 MiB fits; 18 MiB does not.
        run(s, "LRANGE biglist 0 -1").await?;
        let mut script = String::new();
        for i in 0..6 {
            script.push_str(&format!(
                "RPUSH biglist {}\n",
                blob(char::from(b'm' + i), MIB)
            ));
        }
        run(s, &script).await?;
        let error = run(s, "LRANGE biglist 0 -1\nSET after 1")
            .await
            .err()
            .unwrap_or_else(|| panic!("{label}: an 18 MiB reply was read"));
        assert_eq!(error.kind, ErrorKind::Query, "{label}");
        assert!(
            error.message.contains("larger than 16 MiB"),
            "{label}: {}",
            error.message
        );
        assert_eq!(
            cell(&one(s, "EXISTS after").await?),
            Value::Int(0),
            "{label}"
        );
        assert_eq!(cell(&one(s, "LLEN biglist").await?), Value::Int(18));
        run(s, "SETRANGE hugestring 17825792 x").await?;
        let error = run(s, "GET hugestring").await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Query), "{label}");
        match one(s, "GETRANGE hugestring 0 9").await? {
            StatementResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
            other => panic!("{other:?}"),
        }

        // A single element over the limit becomes a placeholder with its
        // size and the page goes on past it.
        run(
            s,
            "RPUSH mixed a b\nEVAL \"redis.call('RPUSH', KEYS[1], string.rep('x', 17825792))\" 1 mixed\nRPUSH mixed c",
        )
        .await?;
        let page = rs
            .read_value(b"mixed", &ValuePage::first(100))
            .await?
            .unwrap_or_else(|| panic!("no mixed"));
        let ValueData::List { items } = &page.data else {
            panic!("{label}: not a list");
        };
        assert_eq!(
            items.iter().map(|i| i.value.clone()).collect::<Vec<_>>(),
            [
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Other("(too large to show: 17825792 bytes)".into()),
                Value::Text("c".into()),
            ],
            "{label}"
        );
        assert_eq!(items[2].index, 2);
        assert_eq!(page.next_cursor, None);

        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

/// The values of a page as one flat list, for searching placeholders.
fn page_values(data: &ValueData) -> Vec<Value> {
    match data {
        ValueData::List { items } => items.iter().map(|i| i.value.clone()).collect(),
        ValueData::Hash { entries } => entries
            .iter()
            .flat_map(|e| [e.field.clone(), e.value.clone()])
            .collect(),
        ValueData::Set { members } => members.clone(),
        ValueData::Zset { members } => members
            .iter()
            .flat_map(|m| [m.member.clone(), m.score.clone()])
            .collect(),
        ValueData::Stream { entries } => entries
            .iter()
            .flat_map(|e| {
                std::iter::once(Value::Text(e.id.clone())).chain(
                    e.fields
                        .iter()
                        .flat_map(|f| [f.field.clone(), f.value.clone()]),
                )
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// Reads every page of a key (small pages) and returns all values.
async fn read_all(rs: &RedisSession, key: &[u8]) -> Result<Vec<Value>, DriverError> {
    let mut page = ValuePage::first(3);
    let mut all = Vec::new();
    for _ in 0..1000 {
        let chunk = rs
            .read_value(key, &page)
            .await?
            .unwrap_or_else(|| panic!("key vanished"));
        all.extend(page_values(&chunk.data));
        match chunk.next_cursor {
            Some(next) => page.cursor = next,
            None => return Ok(all),
        }
    }
    panic!("paging never ended");
}

const HUGE: usize = 17_825_792;

fn huge_at_scan() -> Value {
    Value::Other(format!(
        "(too large to show: {HUGE} bytes at the time of the scan)"
    ))
}

fn huge_mark() -> Value {
    Value::Other(format!("(too large to show: {HUGE} bytes)"))
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn pages_past_oversized_entries() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        let rs = redis(s);
        flush_test_db(s).await?;
        let rep = format!("string.rep('x', {HUGE})");
        // Small entries around one huge value, one huge field name (hash),
        // one huge member (set, sorted set) and one huge entry (stream).
        let mut script = String::new();
        for i in 0..8 {
            script.push_str(&format!(
                "HSET h f{i} v{i}\nSADD st m{i}\nZADD z {i} m{i}\n"
            ));
        }
        // Stream ids must grow: four entries, the huge one, four more.
        for i in 1..=4 {
            script.push_str(&format!("XADD x {i}-0 n {i}\n"));
        }
        script.push_str(&format!(
            "EVAL \"redis.call('HSET', KEYS[1], 'big', {rep}); redis.call('HSET', KEYS[1], {rep} .. 'f', 'small')\" 1 h\n\
             EVAL \"redis.call('HSET', KEYS[1], 'big', {rep})\" 1 hv\n\
             HSET hv a 1 b 2 c 3 d 4 e 5\n\
             EVAL \"redis.call('SADD', KEYS[1], {rep})\" 1 st\n\
             EVAL \"redis.call('ZADD', KEYS[1], 4.5, {rep})\" 1 z\n\
             EVAL \"redis.call('XADD', KEYS[1], '4-1', 'v', {rep})\" 1 x\n"
        ));
        for i in 5..=8 {
            script.push_str(&format!("XADD x {i}-0 n {i}\n"));
        }
        run(s, &script).await?;

        let hash = read_all(rs, b"h").await?;
        for i in 0..8 {
            let at = hash.iter().position(|v| *v == Value::Text(format!("f{i}")));
            let at = at.unwrap_or_else(|| panic!("{label}: f{i} missing"));
            assert_eq!(hash[at + 1], Value::Text(format!("v{i}")), "{label}");
        }
        let big = hash.iter().position(|v| *v == Value::Text("big".into()));
        let big = big.unwrap_or_else(|| panic!("{label}: field big missing: {hash:?}"));
        assert_eq!(hash[big + 1], huge_mark(), "{label}");
        // The huge field name is a placeholder; its small value is not read.
        let long_field = Value::Other(format!("(too large to show: {} bytes)", HUGE + 1));
        assert!(hash.contains(&long_field), "{label}: {hash:?}");
        assert_eq!(hash.len(), 2 * 10, "{label}");

        let set = read_all(rs, b"st").await?;
        assert_eq!(set.len(), 9, "{label}");
        assert!(set.contains(&huge_at_scan()), "{label}: {set:?}");
        assert!(set.contains(&Value::Text("m7".into())), "{label}");

        let zset = read_all(rs, b"z").await?;
        assert_eq!(zset.len(), 2 * 9, "{label}");
        let at = zset.iter().position(|v| *v == huge_mark());
        let at = at.unwrap_or_else(|| panic!("{label}: {zset:?}"));
        assert_eq!(zset[at + 1], Value::Float(4.5), "{label}");
        assert_eq!(zset.last(), Some(&Value::Float(7.0)));

        let stream = read_all(rs, b"x").await?;
        let at = stream.iter().position(|v| *v == Value::Text("4-1".into()));
        let at = at.unwrap_or_else(|| panic!("{label}: {stream:?}"));
        assert_eq!(
            stream[at + 1],
            Value::Other(format!("(too large to show: {} bytes)", HUGE + 1)),
            "{label}"
        );
        assert!(stream.contains(&Value::Text("8-0".into())), "{label}");

        // A user who may not run scripts pages past every oversized entry
        // the same way: no script is involved.
        run(
            s,
            "ACL SETUSER varve_noscript reset on >noscript_secret ~* &* +@all -@scripting",
        )
        .await?;
        let mut p = params(&server);
        p.username = Some("varve_noscript".into());
        p.password = Some(Secret::new("noscript_secret".into()));
        let limited = connect(&p).await?;
        let lr = redis(limited.as_ref());
        let hash = read_all(lr, b"hv").await?;
        assert!(hash.contains(&huge_mark()), "{label}");
        assert_eq!(hash.len(), 2 * 6, "{label}");
        let hash = read_all(lr, b"h").await?;
        assert!(hash.contains(&long_field), "{label}: {hash:?}");
        assert_eq!(hash.len(), 2 * 10, "{label}");
        let set = read_all(lr, b"st").await?;
        assert!(set.contains(&huge_at_scan()), "{label}");
        assert_eq!(set.len(), 9, "{label}");
        let zset = read_all(lr, b"z").await?;
        let at = zset.iter().position(|v| *v == huge_mark());
        let at = at.unwrap_or_else(|| panic!("{label}: {zset:?}"));
        assert_eq!(zset[at + 1], Value::Float(4.5), "{label}");
        let stream = read_all(lr, b"x").await?;
        assert!(stream.contains(&Value::Text("8-0".into())), "{label}");
        limited.close().await?;
        run(s, "ACL DELUSER varve_noscript").await?;

        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn refuses_replies_with_too_many_values() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        // Four million integers in one nested array: about 16 MB on the
        // wire, one top-level row, far more values than the limit.
        let error = run(
            s,
            "EVAL \"local t={} for i=1,4000000 do t[i]=1 end return {t}\" 0",
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("{label}: four million values were converted"));
        assert_eq!(error.kind, ErrorKind::Query, "{label}: {error:?}");
        assert!(
            error.message.contains("more than 1000000 values"),
            "{label}: {}",
            error.message
        );
        // Below the limit it works.
        match one(
            s,
            "EVAL \"local t={} for i=1,500000 do t[i]=1 end return {t}\" 0",
        )
        .await?
        {
            StatementResult::Rows { rows, .. } => {
                assert!(matches!(&rows[0][1], Value::Array(items) if items.len() == 500_000));
            }
            other => panic!("{other:?}"),
        }
        session.ping().await?;
        session.close().await?;
    }
    Ok(())
}

/// `cmdstat_<name>:calls=N` from `INFO commandstats`.
async fn command_calls(session: &dyn Session, name: &str) -> Result<u64, DriverError> {
    let text = match cell(&one(session, "INFO commandstats").await?) {
        Value::Text(text) => text,
        other => panic!("{other:?}"),
    };
    let prefix = format!("cmdstat_{name}:calls=");
    Ok(text
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .and_then(|rest| rest.split(',').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0))
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn split_hash_and_set_steps_respect_the_page_budget() -> TestResult {
    const TEN_MIB: usize = 10 * MIB;
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        let rs = redis(s);
        flush_test_db(s).await?;
        // 24 hash fields and 12 set members of 10 MiB each: a single
        // HSCAN/SSCAN step can hold several of them (Valkey 9 returns whole
        // buckets), far more than one reply may carry.
        let mut script = String::new();
        for i in 0..24 {
            script.push_str(&format!(
                "EVAL \"redis.call('HSET', KEYS[1], 'f{i:02}', string.rep('{}', {TEN_MIB}))\" 1 bh\n",
                char::from(b'a' + i)
            ));
        }
        for i in 0..12 {
            script.push_str(&format!(
                "EVAL \"redis.call('SADD', KEYS[1], string.rep('{}', {TEN_MIB}))\" 1 bs\n",
                char::from(b'a' + i)
            ));
        }
        run(s, &script).await?;
        // Each page: the budget plus at most one entry.
        let allowed = varve_driver_redis::MAX_PAGE_BYTES + TEN_MIB as u64 + 64 * 1024;
        for (key, expected) in [("bh", 24usize), ("bs", 12)] {
            for count in [1u32, 100] {
                let mut page = ValuePage::first(count);
                let mut seen = HashSet::new();
                let mut entries = 0;
                for _ in 0..200 {
                    let chunk = rs
                        .read_value(key.as_bytes(), &page)
                        .await?
                        .unwrap_or_else(|| panic!("{label}: {key} vanished"));
                    let bytes = serde_json::to_string(&chunk.data)?.len() as u64;
                    let names: Vec<String> = match &chunk.data {
                        ValueData::Hash { entries } => entries
                            .iter()
                            .map(|e| {
                                assert!(
                                    matches!(&e.value, Value::Text(v) if v.len() == TEN_MIB),
                                    "{label}: value of {:?} not read",
                                    e.field
                                );
                                format!("{:?}", e.field)
                            })
                            .collect(),
                        // A member too large to keep while its step was read
                        // is a placeholder with its size at scan time; it
                        // stands for one (unknown) member.
                        ValueData::Set { members } => members
                            .iter()
                            .enumerate()
                            .map(|(at, m)| match m {
                                Value::Text(v) if v.len() == TEN_MIB => v[..1].to_string(),
                                Value::Other(text)
                                    if *text
                                        == format!(
                                            "(too large to show: {TEN_MIB} bytes at the time of the scan)"
                                        ) =>
                                {
                                    format!("placeholder {} {at}", entries)
                                }
                                other => panic!("{label}: member not read: {:.40?}", other),
                            })
                            .collect(),
                        other => panic!("{label}: {other:.40?}"),
                    };
                    // No page is empty, the last one included.
                    assert!(!names.is_empty(), "{label}: {key} empty page");
                    assert!(
                        bytes <= allowed,
                        "{label}: {key} count {count}: page of {} entries has {bytes} bytes",
                        names.len()
                    );
                    if count == 1 {
                        assert_eq!(names.len(), 1, "{label}: {key}");
                    }
                    entries += names.len();
                    seen.extend(names);
                    match chunk.next_cursor {
                        Some(next) => page.cursor = next,
                        None => break,
                    }
                }
                assert_eq!(entries, expected, "{label}: {key} count {count}");
                assert_eq!(seen.len(), expected, "{label}: {key} count {count}");
            }
        }
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn read_only_sessions_run_no_eval_without_eval_ro() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        flush_test_db(s).await?;
        let rep = format!("string.rep('x', {HUGE})");
        run(
            s,
            &format!(
                "RPUSH l a\nEVAL \"redis.call('RPUSH', KEYS[1], {rep})\" 1 l\nRPUSH l b\n\
                 ZADD z 1 a\nEVAL \"redis.call('ZADD', KEYS[1], 2, {rep})\" 1 z\nZADD z 3 b\n\
                 HSET h a 1\nEVAL \"redis.call('HSET', KEYS[1], 'big', {rep})\" 1 h"
            ),
        )
        .await?;
        let mut p = params(&server);
        p.read_only = true;
        let ro = connect(&p).await?;
        let rr = redis(ro.as_ref());
        let evals_before = command_calls(s, "eval").await?;
        let eval_ros_before = command_calls(s, "eval_ro").await?;

        // Every oversized entry is measured without a script, also in a
        // read-only session on a server without EVAL_RO.
        let list = read_all(rr, b"l").await?;
        assert_eq!(
            list,
            [
                Value::Text("a".into()),
                huge_mark(),
                Value::Text("b".into())
            ],
            "{label}"
        );
        let zset = read_all(rr, b"z").await?;
        let at = zset.iter().position(|v| *v == huge_mark());
        let at = at.unwrap_or_else(|| panic!("{label}: {zset:?}"));
        assert_eq!(zset[at + 1], Value::Float(2.0), "{label}");
        let hash = read_all(rr, b"h").await?;
        assert!(hash.contains(&huge_mark()), "{label}: {hash:?}");
        // No script ran (`s` only reads the statistics).
        assert_eq!(command_calls(s, "eval").await?, evals_before, "{label}");
        assert_eq!(
            command_calls(s, "eval_ro").await?,
            eval_ros_before,
            "{label}"
        );
        ro.close().await?;
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn scans_past_oversized_key_names() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        flush_test_db(s).await?;
        let mut script = String::new();
        for i in 0..20 {
            script.push_str(&format!("SET small:{i} v\n"));
        }
        script.push_str(&format!(
            "EVAL \"redis.call('SET', string.rep('k', {HUGE}), 'v')\" 0\nPEXPIRE small:3 100000000"
        ));
        run(s, &script).await?;
        // Browse in a read-only session: no script may run, and the side
        // connections must be gone afterwards.
        let mut p = params(&server);
        p.read_only = true;
        let ro = connect(&p).await?;
        let rs = redis(ro.as_ref());
        let evals = command_calls(s, "eval").await? + command_calls(s, "eval_ro").await?;
        let sides_before = rs.measuring_connections();
        let mut cursor = 0;
        let mut names = Vec::new();
        let mut placeholders = Vec::new();
        for _ in 0..1000 {
            let page = rs.scan_keys(cursor, "*", None, 100).await?;
            for key in page.keys {
                if key.placeholder {
                    placeholders.push(key);
                } else {
                    names.push(key.display);
                }
            }
            cursor = page.cursor;
            if cursor == 0 {
                break;
            }
        }
        assert_eq!(cursor, 0, "{label}: scan never ended");
        assert_eq!(names.len(), 20, "{label}: {names:?}");
        assert_eq!(placeholders.len(), 1, "{label}");
        assert_eq!(
            placeholders[0].display,
            format!("(too large to show: {HUGE} bytes)"),
            "{label}"
        );
        assert_eq!(placeholders[0].key_type, KeyType::Unknown, "{label}");
        assert!(placeholders[0].name.is_empty());
        assert_eq!(placeholders[0].ttl_ms, None);
        // With a type filter the server filtered the step: the placeholder
        // has that type.
        let mut cursor = 0;
        let mut strings = Vec::new();
        loop {
            let page = rs
                .scan_keys(cursor, "*", Some(KeyType::String), 100)
                .await?;
            strings.extend(page.keys);
            cursor = page.cursor;
            if cursor == 0 {
                break;
            }
        }
        let typed: Vec<_> = strings.iter().filter(|k| k.placeholder).collect();
        assert_eq!(typed.len(), 1, "{label}");
        assert_eq!(typed[0].key_type, KeyType::String, "{label}");
        assert_eq!(strings.len(), 21, "{label}");
        // The type filter applies on the placeholder path too.
        let mut cursor = 0;
        let mut lists = 0;
        loop {
            let page = rs.scan_keys(cursor, "*", Some(KeyType::List), 100).await?;
            lists += page.keys.len();
            cursor = page.cursor;
            if cursor == 0 {
                break;
            }
        }
        assert_eq!(lists, 0, "{label}");
        let evals_after = command_calls(s, "eval").await? + command_calls(s, "eval_ro").await?;
        assert_eq!(evals_after, evals, "{label}: a script ran");
        assert!(
            rs.measuring_connections() > sides_before,
            "{label}: no side connection"
        );
        let clients = format!("{:?}", one(s, "CLIENT LIST").await?);
        assert!(
            !clients.contains(varve_driver_redis::SIDE_NAME_SUFFIX),
            "{label}: a side connection is still open: {clients}"
        );
        ro.close().await?;
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

/// Field names (hash) or member first letters plus lengths (set) of a page.
fn entry_names(data: &ValueData) -> Vec<String> {
    match data {
        ValueData::Hash { entries } => entries.iter().map(|e| format!("{:?}", e.field)).collect(),
        ValueData::Set { members } => members
            .iter()
            .map(|m| match m {
                Value::Text(t) => format!("{}:{}", &t[..1.min(t.len())], t.len()),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:.40?}"),
    }
}

/// Pages through a key with `count` entries per page and, after the first
/// page, runs the commands `mutate` builds from the names seen so far.
/// Returns every entry name seen, in order.
async fn scan_with(
    s: &dyn Session,
    key: &[u8],
    count: u32,
    mutate: impl Fn(&[String]) -> String,
) -> Result<Vec<String>, DriverError> {
    let rs = redis(s);
    let mut page = ValuePage::first(count);
    let mut seen = Vec::new();
    for round in 0..2000 {
        let chunk = rs
            .read_value(key, &page)
            .await?
            .unwrap_or_else(|| panic!("key vanished"));
        let names = entry_names(&chunk.data);
        assert!(!names.is_empty(), "empty page {round}");
        seen.extend(names);
        if round == 0 {
            let commands = mutate(&seen);
            if !commands.is_empty() {
                run(s, &commands).await?;
            }
        }
        match chunk.next_cursor {
            Some(next) => page.cursor = next,
            None => return Ok(seen),
        }
    }
    panic!("paging never ended");
}

fn no_change(_: &[String]) -> String {
    String::new()
}

/// A set member too large for its step's reply is shown as a placeholder
/// with the size measured during the scan, which stands for one member
/// (unknown which). Asserts: every named member seen is in `may`, every
/// placeholder has the 10 MiB scan-time size, and the named members of
/// `must` seen plus the placeholders cover all of `must`.
fn assert_set_cover(label: &str, seen: &[String], must: &[String], may: &[String]) {
    let placeholder = format!(
        "Other({:?})",
        format!(
            "(too large to show: {} bytes at the time of the scan)",
            10 * MIB
        )
    );
    let placeholders = seen.iter().filter(|n| n.starts_with("Other(")).count();
    for name in seen.iter().filter(|n| n.starts_with("Other(")) {
        assert_eq!(*name, placeholder, "{label}: unexpected placeholder");
    }
    let named: HashSet<&String> = seen.iter().filter(|n| !n.starts_with("Other(")).collect();
    for name in &named {
        assert!(may.contains(name), "{label}: unexpected {name}");
    }
    let covered = must.iter().filter(|m| named.contains(m)).count();
    assert!(
        covered + placeholders >= must.len(),
        "{label}: {covered} named + {placeholders} placeholders for {} members",
        must.len()
    );
}

/// Asserts `must` <= seen <= `may` (as sets).
fn assert_between(label: &str, seen: &[String], must: &[String], may: &[String]) {
    let seen: HashSet<&String> = seen.iter().collect();
    for name in must {
        assert!(seen.contains(name), "{label}: {name} missing");
    }
    for name in &seen {
        assert!(may.contains(name), "{label}: unexpected {name}");
    }
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn split_steps_keep_the_scan_guarantee() -> TestResult {
    const TEN_MIB: usize = 10 * MIB;
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        flush_test_db(s).await?;
        // 24 fields and members of 10 MiB: steps with two or more of them
        // are too large for one reply and get split over pages.
        let mut script = String::new();
        for i in 0..24u8 {
            let c = char::from(b'a' + i);
            script.push_str(&format!(
                "EVAL \"redis.call('HSET', KEYS[1], 'f{i:02}', string.rep('{c}', {TEN_MIB})); redis.call('SADD', KEYS[2], string.rep('{c}', {TEN_MIB}))\" 2 gh gs\n"
            ));
        }
        run(s, &script).await?;
        let fields: Vec<String> = (0..24).map(|i| format!("Text(\"f{i:02}\")")).collect();
        let members: Vec<String> = (0..24u8)
            .map(|i| format!("{}:{TEN_MIB}", char::from(b'a' + i)))
            .collect();

        // No writes: exactly the entries, each at least once.
        let seen = scan_with(s, b"gh", 1, no_change).await?;
        assert_between(&format!("{label} gh"), &seen, &fields, &fields);
        let seen = scan_with(s, b"gs", 1, no_change).await?;
        assert_set_cover(&format!("{label} gs"), &seen, &members, &members);

        // A field already shown is deleted mid-scan: every other field
        // still comes, nothing else does.
        let deleted = std::sync::Mutex::new(String::new());
        let seen = scan_with(s, b"gh", 1, |shown| {
            let name = shown[0]
                .trim_start_matches("Text(\"")
                .trim_end_matches("\")")
                .to_string();
            let mut slot = deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = name.clone();
            format!("HDEL gh {name}")
        })
        .await?;
        let gone = deleted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let rest: Vec<String> = fields
            .iter()
            .filter(|f| **f != format!("Text(\"{gone}\")"))
            .cloned()
            .collect();
        assert_between(&format!("{label} gh HDEL {gone}"), &seen, &rest, &fields);
        run(
            s,
            &format!(
                "EVAL \"redis.call('HSET', KEYS[1], '{gone}', string.rep('z', {TEN_MIB}))\" 1 gh"
            ),
        )
        .await?;

        // A member not shown yet is removed mid-scan: every other member
        // comes, the removed one may or may not (SCAN allows both).
        let removed = std::sync::Mutex::new(String::new());
        let seen = scan_with(s, b"gs", 1, |shown| {
            let letter = (0..24u8)
                .map(|i| char::from(b'a' + i))
                .find(|c| !shown.iter().any(|n| n.starts_with(*c)))
                .unwrap_or('x');
            let mut slot = removed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = format!("{letter}:{TEN_MIB}");
            format!("EVAL \"redis.call('SREM', KEYS[1], string.rep('{letter}', {TEN_MIB}))\" 1 gs")
        })
        .await?;
        let removed = removed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let rest: Vec<String> = members.iter().filter(|m| **m != removed).cloned().collect();
        assert_set_cover(
            &format!("{label} gs SREM {removed}"),
            &seen,
            &rest,
            &members,
        );
        let letter = removed.chars().next().unwrap_or('x');
        run(
            s,
            &format!(
                "EVAL \"redis.call('SADD', KEYS[1], string.rep('{letter}', {TEN_MIB}))\" 1 gs"
            ),
        )
        .await?;

        // Both grow by 200 entries mid-scan (rehash): every original entry
        // comes; the only others are new ones.
        let mut grow = String::new();
        let mut new_fields = Vec::new();
        let mut new_members = Vec::new();
        for i in 0..200 {
            grow.push_str(&format!("HSET gh s{i} x\nSADD gs s{i}\n"));
            new_fields.push(format!("Text(\"s{i}\")"));
            new_members.push(format!("s:{}", format!("s{i}").len()));
        }
        let seen = scan_with(s, b"gh", 1, |_| grow.clone()).await?;
        let may: Vec<String> = fields.iter().chain(&new_fields).cloned().collect();
        assert_between(&format!("{label} gh grow"), &seen, &fields, &may);
        run(s, "EVAL \"for i = 0, 199 do redis.call('HDEL', KEYS[1], 's' .. i); redis.call('SREM', KEYS[2], 's' .. i) end\" 2 gh gs").await?;
        let seen = scan_with(s, b"gs", 3, |_| grow.clone()).await?;
        let may: Vec<String> = members.iter().chain(&new_members).cloned().collect();
        assert_set_cover(&format!("{label} gs grow"), &seen, &members, &may);

        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn page_cursors_are_checked() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        let rs = redis(s);
        flush_test_db(s).await?;
        run(
            s,
            "SET str hello\nRPUSH l a b c\nSADD st a b c\nZADD z 1 a 2 b 3 c\nHSET h a 1 b 2 c 3\nXADD x 1-0 a 1\nXADD x 2-0 b 2\nXADD x 3-0 c 3",
        )
        .await?;
        // A hash with real (hashtable) scan steps for the state cursors.
        let mut big = format!("HSET h f300 v{}", "3".repeat(100));
        for i in 0..300 {
            big.push_str(&format!(" f{i} v{i}"));
        }
        run(s, &big).await?;
        let boundaries = [
            "",
            "0",
            "1",
            "9223372036854775806",
            "9223372036854775807",
            "9223372036854775808",
            "18446744073709551000",
            "18446744073709551615",
            "18446744073709551616",
            "-1",
            "0+1",
            "0+18446744073709551615",
            "p:",
            "p:ffff",
            "p:zz",
            "1-0",
            "18446744073709551615-18446744073709551615",
            "18446744073709551615-0",
            " 1",
            "abc",
        ];
        for key in ["str", "l", "st", "z", "h", "x"] {
            for cursor in boundaries {
                let page = ValuePage {
                    cursor: cursor.to_string(),
                    count: 2,
                };
                match rs.read_value(key.as_bytes(), &page).await {
                    Ok(_) => {}
                    Err(error) => assert_eq!(
                        error.kind,
                        ErrorKind::Query,
                        "{label}: {key} {cursor:?}: {error:?}"
                    ),
                }
                for count in [0, 1, u32::MAX] {
                    let page = ValuePage {
                        cursor: cursor.to_string(),
                        count,
                    };
                    let _ = rs.read_value(key.as_bytes(), &page).await;
                }
            }
            // The session still works after every forged cursor.
            session.ping().await?;
        }
        for cursor in [0u64, 1, u64::MAX] {
            rs.scan_keys(cursor, "*", None, 0).await?;
            rs.scan_keys(cursor, "*", None, u32::MAX).await?;
        }

        // A state cursor works once, for its own key only.
        let first = rs
            .read_value(b"h", &ValuePage::first(1))
            .await?
            .unwrap_or_else(|| panic!("no h"));
        let cursor = first
            .next_cursor
            .unwrap_or_else(|| panic!("{label}: one page of three"));
        if cursor.starts_with("p:") {
            let page = ValuePage {
                cursor: cursor.clone(),
                count: 1,
            };
            let error = rs.read_value(b"st", &page).await.err();
            assert_eq!(
                error.and_then(|e| e.code).as_deref(),
                Some("CURSOR_EXPIRED"),
                "{label}: another key's cursor"
            );
            rs.read_value(b"h", &page).await?;
            let error = rs.read_value(b"h", &page).await.err();
            assert_eq!(
                error.and_then(|e| e.code).as_deref(),
                Some("CURSOR_EXPIRED"),
                "{label}: reused cursor"
            );
        }
        let error = rs
            .read_value(
                b"h",
                &ValuePage {
                    cursor: "p:abc".into(),
                    count: 1,
                },
            )
            .await
            .err();
        assert_eq!(
            error.and_then(|e| e.code).as_deref(),
            Some("CURSOR_EXPIRED")
        );

        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn last_pages_are_never_empty() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        flush_test_db(s).await?;
        let rep = format!("string.rep('x', {HUGE})");
        let mut script = String::from(
            "HSET h a 1 b 2 c 3\nSADD st a b c\nXADD x 1-0 a 1\nXADD x 2-0 b 2\nXADD x 3-0 c 3\n",
        );
        script.push_str(&format!(
            "SADD bs a\nEVAL \"redis.call('SADD', KEYS[1], {rep})\" 1 bs\nSADD bs b\n\
             XADD bx 1-0 a 1\nEVAL \"redis.call('XADD', KEYS[1], '2-0', 'v', {rep})\" 1 bx\nXADD bx 3-0 b 2"
        ));
        run(s, &script).await?;
        for read_only in [false, true] {
            let mut p = params(&server);
            p.read_only = read_only;
            let session = connect(&p).await?;
            let rs = redis(session.as_ref());
            for key in ["h", "st", "x", "bs", "bx"] {
                let mut page = ValuePage::first(1);
                let mut pages = Vec::new();
                for _ in 0..100 {
                    let chunk = match rs.read_value(key.as_bytes(), &page).await {
                        Ok(chunk) => chunk.unwrap_or_else(|| panic!("{label}: {key} gone")),
                        // Read-only Redis 6.2 cannot page past huge set or
                        // stream entries (no scripts); covered elsewhere.
                        Err(_) if read_only && key.starts_with('b') && server.major == 6 => {
                            break;
                        }
                        Err(e) => return Err(e.into()),
                    };
                    pages.push(page_values(&chunk.data));
                    match chunk.next_cursor {
                        Some(next) => page.cursor = next,
                        None => break,
                    }
                }
                for values in &pages {
                    assert!(
                        !values.is_empty(),
                        "{label}: read_only {read_only}: {key}: empty page in {pages:.60?}"
                    );
                }
            }
            session.close().await?;
        }
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

/// `name:value` of `INFO memory`, as a number.
async fn memory_field(session: &dyn Session, name: &str) -> Result<Option<u64>, DriverError> {
    let text = match cell(&one(session, "INFO memory").await?) {
        Value::Text(text) => text,
        other => panic!("{other:?}"),
    };
    Ok(text
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}:")))
        .and_then(|v| v.trim().parse().ok()))
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn oversized_set_steps_run_no_scripts() -> TestResult {
    const TEN_MIB: usize = 10 * MIB;
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        flush_test_db(s).await?;
        // 64 members of 10 MiB: on Valkey a small set's whole table is one
        // SSCAN step of 640 MiB.
        for i in 0..64u32 {
            run(
                s,
                &format!(
                    "EVAL \"redis.call('SADD', KEYS[1], string.rep(ARGV[1], {TEN_MIB}))\" 1 s64 {}",
                    char::from_u32(0x30 + i).unwrap_or('x')
                ),
            )
            .await?;
        }
        run(s, "CONFIG SET slowlog-log-slower-than 50000\nSLOWLOG RESET").await?;
        let evals = command_calls(s, "eval").await? + command_calls(s, "eval_ro").await?;
        // `used_memory_vm_eval` on Redis 7 and newer, `used_memory_lua` before.
        let lua_field = if memory_field(s, "used_memory_vm_eval").await?.is_some() {
            "used_memory_vm_eval"
        } else {
            "used_memory_lua"
        };
        let lua_before = memory_field(s, lua_field).await?.unwrap_or_default();

        let names: Vec<String> = (0..64u32)
            .map(|i| format!("{}:{TEN_MIB}", char::from_u32(0x30 + i).unwrap_or('x')))
            .collect();
        let seen = scan_with(s, b"s64", 1, no_change).await?;
        assert_set_cover(&format!("{label} s64"), &seen, &names, &names);
        let seen = scan_with(s, b"s64", 100, no_change).await?;
        assert_set_cover(&format!("{label} s64 by 100"), &seen, &names, &names);

        let evals_after = command_calls(s, "eval").await? + command_calls(s, "eval_ro").await?;
        assert_eq!(evals_after, evals, "{label}: a script ran");
        let lua_after = memory_field(s, lua_field).await?.unwrap_or_default();
        assert!(
            lua_after <= lua_before + 32 * MIB as u64,
            "{label}: {lua_field} grew from {lua_before} to {lua_after}"
        );
        let slowlog = run(s, "SLOWLOG GET 128").await?;
        let text = format!("{slowlog:?}").to_ascii_uppercase();
        assert!(
            !text.contains("EVAL"),
            "{label}: a script took over 50 ms: {slowlog:?}"
        );
        run(s, "CONFIG SET slowlog-log-slower-than 10000").await?;
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn one_measuring_connection_per_page() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        let rs = redis(s);
        flush_test_db(s).await?;
        let rep = format!("string.rep('x', {HUGE})");
        run(
            s,
            &format!(
                "RPUSH l a\nEVAL \"for i = 1, 3 do redis.call('RPUSH', KEYS[1], {rep}) end\" 1 l\nRPUSH l b"
            ),
        )
        .await?;
        let before = rs.measuring_connections();
        let page = rs
            .read_value(b"l", &ValuePage::first(10))
            .await?
            .unwrap_or_else(|| panic!("{label}: no list"));
        let ValueData::List { items } = &page.data else {
            panic!("{label}: not a list");
        };
        let marks = items.iter().filter(|i| i.value == huge_mark()).count();
        assert_eq!(marks, 3, "{label}: {items:.80?}");
        assert_eq!(items.len(), 5, "{label}");
        assert_eq!(
            rs.measuring_connections() - before,
            1,
            "{label}: one side connection for three measurements"
        );
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

/// One `COMMAND` entry (or subcommand) and what its flags say.
struct Entry {
    /// `name` or `name|sub`, lower case.
    name: String,
    /// `readonly` without `write` or `may_replicate`.
    read_only_flagged: bool,
    /// A module command.
    module: bool,
    /// In `@admin` or `@dangerous`.
    admin_or_dangerous: bool,
}

/// The entries of a `COMMAND` reply, subcommands included.
fn command_entries(reply: &StatementResult) -> Vec<Entry> {
    fn words(field: Option<&Value>) -> Vec<String> {
        match field {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|f| match f {
                    Value::Text(t) => Some(t.to_ascii_lowercase()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
    fn walk(value: &Value, out: &mut Vec<Entry>) {
        let Value::Array(fields) = value else {
            return;
        };
        let Some(Value::Text(name)) = fields.first() else {
            return;
        };
        let flags = words(fields.get(2));
        let categories = words(fields.get(6));
        let has = |f: &str| flags.iter().any(|x| x == f);
        out.push(Entry {
            name: name.to_ascii_lowercase(),
            read_only_flagged: has("readonly") && !has("write") && !has("may_replicate"),
            module: has("module"),
            admin_or_dangerous: categories
                .iter()
                .any(|c| c == "@admin" || c == "@dangerous"),
        });
        if let Some(Value::Array(subs)) = fields.get(9) {
            for sub in subs {
                walk(sub, out);
            }
        }
    }
    let mut out = Vec::new();
    if let StatementResult::Rows { rows, .. } = reply {
        for row in rows {
            if let Some(entry) = row.get(1) {
                walk(entry, &mut out);
            }
        }
    }
    out
}

/// True when `(command, sub)` is in one of the driver's lists.
fn in_list(list: &[(&str, Option<&str>)], command: &str, sub: Option<&str>) -> bool {
    list.iter().any(|(n, s)| {
        n.eq_ignore_ascii_case(command)
            && s.is_none_or(|s| sub.is_some_and(|sub| s.eq_ignore_ascii_case(sub)))
    })
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn read_only_sessions_allow_only_read_only_commands() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        let rs = redis(s);
        let output = s
            .execute("COMMAND", &ExecOptions { max_rows: 100_000 })
            .await?;
        let entries = command_entries(&output.results[0]);
        assert!(entries.len() > 200, "{label}: {}", entries.len());
        let (mut allowed, mut refused) = (0, 0);
        for entry in &entries {
            let name = &entry.name;
            let (command, sub) = name
                .split_once('|')
                .map_or((name.as_str(), None), |(c, s)| (c, Some(s)));
            let typed = match sub {
                Some(sub) => format!("{command} {sub}"),
                None => command.to_string(),
            };
            let allows = rs.read_only_allows(&typed).await?;
            let extra = in_list(varve_driver_redis::READ_ONLY_EXTRAS, command, sub);
            let vetted_module =
                in_list(varve_driver_redis::READ_ONLY_MODULE_COMMANDS, command, sub);
            let denied = in_list(varve_driver_redis::READ_ONLY_DENY, command, sub);
            // What the rules say from the server's own entry.
            let trusted = entry.read_only_flagged
                && !entry.admin_or_dangerous
                && (!entry.module || vetted_module)
                && !denied;
            if allows {
                allowed += 1;
                assert!(
                    trusted || extra,
                    "{label}: {typed} allowed but not read-only"
                );
            } else {
                refused += 1;
                // A Redis 6.2 command with subcommands is one coarse entry:
                // its subcommands decide, so the bare name may be refused.
                assert!(
                    !trusted || sub.is_none() && server.major == 6,
                    "{label}: {typed} is read-only by the rules but refused"
                );
            }
        }
        println!(
            "{label}: {allowed} allowed, {refused} refused of {}",
            entries.len()
        );

        // State-changing commands without a write flag, and unknown ones,
        // are refused before they are sent.
        let mut p = params(&server);
        p.read_only = true;
        let ro = connect(&p).await?;
        for script in [
            "CLIENT UNBLOCK 1",
            "CLIENT NO-EVICT on",
            "ACL LOG RESET",
            "MEMORY PURGE",
            "SCRIPT LOAD \"return 1\"",
            "COMMANDLOG RESET slow",
            "CLUSTER MIGRATESLOTS SLOTSRANGE 0 1 NODE x",
            "CLUSTER MIGRATION IMPORT 0 1",
            "HOTKEYS START",
            "BACKUP START",
            "CONFIG SET maxmemory 0",
            "NOSUCHCOMMAND x",
            "CONFIG NOSUCHSUB",
            "PFCOUNT x",
            "KEYS *",
            "SORT_RO list",
            "EVAL_RO \"return redis.call('slowlog', 'reset')\" 0",
            "FCALL_RO myfunc 0",
            "SEARCH.CLUSTERREFRESH",
            "SEARCH.CLUSTERSET MYID 1",
            "TIMESERIES.REFRESHCLUSTER",
            "CF.COMPACT cf",
            "_FT.DEBUG GC_FORCEINVOKE idx",
        ] {
            let error = run(ro.as_ref(), script).await.err();
            let kind = error.as_ref().map(|e| e.kind);
            // PFCOUNT may update its cache (`may_replicate`) only on 6.2.
            if script.starts_with("PFCOUNT") && !(server.major == 6 && !server.valkey) {
                assert_ne!(kind, Some(ErrorKind::ReadOnly), "{label}: {script}");
                continue;
            }
            assert_eq!(
                kind,
                Some(ErrorKind::ReadOnly),
                "{label}: {script}: {error:?}"
            );
        }
        for script in [
            "PING",
            "INFO server",
            "CLIENT LIST",
            "CONFIG GET maxmemory",
            "MEMORY USAGE nokey",
            "SCAN 0",
            "COMMAND COUNT",
            "SLOWLOG GET 1",
            "MULTI\nGET a\nEXEC",
        ] {
            run(ro.as_ref(), script).await?;
        }
        // Vetted module reads run where the module is loaded.
        if server.has_builtin_json() {
            run(ro.as_ref(), "JSON.GET nokey").await?;
            run(ro.as_ref(), "JSON.TYPE nokey").await?;
            run(ro.as_ref(), "FT.CONFIG GET TIMEOUT").await?;
            let error = run(ro.as_ref(), "FT._LIST").await.err();
            assert_eq!(error.map(|e| e.kind), Some(ErrorKind::ReadOnly), "{label}");
        }
        ro.close().await?;
        session.close().await?;
    }
    Ok(())
}

/// Servers started with renamed commands: `CONFIG` as `MYCONF`, `SHUTDOWN`
/// as `MYSHUT`, `SCRIPT` as `MYSCRIPT` and `FLUSHALL` disabled. From
/// `VARVE_REDIS_RENAMED_SERVERS` (same syntax as `VARVE_REDIS_SERVERS`).
fn renamed_servers() -> Vec<(String, String, u16)> {
    let list = std::env::var("VARVE_REDIS_RENAMED_SERVERS").unwrap_or_else(|_| {
        "redis-6.2@127.0.0.1:63064,redis-8.10@127.0.0.1:63084,valkey-9@127.0.0.1:63194".into()
    });
    list.split(',')
        .map(|item| {
            let (label, endpoint) = item
                .trim()
                .split_once('@')
                .unwrap_or_else(|| panic!("`{item}` must look like redis-6.2@host:port"));
            let (host, port) = endpoint
                .rsplit_once(':')
                .unwrap_or_else(|| panic!("`{endpoint}` must be host:port"));
            (
                label.to_string(),
                host.to_string(),
                port.parse()
                    .unwrap_or_else(|_| panic!("bad port in {endpoint}")),
            )
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs servers started with rename-command (see renamed_servers)"]
async fn renamed_commands_are_classified_by_their_real_name() -> TestResult {
    for (label, host, port) in renamed_servers() {
        let mut p = ConnectParams::new(Engine::Redis, host, port);
        p.password = Some(Secret::new(password()));
        p.tls.mode = TlsMode::Disable;
        p.connect_timeout = Duration::from_secs(5);
        // A renamed destructive command is refused as an init command by
        // its real name when connecting.
        for init in ["MYCONF SET maxmemory 0", "SELECT 1\nMYSHUT NOSAVE"] {
            let mut with_init = p.clone();
            with_init.init_sql = Some(init.into());
            let error = connect(&with_init).await.err();
            assert!(
                error
                    .as_ref()
                    .is_some_and(|e| e.message.contains("cannot be an init command")),
                "{label}: {init}: {error:?}"
            );
        }
        let session = connect(&p).await?;
        let rs = redis(session.as_ref());
        for (command, risk, allowed) in [
            ("MYCONF SET", CommandRisk::Destructive, false),
            ("myconf rewrite", CommandRisk::Destructive, false),
            ("MYCONF GET", CommandRisk::Read, true),
            ("MYSHUT", CommandRisk::Destructive, false),
            ("MYSCRIPT FLUSH", CommandRisk::Destructive, false),
            ("MYSCRIPT LOAD", CommandRisk::Write, false),
            ("MYSCRIPT EXISTS", CommandRisk::Read, true),
            // The old names no longer run on these servers (the server
            // rejects them), but are still classified as what they were.
            ("CONFIG SET", CommandRisk::Destructive, false),
            ("FLUSHALL", CommandRisk::Destructive, false),
        ] {
            assert_eq!(rs.command_risk(command).await?, risk, "{label}: {command}");
            assert_eq!(
                rs.read_only_allows(command).await?,
                allowed,
                "{label}: {command}"
            );
        }
        assert_eq!(
            rs.script_risk("GET a\n'myconf' \"set\" maxmemory 0")
                .await?,
            CommandRisk::Destructive,
            "{label}"
        );
        let mut ro_params = p.clone();
        ro_params.read_only = true;
        let ro = connect(&ro_params).await?;
        for script in [
            "MYCONF SET maxmemory 0",
            "MYCONF REWRITE",
            "MYSCRIPT FLUSH",
            "MYSHUT NOSAVE",
            "myscript load \"return 1\"",
        ] {
            let error = run(ro.as_ref(), script).await.err();
            assert_eq!(
                error.as_ref().map(|e| e.kind),
                Some(ErrorKind::ReadOnly),
                "{label}: {script}: {error:?}"
            );
        }
        run(ro.as_ref(), "MYCONF GET maxmemory").await?;
        run(ro.as_ref(), "MYSCRIPT EXISTS abc").await?;
        // An open MULTI block does not change the classification, during
        // or after it.
        // (A fresh session, so the lookups are not cached yet.)
        let fresh = connect(&p).await?;
        let fr = redis(fresh.as_ref());
        run(fresh.as_ref(), "MULTI").await?;
        assert_eq!(
            fr.command_risk("MYSHUT").await?,
            CommandRisk::Destructive,
            "{label}"
        );
        assert_eq!(
            fr.command_risk("MYCONF SET").await?,
            CommandRisk::Destructive
        );
        match one(fresh.as_ref(), "EXEC").await? {
            StatementResult::Rows { rows, .. } => assert!(rows.is_empty(), "{label}: {rows:?}"),
            other => panic!("{label}: {other:?}"),
        }
        assert_eq!(
            fr.command_risk("MYSHUT").await?,
            CommandRisk::Destructive,
            "{label}"
        );
        fresh.close().await?;
        // The server is still up and unchanged.
        session.ping().await?;
        ro.close().await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn prefer_reuses_the_transport_for_reconnects_and_side_connections() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        flush_test_db(s).await?;
        run(
            s,
            &format!("SET small v\nEVAL \"redis.call('SET', string.rep('k', {HUGE}), 'v')\" 0"),
        )
        .await?;
        // The dev servers are plain TCP: `prefer` probes once with a plain
        // PING and then never again in this session.
        let mut p = params(&server);
        p.tls.mode = TlsMode::Prefer;
        p.connect_timeout = Duration::from_secs(4);
        p.statement_timeout = Some(Duration::from_secs(1));
        let started = Instant::now();
        let prefer = connect(&p).await?;
        let connect_time = started.elapsed();
        let rs = redis(prefer.as_ref());
        for _ in 0..3 {
            let started = Instant::now();
            let mut cursor = 0;
            let mut keys = 0;
            loop {
                let page = rs.scan_keys(cursor, "*", None, 100).await?;
                keys += page.keys.len();
                cursor = page.cursor;
                if cursor == 0 {
                    break;
                }
            }
            assert_eq!(keys, 2, "{label}");
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_millis(1500),
                "{label}: a scan took {elapsed:?} (connect took {connect_time:?})"
            );
        }
        prefer.close().await?;
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn an_open_multi_block_is_left_alone() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        let rs = redis(s);
        flush_test_db(s).await?;
        run(s, "SET present 1").await?;
        run(s, "MULTI").await?;
        // Lookups for commands not looked up before run on a side
        // connection; nothing is queued into the user's transaction.
        assert_eq!(
            rs.command_risk("FLUSHALL").await?,
            CommandRisk::Destructive,
            "{label}"
        );
        assert_eq!(
            rs.command_risk("HSET").await?,
            CommandRisk::Write,
            "{label}"
        );
        assert!(rs.read_only_allows("GET").await?, "{label}");
        assert_eq!(
            rs.script_risk("SET a 1\nLPUSH l x").await?,
            CommandRisk::Write
        );
        session.ping().await?;
        // The key browser and SELECT wait for the transaction.
        let error = rs.scan_keys(0, "*", None, 10).await.err();
        assert_eq!(
            error.and_then(|e| e.code).as_deref(),
            Some("MULTI_OPEN"),
            "{label}"
        );
        let error = rs.read_value(b"present", &ValuePage::first(1)).await.err();
        assert_eq!(
            error.and_then(|e| e.code).as_deref(),
            Some("MULTI_OPEN"),
            "{label}"
        );
        let error = rs.select_db(14).await.err();
        assert_eq!(
            error.and_then(|e| e.code).as_deref(),
            Some("MULTI_OPEN"),
            "{label}"
        );
        let queued = one(s, "SET inside 1").await?;
        assert_eq!(tag(&queued), "QUEUED", "{label}");
        // EXEC returns only the user's command.
        match one(s, "EXEC").await? {
            StatementResult::Rows { rows, .. } => assert_eq!(rows.len(), 1, "{label}: {rows:?}"),
            other => panic!("{label}: {other:?}"),
        }
        // Nothing was cached from the transaction; browsing works again.
        assert_eq!(
            rs.command_risk("FLUSHALL").await?,
            CommandRisk::Destructive,
            "{label}"
        );
        assert_eq!(
            rs.command_risk("CONFIG SET").await?,
            CommandRisk::Destructive
        );
        let page = rs.scan_keys(0, "*", None, 100).await?;
        assert!(page.keys.len() <= 2, "{label}");
        // DISCARD ends it too.
        run(s, "MULTI").await?;
        assert!(rs.scan_keys(0, "*", None, 10).await.is_err());
        run(s, "DISCARD").await?;
        rs.scan_keys(0, "*", None, 10).await?;
        // A read-only session can open a transaction of reads; its
        // lookups do not end up in it either.
        let mut p = params(&server);
        p.read_only = true;
        let ro = connect(&p).await?;
        run(ro.as_ref(), "MULTI").await?;
        run(ro.as_ref(), "GET present").await?;
        let error = run(ro.as_ref(), "SET x 1").await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::ReadOnly), "{label}");
        match one(ro.as_ref(), "EXEC").await? {
            StatementResult::Rows { rows, .. } => assert_eq!(rows.len(), 1, "{label}: {rows:?}"),
            other => panic!("{label}: {other:?}"),
        }
        ro.close().await?;
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn init_commands_that_break_the_connection_are_refused() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        for init in [
            "MULTI",
            "CLIENT REPLY OFF",
            "QUIT",
            "SUBSCRIBE x",
            "BLPOP q 0",
            "HELLO 3",
        ] {
            let mut p = params(&server);
            p.init_sql = Some(format!("SELECT {TEST_DB}\n{init}"));
            let started = Instant::now();
            let error = connect(&p).await.err();
            let error = error.unwrap_or_else(|| panic!("{label}: {init} accepted"));
            assert!(
                error.message.contains("cannot be an init command"),
                "{label}: {init}: {}",
                error.message
            );
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "{label}: {init}"
            );
        }
        // Read-only sessions: init commands must be allowed read-only.
        let mut p = params(&server);
        p.read_only = true;
        // The whole init allowlist is accepted read-only; anything else is
        // refused with the line.
        // CLIENT NO-EVICT is new in Redis 7.0.
        p.init_sql = Some(if server.major == 6 && !server.valkey {
            "CLIENT SETNAME varve-ro\nSELECT 15".into()
        } else {
            "CLIENT NO-EVICT on\nSELECT 15".into()
        });
        connect(&p).await?.ping().await?;
        p.init_sql = Some("SELECT 15\nGET k".into());
        let error = connect(&p).await.err();
        assert!(
            error
                .as_ref()
                .is_some_and(|e| e.message.starts_with("Init command on line 2:")),
            "{label}: {error:?}"
        );
        // Ordinary init commands still connect (Redis 6.2 lists CLIENT as
        // one @dangerous entry; its SETNAME is not destructive).
        let mut rw = params(&server);
        rw.init_sql = Some("CLIENT SETNAME varve-init\nCLIENT NO-TOUCH ON\nSELECT 15".into());
        if server.major == 6 && !server.valkey {
            // CLIENT NO-TOUCH is new in Redis 7.2.
            rw.init_sql = Some("CLIENT SETNAME varve-init\nSELECT 15".into());
        }
        connect(&rw).await?.ping().await?;
        // Destructive init commands are refused before connecting too.
        for init in ["FLUSHALL", "CLIENT PAUSE 100", "CONFIG SET maxmemory 0"] {
            rw.init_sql = Some(init.into());
            let error = connect(&rw).await.err();
            assert!(
                error
                    .as_ref()
                    .is_some_and(|e| e.message.contains("cannot be an init command")),
                "{label}: {init}: {error:?}"
            );
        }
        p.init_sql = Some("CLIENT SETNAME varve-ro\nSELECT 15".into());
        connect(&p).await?.ping().await?;
        p.init_sql = Some("SELECT 15".into());
        connect(&p).await?.ping().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn a_select_queued_in_multi_counts_only_when_it_ran() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        let rs = redis(s);
        flush_test_db(s).await?;
        let other = connect(&params(&server)).await?;

        // DISCARD: the queued SELECT never ran.
        run(s, "MULTI\nSELECT 3").await?;
        assert_eq!(
            rs.current_db().await,
            TEST_DB,
            "{label}: QUEUED changed the db"
        );
        run(s, "DISCARD").await?;
        assert_eq!(rs.current_db().await, TEST_DB, "{label}");
        run(s, "SET where 15").await?;
        assert_eq!(
            cell(&one(other.as_ref(), "GET where").await?),
            Value::Text("15".into())
        );

        // A queued SELECT of a database that does not exist: the session
        // keeps working, also its side connection lookups and EXEC.
        run(s, "MULTI\nSELECT 99").await?;
        assert_eq!(rs.current_db().await, TEST_DB, "{label}");
        assert_eq!(
            rs.command_risk("ZADD").await?,
            CommandRisk::Write,
            "{label}"
        );
        session.ping().await?;
        let exec = run(s, "EXEC").await;
        assert_eq!(rs.current_db().await, TEST_DB, "{label}: {exec:?}");
        run(s, "GET where").await?;
        session.ping().await?;
        run(s, "MULTI\nSELECT 99\nDISCARD\nGET where").await?;
        assert_eq!(rs.current_db().await, TEST_DB, "{label}");

        // EXEC ran the SELECT: the session follows it.
        run(s, "MULTI\nSELECT 14\nEXEC").await?;
        assert_eq!(rs.current_db().await, 14, "{label}");
        run(s, "SELECT 15").await?;
        assert_eq!(rs.current_db().await, TEST_DB, "{label}");

        // EXEC with an error element for another command: the SELECT ran.
        run(s, "SET str v").await?;
        let _ = run(s, "MULTI\nSELECT 14\nLPUSH str x\nEXEC").await;
        assert_eq!(rs.current_db().await, 14, "{label}");
        run(s, "SELECT 15").await?;

        // EXEC aborted by WATCH (nil): nothing ran.
        run(s, "WATCH where\nMULTI\nSELECT 14").await?;
        run(other.as_ref(), "SET where changed").await?;
        let exec = run(s, "EXEC").await;
        assert_eq!(rs.current_db().await, TEST_DB, "{label}: {exec:?}");

        // EXECABORT (an error while queueing): nothing ran.
        let _ = run(s, "MULTI\nSELECT 14\nNOSUCHCOMMANDX").await;
        let _ = run(s, "EXEC").await;
        assert_eq!(rs.current_db().await, TEST_DB, "{label}");

        // After a reconnect the session is on the tracked database.
        run(s, "QUIT").await?;
        run(s, "SET where after").await?;
        assert_eq!(
            cell(&one(other.as_ref(), "GET where").await?),
            Value::Text("after".into())
        );
        other.close().await?;
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn read_only_sessions_can_delete_their_search_cursors() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers().into_iter().filter(Server::has_builtin_json) {
        let label = server.label.clone();
        let mut p = params(&server);
        // Search indexes live in database 0 only.
        p.redis_db = 0;
        let session = connect(&p).await?;
        let s = session.as_ref();
        let _ = run(s, "FT.DROPINDEX varve_cursor_idx DD").await;
        run(
            s,
            "FT.CREATE varve_cursor_idx ON HASH PREFIX 1 varvecur: SCHEMA n NUMERIC\nHSET varvecur:1 n 1\nHSET varvecur:2 n 2",
        )
        .await?;
        p.read_only = true;
        let ro = connect(&p).await?;
        let reply = one(
            ro.as_ref(),
            "FT.AGGREGATE varve_cursor_idx * WITHCURSOR COUNT 1",
        )
        .await?;
        let cursor = match &reply {
            StatementResult::Rows { rows, .. } => rows.get(1).and_then(|r| match r.get(1) {
                Some(Value::Int(id)) => Some(*id),
                _ => None,
            }),
            _ => None,
        };
        let cursor = cursor.unwrap_or_else(|| panic!("{label}: no cursor in {reply:?}"));
        assert_ne!(cursor, 0, "{label}");
        run(
            ro.as_ref(),
            &format!("FT.CURSOR READ varve_cursor_idx {cursor}"),
        )
        .await?;
        run(
            ro.as_ref(),
            &format!("FT.CURSOR DEL varve_cursor_idx {cursor}"),
        )
        .await
        .or_else(|e| {
            if e.kind == ErrorKind::ReadOnly {
                Err(e)
            } else {
                Ok(Vec::new())
            }
        })?;
        let error = run(ro.as_ref(), "FT.CURSOR GC varve_cursor_idx")
            .await
            .err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::ReadOnly), "{label}");
        ro.close().await?;
        run(s, "FT.DROPINDEX varve_cursor_idx DD").await?;
        session.close().await?;
    }
    Ok(())
}

/// Pauses every client of the server for `ms` milliseconds, so a command
/// sent meanwhile gets its reply only afterwards.
async fn pause_all(other: &dyn Session, ms: u64) -> Result<(), DriverError> {
    run(other, &format!("CLIENT PAUSE {ms} ALL")).await?;
    Ok(())
}

/// Runs `text` and gives the call up after `after` (while the server is
/// paused, so the reply was not read yet).
async fn dropped(s: &dyn Session, text: &str, after: Duration) {
    let outcome = tokio::time::timeout(after, s.execute(text, &ExecOptions::default())).await;
    assert!(
        outcome.is_err(),
        "{text} finished before it was dropped: {outcome:?}"
    );
}

/// The session's tracked database matches the server, no transaction is
/// open, and the browser works.
async fn in_step(label: &str, s: &dyn Session, expected: u16) -> TestResult {
    let rs = redis(s);
    assert_eq!(rs.current_db().await, expected, "{label}: tracked db");
    let info = match cell(&one(s, "CLIENT INFO").await?) {
        Value::Text(text) => text,
        other => panic!("{other:?}"),
    };
    assert!(
        info.contains(&format!(" db={expected} ")),
        "{label}: {info}"
    );
    rs.scan_keys(0, "*", None, 10).await?;
    assert_eq!(
        tag(&one(s, "SET instep 1").await?),
        "OK",
        "{label}: queued?"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn dropped_calls_leave_the_session_in_step() -> TestResult {
    const PAUSE: u64 = 700;
    let after = Duration::from_millis(150);
    let settle = Duration::from_millis(PAUSE + 150);
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        flush_test_db(s).await?;
        let other = connect(&params(&server)).await?;
        let o = other.as_ref();
        // Warm the command lookups, so a dropped call has sent its command.
        for warm in [
            "SELECT 15",
            "MULTI",
            "EXEC",
            "DISCARD",
            "WATCH k",
            "UNWATCH",
            "SET k v",
            "BLPOP k 1",
        ] {
            redis(s).command_risk(warm).await?;
        }

        for (name, script) in [
            ("SELECT", "SELECT 14"),
            ("MULTI", "MULTI"),
            ("WATCH", "WATCH k"),
            ("script", "SET a 1\nSELECT 14\nMULTI\nSET b 2"),
        ] {
            pause_all(o, PAUSE).await?;
            dropped(s, script, after).await;
            tokio::time::sleep(settle).await;
            in_step(&format!("{label} {name}"), s, TEST_DB).await?;
        }

        // EXEC and DISCARD of a block opened before.
        for end in ["EXEC", "DISCARD"] {
            run(s, "MULTI\nSET inblock 1").await?;
            pause_all(o, PAUSE).await?;
            dropped(s, end, after).await;
            tokio::time::sleep(settle).await;
            in_step(&format!("{label} {end}"), s, TEST_DB).await?;
        }

        // Given up during a lookup on the side connection (inside an open
        // block, before anything was sent on the session's connection):
        // the block is still open on both sides.
        run(s, "MULTI\nSET x 1").await?;
        pause_all(o, PAUSE).await?;
        dropped(s, "HINCRBY h f 1", after).await;
        tokio::time::sleep(settle).await;
        assert_eq!(tag(&one(s, "SET y 1").await?), "QUEUED", "{label}");
        let exec = one(s, "EXEC").await?;
        assert!(
            matches!(&exec, StatementResult::Rows { rows, .. } if rows.len() == 2),
            "{label}: {exec:?}"
        );
        in_step(&format!("{label} side lookup"), s, TEST_DB).await?;

        // A WATCH given up (never confirmed) does not abort a later
        // transaction.
        pause_all(o, PAUSE).await?;
        dropped(s, "WATCH k", after).await;
        tokio::time::sleep(settle).await;
        run(o, "SET k changed").await?;
        let results = run(s, "MULTI\nSET k mine\nEXEC").await?;
        assert!(
            matches!(results.last(), Some(StatementResult::Rows { rows, .. }) if rows.len() == 1),
            "{label}: {results:?}"
        );

        // A call given up after its SELECT ran stays on that database.
        let dropped_later = tokio::time::timeout(
            Duration::from_millis(400),
            s.execute("SELECT 14\nBLPOP nokey 2", &ExecOptions::default()),
        )
        .await;
        assert!(dropped_later.is_err(), "{label}");
        in_step(&format!("{label} after SELECT ran"), s, 14).await?;
        run(s, "FLUSHDB\nSELECT 15").await?;

        other.close().await?;
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn a_reconnect_returns_to_the_users_database() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let mut p = params(&server);
        p.init_sql = Some("SELECT 14\nCLIENT SETNAME varve-db-test".into());
        let session = connect(&p).await?;
        let s = session.as_ref();
        let rs = redis(s);
        // The init SELECT decides on the first connect.
        assert_eq!(rs.current_db().await, 14, "{label}");
        run(s, "SELECT 15").await?;
        run(s, "QUIT").await?;
        run(s, "SET where after-quit").await?;
        assert_eq!(rs.current_db().await, TEST_DB, "{label}");
        let other = connect(&params(&server)).await?;
        assert_eq!(
            cell(&one(other.as_ref(), "GET where").await?),
            Value::Text("after-quit".into()),
            "{label}"
        );
        // Also after select_db and a reconnect; the init name still applies.
        rs.select_db(14).await?;
        run(s, "QUIT").await?;
        assert_eq!(rs.current_db().await, 14, "{label}");
        assert!(
            matches!(cell(&one(s, "CLIENT INFO").await?), Value::Text(t) if t.contains(" db=14 ") && t.contains("name=varve-db-test")),
            "{label}"
        );
        run(s, "SELECT 15").await?;
        flush_test_db(other.as_ref()).await?;
        other.close().await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn read_only_profiles_accept_connection_setup_init_commands() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let mut p = params(&server);
        p.read_only = true;
        p.init_sql = Some("CLIENT SETNAME varve-ro-init\nSELECT 2".into());
        let session = connect(&p).await?;
        let s = session.as_ref();
        let check = |when: &str, info: &StatementResult| {
            let text = match cell(info) {
                Value::Text(text) => text,
                other => panic!("{other:?}"),
            };
            assert!(
                text.contains("name=varve-ro-init") && text.contains(" db=2 "),
                "{label} {when}: {text}"
            );
        };
        check("connect", &one(s, "CLIENT INFO").await?);
        assert_eq!(redis(s).current_db().await, 2, "{label}");
        // Writes are still refused in the console.
        let error = run(s, "SET k v").await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::ReadOnly), "{label}");
        // A transparent reconnect applies the init commands again.
        run(s, "QUIT").await?;
        check("reconnect", &one(s, "CLIENT INFO").await?);
        session.close().await?;
    }
    Ok(())
}

/// The code of an error, or the error itself when it is not one.
fn code_of(result: Result<Vec<StatementResult>, DriverError>) -> Option<String> {
    result.err().and_then(|e| e.code)
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn a_reset_connection_does_not_commit_a_lost_transaction() -> TestResult {
    const PAUSE: u64 = 700;
    let after = Duration::from_millis(150);
    let settle = Duration::from_millis(PAUSE + 150);
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        flush_test_db(s).await?;
        let other = connect(&params(&server)).await?;
        let o = other.as_ref();
        for warm in ["WATCH k", "GET k", "MULTI", "EXEC", "SET k v", "DISCARD"] {
            redis(s).command_risk(warm).await?;
        }
        run(s, "SET k original").await?;

        // A confirmed WATCH, then the connection is replaced (a dropped
        // call): EXEC is refused without being sent, nothing is written.
        run(s, "WATCH k").await?;
        pause_all(o, PAUSE).await?;
        dropped(s, "GET k", after).await;
        tokio::time::sleep(settle).await;
        run(o, "SET k changed").await?;
        let error = run(s, "MULTI\nSET k mine\nEXEC").await.err();
        let error = error.unwrap_or_else(|| panic!("{label}: EXEC committed a lost WATCH"));
        assert_eq!(error.code.as_deref(), Some("TRANSACTION_RESET"), "{label}");
        assert!(
            error.message.contains("the WATCH was lost"),
            "{label}: {}",
            error.message
        );
        assert_eq!(cell(&one(o, "GET k").await?), Value::Text("changed".into()));
        // Running it again works.
        run(s, "WATCH k\nMULTI\nSET k mine\nEXEC").await?;
        assert_eq!(cell(&one(o, "GET k").await?), Value::Text("mine".into()));

        // Same after a statement timeout, with the block opened in its own
        // call: the queued commands are discarded with the refused EXEC.
        let mut p = params(&server);
        p.statement_timeout = Some(Duration::from_millis(300));
        let timed = connect(&p).await?;
        let t = timed.as_ref();
        run(t, "WATCH k").await?;
        let error = run(t, "BLPOP nokey 2").await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Timeout), "{label}");
        run(t, "MULTI").await?;
        assert_eq!(tag(&one(t, "SET k lost").await?), "QUEUED", "{label}");
        let error = run(t, "EXEC").await.err();
        assert!(
            error
                .as_ref()
                .is_some_and(|e| e.code.as_deref() == Some("TRANSACTION_RESET")
                    && e.message.contains("were discarded")),
            "{label}: {error:?}"
        );
        assert_eq!(cell(&one(o, "GET k").await?), Value::Text("mine".into()));
        assert_eq!(
            tag(&one(t, "SET k after").await?),
            "OK",
            "{label}: block still open"
        );
        timed.close().await?;

        // An open MULTI block lost with the connection: commands meant for
        // it are refused (they would run at once), EXEC too.
        run(s, "MULTI\nSET k queued").await?;
        pause_all(o, PAUSE).await?;
        dropped(s, "SET k queued2", after).await;
        tokio::time::sleep(settle).await;
        assert_eq!(
            code_of(run(s, "SET k direct").await).as_deref(),
            Some("TRANSACTION_RESET")
        );
        assert_eq!(
            code_of(run(s, "EXEC").await).as_deref(),
            Some("TRANSACTION_RESET")
        );
        assert_eq!(
            cell(&one(s, "GET k").await?),
            Value::Text("after".into()),
            "{label}"
        );
        // DISCARD after a lost block is answered without being sent, and
        // the session continues.
        run(s, "MULTI").await?;
        pause_all(o, PAUSE).await?;
        dropped(s, "SET k queued", after).await;
        tokio::time::sleep(settle).await;
        let output = s.execute("DISCARD", &ExecOptions::default()).await?;
        assert_eq!(tag(&output.results[0]), "OK", "{label}");
        assert!(
            output
                .notices
                .iter()
                .any(|n| n.message.contains("DISCARD was not sent")),
            "{label}: {output:?}"
        );
        assert_eq!(tag(&one(s, "SET k fine").await?), "OK", "{label}");

        // A WATCH and a block lost together: neither MULTI nor DISCARD
        // brings the WATCH back, only WATCH or UNWATCH do.
        run(s, "SET k before").await?;
        run(s, "WATCH k").await?;
        run(s, "MULTI").await?;
        pause_all(o, PAUSE).await?;
        dropped(s, "SET k queued", after).await;
        tokio::time::sleep(settle).await;
        run(o, "SET k concurrent").await?;
        // DISCARD first: answered locally, and it says the WATCH is gone.
        let output = s.execute("DISCARD", &ExecOptions::default()).await?;
        assert!(
            output
                .notices
                .iter()
                .any(|n| n.message.contains("The WATCH was lost")),
            "{label}: {output:?}"
        );
        // MULTI..EXEC is still refused, with what to do.
        let error = run(s, "MULTI\nSET k mine\nEXEC").await.err();
        assert!(
            error
                .as_ref()
                .is_some_and(|e| e.code.as_deref() == Some("TRANSACTION_RESET")
                    && e.message.contains("Run WATCH again (or UNWATCH)")),
            "{label}: DISCARD brought the WATCH back: {error:?}"
        );
        // A WATCH that fails (no key) does not count either.
        assert!(run(s, "WATCH").await.is_err());
        assert_eq!(
            code_of(run(s, "MULTI\nSET k mine\nEXEC").await).as_deref(),
            Some("TRANSACTION_RESET"),
            "{label}: a failed WATCH brought the WATCH back"
        );
        assert_eq!(
            cell(&one(o, "GET k").await?),
            Value::Text("concurrent".into())
        );
        // UNWATCH is an explicit choice: the transaction then runs.
        run(s, "UNWATCH\nMULTI\nSET k mine\nEXEC").await?;
        assert_eq!(cell(&one(o, "GET k").await?), Value::Text("mine".into()));

        // Lost in two separate reconnects: first the WATCH, then a block
        // opened on the new connection.
        run(s, "WATCH k").await?;
        pause_all(o, PAUSE).await?;
        dropped(s, "GET k", after).await;
        tokio::time::sleep(settle).await;
        run(s, "MULTI").await?;
        pause_all(o, PAUSE).await?;
        dropped(s, "SET k queued", after).await;
        tokio::time::sleep(settle).await;
        run(o, "SET k concurrent2").await?;
        assert_eq!(
            code_of(run(s, "MULTI\nSET k mine\nEXEC").await).as_deref(),
            Some("TRANSACTION_RESET"),
            "{label}"
        );
        assert_eq!(
            cell(&one(o, "GET k").await?),
            Value::Text("concurrent2".into())
        );
        run(s, "WATCH k\nMULTI\nSET k mine\nEXEC").await?;

        // EXEC or DISCARD without MULTI that gets no answer (timeout) leaves
        // the WATCH in place: the reset records it as lost.
        for end in ["EXEC", "DISCARD"] {
            let mut p = params(&server);
            p.statement_timeout = Some(Duration::from_millis(300));
            let timed = connect(&p).await?;
            let t = timed.as_ref();
            for warm in ["WATCH k", "EXEC", "DISCARD", "MULTI", "SET k v", "GET k"] {
                redis(t).command_risk(warm).await?;
            }
            run(t, "WATCH k").await?;
            pause_all(o, PAUSE).await?;
            let error = run(t, end).await.err();
            assert_eq!(
                error.map(|e| e.kind),
                Some(ErrorKind::Timeout),
                "{label}: {end}"
            );
            tokio::time::sleep(settle).await;
            run(o, &format!("SET k changed-{end}")).await?;
            assert_eq!(
                code_of(run(t, "MULTI\nSET k mine\nEXEC").await).as_deref(),
                Some("TRANSACTION_RESET"),
                "{label}: {end} without an answer forgot the WATCH"
            );
            assert_eq!(
                cell(&one(o, "GET k").await?),
                Value::Text(format!("changed-{end}")),
                "{label}"
            );
            timed.close().await?;
        }

        // EXEC without MULTI fails but keeps the server's WATCH: it is
        // still recorded as lost when the connection is reset.
        run(s, "WATCH k").await?;
        assert!(run(s, "EXEC").await.is_err());
        pause_all(o, PAUSE).await?;
        dropped(s, "GET k", after).await;
        tokio::time::sleep(settle).await;
        run(o, "SET k concurrent3").await?;
        assert_eq!(
            code_of(run(s, "MULTI\nSET k mine\nEXEC").await).as_deref(),
            Some("TRANSACTION_RESET"),
            "{label}: EXEC without MULTI forgot the WATCH"
        );
        run(s, "UNWATCH").await?;

        other.close().await?;
        flush_test_db(s).await?;
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn init_and_console_edges() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = server.label.clone();
        let session = connect(&params(&server)).await?;
        let s = session.as_ref();
        // HELLO without a version gets the right reason.
        let error = run(s, "HELLO AUTH default x").await.err();
        assert!(
            error
                .as_ref()
                .is_some_and(|e| e.kind == ErrorKind::Unsupported
                    && e.message.contains("needs the protocol version")),
            "{label}: {error:?}"
        );
        // Database numbers above 65535 are refused before sending.
        let error = run(s, "SELECT 70000").await.err();
        assert_eq!(
            error.map(|e| e.kind),
            Some(ErrorKind::Unsupported),
            "{label}"
        );
        assert_eq!(redis(s).current_db().await, TEST_DB);
        let mut p = params(&server);
        for (init, reason) in [
            ("SELECT 70000", "outside 0 to 65535"),
            ("AUTH varve_dev", "puts a password"),
            ("HELLO 2 AUTH default varve_dev", "puts a password"),
        ] {
            p.init_sql = Some(init.into());
            let error = connect(&p).await.err();
            assert!(
                error.as_ref().is_some_and(|e| e.message.contains(reason)),
                "{label}: {init}: {error:?}"
            );
        }
        // Fail closed: a user who may not run COMMAND cannot have init
        // commands checked by their real name.
        run(
            s,
            "ACL SETUSER varve_nocommand reset on >nocommand_secret ~* &* +@all -command",
        )
        .await?;
        let mut user = params(&server);
        user.username = Some("varve_nocommand".into());
        user.password = Some(Secret::new("nocommand_secret".into()));
        user.init_sql = Some("CLIENT SETNAME blind".into());
        let error = connect(&user).await.err();
        assert!(
            error
                .as_ref()
                .is_some_and(|e| e.message.contains("cannot be checked")),
            "{label}: {error:?}"
        );
        user.init_sql = None;
        connect(&user).await?.ping().await?;
        run(s, "ACL DELUSER varve_nocommand").await?;
        session.close().await?;
    }
    Ok(())
}
