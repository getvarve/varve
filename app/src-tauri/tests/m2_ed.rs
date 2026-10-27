//! Round trips of the M2 database, schema, rule, policy and table drop
//! editors: the changes are the JSON the UI builds (`lib/ddlDatabase.ts`,
//! `lib/ddlRls.ts`, `buildDrop` in `lib/ddlPlan.ts`), planned with
//! `edit_plan`, run with `edit_apply`, and read back with
//! `catalog_describe`; drop flows check `catalog_drop_impact`. Runs against
//! the shared M2 server matrix (compose project `varve-m2`). Objects use
//! the prefix `m2_ed`.
//!
//! Run with `cargo test -p varve-app --test m2_ed -- --include-ignored`.

use std::sync::Arc;

use serde_json::{Value as J, json};
use varve_app::commands::{CmdError, SecretInput, Services};
use varve_app::m2::editors::EngineChange;
use varve_app::m2::impact::{DependentEffect, DropImpact};
use varve_app::sessions::{SessionError, SessionInfo};
use varve_catalog::{CatalogError, ObjectKind, ObjectRef};
use varve_core::{Edition, Engine};
use varve_records::{ConnectionInput, ConnectionOptions, Environment};
use varve_secrets::MemoryStore;

type Res<T = ()> = Result<T, Box<dyn std::error::Error>>;
/// A command result whose error stays inspectable.
type Cmd<T> = Result<T, Box<CmdError>>;

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
    let dir = std::env::temp_dir().join(format!("varve-m2-ed-{name}-{}", std::process::id()));
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
    Ok(serde_json::from_value(J::String(value.into()))?)
}

struct Target {
    name: &'static str,
    engine: Engine,
    port: u16,
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

fn my_targets() -> [Target; 4] {
    [
        Target {
            name: "MySQL 8.0",
            engine: Engine::Mysql,
            port: port("VARVE_M2_MYSQL80_PORT", 35080),
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

fn input(target: &Target, read_only: bool) -> ConnectionInput {
    let (database, username) = match target.engine {
        Engine::Postgres => (Some("varve".to_string()), Some("varve".to_string())),
        _ => (None, Some("root".to_string())),
    };
    ConnectionInput {
        folder_id: None,
        name: format!("{} m2_ed", target.name),
        color: None,
        engine: target.engine,
        host: host(),
        port: Some(target.port),
        database,
        username,
        environment: Environment::Development,
        tls: Default::default(),
        ssh: Default::default(),
        options: ConnectionOptions {
            read_only,
            confirm_destructive: false,
            ..Default::default()
        },
    }
}

async fn open(s: &Services, target: &Target, read_only: bool) -> Res<SessionInfo> {
    let profile = s.create_connection(&input(target, read_only))?;
    Ok(
        s.open_session(&profile.id, Some(secret("varve_dev")?), None, None)
            .await?,
    )
}

async fn run(s: &Services, session: &SessionInfo, sql: &str) -> Res {
    s.execute(&session.id, sql, true, None).await?;
    Ok(())
}

fn pg(change: J) -> J {
    json!({ "engine": "postgres", "change": change })
}

fn my(change: J) -> J {
    json!({ "engine": "mysql", "change": change })
}

/// Plans a UI change and applies it (confirmed).
async fn apply(s: &Services, session: &SessionInfo, change: J) -> Cmd<()> {
    let change: EngineChange = serde_json::from_value(change).map_err(|e| CmdError::Invalid {
        message: e.to_string(),
    })?;
    let planned = s.edit_plan(&session.id, &change)?;
    s.edit_apply(&session.id, &planned.plan_id, true).await?;
    Ok(())
}

fn object(
    kind: ObjectKind,
    database: Option<&str>,
    schema: Option<&str>,
    parent: Option<&str>,
    name: &str,
) -> ObjectRef {
    ObjectRef {
        kind,
        database: database.map(str::to_owned),
        schema: schema.map(str::to_owned),
        parent: parent.map(str::to_owned),
        name: name.into(),
        signature: None,
    }
}

/// The described object as JSON (`{ engine, object: { kind, ... } }`).
async fn describe(s: &Services, session: &SessionInfo, target: &ObjectRef) -> Cmd<J> {
    let details = s.catalog_describe(&session.id, target).await?;
    serde_json::to_value(details).map_err(|e| {
        Box::new(CmdError::Invalid {
            message: e.to_string(),
        })
    })
}

fn is_missing(result: &Cmd<J>) -> bool {
    matches!(
        result.as_ref().map_err(|e| e.as_ref()),
        Err(CmdError::Catalog {
            error: CatalogError::NotFound { .. }
        })
    )
}

fn driver_code(result: &Cmd<()>) -> Option<String> {
    match result.as_ref().map_err(|e| e.as_ref()) {
        Err(CmdError::Session {
            error: SessionError::Driver { error },
        }) => error.code.clone(),
        _ => None,
    }
}

fn has_dependent(impact: &DropImpact, needle: &str, effect: DependentEffect) -> bool {
    impact
        .dependents
        .iter()
        .any(|d| d.description.contains(needle) && d.effect == effect)
}

fn content(impact: &DropImpact, label: &str) -> Option<String> {
    impact
        .contents
        .iter()
        .find(|c| c.label == label)
        .map(|c| c.count.clone())
}

// ---------------------------------------------------------------------------
// PostgreSQL databases
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn pg_database_editor_round_trip() -> Res {
    for target in pg_targets() {
        let fx = fixture(&format!("pgdb{}", target.port))?;
        let s = &fx.services;
        let main = open(s, &target, false).await?;
        for db in ["m2_ed_db", "m2_ed_db2"] {
            run(
                s,
                &main,
                &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
            )
            .await?;
        }
        let modern = main.server.major >= 17;

        // Create, with the settings as the second change (what the editor sends).
        let mut create = json!({
            "object": "database", "action": "create", "name": "m2_ed_db",
            "owner": { "kind": "name", "name": "varve" }, "template": "template0", "encoding": "UTF8",
            "connectionLimit": 5, "comment": "editor round trip"
        });
        if modern {
            create["localeProvider"] = json!("builtin");
            create["builtinLocale"] = json!("C.UTF-8");
            create["strategy"] = json!("fileCopy");
        }
        apply(s, &main, pg(create)).await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "database", "action": "alter", "name": "m2_ed_db",
                "operations": [{ "op": "set", "setting": { "name": "work_mem", "values": ["64MB"] } }] })),
        )
        .await?;
        let db_ref = object(
            ObjectKind::Database,
            Some("m2_ed_db"),
            None,
            None,
            "m2_ed_db",
        );
        let d = describe(s, &main, &db_ref).await?;
        assert_eq!(d["object"]["connectionLimit"], 5, "{}", target.name);
        assert_eq!(
            d["object"]["comment"], "editor round trip",
            "{}",
            target.name
        );
        assert_eq!(d["object"]["owner"], "varve");
        assert!(
            d["object"]["settings"].as_array().is_some_and(|a| a
                .iter()
                .any(|x| x["name"] == "work_mem" && x["value"] == "64MB")),
            "{}: {d}",
            target.name
        );
        if modern {
            assert_eq!(d["object"]["localeProvider"], "builtin", "{}", target.name);
        }

        // Alter: options and settings in one change, comment, then the rename.
        apply(
            s,
            &main,
            pg(
                json!({ "object": "database", "action": "alter", "name": "m2_ed_db", "operations": [
                { "op": "connectionLimit", "limit": 10 },
                { "op": "allowConnections", "allow": true },
                { "op": "isTemplate", "isTemplate": false },
                { "op": "set", "setting": { "name": "statement_timeout", "values": ["5s"] } },
                { "op": "reset", "name": "work_mem" }
            ] }),
            ),
        )
        .await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "comment", "target": db_ref, "comment": null })),
        )
        .await?;
        apply(
            s,
            &main,
            pg(
                json!({ "object": "database", "action": "alter", "name": "m2_ed_db",
                "operations": [{ "op": "rename", "newName": "m2_ed_db2" }] }),
            ),
        )
        .await?;
        assert!(
            is_missing(&describe(s, &main, &db_ref).await),
            "{}",
            target.name
        );
        let db2 = object(
            ObjectKind::Database,
            Some("m2_ed_db2"),
            None,
            None,
            "m2_ed_db2",
        );
        let d = describe(s, &main, &db2).await?;
        assert_eq!(d["object"]["connectionLimit"], 10, "{}", target.name);
        assert_eq!(d["object"]["comment"], J::Null);
        let settings = d["object"]["settings"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            settings.iter().any(|x| x["name"] == "statement_timeout"),
            "{d}"
        );
        assert!(!settings.iter().any(|x| x["name"] == "work_mem"), "{d}");

        // Another session on the database: listed by the impact, and a drop
        // without FORCE is refused (object_in_use). The browser's pooled
        // session on it is closed by the app before the drop.
        let other = s
            .open_database_session(
                &main.connection_id,
                "m2_ed_db2",
                Some(secret("varve_dev")?),
                None,
                None,
            )
            .await?;
        s.catalog_schemas(&other.id, "m2_ed_db2").await?;
        // M3: the app names its own tab on the database and its open transaction,
        // also a tab of another saved connection to the same server.
        run(s, &other, "BEGIN").await?;
        let second_profile = s.create_connection(&ConnectionInput {
            name: format!("{} m2_ed second", target.name),
            ..input(&target, false)
        })?;
        let second = s
            .open_database_session(
                &second_profile.id,
                "m2_ed_db2",
                Some(secret("varve_dev")?),
                None,
                None,
            )
            .await?;
        let impact = s.catalog_drop_impact(&main.id, &db2).await?;
        assert!(
            impact
                .app_sessions
                .iter()
                .any(|a| a.session_id == second.id && a.certain),
            "{}: {impact:?}",
            target.name
        );
        let tab = impact
            .app_sessions
            .iter()
            .find(|a| a.session_id == other.id);
        assert!(
            tab.is_some_and(|a| a.open_transaction == Some(true) && !a.busy),
            "{}: {impact:?}",
            target.name
        );
        assert!(!impact.app_sessions.iter().any(|a| a.session_id == main.id));
        assert!(!impact.sessions.is_empty(), "{}: {impact:?}", target.name);
        assert!(impact.sessions_complete);
        assert!(impact.blockers.is_empty(), "{impact:?}");
        assert!(impact.sessions.iter().all(|x| x.id.parse::<i64>().is_ok()));

        // A session connected to the database cannot drop it.
        let own = apply(
            s,
            &other,
            pg(json!({ "object": "database", "action": "drop", "name": "m2_ed_db2" })),
        )
        .await;
        assert!(
            matches!(
                own.as_ref().map_err(|e| e.as_ref()),
                Err(CmdError::Invalid { .. })
            ),
            "{}: {own:?}",
            target.name
        );

        let refused = apply(
            s,
            &main,
            pg(json!({ "object": "database", "action": "drop", "name": "m2_ed_db2" })),
        )
        .await;
        assert_eq!(
            driver_code(&refused).as_deref(),
            Some("55006"),
            "{}: {refused:?}",
            target.name
        );
        // FORCE, chosen explicitly, ends the remaining user session.
        apply(
            s,
            &main,
            pg(json!({ "object": "database", "action": "drop", "name": "m2_ed_db2", "force": true })),
        )
        .await?;
        assert!(
            is_missing(&describe(s, &main, &db2).await),
            "{}",
            target.name
        );
        let _ = s.close_session(&other.id).await;
        let _ = s.close_session(&second.id).await;

        // Without FORCE, with only the browser's own pooled session on it.
        apply(
            s,
            &main,
            pg(json!({ "object": "database", "action": "create", "name": "m2_ed_db" })),
        )
        .await?;
        let browse = s
            .open_database_session(
                &main.connection_id,
                "m2_ed_db",
                Some(secret("varve_dev")?),
                None,
                None,
            )
            .await?;
        s.catalog_schemas(&browse.id, "m2_ed_db").await?;
        s.close_session(&browse.id).await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "database", "action": "drop", "name": "m2_ed_db" })),
        )
        .await?;
        assert!(
            is_missing(&describe(s, &main, &db_ref).await),
            "{}",
            target.name
        );

        // Read-only sessions refuse.
        let ro = open(s, &target, true).await?;
        let refused = apply(
            s,
            &ro,
            pg(json!({ "object": "schema", "action": "create", "name": "m2_ed_ro" })),
        )
        .await;
        assert!(
            matches!(
                refused.as_ref().map_err(|e| e.as_ref()),
                Err(CmdError::ReadOnly { .. })
            ),
            "{}: {refused:?}",
            target.name
        );
        s.close_session(&ro.id).await?;
        s.close_session(&main.id).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PostgreSQL schemas, rules, policies, table drops
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn pg_schema_rule_policy_round_trip() -> Res {
    for target in pg_targets() {
        let fx = fixture(&format!("pgobj{}", target.port))?;
        let s = &fx.services;
        let main = open(s, &target, false).await?;
        run(
            s,
            &main,
            "DROP SCHEMA IF EXISTS m2_ed_s CASCADE; DROP SCHEMA IF EXISTS m2_ed_s2 CASCADE; \
             DROP SCHEMA IF EXISTS m2_ed_rp CASCADE; DROP VIEW IF EXISTS public.m2_ed_v",
        )
        .await?;

        // Schema: create, alter (owner, comment), rename.
        apply(
            s,
            &main,
            pg(
                json!({ "object": "schema", "action": "create", "name": "m2_ed_s",
                "owner": { "kind": "name", "name": "varve" }, "comment": "sales" }),
            ),
        )
        .await?;
        let schema = object(ObjectKind::Schema, Some("varve"), None, None, "m2_ed_s");
        let d = describe(s, &main, &schema).await?;
        assert_eq!(d["object"]["comment"], "sales");
        assert_eq!(d["object"]["owner"], "varve");
        apply(
            s,
            &main,
            pg(
                json!({ "object": "schema", "action": "alter", "name": "m2_ed_s",
                "operations": [{ "op": "owner", "owner": { "kind": "currentUser" } }] }),
            ),
        )
        .await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "comment", "target": schema, "comment": "sales data" })),
        )
        .await?;
        apply(
            s,
            &main,
            pg(
                json!({ "object": "schema", "action": "alter", "name": "m2_ed_s",
                "operations": [{ "op": "rename", "newName": "m2_ed_s2" }] }),
            ),
        )
        .await?;
        let schema2 = object(ObjectKind::Schema, Some("varve"), None, None, "m2_ed_s2");
        assert_eq!(
            describe(s, &main, &schema2).await?["object"]["comment"],
            "sales data"
        );

        // Drop impact of the schema: its contents and a view outside it.
        run(
            s,
            &main,
            "CREATE TABLE m2_ed_s2.t (id int); CREATE FUNCTION m2_ed_s2.f() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             CREATE VIEW public.m2_ed_v AS SELECT id FROM m2_ed_s2.t",
        )
        .await?;
        let impact = s.catalog_drop_impact(&main.id, &schema2).await?;
        assert_eq!(
            content(&impact, "tables").as_deref(),
            Some("1"),
            "{}: {impact:?}",
            target.name
        );
        assert_eq!(
            content(&impact, "functions").as_deref(),
            Some("1"),
            "{impact:?}"
        );
        assert!(
            has_dependent(&impact, "view public.m2_ed_v", DependentEffect::Blocks),
            "{}: {impact:?}",
            target.name
        );
        assert!(
            !impact
                .dependents
                .iter()
                .any(|d| d.description.contains("m2_ed_s2.t")),
            "{impact:?}"
        );
        let refused = apply(
            s,
            &main,
            pg(json!({ "object": "schema", "action": "drop", "name": "m2_ed_s2", "behavior": "restrict" })),
        )
        .await;
        assert!(driver_code(&refused).is_some(), "{refused:?}");
        apply(
            s,
            &main,
            pg(json!({ "object": "schema", "action": "drop", "name": "m2_ed_s2", "behavior": "cascade" })),
        )
        .await?;
        assert!(is_missing(&describe(s, &main, &schema2).await));
        let view = object(
            ObjectKind::View,
            Some("varve"),
            Some("public"),
            None,
            "m2_ed_v",
        );
        assert!(
            is_missing(&describe(s, &main, &view).await),
            "CASCADE dropped the view"
        );

        // Rules.
        run(
            s,
            &main,
            "CREATE SCHEMA m2_ed_rp; \
             CREATE TABLE m2_ed_rp.orders (id int PRIMARY KEY, amount int, owner text DEFAULT current_user); \
             CREATE TABLE m2_ed_rp.log (note text)",
        )
        .await?;
        let table = json!({ "schema": "m2_ed_rp", "name": "orders" });
        apply(
            s,
            &main,
            pg(json!({ "object": "rule", "action": "create", "name": "m2_ed_r", "table": table, "orReplace": false,
                "event": "insert", "condition": "new.amount < 0", "instead": true, "actions": [] })),
        )
        .await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "rule", "action": "setState", "name": "m2_ed_r", "table": table, "state": "disabled" })),
        )
        .await?;
        let rule = object(
            ObjectKind::Rule,
            Some("varve"),
            Some("m2_ed_rp"),
            Some("orders"),
            "m2_ed_r",
        );
        let d = describe(s, &main, &rule).await?;
        assert_eq!(d["object"]["event"], "insert");
        assert_eq!(d["object"]["instead"], true);
        assert_eq!(d["object"]["enabled"], "disabled");
        assert!(
            d["object"]["definition"]
                .as_str()
                .is_some_and(|x| x.contains("new.amount < 0")),
            "{d}"
        );
        apply(
            s,
            &main,
            pg(json!({ "object": "rule", "action": "create", "name": "m2_ed_r", "table": table, "orReplace": true,
                "event": "insert", "condition": "new.amount <= 0", "instead": false,
                "actions": ["INSERT INTO m2_ed_rp.log (note) VALUES ('neg')", "INSERT INTO m2_ed_rp.log (note) VALUES ('again')"] })),
        )
        .await?;
        apply(s, &main, pg(json!({ "object": "rule", "action": "setState", "name": "m2_ed_r", "table": table, "state": "enabled" }))).await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "comment", "target": rule, "comment": "negatives" })),
        )
        .await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "rule", "action": "rename", "name": "m2_ed_r", "table": table, "newName": "m2_ed_r2" })),
        )
        .await?;
        let rule2 = object(
            ObjectKind::Rule,
            Some("varve"),
            Some("m2_ed_rp"),
            Some("orders"),
            "m2_ed_r2",
        );
        let d = describe(s, &main, &rule2).await?;
        assert_eq!(d["object"]["instead"], false);
        assert_eq!(d["object"]["enabled"], "origin");
        assert_eq!(d["object"]["comment"], "negatives");
        let def = d["object"]["definition"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            def.contains("new.amount <= 0") && def.contains("again"),
            "{def}"
        );
        run(
            s,
            &main,
            "INSERT INTO m2_ed_rp.orders (id, amount) VALUES (1, -5)",
        )
        .await?;

        // Policies and row level security.
        apply(
            s,
            &main,
            pg(
                json!({ "object": "policy", "action": "create", "name": "m2_ed_p", "table": table,
                "restrictive": false, "command": "select", "roles": [],
                "using": "owner = current_user", "withCheck": null, "comment": "own rows" }),
            ),
        )
        .await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "table", "action": "alter", "name": table,
                "operations": [{ "op": "rowLevelSecurity", "enabled": true }, { "op": "forceRowLevelSecurity", "forced": true }] })),
        )
        .await?;
        let policy = object(
            ObjectKind::Policy,
            Some("varve"),
            Some("m2_ed_rp"),
            Some("orders"),
            "m2_ed_p",
        );
        let d = describe(s, &main, &policy).await?;
        assert_eq!(d["object"]["command"], "select");
        assert_eq!(d["object"]["permissive"], true);
        assert_eq!(d["object"]["comment"], "own rows");
        let orders = object(
            ObjectKind::Table,
            Some("varve"),
            Some("m2_ed_rp"),
            None,
            "orders",
        );
        let t = describe(s, &main, &orders).await?;
        assert_eq!(t["object"]["rlsEnabled"], true, "{}", target.name);
        assert_eq!(t["object"]["rlsForced"], true);
        apply(
            s,
            &main,
            pg(json!({ "object": "policy", "action": "alter", "name": "m2_ed_p", "table": table,
                "roles": [{ "kind": "name", "name": "varve" }], "using": "owner = session_user", "newName": "m2_ed_p2" })),
        )
        .await?;
        let policy2 = object(
            ObjectKind::Policy,
            Some("varve"),
            Some("m2_ed_rp"),
            Some("orders"),
            "m2_ed_p2",
        );
        let d = describe(s, &main, &policy2).await?;
        assert_eq!(d["object"]["roles"], json!(["varve"]));
        assert!(
            d["object"]["using"]
                .as_str()
                .is_some_and(|u| u.to_lowercase().contains("session_user")),
            "{d}"
        );
        // Permissiveness changes: one replace, one transaction (what the editor sends).
        apply(
            s,
            &main,
            pg(
                json!({ "object": "policy", "action": "replace", "name": "m2_ed_p2", "policy": {
                "name": "m2_ed_p2", "table": table, "restrictive": true, "command": "update",
                "roles": [{ "kind": "public" }], "using": "true", "withCheck": "amount >= 0" } }),
            ),
        )
        .await?;
        let d = describe(s, &main, &policy2).await?;
        assert_eq!(d["object"]["permissive"], false);
        assert_eq!(d["object"]["command"], "update");
        assert!(
            d["object"]["withCheck"]
                .as_str()
                .is_some_and(|c| c.contains("amount >= 0")),
            "{d}"
        );

        // Table drop impact: a view and a foreign key elsewhere, its rule and policy.
        run(
            s,
            &main,
            "CREATE VIEW m2_ed_rp.big AS SELECT id FROM m2_ed_rp.orders WHERE amount > 100; \
             CREATE TABLE m2_ed_rp.lines (order_id int REFERENCES m2_ed_rp.orders (id))",
        )
        .await?;
        let impact = s.catalog_drop_impact(&main.id, &orders).await?;
        assert!(
            has_dependent(&impact, "view m2_ed_rp.big", DependentEffect::Blocks),
            "{}: {impact:?}",
            target.name
        );
        assert!(
            impact
                .dependents
                .iter()
                .any(|d| d.description.contains("on table m2_ed_rp.lines")),
            "{impact:?}"
        );
        assert!(
            !impact
                .dependents
                .iter()
                .any(|d| d.description.contains("orders_pkey")),
            "{impact:?}"
        );
        assert_eq!(
            content(&impact, "rules").as_deref(),
            Some("1"),
            "{impact:?}"
        );
        assert_eq!(
            content(&impact, "policies").as_deref(),
            Some("1"),
            "{impact:?}"
        );
        // Rule and policy drops.
        apply(s, &main, pg(json!({ "object": "rule", "action": "drop", "name": "m2_ed_r2", "table": table, "behavior": "restrict" }))).await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "policy", "action": "drop", "name": "m2_ed_p2", "table": table })),
        )
        .await?;
        assert!(is_missing(&describe(s, &main, &rule2).await));
        assert!(is_missing(&describe(s, &main, &policy2).await));
        let refused = apply(
            s,
            &main,
            pg(json!({ "object": "table", "action": "drop", "name": table, "behavior": "restrict" })),
        )
        .await;
        assert!(driver_code(&refused).is_some(), "{refused:?}");
        apply(s, &main, pg(json!({ "object": "table", "action": "drop", "name": table, "behavior": "cascade" }))).await?;
        assert!(is_missing(&describe(s, &main, &orders).await));
        let big = object(
            ObjectKind::View,
            Some("varve"),
            Some("m2_ed_rp"),
            None,
            "big",
        );
        assert!(is_missing(&describe(s, &main, &big).await));
        let lines = object(
            ObjectKind::Table,
            Some("varve"),
            Some("m2_ed_rp"),
            None,
            "lines",
        );
        let l = describe(s, &main, &lines).await?;
        assert_eq!(
            l["object"]["name"], "lines",
            "CASCADE drops the foreign key, not the table"
        );

        run(s, &main, "DROP SCHEMA m2_ed_rp CASCADE").await?;
        s.close_session(&main.id).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MySQL and MariaDB
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn my_database_and_table_drop_round_trip() -> Res {
    for target in my_targets() {
        let fx = fixture(&format!("my{}", target.port))?;
        let s = &fx.services;
        let main = open(s, &target, false).await?;
        run(s, &main, "DROP DATABASE IF EXISTS m2_ed_x").await?;
        run(s, &main, "DROP DATABASE IF EXISTS m2_ed_db").await?;
        let mariadb = target.engine == Engine::Mariadb;
        let mysql_9 = target.engine == Engine::Mysql && main.server.major >= 9;
        let read_only_db = target.engine == Engine::Mysql
            && (
                main.server.major,
                main.server.minor,
                main.server.patch.unwrap_or(0),
            ) >= (8, 0, 22);

        let mut create = json!({ "change": "createDatabase", "name": "m2_ed_db", "charset": "utf8mb4", "collation": "utf8mb4_bin" });
        if mariadb {
            create["comment"] = json!("shop data");
        }
        if mysql_9 {
            create["encryption"] = json!(false);
        }
        apply(s, &main, my(create)).await?;
        let db = object(
            ObjectKind::Database,
            Some("m2_ed_db"),
            None,
            None,
            "m2_ed_db",
        );
        let d = describe(s, &main, &db).await?;
        assert_eq!(d["object"]["charset"], "utf8mb4", "{}", target.name);
        assert_eq!(d["object"]["collation"], "utf8mb4_bin", "{}", target.name);
        if mariadb {
            assert_eq!(d["object"]["comment"], "shop data");
        }

        let mut alter = json!({ "change": "alterDatabase", "name": "m2_ed_db", "collation": "utf8mb4_unicode_ci" });
        if mariadb {
            alter["comment"] = json!("shop");
        }
        if read_only_db {
            alter["readOnly"] = json!(true);
        }
        apply(s, &main, my(alter)).await?;
        let d = describe(s, &main, &db).await?;
        assert_eq!(
            d["object"]["collation"], "utf8mb4_unicode_ci",
            "{}",
            target.name
        );
        if mariadb {
            assert_eq!(d["object"]["comment"], "shop");
        }
        if read_only_db {
            assert_eq!(d["object"]["readOnly"], true, "{d}");
            // M2: a read-only database cannot be dropped; the impact says so.
            let impact = s.catalog_drop_impact(&main.id, &db).await?;
            assert!(
                impact.blockers.iter().any(|b| b.contains("read only")),
                "{}: {impact:?}",
                target.name
            );
            let refused = apply(
                s,
                &main,
                my(json!({ "change": "dropDatabase", "name": "m2_ed_db" })),
            )
            .await;
            assert!(
                driver_code(&refused).is_some_and(|c| c.contains("3989")),
                "{}: {refused:?}",
                target.name
            );
            apply(
                s,
                &main,
                my(json!({ "change": "alterDatabase", "name": "m2_ed_db", "readOnly": false })),
            )
            .await?;
            assert_eq!(describe(s, &main, &db).await?["object"]["readOnly"], false);
        }

        // Dependents of a table and of the database from another database.
        run(
            s,
            &main,
            "CREATE TABLE m2_ed_db.parent (id int PRIMARY KEY) ENGINE=InnoDB; \
             CREATE TABLE m2_ed_db.t (id int) ENGINE=InnoDB; \
             CREATE TRIGGER m2_ed_db.t_bi BEFORE INSERT ON m2_ed_db.t FOR EACH ROW SET NEW.id = NEW.id; \
             CREATE DATABASE m2_ed_x; \
             CREATE TABLE m2_ed_x.child (parent_id int, CONSTRAINT m2_ed_fk FOREIGN KEY (parent_id) REFERENCES m2_ed_db.parent (id)) ENGINE=InnoDB; \
             CREATE VIEW m2_ed_x.v AS SELECT id FROM m2_ed_db.parent; \
             CREATE PROCEDURE m2_ed_x.p() SELECT COUNT(*) FROM m2_ed_db.parent; \
             CREATE TABLE m2_ed_x.audit (id int) ENGINE=InnoDB; \
             CREATE TRIGGER m2_ed_x.audit_bi BEFORE INSERT ON m2_ed_x.audit FOR EACH ROW INSERT INTO m2_ed_db.t VALUES (NEW.id); \
             CREATE EVENT m2_ed_x.e ON SCHEDULE EVERY 1 DAY DISABLE DO DELETE FROM m2_ed_db.t",
        )
        .await?;
        let parent = object(ObjectKind::Table, Some("m2_ed_db"), None, None, "parent");
        let impact = s.catalog_drop_impact(&main.id, &parent).await?;
        assert!(
            has_dependent(
                &impact,
                "foreign key m2_ed_fk on table m2_ed_x.child",
                DependentEffect::Blocks
            ),
            "{}: {impact:?}",
            target.name
        );
        assert!(
            has_dependent(&impact, "view m2_ed_x.v", DependentEffect::Breaks),
            "{}: {impact:?}",
            target.name
        );
        // M1: routines, triggers and events that name it may break.
        assert!(
            has_dependent(&impact, "procedure m2_ed_x.p", DependentEffect::MayBreak),
            "{}: {impact:?}",
            target.name
        );
        let t = object(ObjectKind::Table, Some("m2_ed_db"), None, None, "t");
        let impact = s.catalog_drop_impact(&main.id, &t).await?;
        assert_eq!(
            content(&impact, "triggers").as_deref(),
            Some("1"),
            "{impact:?}"
        );
        assert!(
            has_dependent(
                &impact,
                "trigger m2_ed_x.audit_bi on table m2_ed_x.audit",
                DependentEffect::MayBreak
            ),
            "{}: {impact:?}",
            target.name
        );
        assert!(
            has_dependent(&impact, "event m2_ed_x.e", DependentEffect::MayBreak),
            "{impact:?}"
        );
        // Its own trigger goes with it and is not a dependent.
        assert!(
            !impact
                .dependents
                .iter()
                .any(|d| d.description.contains("m2_ed_db.t_bi")),
            "{impact:?}"
        );
        assert!(
            !impact
                .dependents
                .iter()
                .any(|d| d.description.contains("procedure")),
            "{impact:?}"
        );

        let other = s
            .open_database_session(
                &main.connection_id,
                "m2_ed_db",
                Some(secret("varve_dev")?),
                None,
                None,
            )
            .await?;
        let impact = s.catalog_drop_impact(&main.id, &db).await?;
        assert_eq!(
            content(&impact, "tables").as_deref(),
            Some("2"),
            "{}: {impact:?}",
            target.name
        );
        assert_eq!(
            content(&impact, "triggers").as_deref(),
            Some("1"),
            "{impact:?}"
        );
        assert!(
            has_dependent(&impact, "m2_ed_fk", DependentEffect::Blocks),
            "{impact:?}"
        );
        assert!(
            has_dependent(&impact, "view m2_ed_x.v", DependentEffect::Breaks),
            "{impact:?}"
        );
        assert!(!impact.sessions.is_empty(), "{}: {impact:?}", target.name);
        assert!(impact.sessions_complete, "root has PROCESS");
        assert!(
            !impact.notes.iter().any(|n| n.contains("Routines")),
            "root reads every routine: {impact:?}"
        );
        // MySQL logs binary by default: replicas would drop the database too.
        if target.engine == Engine::Mysql {
            assert!(
                impact
                    .notes
                    .iter()
                    .any(|n| n.contains("Binary logging is on")),
                "{}: {impact:?}",
                target.name
            );
            // With sql_log_bin off on the session that drops, nothing replicates.
            run(s, &main, "SET SESSION sql_log_bin = 0").await?;
            let quiet = s.catalog_drop_impact(&main.id, &db).await?;
            assert!(
                !quiet
                    .notes
                    .iter()
                    .any(|n| n.contains("Binary logging is on")),
                "{}: {quiet:?}",
                target.name
            );
            run(s, &main, "SET SESSION sql_log_bin = 1").await?;
        }
        // The server each session reached is recorded (MySQL server_uuid, MariaDB id, host and port).
        let id = s.sessions.info(&other.id)?.server_identity;
        let expected = if target.engine == Engine::Mariadb {
            "mariadb:"
        } else {
            "mysql:"
        };
        assert!(
            id.as_deref().is_some_and(|i| i.starts_with(expected)),
            "{}: {id:?}",
            target.name
        );
        for needle in [
            "procedure m2_ed_x.p",
            "trigger m2_ed_x.audit_bi",
            "event m2_ed_x.e",
        ] {
            assert!(
                has_dependent(&impact, needle, DependentEffect::MayBreak),
                "{}: {needle} {impact:?}",
                target.name
            );
        }
        assert!(
            impact.app_sessions.iter().any(|a| a.session_id == other.id),
            "{impact:?}"
        );

        // Drops as the UI plans them.
        apply(
            s,
            &main,
            my(json!({ "change": "dropTable", "database": "m2_ed_db", "name": "t" })),
        )
        .await?;
        assert!(is_missing(&describe(s, &main, &t).await), "{}", target.name);
        run(s, &main, "DROP DATABASE m2_ed_x").await?;
        apply(
            s,
            &main,
            my(json!({ "change": "dropDatabase", "name": "m2_ed_db" })),
        )
        .await?;
        assert!(
            is_missing(&describe(s, &main, &db).await),
            "{}",
            target.name
        );
        let _ = s.close_session(&other.id).await;
        s.close_session(&main.id).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Audit fixes
// ---------------------------------------------------------------------------

/// `"name"` with quotes doubled (PostgreSQL), for fixture SQL in tests.
fn pg_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `` `name` `` with backticks doubled (MySQL, MariaDB).
fn my_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// C1: a policy whose permissiveness changes is replaced in one
/// transaction; when the new definition fails, the old policy is still
/// there, unchanged.
#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn pg_policy_replace_is_atomic() -> Res {
    for target in pg_targets() {
        let fx = fixture(&format!("pgrepl{}", target.port))?;
        let s = &fx.services;
        let main = open(s, &target, false).await?;
        run(
            s,
            &main,
            "DROP SCHEMA IF EXISTS m2_ed_c1 CASCADE; CREATE SCHEMA m2_ed_c1; \
             CREATE TABLE m2_ed_c1.t (tenant int, owner text); \
             ALTER TABLE m2_ed_c1.t ENABLE ROW LEVEL SECURITY; \
             CREATE POLICY tenant ON m2_ed_c1.t AS RESTRICTIVE FOR ALL USING (tenant = 1)",
        )
        .await?;
        let table = json!({ "schema": "m2_ed_c1", "name": "t" });
        let policy = object(
            ObjectKind::Policy,
            Some("varve"),
            Some("m2_ed_c1"),
            Some("t"),
            "tenant",
        );
        let failing = apply(
            s,
            &main,
            pg(json!({ "object": "policy", "action": "replace", "name": "tenant", "policy": {
                "name": "tenant", "table": table, "restrictive": false, "command": "all", "roles": [],
                "using": "no_such_column = 1", "withCheck": null } })),
        )
        .await;
        assert!(
            driver_code(&failing).is_some(),
            "{}: {failing:?}",
            target.name
        );
        let d = describe(s, &main, &policy).await?;
        assert_eq!(
            d["object"]["permissive"], false,
            "{}: the restrictive policy survived",
            target.name
        );
        assert!(
            d["object"]["using"]
                .as_str()
                .is_some_and(|u| u.contains("tenant = 1")),
            "{d}"
        );
        // The same change with a valid definition replaces it.
        apply(
            s,
            &main,
            pg(json!({ "object": "policy", "action": "replace", "name": "tenant", "policy": {
                "name": "tenant2", "table": table, "restrictive": false, "command": "select", "roles": [],
                "using": "tenant = 2", "withCheck": null, "comment": "replaced" } })),
        )
        .await?;
        assert!(is_missing(&describe(s, &main, &policy).await));
        let policy2 = object(
            ObjectKind::Policy,
            Some("varve"),
            Some("m2_ed_c1"),
            Some("t"),
            "tenant2",
        );
        let d = describe(s, &main, &policy2).await?;
        assert_eq!(d["object"]["permissive"], true);
        assert_eq!(d["object"]["command"], "select");
        assert_eq!(d["object"]["comment"], "replaced");
        run(s, &main, "DROP SCHEMA m2_ed_c1 CASCADE").await?;
        s.close_session(&main.id).await?;
    }
    Ok(())
}

/// What PostgreSQL itself says a drop affects: the `RESTRICT` error lists
/// every object `CASCADE` would also drop ("X depends on Y"), named as
/// `pg_describe_object` names them under `search_path = pg_catalog`. The
/// statement runs inside a transaction that is rolled back.
async fn pg_restrict_dependents(
    s: &Services,
    session: &SessionInfo,
    drop: &str,
) -> Res<std::collections::BTreeSet<String>> {
    run(s, session, "SET search_path TO pg_catalog").await?;
    run(s, session, "BEGIN").await?;
    let result = s.execute(&session.id, drop, true, None).await;
    run(s, session, "ROLLBACK").await?;
    run(s, session, "RESET search_path").await?;
    let detail = match result {
        Err(CmdError::Session {
            error: SessionError::Driver { error },
        }) if error.code.as_deref() == Some("2BP01") => error.detail.unwrap_or_default(),
        other => return Err(format!("{drop} did not fail with 2BP01: {other:?}").into()),
    };
    Ok(detail
        .lines()
        .filter_map(|line| line.split(" depends on ").next())
        .map(str::to_owned)
        .collect())
}

/// True when the description names an object in `schema` (its first
/// qualified name is in the schema): such objects go with a schema drop.
fn in_schema(description: &str, schema: &str) -> bool {
    description
        .split_whitespace()
        .find(|word| word.contains('.'))
        .is_some_and(|word| word.starts_with(&format!("{schema}.")))
}

/// H1: the impact lists exactly what `DROP ... CASCADE` drops besides the
/// object (row type users, columns of that type, transitive views, foreign
/// keys, column defaults), for a table, a schema, a view, a type and a
/// function, on the oldest and newest PostgreSQL.
#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn pg_drop_impact_matches_cascade() -> Res {
    for target in pg_targets() {
        let fx = fixture(&format!("pgh1{}", target.port))?;
        let s = &fx.services;
        let main = open(s, &target, false).await?;
        run(
            s,
            &main,
            "DROP SCHEMA IF EXISTS m2_ed_h1o CASCADE; DROP SCHEMA IF EXISTS m2_ed_h1s CASCADE",
        )
        .await?;
        run(
            s,
            &main,
            "CREATE SCHEMA m2_ed_h1s; CREATE SCHEMA m2_ed_h1o; \
             CREATE TYPE m2_ed_h1s.mood AS ENUM ('a', 'b'); \
             CREATE TABLE m2_ed_h1s.t (id int PRIMARY KEY, m m2_ed_h1s.mood); \
             CREATE FUNCTION m2_ed_h1s.f(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1'; \
             CREATE FUNCTION m2_ed_h1o.takes_row(m2_ed_h1s.t) RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             CREATE FUNCTION m2_ed_h1o.returns_setof() RETURNS SETOF m2_ed_h1s.t LANGUAGE sql AS 'SELECT * FROM m2_ed_h1s.t'; \
             CREATE TABLE m2_ed_h1o.child (r m2_ed_h1s.t, m m2_ed_h1s.mood, n int); \
             CREATE INDEX child_m ON m2_ed_h1o.child (m); \
             CREATE VIEW m2_ed_h1o.v AS SELECT id FROM m2_ed_h1s.t; \
             CREATE VIEW m2_ed_h1o.vv AS SELECT id FROM m2_ed_h1o.v; \
             CREATE TABLE m2_ed_h1o.lines (tid int REFERENCES m2_ed_h1s.t (id)); \
             CREATE VIEW m2_ed_h1o.fv AS SELECT m2_ed_h1s.f(1) AS x; \
             CREATE TABLE m2_ed_h1o.uses_f (x int DEFAULT m2_ed_h1s.f(2)); \
             CREATE TYPE m2_ed_h1o.comp AS (x m2_ed_h1s.mood); \
             CREATE TABLE m2_ed_h1o.uses_comp (c m2_ed_h1o.comp); \
             CREATE FUNCTION m2_ed_h1o.uses_mood(m2_ed_h1o.comp) RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             CREATE DOMAIN m2_ed_h1s.pos AS int CHECK (VALUE > 0); \
             CREATE TYPE m2_ed_h1o.comp2 AS (p m2_ed_h1s.pos); \
             CREATE TABLE m2_ed_h1s.pt (id int) PARTITION BY RANGE (id); \
             CREATE TABLE m2_ed_h1s.pt1 PARTITION OF m2_ed_h1s.pt FOR VALUES FROM (0) TO (10); \
             CREATE TABLE m2_ed_h1o.pt2 PARTITION OF m2_ed_h1s.pt FOR VALUES FROM (10) TO (20)",
        )
        .await?;
        let cases = [
            (
                object(
                    ObjectKind::Table,
                    Some("varve"),
                    Some("m2_ed_h1s"),
                    None,
                    "t",
                ),
                "DROP TABLE m2_ed_h1s.t RESTRICT",
            ),
            (
                object(
                    ObjectKind::View,
                    Some("varve"),
                    Some("m2_ed_h1o"),
                    None,
                    "v",
                ),
                "DROP VIEW m2_ed_h1o.v RESTRICT",
            ),
            (
                object(
                    ObjectKind::EnumType,
                    Some("varve"),
                    Some("m2_ed_h1s"),
                    None,
                    "mood",
                ),
                "DROP TYPE m2_ed_h1s.mood RESTRICT",
            ),
            (
                ObjectRef {
                    signature: Some("integer".into()),
                    ..object(
                        ObjectKind::Function,
                        Some("varve"),
                        Some("m2_ed_h1s"),
                        None,
                        "f",
                    )
                },
                "DROP FUNCTION m2_ed_h1s.f(integer) RESTRICT",
            ),
            (
                object(
                    ObjectKind::Domain,
                    Some("varve"),
                    Some("m2_ed_h1s"),
                    None,
                    "pos",
                ),
                "DROP DOMAIN m2_ed_h1s.pos RESTRICT",
            ),
            (
                object(ObjectKind::Schema, Some("varve"), None, None, "m2_ed_h1s"),
                "DROP SCHEMA m2_ed_h1s RESTRICT",
            ),
        ];
        for (target_object, drop) in cases {
            let mut expected = pg_restrict_dependents(s, &main, drop).await?;
            if target_object.kind == ObjectKind::Schema {
                expected.retain(|d| !in_schema(d, "m2_ed_h1s"));
            }
            let impact = s.catalog_drop_impact(&main.id, &target_object).await?;
            let listed: std::collections::BTreeSet<String> = impact
                .dependents
                .iter()
                .map(|d| d.description.clone())
                .collect();
            assert!(!expected.is_empty(), "{drop}");
            assert_eq!(listed, expected, "{}: {drop}", target.name);
            assert!(!impact.dependents_truncated);
        }
        // The cases the audit found: functions on the row type and a column of that type.
        let impact = s
            .catalog_drop_impact(
                &main.id,
                &object(
                    ObjectKind::Table,
                    Some("varve"),
                    Some("m2_ed_h1s"),
                    None,
                    "t",
                ),
            )
            .await?;
        for needle in [
            "function m2_ed_h1o.takes_row(m2_ed_h1s.t)",
            "function m2_ed_h1o.returns_setof()",
            "column r of table m2_ed_h1o.child",
            "view m2_ed_h1o.vv",
        ] {
            assert!(
                has_dependent(&impact, needle, DependentEffect::Blocks),
                "{}: {needle} {impact:?}",
                target.name
            );
        }
        // A column of a composite type is that column, not the whole type
        // (and not the tables and functions that use the type).
        let impact = s
            .catalog_drop_impact(
                &main.id,
                &object(
                    ObjectKind::EnumType,
                    Some("varve"),
                    Some("m2_ed_h1s"),
                    None,
                    "mood",
                ),
            )
            .await?;
        assert!(
            has_dependent(
                &impact,
                "column x of composite type m2_ed_h1o.comp",
                DependentEffect::Blocks
            ),
            "{}: {impact:?}",
            target.name
        );
        for wrong in [
            "type m2_ed_h1o.comp",
            "column c of table m2_ed_h1o.uses_comp",
            "function m2_ed_h1o.uses_mood",
        ] {
            assert!(
                !impact
                    .dependents
                    .iter()
                    .any(|d| d.description.starts_with(wrong)),
                "{wrong}: {impact:?}"
            );
        }
        let impact = s
            .catalog_drop_impact(
                &main.id,
                &object(
                    ObjectKind::Domain,
                    Some("varve"),
                    Some("m2_ed_h1s"),
                    None,
                    "pos",
                ),
            )
            .await?;
        assert!(
            has_dependent(
                &impact,
                "column p of composite type m2_ed_h1o.comp2",
                DependentEffect::Blocks
            ),
            "{impact:?}"
        );
        // Partitions (any schema) go with a partitioned table.
        let impact = s
            .catalog_drop_impact(
                &main.id,
                &object(
                    ObjectKind::PartitionedTable,
                    Some("varve"),
                    Some("m2_ed_h1s"),
                    None,
                    "pt",
                ),
            )
            .await?;
        assert_eq!(
            content(&impact, "partitions").as_deref(),
            Some("2"),
            "{}: {impact:?}",
            target.name
        );
        let impact = s
            .catalog_drop_impact(
                &main.id,
                &object(ObjectKind::Schema, Some("varve"), None, None, "m2_ed_h1s"),
            )
            .await?;
        assert!(
            !impact
                .dependents
                .iter()
                .any(|d| d.description == "table m2_ed_h1o.child"),
            "{impact:?}"
        );
        assert!(
            has_dependent(
                &impact,
                "column m of table m2_ed_h1o.child",
                DependentEffect::Blocks
            ),
            "{impact:?}"
        );
        run(
            s,
            &main,
            "DROP SCHEMA m2_ed_h1o CASCADE; DROP SCHEMA m2_ed_h1s CASCADE",
        )
        .await?;
        s.close_session(&main.id).await?;
    }
    Ok(())
}

/// L5: `PROCESS` granted through a role counts; without it the list is
/// reported as possibly incomplete.
#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn my_process_through_roles() -> Res {
    for target in my_targets() {
        let fx = fixture(&format!("myrole{}", target.port))?;
        let s = &fx.services;
        let root = open(s, &target, false).await?;
        let mariadb = target.engine == Engine::Mariadb;
        for stmt in [
            "DROP USER IF EXISTS 'm2_ed_u'@'%'",
            "DROP USER IF EXISTS 'm2_ed_u2'@'%'",
            "DROP ROLE IF EXISTS m2_ed_role",
            "DROP DATABASE IF EXISTS m2_ed_r",
        ] {
            run(s, &root, stmt).await?;
        }
        run(s, &root, "CREATE DATABASE m2_ed_r").await?;
        run(s, &root, "CREATE PROCEDURE m2_ed_r.p() SELECT 1").await?;
        run(s, &root, "CREATE ROLE m2_ed_role").await?;
        run(s, &root, "GRANT PROCESS ON *.* TO m2_ed_role").await?;
        for user in ["m2_ed_u", "m2_ed_u2"] {
            run(
                s,
                &root,
                &format!("CREATE USER '{user}'@'%' IDENTIFIED BY 'varve_dev'"),
            )
            .await?;
            run(
                s,
                &root,
                &format!("GRANT SELECT ON m2_ed_r.* TO '{user}'@'%'"),
            )
            .await?;
        }
        run(s, &root, "GRANT m2_ed_role TO 'm2_ed_u'@'%'").await?;
        run(
            s,
            &root,
            if mariadb {
                "SET DEFAULT ROLE m2_ed_role FOR 'm2_ed_u'@'%'"
            } else {
                "SET DEFAULT ROLE m2_ed_role TO 'm2_ed_u'@'%'"
            },
        )
        .await?;
        let db = object(ObjectKind::Database, Some("m2_ed_r"), None, None, "m2_ed_r");
        for (user, complete) in [("m2_ed_u", true), ("m2_ed_u2", false)] {
            let mut connection = input(&target, false);
            connection.username = Some(user.into());
            connection.name = format!("{} {user}", target.name);
            let profile = s.create_connection(&connection)?;
            let session = s
                .open_session(&profile.id, Some(secret("varve_dev")?), None, None)
                .await?;
            let impact = s.catalog_drop_impact(&session.id, &db).await?;
            assert_eq!(
                impact.sessions_complete, complete,
                "{} {user}: {impact:?}",
                target.name
            );
            // L3: neither account may read every routine body, and the impact says so.
            assert!(
                impact
                    .notes
                    .iter()
                    .any(|n| n.contains("Routines were searched only")),
                "{} {user}: {impact:?}",
                target.name
            );
            s.close_session(&session.id).await?;
        }
        for stmt in [
            "DROP USER 'm2_ed_u'@'%'",
            "DROP USER 'm2_ed_u2'@'%'",
            "DROP ROLE m2_ed_role",
            "DROP DATABASE m2_ed_r",
        ] {
            run(s, &root, stmt).await?;
        }
        s.close_session(&root.id).await?;
    }
    Ok(())
}

/// L7: names with quotes, spaces, unicode and reserved words survive
/// create, alter, describe and drop.
#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn pg_names_with_quotes_round_trip() -> Res {
    let db = "m2_ed \"Ünï\" 'db'";
    let db2 = "m2_ed select ö";
    let schema = "m2_ed \"from\" 's'";
    let rule = "m2_ed 'r' ü";
    let policy = "m2_ed \"p\" where";
    for target in pg_targets() {
        let fx = fixture(&format!("pgnames{}", target.port))?;
        let s = &fx.services;
        let main = open(s, &target, false).await?;
        for name in [db, db2] {
            run(
                s,
                &main,
                &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", pg_ident(name)),
            )
            .await?;
        }
        run(
            s,
            &main,
            &format!("DROP SCHEMA IF EXISTS {} CASCADE", pg_ident(schema)),
        )
        .await?;

        apply(s, &main, pg(json!({ "object": "database", "action": "create", "name": db, "comment": "it's \"quoted\"" }))).await?;
        let dbref = object(ObjectKind::Database, Some(db), None, None, db);
        assert_eq!(
            describe(s, &main, &dbref).await?["object"]["comment"],
            "it's \"quoted\"",
            "{}",
            target.name
        );
        apply(
            s,
            &main,
            pg(
                json!({ "object": "database", "action": "alter", "name": db, "operations": [
                { "op": "set", "setting": { "name": "application_name", "values": ["it's ü"] } },
                { "op": "rename", "newName": db2 }
            ] }),
            ),
        )
        .await?;
        let d = describe(
            s,
            &main,
            &object(ObjectKind::Database, Some(db2), None, None, db2),
        )
        .await?;
        assert_eq!(d["object"]["name"], db2);
        apply(
            s,
            &main,
            pg(json!({ "object": "database", "action": "drop", "name": db2 })),
        )
        .await?;

        apply(
            s,
            &main,
            pg(json!({ "object": "schema", "action": "create", "name": schema, "comment": "ü" })),
        )
        .await?;
        run(
            s,
            &main,
            &format!(
                "CREATE TABLE {}.{} (\"order\" int)",
                pg_ident(schema),
                pg_ident("select")
            ),
        )
        .await?;
        let table = json!({ "schema": schema, "name": "select" });
        apply(
            s,
            &main,
            pg(json!({ "object": "rule", "action": "create", "name": rule, "table": table, "orReplace": false,
                "event": "delete", "instead": true, "actions": [] })),
        )
        .await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "policy", "action": "create", "name": policy, "table": table,
                "restrictive": false, "command": "select", "roles": [], "using": "\"order\" > 0", "withCheck": null })),
        )
        .await?;
        let r = describe(
            s,
            &main,
            &object(
                ObjectKind::Rule,
                Some("varve"),
                Some(schema),
                Some("select"),
                rule,
            ),
        )
        .await?;
        assert_eq!(r["object"]["name"], rule, "{}", target.name);
        let p = describe(
            s,
            &main,
            &object(
                ObjectKind::Policy,
                Some("varve"),
                Some(schema),
                Some("select"),
                policy,
            ),
        )
        .await?;
        assert_eq!(p["object"]["name"], policy);
        apply(
            s,
            &main,
            pg(json!({ "object": "rule", "action": "drop", "name": rule, "table": table })),
        )
        .await?;
        apply(
            s,
            &main,
            pg(json!({ "object": "policy", "action": "drop", "name": policy, "table": table })),
        )
        .await?;
        let sref = object(ObjectKind::Schema, Some("varve"), None, None, schema);
        let impact = s.catalog_drop_impact(&main.id, &sref).await?;
        assert_eq!(
            content(&impact, "tables").as_deref(),
            Some("1"),
            "{impact:?}"
        );
        apply(s, &main, pg(json!({ "object": "schema", "action": "drop", "name": schema, "behavior": "cascade" }))).await?;
        assert!(is_missing(&describe(s, &main, &sref).await));
        s.close_session(&main.id).await?;
    }
    Ok(())
}

/// L7 for MySQL and MariaDB databases.
#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn my_names_with_quotes_round_trip() -> Res {
    let db = "m2_ed ü 'q' `b` select";
    for target in my_targets() {
        let fx = fixture(&format!("mynames{}", target.port))?;
        let s = &fx.services;
        let main = open(s, &target, false).await?;
        run(
            s,
            &main,
            &format!("DROP DATABASE IF EXISTS {}", my_ident(db)),
        )
        .await?;
        apply(
            s,
            &main,
            my(json!({ "change": "createDatabase", "name": db, "charset": "utf8mb4" })),
        )
        .await?;
        let dbref = object(ObjectKind::Database, Some(db), None, None, db);
        assert_eq!(
            describe(s, &main, &dbref).await?["object"]["name"],
            db,
            "{}",
            target.name
        );
        run(
            s,
            &main,
            &format!(
                "CREATE TABLE {}.{} (id int)",
                my_ident(db),
                my_ident("from")
            ),
        )
        .await?;
        let table = object(ObjectKind::Table, Some(db), None, None, "from");
        let impact = s.catalog_drop_impact(&main.id, &dbref).await?;
        assert_eq!(
            content(&impact, "tables").as_deref(),
            Some("1"),
            "{}: {impact:?}",
            target.name
        );
        apply(
            s,
            &main,
            my(json!({ "change": "dropTable", "database": db, "name": "from" })),
        )
        .await?;
        assert!(is_missing(&describe(s, &main, &table).await));
        apply(
            s,
            &main,
            my(json!({ "change": "dropDatabase", "name": db })),
        )
        .await?;
        assert!(
            is_missing(&describe(s, &main, &dbref).await),
            "{}",
            target.name
        );
        s.close_session(&main.id).await?;
    }
    Ok(())
}

/// Re-audit M2: a schema's own objects are never listed as outside
/// dependents, also when its name needs quoting and when it holds hundreds
/// of parts without a schema of their own (serial defaults), which would
/// otherwise fill the list before the real dependents.
#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn pg_schema_impact_filters_by_namespace() -> Res {
    let quoted = "m2_ed \"Q\" s";
    for target in pg_targets() {
        let fx = fixture(&format!("pgnsp{}", target.port))?;
        let s = &fx.services;
        let main = open(s, &target, false).await?;
        let q = pg_ident(quoted);
        run(
            s,
            &main,
            &format!(
                "DROP SCHEMA IF EXISTS m2_ed_qo CASCADE; DROP SCHEMA IF EXISTS {q} CASCADE; \
                 DROP SCHEMA IF EXISTS m2_ed_big CASCADE; \
                 CREATE SCHEMA {q}; CREATE SCHEMA m2_ed_qo; CREATE SCHEMA m2_ed_big; \
                 CREATE TABLE {q}.t (id serial PRIMARY KEY, n int CHECK (n > 0)); \
                 CREATE FUNCTION {q}.f(int) RETURNS int LANGUAGE sql AS 'SELECT $1'; \
                 CREATE TRIGGER tr BEFORE UPDATE ON {q}.t FOR EACH ROW \
                   EXECUTE FUNCTION pg_catalog.suppress_redundant_updates_trigger(); \
                 CREATE VIEW m2_ed_qo.v AS SELECT id FROM {q}.t"
            ),
        )
        .await?;
        run(
            s,
            &main,
            "DO $$ BEGIN FOR i IN 1..300 LOOP \
               EXECUTE pg_catalog.format('CREATE TABLE m2_ed_big.t%s (id serial PRIMARY KEY)', i); \
             END LOOP; END $$",
        )
        .await?;
        run(
            s,
            &main,
            "CREATE VIEW m2_ed_qo.zz AS SELECT id FROM m2_ed_big.t300",
        )
        .await?;

        let impact = s
            .catalog_drop_impact(
                &main.id,
                &object(ObjectKind::Schema, Some("varve"), None, None, quoted),
            )
            .await?;
        let listed: Vec<&str> = impact
            .dependents
            .iter()
            .map(|d| d.description.as_str())
            .collect();
        assert_eq!(
            listed,
            vec!["view m2_ed_qo.v"],
            "{}: {impact:?}",
            target.name
        );
        let impact = s
            .catalog_drop_impact(
                &main.id,
                &object(ObjectKind::Schema, Some("varve"), None, None, "m2_ed_big"),
            )
            .await?;
        let listed: Vec<&str> = impact
            .dependents
            .iter()
            .map(|d| d.description.as_str())
            .collect();
        assert_eq!(
            listed,
            vec!["view m2_ed_qo.zz"],
            "{}: {impact:?}",
            target.name
        );
        assert!(!impact.dependents_truncated);
        assert_eq!(content(&impact, "tables").as_deref(), Some("300"));

        run(
            s,
            &main,
            &format!("DROP SCHEMA m2_ed_qo CASCADE; DROP SCHEMA {q} CASCADE; DROP SCHEMA m2_ed_big CASCADE"),
        )
        .await?;
        s.close_session(&main.id).await?;
    }
    Ok(())
}

/// Re-audit round 3: tabs are matched on the server each session reached,
/// not on what the profiles say now. A connection edited to the address of
/// another server keeps its tab on the old server out of the list; a
/// connection that reaches the same server through another address (an
/// alias, as a tunnel would) is in it, with certainty.
#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn pg_tabs_match_by_server_identity() -> Res {
    let [pg13, pg18] = pg_targets();
    let fx = fixture("pgsrvid")?;
    let s = &fx.services;
    let main = open(s, &pg18, false).await?;
    let old = open(s, &pg13, false).await?;
    for session in [&main, &old] {
        run(s, session, "DROP DATABASE IF EXISTS m2_ed_srv WITH (FORCE)").await?;
        run(s, session, "CREATE DATABASE m2_ed_srv").await?;
    }
    // Profile B reaches PG 13 and opens a tab on m2_ed_srv there.
    let b_profile = s.create_connection(&ConnectionInput {
        name: "m2_ed B".into(),
        ..input(&pg13, false)
    })?;
    let b_tab = s
        .open_database_session(
            &b_profile.id,
            "m2_ed_srv",
            Some(secret("varve_dev")?),
            None,
            None,
        )
        .await?;
    // Profile C reaches PG 18 through another address (`localhost`).
    let c_profile = s.create_connection(&ConnectionInput {
        name: "m2_ed C".into(),
        host: "localhost".into(),
        ..input(&pg18, false)
    })?;
    let c_tab = s
        .open_database_session(
            &c_profile.id,
            "m2_ed_srv",
            Some(secret("varve_dev")?),
            None,
            None,
        )
        .await?;
    // B is edited to point at PG 18 (A's address); its open tab stays on PG 13.
    let a_profile = s.records.connection(&main.connection_id)?;
    s.update_connection(
        &b_profile.id,
        &ConnectionInput {
            name: "m2_ed B".into(),
            host: a_profile.host.clone(),
            port: Some(a_profile.port),
            ..input(&pg18, false)
        },
    )
    .await?;

    for tab in [&b_tab, &c_tab, &main] {
        let id = s.sessions.info(&tab.id)?.server_identity;
        assert!(
            id.as_deref().is_some_and(|i| i.starts_with("pg:")),
            "{id:?}"
        );
    }
    let db = object(
        ObjectKind::Database,
        Some("m2_ed_srv"),
        None,
        None,
        "m2_ed_srv",
    );
    let impact = s.catalog_drop_impact(&main.id, &db).await?;
    let ids: Vec<(&str, bool)> = impact
        .app_sessions
        .iter()
        .map(|a| (a.session_id.as_str(), a.certain))
        .collect();
    assert!(ids.contains(&(c_tab.id.as_str(), true)), "{impact:?}");
    assert!(
        !ids.iter().any(|(id, _)| *id == b_tab.id),
        "the edited connection's tab is on PG 13: {impact:?}"
    );

    let _ = s.close_session(&b_tab.id).await;
    let _ = s.close_session(&c_tab.id).await;
    for session in [&main, &old] {
        run(s, session, "DROP DATABASE m2_ed_srv WITH (FORCE)").await?;
    }
    s.close_session(&old.id).await?;
    s.close_session(&main.id).await?;
    Ok(())
}

/// Re-audit round 4: a streaming standby shares the primary's system
/// identifier. Its tabs are told apart by role: listed as standby tabs
/// (ended when the drop replays there), and a plain drop on the primary
/// succeeds while they are connected.
#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn pg_standby_tabs_are_not_blockers() -> Res {
    let primary = Target {
        name: "PG 18 primary",
        engine: Engine::Postgres,
        port: port("VARVE_M2_PG_PRIMARY_PORT", 55611),
    };
    let standby = Target {
        name: "PG 18 standby",
        engine: Engine::Postgres,
        port: port("VARVE_M2_PG_REPLICA_PORT", 55612),
    };
    let fx = fixture("pgstandby")?;
    let s = &fx.services;
    let main = open(s, &primary, false).await?;
    run(s, &main, "DROP DATABASE IF EXISTS m2_ed_rep WITH (FORCE)").await?;
    run(s, &main, "CREATE DATABASE m2_ed_rep").await?;
    let replica = open(s, &standby, true).await?;
    // Wait for the database to reach the standby.
    let mut tries = 0;
    let replica_tab = loop {
        match s
            .open_database_session(
                &replica.connection_id,
                "m2_ed_rep",
                Some(secret("varve_dev")?),
                None,
                None,
            )
            .await
        {
            Ok(tab) => break tab,
            Err(error) if tries < 50 => {
                tries += 1;
                let _ = error;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let main_id = s
        .sessions
        .info(&main.id)?
        .server_identity
        .unwrap_or_default();
    let replica_id = s
        .sessions
        .info(&replica_tab.id)?
        .server_identity
        .unwrap_or_default();
    assert!(
        main_id.starts_with("pg:") && main_id.ends_with(":primary"),
        "{main_id}"
    );
    assert!(replica_id.ends_with(":standby"), "{replica_id}");
    assert_eq!(
        main_id.rsplit_once(':').map(|(head, _)| head),
        replica_id.rsplit_once(':').map(|(head, _)| head),
        "same system identifier and timeline"
    );

    let db = object(
        ObjectKind::Database,
        Some("m2_ed_rep"),
        None,
        None,
        "m2_ed_rep",
    );
    let impact = s.catalog_drop_impact(&main.id, &db).await?;
    let tab = impact
        .app_sessions
        .iter()
        .find(|a| a.session_id == replica_tab.id);
    assert!(tab.is_some_and(|a| a.standby && a.certain), "{impact:?}");
    // A plain drop (no FORCE) on the primary succeeds with the standby tab connected.
    apply(
        s,
        &main,
        pg(json!({ "object": "database", "action": "drop", "name": "m2_ed_rep" })),
    )
    .await?;
    assert!(is_missing(&describe(s, &main, &db).await));

    let _ = s.close_session(&replica_tab.id).await;
    s.close_session(&replica.id).await?;
    s.close_session(&main.id).await?;
    Ok(())
}
