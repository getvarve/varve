//! Real `catalog_describe` output for the UI contract, against the shared
//! M2 database matrix (PostgreSQL 18, MySQL 9.7, MariaDB 12.3):
//!
//! ```sh
//! cargo test -p varve-catalog --test ui_contract_samples -- --include-ignored
//! VARVE_UPDATE_CONTRACT=1 cargo test -p varve-catalog --test ui_contract_samples -- --include-ignored
//! ```
//!
//! Each object kind the object editors show is created, described, and
//! compared with `packages/ui/src/api/__contract__/samples/describe.<kind>.<server>.json`
//! (written with `VARVE_UPDATE_CONTRACT=1`). `app/src-tauri/tests/ui_contract.rs`
//! types those samples as the UI's `ObjectDetails` with excess-property
//! checks, so a field the UI lacks or misnames fails `vue-tsc`. Objects are
//! named `m2_uisamples*` and dropped at the start and the end of a run.

use std::error::Error;
use std::path::PathBuf;

use serde_json::Value as Json;
use varve_catalog::model::{ObjectKind, ObjectRef};
use varve_catalog::open_catalog;
use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, ExecOptions, Session};
use varve_driver_mysql::MysqlDriver;
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const NAME: &str = "m2_uisamples";

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

fn samples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/ui/src/api/__contract__/samples")
}

async fn exec(session: &dyn Session, sql: &str) -> TestResult {
    session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}\n=> {e}"))?;
    Ok(())
}

/// Values that change from run to run.
fn normalize(value: &mut Json) {
    match value {
        Json::Object(map) => {
            for key in [
                "sizeBytes",
                "totalBytes",
                "tableBytes",
                "indexBytes",
                "rowEstimate",
                "sizeNote",
                "created",
                "updated",
                "checked",
                "lastAltered",
                "dataLength",
                "indexLength",
                "dataFree",
                "avgRowLength",
                "executeAt",
                "starts",
                "lastExecuted",
                "altered",
                "oid",
            ] {
                if map.contains_key(key) {
                    map.insert(key.to_owned(), Json::Null);
                }
            }
            map.values_mut().for_each(normalize);
        }
        Json::Array(items) => items.iter_mut().for_each(normalize),
        _ => {}
    }
}

fn object(
    kind: ObjectKind,
    database: &str,
    schema: Option<&str>,
    parent: Option<&str>,
    name: &str,
) -> ObjectRef {
    ObjectRef {
        kind,
        database: Some(database.into()),
        schema: schema.map(Into::into),
        parent: parent.map(Into::into),
        name: name.into(),
        signature: None,
    }
}

async fn check(session: &dyn Session, server: &str, objects: Vec<(&str, ObjectRef)>) -> TestResult {
    let catalog = open_catalog(session).await?;
    let update = std::env::var("VARVE_UPDATE_CONTRACT").is_ok_and(|v| v == "1");
    for (slug, object) in objects {
        let mut described = serde_json::to_value(
            catalog
                .describe(session, &object)
                .await
                .map_err(|e| format!("describe {slug}: {e}"))?,
        )?;
        normalize(&mut described);
        let file = samples().join(format!("describe.{slug}.{server}.json"));
        if update {
            std::fs::write(&file, serde_json::to_string_pretty(&described)? + "\n")?;
        }
        let sample: Json = serde_json::from_str(&std::fs::read_to_string(&file).map_err(|e| {
            format!(
                "{}: {e} (run with VARVE_UPDATE_CONTRACT=1 to write it)",
                file.display()
            )
        })?)?;
        assert_eq!(described, sample, "{} differs", file.display());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn postgres_18_describe_samples() -> TestResult {
    let mut params = ConnectParams::new(
        Engine::Postgres,
        env_or("VARVE_M2_PG_HOST", "127.0.0.1"),
        port("VARVE_M2_PG18_PORT", 55518),
    );
    params.database = Some("varve".to_owned());
    params.username = Some(env_or("VARVE_M2_PG_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_M2_PG_PASSWORD", "varve_dev")));
    let session = PostgresDriver::new().connect(&params).await?;
    let s = session.as_ref();
    exec(s, &format!("DROP SCHEMA IF EXISTS {NAME} CASCADE")).await?;
    exec(
        s,
        &format!(
            "CREATE SCHEMA {NAME};
             CREATE TABLE {NAME}.t (id int PRIMARY KEY, label text);
             CREATE VIEW {NAME}.v AS SELECT id, label FROM {NAME}.t WHERE id > 0;
             CREATE MATERIALIZED VIEW {NAME}.mv AS SELECT id FROM {NAME}.t;
             CREATE FUNCTION {NAME}.f(a int, b text DEFAULT 'x') RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT a';
             CREATE PROCEDURE {NAME}.p(a int) LANGUAGE sql AS 'SELECT 1';
             CREATE FUNCTION {NAME}.trg() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
             CREATE TRIGGER tr BEFORE INSERT ON {NAME}.t FOR EACH ROW EXECUTE FUNCTION {NAME}.trg();
             CREATE TYPE {NAME}.mood AS ENUM ('sad', 'ok');
             CREATE TYPE {NAME}.pair AS (a int, b text);
             CREATE TYPE {NAME}.floatrange AS RANGE (subtype = float8);
             CREATE DOMAIN {NAME}.pos AS int CHECK (VALUE > 0);
             CREATE RULE r AS ON DELETE TO {NAME}.t DO INSTEAD NOTHING;
             ALTER TABLE {NAME}.t ENABLE ROW LEVEL SECURITY;
             CREATE POLICY pol ON {NAME}.t USING (id > 0);"
        ),
    )
    .await?;
    let db = "varve";
    let mut f = object(ObjectKind::Function, db, Some(NAME), None, "f");
    f.signature = Some("a integer, b text".into());
    let mut p = object(ObjectKind::Procedure, db, Some(NAME), None, "p");
    p.signature = Some("IN a integer".into());
    let result = check(
        s,
        "pg18",
        vec![
            ("view", object(ObjectKind::View, db, Some(NAME), None, "v")),
            (
                "matview",
                object(ObjectKind::MaterializedView, db, Some(NAME), None, "mv"),
            ),
            ("function", f),
            ("procedure", p),
            (
                "trigger",
                object(ObjectKind::Trigger, db, Some(NAME), Some("t"), "tr"),
            ),
            (
                "enum",
                object(ObjectKind::EnumType, db, Some(NAME), None, "mood"),
            ),
            (
                "composite",
                object(ObjectKind::CompositeType, db, Some(NAME), None, "pair"),
            ),
            (
                "range",
                object(ObjectKind::RangeType, db, Some(NAME), None, "floatrange"),
            ),
            (
                "domain",
                object(ObjectKind::Domain, db, Some(NAME), None, "pos"),
            ),
            (
                "rule",
                object(ObjectKind::Rule, db, Some(NAME), Some("t"), "r"),
            ),
            (
                "policy",
                object(ObjectKind::Policy, db, Some(NAME), Some("t"), "pol"),
            ),
            (
                "schema",
                ObjectRef {
                    kind: ObjectKind::Schema,
                    database: Some(db.into()),
                    schema: None,
                    parent: None,
                    name: NAME.into(),
                    signature: None,
                },
            ),
        ],
    )
    .await;
    let cleaned = exec(s, &format!("DROP SCHEMA IF EXISTS {NAME} CASCADE")).await;
    result?;
    cleaned
}

async fn mysql_samples(engine: Engine, port: u16, server: &str) -> TestResult {
    let mut params = ConnectParams::new(engine, env_or("VARVE_M2_MYSQL_HOST", "127.0.0.1"), port);
    params.username = Some(env_or("VARVE_M2_MYSQL_USER", "root"));
    params.password = Some(Secret::new(env_or("VARVE_M2_MYSQL_PASSWORD", "varve_dev")));
    params.tls.mode = varve_driver::TlsMode::Disable;
    let session = MysqlDriver::new().connect(&params).await?;
    let s = session.as_ref();
    exec(s, &format!("DROP DATABASE IF EXISTS {NAME}")).await?;
    for sql in [
        format!("CREATE DATABASE {NAME}"),
        format!("CREATE TABLE {NAME}.t (id int PRIMARY KEY, label varchar(20))"),
        format!("CREATE VIEW {NAME}.v AS SELECT id, label FROM {NAME}.t WHERE id > 0"),
        format!("CREATE PROCEDURE {NAME}.p(IN a int) BEGIN SELECT a; END"),
        format!("CREATE FUNCTION {NAME}.f(a int) RETURNS int DETERMINISTIC RETURN a + 1"),
        format!(
            "CREATE TRIGGER {NAME}.tr BEFORE INSERT ON {NAME}.t FOR EACH ROW SET NEW.label = 'x'"
        ),
        format!(
            "CREATE EVENT {NAME}.ev ON SCHEDULE EVERY 1 DAY STARTS '2030-01-01 00:00:00' DISABLE DO DELETE FROM {NAME}.t"
        ),
    ] {
        exec(s, &sql).await?;
    }
    let result = check(
        s,
        server,
        vec![
            ("view", object(ObjectKind::View, NAME, None, None, "v")),
            (
                "procedure",
                object(ObjectKind::Procedure, NAME, None, None, "p"),
            ),
            (
                "function",
                object(ObjectKind::Function, NAME, None, None, "f"),
            ),
            (
                "trigger",
                object(ObjectKind::Trigger, NAME, None, Some("t"), "tr"),
            ),
            ("event", object(ObjectKind::Event, NAME, None, None, "ev")),
            (
                "database",
                ObjectRef {
                    kind: ObjectKind::Database,
                    database: None,
                    schema: None,
                    parent: None,
                    name: NAME.into(),
                    signature: None,
                },
            ),
        ],
    )
    .await;
    let cleaned = exec(s, &format!("DROP DATABASE IF EXISTS {NAME}")).await;
    result?;
    cleaned
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mysql_9_7_describe_samples() -> TestResult {
    mysql_samples(
        Engine::Mysql,
        port("VARVE_M2_MYSQL_9_7_PORT", 35097),
        "my97",
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn mariadb_12_3_describe_samples() -> TestResult {
    mysql_samples(
        Engine::Mariadb,
        port("VARVE_M2_MARIADB_12_3_PORT", 35123),
        "ma123",
    )
    .await
}
