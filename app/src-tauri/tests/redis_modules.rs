//! End-to-end tests of the Redis modules, console help, bulk TTL and memory
//! Stop services (m2/redis_modules.rs, m2/redis_bulk.rs, m2/live_redis.rs).
//!
//! - Help, module detection, bulk TTL and memory Stop run on Redis 6.2,
//!   Redis 8.10 and Valkey 9 (the M2 server matrix; ports
//!   `VARVE_M2_REDIS62_PORT`, `VARVE_M2_REDIS810_PORT`,
//!   `VARVE_M2_VALKEY9_PORT`, host `VARVE_M2_HOST`), keys `modipc:` in
//!   database 12.
//! - Module keys and Search indexes need a server with the modules:
//!   `VARVE_REDIS_MODULES_PORT` (default 64381, for example Redis 8 or Redis
//!   Stack), keys `modipc:` in database 12 and indexes `modipc:` in
//!   database 0.
//!
//! Run with `cargo test -p varve-app --test redis_modules -- --include-ignored`.

use std::sync::Arc;
use std::time::Duration;

use varve_activity::ActivityFilter;
use varve_app::commands::{CmdError, SecretInput, Services};
use varve_app::m2::redis_bulk::{BulkTtlRequest, BulkTtlState};
use varve_app::m2::redis_edit::RedisEditRequest;
use varve_app::sessions::{SessionError, SessionInfo};
use varve_core::Engine;
use varve_driver::{ErrorKind, Value};
use varve_driver_redis::{
    BulkFilter, BulkTtlAction, EditOutcome, KeyEdit, KeyExpect, KeyType, ModuleFeature,
    ModuleQuery, TsRangeQuery, ValueData, ValuePage,
};
use varve_live_stats::redis::{MemoryScanOptions, ScanStop};
use varve_records::{ConnectionInput, ConnectionOptions, Environment};
use varve_secrets::MemoryStore;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Res<T> = Result<T, Box<dyn std::error::Error>>;

const DB: u16 = 12;

/// One test at a time: they share database 12.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    services: Arc<Services>,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(name: &str) -> Res<Fixture> {
    let dir = std::env::temp_dir().join(format!("varve-mod-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let services = varve_app::build_services(store, &dir, &host, Arc::new(MemoryStore::default()))?;
    Ok(Fixture { services, dir })
}

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn targets() -> Vec<(&'static str, u16)> {
    vec![
        ("Redis 6.2", port("VARVE_M2_REDIS62_PORT", 64062)),
        ("Redis 8.10", port("VARVE_M2_REDIS810_PORT", 64081)),
        ("Valkey 9", port("VARVE_M2_VALKEY9_PORT", 64190)),
    ]
}

fn modules_port() -> u16 {
    port("VARVE_REDIS_MODULES_PORT", 64381)
}

async fn open(
    s: &Services,
    name: &str,
    port: u16,
    environment: Environment,
    read_only: bool,
    db: u16,
) -> Res<SessionInfo> {
    let input = ConnectionInput {
        folder_id: None,
        name: format!("{name} modipc"),
        color: None,
        engine: Engine::Redis,
        host: std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: Some(port),
        database: None,
        username: None,
        environment,
        tls: Default::default(),
        ssh: Default::default(),
        options: ConnectionOptions {
            read_only,
            confirm_destructive: false,
            redis_db: db,
            ..Default::default()
        },
    };
    let profile = s.create_connection(&input)?;
    let secret: SecretInput = serde_json::from_value(serde_json::json!("varve_dev"))?;
    Ok(s.open_session(&profile.id, Some(secret), None, None)
        .await?)
}

async fn run(s: &Services, session: &SessionInfo, text: &str) -> Res<()> {
    s.execute(&session.id, text, true, None).await?;
    Ok(())
}

async fn clean(s: &Services, session: &SessionInfo) -> Res<()> {
    run(
        s,
        session,
        "EVAL \"local n = 0 for _, k in ipairs(redis.call('KEYS', 'modipc:*')) do redis.call('DEL', k) n = n + 1 end return n\" 0",
    )
    .await
}

fn text(v: &str) -> Value {
    Value::Text(v.into())
}

fn needs_confirmation<T: std::fmt::Debug>(result: &Result<T, CmdError>) -> bool {
    matches!(
        result,
        Err(CmdError::Session {
            error: SessionError::ConfirmationRequired { .. }
        })
    )
}

fn events(s: &Services, connection_id: &str) -> Res<Vec<(String, serde_json::Value)>> {
    let page = s.activity(&ActivityFilter {
        connection_id: Some(connection_id.to_string()),
        limit: 500,
        ..Default::default()
    })?;
    Ok(page
        .events
        .into_iter()
        .map(|e| (e.action, e.details))
        .collect())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn help_and_module_detection_on_the_matrix() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("help")?;
    let s = &f.services;
    for (name, port) in targets() {
        let session = open(s, name, port, Environment::Development, true, DB).await?;
        let help = s.redis_command_help(&session.id, "set").await?;
        assert!(help.known, "{name}");
        assert_eq!(help.from_docs, name != "Redis 6.2", "{name}: {help:?}");
        if name == "Redis 6.2" {
            assert!(help.note.unwrap_or_default().contains("COMMAND DOCS"));
        } else {
            assert!(help.syntax.unwrap_or_default().starts_with("SET key value"));
        }
        let report = s.redis_modules(&session.id).await?;
        let search = report
            .features
            .iter()
            .find(|f| f.feature == ModuleFeature::Search)
            .ok_or("no search entry")?;
        assert_eq!(search.available, name == "Redis 8.10", "{name}");
        if search.available {
            s.redis_search_indexes(&session.id).await?;
        } else {
            assert!(search.reason.is_some());
            let refused = s.redis_search_indexes(&session.id).await;
            assert!(
                matches!(
                    &refused,
                    Err(CmdError::Session { error: SessionError::Driver { error } })
                        if error.kind == ErrorKind::Unsupported
                ),
                "{name}: {refused:?}"
            );
        }
        s.close_session(&session.id).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn module_keys_and_search_indexes() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("modules")?;
    let s = &f.services;
    let session = open(
        s,
        "Modules",
        modules_port(),
        Environment::Development,
        false,
        DB,
    )
    .await?;
    clean(s, &session).await?;
    let outcome: TestResult = async {
        run(s, &session, "TS.CREATE modipc:ts LABELS room a").await?;
        for t in 1..=5 {
            run(s, &session, &format!("TS.ADD modipc:ts {t}000 {t}")).await?;
        }
        run(s, &session, "BF.ADD modipc:bf hello").await?;

        let chunk = s
            .redis_read_value(&session.id, "modipc:ts", ValuePage::first(2))
            .await?
            .ok_or("no series")?;
        assert!(matches!(chunk.data, ValueData::TimeSeries { .. }));
        let page = s
            .redis_ts_range(
                &session.id,
                "modipc:ts",
                &TsRangeQuery {
                    from: None,
                    to: None,
                    reverse: true,
                    count: 2,
                    cursor: None,
                },
            )
            .await?
            .ok_or("no series")?;
        assert_eq!(page.samples[0].timestamp, 5000);
        assert_eq!(page.next.as_deref(), Some("3999"));

        let expect = KeyExpect::Present {
            key_type: KeyType::Other("TSDB-TYPE".into()),
            version: None,
        };
        let planned = s
            .redis_edit_plan(
                &session.id,
                &RedisEditRequest {
                    key: "modipc:ts".into(),
                    expect: expect.clone(),
                    edits: vec![KeyEdit::TsAdd {
                        timestamp: Some("9000".into()),
                        value: text("9"),
                    }],
                },
            )
            .await?;
        let applied = s
            .redis_edit_apply(&session.id, &planned.plan_id, false)
            .await?;
        assert!(matches!(applied, EditOutcome::Applied { .. }), "{applied:?}");
        let edited = events(s, &session.connection_id)?;
        assert!(edited.iter().any(|(action, details)| action == "data.edited"
            && details["operations"] == serde_json::json!(["tsAdd"])));

        let answer = s
            .redis_module_query(
                &session.id,
                "modipc:bf",
                &ModuleQuery::BloomExists {
                    items: vec![text("hello"), text("nope")],
                },
            )
            .await?
            .ok_or("no filter")?;
        assert_eq!(answer.rows[0].output, Value::Bool(true));
        assert_eq!(answer.rows[1].output, Value::Bool(false));

        // Read-only sessions query, and refuse the plan.
        let ro = open(s, "Modules ro", modules_port(), Environment::Development, true, DB).await?;
        assert!(
            s.redis_module_query(
                &ro.id,
                "modipc:bf",
                &ModuleQuery::BloomExists {
                    items: vec![text("hello")]
                }
            )
            .await?
            .is_some()
        );
        let refused = s
            .redis_edit_plan(
                &ro.id,
                &RedisEditRequest {
                    key: "modipc:bf".into(),
                    expect: KeyExpect::Present {
                        key_type: KeyType::Other("MBbloom--".into()),
                        version: None,
                    },
                    edits: vec![KeyEdit::BloomAdd {
                        items: vec![text("x")],
                    }],
                },
            )
            .await;
        assert!(matches!(refused, Err(CmdError::ReadOnly { .. })), "{refused:?}");
        s.close_session(&ro.id).await?;

        // Search indexes live on database 0, read on the admin session.
        let db0 = open(s, "Modules db0", modules_port(), Environment::Development, false, 0).await?;
        let _ = run(s, &db0, "FT.DROPINDEX modipc:idx").await;
        run(
            s,
            &db0,
            "FT.CREATE modipc:idx ON HASH PREFIX 1 modipc:user: SCHEMA name TEXT SORTABLE age NUMERIC",
        )
        .await?;
        let report = s.redis_modules(&db0.id).await?;
        assert!(report.has(ModuleFeature::Search) && report.has(ModuleFeature::TimeSeries));
        let names = s.redis_search_indexes(&db0.id).await?;
        assert!(names.contains(&"modipc:idx".to_string()), "{names:?}");
        let index = s
            .redis_search_index(&db0.id, "modipc:idx")
            .await?
            .ok_or("no index")?;
        assert!(index.ddl.unwrap_or_default().starts_with("FT.CREATE modipc:idx ON HASH PREFIX 1 modipc:user:"));
        assert!(s.redis_search_index(&db0.id, "modipc:none").await?.is_none());
        run(s, &db0, "FT.DROPINDEX modipc:idx").await?;
        s.close_session(&db0.id).await?;
        Ok(())
    }
    .await;
    clean(s, &session).await?;
    outcome
}

/// Polls a bulk job until it ends (at most 20 s).
async fn wait_for(
    s: &Services,
    session: &SessionInfo,
    job: &str,
) -> Res<varve_app::m2::redis_bulk::BulkTtlStatus> {
    for _ in 0..400 {
        let status = s.redis_bulk_ttl_status(&session.id, job)?;
        if status.state != BulkTtlState::Running {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err("the bulk job did not end".into())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn bulk_ttl_preview_confirmation_progress_and_stop() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("bulk")?;
    let s = &f.services;
    for (name, port) in targets() {
        let session = open(s, name, port, Environment::Production, false, DB).await?;
        clean(s, &session).await?;
        let outcome: TestResult = async {
            run(
                s,
                &session,
                "EVAL \"for i=1,3000 do redis.call('SET', 'modipc:k:' .. i, 'v') end return 1\" 0",
            )
            .await?;
            let filter = BulkFilter {
                pattern: "modipc:k:*".into(),
                key_type: Some(KeyType::String),
            };
            let preview = s.redis_bulk_ttl_preview(&session.id, &filter).await?;
            assert_eq!(preview.keys, 3000, "{name}");
            assert!(preview.complete);
            assert_eq!(preview.db, DB);
            let request = BulkTtlRequest {
                filter: filter.clone(),
                action: BulkTtlAction::Expire { ttl_ms: 600_000 },
                db: DB,
            };
            // Production asks first.
            assert!(needs_confirmation(
                &s.redis_bulk_ttl_start(&session.id, &request, false).await
            ));
            let job = s.redis_bulk_ttl_start(&session.id, &request, true).await?;
            let done = wait_for(s, &session, &job.job_id).await?;
            assert_eq!(done.state, BulkTtlState::Complete, "{name}: {done:?}");
            assert_eq!(done.counts.changed, 3000);
            let info = s
                .redis_key_info(&session.id, "modipc:k:1")
                .await?
                .ok_or("no key")?;
            assert!(info.ttl_ms.is_some_and(|t| t > 590_000));
            let logged = events(s, &session.connection_id)?;
            assert!(
                logged
                    .iter()
                    .any(|(action, details)| action == "data.edited"
                        && details["operations"] == serde_json::json!(["bulkExpire"])
                        && details["status"] == "complete"
                        && details["rows_updated"] == 3000)
            );
            assert!(
                logged
                    .iter()
                    .any(|(action, _)| action == "destructive.confirmed")
            );

            // Stop: the job ends at its next step and keeps what it changed.
            let persist = BulkTtlRequest {
                filter: filter.clone(),
                action: BulkTtlAction::Persist,
                db: DB,
            };
            let job = s.redis_bulk_ttl_start(&session.id, &persist, true).await?;
            let stopped = s.redis_bulk_ttl_stop(&session.id, &job.job_id)?;
            assert_eq!(stopped.job_id, job.job_id);
            let ended = wait_for(s, &session, &job.job_id).await?;
            assert!(
                matches!(ended.state, BulkTtlState::Stopped | BulkTtlState::Complete),
                "{name}: {ended:?}"
            );
            assert!(ended.counts.changed <= 3000);

            // Another database than the preview's is refused.
            s.redis_select_db(&session.id, DB + 1).await?;
            let moved = s.redis_bulk_ttl_start(&session.id, &request, true).await;
            assert!(matches!(moved, Err(CmdError::Invalid { .. })), "{moved:?}");
            s.redis_select_db(&session.id, DB).await?;
            // Unknown jobs and other sessions' jobs are not found.
            assert!(s.redis_bulk_ttl_status(&session.id, "nope").is_err());

            // Read-only sessions refuse.
            let ro = open(s, name, port, Environment::Development, true, DB).await?;
            assert!(matches!(
                s.redis_bulk_ttl_preview(&ro.id, &filter).await,
                Err(CmdError::ReadOnly { .. })
            ));
            assert!(matches!(
                s.redis_bulk_ttl_start(&ro.id, &request, true).await,
                Err(CmdError::ReadOnly { .. })
            ));
            assert!(s.redis_bulk_ttl_status(&ro.id, &job.job_id).is_err());
            s.close_session(&ro.id).await?;
            Ok(())
        }
        .await;
        clean(s, &session).await?;
        s.close_session(&session.id).await?;
        outcome.map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn memory_analysis_stops_on_request() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("memory")?;
    let s = Arc::clone(&f.services);
    for (name, port) in targets() {
        let session = open(&s, name, port, Environment::Development, false, DB).await?;
        clean(&s, &session).await?;
        let outcome: TestResult = async {
            run(
                &s,
                &session,
                "EVAL \"for i=1,30000 do redis.call('SET', 'modipc:m:' .. i, 'v') end return 1\" 0",
            )
            .await?;
            let options = MemoryScanOptions {
                pattern: Some("modipc:*".into()),
                max_keys: 1_000_000,
                time_budget_ms: 60_000,
                scan_count: 50,
                ..MemoryScanOptions::default()
            };
            let task = {
                let s = Arc::clone(&s);
                let id = session.id.clone();
                let options = options.clone();
                tokio::spawn(async move {
                    s.redis_memory_analysis(&id, Some(&options), Some("run-1"))
                        .await
                })
            };
            tokio::time::sleep(Duration::from_millis(300)).await;
            let started = std::time::Instant::now();
            s.redis_memory_analysis_stop(&session.id, "run-1");
            let analysis = task.await??;
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{name}: {:?}",
                started.elapsed()
            );
            assert_eq!(analysis.stopped_by, ScanStop::Stopped, "{name}");
            assert!(!analysis.complete);
            assert!(analysis.scanned_keys > 0 && analysis.scanned_keys < 30_000);
            // A stop of another session's run does nothing; a stop sent
            // first stops the run at once.
            s.redis_memory_analysis_stop("other-session", "run-2");
            let small = MemoryScanOptions {
                max_keys: 500,
                ..options.clone()
            };
            let other = s
                .redis_memory_analysis(&session.id, Some(&small), Some("run-2"))
                .await?;
            assert_eq!(other.stopped_by, ScanStop::KeyBudget);
            s.redis_memory_analysis_stop(&session.id, "run-3");
            let early = s
                .redis_memory_analysis(&session.id, Some(&options), Some("run-3"))
                .await?;
            assert_eq!(early.stopped_by, ScanStop::Stopped);
            assert_eq!(early.scanned_keys, 0);
            Ok(())
        }
        .await;
        clean(&s, &session).await?;
        s.close_session(&session.id).await?;
        outcome.map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn a_confirmed_bulk_job_that_fails_is_logged_as_confirmed() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("bulkfail")?;
    let s = &f.services;
    let admin = open(
        s,
        "Modules admin",
        modules_port(),
        Environment::Development,
        false,
        DB,
    )
    .await?;
    clean(s, &admin).await?;
    // A user that may scan but not change expiries: the job fails at its
    // first step.
    run(
        s,
        &admin,
        "ACL SETUSER modipc_noexp on >modipc_pw ~* &* +@all -pexpire -persist",
    )
    .await?;
    run(s, &admin, "SET modipc:a 1").await?;
    let outcome: TestResult = async {
        let input = ConnectionInput {
            folder_id: None,
            name: "Modules noexp modipc".into(),
            color: None,
            engine: Engine::Redis,
            host: std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: Some(modules_port()),
            database: None,
            username: Some("modipc_noexp".into()),
            environment: Environment::Production,
            tls: Default::default(),
            ssh: Default::default(),
            options: ConnectionOptions {
                redis_db: DB,
                ..Default::default()
            },
        };
        let profile = s.create_connection(&input)?;
        let secret: SecretInput = serde_json::from_value(serde_json::json!("modipc_pw"))?;
        let session = s
            .open_session(&profile.id, Some(secret), None, None)
            .await?;
        let request = BulkTtlRequest {
            filter: BulkFilter {
                pattern: "modipc:*".into(),
                key_type: None,
            },
            action: BulkTtlAction::Expire { ttl_ms: 60_000 },
            db: DB,
        };
        let job = s.redis_bulk_ttl_start(&session.id, &request, true).await?;
        let ended = wait_for(s, &session, &job.job_id).await?;
        assert_eq!(ended.state, BulkTtlState::Failed, "{ended:?}");
        let logged = events(s, &session.connection_id)?;
        assert!(
            logged
                .iter()
                .any(|(action, _)| action == "destructive.confirmed")
        );
        assert!(
            logged
                .iter()
                .any(|(action, details)| action == "data.edited" && details["status"] == "failed")
        );
        // The tab's job list finds it (a reopened dialog reconciles with it).
        let jobs = s.redis_bulk_ttl_jobs(&session.id);
        assert_eq!(
            jobs.first().map(|j| j.job_id.as_str()),
            Some(job.job_id.as_str())
        );
        s.close_session(&session.id).await?;
        Ok(())
    }
    .await;
    run(s, &admin, "ACL DELUSER modipc_noexp").await?;
    clean(s, &admin).await?;
    outcome
}
