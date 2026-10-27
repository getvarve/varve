//! Redis live panels against the M2 matrix: Redis 6.2, Redis 8.10 and
//! Valkey 9. Uses databases 10 to 13 only (flushed with `FLUSHDB`), restores
//! every `CONFIG SET` it makes and never runs `CONFIG REWRITE`.
//!
//! Endpoints: `VARVE_M2_REDIS_SERVERS` (`name=host:port,...`, default the
//! matrix ports) and `VARVE_M2_REDIS_PASSWORD` (default `varve_dev`).

use std::time::Duration;

use std::io::{Read, Write};
use std::net::TcpStream;
use varve_catalog::CatalogError;
use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, ErrorKind, ExecOptions, ServerInfo, Session, Value};
use varve_driver_redis::RedisDriver;
use varve_live_stats::redis::{
    self, ConfigChange, Flavor, MemoryScanOptions, ScanStop, ServerMode,
};
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const DB_MAIN: u16 = 10;
const DB_EMPTY: u16 = 11;
const DB_BIG: u16 = 13;

/// The tests share the servers' databases (FLUSHDB, keyspace counts over
/// every database), so they run one at a time.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// The memory Stop test's own database (tests run in parallel).
const DB_STOP: u16 = 9;
const PREFIX: &str = "m2_redislive";

struct Server {
    name: String,
    host: String,
    port: u16,
}

fn servers() -> Vec<Server> {
    let list = std::env::var("VARVE_M2_REDIS_SERVERS").unwrap_or_else(|_| {
        "redis-6.2=127.0.0.1:64062,redis-8.10=127.0.0.1:64081,valkey-9=127.0.0.1:64190".into()
    });
    list.split(',')
        .filter_map(|entry| {
            let (name, endpoint) = entry.split_once('=')?;
            let (host, port) = endpoint.rsplit_once(':')?;
            Some(Server {
                name: name.trim().into(),
                host: host.into(),
                port: port.parse().ok()?,
            })
        })
        .collect()
}

fn password() -> String {
    std::env::var("VARVE_M2_REDIS_PASSWORD").unwrap_or_else(|_| "varve_dev".into())
}

fn params(server: &Server, db: u16) -> ConnectParams {
    let mut p = ConnectParams::new(Engine::Redis, server.host.clone(), server.port);
    p.password = Some(Secret::new(password()));
    p.redis_db = db;
    p.application_name = format!("{PREFIX}_test");
    p
}

async fn connect(server: &Server, db: u16) -> Result<Box<dyn Session>, Box<dyn std::error::Error>> {
    Ok(RedisDriver::new().connect(&params(server, db)).await?)
}

async fn exec(session: &dyn Session, line: &str) -> Result<(), Box<dyn std::error::Error>> {
    session.execute(line, &ExecOptions::default()).await?;
    Ok(())
}

fn info_of(session: &dyn Session) -> ServerInfo {
    session.server_info().clone()
}

/// Runs `body` for every server, flushing the test databases before and
/// after (also when the body fails).
async fn each_server<F>(body: F) -> TestResult
where
    F: for<'a> Fn(
        &'a Server,
        &'a dyn Session,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TestResult> + 'a>>,
{
    for server in servers() {
        let session = connect(&server, DB_MAIN).await?;
        exec(&*session, "FLUSHDB").await?;
        let outcome = body(&server, &*session).await;
        exec(&*session, "FLUSHDB").await?;
        outcome.map_err(|e| format!("{}: {e}", server.name))?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn server_info_and_databases() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    each_server(|server, s| {
        Box::pin(async move {
            let info = info_of(s);
            let report = redis::server_info(s, &info).await?;
            let expected = if server.name.starts_with("valkey") {
                Flavor::Valkey
            } else {
                Flavor::Redis
            };
            assert_eq!(report.flavor, expected);
            assert_eq!(report.mode, ServerMode::Standalone);
            assert!(report.memory.used_memory.is_some_and(|m| m > 0));
            assert!(!report.commandstats.is_empty());
            assert!(report.stats.total_commands_processed.is_some());

            exec(s, &format!("SET {PREFIX}:a 1")).await?;
            exec(s, &format!("SET {PREFIX}:b 2 EX 1000")).await?;
            let empty = connect(server, DB_EMPTY).await?;
            exec(&*empty, "FLUSHDB").await?;
            let dbs = redis::databases(s, &info).await?;
            assert_eq!(dbs.mode, ServerMode::Standalone);
            assert_eq!(dbs.count, Some(16));
            assert_eq!(dbs.databases.len(), 16);
            let main = &dbs.databases[usize::from(DB_MAIN)];
            assert_eq!((main.keys, main.expires), (2, 1));
            assert_eq!(dbs.databases[usize::from(DB_EMPTY)].keys, 0);
            assert_eq!(redis::cluster_topology(s, &info).await?, None);
            assert_eq!(redis::sentinel_topology(s, &info).await?, None);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn memory_analysis() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    each_server(|_server, s| {
        Box::pin(async move {
            let info = info_of(s);
            for i in 0..40 {
                exec(s, &format!("HSET {PREFIX}:user:{i} name n{i} email e{i}")).await?;
            }
            for i in 0..15 {
                exec(s, &format!("SET {PREFIX}:session:{i} token EX 600")).await?;
            }
            let items: Vec<String> = (0..500).map(|i| format!("item-{i}")).collect();
            exec(s, &format!("RPUSH {PREFIX}:queue:big {}", items.join(" "))).await?;
            exec(s, &format!("XADD {PREFIX}:events * kind login")).await?;
            exec(s, &format!("SET \"{PREFIX}:bin:\\xff\\x00\" raw")).await?;
            exec(s, "SET \"plain key with space\" v").await?;

            let options = MemoryScanOptions {
                depth: 2,
                ..MemoryScanOptions::default()
            };
            let analysis = redis::analyze_memory(s, &info, &options).await?;
            assert!(analysis.complete);
            assert_eq!(analysis.stopped_by, ScanStop::Complete);
            assert_eq!(analysis.scanned_keys, 59);
            assert_eq!(analysis.db_size, 59);
            let users = analysis
                .prefixes
                .iter()
                .find(|p| p.prefix == Some(Value::Text(format!("{PREFIX}:user:"))))
                .ok_or("no user prefix")?;
            assert_eq!(users.keys, 40);
            assert_eq!(users.estimated_bytes, Some(users.bytes));
            assert!(
                analysis
                    .prefixes
                    .iter()
                    .any(|p| p.prefix.is_none() && p.keys == 1),
                "the key without delimiter"
            );
            assert!(analysis.prefixes.iter().any(|p| matches!(
                &p.max_key,
                Value::Bytes(b) if b.ends_with(&[0xff, 0x00])
            )));
            let lists = analysis
                .types
                .iter()
                .find(|t| t.key_type == "list")
                .ok_or("no list type")?;
            assert_eq!(lists.biggest[0].length, Some(500));
            let hashes = analysis
                .types
                .iter()
                .find(|t| t.key_type == "hash")
                .ok_or("no hash type")?;
            assert_eq!(hashes.keys, 40);
            assert_eq!(hashes.biggest.len(), 10);
            assert_eq!(hashes.biggest[0].length, Some(2));
            let streams = analysis
                .types
                .iter()
                .find(|t| t.key_type == "stream")
                .ok_or("no stream")?;
            assert_eq!(streams.biggest[0].length, Some(1));

            let partial = redis::analyze_memory(
                s,
                &info,
                &MemoryScanOptions {
                    max_keys: 5,
                    scan_count: 5,
                    pattern: Some(format!("{PREFIX}:*")),
                    ..MemoryScanOptions::default()
                },
            )
            .await?;
            assert!(!partial.complete);
            assert_eq!(partial.stopped_by, ScanStop::KeyBudget);
            assert_eq!(partial.scanned_keys, 5);
            assert_eq!(
                partial.prefixes[0].estimated_bytes, None,
                "no estimate for an incomplete scan with a pattern"
            );
            let unpatterned = redis::analyze_memory(
                s,
                &info,
                &MemoryScanOptions {
                    max_keys: 5,
                    scan_count: 5,
                    ..MemoryScanOptions::default()
                },
            )
            .await?;
            assert!(!unpatterned.complete);
            assert!(
                unpatterned.prefixes[0]
                    .estimated_bytes
                    .is_some_and(|e| e > unpatterned.prefixes[0].bytes)
            );
            assert!(matches!(
                redis::analyze_memory(
                    s,
                    &info,
                    &MemoryScanOptions {
                        max_keys: 0,
                        ..MemoryScanOptions::default()
                    }
                )
                .await,
                Err(CatalogError::Invalid { .. })
            ));

            let stats = redis::memory_stats(s, &info).await?;
            assert!(stats.databases.iter().any(|d| d.db == u32::from(DB_MAIN)));
            assert!(!redis::memory_doctor(s, &info).await?.is_empty());
            let size =
                redis::key_memory(s, &info, format!("{PREFIX}:queue:big").as_bytes(), 5).await?;
            assert!(size.is_some_and(|n| n > 1000));
            assert_eq!(redis::key_memory(s, &info, b"nope", 5).await?, None);
            Ok(())
        })
    })
    .await
}

/// Reads one configuration value straight from the server.
async fn config_now(s: &dyn Session, name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let entries = redis::config_get(s, &info_of(s), name).await?;
    entries
        .into_iter()
        .find(|e| e.name == name)
        .and_then(|e| e.value)
        .ok_or_else(|| format!("no {name}").into())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn slow_log_and_latency() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    each_server(|_server, s| {
        Box::pin(async move {
            let info = info_of(s);
            let slower = config_now(s, "slowlog-log-slower-than").await?;
            let threshold = config_now(s, "latency-monitor-threshold").await?;
            let outcome: TestResult = async {
                exec(s, "CONFIG SET slowlog-log-slower-than 0").await?;
                exec(s, "CONFIG SET latency-monitor-threshold 1").await?;
                exec(s, &format!("ECHO {PREFIX}_marker")).await?;
                exec(
                    s,
                    "EVAL \"local i=0 while i<3000000 do i=i+1 end return i\" 0",
                )
                .await?;
                let log = redis::slowlog(s, &info, 50).await?;
                assert_eq!(log.threshold_us, Some(0));
                assert!(log.max_len.is_some());
                assert!(log.length >= 2);
                let echo = log
                    .entries
                    .iter()
                    .find(|e| e.args.get(1) == Some(&Value::Text(format!("{PREFIX}_marker"))))
                    .ok_or("ECHO not in the slow log")?;
                assert!(
                    echo.client_name
                        .as_deref()
                        .is_some_and(|n| n.starts_with(PREFIX))
                );
                let config_entry = log.entries.iter().find(|e| {
                    e.args
                        .iter()
                        .any(|a| a == &Value::Text("latency-monitor-threshold".into()))
                });
                assert!(config_entry.is_some());

                let latency = redis::latency(s, &info).await?;
                assert_eq!(latency.threshold_ms, Some(1));
                let event = latency
                    .events
                    .iter()
                    .find(|e| e.event == "command")
                    .ok_or("no command latency event")?;
                assert!(event.max_ms >= 1);
                let history = redis::latency_history(s, &info, "command").await?;
                assert!(!history.is_empty());
                assert!(!redis::latency_doctor(s, &info).await?.is_empty());
                assert!(redis::reset_latency(s, &info, &["command".into()]).await? >= 1);
                assert!(
                    redis::latency_history(s, &info, "command")
                        .await?
                        .is_empty()
                );
                assert!(matches!(
                    redis::latency_history(s, &info, "two words").await,
                    Err(CatalogError::Invalid { .. })
                ));
                Ok(())
            }
            .await;
            exec(s, &format!("CONFIG SET slowlog-log-slower-than {slower}")).await?;
            exec(
                s,
                &format!("CONFIG SET latency-monitor-threshold {threshold}"),
            )
            .await?;
            outcome?;
            assert_eq!(config_now(s, "slowlog-log-slower-than").await?, slower);
            let before = redis::slowlog(s, &info, 1).await?.length;
            redis::reset_slowlog(s, &info).await?;
            assert!(redis::slowlog(s, &info, 1).await?.length <= before);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn client_list_and_kill() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    each_server(|server, s| {
        Box::pin(async move {
            let info = info_of(s);
            let other = connect(server, DB_EMPTY + 1).await?;
            exec(&*other, &format!("CLIENT SETNAME {PREFIX}_victim")).await?;
            let list = redis::client_list(s, &info).await?;
            let me = list.iter().filter(|c| c.is_self).collect::<Vec<_>>();
            assert_eq!(me.len(), 1);
            assert_eq!(me[0].db, Some(i64::from(DB_MAIN)));
            let victim = list
                .iter()
                .find(|c| c.name.as_deref() == Some(&format!("{PREFIX}_victim")))
                .ok_or("victim not listed")?;
            assert!(!victim.is_self);
            assert_eq!(victim.db, Some(i64::from(DB_EMPTY + 1)));

            assert!(matches!(
                redis::kill_client(s, &info, me[0].id).await,
                Err(CatalogError::Invalid { .. })
            ));
            redis::kill_client(s, &info, victim.id).await?;
            let after = redis::client_list(s, &info).await?;
            assert!(after.iter().all(|c| c.id != victim.id));
            assert!(matches!(
                redis::kill_client(s, &info, victim.id).await,
                Err(CatalogError::NotFound { .. })
            ));

            let mut ro = params(server, DB_MAIN);
            ro.read_only = true;
            let ro = RedisDriver::new().connect(&ro).await?;
            assert!(!redis::client_list(&*ro, &info).await?.is_empty());
            match redis::kill_client(&*ro, &info, me[0].id).await {
                Err(CatalogError::Driver { error }) => assert_eq!(error.kind, ErrorKind::ReadOnly),
                other => return Err(format!("read-only kill: {other:?}").into()),
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn config_get_set_and_rewrite_plans() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    each_server(|_server, s| {
        Box::pin(async move {
            let info = info_of(s);
            let all = redis::config_get(s, &info, "*").await?;
            let json = serde_json::to_string(&all)?;
            assert!(
                !json.contains(&password()),
                "the password must never be returned"
            );
            let pass = all
                .iter()
                .find(|e| e.name == "requirepass")
                .ok_or("no requirepass")?;
            assert!(pass.sensitive && pass.has_value && pass.value.is_none());

            let samples = config_now(s, "maxmemory-samples").await?;
            let max_len = config_now(s, "slowlog-max-len").await?;
            let changes = [
                ConfigChange {
                    name: "maxmemory-samples".into(),
                    value: "7".into(),
                },
                ConfigChange {
                    name: "slowlog-max-len".into(),
                    value: "129".into(),
                },
            ];
            let plan = redis::plan_config_set(s, &info, &changes).await?;
            assert_eq!(plan.commands.len(), if info.at_least(7, 0) { 1 } else { 2 });
            assert!(!plan.destructive);
            let applied = redis::apply_plan(s, &info, &plan).await;
            let restore = redis::plan_config_set(
                s,
                &info,
                &[
                    ConfigChange {
                        name: "maxmemory-samples".into(),
                        value: samples.clone(),
                    },
                    ConfigChange {
                        name: "slowlog-max-len".into(),
                        value: max_len.clone(),
                    },
                ],
            )
            .await?;
            let read_back = (
                config_now(s, "maxmemory-samples").await?,
                config_now(s, "slowlog-max-len").await?,
            );
            redis::apply_plan(s, &info, &restore).await?;
            applied?;
            assert_eq!(read_back, ("7".to_string(), "129".to_string()));
            assert_eq!(config_now(s, "maxmemory-samples").await?, samples);
            assert_eq!(config_now(s, "slowlog-max-len").await?, max_len);

            // A server-side rejection is reported with the failing command.
            let bad = redis::CommandPlan {
                commands: vec!["CONFIG SET maxmemory-samples notanumber".into()],
                display: Vec::new(),
                destructive: false,
                sensitive: false,
                warnings: Vec::new(),
            };
            match redis::apply_plan(s, &info, &bad).await {
                Err(CatalogError::Driver { error }) => {
                    assert_eq!(error.detail.as_deref(), Some("Command 1 of 1"));
                }
                other => return Err(format!("bad plan: {other:?}").into()),
            }
            assert_eq!(config_now(s, "maxmemory-samples").await?, samples);

            let secret = redis::plan_config_set(
                s,
                &info,
                &[ConfigChange {
                    name: "requirepass".into(),
                    value: "never applied".into(),
                }],
            )
            .await?;
            assert!(secret.sensitive && secret.destructive);
            assert!(secret.display.iter().all(|l| !l.contains("never applied")));
            assert!(matches!(
                redis::plan_config_set(
                    s,
                    &info,
                    &[ConfigChange {
                        name: "databases".into(),
                        value: "4".into()
                    }]
                )
                .await,
                Err(CatalogError::Invalid { .. })
            ));
            // The matrix servers run without a configuration file.
            assert!(matches!(
                redis::plan_config_rewrite(s, &info).await,
                Err(CatalogError::Invalid { .. })
            ));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn functions_and_scripts() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    each_server(|_server, s| {
        Box::pin(async move {
            let info = info_of(s);
            let library = format!("{PREFIX}_lib");
            let code = format!(
                "#!lua name={library}\nredis.register_function{{function_name='{PREFIX}_ro', \
                 callback=function(keys, args) return \"it's\" end, flags={{'no-writes'}}, \
                 description='read only'}}"
            );
            if !info.at_least(7, 0) {
                assert!(matches!(
                    redis::function_list(s, &info, None, false).await,
                    Err(CatalogError::Unsupported { .. })
                ));
                assert!(redis::function_load_plan(&info, &code, false).is_err());
            } else {
                let _ = s
                    .execute(
                        &format!("FUNCTION DELETE {library}"),
                        &ExecOptions::default(),
                    )
                    .await;
                let load = redis::function_load_plan(&info, &code, false)?;
                let replies = redis::apply_plan(s, &info, &load).await?;
                assert_eq!(replies, [Value::Text(library.clone())]);
                let libs =
                    redis::function_list(s, &info, Some(&format!("{PREFIX}*")), true).await?;
                assert_eq!(libs.len(), 1);
                assert_eq!(
                    libs[0].code.as_deref(),
                    Some(code.as_str()),
                    "code round trips"
                );
                assert_eq!(libs[0].functions[0].flags, ["no-writes"]);
                let replace = redis::function_load_plan(&info, &code, true)?;
                redis::apply_plan(s, &info, &replace).await?;
                let delete = redis::function_delete_plan(&info, &library)?;
                redis::apply_plan(s, &info, &delete).await?;
                assert!(
                    redis::function_list(s, &info, Some(&format!("{PREFIX}*")), false)
                        .await?
                        .is_empty()
                );
            }

            let output = s
                .execute(
                    &format!("SCRIPT LOAD \"return '{PREFIX}'\""),
                    &ExecOptions::default(),
                )
                .await?;
            let sha = match output.results.first() {
                Some(varve_driver::StatementResult::Rows { rows, .. }) => {
                    rows[0][0].to_plain_text().unwrap_or_default()
                }
                _ => return Err("no SHA".into()),
            };
            let missing = "f".repeat(40);
            let presence = redis::script_exists(s, &info, &[sha.to_uppercase(), missing]).await?;
            assert!(presence[0].exists);
            assert_eq!(presence[0].sha, sha);
            assert!(!presence[1].exists);
            assert!(matches!(
                redis::script_exists(s, &info, &["xyz".into()]).await,
                Err(CatalogError::Invalid { .. })
            ));
            Ok(())
        })
    })
    .await
}

/// A raw subscriber connection (the driver has no subscriber mode). Plain
/// blocking I/O: the dev tokio features have no networking, and the
/// exchange is a few small packets.
fn subscriber(
    server: &Server,
    commands: &[&[&str]],
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let mut stream = TcpStream::connect((server.host.as_str(), server.port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let pass = password();
    let mut all: Vec<Vec<&str>> = vec![vec!["AUTH", pass.as_str()]];
    all.extend(commands.iter().map(|c| c.to_vec()));
    let mut wire = Vec::new();
    for command in &all {
        wire.extend(format!("*{}\r\n", command.len()).into_bytes());
        for arg in command {
            wire.extend(format!("${}\r\n{arg}\r\n", arg.len()).into_bytes());
        }
    }
    stream.write_all(&wire)?;
    // Wait for the subscribe confirmations.
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    let expected = commands.len();
    while seen
        .windows(9)
        .filter(|w| w.ends_with(b"subscribe"))
        .count()
        < expected
    {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Err("subscriber closed".into());
        }
        seen.extend_from_slice(&buf[..n]);
    }
    Ok(stream)
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn pubsub_snapshot() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    each_server(|server, s| {
        Box::pin(async move {
            let info = info_of(s);
            let channel = format!("{PREFIX}:news");
            let shard = format!("{PREFIX}:shard");
            let pattern = format!("{PREFIX}:p*");
            let mut commands: Vec<&[&str]> = Vec::new();
            let sub = ["SUBSCRIBE", channel.as_str()];
            let psub = ["PSUBSCRIBE", pattern.as_str()];
            let ssub = ["SSUBSCRIBE", shard.as_str()];
            commands.push(&sub);
            commands.push(&psub);
            if info.at_least(7, 0) {
                commands.push(&ssub);
            }
            let _first = subscriber(server, &commands)?;
            let _second = subscriber(server, &[&sub])?;
            let snapshot =
                redis::pubsub_snapshot(s, &info, Some(&format!("{PREFIX}:*")), 100).await?;
            assert!(!snapshot.truncated);
            assert_eq!(snapshot.channels.len(), 1);
            assert_eq!(snapshot.channels[0].channel, Value::Text(channel.clone()));
            assert_eq!(snapshot.channels[0].subscribers, 2);
            assert!(snapshot.pattern_subscriptions >= 1);
            if info.at_least(7, 0) {
                let shards = snapshot.shard_channels.ok_or("no shard channels")?;
                assert_eq!(shards.len(), 1);
                assert_eq!(shards[0].subscribers, 1);
            } else {
                assert_eq!(snapshot.shard_channels, None);
            }
            let limited = redis::pubsub_snapshot(s, &info, Some(&format!("{PREFIX}:*")), 0).await?;
            assert!(limited.truncated && limited.channels.is_empty());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn memory_analysis_time_budget_bounds_the_work() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let s = connect(&server, DB_BIG).await?;
        exec(&*s, "FLUSHDB").await?;
        let outcome: TestResult = async {
            let info = info_of(&*s);
            // 200,000 keys over 5,000 prefixes, written by four scripts.
            for part in 0..4 {
                exec(
                    &*s,
                    &format!(
                        "EVAL \"for i={from},{to} do redis.call('SET', '{PREFIX}:p' .. (i % 5000) .. ':' .. i, 'v') end return 1\" 0",
                        from = part * 50_000 + 1,
                        to = (part + 1) * 50_000
                    ),
                )
                .await?;
            }
            for scan_count in [10, 1_000, 1_000_000] {
                let started = std::time::Instant::now();
                let analysis = redis::analyze_memory(
                    &*s,
                    &info,
                    &MemoryScanOptions {
                        depth: 2,
                        max_keys: 1_000_000,
                        time_budget_ms: 500,
                        scan_count,
                        ..MemoryScanOptions::default()
                    },
                )
                .await?;
                let wall = started.elapsed();
                assert!(
                    wall < Duration::from_millis(1_200),
                    "{}: scan_count {scan_count} took {wall:?}",
                    server.name
                );
                assert_eq!(analysis.db_size, 200_000);
                assert!(!analysis.complete);
                assert_eq!(analysis.stopped_by, ScanStop::TimeBudget);
                assert!(analysis.scanned_keys > 0);
                assert!(analysis.prefixes.len() <= 100);
                if let Some(other) = &analysis.other_prefixes {
                    assert_eq!(
                        analysis.prefixes.iter().map(|p| p.keys).sum::<u64>() + other.keys,
                        analysis.scanned_keys
                    );
                }
                assert!(analysis.prefixes[0].estimated_bytes.is_some());
            }
            Ok(())
        }
        .await;
        exec(&*s, "FLUSHDB").await?;
        outcome.map_err(|e| format!("{}: {e}", server.name))?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn memory_analysis_stops_promptly_and_keeps_partial_results() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    for server in servers() {
        let s = connect(&server, DB_STOP).await?;
        exec(&*s, "FLUSHDB").await?;
        let outcome: TestResult = async {
            let info = info_of(&*s);
            exec(
                &*s,
                &format!(
                    "EVAL \"for i=1,50000 do redis.call('SET', '{PREFIX}:s' .. (i % 50) .. ':' .. i, 'v') end return 1\" 0"
                ),
            )
            .await?;
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let stopper = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                flag.store(true, Ordering::SeqCst);
            });
            let started = std::time::Instant::now();
            let analysis = redis::analyze_memory_until(
                &*s,
                &info,
                &MemoryScanOptions {
                    max_keys: 1_000_000,
                    time_budget_ms: 60_000,
                    scan_count: 100,
                    ..MemoryScanOptions::default()
                },
                &stop,
            )
            .await?;
            let wall = started.elapsed();
            stopper.await?;
            assert_eq!(analysis.stopped_by, ScanStop::Stopped, "{}", server.name);
            assert!(!analysis.complete);
            assert!(
                wall < Duration::from_millis(2_000),
                "{}: the stop took {wall:?}",
                server.name
            );
            assert!(analysis.scanned_keys > 0 && analysis.scanned_keys < 50_000);
            assert_eq!(analysis.db_size, 50_000);
            assert!(!analysis.prefixes.is_empty());
            assert_eq!(
                analysis.types.iter().map(|t| t.keys).sum::<u64>(),
                analysis.scanned_keys
            );
            // A stop set before the start ends it at once.
            let early = redis::analyze_memory_until(
                &*s,
                &info,
                &MemoryScanOptions::default(),
                &AtomicBool::new(true),
            )
            .await?;
            assert_eq!(early.stopped_by, ScanStop::Stopped);
            assert_eq!(early.scanned_keys, 0);
            Ok(())
        }
        .await;
        exec(&*s, "FLUSHDB").await?;
        outcome.map_err(|e| format!("{}: {e}", server.name))?;
    }
    Ok(())
}

/// Where the generated refusal tables live.
const REFUSALS_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/redis/config_refusals.rs");

/// Matrix servers the refusal tables come from: server name, flavor, the
/// lowest version a table applies to, and its constant name.
const REFUSAL_SOURCES: [(&str, &str, (u32, u32), &str); 3] = [
    ("redis-6.2", "redis", (6, 0), "REDIS_6_2"),
    ("redis-8.10", "redis", (7, 0), "REDIS_8_10"),
    ("valkey-9", "valkey", (7, 0), "VALKEY_9"),
];

/// Runs `CONFIG SET <name> <current value>` for every parameter (a no-op
/// for every parameter the server accepts) and records the refusals:
/// (name, kind, reason). Values never appear in the reasons.
async fn probe_refusals(
    server: &Server,
) -> Result<(String, Vec<(String, &'static str, String)>), Box<dyn std::error::Error>> {
    let s = connect(server, DB_MAIN).await?;
    let version = redis::server_info(&*s, &info_of(&*s))
        .await?
        .server
        .valkey_version
        .or_else(|| s.server_info().version.clone().into())
        .unwrap_or_default();
    let output = s
        .execute("CONFIG GET *", &ExecOptions { max_rows: 100_000 })
        .await?;
    let rows = match output.results.first() {
        Some(varve_driver::StatementResult::Rows { rows, .. }) => rows.clone(),
        _ => return Err("CONFIG GET * returned no rows".into()),
    };
    let flat: Vec<String> = rows
        .iter()
        .map(|r| r.get(1).and_then(Value::to_plain_text).unwrap_or_default())
        .collect();
    let mut refusals = Vec::new();
    for pair in flat.chunks(2) {
        let [name, value] = pair else { continue };
        let line = varve_redis_syntax::join_args(&["CONFIG", "SET", name.as_str(), value.as_str()]);
        let Err(error) = s.execute(&line, &ExecOptions::default()).await else {
            continue;
        };
        let message = error.message;
        let (kind, reason) = if message.contains("immutable") {
            ("Immutable", "can't set immutable config".to_string())
        } else if message.contains("Unsupported CONFIG parameter") {
            ("Immutable", "unsupported CONFIG parameter".to_string())
        } else if message.contains("protected") {
            ("Protected", "can't set protected config".to_string())
        } else if redis::is_sensitive_config(name) || value.is_empty() {
            ("Rejected", message)
        } else {
            ("Rejected", message.replace(value.as_str(), "<value>"))
        };
        refusals.push((name.clone(), kind, reason));
    }
    refusals.sort();
    Ok((version, refusals))
}

fn refusal_section(
    const_name: &str,
    server: &str,
    version: &str,
    rows: &[(String, &str, String)],
) -> String {
    let mut out = format!(
        "/// Probed on {server} {version}.\n#[rustfmt::skip]\nconst {const_name}: &[(&str, Refusal, &str)] = &[\n"
    );
    for (name, kind, reason) in rows {
        out.push_str(&format!("    ({name:?}, Refusal::{kind}, {reason:?}),\n"));
    }
    out.push_str("];\n");
    out
}

/// One generated table: flavor, lowest version, constant name, probed
/// version and the Rust text.
type RefusalSection = (&'static str, (u32, u32), &'static str, String, String);

fn refusals_file(sections: &[RefusalSection]) -> String {
    let mut out = String::from(
        "//! Parameters `CONFIG SET` refuses, probed on the M2 matrix (Redis 6.2,\n\
         //! Redis 8.10, Valkey 9) by setting every parameter to its current value.\n\
         //!\n\
         //! Generated by the ignored `generate_redis_config_refusals` test in\n\
         //! `tests/redis_live.rs` (run it with `VARVE_WRITE_REDIS_CONFIG_REFUSALS=1`).\n\
         //! Do not edit by hand. Protected parameters depend on the server's\n\
         //! `enable-protected-configs` (the matrix runs the default, `no`).\n\n\
         use super::config::Refusal;\n\n\
         /// Per probed server: flavor (`redis` or `valkey`), the lowest\n\
         /// (major, minor) version the table applies to, the probed version, and\n\
         /// the refused parameters sorted by name with the kind and the reason.\n\
         pub(super) type Table = (\n    &'static str,\n    (u32, u32),\n    &'static str,\n    &'static [(&'static str, Refusal, &'static str)],\n);\n\n\
         /// Tables ordered by flavor and version.\npub(super) const TABLES: &[Table] = &[\n",
    );
    for (flavor, key, const_name, version, _) in sections {
        out.push_str(&format!(
            "    ({flavor:?}, ({}, {}), {version:?}, {const_name}),\n",
            key.0, key.1
        ));
    }
    out.push_str("];\n");
    for (_, _, _, _, section) in sections {
        out.push('\n');
        out.push_str(section);
    }
    out
}

fn source_server(name: &str) -> Result<Server, Box<dyn std::error::Error>> {
    servers()
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no server {name} in VARVE_M2_REDIS_SERVERS").into())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix; writes src/redis/config_refusals.rs with VARVE_WRITE_REDIS_CONFIG_REFUSALS=1"]
async fn generate_redis_config_refusals() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let mut sections = Vec::new();
    for (name, flavor, key, const_name) in REFUSAL_SOURCES {
        let server = source_server(name)?;
        let (version, rows) = probe_refusals(&server).await?;
        assert!(rows.len() > 10, "{name}: {} refusals", rows.len());
        let section = refusal_section(const_name, name, &version, &rows);
        sections.push((flavor, key, const_name, version, section));
    }
    let text = refusals_file(&sections);
    if std::env::var("VARVE_WRITE_REDIS_CONFIG_REFUSALS").is_ok_and(|v| v == "1") {
        std::fs::write(REFUSALS_FILE, text)?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn redis_config_refusals_match_the_servers() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let committed = std::fs::read_to_string(REFUSALS_FILE)?;
    for (name, _, _, const_name) in REFUSAL_SOURCES {
        let server = source_server(name)?;
        let (version, rows) = probe_refusals(&server).await?;
        let section = refusal_section(const_name, name, &version, &rows);
        assert!(
            committed.contains(&section),
            "{name}: the committed refusals differ from what {version} reports; regenerate them"
        );
    }
    Ok(())
}
