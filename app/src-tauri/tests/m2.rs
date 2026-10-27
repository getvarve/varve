//! End-to-end tests of the M2 services (object browser, editors, EXPLAIN,
//! grid editing, live panels) against the shared M2 server matrix
//! (compose project `varve-m2`). Objects use the prefix `m2_ipc`.
//!
//! Run with `cargo test -p varve-app --test m2 -- --include-ignored`.
//! `VARVE_ONLY_SERVERS` (compose service names such as `postgres-18,mysql-9.7`)
//! limits the tests that loop over every server to those servers.

use std::sync::Arc;

use serde_json::json;
use varve_activity::ActivityFilter;
use varve_app::commands::{CmdError, SecretInput, Services};
use varve_app::m2::editors::EngineChange;
use varve_app::m2::live_redis::{ConfigRequest, FunctionRequest};
use varve_app::m2::plans::StoredPlan;
use varve_app::sessions::{SessionError, SessionInfo};
use varve_catalog::data_edit::{ApplyOutcome, PlanOptions, RowChange, TableTarget};
use varve_catalog::explain::ExplainOptions;
use varve_catalog::{Container, EditPlan, ObjectKind, ObjectRef};
use varve_core::{Edition, Engine};
use varve_driver::{StatementResult, Value};
use varve_live_stats::redis::MemoryScanOptions;
use varve_live_stats::sql::{KillTarget, LiveError, VariableChange};
use varve_records::{ConnectionInput, ConnectionOptions, Environment};
use varve_secrets::MemoryStore;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Res<T> = Result<T, Box<dyn std::error::Error>>;

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
    let dir = std::env::temp_dir().join(format!("varve-m2-ipc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let services = varve_app::build_services(
        store,
        &dir,
        &host,
        Edition::Community,
        Arc::new(MemoryStore::default()),
    )?;
    Ok(Fixture { services, dir })
}

/// Services on an existing data directory, as after an app restart.
fn services_on(dir: &std::path::Path) -> Res<Arc<Services>> {
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(dir, &host)?);
    Ok(varve_app::build_services(
        store,
        dir,
        &host,
        Edition::Community,
        Arc::new(MemoryStore::default()),
    )?)
}

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn host() -> String {
    std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

fn secret(value: &str) -> Res<SecretInput> {
    Ok(serde_json::from_value(serde_json::Value::String(
        value.into(),
    ))?)
}

struct Target {
    name: &'static str,
    engine: Engine,
    port: u16,
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

/// `targets` without the servers [`selected`] leaves out, never none. Tests
/// that loop over every server use it; tests written for one version index
/// the full lists.
fn only_selected<const N: usize>(targets: [Target; N]) -> Vec<Target> {
    let targets: Vec<Target> = targets
        .into_iter()
        .filter(|target| {
            let service = target
                .name
                .to_lowercase()
                .replace("pg ", "postgres-")
                .replace(' ', "-");
            selected(&service)
        })
        .collect();
    assert!(
        !targets.is_empty(),
        "VARVE_ONLY_SERVERS selects none of these servers"
    );
    targets
}

fn pg_targets() -> [Target; 2] {
    [
        Target {
            name: "PG 13",
            engine: Engine::Postgres,
            port: port("VARVE_M2_PG13_PORT", 55513),
        },
        Target {
            name: "PG 18",
            engine: Engine::Postgres,
            port: port("VARVE_M2_PG18_PORT", 55518),
        },
    ]
}

fn my_targets() -> [Target; 5] {
    [
        Target {
            name: "MySQL 8.0",
            engine: Engine::Mysql,
            port: port("VARVE_M2_MYSQL80_PORT", 35080),
        },
        Target {
            name: "MySQL 8.4",
            engine: Engine::Mysql,
            port: port("VARVE_M2_MYSQL84_PORT", 35084),
        },
        Target {
            name: "MySQL 9.7",
            engine: Engine::Mysql,
            port: port("VARVE_M2_MYSQL97_PORT", 35097),
        },
        Target {
            name: "MariaDB 10.6",
            engine: Engine::Mariadb,
            port: port("VARVE_M2_MARIADB106_PORT", 35106),
        },
        Target {
            name: "MariaDB 12.3",
            engine: Engine::Mariadb,
            port: port("VARVE_M2_MARIADB123_PORT", 35123),
        },
    ]
}

fn redis_targets() -> [Target; 3] {
    [
        Target {
            name: "Redis 6.2",
            engine: Engine::Redis,
            port: port("VARVE_M2_REDIS62_PORT", 64062),
        },
        Target {
            name: "Redis 8.10",
            engine: Engine::Redis,
            port: port("VARVE_M2_REDIS810_PORT", 64081),
        },
        Target {
            name: "Valkey 9",
            engine: Engine::Redis,
            port: port("VARVE_M2_VALKEY9_PORT", 64190),
        },
    ]
}

fn input(target: &Target, environment: Environment, read_only: bool) -> ConnectionInput {
    let (database, username) = match target.engine {
        Engine::Postgres => (Some("varve".to_string()), Some("varve".to_string())),
        Engine::Mysql | Engine::Mariadb => (None, Some("root".to_string())),
        Engine::Redis => (None, None),
    };
    ConnectionInput {
        folder_id: None,
        name: format!("{} m2_ipc", target.name),
        color: None,
        engine: target.engine,
        host: host(),
        port: Some(target.port),
        database,
        username,
        environment,
        tls: Default::default(),
        ssh: Default::default(),
        options: ConnectionOptions {
            read_only,
            confirm_destructive: false,
            ..Default::default()
        },
    }
}

async fn open(
    s: &Services,
    target: &Target,
    environment: Environment,
    read_only: bool,
) -> Res<SessionInfo> {
    let profile = s.create_connection(&input(target, environment, read_only))?;
    Ok(
        s.open_session(&profile.id, Some(secret("varve_dev")?), None, None)
            .await?,
    )
}

async fn run(s: &Services, session: &SessionInfo, sql: &str) -> Res<varve_driver::ExecOutput> {
    Ok(s.execute(&session.id, sql, true, None).await?)
}

fn first_value(output: &varve_driver::ExecOutput) -> Option<Value> {
    output.results.iter().find_map(|r| match r {
        StatementResult::Rows { rows, .. } => rows.first().and_then(|row| row.first()).cloned(),
        StatementResult::Command { .. } => None,
    })
}

/// Rows in the safety fixture table, read through the setup session.
async fn safety_rows(s: &Services, setup: &SessionInfo) -> Res<Option<i64>> {
    Ok(int_of(first_value(
        &run(s, setup, "SELECT count(*) FROM m2_ipc_safety.t").await?,
    )))
}

fn int_of(value: Option<Value>) -> Option<i64> {
    match value? {
        Value::Int(i) => Some(i),
        Value::UInt(u) => i64::try_from(u).ok(),
        Value::Text(t) | Value::Decimal(t) => t.parse().ok(),
        _ => None,
    }
}

fn needs_confirmation<T: std::fmt::Debug>(result: &Result<T, CmdError>) -> bool {
    matches!(
        result,
        Err(CmdError::Session {
            error: SessionError::ConfirmationRequired { .. }
        })
    )
}

fn is_read_only<T: std::fmt::Debug>(result: &Result<T, CmdError>) -> bool {
    matches!(result, Err(CmdError::ReadOnly { .. }))
}

fn actions(s: &Services, connection_id: &str) -> Res<Vec<String>> {
    let page = s.activity(&ActivityFilter {
        connection_id: Some(connection_id.to_string()),
        limit: 500,
        ..Default::default()
    })?;
    Ok(page.events.into_iter().map(|e| e.action).collect())
}

/// Stores a hand made editor plan, as `edit_plan` would.
fn stored(s: &Services, session: &SessionInfo, plan: EditPlan) -> String {
    s.m2.plans.put(&session.id, StoredPlan::Edit(plan))
}

fn is_unavailable<T: std::fmt::Debug>(result: &Result<T, CmdError>) -> bool {
    matches!(result, Err(CmdError::NotFound { .. }))
}

fn container(database: &str, schema: Option<&str>) -> Container {
    Container {
        database: Some(database.into()),
        schema: schema.map(Into::into),
        parent: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn postgres_browser_editors_explain_grid_and_panels() -> TestResult {
    let f = fixture("pg")?;
    let s = &f.services;
    for target in only_selected(pg_targets()) {
        let name = target.name;
        let session = open(s, &target, Environment::Development, false).await?;
        assert_eq!(session.database.as_deref(), Some("varve"), "{name}");
        run(s, &session, "DROP SCHEMA IF EXISTS m2_ipc CASCADE").await?;

        // Editors: create a schema through a plan.
        let create = EngineChange::Postgres(Box::new(serde_json::from_value(json!({
            "object": "schema", "action": "create", "name": "m2_ipc"
        }))?));
        let planned = s.edit_plan(&session.id, &create)?;
        // Inside an open transaction the plan is refused and stays usable.
        run(s, &session, "BEGIN").await?;
        let refused = s.edit_apply(&session.id, &planned.plan_id, false).await;
        assert!(
            matches!(
                refused,
                Err(CmdError::Session {
                    error: SessionError::OpenTransaction { .. }
                })
            ),
            "{name}: {refused:?}"
        );
        run(s, &session, "ROLLBACK").await?;
        let applied = s.edit_apply(&session.id, &planned.plan_id, false).await?;
        assert_eq!(applied.statements, planned.plan.statements.len(), "{name}");
        // A plan runs once, only on its session, and only by a real id.
        assert!(is_unavailable(
            &s.edit_apply(&session.id, &planned.plan_id, false).await
        ));
        assert!(is_unavailable(
            &s.edit_apply(&session.id, "0190f0f0-0000-7000-8000-000000000000", false)
                .await
        ));
        run(
            s,
            &session,
            "CREATE TABLE m2_ipc.items (id int PRIMARY KEY, label text NOT NULL)",
        )
        .await?;

        // A MySQL change on a PostgreSQL session is refused.
        let wrong = EngineChange::Mysql(Box::new(serde_json::from_value(json!({
            "change": "dropDatabase", "name": "m2_ipc"
        }))?));
        assert!(matches!(
            s.edit_plan(&session.id, &wrong),
            Err(CmdError::Invalid { .. })
        ));

        // A transactional plan whose second statement fails changes nothing.
        let failing = EditPlan {
            statements: vec![
                "CREATE TABLE m2_ipc.half (a int)".into(),
                "CREATE TABLE m2_ipc_missing_schema.other (a int)".into(),
            ],
            transactional: true,
            destructive: false,
            warnings: vec![],
        };
        let failing_id = stored(s, &session, failing);
        assert!(s.edit_apply(&session.id, &failing_id, false).await.is_err());
        let half = run(
            s,
            &session,
            "SELECT count(*) FROM pg_catalog.pg_tables WHERE schemaname = 'm2_ipc' AND tablename = 'half'",
        )
        .await?;
        assert_eq!(int_of(first_value(&half)), Some(0), "{name}: rolled back");

        // Object browser.
        let caps = s.catalog_capabilities(&session.id).await?;
        assert_eq!(caps.engine, Engine::Postgres);
        assert!(caps.kinds.contains(&ObjectKind::Table), "{name}");
        // Browsing runs on a hidden metadata session, not the user's.
        let meta = s
            .metadata_session_id(&session.id)
            .ok_or("no metadata session")?;
        assert_ne!(meta, session.id, "{name}");
        assert!(s.catalogs.contains(&meta), "{name}: catalog cached");
        assert!(!s.catalogs.contains(&session.id), "{name}");
        assert!(
            s.sessions().iter().all(|i| i.id != meta),
            "{name}: metadata session listed"
        );
        let meta_info = s
            .sessions
            .list_all()
            .into_iter()
            .find(|i| i.id == meta)
            .ok_or("metadata session not open")?;
        assert!(meta_info.read_only, "{name}: metadata session writable");
        assert_eq!(meta_info.database.as_deref(), Some("varve"), "{name}");
        // A transaction the user left open does not affect browsing.
        run(s, &session, "BEGIN").await?;
        run(s, &session, "CREATE TABLE m2_ipc.uncommitted (a int)").await?;
        let databases = s.catalog_databases(&session.id).await?;
        assert!(databases.iter().any(|d| d.object.name == "varve"), "{name}");
        let in_tx = s
            .catalog_list(
                &session.id,
                &container("varve", Some("m2_ipc")),
                ObjectKind::Table,
                None,
            )
            .await?;
        assert!(
            in_tx.iter().all(|t| t.object.name != "uncommitted"),
            "{name}: browser saw the user's uncommitted table"
        );
        run(s, &session, "ROLLBACK").await?;
        // A lost metadata connection is reopened transparently.
        let meta_pid = int_of(first_value(
            &run(s, &meta_info, "SELECT pg_backend_pid()").await?,
        ))
        .ok_or("no metadata pid")?;
        run(
            s,
            &session,
            &format!("SELECT pg_terminate_backend({meta_pid})"),
        )
        .await?;
        let again = s.catalog_schemas(&session.id, "varve").await?;
        assert!(again.iter().any(|d| d.object.name == "m2_ipc"), "{name}");
        let reopened = s
            .metadata_session_id(&session.id)
            .ok_or("no metadata session after reconnect")?;
        assert_ne!(reopened, meta, "{name}: not reopened");
        // Idle metadata sessions close and reopen on the next call.
        assert!(
            s.close_idle_metadata(std::time::Duration::ZERO).await >= 1,
            "{name}"
        );
        assert!(s.metadata_session_id(&session.id).is_none(), "{name}");
        s.catalog_capabilities(&session.id).await?;
        assert!(s.metadata_session_id(&session.id).is_some(), "{name}");
        let schemas = s.catalog_schemas(&session.id, "varve").await?;
        assert!(schemas.iter().any(|d| d.object.name == "m2_ipc"), "{name}");
        let here = container("varve", Some("m2_ipc"));
        let kinds = s.catalog_kinds(&session.id, &here, Some(false)).await?;
        assert!(
            kinds
                .iter()
                .any(|k| k.kind == ObjectKind::Table && k.count == 1),
            "{name}: {kinds:?}"
        );
        let tables = s
            .catalog_list(&session.id, &here, ObjectKind::Table, None)
            .await?;
        let items = tables
            .iter()
            .find(|t| t.object.name == "items")
            .ok_or("items not listed")?;
        s.catalog_describe(&session.id, &items.object).await?;
        let ddl = s.catalog_ddl(&session.id, &items.object).await?;
        assert!(ddl.contains("CREATE TABLE"), "{name}: {ddl}");
        let missing = ObjectRef {
            name: "m2_ipc_nope".into(),
            ..items.object.clone()
        };
        assert!(matches!(
            s.catalog_describe(&session.id, &missing).await,
            Err(CmdError::Catalog { .. })
        ));

        // Another database of the same profile.
        let other = s
            .open_database_session(
                &session.connection_id,
                "postgres",
                Some(secret("varve_dev")?),
                None,
                None,
            )
            .await?;
        assert_eq!(other.database.as_deref(), Some("postgres"), "{name}");
        let other_databases = s.catalog_databases(&other.id).await?;
        assert!(!other_databases.is_empty(), "{name}");
        assert_ne!(
            s.metadata_session_id(&other.id),
            s.metadata_session_id(&session.id),
            "{name}: one metadata session per database"
        );
        let current = run(s, &other, "SELECT current_database()").await?;
        assert_eq!(
            first_value(&current),
            Some(Value::Text("postgres".into())),
            "{name}"
        );
        assert!(
            s.open_database_session(&session.connection_id, " ", None, None, None)
                .await
                .is_err()
        );

        // EXPLAIN.
        let caps = s.explain_capabilities(&session.id)?;
        assert!(caps.analyze, "{name}");
        let plan = s
            .explain(
                &session.id,
                "SELECT * FROM m2_ipc.items WHERE id = 1",
                &ExplainOptions::default(),
                false,
            )
            .await?;
        assert!(!plan.analyzed, "{name}");
        let analyze = ExplainOptions {
            analyze: true,
            ..Default::default()
        };
        let insert = "INSERT INTO m2_ipc.items VALUES (99, 'x')";
        assert!(needs_confirmation(
            &s.explain(&session.id, insert, &analyze, false).await
        ));
        s.explain(&session.id, insert, &analyze, true).await?;
        let count = run(s, &session, "SELECT count(*) FROM m2_ipc.items").await?;
        assert_eq!(int_of(first_value(&count)), Some(0), "{name}: rolled back");

        // Grid editing.
        let table = TableTarget {
            database: "varve".into(),
            schema: Some("m2_ipc".into()),
            table: "items".into(),
        };
        let key = s.editable_key(&session.id, &table).await?.ok_or("no key")?;
        let options = PlanOptions {
            check_original: true,
            key,
        };
        let changes: Vec<RowChange> = serde_json::from_value(json!([
            { "op": "insert", "values": [
                ["id", { "kind": "value", "value": { "t": "int", "v": 1 } }],
                ["label", { "kind": "value", "value": { "t": "text", "v": "secret cell" } }]
            ] }
        ]))?;
        let planned = s.row_edit_plan(&session.id, &table, &changes, &options)?;
        // Another session cannot use the plan.
        assert!(is_unavailable(
            &s.row_edit_apply(&other.id, &planned.plan_id, true).await
        ));
        let outcome = s
            .row_edit_apply(&session.id, &planned.plan_id, false)
            .await?;
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "{name}: {outcome:?}"
        );
        // A plan id of another kind is refused.
        let edit_id = stored(
            s,
            &session,
            EditPlan {
                statements: vec!["CREATE TABLE m2_ipc.never (a int)".into()],
                transactional: true,
                destructive: false,
                warnings: vec![],
            },
        );
        assert!(matches!(
            s.row_edit_apply(&session.id, &edit_id, true).await,
            Err(CmdError::Invalid { .. })
        ));

        // Live panels.
        // Panels run on the admin session: a query running on the user's
        // own session can be listed and cancelled from the same tab.
        let user_pid = int_of(first_value(
            &run(s, &session, "SELECT pg_backend_pid()").await?,
        ))
        .ok_or("no pid")?;
        let sleeper = {
            let services = Arc::clone(s);
            let id = session.id.clone();
            tokio::spawn(async move {
                services
                    .execute(&id, "SELECT pg_sleep(20)", false, None)
                    .await
            })
        };
        let mut running = false;
        for _ in 0..50 {
            let processes = s.process_list(&session.id).await?;
            if processes.processes.iter().any(|p| {
                p.id == user_pid && p.query.as_deref().is_some_and(|q| q.contains("pg_sleep"))
            }) {
                running = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(running, "{name}: the user's query is not listed");
        let killed = s
            .kill_process(&session.id, user_pid, KillTarget::Query)
            .await?;
        assert!(killed.signalled, "{name}");
        let slept = sleeper.await?;
        assert!(slept.is_err(), "{name}: the query was not cancelled");
        let processes = s.process_list(&session.id).await?;
        let own = processes.own_id.ok_or("no own id")?;
        assert_ne!(own, user_pid, "{name}: panel ran on the user's session");
        assert!(matches!(
            s.kill_process(&session.id, own, KillTarget::Query).await,
            Err(CmdError::Live {
                error: LiveError::OwnConnection { .. }
            })
        ));
        let variables = s.server_variables(&session.id).await?;
        assert!(variables.iter().any(|v| v.name == "work_mem"), "{name}");
        let change: VariableChange = serde_json::from_value(json!({
            "action": "set", "scope": "session", "value": "8MB"
        }))?;
        let planned = s
            .variable_change_plan(&session.id, "work_mem", &change)
            .await?;
        s.variable_change_apply(&session.id, &planned.plan_id, false)
            .await?;
        let work_mem = run(s, &session, "SHOW work_mem").await?;
        assert_eq!(
            first_value(&work_mem),
            Some(Value::Text("8MB".into())),
            "{name}"
        );
        s.server_status(&session.id).await?;
        assert!(s.galera_status(&session.id).await.is_err(), "{name}");
        assert!(s.redis_info(&session.id).await.is_err(), "{name}");

        // A destructive editor plan needs confirmation even on development.
        let drop = EngineChange::Postgres(Box::new(serde_json::from_value(json!({
            "object": "schema", "action": "drop", "name": "m2_ipc", "behavior": "cascade"
        }))?));
        let planned = s.edit_plan(&session.id, &drop)?;
        assert!(planned.plan.destructive, "{name}");
        assert!(needs_confirmation(
            &s.edit_apply(&session.id, &planned.plan_id, false).await
        ));
        s.edit_apply(&session.id, &planned.plan_id, true).await?;

        let logged = actions(s, &session.connection_id)?;
        for action in [
            "schema.changed",
            "object.dropped",
            "destructive.blocked",
            "destructive.confirmed",
            "data.edited",
            "process.killed",
            "query.executed",
        ] {
            assert!(
                logged.iter().any(|a| a == action),
                "{name}: {action} in {logged:?}"
            );
        }
        let page = s.activity(&ActivityFilter {
            connection_id: Some(session.connection_id.clone()),
            actions: vec!["data.edited".into()],
            limit: 50,
            ..Default::default()
        })?;
        let text = serde_json::to_string(&page.events)?;
        assert!(!text.contains("secret cell"), "{name}: cell value logged");

        let opened = s.activity(&ActivityFilter {
            connection_id: Some(session.connection_id.clone()),
            actions: vec!["connection.opened".into()],
            limit: 50,
            ..Default::default()
        })?;
        assert!(
            opened
                .events
                .iter()
                .any(|e| e.details["purpose"] == "metadata"),
            "{name}: metadata connection not marked"
        );

        // The last user session of the profile closes its metadata
        // sessions (here one per database: varve and postgres).
        let other_meta = s
            .metadata_session_id(&other.id)
            .ok_or("no metadata session for the second database")?;
        s.close_session(&other.id).await?;
        assert!(s.metadata_session_id(&session.id).is_some(), "{name}");
        let last_meta = s.metadata_session_id(&session.id).ok_or("gone early")?;
        s.close_session(&session.id).await?;
        let open_ids: Vec<String> = s.sessions.list_all().into_iter().map(|i| i.id).collect();
        assert!(
            !open_ids.contains(&other_meta) && !open_ids.contains(&last_meta),
            "{name}: metadata sessions left open"
        );
        assert!(!s.catalogs.contains(&last_meta), "{name}: catalog dropped");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn postgres_safety_rules_for_production_and_read_only() -> TestResult {
    let f = fixture("pg-safety")?;
    let s = &f.services;
    let target = &pg_targets()[1];
    let setup = open(s, target, Environment::Development, false).await?;
    run(s, &setup, "DROP SCHEMA IF EXISTS m2_ipc_safety CASCADE").await?;
    run(
        s,
        &setup,
        "CREATE SCHEMA m2_ipc_safety; CREATE TABLE m2_ipc_safety.t (id int PRIMARY KEY, v text); \
         INSERT INTO m2_ipc_safety.t VALUES (1, 'a'), (2, 'b')",
    )
    .await?;
    let table = TableTarget {
        database: "varve".into(),
        schema: Some("m2_ipc_safety".into()),
        table: "t".into(),
    };

    // Production: deleting rows needs confirmation.
    let prod = open(s, target, Environment::Production, false).await?;
    assert!(prod.confirm_destructive);
    let key = s.editable_key(&prod.id, &table).await?.ok_or("no key")?;
    let options = PlanOptions {
        check_original: false,
        key,
    };
    let delete: Vec<RowChange> = serde_json::from_value(json!([
        { "op": "delete", "key": { "columns": [["id", { "t": "int", "v": 1 }]] }, "original": [] }
    ]))?;
    let planned = s.row_edit_plan(&prod.id, &table, &delete, &options)?;
    assert!(needs_confirmation(
        &s.row_edit_apply(&prod.id, &planned.plan_id, false).await
    ));
    let outcome = s.row_edit_apply(&prod.id, &planned.plan_id, true).await?;
    assert!(matches!(outcome, ApplyOutcome::Applied { .. }));

    // Read-only: every change is refused before it reaches the server.
    let ro = open(s, target, Environment::Development, true).await?;
    assert!(ro.read_only);
    assert!(is_read_only(
        &s.row_edit_apply(&ro.id, &planned.plan_id, true).await
    ));
    let plan_ddl = EditPlan {
        statements: vec!["CREATE TABLE m2_ipc_safety.u (a int)".into()],
        transactional: true,
        destructive: false,
        warnings: vec![],
    };
    let ddl_id = stored(s, &ro, plan_ddl.clone());
    assert!(is_read_only(&s.edit_apply(&ro.id, &ddl_id, true).await));
    assert!(is_read_only(
        &s.kill_process(&ro.id, 1, KillTarget::Query).await
    ));
    assert!(is_read_only(
        &s.variable_change_apply(&ro.id, &ddl_id, true).await
    ));
    let analyze = ExplainOptions {
        analyze: true,
        ..Default::default()
    };
    assert!(is_read_only(
        &s.explain(&ro.id, "UPDATE m2_ipc_safety.t SET v = 'z'", &analyze, true)
            .await
    ));
    // Reading still works on a read-only session: browsing, details, DDL
    // and EXPLAIN ANALYZE of a read.
    let here = container("varve", Some("m2_ipc_safety"));
    let kinds = s.catalog_kinds(&ro.id, &here, None).await;
    assert!(kinds.is_ok(), "read-only kinds: {kinds:?}");
    let tables = s.catalog_list(&ro.id, &here, ObjectKind::Table, None).await;
    assert!(tables.is_ok(), "read-only list: {tables:?}");
    let object = ObjectRef {
        kind: ObjectKind::Table,
        database: Some("varve".into()),
        schema: Some("m2_ipc_safety".into()),
        parent: None,
        name: "t".into(),
        signature: None,
    };
    let described = s.catalog_describe(&ro.id, &object).await;
    assert!(described.is_ok(), "read-only describe: {described:?}");
    let ddl = s.catalog_ddl(&ro.id, &object).await;
    assert!(ddl.is_ok(), "read-only ddl: {ddl:?}");
    // EXPLAIN ANALYZE of a read on a read-only session: the server cannot
    // resolve the statement's names there (the check's temporary view
    // cannot be created in a read-only transaction), so the read asks for
    // confirmation with exactly that reason, and the confirmed run works
    // and stays read-only.
    let asked = s
        .explain(&ro.id, "SELECT * FROM m2_ipc_safety.t", &analyze, false)
        .await;
    let Err(CmdError::Session {
        error: SessionError::ConfirmationRequired { statements, .. },
    }) = &asked
    else {
        return Err(
            format!("read-only explain analyze: expected a confirmation, got {asked:?}").into(),
        );
    };
    assert!(
        statements
            .iter()
            .any(|w| w == varve_catalog::explain::READ_ONLY_CONNECTION_REASON),
        "read-only explain analyze: the dialog does not give the reason: {statements:?}"
    );
    let explained = s
        .explain(&ro.id, "SELECT * FROM m2_ipc_safety.t", &analyze, true)
        .await;
    assert!(
        explained.as_ref().is_ok_and(|p| p.analyzed),
        "read-only explain analyze, confirmed: {explained:?}"
    );
    // A confirmed run on a read-only session cannot write: a read whose
    // function writes when it runs (the text takes it for a read, and the
    // only reason to confirm is the read-only connection) is stopped by
    // the server's read-only transaction, and a data change is refused
    // up front.
    run(
        s,
        &setup,
        "CREATE FUNCTION m2_ipc_safety.w() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO m2_ipc_safety.t VALUES (3, 'w') RETURNING id $$",
    )
    .await?;
    let before = safety_rows(s, &setup).await?;
    let writing = "SELECT m2_ipc_safety.w()";
    let asked = s.explain(&ro.id, writing, &analyze, false).await;
    let Err(CmdError::Session {
        error: SessionError::ConfirmationRequired { statements, .. },
    }) = &asked
    else {
        return Err(
            format!("read-only writing function: expected a confirmation, got {asked:?}").into(),
        );
    };
    assert!(
        statements
            .iter()
            .any(|w| w == varve_catalog::explain::READ_ONLY_CONNECTION_REASON),
        "read-only writing function: {statements:?}"
    );
    let ran = s.explain(&ro.id, writing, &analyze, true).await;
    assert!(
        matches!(&ran, Err(CmdError::Catalog {
            error: varve_catalog::CatalogError::Driver { error }
        }) if error.kind == varve_driver::ErrorKind::ReadOnly
            && error.code.as_deref() == Some("25006")),
        "read-only writing function, confirmed: expected the server's read-only refusal, got {ran:?}"
    );
    assert!(
        !ran.as_ref().is_ok_and(|p| p.analyzed),
        "read-only writing function, confirmed: ran"
    );
    assert_eq!(
        safety_rows(s, &setup).await?,
        before,
        "a confirmed run on a read-only session wrote"
    );
    assert!(is_read_only(
        &s.explain(
            &ro.id,
            "WITH w AS (INSERT INTO m2_ipc_safety.t VALUES (4, 'x') RETURNING id) SELECT * FROM w",
            &analyze,
            true
        )
        .await
    ));
    assert_eq!(
        safety_rows(s, &setup).await?,
        before,
        "a confirmed data change on a read-only session wrote"
    );

    run(s, &setup, "DROP SCHEMA m2_ipc_safety CASCADE").await?;
    for session in [setup, prod, ro] {
        s.close_session(&session.id).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mysql_and_mariadb_browser_editors_grid_and_panels() -> TestResult {
    let f = fixture("my")?;
    let s = &f.services;
    for target in only_selected(my_targets()) {
        let name = target.name;
        let session = open(s, &target, Environment::Development, false).await?;
        run(s, &session, "DROP DATABASE IF EXISTS m2_ipc").await?;

        let create = EngineChange::Mysql(Box::new(serde_json::from_value(json!({
            "change": "createDatabase", "name": "m2_ipc"
        }))?));
        let planned = s.edit_plan(&session.id, &create)?;
        assert!(!planned.plan.transactional, "{name}");
        s.edit_apply(&session.id, &planned.plan_id, false)
            .await
            .map_err(|e| format!("{name}: create database: {e:?}"))?;
        run(
            s,
            &session,
            "CREATE TABLE m2_ipc.items (id int PRIMARY KEY, label varchar(40) NOT NULL)",
        )
        .await?;
        let transactional = EditPlan {
            statements: vec!["CREATE TABLE m2_ipc.x (a int)".into()],
            transactional: true,
            destructive: false,
            warnings: vec![],
        };
        assert!(matches!(
            s.edit_apply(&session.id, &stored(s, &session, transactional), false)
                .await,
            Err(CmdError::Invalid { .. })
        ));

        let caps = s.catalog_capabilities(&session.id).await?;
        assert_eq!(caps.engine, target.engine, "{name}");
        let databases = s.catalog_databases(&session.id).await?;
        assert!(
            databases.iter().any(|d| d.object.name == "m2_ipc"),
            "{name}"
        );
        assert!(
            s.catalog_schemas(&session.id, "m2_ipc").await?.is_empty(),
            "{name}"
        );
        let here = container("m2_ipc", None);
        let tables = s
            .catalog_list(&session.id, &here, ObjectKind::Table, Some(false))
            .await?;
        let items = tables
            .iter()
            .find(|t| t.object.name == "items")
            .ok_or("items not listed")?;
        let ddl = s.catalog_ddl(&session.id, &items.object).await?;
        assert!(ddl.to_ascii_uppercase().contains("CREATE TABLE"), "{name}");
        s.catalog_describe(&session.id, &items.object).await?;

        let explained = s
            .explain(
                &session.id,
                "SELECT * FROM m2_ipc.items WHERE id = 1",
                &ExplainOptions::default(),
                false,
            )
            .await;
        assert!(explained.is_ok(), "{name}: {explained:?}");

        let table = TableTarget {
            database: "m2_ipc".into(),
            schema: None,
            table: "items".into(),
        };
        let key = s.editable_key(&session.id, &table).await?.ok_or("no key")?;
        let options = PlanOptions {
            check_original: true,
            key,
        };
        let changes: Vec<RowChange> = serde_json::from_value(json!([
            { "op": "insert", "values": [
                ["id", { "kind": "value", "value": { "t": "int", "v": 1 } }],
                ["label", { "kind": "value", "value": { "t": "text", "v": "one" } }]
            ] }
        ]))?;
        let planned = s.row_edit_plan(&session.id, &table, &changes, &options)?;
        let outcome = s
            .row_edit_apply(&session.id, &planned.plan_id, false)
            .await?;
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "{name}: {outcome:?}"
        );

        let processes = s.process_list(&session.id).await?;
        assert!(processes.own_id.is_some(), "{name}");
        let variables = s.server_variables(&session.id).await?;
        assert!(
            variables
                .iter()
                .any(|v| v.name.eq_ignore_ascii_case("sort_buffer_size")),
            "{name}"
        );
        let change: VariableChange = serde_json::from_value(json!({
            "action": "set", "scope": "session", "value": "524288"
        }))?;
        let planned = s
            .variable_change_plan(&session.id, "sort_buffer_size", &change)
            .await?;
        s.variable_change_apply(&session.id, &planned.plan_id, false)
            .await?;
        // A non-transactional DDL plan inside an open transaction is
        // refused: MySQL would commit the transaction implicitly.
        run(s, &session, "START TRANSACTION").await?;
        run(s, &session, "INSERT INTO m2_ipc.items VALUES (50, 'tx')").await?;
        let ddl_in_tx = stored(
            s,
            &session,
            EditPlan {
                statements: vec!["CREATE TABLE m2_ipc.in_tx (a int)".into()],
                transactional: false,
                destructive: false,
                warnings: vec![],
            },
        );
        let refused = s.edit_apply(&session.id, &ddl_in_tx, false).await;
        assert!(
            matches!(
                refused,
                Err(CmdError::Session {
                    error: SessionError::OpenTransaction { .. }
                })
            ),
            "{name}: {refused:?}"
        );
        run(s, &session, "ROLLBACK").await?;
        let kept = text_value(
            s,
            &session,
            "SELECT COUNT(*) FROM m2_ipc.items WHERE id = 50",
        )
        .await?;
        assert!(
            kept.as_deref()
                .is_none_or(|v| v == "0" || v.contains("Int(0)")),
            "{name}: the transaction was committed: {kept:?}"
        );
        s.edit_apply(&session.id, &ddl_in_tx, false)
            .await
            .map_err(|e| format!("{name}: after rollback: {e:?}"))?;
        s.server_status(&session.id).await?;
        let galera = s.galera_status(&session.id).await;
        if target.engine == Engine::Mysql {
            assert!(galera.is_err(), "{name}");
        }

        let drop = EngineChange::Mysql(Box::new(serde_json::from_value(json!({
            "change": "dropDatabase", "name": "m2_ipc"
        }))?));
        let planned = s.edit_plan(&session.id, &drop)?;
        assert!(needs_confirmation(
            &s.edit_apply(&session.id, &planned.plan_id, false).await
        ));
        s.edit_apply(&session.id, &planned.plan_id, true).await?;

        let logged = actions(s, &session.connection_id)?;
        for action in ["schema.changed", "object.dropped", "data.edited"] {
            assert!(
                logged.iter().any(|a| a == action),
                "{name}: {action} in {logged:?}"
            );
        }
        let meta = s
            .metadata_session_id(&session.id)
            .ok_or("no metadata session")?;
        s.close_session(&session.id).await?;
        assert!(!s.catalogs.contains(&meta), "{name}");
        assert!(
            s.sessions.list_all().iter().all(|i| i.id != meta),
            "{name}: metadata session left open"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn redis_panels() -> TestResult {
    let f = fixture("redis")?;
    let s = &f.services;
    for target in only_selected(redis_targets()) {
        let name = target.name;
        let session = open(s, &target, Environment::Development, false).await?;
        run(s, &session, "SET m2_ipc:a 1").await?;
        run(s, &session, "SET m2_ipc:b 22").await?;

        let info = s.redis_info(&session.id).await?;
        assert!(
            info.server.redis_version.is_some() || info.server.valkey_version.is_some(),
            "{name}"
        );
        s.redis_databases(&session.id).await?;
        s.redis_slowlog(&session.id, Some(10)).await?;
        s.redis_slowlog_reset(&session.id).await?;
        s.redis_latency(&session.id).await?;
        s.redis_latency_reset(&session.id, &[]).await?;
        let clients = s.redis_clients(&session.id).await?;
        let own = clients.iter().find(|c| c.is_self).ok_or("no own client")?;
        assert!(
            s.redis_client_kill(&session.id, own.id).await.is_err(),
            "{name}"
        );
        let config = s.redis_config(&session.id, Some("slowlog-max-len")).await?;
        let current = config
            .first()
            .and_then(|c| c.value.clone())
            .ok_or("no config value")?;
        let request: ConfigRequest = serde_json::from_value(json!({
            "kind": "set", "changes": [{ "name": "slowlog-max-len", "value": current }]
        }))?;
        let planned = s.redis_config_plan(&session.id, &request).await?;
        s.redis_plan_apply(&session.id, &planned.plan_id, true)
            .await?;
        assert!(is_unavailable(
            &s.redis_plan_apply(&session.id, &planned.plan_id, true)
                .await
        ));
        let plan = s.redis_config_plan(&session.id, &request).await?;
        let analysis = s
            .redis_memory_analysis(
                &session.id,
                Some(&MemoryScanOptions {
                    pattern: Some("m2_ipc:*".into()),
                    max_keys: 100,
                    time_budget_ms: 5000,
                    scan_count: 100,
                    top_keys: 5,
                    ..MemoryScanOptions::default()
                }),
                None,
            )
            .await?;
        assert!(
            serde_json::to_string(&analysis)?.contains("m2_ipc"),
            "{name}"
        );
        let topology = s.redis_topology(&session.id).await?;
        assert!(
            topology.cluster.is_none() && topology.sentinel.is_none(),
            "{name}"
        );
        let functions = s.redis_functions(&session.id, None, false).await;
        if session.server.major < 7 {
            assert!(functions.is_err(), "{name}");
            assert!(
                s.redis_function_plan(
                    &session.id,
                    &FunctionRequest::Delete {
                        library: "m2_ipc".into()
                    }
                )
                .is_err()
            );
        } else {
            assert!(functions.is_ok(), "{name}: {functions:?}");
        }
        s.redis_pubsub(&session.id, None, Some(10)).await?;
        assert!(s.process_list(&session.id).await.is_err(), "{name}");
        assert!(s.catalog_capabilities(&session.id).await.is_err(), "{name}");

        let ro = open(s, &target, Environment::Development, true).await?;
        assert!(is_read_only(&s.redis_slowlog_reset(&ro.id).await));
        assert!(is_read_only(
            &s.redis_plan_apply(&ro.id, &plan.plan_id, true).await
        ));
        s.redis_info(&ro.id).await?;

        let logged = actions(s, &session.connection_id)?;
        for action in [
            "slowlog.reset",
            "latency.reset",
            "process.killed",
            "query.executed",
        ] {
            assert!(
                logged.iter().any(|a| a == action),
                "{name}: {action} in {logged:?}"
            );
        }
        run(s, &session, "DEL m2_ipc:a m2_ipc:b").await?;
        s.close_session(&ro.id).await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}

async fn text_value(s: &Services, session: &SessionInfo, sql: &str) -> Res<Option<String>> {
    Ok(match first_value(&run(s, session, sql).await?) {
        Some(Value::Text(t)) => Some(t),
        Some(Value::Null) | None => None,
        Some(other) => Some(format!("{other:?}")),
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mysql_ddl_runs_with_its_session_header() -> TestResult {
    let f = fixture("ddl")?;
    let s = &f.services;
    for target in only_selected(my_targets()) {
        let name = target.name;
        let session = open(s, &target, Environment::Development, false).await?;
        run(s, &session, "DROP DATABASE IF EXISTS m2_ipc_ddl").await?;
        run(
            s,
            &session,
            "CREATE DATABASE m2_ipc_ddl CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
        )
        .await?;

        // Session variables apply to this call only.
        let ddl = "-- varve-session: character_set_client=latin1; \
                   collation_connection=latin1_swedish_ci; sql_mode=ANSI_QUOTES\n\
                   CREATE PROCEDURE m2_ipc_ddl.p() SELECT 1";
        let applied = s.ddl_apply(&session.id, ddl, None, false).await?;
        assert!(applied.session.is_some(), "{name}");
        let client = text_value(
            s,
            &session,
            "SELECT CHARACTER_SET_CLIENT FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = 'm2_ipc_ddl' AND ROUTINE_NAME = 'p'",
        )
        .await?;
        assert_eq!(client.as_deref(), Some("latin1"), "{name}");
        let mode = text_value(
            s,
            &session,
            "SELECT SQL_MODE FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = 'm2_ipc_ddl' AND ROUTINE_NAME = 'p'",
        )
        .await?;
        assert!(
            mode.as_deref().is_some_and(|m| m.contains("ANSI_QUOTES")),
            "{name}: {mode:?}"
        );
        let own = text_value(s, &session, "SELECT @@session.character_set_client").await?;
        assert_eq!(own.as_deref(), Some("utf8mb4"), "{name}: session restored");

        // collation_database alters the database for the run and back.
        let ddl = "-- varve-session: collation_database=latin1_swedish_ci\n\
                   CREATE PROCEDURE m2_ipc_ddl.q() SELECT 2";
        assert!(matches!(
            s.ddl_apply(&session.id, ddl, None, true).await,
            Err(CmdError::Invalid { .. })
        ));
        assert!(needs_confirmation(
            &s.ddl_apply(&session.id, ddl, Some("m2_ipc_ddl"), false)
                .await
        ));
        let applied = s
            .ddl_apply(&session.id, ddl, Some("m2_ipc_ddl"), true)
            .await?;
        assert_eq!(
            applied.collation_database.as_deref(),
            Some("m2_ipc_ddl"),
            "{name}"
        );
        let created_with = text_value(
            s,
            &session,
            "SELECT DATABASE_COLLATION FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = 'm2_ipc_ddl' AND ROUTINE_NAME = 'q'",
        )
        .await?;
        assert_eq!(created_with.as_deref(), Some("latin1_swedish_ci"), "{name}");
        let restored = text_value(
            s,
            &session,
            "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
             WHERE SCHEMA_NAME = 'm2_ipc_ddl'",
        )
        .await?;
        assert_eq!(restored.as_deref(), Some("utf8mb4_bin"), "{name}: restored");

        // Without a header the DDL runs through the session manager.
        s.ddl_apply(
            &session.id,
            "CREATE TABLE m2_ipc_ddl.t (a int)",
            None,
            false,
        )
        .await?;
        let ro = open(s, &target, Environment::Development, true).await?;
        assert!(is_read_only(
            &s.ddl_apply(&ro.id, "CREATE TABLE m2_ipc_ddl.u (a int)", None, true)
                .await
        ));
        let logged = actions(s, &session.connection_id)?;
        assert!(
            logged.iter().filter(|a| *a == "schema.changed").count() >= 3,
            "{name}: {logged:?}"
        );
        run(s, &session, "DROP DATABASE m2_ipc_ddl").await?;
        s.close_session(&ro.id).await?;
        s.close_session(&session.id).await?;
    }

    // PostgreSQL has no session header.
    let pg = open(s, &pg_targets()[1], Environment::Development, false).await?;
    assert!(matches!(
        s.ddl_apply(
            &pg.id,
            "-- varve-session: sql_mode=ANSI\nSELECT 1",
            None,
            false
        )
        .await,
        Err(CmdError::Invalid { .. })
    ));
    s.close_session(&pg.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn redis_destructive_commands_are_found_however_they_are_written() -> TestResult {
    let f = fixture("redis-risk")?;
    let s = &f.services;
    for target in only_selected(redis_targets()) {
        let name = target.name;
        // Production: destructive commands need confirmation. None of these
        // scripts is ever confirmed, so nothing is flushed.
        let prod = open(s, &target, Environment::Production, false).await?;
        for script in [
            "FLUSHALL",
            "\"FLUSHALL\"",
            "'flushall' ASYNC",
            "FlushDb",
            "PING\n\"FLUSHDB\"",
            "# comment\nGET m2_ipc:none\n  \"flushall\"  ",
            "SCRIPT FLUSH",
            "\"script\" \"flush\" SYNC",
            "EVAL \"return redis.call('FLUSHALL')\" 0",
        ]
        .into_iter()
        .chain(if prod.server.major >= 7 {
            vec!["FUNCTION FLUSH", "function \"FLUSH\" ASYNC"]
        } else {
            Vec::new()
        }) {
            let result = s.execute(&prod.id, script, false, None).await;
            assert!(
                needs_confirmation(&result),
                "{name}: {script:?} ran without confirmation: {result:?}"
            );
        }
        let read = s.execute(&prod.id, "GET m2_ipc:none", false, None).await;
        assert!(read.is_ok(), "{name}: {read:?}");
        let logged = actions(s, &prod.connection_id)?;
        assert!(
            logged
                .iter()
                .filter(|a| *a == "destructive.blocked")
                .count()
                >= 9,
            "{name}: {logged:?}"
        );
        s.close_session(&prod.id).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn typed_secrets_do_not_follow_a_profile_to_another_identity() -> TestResult {
    let f = fixture("typed")?;
    let s = &f.services;
    let target = &pg_targets()[1];
    let profile = s.create_connection(&input(target, Environment::Development, false))?;
    // The password is typed, not saved.
    let session = s
        .open_session(&profile.id, Some(secret("varve_dev")?), None, None)
        .await?;
    s.catalog_databases(&session.id).await?;
    let meta = s
        .metadata_session_id(&session.id)
        .ok_or("no metadata session")?;
    assert!(s.metadata.has_typed(&profile.id));

    // A rename keeps the identity: secrets and pooled sessions stay.
    let mut renamed = input(target, Environment::Development, false);
    renamed.name = "renamed m2_ipc".into();
    s.update_connection(&profile.id, &renamed).await?;
    assert!(s.metadata.has_typed(&profile.id));
    assert_eq!(
        s.metadata_session_id(&session.id).as_deref(),
        Some(meta.as_str())
    );

    // Another host (here the same server under another name) is another
    // identity: typed secrets are dropped and pooled sessions closed, so
    // browsing asks for the password instead of sending it there.
    let mut moved = renamed.clone();
    moved.host = "localhost".into();
    s.update_connection(&profile.id, &moved).await?;
    assert!(!s.metadata.has_typed(&profile.id));
    assert!(
        s.sessions.list_all().iter().all(|i| i.id != meta),
        "pooled session left open"
    );
    // The old tab is on the old server: browsing from it would open a
    // session with the edited profile, so it must reconnect.
    let browse = s.catalog_databases(&session.id).await;
    assert!(
        matches!(browse, Err(CmdError::ReconnectRequired { .. })),
        "{browse:?}"
    );

    // Deleting the profile drops everything too.
    s.close_session(&session.id).await?;
    let again = s
        .open_session(&profile.id, Some(secret("varve_dev")?), None, None)
        .await?;
    s.catalog_databases(&again.id).await?;
    let meta = s.metadata_session_id(&again.id).ok_or("no metadata")?;
    s.delete_connection(&profile.id).await?;
    assert!(!s.metadata.has_typed(&profile.id));
    assert!(s.sessions.list_all().iter().all(|i| i.id != meta));
    s.close_session(&again.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn ddl_collation_is_restored_after_failure_cancel_and_concurrency() -> TestResult {
    let f = fixture("ddl-restore")?;
    let s = &f.services;
    for target in [&my_targets()[0], &my_targets()[4]] {
        let name = target.name;
        let session = open(s, target, Environment::Development, false).await?;
        run(s, &session, "DROP DATABASE IF EXISTS m2_ipc_restore").await?;
        run(
            s,
            &session,
            "CREATE DATABASE m2_ipc_restore CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
        )
        .await?;
        let collation = "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
                         WHERE SCHEMA_NAME = 'm2_ipc_restore'";

        // A DDL that fails.
        let failing = "-- varve-session: collation_database=latin1_swedish_ci\n\
                       CREATE PROCEDURE m2_ipc_restore.p( BEGIN";
        assert!(
            s.ddl_apply(&session.id, failing, Some("m2_ipc_restore"), true)
                .await
                .is_err()
        );
        assert_eq!(
            text_value(s, &session, collation).await?.as_deref(),
            Some("utf8mb4_bin"),
            "{name}: not restored after a failure"
        );

        // A DDL that is cancelled while it runs.
        let slow = "-- varve-session: collation_database=latin1_swedish_ci\n\
                    CREATE TABLE m2_ipc_restore.slow AS SELECT SLEEP(5) AS s";
        let runner = {
            let services = Arc::clone(s);
            let id = session.id.clone();
            tokio::spawn(async move {
                services
                    .ddl_apply(&id, slow, Some("m2_ipc_restore"), true)
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        s.cancel(&session.id).await?;
        // A cancelled SLEEP ends early without an error, so the run may
        // report success; either way it must be over and restored.
        let started = std::time::Instant::now();
        let _cancelled = runner.await?;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "{name}: the cancel did not stop the DDL"
        );
        assert_eq!(
            text_value(s, &session, collation).await?.as_deref(),
            Some("utf8mb4_bin"),
            "{name}: not restored after a cancel"
        );

        // Two runs on the same database from two sessions are serialized:
        // both objects get the header collation and the database its own.
        let second = s
            .open_session(
                &session.connection_id,
                Some(secret("varve_dev")?),
                None,
                None,
            )
            .await?;
        let a = "-- varve-session: collation_database=latin1_swedish_ci\n\
                 CREATE PROCEDURE m2_ipc_restore.a() SELECT 1";
        let b = "-- varve-session: collation_database=latin1_swedish_ci\n\
                 CREATE PROCEDURE m2_ipc_restore.b() SELECT 2";
        let (ra, rb) = tokio::join!(
            s.ddl_apply(&session.id, a, Some("m2_ipc_restore"), true),
            s.ddl_apply(&second.id, b, Some("m2_ipc_restore"), true)
        );
        ra?;
        rb?;
        assert_eq!(
            text_value(s, &session, collation).await?.as_deref(),
            Some("utf8mb4_bin"),
            "{name}: not restored after concurrent runs"
        );
        for routine in ["a", "b"] {
            let made = text_value(
                s,
                &session,
                &format!(
                    "SELECT DATABASE_COLLATION FROM information_schema.ROUTINES \
                     WHERE ROUTINE_SCHEMA = 'm2_ipc_restore' AND ROUTINE_NAME = '{routine}'"
                ),
            )
            .await?;
            assert_eq!(
                made.as_deref(),
                Some("latin1_swedish_ci"),
                "{name}: {routine}"
            );
        }
        assert!(s.ddl_pending_restores().is_empty(), "{name}");

        // DDL inside an open transaction is refused.
        run(s, &session, "START TRANSACTION").await?;
        run(s, &session, "SELECT 1").await?;
        let in_tx = s
            .ddl_apply(
                &session.id,
                "CREATE TABLE m2_ipc_restore.t (a int)",
                None,
                false,
            )
            .await;
        assert!(
            matches!(
                in_tx,
                Err(CmdError::Session {
                    error: SessionError::OpenTransaction { .. }
                })
            ),
            "{name}: DDL inside a transaction: {in_tx:?}"
        );
        run(s, &session, "ROLLBACK").await?;

        run(s, &session, "DROP DATABASE m2_ipc_restore").await?;
        s.close_session(&second.id).await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn pending_collation_restore_flow_survives_a_restart() -> TestResult {
    let f = fixture("pending")?;
    let s = &f.services;
    let target = &my_targets()[0];
    let session = open(s, target, Environment::Development, false).await?;
    run(s, &session, "DROP DATABASE IF EXISTS m2_ipc_pending").await?;
    run(
        s,
        &session,
        "CREATE DATABASE m2_ipc_pending CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
    )
    .await?;
    // The DDL drops its own database, so restoring its collation fails.
    let ddl = "-- varve-session: collation_database=latin1_swedish_ci\n\
               DROP DATABASE m2_ipc_pending";
    let applied = s
        .ddl_apply(&session.id, ddl, Some("m2_ipc_pending"), true)
        .await?;
    assert_eq!(applied.warnings.len(), 1, "{applied:?}");
    let pending = s.ddl_pending_restores();
    assert!(
        pending
            .iter()
            .any(|p| p.database == "m2_ipc_pending" && p.collation == "utf8mb4_bin"),
        "{pending:?}"
    );
    let logged = actions(s, &session.connection_id)?;
    assert!(
        logged
            .iter()
            .any(|a| a == "schema.collation_restore_failed")
    );

    // After a restart the pending restore is still known.
    let restarted = services_on(&f.dir)?;
    assert_eq!(restarted.ddl_pending_restores(), pending);

    // Once the database exists again, the retry restores its collation.
    run(
        s,
        &session,
        "CREATE DATABASE m2_ipc_pending CHARACTER SET latin1 COLLATE latin1_swedish_ci",
    )
    .await?;
    s.ddl_restore_collation(&session.id, "m2_ipc_pending", false)
        .await?;
    let restored = text_value(
        s,
        &session,
        "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
         WHERE SCHEMA_NAME = 'm2_ipc_pending'",
    )
    .await?;
    assert_eq!(restored.as_deref(), Some("utf8mb4_bin"));
    assert!(s.ddl_pending_restores().is_empty());
    assert!(services_on(&f.dir)?.ddl_pending_restores().is_empty());
    assert!(matches!(
        s.ddl_restore_collation(&session.id, "m2_ipc_pending", false)
            .await,
        Err(CmdError::NotFound { .. })
    ));
    run(s, &session, "DROP DATABASE m2_ipc_pending").await?;
    s.close_session(&session.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mysql_tab_kills_its_own_query_and_grid_respects_transactions() -> TestResult {
    let f = fixture("my-tab")?;
    let s = &f.services;
    for target in [&my_targets()[0], &my_targets()[4]] {
        let name = target.name;
        let session = open(s, target, Environment::Development, false).await?;
        run(s, &session, "DROP DATABASE IF EXISTS m2_ipc_tab").await?;
        run(s, &session, "CREATE DATABASE m2_ipc_tab").await?;
        run(
            s,
            &session,
            "CREATE TABLE m2_ipc_tab.items (id int PRIMARY KEY, label varchar(20) NOT NULL)",
        )
        .await?;

        // Kill the tab's own running query from the process list.
        let own = int_of(first_value(
            &run(s, &session, "SELECT CONNECTION_ID()").await?,
        ))
        .ok_or("no connection id")?;
        let started = std::time::Instant::now();
        let sleeper = {
            let services = Arc::clone(s);
            let id = session.id.clone();
            tokio::spawn(
                async move { services.execute(&id, "SELECT SLEEP(20)", false, None).await },
            )
        };
        let mut listed = false;
        for _ in 0..50 {
            let processes = s.process_list(&session.id).await?;
            if processes
                .processes
                .iter()
                .any(|p| p.id == own && p.query.as_deref().is_some_and(|q| q.contains("SLEEP")))
            {
                listed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(listed, "{name}: the tab's query is not listed");
        s.kill_process(&session.id, own, KillTarget::Query).await?;
        let _ = sleeper.await?;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "{name}: the query was not stopped"
        );

        // Grid changes inside the user's open transaction are refused and
        // the user's transaction is left alone.
        let table = TableTarget {
            database: "m2_ipc_tab".into(),
            schema: None,
            table: "items".into(),
        };
        let key = s.editable_key(&session.id, &table).await?.ok_or("no key")?;
        let changes: Vec<RowChange> = serde_json::from_value(json!([
            { "op": "insert", "values": [
                ["id", { "kind": "value", "value": { "t": "int", "v": 1 } }],
                ["label", { "kind": "value", "value": { "t": "text", "v": "grid" } }]
            ] }
        ]))?;
        let planned = s.row_edit_plan(
            &session.id,
            &table,
            &changes,
            &PlanOptions {
                check_original: false,
                key,
            },
        )?;
        run(s, &session, "START TRANSACTION").await?;
        run(
            s,
            &session,
            "INSERT INTO m2_ipc_tab.items VALUES (2, 'user')",
        )
        .await?;
        let refused = s.row_edit_apply(&session.id, &planned.plan_id, false).await;
        assert!(
            matches!(
                refused,
                Err(CmdError::Session {
                    error: SessionError::OpenTransaction { .. }
                })
            ),
            "{name}: {refused:?}"
        );
        run(s, &session, "ROLLBACK").await?;
        let count = int_of(first_value(
            &run(s, &session, "SELECT COUNT(*) FROM m2_ipc_tab.items").await?,
        ));
        assert_eq!(
            count,
            Some(0),
            "{name}: the user's transaction was committed"
        );
        // The plan was kept and applies once the transaction is gone.
        let outcome = s
            .row_edit_apply(&session.id, &planned.plan_id, false)
            .await?;
        assert!(matches!(outcome, ApplyOutcome::Applied { .. }), "{name}");
        let logged = actions(s, &session.connection_id)?;
        assert!(logged.iter().any(|a| a == "policy.blocked"), "{name}");

        run(s, &session, "DROP DATABASE m2_ipc_tab").await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn a_tab_keeps_its_server_after_the_profile_is_edited() -> TestResult {
    let f = fixture("tab-identity")?;
    let s = &f.services;
    let target = &pg_targets()[1];
    let profile = s.create_connection(&input(target, Environment::Development, false))?;
    s.save_secret(
        &profile.id,
        varve_records::SecretKindName::DbPassword,
        secret("varve_dev")?,
    )?;
    let tab = s.open_session(&profile.id, None, None, None).await?;
    s.catalog_databases(&tab.id).await?;
    s.process_list(&tab.id).await?;
    assert!(!tab.confirm_destructive);

    // The profile becomes production: the old tab must not be served by
    // pooled sessions of the new settings.
    let mut prod = input(target, Environment::Production, false);
    prod.name = "m2_ipc prod".into();
    s.update_connection(&profile.id, &prod).await?;
    for result in [
        s.catalog_databases(&tab.id).await.map(|_| ()),
        s.process_list(&tab.id).await.map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(CmdError::ReconnectRequired { .. })),
            "{result:?}"
        );
    }
    // A reconnected tab uses the new settings, production rules included.
    let fresh = s.open_session(&profile.id, None, None, None).await?;
    assert!(fresh.confirm_destructive);
    s.catalog_databases(&fresh.id).await?;
    s.process_list(&fresh.id).await?;
    let admin = s.admin_session_id(&fresh.id).ok_or("no admin session")?;
    let admin_info = s
        .sessions
        .list_all()
        .into_iter()
        .find(|i| i.id == admin)
        .ok_or("admin session not open")?;
    assert!(
        admin_info.confirm_destructive,
        "admin runs with production rules"
    );
    s.close_session(&tab.id).await?;
    s.close_session(&fresh.id).await?;
    Ok(())
}

/// Round 3 audit probe r3p1: the collation ALTER, the DDL and the restore
/// run under one lock of the tab's session, so a user transaction queued on
/// the same tab starts only afterwards and is never committed by the
/// restore.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn collation_restore_never_commits_a_queued_user_transaction() -> TestResult {
    let f = fixture("r3p1")?;
    let s = &f.services;
    let target = &my_targets()[0];
    let tab = open(s, target, Environment::Development, false).await?;
    let blocker = open(s, target, Environment::Development, false).await?;
    let observer = open(s, target, Environment::Development, false).await?;
    run(s, &tab, "DROP DATABASE IF EXISTS m2_ipc_r3p1").await?;
    run(s, &tab, "DROP DATABASE IF EXISTS m2_ipc_r3p1_other").await?;
    run(
        s,
        &tab,
        "CREATE DATABASE m2_ipc_r3p1 COLLATE utf8mb4_general_ci",
    )
    .await?;
    run(s, &tab, "CREATE TABLE m2_ipc_r3p1.t (id int PRIMARY KEY)").await?;
    run(s, &tab, "CREATE DATABASE m2_ipc_r3p1_other").await?;
    run(
        s,
        &tab,
        "CREATE TABLE m2_ipc_r3p1_other.log (id int PRIMARY KEY) ENGINE=InnoDB",
    )
    .await?;
    // Holds the ALTER DATABASE on the schema lock, so the user's script
    // is queued on the tab while the DDL is in progress.
    run(s, &blocker, "LOCK TABLES m2_ipc_r3p1.t WRITE").await?;
    let ddl = {
        let services = Arc::clone(s);
        let id = tab.id.clone();
        tokio::spawn(async move {
            services
                .ddl_apply(
                    &id,
                    "-- varve-session: collation_database=utf8mb4_bin\n\
                     CREATE TABLE m2_ipc_r3p1.made (id int)",
                    Some("m2_ipc_r3p1"),
                    true,
                )
                .await
        })
    };
    let mut waiting = false;
    for _ in 0..100 {
        let found = int_of(first_value(
            &run(
                s,
                &observer,
                "SELECT COUNT(*) FROM information_schema.PROCESSLIST \
                 WHERE INFO LIKE 'ALTER DATABASE%m2_ipc_r3p1%'",
            )
            .await?,
        ));
        if found.is_some_and(|n| n > 0) {
            waiting = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(waiting, "the ALTER DATABASE never waited on the lock");
    let user = {
        let services = Arc::clone(s);
        let id = tab.id.clone();
        tokio::spawn(async move {
            services
                .execute(
                    &id,
                    "START TRANSACTION; INSERT INTO m2_ipc_r3p1_other.log VALUES (1)",
                    false,
                    None,
                )
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    run(s, &blocker, "UNLOCK TABLES").await?;
    let ddl_result = ddl.await?;
    let user_result = user.await?;
    assert!(ddl_result.is_ok(), "{ddl_result:?}");
    assert!(user_result.is_ok(), "{user_result:?}");
    run(s, &tab, "ROLLBACK").await?;
    let rows = int_of(first_value(
        &run(s, &tab, "SELECT COUNT(*) FROM m2_ipc_r3p1_other.log").await?,
    ));
    assert_eq!(rows, Some(0), "the user's transaction was committed");
    let collation = text_value(
        s,
        &tab,
        "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
         WHERE SCHEMA_NAME = 'm2_ipc_r3p1'",
    )
    .await?;
    assert_eq!(collation.as_deref(), Some("utf8mb4_general_ci"));
    assert!(s.ddl_pending_restores().is_empty());
    run(s, &tab, "DROP DATABASE m2_ipc_r3p1").await?;
    run(s, &tab, "DROP DATABASE m2_ipc_r3p1_other").await?;
    for session in [tab, blocker, observer] {
        s.close_session(&session.id).await?;
    }
    Ok(())
}

/// Round 3 audit probe r3p2: a PostgreSQL tab opened without a database
/// browses the database it is connected to, also after the profile names
/// another one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn pg_tab_browses_its_own_database_after_a_profile_edit() -> TestResult {
    let f = fixture("r3p2")?;
    let s = &f.services;
    let target = &pg_targets()[1];
    let mut no_database = input(target, Environment::Development, false);
    no_database.database = None;
    let profile = s.create_connection(&no_database)?;
    let tab = s
        .open_session(&profile.id, Some(secret("varve_dev")?), None, None)
        .await?;
    let tab_database = text_value(s, &tab, "SELECT current_database()").await?;
    assert_eq!(tab.database, tab_database, "the tab records its database");
    let mut edited = no_database.clone();
    edited.database = Some("postgres".into());
    s.update_connection(&profile.id, &edited).await?;
    s.catalog_databases(&tab.id).await?;
    let meta = s
        .metadata_session_id(&tab.id)
        .ok_or("no metadata session")?;
    let meta_info = s
        .sessions
        .list_all()
        .into_iter()
        .find(|i| i.id == meta)
        .ok_or("metadata session not open")?;
    let meta_database = text_value(s, &meta_info, "SELECT current_database()").await?;
    assert_eq!(meta_database, tab_database, "browser on another database");
    s.close_session(&tab.id).await?;
    Ok(())
}

/// Round 3 audit probe r3p3 (partly blocked on the driver contract): a
/// GRANT that did not take effect is never logged as applied.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn privilege_events_are_never_logged_for_grants_that_did_not_apply() -> TestResult {
    let f = fixture("r3p3")?;
    let s = &f.services;
    let pg = open(s, &pg_targets()[1], Environment::Development, false).await?;
    run(s, &pg, "DROP TABLE IF EXISTS m2_ipc_r3p3").await?;
    run(s, &pg, "CREATE TABLE m2_ipc_r3p3 (id int)").await?;
    // Rolled back with the failed transaction: not applied, not logged.
    let failed = s
        .execute(
            &pg.id,
            "BEGIN; GRANT SELECT ON m2_ipc_r3p3 TO PUBLIC; SELECT 1/0",
            true,
            None,
        )
        .await;
    assert!(failed.is_err());
    run(s, &pg, "ROLLBACK").await?;
    let acl = text_value(
        s,
        &pg,
        "SELECT relacl::text FROM pg_class WHERE relname = 'm2_ipc_r3p3'",
    )
    .await?;
    assert!(
        acl.as_deref().is_none_or(|a| !a.contains("=r/")),
        "the grant applied: {acl:?}"
    );
    assert!(
        !actions(s, &pg.connection_id)?
            .iter()
            .any(|a| a == "privilege.granted"),
        "a rolled back GRANT was logged"
    );
    // A GRANT that committed is logged.
    run(s, &pg, "-- why\nGRANT SELECT ON m2_ipc_r3p3 TO PUBLIC").await?;
    assert!(
        actions(s, &pg.connection_id)?
            .iter()
            .any(|a| a == "privilege.granted")
    );
    run(s, &pg, "DROP TABLE m2_ipc_r3p3").await?;
    s.close_session(&pg.id).await?;
    Ok(())
}

fn count_of(actions: &[String], action: &str) -> usize {
    actions.iter().filter(|a| *a == action).count()
}

/// Round 3 audit finding R3-4: a GRANT or REVOKE that completed before a
/// failing statement is logged when its effect persisted, also when the
/// error has no position (MySQL, MariaDB, PostgreSQL runtime errors), and
/// never when it was rolled back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn privileges_granted_before_a_failure_are_logged_when_they_persisted() -> TestResult {
    let f = fixture("r3-4")?;
    let s = &f.services;

    // PostgreSQL: every statement is sent on its own, so outside a
    // transaction block the GRANT commits before the failure.
    let pg = open(s, &pg_targets()[1], Environment::Development, false).await?;
    run(s, &pg, "DROP TABLE IF EXISTS m2_ipc_r34").await?;
    run(s, &pg, "CREATE TABLE m2_ipc_r34 (id int)").await?;
    let acl = "SELECT coalesce(relacl::text, '') FROM pg_class WHERE relname = 'm2_ipc_r34'";
    let failed = s
        .execute(
            &pg.id,
            "GRANT SELECT ON m2_ipc_r34 TO PUBLIC; SELECT 1/0",
            true,
            None,
        )
        .await;
    let Err(CmdError::Session {
        error: SessionError::Driver { error },
    }) = failed
    else {
        return Err(format!("{failed:?}").into());
    };
    assert_eq!(error.position, None, "{error:?}");
    let partial = error.partial.ok_or("no partial output")?;
    assert_eq!(partial.failed_statement, Some(1));
    assert!(
        text_value(s, &pg, acl)
            .await?
            .is_some_and(|a| a.contains("=r/"))
    );
    assert_eq!(
        count_of(&actions(s, &pg.connection_id)?, "privilege.granted"),
        1
    );

    // Rolled back by the script itself (it succeeded): not logged.
    run(
        s,
        &pg,
        "BEGIN; REVOKE SELECT ON m2_ipc_r34 FROM PUBLIC; ROLLBACK",
    )
    .await?;
    // Aborted by the failure and then rolled back: not logged.
    assert!(
        s.execute(
            &pg.id,
            "BEGIN; REVOKE SELECT ON m2_ipc_r34 FROM PUBLIC; SELECT 1/0",
            true,
            None,
        )
        .await
        .is_err()
    );
    run(s, &pg, "ROLLBACK").await?;
    assert!(
        text_value(s, &pg, acl)
            .await?
            .is_some_and(|a| a.contains("=r/"))
    );
    assert_eq!(
        count_of(&actions(s, &pg.connection_id)?, "privilege.revoked"),
        0,
        "a rolled back REVOKE was logged"
    );
    // Committed inside the script before the failure: logged.
    assert!(
        s.execute(
            &pg.id,
            "BEGIN; REVOKE SELECT ON m2_ipc_r34 FROM PUBLIC; COMMIT; SELECT 1/0",
            true,
            None,
        )
        .await
        .is_err()
    );
    assert!(
        !text_value(s, &pg, acl)
            .await?
            .is_some_and(|a| a.contains("=r/"))
    );
    assert_eq!(
        count_of(&actions(s, &pg.connection_id)?, "privilege.revoked"),
        1
    );
    run(s, &pg, "DROP TABLE m2_ipc_r34").await?;
    s.close_session(&pg.id).await?;

    // MySQL and MariaDB: errors carry no position; a GRANT commits
    // implicitly, also inside a transaction.
    for target in [&my_targets()[0], &my_targets()[4]] {
        let name = target.name;
        let my = open(s, target, Environment::Development, false).await?;
        run(s, &my, "CREATE DATABASE IF NOT EXISTS m2_ipc_r34").await?;
        run(s, &my, "CREATE TABLE IF NOT EXISTS m2_ipc_r34.t (id int)").await?;
        let grants = "SELECT COUNT(*) FROM mysql.tables_priv                       WHERE Db = 'm2_ipc_r34' AND Table_name = 't'";
        let failed = s
            .execute(
                &my.id,
                "GRANT SELECT ON m2_ipc_r34.t TO 'varve'@'%'; SELECT * FROM m2_ipc_r34.missing",
                true,
                None,
            )
            .await;
        let Err(CmdError::Session {
            error: SessionError::Driver { error },
        }) = failed
        else {
            return Err(format!("{name}: {failed:?}").into());
        };
        assert_eq!(error.position, None, "{name}: {error:?}");
        assert_eq!(
            error.partial.ok_or("no partial output")?.failed_statement,
            Some(1)
        );
        assert_eq!(
            int_of(first_value(&run(s, &my, grants).await?)),
            Some(1),
            "{name}"
        );
        assert_eq!(
            count_of(&actions(s, &my.connection_id)?, "privilege.granted"),
            1,
            "{name}"
        );
        assert!(
            s.execute(
                &my.id,
                "START TRANSACTION; REVOKE SELECT ON m2_ipc_r34.t FROM 'varve'@'%'; \
                 SELECT * FROM m2_ipc_r34.missing",
                true,
                None,
            )
            .await
            .is_err()
        );
        run(s, &my, "ROLLBACK").await?;
        assert_eq!(
            int_of(first_value(&run(s, &my, grants).await?)),
            Some(0),
            "{name}"
        );
        assert_eq!(
            count_of(&actions(s, &my.connection_id)?, "privilege.revoked"),
            1,
            "{name}"
        );
        run(s, &my, "DROP DATABASE m2_ipc_r34").await?;
        s.close_session(&my.id).await?;
    }
    Ok(())
}

/// Partial results audit round 4: on PostgreSQL every DO block and every
/// CALL that ran is one `privilege.unverified` event per script, whatever
/// the code it runs (a procedure called from a DO block, a function through
/// PERFORM, a BEGIN ATOMIC procedure, one found through search_path or
/// redefined by another session); ALTER GROUP ... ADD / DROP USER is a
/// `privilege.granted` / `privilege.revoked` event once committed. Whenever
/// the table's privileges or the group's members change, an event says so.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn every_do_block_and_call_is_in_doubt() -> TestResult {
    let f = fixture("pc-do")?;
    let s = &f.services;
    let pg = open(s, &pg_targets()[1], Environment::Development, false).await?;
    let other = open(s, &pg_targets()[1], Environment::Development, false).await?;
    let count =
        |action: &str| -> Res<usize> { Ok(count_of(&actions(s, &pg.connection_id)?, action)) };
    // The table's privileges and the group's members.
    let state = || async {
        text_value(
            s,
            &other,
            "SELECT concat_ws(' | ', \
             (SELECT relacl::text FROM pg_catalog.pg_class WHERE oid = 'public.m2_ipc_pcdo'::regclass), \
             (SELECT string_agg(u.rolname, ',' ORDER BY u.rolname) \
                FROM pg_catalog.pg_auth_members m \
                JOIN pg_catalog.pg_roles g ON g.oid = m.roleid \
                JOIN pg_catalog.pg_roles u ON u.oid = m.member \
               WHERE g.rolname = 'm2_ipc_grp'))",
        )
        .await
    };
    run(
        s,
        &pg,
        "DROP TABLE IF EXISTS m2_ipc_pcdo; DROP SCHEMA IF EXISTS m2_ipc_s CASCADE; \
         DROP ROLE IF EXISTS m2_ipc_usr; DROP ROLE IF EXISTS m2_ipc_grp",
    )
    .await?;
    run(
        s,
        &pg,
        "CREATE TABLE m2_ipc_pcdo (id int); CREATE ROLE m2_ipc_grp; CREATE ROLE m2_ipc_usr; \
         CREATE SCHEMA m2_ipc_s",
    )
    .await?;
    run(
        s,
        &pg,
        "CREATE OR REPLACE PROCEDURE m2_ipc_noop() LANGUAGE plpgsql AS $$ BEGIN PERFORM 1; END $$",
    )
    .await?;
    run(
        s,
        &pg,
        "CREATE OR REPLACE PROCEDURE m2_ipc_grantp() LANGUAGE plpgsql AS \
         $$ BEGIN GRANT SELECT ON m2_ipc_pcdo TO m2_ipc_usr; END $$",
    )
    .await?;
    run(
        s,
        &pg,
        "CREATE OR REPLACE FUNCTION m2_ipc_revf() RETURNS int LANGUAGE plpgsql AS \
         $$ BEGIN REVOKE SELECT ON m2_ipc_pcdo FROM m2_ipc_usr; RETURN 1; END $$",
    )
    .await?;
    run(
        s,
        &pg,
        "CREATE OR REPLACE FUNCTION m2_ipc_grantf() RETURNS int LANGUAGE plpgsql AS \
         $$ BEGIN GRANT INSERT ON m2_ipc_pcdo TO m2_ipc_usr; RETURN 1; END $$",
    )
    .await?;
    run(
        s,
        &pg,
        "CREATE OR REPLACE PROCEDURE m2_ipc_atomic() LANGUAGE sql \
         BEGIN ATOMIC SELECT m2_ipc_grantf(); END",
    )
    .await?;
    run(
        s,
        &pg,
        "CREATE OR REPLACE PROCEDURE m2_ipc_s.m2_ipc_noop() LANGUAGE plpgsql AS \
         $$ BEGIN GRANT UPDATE ON public.m2_ipc_pcdo TO m2_ipc_usr; END $$",
    )
    .await?;
    // (script, may fail, (unverified, granted, revoked) events it adds).
    let cases: [(&str, bool, (usize, usize, usize)); 13] = [
        ("DO $$ BEGIN PERFORM 'grant'; END $$", false, (1, 0, 0)),
        ("CALL m2_ipc_noop()", false, (1, 0, 0)),
        ("CALL m2_ipc_noop(); CALL m2_ipc_noop()", false, (1, 0, 0)),
        ("DO $$ BEGIN CALL m2_ipc_grantp(); END $$", false, (1, 0, 0)),
        (
            "DO $$ BEGIN PERFORM m2_ipc_revf(); END $$",
            false,
            (1, 0, 0),
        ),
        ("CALL m2_ipc_atomic()", false, (1, 0, 0)),
        (
            "SET search_path = m2_ipc_s, public; CALL m2_ipc_noop(); RESET search_path",
            false,
            (1, 0, 0),
        ),
        // Redefined by the other session (below), then called here.
        ("CALL m2_ipc_noop()", false, (1, 0, 0)),
        // A DO block that failed after it started (over-reporting).
        (
            "DO $$ BEGIN GRANT DELETE ON m2_ipc_pcdo TO m2_ipc_usr; RAISE EXCEPTION 'no'; END $$",
            true,
            (1, 0, 0),
        ),
        (
            "ALTER GROUP m2_ipc_grp ADD USER m2_ipc_usr",
            false,
            (0, 1, 0),
        ),
        (
            "ALTER GROUP m2_ipc_grp DROP USER m2_ipc_usr",
            false,
            (0, 0, 1),
        ),
        (
            "BEGIN; ALTER GROUP m2_ipc_grp ADD USER m2_ipc_usr; ROLLBACK",
            false,
            (0, 0, 0),
        ),
        (
            "BEGIN; ALTER GROUP m2_ipc_grp ADD USER m2_ipc_usr; COMMIT",
            false,
            (0, 1, 0),
        ),
    ];
    for (at, (sql, may_fail, (unverified, granted, revoked))) in cases.into_iter().enumerate() {
        if at == 7 {
            run(
                s,
                &other,
                "CREATE OR REPLACE PROCEDURE public.m2_ipc_noop() LANGUAGE plpgsql AS \
                 $$ BEGIN GRANT TRUNCATE ON public.m2_ipc_pcdo TO m2_ipc_usr; END $$",
            )
            .await?;
        }
        let before = (
            count("privilege.unverified")?,
            count("privilege.granted")?,
            count("privilege.revoked")?,
        );
        let state_before = state().await?;
        let result = run(s, &pg, sql).await;
        assert!(may_fail || result.is_ok(), "{sql}: {result:?}");
        let after = (
            count("privilege.unverified")?,
            count("privilege.granted")?,
            count("privilege.revoked")?,
        );
        assert_eq!(
            after,
            (
                before.0 + unverified,
                before.1 + granted,
                before.2 + revoked
            ),
            "privilege events after {sql}"
        );
        if after == before {
            assert_eq!(
                state().await?,
                state_before,
                "{sql} changed privileges silently"
            );
        }
    }
    // The cases did change the table's privileges and the group.
    let state = state().await?.unwrap_or_default();
    for expected in ["m2_ipc_usr=", "| m2_ipc_usr"] {
        assert!(state.contains(expected), "{expected} in {state}");
    }
    run(
        s,
        &pg,
        "DROP SCHEMA m2_ipc_s CASCADE; DROP PROCEDURE m2_ipc_noop(); \
         DROP PROCEDURE m2_ipc_grantp(); DROP PROCEDURE m2_ipc_atomic(); \
         DROP FUNCTION m2_ipc_revf(); DROP FUNCTION m2_ipc_grantf(); DROP TABLE m2_ipc_pcdo; \
         DROP ROLE m2_ipc_usr; DROP ROLE m2_ipc_grp",
    )
    .await?;
    s.close_session(&other.id).await?;
    s.close_session(&pg.id).await?;
    Ok(())
}

/// Round 3 audit probe r3p4: after a failed restore, a header asking for
/// the database's own collation still changes the leftover temporary
/// collation first, so the object gets the collation it asked for.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn collation_is_compared_with_the_current_one_not_the_recorded_one() -> TestResult {
    let f = fixture("r3p4")?;
    let s = &f.services;
    let tab = open(s, &my_targets()[0], Environment::Development, false).await?;
    run(s, &tab, "DROP DATABASE IF EXISTS m2_ipc_r3p4").await?;
    run(
        s,
        &tab,
        "CREATE DATABASE m2_ipc_r3p4 CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
    )
    .await?;
    let applied = s
        .ddl_apply(
            &tab.id,
            "-- varve-session: collation_database=latin1_swedish_ci\n\
             DROP DATABASE m2_ipc_r3p4",
            Some("m2_ipc_r3p4"),
            true,
        )
        .await?;
    assert_eq!(applied.warnings.len(), 1);
    // As a failed restore leaves it: at the temporary collation.
    run(
        s,
        &tab,
        "CREATE DATABASE m2_ipc_r3p4 CHARACTER SET latin1 COLLATE latin1_swedish_ci",
    )
    .await?;
    s.ddl_apply(
        &tab.id,
        "-- varve-session: collation_database=utf8mb4_bin\n\
         CREATE PROCEDURE m2_ipc_r3p4.p() SELECT 1",
        Some("m2_ipc_r3p4"),
        true,
    )
    .await?;
    let made = text_value(
        s,
        &tab,
        "SELECT DATABASE_COLLATION FROM information_schema.ROUTINES \
         WHERE ROUTINE_SCHEMA = 'm2_ipc_r3p4' AND ROUTINE_NAME = 'p'",
    )
    .await?;
    assert_eq!(made.as_deref(), Some("utf8mb4_bin"));
    let now = text_value(
        s,
        &tab,
        "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
         WHERE SCHEMA_NAME = 'm2_ipc_r3p4'",
    )
    .await?;
    assert_eq!(now.as_deref(), Some("utf8mb4_bin"));
    assert!(s.ddl_pending_restores().is_empty());
    run(s, &tab, "DROP DATABASE m2_ipc_r3p4").await?;
    s.close_session(&tab.id).await?;
    Ok(())
}

/// R3-2 and R5-1: pending restores belong to the server as it identifies
/// itself and the route that reached it: another server with a database
/// of the same name does not see them, and neither does another route.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn pending_restores_follow_the_server_not_its_address() -> TestResult {
    let f = fixture("r3-server")?;
    let s = &f.services;
    let mysql80 = &my_targets()[0];
    let tab = open(s, mysql80, Environment::Development, false).await?;
    run(s, &tab, "DROP DATABASE IF EXISTS m2_ipc_r3srv").await?;
    run(
        s,
        &tab,
        "CREATE DATABASE m2_ipc_r3srv CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
    )
    .await?;
    s.ddl_apply(
        &tab.id,
        "-- varve-session: collation_database=latin1_swedish_ci\n\
         DROP DATABASE m2_ipc_r3srv",
        Some("m2_ipc_r3srv"),
        true,
    )
    .await?;
    let pending = s.ddl_pending_restores();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].server.starts_with("mysql:"), "{pending:?}");

    // Another server with a database of the same name: not its restore.
    let other = open(s, &my_targets()[2], Environment::Development, false).await?;
    run(s, &other, "DROP DATABASE IF EXISTS m2_ipc_r3srv").await?;
    run(
        s,
        &other,
        "CREATE DATABASE m2_ipc_r3srv CHARACTER SET latin1 COLLATE latin1_swedish_ci",
    )
    .await?;
    assert!(matches!(
        s.ddl_restore_collation(&other.id, "m2_ipc_r3srv", false)
            .await,
        Err(CmdError::NotFound { .. })
    ));
    let untouched = text_value(
        s,
        &other,
        "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
         WHERE SCHEMA_NAME = 'm2_ipc_r3srv'",
    )
    .await?;
    assert_eq!(untouched.as_deref(), Some("latin1_swedish_ci"));

    // Another route to the same server sees the restore (one entry per
    // server and database), but cannot prove it is the same server (copies
    // of one data directory share the server_uuid): it asks to confirm.
    // The route that recorded it restores without asking.
    let mut alias = input(mysql80, Environment::Development, false);
    alias.host = "localhost".into();
    let alias_profile = s.create_connection(&alias)?;
    let aliased = s
        .open_session(&alias_profile.id, Some(secret("varve_dev")?), None, None)
        .await?;
    run(
        s,
        &aliased,
        "CREATE DATABASE m2_ipc_r3srv CHARACTER SET latin1 COLLATE latin1_swedish_ci",
    )
    .await?;
    assert!(needs_confirmation(
        &s.ddl_restore_collation(&aliased.id, "m2_ipc_r3srv", false)
            .await
    ));
    s.ddl_restore_collation(&tab.id, "m2_ipc_r3srv", false)
        .await?;
    let restored = text_value(
        s,
        &tab,
        "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
         WHERE SCHEMA_NAME = 'm2_ipc_r3srv'",
    )
    .await?;
    assert_eq!(restored.as_deref(), Some("utf8mb4_bin"));
    assert!(s.ddl_pending_restores().is_empty());
    run(s, &tab, "DROP DATABASE m2_ipc_r3srv").await?;
    run(s, &other, "DROP DATABASE m2_ipc_r3srv").await?;
    for session in [tab, other, aliased] {
        s.close_session(&session.id).await?;
    }
    Ok(())
}

/// R3-8 and R3-9: saved passwords do not follow a profile to another
/// server, and the panels reopen an admin session closed under them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn saved_secrets_stay_with_their_server_and_admin_sessions_reopen() -> TestResult {
    let f = fixture("r3-secrets")?;
    let s = &f.services;
    let target = &pg_targets()[1];
    let original = input(target, Environment::Development, false);
    let profile = s.create_connection(&original)?;
    let saved = s.save_secret(
        &profile.id,
        varve_records::SecretKindName::DbPassword,
        secret("varve_dev")?,
    )?;
    assert!(!saved.saved_secrets.is_empty());

    let tab = s.open_session(&profile.id, None, None, None).await?;
    s.process_list(&tab.id).await?;
    let admin = s.admin_session_id(&tab.id).ok_or("no admin session")?;
    s.sessions.close(&admin).await?;
    s.process_list(&tab.id).await?;
    let reopened = s.admin_session_id(&tab.id).ok_or("no admin session")?;
    assert_ne!(reopened, admin);
    s.close_session(&tab.id).await?;

    let mut renamed = original.clone();
    renamed.name = "renamed m2_ipc".into();
    let kept = s.update_connection(&profile.id, &renamed).await?;
    assert_eq!(kept.saved_secrets, saved.saved_secrets, "a rename keeps it");
    let mut moved = renamed.clone();
    moved.host = "localhost".into();
    let dropped = s.update_connection(&profile.id, &moved).await?;
    assert!(
        dropped.saved_secrets.is_empty(),
        "the password followed the profile to another host"
    );
    let reopen = s.open_session(&profile.id, None, None, None).await;
    assert!(reopen.is_err(), "connected without a password: {reopen:?}");
    Ok(())
}

/// A keychain whose deletes fail while `failing` is set, and whose next
/// `set` (when `pause_next_set` is armed) stores the value, reports it on
/// the armed channel and waits to be released before returning. The next
/// `delete` (when `pause_next_delete` is armed) reports on its channel and
/// waits to be released before deleting, like a keychain prompt nobody
/// answers yet.
#[derive(Default)]
struct FlakyKeychain {
    inner: MemoryStore,
    failing: std::sync::atomic::AtomicBool,
    pause_next_set:
        std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    pause_next_delete:
        std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl varve_secrets::SecretStore for FlakyKeychain {
    fn set(
        &self,
        key: &varve_secrets::SecretKey,
        secret: &varve_secrets::Secret,
    ) -> Result<(), varve_secrets::SecretError> {
        self.inner.set(key, secret)?;
        let pause = self.pause_next_set.lock().ok().and_then(|mut p| p.take());
        if let Some((stored, release)) = pause {
            let _ = stored.send(());
            let _ = release.recv();
        }
        Ok(())
    }
    fn get(
        &self,
        key: &varve_secrets::SecretKey,
    ) -> Result<Option<varve_secrets::Secret>, varve_secrets::SecretError> {
        self.inner.get(key)
    }
    fn delete(&self, key: &varve_secrets::SecretKey) -> Result<(), varve_secrets::SecretError> {
        let pause = self
            .pause_next_delete
            .lock()
            .ok()
            .and_then(|mut p| p.take());
        if let Some((started, release)) = pause {
            let _ = started.send(());
            let _ = release.recv();
        }
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(varve_secrets::SecretError::Keychain(
                "the keychain is locked".into(),
            ));
        }
        self.inner.delete(key)
    }
}

/// R4-6: a keychain entry that could not be removed after an edit is no
/// longer listed (never sent), recorded, and removed by a later retry.
#[tokio::test(flavor = "multi_thread")]
async fn failed_keychain_removals_are_retried() -> TestResult {
    let dir = std::env::temp_dir().join(format!("varve-m2-ipc-r4-6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let keychain = Arc::new(FlakyKeychain::default());
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let s = varve_app::build_services(
        store,
        &dir,
        &host,
        Edition::Community,
        Arc::clone(&keychain) as Arc<dyn varve_secrets::SecretStore>,
    )?;
    let original = input(&pg_targets()[1], Environment::Development, false);
    let profile = s.create_connection(&original)?;
    s.save_secret(
        &profile.id,
        varve_records::SecretKindName::DbPassword,
        secret("varve_dev")?,
    )?;
    let key = varve_secrets::SecretKey::new(
        profile.id.clone(),
        varve_records::SecretKindName::DbPassword.into(),
    )?;
    keychain
        .failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut moved = original.clone();
    moved.host = "elsewhere.invalid".into();
    let edited = s.update_connection(&profile.id, &moved).await;
    assert!(
        matches!(edited, Err(CmdError::Keychain { .. })),
        "{edited:?}"
    );
    // The edit is stored and the password is no longer listed.
    let stored = s
        .tree()?
        .connections
        .into_iter()
        .find(|c| c.id == profile.id)
        .ok_or("profile gone")?;
    assert_eq!(stored.host, "elsewhere.invalid");
    assert!(stored.saved_secrets.is_empty());
    assert_eq!(s.pending_keychain_deletions().len(), 1);
    assert!(
        varve_secrets::SecretStore::get(&keychain.inner, &key)?.is_some(),
        "still in the keychain"
    );
    // Still failing: reported, kept.
    assert_eq!(s.retry_keychain_cleanup().len(), 1);
    assert_eq!(s.pending_keychain_deletions().len(), 1);
    // The keychain works again: removed and forgotten.
    keychain
        .failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(s.retry_keychain_cleanup().is_empty());
    assert!(s.pending_keychain_deletions().is_empty());
    assert!(varve_secrets::SecretStore::get(&keychain.inner, &key)?.is_none());
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// R4-1: MariaDB cannot identify itself for sure, so a pending restore is
/// keyed with the route as well, blocks further collation DDL on that
/// database, is applied only with the user's confirmation and only while
/// the database is at the collation the failed run left, and can be
/// dismissed when stale.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mariadb_pending_restores_need_confirmation_and_can_be_dismissed() -> TestResult {
    let f = fixture("r4-1")?;
    let s = &f.services;
    let target = &my_targets()[4];
    let tab = open(s, target, Environment::Development, false).await?;
    let collation_of = "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
                        WHERE SCHEMA_NAME = 'm2_ipc_r41'";
    let fail_restore = "-- varve-session: collation_database=latin1_swedish_ci\n\
                        DROP DATABASE m2_ipc_r41";
    run(s, &tab, "DROP DATABASE IF EXISTS m2_ipc_r41").await?;
    run(
        s,
        &tab,
        "CREATE DATABASE m2_ipc_r41 CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
    )
    .await?;
    let applied = s
        .ddl_apply(&tab.id, fail_restore, Some("m2_ipc_r41"), true)
        .await?;
    assert_eq!(applied.warnings.len(), 1);
    let pending = s.ddl_pending_restores();
    let entry = pending
        .iter()
        .find(|p| p.database == "m2_ipc_r41")
        .ok_or("no pending entry")?
        .clone();
    assert!(!entry.verified);
    assert!(entry.server.starts_with("mariadb:"), "{entry:?}");
    assert_eq!(
        entry.route,
        format!("127.0.0.1:{}", target.port),
        "the route is recorded: {entry:?}"
    );
    assert_eq!(entry.temporaries, ["latin1_swedish_ci"]);

    // The database as the failed restore left it.
    run(
        s,
        &tab,
        "CREATE DATABASE m2_ipc_r41 CHARACTER SET latin1 COLLATE latin1_swedish_ci",
    )
    .await?;
    let blocked = s
        .ddl_apply(
            &tab.id,
            "-- varve-session: collation_database=utf8mb4_bin\n\
             CREATE PROCEDURE m2_ipc_r41.p() SELECT 1",
            Some("m2_ipc_r41"),
            true,
        )
        .await;
    assert!(
        matches!(blocked, Err(CmdError::Invalid { .. })),
        "{blocked:?}"
    );
    assert!(needs_confirmation(
        &s.ddl_restore_collation(&tab.id, "m2_ipc_r41", false).await
    ));
    s.ddl_restore_collation(&tab.id, "m2_ipc_r41", true).await?;
    assert_eq!(
        text_value(s, &tab, collation_of).await?.as_deref(),
        Some("utf8mb4_bin")
    );
    assert!(s.ddl_pending_restores().is_empty());

    // Stale: the database is no longer at the temporary collation.
    run(s, &tab, "DROP DATABASE m2_ipc_r41").await?;
    run(
        s,
        &tab,
        "CREATE DATABASE m2_ipc_r41 CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
    )
    .await?;
    s.ddl_apply(&tab.id, fail_restore, Some("m2_ipc_r41"), true)
        .await?;
    run(
        s,
        &tab,
        "CREATE DATABASE m2_ipc_r41 CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci",
    )
    .await?;
    let stale = s.ddl_restore_collation(&tab.id, "m2_ipc_r41", true).await;
    assert!(matches!(stale, Err(CmdError::Invalid { .. })), "{stale:?}");
    assert_eq!(
        text_value(s, &tab, collation_of).await?.as_deref(),
        Some("utf8mb4_general_ci"),
        "a stale restore changed the database"
    );
    let entry = s
        .ddl_pending_restores()
        .into_iter()
        .find(|p| p.database == "m2_ipc_r41")
        .ok_or("no pending entry")?;
    s.ddl_dismiss_pending_restore(&entry.server, &entry.database)
        .await?;
    assert!(s.ddl_pending_restores().is_empty());
    assert!(matches!(
        s.ddl_dismiss_pending_restore(&entry.server, &entry.database)
            .await,
        Err(CmdError::NotFound { .. })
    ));
    assert!(
        actions(s, &tab.connection_id)?
            .iter()
            .any(|a| a == "schema.collation_restore_dismissed")
    );
    run(s, &tab, "DROP DATABASE m2_ipc_r41").await?;
    s.close_session(&tab.id).await?;
    Ok(())
}

/// R4-3: the collation ALTERs give up after a short lock wait instead of
/// blocking the database for every client, nothing changes when the first
/// one gives up, and a restore that gives up stays pending.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn collation_alters_do_not_block_a_database_in_use() -> TestResult {
    let f = fixture("r4-3")?;
    let s = &f.services;
    let target = &my_targets()[0];
    let tab = open(s, target, Environment::Development, false).await?;
    let holder = open(s, target, Environment::Development, false).await?;
    let collation_of = "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
                        WHERE SCHEMA_NAME = 'm2_ipc_r43'";
    run(s, &tab, "DROP DATABASE IF EXISTS m2_ipc_r43").await?;
    run(
        s,
        &tab,
        "CREATE DATABASE m2_ipc_r43 CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci",
    )
    .await?;
    run(s, &tab, "CREATE TABLE m2_ipc_r43.t (id int PRIMARY KEY)").await?;

    // Another client has a transaction on the database: the first ALTER
    // gives up quickly and nothing changes.
    run(s, &holder, "START TRANSACTION").await?;
    run(s, &holder, "SELECT * FROM m2_ipc_r43.t").await?;
    let started = std::time::Instant::now();
    let refused = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        s.ddl_apply(
            &tab.id,
            "-- varve-session: collation_database=utf8mb4_bin\n\
             CREATE TABLE m2_ipc_r43.made (id int)",
            Some("m2_ipc_r43"),
            true,
        ),
    )
    .await?;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the ALTER waited {:?}",
        started.elapsed()
    );
    let in_use = match &refused {
        Err(CmdError::Session {
            error: SessionError::Driver { error },
        }) => error
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("in use")),
        _ => false,
    };
    assert!(in_use, "{refused:?}");
    run(s, &holder, "ROLLBACK").await?;
    assert_eq!(
        text_value(s, &tab, collation_of).await?.as_deref(),
        Some("utf8mb4_general_ci")
    );
    assert!(s.ddl_pending_restores().is_empty());
    // The session's own lock_wait_timeout is back.
    let timeout = text_value(s, &tab, "SELECT @@SESSION.lock_wait_timeout").await?;
    assert!(
        timeout
            .as_deref()
            .is_some_and(|t| t != "2" && !t.contains("Int(2)")),
        "{timeout:?}"
    );

    // The other client starts its transaction while the DDL runs: the
    // restore gives up quickly and stays pending.
    let ddl = {
        let services = Arc::clone(s);
        let id = tab.id.clone();
        tokio::spawn(async move {
            services
                .ddl_apply(
                    &id,
                    "-- varve-session: collation_database=utf8mb4_bin\n\
                     CREATE TABLE m2_ipc_r43.slow AS SELECT SLEEP(2) AS s",
                    Some("m2_ipc_r43"),
                    true,
                )
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    run(s, &holder, "START TRANSACTION").await?;
    run(s, &holder, "SELECT * FROM m2_ipc_r43.t").await?;
    let started = std::time::Instant::now();
    let applied = tokio::time::timeout(std::time::Duration::from_secs(30), ddl).await??;
    let applied = applied?;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "the restore waited {:?}",
        started.elapsed()
    );
    assert_eq!(applied.warnings.len(), 1, "{applied:?}");
    assert_eq!(s.ddl_pending_restores().len(), 1);
    run(s, &holder, "ROLLBACK").await?;
    s.ddl_restore_collation(&tab.id, "m2_ipc_r43", false)
        .await?;
    assert_eq!(
        text_value(s, &tab, collation_of).await?.as_deref(),
        Some("utf8mb4_general_ci")
    );
    assert!(s.ddl_pending_restores().is_empty());
    run(s, &tab, "DROP DATABASE m2_ipc_r43").await?;
    s.close_session(&holder.id).await?;
    s.close_session(&tab.id).await?;
    Ok(())
}

/// Two throwaway MariaDB containers with the same host name, removed on
/// drop.
struct TwinMariadb {
    names: Vec<String>,
    networks: Vec<String>,
    ports: Vec<u16>,
}

impl TwinMariadb {
    /// Starts two `mariadb:10.6 --hostname db-0` containers on free ports
    /// and waits until both answer. With `same_mac` each runs on its own
    /// docker network with the same MAC address, so they also report the
    /// same `@@server_uid` (a hash of the MAC address and the port).
    fn start(same_mac: bool) -> Res<Self> {
        let mut twins = Self {
            names: Vec::new(),
            networks: Vec::new(),
            ports: Vec::new(),
        };
        let tag = if same_mac { "mac" } else { "own" };
        for side in ["a", "b"] {
            let name = format!("varve-m2-ipc-twin-{tag}-{side}-{}", std::process::id());
            let mut network_args: Vec<String> = Vec::new();
            if same_mac {
                let network = format!("{name}-net");
                let created = std::process::Command::new("docker")
                    .args(["network", "create", &network])
                    .output()?;
                if !created.status.success() {
                    return Err(format!(
                        "docker network create failed: {}",
                        String::from_utf8_lossy(&created.stderr)
                    )
                    .into());
                }
                twins.networks.push(network.clone());
                network_args = vec![
                    "--network".into(),
                    network,
                    "--mac-address".into(),
                    "02:42:ac:1f:00:42".into(),
                ];
            }
            let started = std::process::Command::new("docker")
                .args(["run", "-d", "--name", &name, "--hostname", "db-0"])
                .args(&network_args)
                .args([
                    "-p",
                    "127.0.0.1::3306",
                    "-e",
                    "MARIADB_ROOT_PASSWORD=varve_dev",
                ])
                .arg("mariadb:10.6")
                .output()?;
            if !started.status.success() {
                return Err(format!(
                    "docker run failed: {}",
                    String::from_utf8_lossy(&started.stderr)
                )
                .into());
            }
            twins.names.push(name.clone());
            let port = std::process::Command::new("docker")
                .args(["port", &name, "3306/tcp"])
                .output()?;
            let text = String::from_utf8_lossy(&port.stdout);
            let port: u16 = text
                .lines()
                .next()
                .and_then(|l| l.rsplit(':').next())
                .ok_or("no published port")?
                .trim()
                .parse()?;
            twins.ports.push(port);
        }
        for name in &twins.names {
            let mut ready = false;
            for _ in 0..60 {
                let probe = std::process::Command::new("docker")
                    .args([
                        "exec",
                        name,
                        "mariadb",
                        "-uroot",
                        "-pvarve_dev",
                        "-e",
                        "SELECT 1",
                    ])
                    .output()?;
                if probe.status.success() {
                    ready = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            if !ready {
                return Err(format!("{name} did not start").into());
            }
        }
        Ok(twins)
    }
}

impl Drop for TwinMariadb {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", name])
                .output();
        }
        for network in &self.networks {
            let _ = std::process::Command::new("docker")
                .args(["network", "rm", network])
                .output();
        }
    }
}

/// Round 4 audit probe P1: two MariaDB servers that report the same host
/// name, port, data directory and server id (containers from one image
/// with the same `--hostname`). A failed restore on A must not be applied
/// to B's database of the same name, and stays restorable on A.
///
/// Uses the two servers in `VARVE_M2_TWIN_MARIADB_PORTS` (`port,port`) or,
/// without it, starts its own two `mariadb:10.6 --hostname db-0`
/// containers through docker and removes them afterwards; it fails when
/// neither is possible.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker (or two MariaDB servers in VARVE_M2_TWIN_MARIADB_PORTS)"]
async fn mariadb_twins_with_equal_server_facts_keep_their_own_restores() -> TestResult {
    if let Ok(ports) = std::env::var("VARVE_M2_TWIN_MARIADB_PORTS") {
        let ports: Vec<u16> = ports
            .split(',')
            .map(|p| p.trim().parse())
            .collect::<Result<_, _>>()?;
        let [port_a, port_b] = ports[..] else {
            return Err("VARVE_M2_TWIN_MARIADB_PORTS needs two ports".into());
        };
        return twin_scenario(port_a, port_b, None).await;
    }
    // Twins on one docker host (different MAC addresses, so different
    // @@server_uid), then twins with one MAC address on two networks (the
    // same @@server_uid): both identity branches run.
    for same_mac in [false, true] {
        let twins = tokio::task::spawn_blocking(move || {
            TwinMariadb::start(same_mac)
                .map_err(|e| format!("could not start the twin servers: {e}"))
        })
        .await??;
        let [port_a, port_b] = twins.ports[..] else {
            return Err("two ports expected".into());
        };
        twin_scenario(port_a, port_b, Some(same_mac)).await?;
        drop(twins);
    }
    Ok(())
}

/// The twin scenario on two servers; `same_uid` says which identity branch
/// the servers were set up for (checked against their `@@server_uid`).
async fn twin_scenario(port_a: u16, port_b: u16, same_uid: Option<bool>) -> TestResult {
    let f = fixture("twins")?;
    let s = &f.services;
    let twin = |port| Target {
        name: "MariaDB twin",
        engine: Engine::Mariadb,
        port,
    };
    let a = open(s, &twin(port_a), Environment::Production, false).await?;
    let b = open(s, &twin(port_b), Environment::Development, false).await?;
    let facts = "SELECT CONCAT_WS('|', @@hostname, @@port, @@datadir, @@server_id)";
    assert_eq!(
        text_value(s, &a, facts).await?,
        text_value(s, &b, facts).await?,
        "the twins must report the same facts for this test"
    );
    let collation_of = "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
                        WHERE SCHEMA_NAME = 'm2_ipc_twin'";
    for (session, collation) in [(&a, "utf8mb4_bin"), (&b, "latin1_swedish_ci")] {
        run(s, session, "DROP DATABASE IF EXISTS m2_ipc_twin").await?;
        run(
            s,
            session,
            &format!("CREATE DATABASE m2_ipc_twin COLLATE {collation}"),
        )
        .await?;
    }
    // A failed restore on A leaves A at the temporary collation.
    s.ddl_apply(
        &a.id,
        "-- varve-session: collation_database=utf8mb4_general_ci\n\
         DROP DATABASE m2_ipc_twin",
        Some("m2_ipc_twin"),
        true,
    )
    .await?;
    run(
        s,
        &a,
        "CREATE DATABASE m2_ipc_twin COLLATE utf8mb4_general_ci",
    )
    .await?;
    // B reports the same host name, port, data directory and server id.
    // Its @@server_uid (MAC address and port) differs when the twins run on
    // one docker host, and can be equal across hosts. Either way A's
    // restore is never applied to B: with its own identity B's run is its
    // own business (it ends at B's own collation); with a shared one it is
    // refused while A's entry exists.
    let run_on_b = s
        .ddl_apply(
            &b.id,
            "-- varve-session: collation_database=utf8mb4_general_ci\n\
             CREATE PROCEDURE m2_ipc_twin.p() SELECT 1",
            Some("m2_ipc_twin"),
            true,
        )
        .await;
    let uid = "SELECT @@server_uid";
    let uids_equal = text_value(s, &a, uid).await? == text_value(s, &b, uid).await?;
    if let Some(expected) = same_uid {
        assert_eq!(
            uids_equal, expected,
            "the twins were not set up as intended"
        );
    }
    let shared_identity = match &run_on_b {
        Ok(applied) => {
            assert!(applied.warnings.is_empty(), "{applied:?}");
            false
        }
        Err(CmdError::Invalid { .. }) => true,
        Err(other) => return Err(format!("unexpected result on B: {other:?}").into()),
    };
    assert_eq!(
        shared_identity, uids_equal,
        "the branch must follow @@server_uid: shared identity exactly when it is equal"
    );
    assert_eq!(
        text_value(s, &b, collation_of).await?.as_deref(),
        Some("latin1_swedish_ci"),
        "B got A's collation"
    );
    // A's restore is still there and still works (with confirmation).
    assert_eq!(s.ddl_pending_restores().len(), 1);
    // From B: not B's restore (another identity), or stale (B is not at
    // A's temporary collation).
    let from_b = s.ddl_restore_collation(&b.id, "m2_ipc_twin", true).await;
    if shared_identity {
        assert!(
            matches!(from_b, Err(CmdError::Invalid { .. })),
            "{from_b:?}"
        );
    } else {
        assert!(
            matches!(from_b, Err(CmdError::NotFound { .. })),
            "{from_b:?}"
        );
    }
    s.ddl_restore_collation(&a.id, "m2_ipc_twin", true).await?;
    assert_eq!(
        text_value(s, &a, collation_of).await?.as_deref(),
        Some("utf8mb4_bin")
    );
    for session in [&a, &b] {
        run(s, session, "DROP DATABASE m2_ipc_twin").await?;
    }
    s.close_session(&a.id).await?;
    s.close_session(&b.id).await?;
    Ok(())
}

/// Round 5 audit probe P4: a keychain cleanup running while a secret is
/// saved into the same slot never deletes the fresh secret.
#[tokio::test(flavor = "multi_thread")]
async fn keychain_cleanup_never_deletes_a_secret_being_saved() -> TestResult {
    let dir = std::env::temp_dir().join(format!("varve-m2-ipc-r5-2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let keychain = Arc::new(FlakyKeychain::default());
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let s = varve_app::build_services(
        store,
        &dir,
        &host,
        Edition::Community,
        Arc::clone(&keychain) as Arc<dyn varve_secrets::SecretStore>,
    )?;
    let original = input(&pg_targets()[1], Environment::Development, false);
    let profile = s.create_connection(&original)?;
    s.save_secret(
        &profile.id,
        varve_records::SecretKindName::DbPassword,
        secret("old-server-pw")?,
    )?;
    // A failed removal queues the slot for cleanup.
    keychain
        .failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut moved = original.clone();
    moved.host = "new-server.invalid".into();
    assert!(s.update_connection(&profile.id, &moved).await.is_err());
    keychain
        .failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(s.pending_keychain_deletions().len(), 1);

    // The user saves a password for the new server; a cleanup runs while
    // the keychain write is in progress.
    let (stored_tx, stored_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    if let Ok(mut pause) = keychain.pause_next_set.lock() {
        *pause = Some((stored_tx, release_rx));
    }
    let saver = {
        let s = Arc::clone(&s);
        let id = profile.id.clone();
        let value = secret("new-server-pw")?;
        std::thread::spawn(move || {
            s.save_secret(&id, varve_records::SecretKindName::DbPassword, value)
                .map_err(|e| e.to_string())
        })
    };
    stored_rx.recv_timeout(std::time::Duration::from_secs(5))?;
    let cleaner = {
        let s = Arc::clone(&s);
        std::thread::spawn(move || s.retry_keychain_cleanup())
    };
    std::thread::sleep(std::time::Duration::from_millis(200));
    release_tx.send(())?;
    let saved = saver.join().map_err(|_| "save panicked")??;
    let problems = cleaner.join().map_err(|_| "cleanup panicked")?;
    assert!(problems.is_empty(), "{problems:?}");
    assert!(
        saved
            .saved_secrets
            .contains(&varve_records::SecretKindName::DbPassword)
    );
    let key = varve_secrets::SecretKey::new(
        profile.id.clone(),
        varve_records::SecretKindName::DbPassword.into(),
    )?;
    let kept = varve_secrets::SecretStore::get(&keychain.inner, &key)?;
    assert_eq!(
        kept.as_ref().map(|k| k.expose().to_string()).as_deref(),
        Some("new-server-pw"),
        "the cleanup deleted the secret being saved"
    );
    assert!(s.pending_keychain_deletions().is_empty());
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Round 5 audit probe P1: a pending restore that does not match the
/// database (a server from the same baked data directory, a recreated
/// database) is never overwritten or cleared by a DDL run: the run is
/// refused until the user dismisses it. MySQL keys hold the route too.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn stale_pending_restores_are_never_removed_by_a_run() -> TestResult {
    let f = fixture("r5-1")?;
    let s = &f.services;
    for target in [&my_targets()[0], &my_targets()[1], &my_targets()[2]] {
        let name = target.name;
        let tab = open(s, target, Environment::Development, false).await?;
        run(s, &tab, "DROP DATABASE IF EXISTS m2_ipc_r51").await?;
        run(
            s,
            &tab,
            "CREATE DATABASE m2_ipc_r51 CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
        )
        .await?;
        s.ddl_apply(
            &tab.id,
            "-- varve-session: collation_database=latin1_swedish_ci\n\
             DROP DATABASE m2_ipc_r51",
            Some("m2_ipc_r51"),
            true,
        )
        .await?;
        let pending = s.ddl_pending_restores();
        assert_eq!(pending.len(), 1, "{name}");
        assert_eq!(
            pending[0].route,
            format!("127.0.0.1:{}", target.port),
            "{name}: the route is recorded: {pending:?}"
        );
        // The database no longer looks like the failed run left it.
        run(
            s,
            &tab,
            "CREATE DATABASE m2_ipc_r51 CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci",
        )
        .await?;
        for header in ["latin1_swedish_ci", "utf8mb4_general_ci"] {
            let refused = s
                .ddl_apply(
                    &tab.id,
                    &format!(
                        "-- varve-session: collation_database={header}\n\
                         CREATE TABLE m2_ipc_r51.t (a int)"
                    ),
                    Some("m2_ipc_r51"),
                    true,
                )
                .await;
            assert!(
                matches!(refused, Err(CmdError::Invalid { .. })),
                "{name} {header}: {refused:?}"
            );
            assert_eq!(s.ddl_pending_restores(), pending, "{name}: entry changed");
        }
        s.ddl_dismiss_pending_restore(&pending[0].server, &pending[0].database)
            .await?;
        assert!(s.ddl_pending_restores().is_empty());
        run(s, &tab, "DROP DATABASE m2_ipc_r51").await?;
        s.close_session(&tab.id).await?;
    }
    Ok(())
}

/// Round 5 audit probe P2b: restoring on a server that cannot be
/// identified for sure asks with a text that says so, naming the route,
/// the database, both collations and who recorded it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn unverified_restore_confirmation_says_the_server_is_unproven() -> TestResult {
    let f = fixture("r5-3")?;
    let s = &f.services;
    let target = &my_targets()[4];
    let mut recorder_input = input(target, Environment::Production, false);
    recorder_input.name = "m2_ipc recorder".into();
    let recorder_profile = s.create_connection(&recorder_input)?;
    let recorder = s
        .open_session(&recorder_profile.id, Some(secret("varve_dev")?), None, None)
        .await?;
    run(s, &recorder, "DROP DATABASE IF EXISTS m2_ipc_r53").await?;
    run(
        s,
        &recorder,
        "CREATE DATABASE m2_ipc_r53 CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
    )
    .await?;
    s.ddl_apply(
        &recorder.id,
        "-- varve-session: collation_database=latin1_swedish_ci\n\
         DROP DATABASE m2_ipc_r53",
        Some("m2_ipc_r53"),
        true,
    )
    .await?;
    run(
        s,
        &recorder,
        "CREATE DATABASE m2_ipc_r53 CHARACTER SET latin1 COLLATE latin1_swedish_ci",
    )
    .await?;
    let other = open(s, target, Environment::Development, false).await?;
    let asked = s
        .ddl_restore_collation(&other.id, "m2_ipc_r53", false)
        .await;
    let Err(CmdError::Session {
        error: SessionError::ConfirmationRequired { statements, .. },
    }) = &asked
    else {
        return Err(format!("expected a confirmation, got {asked:?}").into());
    };
    let text = statements.join(" ");
    for part in [
        "cannot prove",
        &format!("127.0.0.1:{}", target.port),
        "m2_ipc_r53",
        "latin1_swedish_ci",
        "utf8mb4_bin",
        "m2_ipc recorder",
    ] {
        assert!(text.contains(part), "{part:?} missing in {text:?}");
    }
    s.ddl_restore_collation(&other.id, "m2_ipc_r53", true)
        .await?;
    assert!(s.ddl_pending_restores().is_empty());
    run(s, &other, "DROP DATABASE m2_ipc_r53").await?;
    s.close_session(&other.id).await?;
    s.close_session(&recorder.id).await?;
    Ok(())
}

/// Round 6 audit probe P1: a run that takes over a valid pending restore
/// and fails at its first ALTER (the database in use) leaves the entry
/// exactly as it was, still restorable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn a_failed_run_keeps_a_valid_pending_restore_valid() -> TestResult {
    let f = fixture("r6-1")?;
    let s = &f.services;
    for target in [&my_targets()[0], &my_targets()[1]] {
        let name = target.name;
        let tab = open(s, target, Environment::Development, false).await?;
        let holder = open(s, target, Environment::Development, false).await?;
        let collation_of = "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
                            WHERE SCHEMA_NAME = 'm2_ipc_r61'";
        run(s, &tab, "DROP DATABASE IF EXISTS m2_ipc_r61").await?;
        run(
            s,
            &tab,
            "CREATE DATABASE m2_ipc_r61 CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
        )
        .await?;
        s.ddl_apply(
            &tab.id,
            "-- varve-session: collation_database=latin1_swedish_ci\n\
             DROP DATABASE m2_ipc_r61",
            Some("m2_ipc_r61"),
            true,
        )
        .await?;
        // As the failed restore left it: valid for the pending entry.
        run(
            s,
            &tab,
            "CREATE DATABASE m2_ipc_r61 CHARACTER SET latin1 COLLATE latin1_swedish_ci",
        )
        .await?;
        run(s, &tab, "CREATE TABLE m2_ipc_r61.t (id int PRIMARY KEY)").await?;
        let before = s.ddl_pending_restores();
        assert_eq!(before.len(), 1, "{name}");
        run(s, &holder, "START TRANSACTION").await?;
        run(s, &holder, "SELECT * FROM m2_ipc_r61.t").await?;
        let failed = s
            .ddl_apply(
                &tab.id,
                "-- varve-session: collation_database=utf8mb4_general_ci\n\
                 CREATE TABLE m2_ipc_r61.made (id int)",
                Some("m2_ipc_r61"),
                true,
            )
            .await;
        assert!(failed.is_err(), "{name}: {failed:?}");
        run(s, &holder, "ROLLBACK").await?;
        assert_eq!(
            text_value(s, &tab, collation_of).await?.as_deref(),
            Some("latin1_swedish_ci"),
            "{name}"
        );
        assert_eq!(s.ddl_pending_restores(), before, "{name}: entry changed");
        s.ddl_restore_collation(&tab.id, "m2_ipc_r61", false)
            .await?;
        assert_eq!(
            text_value(s, &tab, collation_of).await?.as_deref(),
            Some("utf8mb4_bin"),
            "{name}"
        );
        assert!(s.ddl_pending_restores().is_empty(), "{name}");
        run(s, &tab, "DROP DATABASE m2_ipc_r61").await?;
        s.close_session(&holder.id).await?;
        s.close_session(&tab.id).await?;
    }
    Ok(())
}

/// Round 6 audit probe P2: two tabs reaching one server through two names
/// (127.0.0.1 and localhost) are serialized on the server's own identity:
/// each object gets the collation its header asked for and the database
/// ends at its own collation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn runs_through_two_routes_to_one_server_are_serialized() -> TestResult {
    let f = fixture("r6-2")?;
    let s = &f.services;
    let target = &my_targets()[1];
    let a = open(s, target, Environment::Development, false).await?;
    let mut alias = input(target, Environment::Development, false);
    alias.host = "localhost".into();
    let alias_profile = s.create_connection(&alias)?;
    let b = s
        .open_session(&alias_profile.id, Some(secret("varve_dev")?), None, None)
        .await?;
    run(s, &a, "DROP DATABASE IF EXISTS m2_ipc_r62").await?;
    run(
        s,
        &a,
        "CREATE DATABASE m2_ipc_r62 CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci",
    )
    .await?;
    let first = {
        let services = Arc::clone(s);
        let id = a.id.clone();
        tokio::spawn(async move {
            services
                .ddl_apply(
                    &id,
                    "-- varve-session: collation_database=utf8mb4_bin\n\
                     CREATE TABLE m2_ipc_r62.ta AS SELECT SLEEP(2) AS s",
                    Some("m2_ipc_r62"),
                    true,
                )
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let second = s
        .ddl_apply(
            &b.id,
            "-- varve-session: collation_database=latin1_swedish_ci\n\
             CREATE TABLE m2_ipc_r62.tb (a int)",
            Some("m2_ipc_r62"),
            true,
        )
        .await?;
    let first = first.await??;
    assert!(first.warnings.is_empty(), "{first:?}");
    assert!(second.warnings.is_empty(), "{second:?}");
    let table_collation = |table: &str| {
        format!(
            "SELECT TABLE_COLLATION FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = 'm2_ipc_r62' AND TABLE_NAME = '{table}'"
        )
    };
    assert_eq!(
        text_value(s, &a, &table_collation("ta")).await?.as_deref(),
        Some("utf8mb4_bin")
    );
    assert_eq!(
        text_value(s, &a, &table_collation("tb")).await?.as_deref(),
        Some("latin1_swedish_ci")
    );
    assert_eq!(
        text_value(
            s,
            &a,
            "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
             WHERE SCHEMA_NAME = 'm2_ipc_r62'"
        )
        .await?
        .as_deref(),
        Some("utf8mb4_general_ci")
    );
    assert!(s.ddl_pending_restores().is_empty());
    run(s, &a, "DROP DATABASE m2_ipc_r62").await?;
    s.close_session(&a.id).await?;
    s.close_session(&b.id).await?;
    Ok(())
}

/// Round 6 audit probe P3f: a Cancel while the DDL waits for its tab
/// stops it before it runs.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn a_cancel_before_the_script_starts_stops_the_ddl() -> TestResult {
    let f = fixture("r6-4")?;
    let s = &f.services;
    let tab = open(s, &my_targets()[0], Environment::Development, false).await?;
    run(s, &tab, "DROP DATABASE IF EXISTS m2_ipc_r64").await?;
    run(s, &tab, "CREATE DATABASE m2_ipc_r64").await?;
    let busy = {
        let services = Arc::clone(s);
        let id = tab.id.clone();
        tokio::spawn(async move { services.execute(&id, "SELECT SLEEP(2)", false, None).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let ddl = {
        let services = Arc::clone(s);
        let id = tab.id.clone();
        tokio::spawn(async move {
            services
                .ddl_apply(&id, "CREATE TABLE m2_ipc_r64.f1 (a int)", None, false)
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    s.cancel(&tab.id).await?;
    let _ = busy.await?;
    let cancelled = ddl.await?;
    assert!(
        matches!(
            &cancelled,
            Err(CmdError::Session {
                error: SessionError::Driver { error }
            }) if error.kind == varve_driver::ErrorKind::Cancelled
        ),
        "{cancelled:?}"
    );
    let made = int_of(first_value(
        &run(
            s,
            &tab,
            "SELECT COUNT(*) FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = 'm2_ipc_r64' AND TABLE_NAME = 'f1'",
        )
        .await?,
    ));
    assert_eq!(made, Some(0), "the cancelled DDL ran");
    run(s, &tab, "DROP DATABASE m2_ipc_r64").await?;
    s.close_session(&tab.id).await?;
    Ok(())
}

/// Round 6 audit probe P4: a secret save of one kind overlapping an edit
/// that unlists another kind (with a failing keychain delete) never lists
/// the old server's secret again, and the cleanup still removes it. A
/// secret entered for the old settings is refused.
#[tokio::test(flavor = "multi_thread")]
async fn secret_saves_and_edits_never_relist_the_old_servers_secret() -> TestResult {
    let dir = std::env::temp_dir().join(format!("varve-m2-ipc-r6-3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let keychain = Arc::new(FlakyKeychain::default());
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let s = varve_app::build_services(
        store,
        &dir,
        &host,
        Edition::Community,
        Arc::clone(&keychain) as Arc<dyn varve_secrets::SecretStore>,
    )?;
    let mut original = input(&pg_targets()[1], Environment::Development, false);
    original.ssh = varve_records::SshSettings {
        enabled: true,
        host: "bastion.invalid".into(),
        port: 22,
        username: "ops".into(),
        auth: varve_records::SshAuthMethod::Password,
        key_path: None,
    };
    let profile = s.create_connection(&original)?;
    let first = s.save_secret(
        &profile.id,
        varve_records::SecretKindName::DbPassword,
        secret("old-server-pw")?,
    )?;
    let db_key = varve_secrets::SecretKey::new(
        profile.id.clone(),
        varve_records::SecretKindName::DbPassword.into(),
    )?;

    // save_secret(SshPassword) pauses inside the keychain write while the
    // host is edited and the old DbPassword cannot be deleted.
    let (stored_tx, stored_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    if let Ok(mut pause) = keychain.pause_next_set.lock() {
        *pause = Some((stored_tx, release_rx));
    }
    let saver = {
        let s = Arc::clone(&s);
        let id = profile.id.clone();
        let value = secret("bastion-pw")?;
        std::thread::spawn(move || {
            s.save_secret(&id, varve_records::SecretKindName::SshPassword, value)
                .map_err(|e| e.to_string())
        })
    };
    stored_rx.recv_timeout(std::time::Duration::from_secs(5))?;
    keychain
        .failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let edit = {
        let s = Arc::clone(&s);
        let id = profile.id.clone();
        let mut moved = original.clone();
        moved.host = "localhost".into();
        tokio::spawn(async move {
            s.update_connection(&id, &moved)
                .await
                .map_err(|e| e.to_string())
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    release_tx.send(())?;
    let saved = saver.join().map_err(|_| "save panicked")??;
    assert!(
        saved
            .saved_secrets
            .contains(&varve_records::SecretKindName::SshPassword)
    );
    let edited = edit.await?;
    assert!(
        edited.is_err(),
        "the failing delete is reported: {edited:?}"
    );
    keychain
        .failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let stored = s
        .tree()?
        .connections
        .into_iter()
        .find(|c| c.id == profile.id)
        .ok_or("profile gone")?;
    assert_eq!(stored.host, "localhost");
    assert_eq!(
        stored.saved_secrets,
        [varve_records::SecretKindName::SshPassword],
        "the old server's password is listed for the new host"
    );
    assert!(s.retry_keychain_cleanup().is_empty());
    assert!(
        varve_secrets::SecretStore::get(&keychain.inner, &db_key)?.is_none(),
        "the old server's password is still in the keychain"
    );

    // A password typed for the old settings (version before the edit) is
    // refused; one typed for the new settings is saved.
    let typed_for_old = s.save_secret_for(
        &profile.id,
        varve_records::SecretKindName::DbPassword,
        secret("typed-for-old")?,
        Some(first.version),
    );
    assert!(
        matches!(typed_for_old, Err(CmdError::Invalid { .. })),
        "{typed_for_old:?}"
    );
    s.save_secret_for(
        &profile.id,
        varve_records::SecretKindName::DbPassword,
        secret("typed-for-new")?,
        Some(stored.version),
    )?;
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// EXPLAIN ANALYZE asks the server whether a name the statement leaves to
/// `search_path` resolves to a user object: a statement only that check
/// catches gets a confirmation dialog (naming the function), not an error.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn explain_analyze_confirms_functions_only_the_server_check_finds() -> TestResult {
    let f = fixture("explain-checked")?;
    let s = &f.services;
    for target in only_selected(pg_targets()) {
        let name = target.name;
        let tab = open(s, &target, Environment::Development, false).await?;
        run(s, &tab, "DROP SCHEMA IF EXISTS m2_ipc_shadow CASCADE").await?;
        run(s, &tab, "CREATE SCHEMA m2_ipc_shadow").await?;
        run(
            s,
            &tab,
            "CREATE FUNCTION m2_ipc_shadow.lower(text) RETURNS text LANGUAGE sql \
             AS 'SELECT upper($1)'",
        )
        .await?;
        run(
            s,
            &tab,
            "SET search_path = m2_ipc_shadow, pg_catalog, public",
        )
        .await?;
        let analyze = ExplainOptions {
            analyze: true,
            ..Default::default()
        };
        let statement = "SELECT lower('x')";
        let asked = s.explain(&tab.id, statement, &analyze, false).await;
        let Err(CmdError::Session {
            error: SessionError::ConfirmationRequired { statements, .. },
        }) = &asked
        else {
            return Err(format!("{name}: expected a confirmation, got {asked:?}").into());
        };
        assert!(
            statements.join(" ").contains("lower"),
            "{name}: the dialog does not name the function: {statements:?}"
        );
        let plan = s.explain(&tab.id, statement, &analyze, true).await?;
        assert!(plan.analyzed, "{name}");
        // Without the shadowing schema the same statement needs nothing.
        run(s, &tab, "SET search_path = pg_catalog, public").await?;
        s.explain(&tab.id, statement, &analyze, false).await?;
        run(s, &tab, "DROP SCHEMA m2_ipc_shadow CASCADE").await?;
        s.close_session(&tab.id).await?;
    }
    Ok(())
}

/// Services with a keychain double on their own data directory.
fn flaky_services(name: &str) -> Res<(Arc<Services>, Arc<FlakyKeychain>, std::path::PathBuf)> {
    let dir = std::env::temp_dir().join(format!("varve-m2-ipc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let keychain = Arc::new(FlakyKeychain::default());
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let s = varve_app::build_services(
        store,
        &dir,
        &host,
        Edition::Community,
        Arc::clone(&keychain) as Arc<dyn varve_secrets::SecretStore>,
    )?;
    Ok((s, keychain, dir))
}

/// Round 7 audit probe P5: edits of two profiles at the same time keep
/// both profiles' "invalidated at" versions; a damaged record refuses
/// saves (fail closed).
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_edits_keep_every_profiles_secret_versions() -> TestResult {
    let (s, _keychain, dir) = flaky_services("r7-1")?;
    let target = &pg_targets()[1];
    let a = s.create_connection(&input(target, Environment::Development, false))?;
    let b = s.create_connection(&input(target, Environment::Development, false))?;
    let db = varve_records::SecretKindName::DbPassword;
    for round in 0..40 {
        let mut moved = input(target, Environment::Development, false);
        moved.host = format!("host-{round}.invalid");
        let (ra, rb) = tokio::join!(
            {
                let s = Arc::clone(&s);
                let id = a.id.clone();
                let moved = moved.clone();
                tokio::spawn(async move {
                    s.update_connection(&id, &moved)
                        .await
                        .map_err(|e| e.to_string())
                })
            },
            {
                let s = Arc::clone(&s);
                let id = b.id.clone();
                let moved = moved.clone();
                tokio::spawn(async move {
                    s.update_connection(&id, &moved)
                        .await
                        .map_err(|e| e.to_string())
                })
            }
        );
        let (ra, rb) = (ra??, rb??);
        assert_eq!(
            s.secret_invalidated_at(&a.id, db)?,
            Some(ra.version),
            "round {round}: a lost its version"
        );
        assert_eq!(
            s.secret_invalidated_at(&b.id, db)?,
            Some(rb.version),
            "round {round}: b lost its version"
        );
    }
    // A damaged record (R8-2) stays with its connection and recovers: every
    // kind then needs a secret entered for the current version.
    s.store.with_conn(|c| {
        c.execute(
            "INSERT INTO core_meta (key, value) VALUES (?1, 'damaged') \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [format!("secrets.invalidated_at.{}", a.id)],
        )?;
        Ok(())
    })?;
    let current_a = s
        .tree()?
        .connections
        .into_iter()
        .find(|c| c.id == a.id)
        .ok_or("a gone")?
        .version;
    assert_eq!(s.secret_invalidated_at(&a.id, db)?, Some(current_a));
    let older = s.save_secret_for(&a.id, db, secret("pw")?, Some(current_a - 1));
    assert!(matches!(older, Err(CmdError::Invalid { .. })), "{older:?}");
    s.save_secret_for(&a.id, db, secret("pw")?, Some(current_a))?;
    // The other connection is not affected, and edits still work.
    s.secret_invalidated_at(&b.id, db)?;
    let mut renamed = input(target, Environment::Development, false);
    renamed.name = "renamed".into();
    renamed.host = "host-39.invalid".into();
    s.update_connection(&b.id, &renamed).await?;
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Round 7 audit probe P6: a secret save in flight when its folder is
/// deleted leaves nothing in the keychain.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_folder_during_a_secret_save_leaves_no_keychain_item() -> TestResult {
    let (s, keychain, dir) = flaky_services("r7-2")?;
    let folder = s.create_folder(None, "m2_ipc folder", None)?;
    let mut inside = input(&pg_targets()[1], Environment::Development, false);
    inside.folder_id = Some(folder.id.clone());
    let profile = s.create_connection(&inside)?;
    let key = varve_secrets::SecretKey::new(
        profile.id.clone(),
        varve_records::SecretKindName::DbPassword.into(),
    )?;
    let (stored_tx, stored_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    if let Ok(mut pause) = keychain.pause_next_set.lock() {
        *pause = Some((stored_tx, release_rx));
    }
    let saver = {
        let s = Arc::clone(&s);
        let id = profile.id.clone();
        let value = secret("pw")?;
        std::thread::spawn(move || {
            s.save_secret_for(&id, varve_records::SecretKindName::DbPassword, value, None)
                .map_err(|e| e.to_string())
        })
    };
    stored_rx.recv_timeout(std::time::Duration::from_secs(5))?;
    let deleter = {
        let s = Arc::clone(&s);
        let id = folder.id.clone();
        tokio::spawn(async move {
            s.delete_folder(&id, varve_records::FolderDeleteMode::DeleteContents)
                .await
                .map_err(|e| e.to_string())
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    release_tx.send(())?;
    let saved = saver.join().map_err(|_| "save panicked")?;
    deleter.await??;
    // Either the save finished first (and the delete removed its secret)
    // or it found the profile gone: nothing is left either way.
    let _ = saved;
    assert!(
        varve_secrets::SecretStore::get(&keychain.inner, &key)?.is_none(),
        "a keychain item of a deleted connection was left behind"
    );
    assert!(s.pending_keychain_deletions().is_empty());
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Round 7 audit probe P8: an edit that cannot be stored (its folder is
/// gone) leaves the profile, its listed secrets and their versions as they
/// were.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_edit_keeps_secrets_listed_and_usable() -> TestResult {
    let (s, keychain, dir) = flaky_services("r7-3")?;
    let original = input(&pg_targets()[1], Environment::Development, false);
    let profile = s.create_connection(&original)?;
    let saved = s.save_secret(
        &profile.id,
        varve_records::SecretKindName::DbPassword,
        secret("pw")?,
    )?;
    let mut broken = original.clone();
    broken.host = "elsewhere.invalid".into();
    broken.folder_id = Some("no-such-folder".into());
    assert!(s.update_connection(&profile.id, &broken).await.is_err());
    let now = s
        .tree()?
        .connections
        .into_iter()
        .find(|c| c.id == profile.id)
        .ok_or("profile gone")?;
    assert_eq!(now.host, saved.host);
    assert_eq!(now.saved_secrets, saved.saved_secrets, "secrets unlisted");
    let key = varve_secrets::SecretKey::new(
        profile.id.clone(),
        varve_records::SecretKindName::DbPassword.into(),
    )?;
    assert!(varve_secrets::SecretStore::get(&keychain.inner, &key)?.is_some());
    assert_eq!(
        s.secret_invalidated_at(&profile.id, varve_records::SecretKindName::DbPassword)?,
        None,
        "a failed edit left the kind marked"
    );
    s.save_secret_for(
        &profile.id,
        varve_records::SecretKindName::DbPassword,
        secret("pw2")?,
        Some(now.version),
    )?;
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Reads one `core_meta` value.
fn meta(s: &Services, key: &str) -> Res<Option<String>> {
    Ok(s.store.with_conn(|c| {
        let mut stmt = c.prepare("SELECT value FROM core_meta WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    })?)
}

/// Writes one `core_meta` value.
fn set_meta(s: &Services, key: &str, value: &str) -> TestResult {
    s.store.with_conn(|c| {
        c.execute(
            "INSERT INTO core_meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    })?;
    Ok(())
}

/// Round 8 audit probes Q1, Q1b, Q1c: an "edit in progress" mark never
/// blocks saving for good. A crash after the stored edit, a failed version
/// write, a failed edit and a round 7 mark all resolve against the stored
/// profile on the next read.
#[tokio::test(flavor = "multi_thread")]
async fn edit_marks_resolve_against_the_stored_profile() -> TestResult {
    let (s, _keychain, dir) = flaky_services("r8-1")?;
    let db = varve_records::SecretKindName::DbPassword;
    let original = input(&pg_targets()[1], Environment::Development, false);
    let version_of = |s: &Services, id: &str| -> Res<i64> {
        Ok(s.tree()?
            .connections
            .into_iter()
            .find(|c| c.id == id)
            .ok_or("gone")?
            .version)
    };

    // Q1: the mark was written and the edit stored, then the app stopped.
    let crashed = s.create_connection(&original)?;
    set_meta(
        &s,
        &format!("secrets.invalidated_at.{}", crashed.id),
        &format!(
            r#"{{"DbPassword":{{"pendingFrom":{},"previous":null}}}}"#,
            crashed.version
        ),
    )?;
    let mut moved = original.clone();
    moved.host = "moved.invalid".into();
    s.records.update_connection(&crashed.id, &moved)?;
    let now = version_of(&s, &crashed.id)?;
    assert_eq!(s.secret_invalidated_at(&crashed.id, db)?, Some(now));
    s.save_secret_for(&crashed.id, db, secret("pw")?, Some(now))?;
    let old = s.save_secret_for(&crashed.id, db, secret("pw")?, Some(crashed.version));
    assert!(matches!(old, Err(CmdError::Invalid { .. })), "{old:?}");

    // The mark was written but the edit never stored: nothing changed.
    let unchanged = s.create_connection(&original)?;
    set_meta(
        &s,
        &format!("secrets.invalidated_at.{}", unchanged.id),
        &format!(
            r#"{{"DbPassword":{{"pendingFrom":{},"previous":null}}}}"#,
            unchanged.version
        ),
    )?;
    assert_eq!(s.secret_invalidated_at(&unchanged.id, db)?, None);
    s.save_secret_for(&unchanged.id, db, secret("pw")?, Some(unchanged.version))?;

    // A round 7 mark (i64::MAX) left behind.
    let round7 = s.create_connection(&original)?;
    set_meta(
        &s,
        &format!("secrets.invalidated_at.{}", round7.id),
        &format!(r#"{{"DbPassword":{}}}"#, i64::MAX),
    )?;
    s.save_secret_for(&round7.id, db, secret("pw")?, Some(round7.version))?;

    // Q1b: the version write after the stored edit fails. The edit is
    // reported as saved with a warning, and saving still works.
    let failing = s.create_connection(&original)?;
    s.store.with_conn(|c| {
        c.execute_batch(
            "CREATE TRIGGER m2_ipc_versions_fail BEFORE UPDATE ON core_meta \
             WHEN NEW.key LIKE 'secrets.invalidated_at.%' AND NEW.value NOT LIKE '%pendingFrom%' \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )?;
        Ok(())
    })?;
    let edited = s.update_connection(&failing.id, &moved).await;
    let Err(CmdError::Keychain {
        message,
        profile: Some(stored),
    }) = &edited
    else {
        return Err(format!("expected a saved-with-warning reply, got {edited:?}").into());
    };
    assert!(message.contains("The connection was saved"), "{message}");
    assert_eq!(stored.host, "moved.invalid");
    s.store.with_conn(|c| {
        c.execute_batch("DROP TRIGGER m2_ipc_versions_fail;")?;
        Ok(())
    })?;
    s.save_secret_for(&failing.id, db, secret("pw")?, Some(stored.version))?;

    // Q1c: an edit that cannot be stored, whose version restore fails too.
    let restored = s.create_connection(&original)?;
    s.store.with_conn(|c| {
        c.execute_batch(
            "CREATE TRIGGER m2_ipc_restore_fail BEFORE UPDATE ON core_meta \
             WHEN NEW.key LIKE 'secrets.invalidated_at.%' AND NEW.value NOT LIKE '%pendingFrom%' \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )?;
        Ok(())
    })?;
    let mut broken = moved.clone();
    broken.folder_id = Some("no-such-folder".into());
    assert!(s.update_connection(&restored.id, &broken).await.is_err());
    s.store.with_conn(|c| {
        c.execute_batch("DROP TRIGGER m2_ipc_restore_fail;")?;
        Ok(())
    })?;
    assert_eq!(s.secret_invalidated_at(&restored.id, db)?, None);
    s.save_secret_for(&restored.id, db, secret("pw")?, Some(restored.version))?;
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Round 8 audit probe Q2: the one shared map of round 6 builds is moved
/// into the per-connection keys at start and removed; a damaged one is
/// removed without locking anyone out.
#[tokio::test(flavor = "multi_thread")]
async fn old_shared_secret_versions_are_migrated_and_removed() -> TestResult {
    let (s, _keychain, dir) = flaky_services("r8-2")?;
    let db = varve_records::SecretKindName::DbPassword;
    let a = s.create_connection(&input(&pg_targets()[1], Environment::Development, false))?;
    set_meta(
        &s,
        "secrets.invalidated_at",
        &format!(
            r#"{{"{}":{{"DbPassword":7}},"deleted-profile":{{"DbPassword":3}}}}"#,
            a.id
        ),
    )?;
    drop(s);
    let s = services_on(&dir)?;
    assert_eq!(meta(&s, "secrets.invalidated_at")?, None, "shared map kept");
    assert_eq!(s.secret_invalidated_at(&a.id, db)?, Some(7));
    assert_eq!(meta(&s, "secrets.invalidated_at.deleted-profile")?, None);

    set_meta(&s, "secrets.invalidated_at", "damaged")?;
    drop(s);
    let s = services_on(&dir)?;
    assert_eq!(meta(&s, "secrets.invalidated_at")?, None);
    let mut moved = input(&pg_targets()[1], Environment::Development, false);
    moved.host = "moved.invalid".into();
    let edited = s.update_connection(&a.id, &moved).await?;
    s.save_secret_for(&a.id, db, secret("pw")?, Some(edited.version))?;
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// R8-5: connections moved into a folder while it is deleted are either
/// moved before (and deleted with their secrets) or after (and kept): no
/// deleted connection leaves a keychain item behind.
#[tokio::test(flavor = "multi_thread")]
async fn folder_deletes_and_moves_never_orphan_secrets() -> TestResult {
    let (s, keychain, dir) = flaky_services("r8-5")?;
    let db = varve_records::SecretKindName::DbPassword;
    for round in 0..15 {
        let folder = s.create_folder(None, &format!("m2_ipc {round}"), None)?;
        let mut profiles = Vec::new();
        for _ in 0..4 {
            let p =
                s.create_connection(&input(&pg_targets()[1], Environment::Development, false))?;
            s.save_secret(&p.id, db, secret("pw")?)?;
            profiles.push(p);
        }
        let movers: Vec<_> = profiles
            .iter()
            .map(|p| {
                let s = Arc::clone(&s);
                let id = p.id.clone();
                let folder = folder.id.clone();
                std::thread::spawn(move || {
                    for _ in 0..5 {
                        let _ = s.move_connection(&id, Some(&folder));
                        let _ = s.move_connection(&id, None);
                        let _ = s.move_connection(&id, Some(&folder));
                    }
                })
            })
            .collect();
        s.delete_folder(&folder.id, varve_records::FolderDeleteMode::DeleteContents)
            .await?;
        for mover in movers {
            mover.join().map_err(|_| "mover panicked")?;
        }
        let alive: Vec<String> = s.tree()?.connections.into_iter().map(|c| c.id).collect();
        for p in &profiles {
            let key = varve_secrets::SecretKey::new(p.id.clone(), db.into())?;
            let in_keychain = varve_secrets::SecretStore::get(&keychain.inner, &key)?.is_some();
            assert_eq!(
                in_keychain,
                alive.contains(&p.id),
                "round {round}: a deleted connection kept its secret (or a kept one lost it)"
            );
        }
        for id in alive {
            s.delete_connection(&id).await?;
        }
    }
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// EXPLAIN ANALYZE after the catalog redesign (one catalog call per run):
/// a provable read runs read-only without confirmation, also on a
/// read-only session and also when the UI confirmed up front (never as a
/// writable run); a data change needs confirmation naming it; inside the
/// session's PostgreSQL transaction every run confirms and says why.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn explain_analyze_confirms_only_what_the_catalog_cannot_prove() -> TestResult {
    let f = fixture("explain-redesign")?;
    let s = &f.services;
    let analyze = ExplainOptions {
        analyze: true,
        ..Default::default()
    };
    for target in only_selected(pg_targets()) {
        let name = target.name;
        let tab = open(s, &target, Environment::Development, false).await?;
        run(s, &tab, "DROP TABLE IF EXISTS m2_ipc_explain").await?;
        run(s, &tab, "CREATE TABLE m2_ipc_explain (a int)").await?;

        // (1) A provable read with an up-front confirmation: runs, and is
        // not treated as a confirmed (writable) run.
        let confirmations = |s: &Services| -> Res<usize> {
            Ok(actions(s, &tab.connection_id)?
                .iter()
                .filter(|a| a.starts_with("destructive."))
                .count())
        };
        let before = confirmations(s)?;
        let plan = s
            .explain(&tab.id, "SELECT a FROM m2_ipc_explain", &analyze, true)
            .await?;
        assert!(plan.analyzed, "{name}");
        assert_eq!(
            confirmations(s)?,
            before,
            "{name}: a provable read went through the confirmation path"
        );

        // (2) A read-only session cannot resolve the statement's names on
        // the server (the check's temporary view cannot be created in a
        // read-only transaction), so a read asks for confirmation with that
        // reason instead of being refused; the confirmed run still runs
        // read-only (it would be refused as read-only if the confirmation
        // turned it into a writable run), and changes are refused.
        let ro = open(s, &target, Environment::Development, true).await?;
        let asked = s
            .explain(&ro.id, "SELECT a FROM m2_ipc_explain", &analyze, false)
            .await;
        let Err(CmdError::Session {
            error: SessionError::ConfirmationRequired { statements, .. },
        }) = &asked
        else {
            return Err(format!("{name}: expected a confirmation, got {asked:?}").into());
        };
        assert!(
            statements.join(" ").contains("read-only"),
            "{name}: the dialog does not give the reason: {statements:?}"
        );
        let plan = s
            .explain(&ro.id, "SELECT a FROM m2_ipc_explain", &analyze, true)
            .await;
        assert!(
            plan.as_ref().is_ok_and(|p| p.analyzed),
            "{name}: an up-front confirmation made a provable read a writable run: {plan:?}"
        );
        assert!(is_read_only(
            &s.explain(
                &ro.id,
                "INSERT INTO m2_ipc_explain VALUES (1)",
                &analyze,
                true
            )
            .await
        ));
        // (2b) Inside the read-only session's own transaction even a plain
        // read is refused, with the real reason and what to do.
        run(s, &ro, "BEGIN").await?;
        let refused = s
            .explain(&ro.id, "SELECT a FROM m2_ipc_explain", &analyze, false)
            .await;
        let Err(CmdError::ReadOnly { message }) = &refused else {
            return Err(format!("{name}: expected a read-only refusal, got {refused:?}").into());
        };
        assert!(
            message.contains("inside a transaction") && message.contains("Commit or roll back"),
            "{name}: the refusal does not give the real reason: {message}"
        );
        assert!(
            !message.contains("side effects"),
            "{name}: misleading reason: {message}"
        );
        run(s, &ro, "ROLLBACK").await?;
        s.close_session(&ro.id).await?;

        // (3) A data change needs confirmation that says so.
        let asked = s
            .explain(
                &tab.id,
                "INSERT INTO m2_ipc_explain VALUES (1)",
                &analyze,
                false,
            )
            .await;
        let Err(CmdError::Session {
            error: SessionError::ConfirmationRequired { statements, .. },
        }) = &asked
        else {
            return Err(format!("{name}: expected a confirmation, got {asked:?}").into());
        };
        assert!(
            statements.join(" ").contains("changes data"),
            "{name}: {statements:?}"
        );
        let confirmed_runs = |s: &Services| -> Res<usize> {
            Ok(actions(s, &tab.connection_id)?
                .iter()
                .filter(|a| *a == "destructive.confirmed")
                .count())
        };
        let before = confirmed_runs(s)?;
        s.explain(
            &tab.id,
            "INSERT INTO m2_ipc_explain VALUES (1)",
            &analyze,
            true,
        )
        .await?;
        assert_eq!(
            confirmed_runs(s)?,
            before + 1,
            "{name}: a confirmed EXPLAIN ANALYZE was not logged as destructive.confirmed"
        );
        let rows = int_of(first_value(
            &run(s, &tab, "SELECT count(*) FROM m2_ipc_explain").await?,
        ));
        assert_eq!(
            rows,
            Some(0),
            "{name}: the confirmed run was not rolled back"
        );

        // (5) Inside the session's transaction every run confirms, and the
        // dialog says why.
        run(s, &tab, "BEGIN").await?;
        let asked = s
            .explain(&tab.id, "SELECT a FROM m2_ipc_explain", &analyze, false)
            .await;
        let Err(CmdError::Session {
            error: SessionError::ConfirmationRequired { statements, .. },
        }) = &asked
        else {
            return Err(format!("{name}: expected a confirmation, got {asked:?}").into());
        };
        assert!(
            statements.join(" ").contains("inside a transaction"),
            "{name}: the dialog does not say why: {statements:?}"
        );
        let plan = s
            .explain(&tab.id, "SELECT a FROM m2_ipc_explain", &analyze, true)
            .await?;
        assert!(plan.analyzed, "{name}");
        run(s, &tab, "ROLLBACK").await?;

        run(s, &tab, "DROP TABLE m2_ipc_explain").await?;
        s.close_session(&tab.id).await?;
    }
    Ok(())
}

/// Arms one pause of the keychain double: returns the channel that reports
/// the call started and the sender that releases it.
fn arm(
    slot: &std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
) -> Res<(std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>)> {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *slot.lock().map_err(|_| "keychain double lock poisoned")? = Some((started_tx, release_rx));
    Ok((started_rx, release_tx))
}

/// Tree operations on connections other than the one whose keychain call
/// hangs: create, move, rename, duplicate, delete, folder create, move and
/// delete. Every one runs on the blocking pool (as the IPC commands do), so
/// a call that waits for the hanging keychain shows up as a timeout instead
/// of stalling the test.
async fn other_tree_operations(s: &Arc<Services>, round: &str) -> Res<std::time::Duration> {
    let started = std::time::Instant::now();
    let target = &pg_targets()[1];
    let folder = {
        let s = Arc::clone(s);
        let name = format!("m2 ipc r9-1 {round}");
        tokio::task::spawn_blocking(move || {
            s.create_folder(None, &name, None)
                .map_err(|e| e.to_string())
        })
        .await??
    };
    let sub = {
        let s = Arc::clone(s);
        let name = format!("m2 ipc r9-1 {round} sub");
        tokio::task::spawn_blocking(move || {
            s.create_folder(None, &name, None)
                .map_err(|e| e.to_string())
        })
        .await??
    };
    let mut created = input(target, Environment::Development, false);
    created.name = format!("m2 ipc r9-1 {round}");
    let c = {
        let s = Arc::clone(s);
        let created = created.clone();
        tokio::task::spawn_blocking(move || {
            s.create_connection(&created).map_err(|e| e.to_string())
        })
        .await??
    };
    {
        let s = Arc::clone(s);
        let (id, folder_id) = (c.id.clone(), folder.id.clone());
        tokio::task::spawn_blocking(move || {
            s.move_connection(&id, Some(&folder_id))
                .map_err(|e| e.to_string())
        })
        .await??;
    }
    {
        let s = Arc::clone(s);
        let (id, parent) = (sub.id.clone(), folder.id.clone());
        tokio::task::spawn_blocking(move || {
            s.move_folder(&id, Some(&parent)).map_err(|e| e.to_string())
        })
        .await??;
    }
    let mut renamed = created;
    renamed.name = format!("m2 ipc r9-1 {round} renamed");
    renamed.folder_id = Some(folder.id.clone());
    s.update_connection(&c.id, &renamed).await?;
    let copy = {
        let s = Arc::clone(s);
        let id = c.id.clone();
        tokio::task::spawn_blocking(move || s.duplicate_connection(&id).map_err(|e| e.to_string()))
            .await??
    };
    s.delete_connection(&copy.id).await?;
    s.delete_folder(&folder.id, varve_records::FolderDeleteMode::DeleteContents)
        .await?;
    Ok(started.elapsed())
}

/// Runs [`other_tree_operations`] with a time limit while a keychain call
/// hangs, then releases the call. The limit is far below the time the call
/// hangs, so waiting for it fails the test.
async fn promptly_while_hanging(
    s: &Arc<Services>,
    round: &str,
    release: &std::sync::mpsc::Sender<()>,
) -> TestResult {
    let limit = std::time::Duration::from_secs(3);
    let outcome = tokio::time::timeout(limit, other_tree_operations(s, round)).await;
    // Released before any assertion, so a failure does not leave a thread
    // waiting for ever.
    release.send(())?;
    let elapsed = outcome.map_err(|_| {
        format!("{round}: tree operations of other connections waited for the keychain")
    })??;
    assert!(elapsed < limit, "{round}: {elapsed:?}");
    Ok(())
}

/// R9-1 and R9-2: a keychain call that hangs (a macOS keychain prompt can
/// wait for minutes) holds up only the connection whose secret it touches.
/// Creating, moving, renaming, duplicating and deleting other connections
/// and folders completes promptly while (1) a secret save hangs in its
/// keychain write and an edit of the same connection waits for it, (2) an
/// edit hangs removing the old server's password, and (3) a folder delete
/// hangs removing a deleted connection's password. While (2) and (3) hang,
/// which is what a crash at that moment leaves behind, the unlisted
/// entries are already recorded for removal, and they are taken off once
/// removed.
#[tokio::test(flavor = "multi_thread")]
async fn a_hanging_keychain_call_holds_up_only_its_own_connection() -> TestResult {
    let (s, keychain, dir) = flaky_services("r9-1")?;
    let target = &pg_targets()[1];
    let db = varve_records::SecretKindName::DbPassword;
    let pending = |s: &Services, id: &str| {
        s.pending_keychain_deletions()
            .iter()
            .any(|e| e.connection_id == id && e.kind == db)
    };

    // (1) A secret save hangs in the keychain write, an edit of the same
    // connection waits for it.
    let a = s.create_connection(&input(target, Environment::Development, false))?;
    let (started, release) = arm(&keychain.pause_next_set)?;
    let saver = {
        let s = Arc::clone(&s);
        let (id, value) = (a.id.clone(), secret("a-pw")?);
        std::thread::spawn(move || {
            s.save_secret_for(&id, db, value, None)
                .map_err(|e| e.to_string())
        })
    };
    started.recv_timeout(std::time::Duration::from_secs(5))?;
    let editor = {
        let s = Arc::clone(&s);
        let id = a.id.clone();
        let mut moved = input(target, Environment::Development, false);
        moved.host = "a-moved.invalid".into();
        tokio::spawn(async move { s.update_connection(&id, &moved).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    promptly_while_hanging(&s, "save", &release).await?;
    saver.join().map_err(|_| "save panicked")??;
    let edited = editor.await??;
    assert!(
        !edited.saved_secrets.contains(&db),
        "the old server's password is still listed: {:?}",
        edited.saved_secrets
    );

    // (2) An edit hangs removing the old server's password.
    let b = s.create_connection(&input(target, Environment::Development, false))?;
    s.save_secret(&b.id, db, secret("b-pw")?)?;
    let (started, release) = arm(&keychain.pause_next_delete)?;
    let editor = {
        let s = Arc::clone(&s);
        let id = b.id.clone();
        let mut moved = input(target, Environment::Development, false);
        moved.host = "b-moved.invalid".into();
        tokio::spawn(async move { s.update_connection(&id, &moved).await })
    };
    started.recv_timeout(std::time::Duration::from_secs(5))?;
    assert!(
        !s.records.connection(&b.id)?.saved_secrets.contains(&db),
        "the edit had not unlisted the password yet"
    );
    assert!(
        pending(&s, &b.id),
        "an unlisted password is not recorded for removal while its removal runs"
    );
    promptly_while_hanging(&s, "edit", &release).await?;
    editor.await??;
    assert!(!pending(&s, &b.id), "a removed password is still recorded");
    let key = varve_secrets::SecretKey::new(b.id.clone(), db.into())?;
    assert!(varve_secrets::SecretStore::get(&keychain.inner, &key)?.is_none());

    // (3) A folder delete hangs removing a deleted connection's password.
    let doomed = s.create_folder(None, "m2 ipc r9-1 doomed", None)?;
    let e = s.create_connection(&input(target, Environment::Development, false))?;
    s.move_connection(&e.id, Some(&doomed.id))?;
    s.save_secret(&e.id, db, secret("e-pw")?)?;
    let (started, release) = arm(&keychain.pause_next_delete)?;
    let deleter = {
        let s = Arc::clone(&s);
        let id = doomed.id.clone();
        tokio::spawn(async move {
            s.delete_folder(&id, varve_records::FolderDeleteMode::DeleteContents)
                .await
        })
    };
    started.recv_timeout(std::time::Duration::from_secs(5))?;
    assert!(
        s.records.connection(&e.id).is_err(),
        "the connection was not deleted yet"
    );
    assert!(
        pending(&s, &e.id),
        "a deleted connection's password is not recorded for removal while its removal runs"
    );
    promptly_while_hanging(&s, "folder delete", &release).await?;
    deleter.await??;
    assert!(!pending(&s, &e.id), "a removed password is still recorded");
    let key = varve_secrets::SecretKey::new(e.id.clone(), db.into())?;
    assert!(varve_secrets::SecretStore::get(&keychain.inner, &key)?.is_none());

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// R9-2: an edit that unlists a saved secret records it for removal in
/// the same `app.db` write as its version marks, so a crash between
/// unlisting and removing it (simulated: the keychain removal fails and
/// the app restarts) leaves it recorded, and deleting the connection
/// afterwards does not lose it either.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlisted_secret_stays_recorded_across_a_crash() -> TestResult {
    let (s, keychain, dir) = flaky_services("r9-2")?;
    let target = &pg_targets()[1];
    let db = varve_records::SecretKindName::DbPassword;
    let p = s.create_connection(&input(target, Environment::Development, false))?;
    s.save_secret(&p.id, db, secret("old-pw")?)?;
    let (started, release) = arm(&keychain.pause_next_delete)?;
    let editor = {
        let s = Arc::clone(&s);
        let id = p.id.clone();
        let mut moved = input(target, Environment::Development, false);
        moved.host = "crash-moved.invalid".into();
        tokio::spawn(async move { s.update_connection(&id, &moved).await })
    };
    started.recv_timeout(std::time::Duration::from_secs(5))?;
    // What a crash now leaves in app.db, read by a second process.
    let after_crash = services_on(&dir)?;
    let recorded = after_crash
        .pending_keychain_deletions()
        .iter()
        .any(|e| e.connection_id == p.id && e.kind == db);
    let marks = meta(&after_crash, &format!("secrets.invalidated_at.{}", p.id))?;
    keychain
        .failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    release.send(())?;
    let edited = editor.await?;
    assert!(
        recorded,
        "the unlisted password was not recorded before the removal"
    );
    assert!(
        marks.is_some(),
        "the version marks were not written with it"
    );
    let Err(CmdError::Keychain { .. }) = edited else {
        return Err(format!("expected a keychain note, got {edited:?}").into());
    };
    // Deleting the connection keeps the entry recorded.
    let deleted = s.delete_connection(&p.id).await;
    assert!(deleted.is_ok(), "{deleted:?}");
    assert!(
        s.pending_keychain_deletions()
            .iter()
            .any(|e| e.connection_id == p.id && e.kind == db),
        "deleting the connection lost the recorded removal"
    );
    keychain
        .failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(s.retry_keychain_cleanup().is_empty());
    let key = varve_secrets::SecretKey::new(p.id.clone(), db.into())?;
    assert!(varve_secrets::SecretStore::get(&keychain.inner, &key)?.is_none());
    assert!(s.pending_keychain_deletions().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Waits up to five seconds for the background keychain cleanup to empty
/// the list of pending removals.
async fn cleanup_drained(s: &Services) -> bool {
    for _ in 0..50 {
        if s.pending_keychain_deletions().is_empty() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}

/// R10-1: an earlier failed keychain removal of connection D is retried
/// in the background; while that retry hangs (a keychain prompt), renaming
/// connection C and every other tree operation completes promptly. Only
/// D's own entries are retried inline, by D's own edits.
#[tokio::test(flavor = "multi_thread")]
async fn a_hanging_cleanup_of_one_connection_does_not_hold_up_another() -> TestResult {
    let (s, keychain, dir) = flaky_services("r10-1")?;
    let target = &pg_targets()[1];
    let db = varve_records::SecretKindName::DbPassword;
    let c = s.create_connection(&input(target, Environment::Development, false))?;
    let d = s.create_connection(&input(target, Environment::Development, false))?;
    s.save_secret(&d.id, db, secret("d-pw")?)?;
    // D's edit cannot remove the old password: it stays recorded.
    keychain
        .failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut moved = input(target, Environment::Development, false);
    moved.host = "d-moved.invalid".into();
    let edited = s.update_connection(&d.id, &moved).await;
    assert!(
        matches!(edited, Err(CmdError::Keychain { .. })),
        "{edited:?}"
    );
    assert!(
        s.pending_keychain_deletions()
            .iter()
            .any(|e| e.connection_id == d.id)
    );

    // The next removal hangs (armed before the keychain works again, so
    // the background retry D's edit started cannot remove the entry
    // first); renaming C starts the background retry, which hangs on D's
    // entry.
    let (started, release) = arm(&keychain.pause_next_delete)?;
    keychain
        .failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let limit = std::time::Duration::from_secs(3);
    let mut renamed = input(target, Environment::Development, false);
    renamed.name = "m2 ipc r10-1 renamed".into();
    let rename = tokio::time::timeout(limit, s.update_connection(&c.id, &renamed)).await;
    let hanging = started.recv_timeout(std::time::Duration::from_secs(5));
    if rename.is_err() || hanging.is_err() {
        release.send(())?;
    }
    rename.map_err(|_| "renaming C waited for D's keychain removal")??;
    hanging.map_err(|_| "the background retry did not try D's entry")?;
    renamed.name = "m2 ipc r10-1 renamed again".into();
    let again = tokio::time::timeout(limit, s.update_connection(&c.id, &renamed)).await;
    if again.is_err() {
        release.send(())?;
    }
    again.map_err(|_| "renaming C again waited for D's keychain removal")??;
    promptly_while_hanging(&s, "background cleanup", &release).await?;
    assert!(
        cleanup_drained(&s).await,
        "the background retry did not finish: {:?}",
        s.pending_keychain_deletions()
    );
    let key = varve_secrets::SecretKey::new(d.id.clone(), db.into())?;
    assert!(varve_secrets::SecretStore::get(&keychain.inner, &key)?.is_none());
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// R10-2: deleting a folder finishes while connections are created in it
/// as fast as possible (new connections are locked without waiting under
/// the tree lock instead of starting over).
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_delete_finishes_under_fast_creates() -> TestResult {
    let (s, _keychain, dir) = flaky_services("r10-2")?;
    let target = &pg_targets()[1];
    let folder = s.create_folder(None, "m2 ipc r10-2", None)?;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let creator = {
        let s = Arc::clone(&s);
        let stop = Arc::clone(&stop);
        let mut created = input(target, Environment::Development, false);
        created.folder_id = Some(folder.id.clone());
        std::thread::spawn(move || {
            let mut count = 0_u32;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                if s.create_connection(&created).is_err() {
                    break;
                }
                count += 1;
            }
            count
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let deleted = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        s.delete_folder(&folder.id, varve_records::FolderDeleteMode::DeleteContents),
    )
    .await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let created = creator.join().map_err(|_| "the creator panicked")?;
    deleted.map_err(|_| "the folder delete did not finish under fast creates")??;
    assert!(created > 0, "no connection was created during the delete");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// R10-3: a secret save records its slot as a pending removal before the
/// keychain write, so a crash between the write and the listing leaves the
/// item recorded (removed by the cleanup unless listed); a save that
/// completes takes it off again.
#[tokio::test(flavor = "multi_thread")]
async fn a_secret_written_but_not_listed_stays_recorded_across_a_crash() -> TestResult {
    let (s, keychain, dir) = flaky_services("r10-3")?;
    let target = &pg_targets()[1];
    let db = varve_records::SecretKindName::DbPassword;
    let p = s.create_connection(&input(target, Environment::Development, false))?;
    let (stored, release) = arm(&keychain.pause_next_set)?;
    let saver = {
        let s = Arc::clone(&s);
        let (id, value) = (p.id.clone(), secret("pw")?);
        std::thread::spawn(move || {
            s.save_secret_for(&id, db, value, None)
                .map_err(|e| e.to_string())
        })
    };
    stored.recv_timeout(std::time::Duration::from_secs(5))?;
    // What a crash now leaves in app.db, read by a second process.
    let after_crash = services_on(&dir)?;
    let recorded = after_crash
        .pending_keychain_deletions()
        .iter()
        .any(|e| e.connection_id == p.id && e.kind == db);
    let listed = after_crash
        .records
        .connection(&p.id)?
        .saved_secrets
        .contains(&db);
    release.send(())?;
    let saved = saver.join().map_err(|_| "save panicked")??;
    assert!(
        recorded,
        "a written, unlisted keychain item was not recorded"
    );
    assert!(!listed, "the item was listed before the write returned");
    assert!(saved.saved_secrets.contains(&db));
    assert!(
        s.pending_keychain_deletions().is_empty(),
        "a listed secret is still recorded for removal"
    );
    let key = varve_secrets::SecretKey::new(p.id.clone(), db.into())?;
    assert!(varve_secrets::SecretStore::get(&keychain.inner, &key)?.is_some());
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// R11-1: while the background cleanup hangs on connection E (a keychain
/// prompt), deleting E's folder waits for E without holding E's siblings,
/// so renaming sibling C2 completes promptly; the folder delete finishes
/// once E is free.
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_delete_waiting_for_one_connection_holds_none_of_its_siblings() -> TestResult {
    let (s, keychain, dir) = flaky_services("r11-1")?;
    let target = &pg_targets()[1];
    let db = varve_records::SecretKindName::DbPassword;
    let folder = s.create_folder(None, "m2 ipc r11-1", None)?;
    let mut in_folder = input(target, Environment::Development, false);
    in_folder.folder_id = Some(folder.id.clone());
    let first = s.create_connection(&in_folder)?;
    let second = s.create_connection(&in_folder)?;
    // The sibling sorts first, so a delete taking locks in id order would
    // hold it while waiting for E.
    let (c2, e) = if first.id < second.id {
        (first, second)
    } else {
        (second, first)
    };
    let other = s.create_connection(&input(target, Environment::Development, false))?;
    s.save_secret(&e.id, db, secret("e-pw")?)?;
    // E's edit cannot remove the old password: it stays recorded.
    keychain
        .failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut moved = in_folder.clone();
    moved.host = "e-moved.invalid".into();
    let edited = s.update_connection(&e.id, &moved).await;
    assert!(
        matches!(edited, Err(CmdError::Keychain { .. })),
        "{edited:?}"
    );
    let (started, release) = arm(&keychain.pause_next_delete)?;
    keychain
        .failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    // Editing another connection starts the background retry, which hangs
    // on E's entry while holding E's record lock.
    let mut renamed_other = input(target, Environment::Development, false);
    renamed_other.name = "m2 ipc r11-1 other".into();
    let limit = std::time::Duration::from_secs(3);
    let edited_other =
        tokio::time::timeout(limit, s.update_connection(&other.id, &renamed_other)).await;
    let hanging = started.recv_timeout(std::time::Duration::from_secs(5));
    if edited_other.is_err() || hanging.is_err() {
        release.send(())?;
    }
    edited_other.map_err(|_| "editing another connection waited for E")??;
    hanging.map_err(|_| "the background retry did not try E's entry")?;

    let deleter = {
        let s = Arc::clone(&s);
        let id = folder.id.clone();
        tokio::spawn(async move {
            s.delete_folder(&id, varve_records::FolderDeleteMode::DeleteContents)
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let mut renamed = in_folder.clone();
    renamed.name = "m2 ipc r11-1 sibling renamed".into();
    let rename = tokio::time::timeout(limit, s.update_connection(&c2.id, &renamed)).await;
    release.send(())?;
    rename.map_err(|_| "renaming a sibling waited for the folder delete")??;
    deleter.await??;
    assert!(
        s.records.connection(&c2.id).is_err(),
        "the sibling was not deleted"
    );
    assert!(s.records.connection(&e.id).is_err(), "E was not deleted");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// R11-2: saving over a listed secret when the listing write fails keeps
/// the new keychain item (the profile lists the kind), reports the error
/// and records nothing; saving a new kind under the same failure removes
/// the written item.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_listing_never_leaves_a_listed_secret_without_its_item() -> TestResult {
    let (s, keychain, dir) = flaky_services("r11-2")?;
    let target = &pg_targets()[1];
    let db = varve_records::SecretKindName::DbPassword;
    let ssh = varve_records::SecretKindName::SshPassword;
    let p = s.create_connection(&input(target, Environment::Development, false))?;
    s.save_secret(&p.id, db, secret("old-pw")?)?;
    let sql = |text: &str| {
        s.store.with_conn(|c| {
            c.execute_batch(text)?;
            Ok(())
        })
    };
    sql(
        "CREATE TRIGGER m2_ipc_r11_list BEFORE UPDATE OF saved_secrets ON core_connections \
         BEGIN SELECT RAISE(ABORT, 'm2_ipc_r11 listing fails'); END;",
    )?;
    let over = s.save_secret(&p.id, db, secret("new-pw")?);
    let fresh = s.save_secret(&p.id, ssh, secret("ssh-pw")?);
    sql("DROP TRIGGER m2_ipc_r11_list;")?;
    assert!(over.is_err(), "{over:?}");
    assert!(fresh.is_err(), "{fresh:?}");
    let listed = s.records.connection(&p.id)?.saved_secrets;
    assert_eq!(listed, vec![db]);
    let db_key = varve_secrets::SecretKey::new(p.id.clone(), db.into())?;
    let kept = varve_secrets::SecretStore::get(&keychain.inner, &db_key)?;
    assert!(
        kept.is_some(),
        "the listed password no longer exists in the keychain"
    );
    let ssh_key = varve_secrets::SecretKey::new(p.id.clone(), ssh.into())?;
    assert!(
        varve_secrets::SecretStore::get(&keychain.inner, &ssh_key)?.is_none(),
        "an unlisted item stayed in the keychain"
    );
    assert!(
        s.pending_keychain_deletions().is_empty(),
        "{:?}",
        s.pending_keychain_deletions()
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The tab's Cancel stops an EXPLAIN ANALYZE as a whole, wherever it
/// arrives: right after the call registered (before it has the session),
/// during the catalog's check, and during the run of a slow statement. It
/// always stops within a second, is logged as cancelled, and the session
/// stays usable (nothing left open).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn cancel_stops_explain_analyze_at_every_step() -> TestResult {
    let f = fixture("explain-cancel")?;
    let s = &f.services;
    let analyze = ExplainOptions {
        analyze: true,
        ..Default::default()
    };
    let [_, pg18] = pg_targets();
    let [.., mariadb123] = my_targets();
    for target in only_selected([pg18, mariadb123]) {
        let name = target.name;
        let sleep = if name.starts_with("PG") {
            "SELECT pg_sleep(5)"
        } else {
            "SELECT SLEEP(5)"
        };
        let tab = open(s, &target, Environment::Development, false).await?;
        for delay_ms in [0_u64, 2, 10, 40, 150, 600, 1500] {
            let task = {
                let s = Arc::clone(s);
                let (id, options) = (tab.id.clone(), analyze);
                tokio::spawn(async move { s.explain(&id, sleep, &options, true).await })
            };
            let registered = std::time::Instant::now();
            while s.m2.running_explains(&tab.id) == 0 {
                if registered.elapsed() > std::time::Duration::from_secs(2) {
                    return Err(format!("{name}: the explain never registered").into());
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            let sent = std::time::Instant::now();
            s.cancel(&tab.id).await?;
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), task).await;
            let Ok(joined) = outcome else {
                return Err(format!(
                    "{name}: a Cancel {delay_ms} ms in did not stop the explain within a second"
                )
                .into());
            };
            let result = joined?;
            assert!(
                result.is_err(),
                "{name}: a Cancel {delay_ms} ms in let the explain finish: {result:?}"
            );
            assert!(
                format!("{result:?}").to_lowercase().contains("cancel"),
                "{name}: a Cancel {delay_ms} ms in failed with something else: {result:?}"
            );
            assert!(sent.elapsed() < std::time::Duration::from_secs(1), "{name}");
            assert_eq!(s.m2.running_explains(&tab.id), 0, "{name}");
            // The session is usable and nothing is left open.
            let one = int_of(first_value(&run(s, &tab, "SELECT 1").await?));
            assert_eq!(one, Some(1), "{name}: after a Cancel {delay_ms} ms in");
            let plan = s
                .explain(&tab.id, "SELECT 1", &ExplainOptions::default(), false)
                .await?;
            assert!(!plan.analyzed, "{name}");
            let open_transaction = if name.starts_with("PG") {
                "SELECT (pg_catalog.transaction_timestamp() OPERATOR(pg_catalog.<>) \
                 pg_catalog.statement_timestamp())::int"
            } else {
                "SELECT @@in_transaction"
            };
            let in_transaction = int_of(first_value(&run(s, &tab, open_transaction).await?));
            assert_eq!(
                in_transaction,
                Some(0),
                "{name}: a Cancel {delay_ms} ms in left a transaction open"
            );
        }
        assert!(
            actions(s, &tab.connection_id)?
                .iter()
                .any(|a| a == "query.cancelled"),
            "{name}: cancelled explains are not logged as cancelled"
        );
        s.close_session(&tab.id).await?;
    }
    Ok(())
}

/// Security event counts of a connection: `privilege.unverified`,
/// `privilege.granted`, `privilege.revoked`, `user.created`,
/// `user.altered`, `user.dropped`.
fn security_counts(s: &Services, connection_id: &str) -> Res<[usize; 6]> {
    let actions = actions(s, connection_id)?;
    Ok([
        "privilege.unverified",
        "privilege.granted",
        "privilege.revoked",
        "user.created",
        "user.altered",
        "user.dropped",
    ]
    .map(|action| count_of(&actions, action)))
}

/// The `reason` of every `privilege.unverified` event of a connection, and
/// the `password` detail of every `user.*` event.
fn security_details(s: &Services, connection_id: &str) -> Res<(Vec<String>, Vec<String>)> {
    let page = s.activity(&ActivityFilter {
        connection_id: Some(connection_id.to_string()),
        limit: 500,
        ..Default::default()
    })?;
    let detail = |e: &varve_activity::ActivityEvent, key: &str| {
        e.details
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    let reasons = page
        .events
        .iter()
        .filter(|e| e.action == "privilege.unverified")
        .filter_map(|e| detail(e, "reason"))
        .collect();
    let passwords = page
        .events
        .iter()
        .filter(|e| e.action.starts_with("user."))
        .filter_map(|e| detail(e, "password"))
        .collect();
    Ok((reasons, passwords))
}

/// Adds `delta` to `counts`.
fn plus(counts: [usize; 6], delta: [usize; 6]) -> [usize; 6] {
    let mut sum = counts;
    for (value, add) in sum.iter_mut().zip(delta) {
        *value += add;
    }
    sum
}

/// Runs `cases` (script, whether it may fail, events it adds) on
/// `session`, checking the security event counts after each.
async fn run_security_cases(
    s: &Services,
    session: &SessionInfo,
    name: &str,
    cases: &[(&str, bool, [usize; 6])],
) -> TestResult {
    for (sql, may_fail, delta) in cases {
        let before = security_counts(s, &session.connection_id)?;
        let result = run(s, session, sql).await;
        assert!(*may_fail || result.is_ok(), "{name}: {sql}: {result:?}");
        assert!(!*may_fail || result.is_err(), "{name}: {sql} did not fail");
        assert_eq!(
            security_counts(s, &session.connection_id)?,
            plus(before, *delta),
            "{name}: security events after {sql}"
        );
    }
    Ok(())
}

/// Drops every PostgreSQL role named `m2_ipc_r5*` with what it owns in the
/// connected database.
const PG_R5_CLEANUP: &str = "DO $$ DECLARE r text; BEGIN \
     FOR r IN SELECT rolname FROM pg_catalog.pg_roles WHERE rolname LIKE 'm2\\_ipc\\_r5%' LOOP \
     EXECUTE format('DROP OWNED BY %I', r); EXECUTE format('DROP ROLE %I', r); \
     END LOOP; END $$";

/// Partial results audit round 5 (Medium 1, Low 1, Low 2): PostgreSQL role
/// statements are `user.*` events, membership options and default
/// privileges are exact privilege events, ownership changes are in doubt, a
/// rejected statement is no event, and a GRANT or REVOKE the server says
/// had no effect is in doubt instead of exact.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn postgres_role_statements_and_no_effect_grants_are_logged() -> TestResult {
    let f = fixture("r5-pg")?;
    let s = &f.services;
    for target in only_selected(pg_targets()) {
        let name = target.name;
        let pg = open(s, &target, Environment::Development, false).await?;
        let pg18 = name == "PG 18";
        run(s, &pg, "DROP SCHEMA IF EXISTS m2_ipc_r5s CASCADE").await?;
        run(s, &pg, "DROP TABLE IF EXISTS m2_ipc_r5t").await?;
        run(s, &pg, PG_R5_CLEANUP).await?;
        run(
            s,
            &pg,
            "CREATE TABLE m2_ipc_r5t (a int); CREATE ROLE m2_ipc_r5g; CREATE ROLE m2_ipc_r5u; \
             CREATE ROLE m2_ipc_r5o",
        )
        .await?;
        let created_member = [0, 1, 0, 1, 0, 0];
        let altered = [0, 0, 0, 0, 1, 0];
        let granted = [0, 1, 0, 0, 0, 0];
        let revoked = [0, 0, 1, 0, 0, 0];
        let unverified = [1, 0, 0, 0, 0, 0];
        let nothing = [0; 6];
        let cases: Vec<(&str, bool, [usize; 6])> = vec![
            (
                "CREATE ROLE m2_ipc_r5n IN ROLE m2_ipc_r5g",
                false,
                created_member,
            ),
            (
                "CREATE ROLE m2_ipc_r5m ADMIN m2_ipc_r5u",
                false,
                created_member,
            ),
            (
                "CREATE GROUP m2_ipc_r5h USER m2_ipc_r5u",
                false,
                created_member,
            ),
            ("ALTER ROLE m2_ipc_r5u CREATEROLE", false, altered),
            ("ALTER USER m2_ipc_r5u WITH BYPASSRLS", false, altered),
            (
                "ALTER DEFAULT PRIVILEGES FOR ROLE m2_ipc_r5o GRANT SELECT ON TABLES TO m2_ipc_r5u",
                false,
                granted,
            ),
            (
                "ALTER DEFAULT PRIVILEGES FOR ROLE m2_ipc_r5o \
                 REVOKE SELECT ON TABLES FROM m2_ipc_r5u",
                false,
                revoked,
            ),
            (
                "CREATE SCHEMA m2_ipc_r5s CREATE TABLE x (a int) \
                 GRANT SELECT ON TABLE x TO m2_ipc_r5u",
                false,
                granted,
            ),
            (
                "ALTER TABLE m2_ipc_r5t OWNER TO m2_ipc_r5o",
                false,
                unverified,
            ),
            ("REASSIGN OWNED BY m2_ipc_r5o TO varve", false, unverified),
            ("DROP OWNED BY m2_ipc_r5u", false, revoked),
            (
                "ALTER GROUP m2_ipc_r5g RENAME TO m2_ipc_r5g2",
                false,
                altered,
            ),
            (
                "ALTER GROUP m2_ipc_r5g2 RENAME TO m2_ipc_r5g",
                false,
                altered,
            ),
            ("DROP ROLE m2_ipc_r5n", false, [0, 0, 0, 0, 0, 1]),
            // Rejected by the server: nothing changed, no event (Low 2).
            (
                "GRANT SELECT ON m2_ipc_r5missing TO m2_ipc_r5u",
                true,
                nothing,
            ),
            (
                "ALTER GROUP m2_ipc_r5g ADD USER m2_ipc_r5missing",
                true,
                nothing,
            ),
            // A membership already held (Low 1).
            ("GRANT m2_ipc_r5g TO m2_ipc_r5u", false, granted),
            ("GRANT m2_ipc_r5g TO m2_ipc_r5u", false, unverified),
            // A membership granted by another role: PostgreSQL 16 and later
            // keep it on REVOKE and warn; 13 removes it.
            (
                "GRANT m2_ipc_r5g TO m2_ipc_r5o WITH ADMIN OPTION",
                false,
                granted,
            ),
            (
                "GRANT m2_ipc_r5g TO m2_ipc_r5m GRANTED BY m2_ipc_r5o",
                false,
                granted,
            ),
            (
                "REVOKE m2_ipc_r5g FROM m2_ipc_r5m",
                false,
                if pg18 { unverified } else { revoked },
            ),
            (
                "CREATE ROLE m2_ipc_r5l LOGIN PASSWORD 'r5-Login-pw'",
                false,
                [0, 0, 0, 1, 0, 0],
            ),
            ("GRANT SELECT ON m2_ipc_r5t TO m2_ipc_r5l", false, granted),
        ];
        run_security_cases(s, &pg, name, &cases).await?;
        let member = |role: &'static str| {
            format!(
                "SELECT count(*)::text FROM pg_catalog.pg_auth_members m \
                 JOIN pg_catalog.pg_roles g ON g.oid = m.roleid \
                 JOIN pg_catalog.pg_roles u ON u.oid = m.member \
                 WHERE g.rolname = 'm2_ipc_r5g' AND u.rolname = '{role}'"
            )
        };
        assert_eq!(
            text_value(s, &pg, &member("m2_ipc_r5m")).await?.as_deref(),
            Some(if pg18 { "1" } else { "0" }),
            "{name}: membership after the REVOKE"
        );
        // A non-superuser without the grant option: the server warns and
        // changes nothing; no exact event.
        let mut login = input(&target, Environment::Development, false);
        login.name = format!("{name} m2_ipc_r5l");
        login.username = Some("m2_ipc_r5l".into());
        let login = s.create_connection(&login)?;
        let limited = s
            .open_session(&login.id, Some(secret("r5-Login-pw")?), None, None)
            .await?;
        let acl = "SELECT coalesce(relacl::text, '') FROM pg_catalog.pg_class \
                   WHERE oid = 'public.m2_ipc_r5t'::regclass";
        let before = text_value(s, &pg, acl).await?;
        run_security_cases(
            s,
            &limited,
            name,
            &[(
                "GRANT SELECT ON m2_ipc_r5t TO m2_ipc_r5u",
                false,
                unverified,
            )],
        )
        .await?;
        assert_eq!(
            text_value(s, &pg, acl).await?,
            before,
            "{name}: ACL changed"
        );
        run(s, &pg, "GRANT SELECT ON m2_ipc_r5t TO m2_ipc_r5u").await?;
        run_security_cases(
            s,
            &limited,
            name,
            &[(
                "REVOKE SELECT ON m2_ipc_r5t FROM m2_ipc_r5u",
                false,
                unverified,
            )],
        )
        .await?;
        assert!(
            text_value(s, &pg, acl)
                .await?
                .is_some_and(|a| a.contains("m2_ipc_r5u=r/")),
            "{name}: the REVOKE without effect removed the privilege"
        );
        let (reasons, _) = security_details(s, &limited.connection_id)?;
        assert_eq!(reasons, ["no_effect", "no_effect"], "{name}");
        let (reasons, passwords) = security_details(s, &pg.connection_id)?;
        for reason in ["ownership", "no_effect"] {
            assert!(
                reasons.iter().any(|r| r == reason),
                "{name}: {reason} in {reasons:?}"
            );
        }
        assert_eq!(passwords, ["changed"], "{name}");
        s.close_session(&limited.id).await?;
        run(s, &pg, "DROP SCHEMA m2_ipc_r5s CASCADE").await?;
        run(s, &pg, "DROP TABLE m2_ipc_r5t").await?;
        run(s, &pg, PG_R5_CLEANUP).await?;
        assert_eq!(
            text_value(
                s,
                &pg,
                "SELECT count(*)::text FROM pg_catalog.pg_roles WHERE rolname LIKE 'm2\\_ipc\\_r5%'"
            )
            .await?
            .as_deref(),
            Some("0"),
            "{name}: roles left"
        );
        s.close_session(&pg.id).await?;
    }
    Ok(())
}

/// Partial results audit round 5 (High 1, Medium 2 to 4, Low 3, Low 5) on
/// MySQL and MariaDB: the statement an exactly reported script failed at is
/// examined, account and role statements are `user.*` events, more grant
/// table writes and events are in doubt, a GRANT inside a string is not,
/// and `USE mysql` in the profile's init SQL counts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mysql_account_statements_and_failing_scripts_are_logged() -> TestResult {
    let f = fixture("r5-my")?;
    let s = &f.services;
    for target in only_selected(my_targets()) {
        let name = target.name;
        let mariadb = target.engine == Engine::Mariadb;
        let my = open(s, &target, Environment::Development, false).await?;
        let other = open(s, &target, Environment::Development, false).await?;
        let cleanup = [
            "DROP DATABASE IF EXISTS m2_ipc_r5",
            "DROP USER IF EXISTS m2r5_a, m2r5_b, m2r5_b2, m2r5_c",
            "DROP ROLE IF EXISTS m2r5_r, m2r5_s",
        ];
        for sql in cleanup {
            run(s, &other, sql).await?;
        }
        for sql in [
            "CREATE DATABASE m2_ipc_r5",
            "CREATE USER m2r5_a",
            "CREATE ROLE m2r5_r",
            "CREATE PROCEDURE m2_ipc_r5.gf() BEGIN GRANT INSERT ON m2_ipc_r5.* TO m2r5_a; \
             SELECT * FROM m2_ipc_r5.missing; END",
        ] {
            run(s, &other, sql).await?;
        }
        let grants = || async {
            let output = run(s, &other, "SHOW GRANTS FOR m2r5_a").await?;
            Ok::<String, Box<dyn std::error::Error>>(format!("{:?}", output.results))
        };
        let unverified = [1, 0, 0, 0, 0, 0];
        let granted_and_doubt = [1, 1, 0, 0, 0, 0];
        let created = [0, 0, 0, 1, 0, 0];
        let created_member = [0, 1, 0, 1, 0, 0];
        let altered = [0, 0, 0, 0, 1, 0];
        let dropped = [0, 0, 0, 0, 0, 1];
        let nothing = [0; 6];
        let partly = if mariadb { unverified } else { nothing };
        let mut cases: Vec<(&str, bool, [usize; 6])> = vec![
            // High 1: what the failing statement did before it failed.
            (
                "GRANT SELECT ON m2_ipc_r5.* TO m2r5_a; CALL m2_ipc_r5.gf()",
                true,
                granted_and_doubt,
            ),
            (
                "GRANT SELECT ON m2_ipc_r5.* TO m2r5_a; \
                 PREPARE g FROM 'CALL m2_ipc_r5.gf()'; EXECUTE g",
                true,
                granted_and_doubt,
            ),
            (
                "GRANT UPDATE ON m2_ipc_r5.* TO m2r5_a, m2r5_missing",
                true,
                partly,
            ),
        ];
        run_security_cases(s, &my, name, &cases).await?;
        let shown = grants().await?;
        assert!(shown.contains("INSERT"), "{name}: {shown}");
        assert_eq!(shown.contains("UPDATE"), mariadb, "{name}: {shown}");
        cases = vec![(
            "REVOKE UPDATE ON m2_ipc_r5.* FROM m2r5_a, m2r5_missing",
            true,
            partly,
        )];
        run_security_cases(s, &my, name, &cases).await?;
        assert!(!grants().await?.contains("UPDATE"), "{name}");
        // Medium 2: account and role statements.
        cases = vec![("CREATE USER m2r5_b", false, created)];
        if mariadb {
            cases.extend([
                ("CREATE USER m2r5_c", false, created),
                ("GRANT m2r5_r TO m2r5_c", false, [0, 1, 0, 0, 0, 0]),
                ("SET DEFAULT ROLE m2r5_r FOR m2r5_c", false, altered),
                (
                    "CREATE ROLE m2r5_s WITH ADMIN m2r5_a",
                    false,
                    created_member,
                ),
            ]);
        } else {
            cases.extend([
                (
                    "CREATE USER m2r5_c DEFAULT ROLE m2r5_r",
                    false,
                    created_member,
                ),
                ("SET DEFAULT ROLE NONE TO m2r5_c", false, altered),
                ("ALTER USER m2r5_c DEFAULT ROLE m2r5_r", false, altered),
                ("CREATE ROLE m2r5_s", false, created),
            ]);
        }
        cases.extend([
            ("RENAME USER m2r5_b TO m2r5_b2", false, altered),
            (
                "ALTER USER m2r5_b2 IDENTIFIED BY 'r5-Secret-pw'",
                false,
                altered,
            ),
            ("DROP USER m2r5_b2", false, dropped),
            ("DROP ROLE m2r5_s", false, dropped),
            // Medium 4: an event that grants later.
            (
                "CREATE EVENT m2_ipc_r5.ev ON SCHEDULE AT CURRENT_TIMESTAMP + INTERVAL 1 HOUR \
                 DO GRANT SELECT ON m2_ipc_r5.* TO m2r5_a",
                false,
                unverified,
            ),
            // Low 3: a GRANT in a string is no code.
            ("SELECT 1; SELECT 'GRANT ALL ON *.* TO x'", false, nothing),
        ]);
        // Medium 3: grant tables the classifier did not know (written and
        // removed again before any reload).
        if mariadb {
            cases.extend([
                (
                    "INSERT INTO mysql.roles_mapping VALUES ('%', 'm2r5_a', 'm2r5_r', 'N')",
                    false,
                    unverified,
                ),
                (
                    "DELETE FROM mysql.roles_mapping WHERE User = 'm2r5_a' AND Role = 'm2r5_r'",
                    false,
                    unverified,
                ),
                (
                    "ANALYZE UPDATE mysql.db SET Select_priv = 'Y' WHERE User = 'm2r5_none'",
                    false,
                    unverified,
                ),
            ]);
        } else {
            cases.extend([
                (
                    "INSERT INTO mysql.global_grants VALUES ('m2r5_a', '%', 'BACKUP_ADMIN', 'N')",
                    false,
                    unverified,
                ),
                (
                    "DELETE FROM mysql.global_grants WHERE USER = 'm2r5_a'",
                    false,
                    unverified,
                ),
            ]);
        }
        run_security_cases(s, &my, name, &cases).await?;
        let (reasons, passwords) = security_details(s, &my.connection_id)?;
        for reason in [
            "procedure_call",
            "dynamic_sql",
            "scheduled_event",
            "grant_tables",
        ] {
            assert!(
                reasons.iter().any(|r| r == reason),
                "{name}: {reason} in {reasons:?}"
            );
        }
        assert_eq!(
            reasons.iter().filter(|r| *r == "not_confirmed").count(),
            if mariadb { 2 } else { 0 },
            "{name}: {reasons:?}"
        );
        assert_eq!(passwords, ["changed"], "{name}");
        // Low 5: USE mysql in the profile's init SQL.
        let mut profile = input(&target, Environment::Development, false);
        profile.name = format!("{name} m2_ipc_r5 init");
        profile.options.init_sql = Some("USE mysql".into());
        let profile = s.create_connection(&profile)?;
        let init = s
            .open_session(&profile.id, Some(secret("varve_dev")?), None, None)
            .await?;
        run_security_cases(
            s,
            &init,
            name,
            &[(
                "UPDATE db SET Select_priv = 'Y' WHERE User = 'm2r5_none'",
                false,
                unverified,
            )],
        )
        .await?;
        s.close_session(&init.id).await?;
        for sql in cleanup {
            run(s, &other, sql).await?;
        }
        s.close_session(&other.id).await?;
        s.close_session(&my.id).await?;
    }
    Ok(())
}

/// A grid plan stays available when nothing ran: here the transaction check
/// before the apply fails because the server ended the tab's connection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn a_grid_plan_is_kept_when_the_check_before_it_fails() -> TestResult {
    let f = fixture("grid-kept")?;
    let s = &f.services;
    let target = &pg_targets()[1];
    let profile = s.create_connection(&input(target, Environment::Development, false))?;
    s.save_secret(
        &profile.id,
        varve_records::SecretKindName::DbPassword,
        secret("varve_dev")?,
    )?;
    let admin = s.open_session(&profile.id, None, None, None).await?;
    run(s, &admin, "DROP SCHEMA IF EXISTS m2_ipc_kept CASCADE").await?;
    run(
        s,
        &admin,
        "CREATE SCHEMA m2_ipc_kept; CREATE TABLE m2_ipc_kept.items (id int PRIMARY KEY, label text)",
    )
    .await?;
    let session = s.open_session(&profile.id, None, None, None).await?;
    let table = TableTarget {
        database: "varve".into(),
        schema: Some("m2_ipc_kept".into()),
        table: "items".into(),
    };
    let key = s.editable_key(&session.id, &table).await?.ok_or("no key")?;
    let changes: Vec<RowChange> = serde_json::from_value(json!([
        { "op": "insert", "values": [
            ["id", { "kind": "value", "value": { "t": "int", "v": 1 } }],
            ["label", { "kind": "value", "value": { "t": "text", "v": "kept" } }]
        ] }
    ]))?;
    let planned = s.row_edit_plan(
        &session.id,
        &table,
        &changes,
        &PlanOptions {
            check_original: false,
            key,
        },
    )?;
    let pid = int_of(first_value(
        &run(s, &session, "SELECT pg_backend_pid()").await?,
    ))
    .ok_or("no pid")?;
    run(s, &admin, &format!("SELECT pg_terminate_backend({pid})")).await?;
    // Give the driver a moment to see the connection end.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let refused = s.row_edit_apply(&session.id, &planned.plan_id, false).await;
    assert!(refused.is_err(), "{refused:?}");
    // Nothing ran, so the plan was not spent.
    assert!(
        s.m2.plans.peek(&session.id, &planned.plan_id).is_ok(),
        "the plan was spent although nothing ran: {refused:?}"
    );
    let count = int_of(first_value(
        &run(s, &admin, "SELECT count(*) FROM m2_ipc_kept.items").await?,
    ));
    assert_eq!(count, Some(0));
    run(s, &admin, "DROP SCHEMA m2_ipc_kept CASCADE").await?;
    s.close_session(&admin.id).await?;
    let _ = s.close_session(&session.id).await;
    Ok(())
}

/// Every event of a connection with `action`.
fn events_of(
    s: &Services,
    connection_id: &str,
    action: &str,
) -> Res<Vec<varve_activity::ActivityEvent>> {
    let page = s.activity(&ActivityFilter {
        connection_id: Some(connection_id.to_string()),
        limit: 500,
        ..Default::default()
    })?;
    Ok(page
        .events
        .into_iter()
        .filter(|e| e.action == action)
        .collect())
}

/// True when some `action` event of the connection has `details[key]`
/// containing `value` (a string or an array holding it) and `target`.
fn has_detail(
    s: &Services,
    connection_id: &str,
    action: &str,
    key: &str,
    value: &str,
    target: Option<&str>,
) -> Res<bool> {
    Ok(events_of(s, connection_id, action)?.iter().any(|e| {
        let found = match e.details.get(key) {
            Some(serde_json::Value::String(v)) => v == value,
            Some(serde_json::Value::Array(items)) => items.iter().any(|i| i == value),
            _ => false,
        };
        found && target.is_none_or(|t| e.target.as_deref() == Some(t))
    }))
}

const PG_R6_CLEANUP: &str = "DO $$ DECLARE r text; BEGIN \
     FOR r IN SELECT rolname FROM pg_catalog.pg_roles WHERE rolname LIKE 'm2\\_ipc\\_r6%' LOOP \
     EXECUTE format('DROP OWNED BY %I', r); EXECUTE format('DROP ROLE %I', r); \
     END LOOP; END $$";

/// Partial results audit round 6 on PostgreSQL 13 and 18: structured
/// details (Medium 1), catalog writes (Medium 3), the PostgreSQL 16+
/// creator membership (Low 3) and no false downgrades or doubts (Low 5).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn postgres_round6_privilege_paths_are_logged() -> TestResult {
    let f = fixture("r6-pg")?;
    let s = &f.services;
    for target in only_selected(pg_targets()) {
        let name = target.name;
        let pg18 = name == "PG 18";
        let pg = open(s, &target, Environment::Development, false).await?;
        run(s, &pg, "DROP TABLE IF EXISTS m2_ipc_r6t").await?;
        run(s, &pg, PG_R6_CLEANUP).await?;
        run(
            s,
            &pg,
            "CREATE TABLE m2_ipc_r6t (a int, owner int); CREATE ROLE m2_ipc_r6u; \
             CREATE ROLE m2_ipc_r6c LOGIN CREATEROLE PASSWORD 'r6-Login-pw'",
        )
        .await?;
        let unverified = [1, 0, 0, 0, 0, 0];
        let granted = [0, 1, 0, 0, 0, 0];
        let nothing = [0; 6];
        let cases: Vec<(&str, bool, [usize; 6])> = vec![
            (
                "ALTER ROLE m2_ipc_r6u SUPERUSER CREATEROLE",
                false,
                [0, 0, 0, 0, 1, 0],
            ),
            (
                "ALTER ROLE m2_ipc_r6u NOSUPERUSER",
                false,
                [0, 0, 0, 0, 1, 0],
            ),
            (
                "UPDATE pg_catalog.pg_authid SET rolcreatedb = true WHERE rolname = 'm2_ipc_r6u'",
                false,
                unverified,
            ),
            (
                "GRANT SELECT ON m2_ipc_r6t TO m2_ipc_r6u; COMMIT",
                false,
                granted,
            ),
            (
                "ALTER TABLE m2_ipc_r6t RENAME COLUMN owner TO o2",
                false,
                nothing,
            ),
        ];
        run_security_cases(s, &pg, name, &cases).await?;
        assert!(
            text_value(
                s,
                &pg,
                "SELECT rolcreatedb::text FROM pg_catalog.pg_roles WHERE rolname = 'm2_ipc_r6u'"
            )
            .await?
            .as_deref()
                == Some("true"),
            "{name}: the catalog write did not apply"
        );
        assert!(
            has_detail(
                s,
                &pg.connection_id,
                "user.altered",
                "attributes",
                "SUPERUSER",
                Some("m2_ipc_r6u")
            )?,
            "{name}: no SUPERUSER attribute"
        );
        assert!(has_detail(
            s,
            &pg.connection_id,
            "privilege.granted",
            "grantees",
            "m2_ipc_r6u",
            Some("m2_ipc_r6u")
        )?);
        assert!(has_detail(
            s,
            &pg.connection_id,
            "privilege.granted",
            "object",
            "m2_ipc_r6t",
            None
        )?);
        let (reasons, _) = security_details(s, &pg.connection_id)?;
        assert!(
            reasons.iter().any(|r| r == "catalog_write"),
            "{name}: {reasons:?}"
        );
        assert!(
            !reasons.iter().any(|r| r == "no_effect" || r == "ownership"),
            "{name}: {reasons:?}"
        );
        // A non-superuser with CREATEROLE: PostgreSQL 16+ grants the new
        // role to its creator.
        let mut login = input(&target, Environment::Development, false);
        login.name = format!("{name} m2_ipc_r6c");
        login.username = Some("m2_ipc_r6c".into());
        let login = s.create_connection(&login)?;
        let creator = s
            .open_session(&login.id, Some(secret("r6-Login-pw")?), None, None)
            .await?;
        run_security_cases(
            s,
            &creator,
            name,
            &[(
                "CREATE ROLE m2_ipc_r6n",
                false,
                if pg18 {
                    [0, 1, 0, 1, 0, 0]
                } else {
                    [0, 0, 0, 1, 0, 0]
                },
            )],
        )
        .await?;
        let member = text_value(
            s,
            &pg,
            "SELECT count(*)::text FROM pg_catalog.pg_auth_members m \
             JOIN pg_catalog.pg_roles g ON g.oid = m.roleid \
             JOIN pg_catalog.pg_roles u ON u.oid = m.member \
             WHERE g.rolname = 'm2_ipc_r6n' AND u.rolname = 'm2_ipc_r6c'",
        )
        .await?;
        assert_eq!(
            member.as_deref(),
            Some(if pg18 { "1" } else { "0" }),
            "{name}"
        );
        if pg18 {
            assert!(has_detail(
                s,
                &creator.connection_id,
                "privilege.granted",
                "accounts",
                "m2_ipc_r6n",
                Some("m2_ipc_r6n")
            )?);
        }
        s.close_session(&creator.id).await?;
        run(s, &pg, "DROP TABLE m2_ipc_r6t").await?;
        run(s, &pg, PG_R6_CLEANUP).await?;
        s.close_session(&pg.id).await?;
    }
    Ok(())
}

/// Partial results audit round 6 on MySQL and MariaDB: structured
/// details with hosts (Medium 1), ALTER EVENT without a body (Medium 4),
/// routine privileges (Low 2), skip warnings (Low 4), no noise (Low 5) and
/// MariaDB CREATE OR REPLACE (Low 6).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mysql_round6_privilege_paths_are_logged() -> TestResult {
    let f = fixture("r6-my")?;
    let s = &f.services;
    for target in only_selected(my_targets()) {
        let name = target.name;
        let mariadb = target.engine == Engine::Mariadb;
        let my = open(s, &target, Environment::Development, false).await?;
        let other = open(s, &target, Environment::Development, false).await?;
        let cleanup = [
            "DROP DATABASE IF EXISTS m2_ipc_r6",
            "DROP USER IF EXISTS m2r6_a, m2r6_b",
            "DROP ROLE IF EXISTS m2r6_r, m2r6_s",
        ];
        for sql in cleanup {
            run(s, &other, sql).await?;
        }
        for sql in [
            "CREATE DATABASE m2_ipc_r6",
            "CREATE USER m2r6_a",
            "CREATE ROLE m2r6_r",
            "CREATE EVENT m2_ipc_r6.ev ON SCHEDULE AT CURRENT_TIMESTAMP + INTERVAL 1 DAY \
             DO GRANT SELECT ON m2_ipc_r6.* TO m2r6_a",
        ] {
            run(s, &other, sql).await?;
        }
        let unverified = [1, 0, 0, 0, 0, 0];
        let nothing = [0; 6];
        let mut cases: Vec<(&str, bool, [usize; 6])> = vec![
            (
                "GRANT SELECT ON m2_ipc_r6.* TO 'm2r6_a'@'%'",
                false,
                [0, 1, 0, 0, 0, 0],
            ),
            (
                "ALTER EVENT m2_ipc_r6.ev ON SCHEDULE AT CURRENT_TIMESTAMP + INTERVAL 2 DAY",
                false,
                unverified,
            ),
            ("CREATE PROCEDURE m2_ipc_r6.p() SELECT 1", false, unverified),
            ("DROP PROCEDURE m2_ipc_r6.p", false, unverified),
            ("SET @mandatory_roles = 1", false, nothing),
            ("SELECT @@global.event_scheduler", false, nothing),
        ];
        if mariadb {
            cases.extend([
                (
                    "CREATE ROLE IF NOT EXISTS m2r6_r",
                    false,
                    [1, 0, 0, 1, 0, 0],
                ),
                ("SET PASSWORD FOR m2r6_a = 'plain'", true, nothing),
                ("CREATE OR REPLACE USER m2r6_b", false, [0, 0, 0, 1, 0, 0]),
                (
                    "SELECT 1; CREATE OR REPLACE ROLE m2r6_s; SELECT 2",
                    false,
                    [0, 1, 0, 1, 0, 0],
                ),
            ]);
        } else {
            cases.extend([
                ("SELECT @@global.mandatory_roles", false, nothing),
                (
                    "CREATE USER IF NOT EXISTS m2r6_a DEFAULT ROLE m2r6_r",
                    false,
                    [1, 0, 0, 1, 0, 0],
                ),
                (
                    // No grant on that database at all: MySQL warns 1141.
                    "REVOKE IF EXISTS INSERT ON m2_ipc_r6_none.* FROM m2r6_a",
                    false,
                    unverified,
                ),
                (
                    "REVOKE SELECT ON m2_ipc_r6.* FROM m2r6_missing IGNORE UNKNOWN USER",
                    false,
                    unverified,
                ),
            ]);
        }
        run_security_cases(s, &my, name, &cases).await?;
        assert!(
            has_detail(
                s,
                &my.connection_id,
                "privilege.granted",
                "grantees",
                "m2r6_a@%",
                Some("m2r6_a@%")
            )?,
            "{name}: no grantee with host"
        );
        assert!(has_detail(
            s,
            &my.connection_id,
            "privilege.granted",
            "object",
            "m2_ipc_r6.*",
            None
        )?);
        let (reasons, _) = security_details(s, &my.connection_id)?;
        for reason in ["scheduled_event", "routine_privileges", "no_effect"] {
            assert!(
                reasons.iter().any(|r| r == reason),
                "{name}: {reason} in {reasons:?}"
            );
        }
        for sql in cleanup {
            run(s, &other, sql).await?;
        }
        s.close_session(&other.id).await?;
        s.close_session(&my.id).await?;
    }
    Ok(())
}

/// Partial results audit round 6, Medium 2: the profile's init SQL is
/// recorded when the session opens (query.executed and security events,
/// `source: init_sql`), on all seven servers, and a failed open with init
/// SQL leaves a doubt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn init_sql_privilege_statements_are_logged_at_open() -> TestResult {
    let f = fixture("r6-init")?;
    let s = &f.services;
    let targets: Vec<Target> = only_selected(pg_targets())
        .into_iter()
        .chain(only_selected(my_targets()))
        .collect();
    for target in targets {
        let name = target.name;
        let pg = target.engine == Engine::Postgres;
        let admin = open(s, &target, Environment::Development, false).await?;
        let (setup, grant, check, cleanup): (Vec<&str>, &str, &str, Vec<&str>) = if pg {
            (
                vec![
                    "DROP TABLE IF EXISTS m2_ipc_r6i",
                    "DROP ROLE IF EXISTS m2_ipc_r6iu",
                    "CREATE TABLE m2_ipc_r6i (a int)",
                    "CREATE ROLE m2_ipc_r6iu",
                ],
                "GRANT SELECT ON m2_ipc_r6i TO m2_ipc_r6iu",
                "SELECT coalesce(relacl::text, '') FROM pg_catalog.pg_class \
                 WHERE relname = 'm2_ipc_r6i'",
                vec!["DROP TABLE m2_ipc_r6i", "DROP ROLE m2_ipc_r6iu"],
            )
        } else {
            (
                vec![
                    "DROP DATABASE IF EXISTS m2_ipc_r6i",
                    "DROP USER IF EXISTS m2r6_iu",
                    "CREATE DATABASE m2_ipc_r6i",
                    "CREATE USER m2r6_iu",
                ],
                "GRANT SELECT ON m2_ipc_r6i.* TO m2r6_iu",
                "SELECT CAST(COUNT(*) AS CHAR) FROM mysql.db \
                 WHERE Db = 'm2_ipc_r6i' AND User = 'm2r6_iu'",
                vec!["DROP DATABASE m2_ipc_r6i", "DROP USER m2r6_iu"],
            )
        };
        for sql in &setup {
            run(s, &admin, sql).await?;
        }
        let mut profile = input(&target, Environment::Development, false);
        profile.name = format!("{name} m2_ipc_r6 init");
        profile.options.init_sql = Some(grant.into());
        let profile = s.create_connection(&profile)?;
        let session = s
            .open_session(&profile.id, Some(secret("varve_dev")?), None, None)
            .await?;
        // The grant was made at open.
        let state = text_value(s, &admin, check).await?.unwrap_or_default();
        assert!(
            state.contains("m2_ipc_r6iu=r") || state == "1",
            "{name}: init SQL did not grant: {state}"
        );
        assert!(
            has_detail(s, &profile.id, "query.executed", "source", "init_sql", None)?,
            "{name}: no query.executed for the init SQL"
        );
        let action = if pg {
            "privilege.granted"
        } else {
            "privilege.unverified"
        };
        assert!(
            has_detail(s, &profile.id, action, "source", "init_sql", None)?,
            "{name}: no {action} for the init SQL"
        );
        s.close_session(&session.id).await?;
        // Round 7, Medium 2: Test connection runs the init SQL as well.
        let tested = |e: &varve_activity::ActivityEvent| {
            e.details.get("test_connection").and_then(|v| v.as_str()) == Some("true")
                && e.details.get("source").and_then(|v| v.as_str()) == Some("init_sql")
        };
        let before = events_of(s, &profile.id, action)?
            .iter()
            .filter(|e| tested(e))
            .count();
        s.test_connection(
            varve_app::commands::TestTarget::Saved {
                id: profile.id.clone(),
            },
            Some(secret("varve_dev")?),
            None,
            None,
        )
        .await?;
        assert_eq!(
            events_of(s, &profile.id, action)?
                .iter()
                .filter(|e| tested(e))
                .count(),
            before + 1,
            "{name}: no {action} for the init SQL of a connection test"
        );
        assert!(
            events_of(s, &profile.id, "query.executed")?
                .iter()
                .any(tested),
            "{name}: no query.executed for the init SQL of a connection test"
        );
        // Round 8, Low 1: a refused login never ran the init SQL: no doubt.
        let failed = s
            .open_session(&profile.id, Some(secret("wrong-password")?), None, None)
            .await;
        assert!(failed.is_err(), "{name}: opened with a wrong password");
        assert!(
            events_of(s, &profile.id, "privilege.unverified")?
                .iter()
                .all(|e| e.details.get("open_failed").is_none()),
            "{name}: a doubt for a login that failed"
        );
        // An init SQL that fails after its GRANT ran: in doubt.
        let mut broken = input(&target, Environment::Development, false);
        broken.name = format!("{name} m2_ipc_r6 init broken");
        broken.options.init_sql = Some(format!("{grant}; SELECT * FROM m2_ipc_r6_missing"));
        let broken = s.create_connection(&broken)?;
        let failed = s
            .open_session(&broken.id, Some(secret("varve_dev")?), None, None)
            .await;
        assert!(failed.is_err(), "{name}: the broken init SQL did not fail");
        assert!(
            has_detail(
                s,
                &broken.id,
                "privilege.unverified",
                "open_failed",
                "true",
                None
            )?,
            "{name}: no doubt for an init SQL that failed part way"
        );
        for sql in &cleanup {
            run(s, &admin, sql).await?;
        }
        s.close_session(&admin.id).await?;
    }
    Ok(())
}

/// Partial results audit round 7 on MySQL and MariaDB: details read in the
/// session's quoting modes never leak a password and name no wrong account
/// (High 1), skip warnings belong to their statement (Low 3), every doubt
/// reason is kept (Low 4), ambiguous account names are quoted (Low 5).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mysql_round7_details_and_warnings() -> TestResult {
    let f = fixture("r7-my")?;
    let s = &f.services;
    for target in only_selected(my_targets()) {
        let name = target.name;
        let mariadb = target.engine == Engine::Mariadb;
        let my = open(s, &target, Environment::Development, false).await?;
        let other = open(s, &target, Environment::Development, false).await?;
        let cleanup = [
            "SET SESSION sql_mode = DEFAULT",
            "DROP DATABASE IF EXISTS m2_ipc_r7",
            "DROP USER IF EXISTS 'm2r7_n1\\\\', m2r7_n2, m2r7_n3, m2r7_a, 'm2r7@x'",
            "DROP ROLE IF EXISTS m2r7_r",
        ];
        for sql in cleanup {
            run(s, &other, sql).await?;
        }
        for sql in [
            "CREATE DATABASE m2_ipc_r7",
            "CREATE ROLE m2r7_r",
            "CREATE PROCEDURE m2_ipc_r7.gf() BEGIN GRANT INSERT ON m2_ipc_r7.* TO m2r7_r; \
             SELECT * FROM m2_ipc_r7.missing; END",
        ] {
            run(s, &other, sql).await?;
        }
        // High 1: NO_BACKSLASH_ESCAPES makes 'm2r7_n1\' a whole name.
        run(
            s,
            &my,
            "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES')",
        )
        .await?;
        run_security_cases(
            s,
            &my,
            name,
            &[
                (
                    "CREATE USER 'm2r7_n1\\' IDENTIFIED BY 'hunterpw', 'm2r7_n2'@'%' \
                     IDENTIFIED BY 'Hunter2pw!'",
                    false,
                    [0, 0, 0, 1, 0, 0],
                ),
                (
                    "GRANT SELECT ON m2_ipc_r7.* TO 'm2r7_n1\\', m2r7_n2",
                    false,
                    [0, 1, 0, 0, 0, 0],
                ),
            ],
        )
        .await?;
        let all = s.activity(&ActivityFilter {
            connection_id: Some(my.connection_id.clone()),
            limit: 500,
            ..Default::default()
        })?;
        for event in &all.events {
            let stored = format!("{} {:?}", event.details, event.target).to_lowercase();
            assert!(
                !stored.contains("hunter"),
                "{name}: a password in an event: {stored}"
            );
        }
        assert!(has_detail(
            s,
            &my.connection_id,
            "user.created",
            "details_omitted",
            "ambiguous_quoting",
            None
        )?);
        run(s, &my, "SET SESSION sql_mode = DEFAULT").await?;
        // ANSI_QUOTES: a double-quoted account name.
        run(s, &my, "SET SESSION sql_mode = 'ANSI_QUOTES'").await?;
        run(s, &my, "CREATE USER \"m2r7_n3\"@'%'").await?;
        assert!(has_detail(
            s,
            &my.connection_id,
            "user.created",
            "accounts",
            "m2r7_n3@%",
            Some("m2r7_n3@%")
        )?);
        run(s, &my, "SET SESSION sql_mode = DEFAULT").await?;
        // Low 5: an account name with @ is quoted.
        run(s, &my, "CREATE USER 'm2r7@x'@'%'").await?;
        assert!(has_detail(
            s,
            &my.connection_id,
            "user.created",
            "accounts",
            "'m2r7@x'@'%'",
            None
        )?);
        // Low 4: the failing CALL keeps procedure_call first.
        let result = run(
            s,
            &my,
            "GRANT SELECT ON m2_ipc_r7.* TO m2r7_r; \
             CREATE PROCEDURE m2_ipc_r7.tmpp() SELECT 1; CALL m2_ipc_r7.gf()",
        )
        .await;
        assert!(result.is_err(), "{name}: the CALL did not fail");
        let last = events_of(s, &my.connection_id, "privilege.unverified")?;
        assert!(
            last.iter().any(|e| {
                e.details.get("reason").and_then(|v| v.as_str()) == Some("procedure_call")
                    && e.details
                        .get("reasons")
                        .and_then(|v| v.as_array())
                        .is_some_and(|r| r.iter().any(|x| x == "routine_privileges"))
            }),
            "{name}: reasons not kept"
        );
        // Low 3: the DROP's warning leaves the CREATE exact (MySQL only:
        // MariaDB has no CREATE USER ... DEFAULT ROLE).
        if !mariadb {
            run_security_cases(
                s,
                &my,
                name,
                &[(
                    "CREATE USER IF NOT EXISTS m2r7_a DEFAULT ROLE m2r7_r; \
                     DROP USER IF EXISTS m2r7_missing",
                    false,
                    [0, 1, 0, 1, 0, 1],
                )],
            )
            .await?;
        }
        for sql in cleanup {
            run(s, &other, sql).await?;
        }
        s.close_session(&other.id).await?;
        s.close_session(&my.id).await?;
    }
    Ok(())
}

const PG_R7_CLEANUP: &str = "DO $$ DECLARE r text; BEGIN \
     FOR r IN SELECT rolname FROM pg_catalog.pg_roles WHERE rolname LIKE 'm2\\_ipc\\_r7%' LOOP \
     EXECUTE format('DROP OWNED BY %I', r); EXECUTE format('DROP ROLE %I', r); \
     END LOOP; END $$";

/// Partial results audit round 7 on PostgreSQL 13 and 18: `TO GROUP`
/// grantees (Medium 1), init SQL notices (Low 1), CREATE ROLE after SET
/// ROLE (Low 2).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn postgres_round7_paths() -> TestResult {
    let f = fixture("r7-pg")?;
    let s = &f.services;
    for target in only_selected(pg_targets()) {
        let name = target.name;
        let pg18 = name == "PG 18";
        let pg = open(s, &target, Environment::Development, false).await?;
        run(s, &pg, "DROP TABLE IF EXISTS m2_ipc_r7t").await?;
        run(s, &pg, PG_R7_CLEANUP).await?;
        run(
            s,
            &pg,
            "CREATE TABLE m2_ipc_r7t (a int); CREATE ROLE m2_ipc_r7g; CREATE ROLE m2_ipc_r7u; \
             CREATE ROLE m2_ipc_r7c CREATEROLE; \
             CREATE ROLE m2_ipc_r7l LOGIN PASSWORD 'r7-Login-pw'; \
             GRANT SELECT ON m2_ipc_r7t TO m2_ipc_r7l",
        )
        .await?;
        // Medium 1.
        run(s, &pg, "GRANT SELECT ON m2_ipc_r7t TO GROUP m2_ipc_r7g").await?;
        assert!(has_detail(
            s,
            &pg.connection_id,
            "privilege.granted",
            "grantees",
            "m2_ipc_r7g",
            Some("m2_ipc_r7g")
        )?);
        // Low 2: CREATE ROLE while SET ROLE to a non-superuser.
        run_security_cases(
            s,
            &pg,
            name,
            &[
                (
                    "SET ROLE m2_ipc_r7c; CREATE ROLE m2_ipc_r7n; RESET ROLE",
                    false,
                    if pg18 {
                        [1, 0, 0, 1, 0, 0]
                    } else {
                        [0, 0, 0, 1, 0, 0]
                    },
                ),
                // Read again after RESET ROLE: a superuser, so exact.
                ("CREATE ROLE m2_ipc_r7m", false, [0, 0, 0, 1, 0, 0]),
            ],
        )
        .await?;
        // Low 1: init SQL of a role without the grant option warns.
        let mut login = input(&target, Environment::Development, false);
        login.name = format!("{name} m2_ipc_r7l");
        login.username = Some("m2_ipc_r7l".into());
        login.options.init_sql = Some("GRANT SELECT ON m2_ipc_r7t TO m2_ipc_r7u".into());
        let login = s.create_connection(&login)?;
        let limited = s
            .open_session(&login.id, Some(secret("r7-Login-pw")?), None, None)
            .await?;
        assert!(
            events_of(s, &login.id, "privilege.granted")?.is_empty(),
            "{name}: a GRANT without effect logged as granted"
        );
        assert!(has_detail(
            s,
            &login.id,
            "privilege.unverified",
            "reason",
            "no_effect",
            None
        )?);
        s.close_session(&limited.id).await?;
        run(s, &pg, "DROP TABLE m2_ipc_r7t").await?;
        run(s, &pg, PG_R7_CLEANUP).await?;
        s.close_session(&pg.id).await?;
    }
    Ok(())
}
