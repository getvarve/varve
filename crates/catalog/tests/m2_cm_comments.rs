//! Comments on every object kind the browser lists (design doc 4.2,
//! "Comments E on every object") and the extension install picker, against
//! the M2 matrix:
//!
//! ```sh
//! cargo test -p varve-catalog --test m2_cm_comments -- --include-ignored
//! ```
//!
//! PostgreSQL: every object is found the way the UI finds it (listed in its
//! container), its comment is planned with the generic comment change
//! (`PgChange::Comment`) on exactly that reference, run, read back from
//! `catalog_describe` (at `comment` or `meta.comment`, where the comment
//! editor reads it), then removed again. The extension test reads
//! `available_extensions` and installs a chosen version and a dependent
//! extension with `CASCADE`.
//!
//! MySQL and MariaDB: routine and event comments (the only object comments
//! these engines have outside tables, columns, indexes and partitions) are
//! changed through the editors' changes and read back from
//! `information_schema` and the catalog. `alterRoutine` changes a routine's
//! comment, data access and SQL security in place and keeps the
//! `EXECUTE` / `ALTER ROUTINE` grants on it.
//!
//! Every test works in its own `m2_cm_*` database, dropped before it starts
//! and again when it ends.

use std::time::Duration;

use serde_json::{Value as Json, json};
use varve_catalog::mysql::edit::MyChange;
use varve_catalog::postgres::edit::{CommentChange, CreateExtension, ExtensionChange, PgChange};
use varve_catalog::postgres::programs::available_extensions;
use varve_catalog::{Catalog, Container, EditPlan, ObjectKind, ObjectRef, ObjectSummary};
use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, ExecOptions, ExecOutput, Session, StatementResult, TlsMode, Value,
};
use varve_driver_mysql::MysqlDriver;
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const PG_DB: &str = "m2_cm_pg";
const PG_EXT_DB: &str = "m2_cm_pg_ext";
const PG_SUB_DB: &str = "m2_cm_pg_sub";
/// A role without superuser rights, for reading subscriptions.
const PG_READER: &str = "m2_cm_reader";
const PG_READER_PASSWORD: &str = "m2_cm_pw_4Rt";
const MY_DB: &str = "m2_cm_my";
const MY_GRANT_DB: &str = "m2_cm_my_grants";
/// Account the grant test gives routine privileges to.
const MY_USER: &str = "m2_cm_user";
const SCHEMA: &str = "m2_cm";
/// A comment that needs quoting.
const COMMENT: &str = "it's the m2 comment";

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

/// `VARVE_ONLY_SERVERS` (comma separated compose service names) limits a
/// run to those servers; without it every server is tested.
fn selected(service: &str) -> bool {
    std::env::var("VARVE_ONLY_SERVERS")
        .ok()
        .is_none_or(|list| list.split(',').any(|name| name.trim() == service))
}

/// Oldest and newest PostgreSQL as `(major, port)`.
fn pg_servers() -> Vec<(u32, u16)> {
    [
        (13, port("VARVE_PG13_PORT", 55513)),
        (18, port("VARVE_PG18_PORT", 55518)),
    ]
    .into_iter()
    .filter(|(major, _)| selected(&format!("postgres-{major}")))
    .collect()
}

/// Oldest and newest MySQL and MariaDB as `(service, engine, port)`.
fn my_servers() -> Vec<(&'static str, Engine, u16)> {
    [
        (
            "mysql-8.0",
            Engine::Mysql,
            port("VARVE_M2_MYSQL_8_0_PORT", 35080),
        ),
        (
            "mysql-9.7",
            Engine::Mysql,
            port("VARVE_M2_MYSQL_9_7_PORT", 35097),
        ),
        (
            "mariadb-10.6",
            Engine::Mariadb,
            port("VARVE_M2_MARIADB_10_6_PORT", 35106),
        ),
        (
            "mariadb-12.3",
            Engine::Mariadb,
            port("VARVE_M2_MARIADB_12_3_PORT", 35123),
        ),
    ]
    .into_iter()
    .filter(|(service, _, _)| selected(service))
    .collect()
}

async fn pg_connect(port: u16, database: &str) -> TestResult<Box<dyn Session>> {
    let mut params =
        ConnectParams::new(Engine::Postgres, env_or("VARVE_PG_HOST", "127.0.0.1"), port);
    params.database = Some(database.to_owned());
    params.username = Some(env_or("VARVE_PG_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_PG_PASSWORD", "varve_dev")));
    params.connect_timeout = Duration::from_secs(10);
    params.application_name = "varve-catalog m2_cm_comments tests".into();
    Ok(PostgresDriver::new().connect(&params).await?)
}

async fn pg_connect_as(
    port: u16,
    database: &str,
    user: &str,
    password: &str,
) -> TestResult<Box<dyn Session>> {
    let mut params =
        ConnectParams::new(Engine::Postgres, env_or("VARVE_PG_HOST", "127.0.0.1"), port);
    params.database = Some(database.to_owned());
    params.username = Some(user.to_owned());
    params.password = Some(Secret::new(password.to_owned()));
    params.connect_timeout = Duration::from_secs(10);
    params.application_name = "varve-catalog m2_cm_comments tests".into();
    Ok(PostgresDriver::new().connect(&params).await?)
}

async fn my_connect(engine: Engine, port: u16) -> TestResult<Box<dyn Session>> {
    let mut params = ConnectParams::new(engine, env_or("VARVE_M2_MYSQL_HOST", "127.0.0.1"), port);
    params.username = Some(env_or("VARVE_M2_MYSQL_USER", "root"));
    params.password = Some(Secret::new(env_or("VARVE_M2_MYSQL_PASSWORD", "varve_dev")));
    params.tls.mode = TlsMode::Disable;
    params.connect_timeout = Duration::from_secs(10);
    params.application_name = "varve-catalog m2_cm_comments tests".into();
    Ok(MysqlDriver::new().connect(&params).await?)
}

async fn exec(session: &dyn Session, sql: &str) -> TestResult<ExecOutput> {
    session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}\n=> {e}").into())
}

/// The first column of the first result set, as text (`None` for NULL).
async fn column(session: &dyn Session, sql: &str) -> TestResult<Vec<Option<String>>> {
    let output = exec(session, sql).await?;
    for result in output.results {
        if let StatementResult::Rows { rows, .. } = result {
            return Ok(rows
                .into_iter()
                .map(|row| row.first().and_then(Value::to_plain_text))
                .collect());
        }
    }
    Err(format!("{sql} returned no rows").into())
}

async fn apply(session: &dyn Session, plan: &EditPlan) -> TestResult {
    for statement in &plan.statements {
        exec(session, statement).await?;
    }
    Ok(())
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

async fn pg_drop_database(port: u16, database: &str) -> TestResult {
    let admin = pg_connect(port, &env_or("VARVE_PG_DATABASE", "varve")).await?;
    let exists = column(
        admin.as_ref(),
        &format!("SELECT datname FROM pg_database WHERE datname = '{database}'"),
    )
    .await?;
    if !exists.is_empty() {
        // Subscriptions block DROP DATABASE; drop them without a remote slot.
        let session = pg_connect(port, database).await?;
        let subs = column(
            session.as_ref(),
            "SELECT subname FROM pg_subscription WHERE subdbid = \
             (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .await?;
        for name in subs.into_iter().flatten() {
            let name = quote(&name);
            exec(
                session.as_ref(),
                &format!("ALTER SUBSCRIPTION {name} DISABLE"),
            )
            .await?;
            exec(
                session.as_ref(),
                &format!("ALTER SUBSCRIPTION {name} SET (slot_name = NONE)"),
            )
            .await?;
            exec(session.as_ref(), &format!("DROP SUBSCRIPTION {name}")).await?;
        }
        session.close().await?;
        exec(
            admin.as_ref(),
            &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quote(database)),
        )
        .await?;
    }
    admin.close().await?;
    Ok(())
}

async fn pg_fresh_database(port: u16, database: &str) -> TestResult<Box<dyn Session>> {
    pg_drop_database(port, database).await?;
    let admin = pg_connect(port, &env_or("VARVE_PG_DATABASE", "varve")).await?;
    exec(
        admin.as_ref(),
        &format!("CREATE DATABASE {}", quote(database)),
    )
    .await?;
    admin.close().await?;
    pg_connect(port, database).await
}

/// One object of every kind the PostgreSQL browser lists.
fn pg_fixtures() -> String {
    let s = SCHEMA;
    format!(
        "CREATE SCHEMA {s};
         CREATE TABLE {s}.t (id integer CONSTRAINT t_pk PRIMARY KEY,
             a integer CONSTRAINT t_a_check CHECK (a > 0));
         CREATE INDEX t_a_idx ON {s}.t (a);
         CREATE TABLE {s}.pt (id integer, at date) PARTITION BY RANGE (at);
         CREATE TABLE {s}.pt_2020 PARTITION OF {s}.pt
             FOR VALUES FROM ('2020-01-01') TO ('2021-01-01');
         CREATE VIEW {s}.v AS SELECT id FROM {s}.t;
         CREATE MATERIALIZED VIEW {s}.mv AS SELECT id FROM {s}.t;
         CREATE SEQUENCE {s}.seq;
         CREATE FUNCTION {s}.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1';
         CREATE PROCEDURE {s}.p() LANGUAGE sql AS 'SELECT 1';
         CREATE AGGREGATE {s}.agg(integer) (SFUNC = int4pl, STYPE = integer);
         CREATE FUNCTION {s}.trg() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TRIGGER t_trg BEFORE INSERT ON {s}.t FOR EACH ROW EXECUTE FUNCTION {s}.trg();
         CREATE FUNCTION {s}.evt() RETURNS event_trigger LANGUAGE plpgsql AS 'BEGIN END';
         CREATE EVENT TRIGGER m2_cm_evt ON ddl_command_end EXECUTE FUNCTION {s}.evt();
         CREATE TYPE {s}.e AS ENUM ('a');
         CREATE TYPE {s}.c AS (x integer);
         CREATE TYPE {s}.r AS RANGE (subtype = integer);
         CREATE DOMAIN {s}.d AS integer;
         CREATE EXTENSION hstore SCHEMA {s};
         CREATE RULE v_rule AS ON UPDATE TO {s}.v DO INSTEAD NOTHING;
         ALTER TABLE {s}.t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY t_pol ON {s}.t USING (true);
         CREATE PUBLICATION m2_cm_pub FOR TABLE {s}.t;
         CREATE SUBSCRIPTION m2_cm_sub CONNECTION 'host=127.0.0.1 dbname=m2_cm_none'
             PUBLICATION m2_cm_pub WITH (connect = false, slot_name = NONE);
         CREATE FOREIGN DATA WRAPPER m2_cm_fdw;
         CREATE SERVER m2_cm_srv FOREIGN DATA WRAPPER m2_cm_fdw;
         CREATE FOREIGN TABLE {s}.ft (x integer) SERVER m2_cm_srv;
         CREATE COLLATION {s}.coll (provider = libc, locale = 'C');
         CREATE CAST ({s}.c AS text) WITH INOUT;
         CREATE FUNCTION {s}.neg(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT -$1';
         CREATE OPERATOR {s}.~~~ (RIGHTARG = integer, FUNCTION = {s}.neg);"
    )
}

fn schema_container(database: &str) -> Container {
    Container {
        database: Some(database.to_owned()),
        schema: Some(SCHEMA.to_owned()),
        parent: None,
    }
}

fn database_container(database: &str) -> Container {
    Container {
        database: Some(database.to_owned()),
        schema: None,
        parent: None,
    }
}

fn child_container(database: &str, kind: ObjectKind, table: &str) -> Container {
    Container {
        database: Some(database.to_owned()),
        schema: Some(SCHEMA.to_owned()),
        parent: Some(ObjectRef {
            kind,
            database: Some(database.to_owned()),
            schema: Some(SCHEMA.to_owned()),
            parent: None,
            name: table.to_owned(),
            signature: None,
        }),
    }
}

/// The browser's reference to one object: listed in its container, as the
/// tree shows it.
async fn find(
    catalog: &dyn Catalog,
    session: &dyn Session,
    container: &Container,
    kind: ObjectKind,
    name: &str,
) -> TestResult<ObjectRef> {
    let listed: Vec<ObjectSummary> = catalog.list(session, container, kind, true).await?;
    listed
        .into_iter()
        .map(|s| s.object)
        .find(|o| o.name == name)
        .ok_or_else(|| format!("{kind:?} {name} is not listed in {container:?}").into())
}

/// The comment as the comment editor reads it from `catalog_describe`:
/// `object.comment`, else `object.meta.comment`. A details shape with
/// neither key fails.
async fn described_comment(
    catalog: &dyn Catalog,
    session: &dyn Session,
    object: &ObjectRef,
) -> TestResult<Option<String>> {
    let details = serde_json::to_value(catalog.describe(session, object).await?)?;
    let inner = &details["object"];
    let value = match inner.get("comment") {
        Some(v) => v,
        None => inner
            .get("meta")
            .and_then(|m| m.get("comment"))
            .ok_or_else(|| format!("{:?} details have no comment: {inner}", object.kind))?,
    };
    Ok(value.as_str().map(str::to_owned))
}

async fn comment_round_trip(
    catalog: &dyn Catalog,
    session: &dyn Session,
    object: &ObjectRef,
) -> TestResult {
    let server = session.server_info();
    for comment in [Some(COMMENT.to_owned()), None] {
        let plan = varve_catalog::postgres::edit::plan(
            server,
            &PgChange::Comment(CommentChange {
                target: object.clone(),
                comment: comment.clone(),
            }),
        )?;
        assert_eq!(plan.statements.len(), 1, "{plan:?}");
        assert!(plan.statements[0].starts_with("COMMENT ON "), "{plan:?}");
        assert!(!plan.destructive, "{plan:?}");
        apply(session, &plan).await?;
        let read = described_comment(catalog, session, object).await?;
        assert_eq!(
            read, comment,
            "{:?} {} after {plan:?}",
            object.kind, object.name
        );
    }
    Ok(())
}

async fn pg_comments(major: u32, port: u16) -> TestResult {
    let session = pg_fresh_database(port, PG_DB).await?;
    assert_eq!(session.server_info().major, major);
    exec(session.as_ref(), &pg_fixtures()).await?;
    let s = session.as_ref();
    let catalog = varve_catalog::catalog_for(s.server_info())?;
    let c = catalog.as_ref();
    use ObjectKind as K;

    let in_schema = schema_container(PG_DB);
    let in_database = database_container(PG_DB);
    let mut objects = Vec::new();
    for (kind, name) in [
        (K::Table, "t"),
        (K::PartitionedTable, "pt"),
        (K::View, "v"),
        (K::MaterializedView, "mv"),
        (K::ForeignTable, "ft"),
        (K::Sequence, "seq"),
        (K::Function, "f"),
        (K::Procedure, "p"),
        (K::Aggregate, "agg"),
        (K::EnumType, "e"),
        (K::CompositeType, "c"),
        (K::RangeType, "r"),
        (K::Domain, "d"),
        (K::Collation, "coll"),
        (K::Operator, "~~~"),
    ] {
        objects.push(find(c, s, &in_schema, kind, name).await?);
    }
    for (kind, name) in [
        (K::EventTrigger, "m2_cm_evt"),
        (K::Extension, "hstore"),
        (K::Publication, "m2_cm_pub"),
        (K::Subscription, "m2_cm_sub"),
        (K::ForeignDataWrapper, "m2_cm_fdw"),
        (K::ForeignServer, "m2_cm_srv"),
    ] {
        objects.push(find(c, s, &in_database, kind, name).await?);
    }
    // Casts are listed per database under their `source AS target` name.
    let casts: Vec<ObjectSummary> = c.list(s, &in_database, K::Cast, false).await?;
    let cast = casts
        .into_iter()
        .map(|x| x.object)
        .find(|o| {
            o.signature
                .as_deref()
                .is_some_and(|sig| sig.contains("m2_cm.c"))
        })
        .ok_or("the m2_cm.c cast is not listed")?;
    objects.push(cast);
    for (parent_kind, table, kind, name) in [
        (K::Table, "t", K::Column, "a"),
        (K::Table, "t", K::Index, "t_a_idx"),
        (K::Table, "t", K::Constraint, "t_pk"),
        (K::Table, "t", K::Constraint, "t_a_check"),
        (K::Table, "t", K::Trigger, "t_trg"),
        (K::Table, "t", K::Policy, "t_pol"),
        (K::View, "v", K::Rule, "v_rule"),
        (K::PartitionedTable, "pt", K::Partition, "pt_2020"),
    ] {
        let container = child_container(PG_DB, parent_kind, table);
        objects.push(find(c, s, &container, kind, name).await?);
    }
    let schema = c
        .schemas(s, PG_DB)
        .await?
        .into_iter()
        .map(|x| x.object)
        .find(|o| o.name == SCHEMA)
        .ok_or("schema m2_cm is not listed")?;
    objects.push(schema);
    let database = c
        .databases(s)
        .await?
        .into_iter()
        .map(|x| x.object)
        .find(|o| o.name == PG_DB)
        .ok_or("database m2_cm_pg is not listed")?;
    objects.push(database);

    let mut covered: Vec<ObjectKind> = Vec::new();
    for object in &objects {
        comment_round_trip(c, s, object).await?;
        if !covered.contains(&object.kind) {
            covered.push(object.kind);
        }
    }

    // Tablespaces are server wide: comment pg_default and put the original
    // comment back.
    let tablespaces: Vec<ObjectSummary> = c
        .list(s, &Container::default(), K::Tablespace, true)
        .await?;
    let pg_default = tablespaces
        .into_iter()
        .map(|x| x.object)
        .find(|o| o.name == "pg_default")
        .ok_or("pg_default is not listed")?;
    let before = described_comment(c, s, &pg_default).await?;
    let result = comment_round_trip(c, s, &pg_default).await;
    let restore = varve_catalog::postgres::edit::plan(
        s.server_info(),
        &PgChange::Comment(CommentChange {
            target: pg_default.clone(),
            comment: before.clone(),
        }),
    )?;
    apply(s, &restore).await?;
    result?;
    covered.push(K::Tablespace);

    // Every kind the server lists in the tree has been commented.
    for kind in &c.capabilities().kinds {
        assert!(
            covered.contains(kind),
            "PostgreSQL {major}: {kind:?} is listed but not covered"
        );
    }
    session.close().await?;
    pg_drop_database(port, PG_DB).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn postgres_comments_on_every_listed_kind() -> TestResult {
    for (major, port) in pg_servers() {
        let result = pg_comments(major, port).await;
        if result.is_err() {
            let _ = pg_drop_database(port, PG_DB).await;
        }
        result.map_err(|e| format!("PostgreSQL {major}: {e}"))?;
    }
    Ok(())
}

async fn pg_extension_picker(major: u32, port: u16) -> TestResult {
    let session = pg_fresh_database(port, PG_EXT_DB).await?;
    let s = session.as_ref();
    exec(s, &format!("CREATE SCHEMA {SCHEMA}")).await?;

    let before = available_extensions(s).await?;
    let names: Vec<&str> = before.iter().map(|e| e.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "sorted by name");
    let plpgsql = before
        .iter()
        .find(|e| e.name == "plpgsql")
        .ok_or("plpgsql is not available")?;
    assert!(plpgsql.installed_version.is_some());
    assert_eq!(plpgsql.installed_schema.as_deref(), Some("pg_catalog"));
    let citext = before
        .iter()
        .find(|e| e.name == "citext")
        .ok_or("citext is not available")?;
    assert!(citext.installed_version.is_none() && citext.installed_schema.is_none());
    assert!(citext.comment.is_some());
    let default = citext
        .default_version
        .clone()
        .ok_or("citext has no default")?;
    assert!(citext.versions.contains(&default), "{citext:?}");
    assert!(citext.versions.len() > 1, "{citext:?}");
    assert!(citext.relocatable && citext.fixed_schema.is_none());
    assert!(
        citext.trusted,
        "PostgreSQL {major}: citext is trusted from 13 on"
    );
    let earth = before
        .iter()
        .find(|e| e.name == "earthdistance")
        .ok_or("earthdistance is not available")?;
    assert_eq!(earth.requires, vec!["cube".to_owned()]);
    // Every version carries its own control file data, in version order.
    let detail_versions: Vec<&str> = citext
        .version_details
        .iter()
        .map(|v| v.version.as_str())
        .collect();
    assert_eq!(
        detail_versions,
        citext
            .versions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    assert!(
        earth
            .version_details
            .iter()
            .all(|v| v.requires == vec!["cube".to_owned()])
    );
    let default_detail = citext
        .version_details
        .iter()
        .find(|v| v.version == default)
        .ok_or("no details for the default version")?;
    assert_eq!(
        (default_detail.trusted, default_detail.relocatable),
        (citext.trusted, citext.relocatable)
    );

    // Install an older citext version into the chosen schema.
    let oldest = citext.versions[0].clone();
    let installable = column(
        s,
        &format!(
            "SELECT version FROM pg_available_extension_versions WHERE name = 'citext' AND version = '{oldest}'"
        ),
    )
    .await?;
    assert_eq!(installable.len(), 1);
    let plan = varve_catalog::postgres::edit::plan(
        s.server_info(),
        &PgChange::Extension(ExtensionChange::Create(CreateExtension {
            name: "citext".into(),
            if_not_exists: false,
            schema: Some(SCHEMA.into()),
            version: Some(oldest.clone()),
            cascade: false,
            comment: None,
        })),
    )?;
    apply(s, &plan).await?;
    // CASCADE installs the required extension.
    let plan = varve_catalog::postgres::edit::plan(
        s.server_info(),
        &PgChange::Extension(ExtensionChange::Create(CreateExtension {
            name: "earthdistance".into(),
            if_not_exists: false,
            schema: Some(SCHEMA.into()),
            version: None,
            cascade: true,
            comment: None,
        })),
    )?;
    assert!(plan.statements[0].contains("CASCADE"), "{plan:?}");
    apply(s, &plan).await?;

    let after = available_extensions(s).await?;
    let installed = |name: &str| {
        after
            .iter()
            .find(|e| e.name == name)
            .map(|e| (e.installed_version.clone(), e.installed_schema.clone()))
    };
    assert_eq!(
        installed("citext"),
        Some((Some(oldest), Some(SCHEMA.to_owned())))
    );
    assert_eq!(
        installed("cube").and_then(|(_, schema)| schema),
        Some(SCHEMA.to_owned())
    );
    assert!(installed("earthdistance").is_some_and(|(v, _)| v.is_some()));

    // Inside a transaction block the call refuses, as every catalog call.
    exec(s, "BEGIN").await?;
    assert!(available_extensions(s).await.is_err());
    exec(s, "ROLLBACK").await?;
    session.close().await?;
    pg_drop_database(port, PG_EXT_DB).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn postgres_available_extensions_feed_the_install_picker() -> TestResult {
    for (major, port) in pg_servers() {
        let result = pg_extension_picker(major, port).await;
        if result.is_err() {
            let _ = pg_drop_database(port, PG_EXT_DB).await;
        }
        result.map_err(|e| format!("PostgreSQL {major}: {e}"))?;
    }
    Ok(())
}

fn my_change(value: Json) -> TestResult<MyChange> {
    Ok(serde_json::from_value(value)?)
}

async fn my_run(session: &dyn Session, change: Json) -> TestResult<EditPlan> {
    let plan = varve_catalog::mysql::edit::plan(session.server_info(), &my_change(change)?)?;
    apply(session, &plan).await?;
    Ok(plan)
}

async fn my_comments(engine: Engine, port: u16) -> TestResult {
    let session = my_connect(engine, port).await?;
    let s = session.as_ref();
    exec(s, &format!("DROP DATABASE IF EXISTS `{MY_DB}`")).await?;
    exec(s, &format!("CREATE DATABASE `{MY_DB}`")).await?;
    exec(
        s,
        &format!("CREATE PROCEDURE `{MY_DB}`.`p`() COMMENT 'old' BEGIN SELECT 1; END"),
    )
    .await?;
    exec(
        s,
        &format!(
            "CREATE EVENT `{MY_DB}`.`ev` ON SCHEDULE EVERY 1 DAY DISABLE COMMENT 'old' DO SELECT 1"
        ),
    )
    .await?;
    let catalog = varve_catalog::open_catalog(s).await?;
    let read_back = |kind: ObjectKind, name: &str| ObjectRef {
        kind,
        database: Some(MY_DB.to_owned()),
        schema: None,
        parent: None,
        name: name.to_owned(),
        signature: None,
    };

    // Routine: the routine editor's change carries the comment.
    my_run(
        s,
        json!({"change": "createRoutine", "database": MY_DB, "name": "p", "routine": "procedure",
            "replace": true, "parameters": [], "returns": null,
            "characteristics": {"comment": COMMENT, "deterministic": null, "dataAccess": null, "security": null},
            "body": "BEGIN SELECT 1; END"}),
    )
    .await?;
    assert_eq!(
        column(
            s,
            &format!(
                "SELECT ROUTINE_COMMENT FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = '{MY_DB}' AND ROUTINE_NAME = 'p'"
            )
        )
        .await?,
        vec![Some(COMMENT.to_owned())]
    );
    let details = serde_json::to_value(
        catalog
            .describe(s, &read_back(ObjectKind::Procedure, "p"))
            .await?,
    )?;
    assert_eq!(details["object"]["comment"], json!(COMMENT), "{details}");

    // Event: ALTER EVENT ... COMMENT changes only the comment.
    let plan = my_run(
        s,
        json!({"change": "alterEvent", "database": MY_DB, "name": "ev", "comment": COMMENT}),
    )
    .await?;
    assert_eq!(plan.statements.len(), 1, "{plan:?}");
    assert!(!plan.destructive);
    assert_eq!(
        column(
            s,
            &format!(
                "SELECT EVENT_COMMENT FROM information_schema.EVENTS WHERE EVENT_SCHEMA = '{MY_DB}' AND EVENT_NAME = 'ev'"
            )
        )
        .await?,
        vec![Some(COMMENT.to_owned())]
    );
    let details = serde_json::to_value(
        catalog
            .describe(s, &read_back(ObjectKind::Event, "ev"))
            .await?,
    )?;
    assert_eq!(details["object"]["comment"], json!(COMMENT), "{details}");

    exec(s, &format!("DROP DATABASE `{MY_DB}`")).await?;
    session.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn mysql_and_mariadb_routine_and_event_comments() -> TestResult {
    for (service, engine, port) in my_servers() {
        let result = my_comments(engine, port).await;
        if result.is_err()
            && let Ok(session) = my_connect(engine, port).await
        {
            let _ = exec(
                session.as_ref(),
                &format!("DROP DATABASE IF EXISTS `{MY_DB}`"),
            )
            .await;
            let _ = session.close().await;
        }
        result.map_err(|e| format!("{service}: {e}"))?;
    }
    Ok(())
}

/// The routine privileges `MY_USER` holds on `name`, from `mysql.procs_priv`.
async fn routine_grants(
    session: &dyn Session,
    routine: &str,
    name: &str,
) -> TestResult<Vec<Option<String>>> {
    column(
        session,
        &format!(
            "SELECT Proc_priv FROM mysql.procs_priv WHERE Db = '{MY_GRANT_DB}' AND User = '{MY_USER}' \
             AND Routine_name = '{name}' AND Routine_type = '{routine}'"
        ),
    )
    .await
}

async fn my_alter_keeps_grants(engine: Engine, port: u16) -> TestResult {
    let session = my_connect(engine, port).await?;
    let s = session.as_ref();
    exec(s, &format!("DROP DATABASE IF EXISTS `{MY_GRANT_DB}`")).await?;
    exec(s, &format!("DROP USER IF EXISTS '{MY_USER}'@'%'")).await?;
    exec(s, &format!("CREATE DATABASE `{MY_GRANT_DB}`")).await?;
    exec(
        s,
        &format!("CREATE USER '{MY_USER}'@'%' IDENTIFIED BY 'm2_cm_pw_7Qx'"),
    )
    .await?;
    exec(
        s,
        &format!("CREATE PROCEDURE `{MY_GRANT_DB}`.`p`() COMMENT 'old' BEGIN SELECT 1; END"),
    )
    .await?;
    exec(
        s,
        &format!(
            "CREATE FUNCTION `{MY_GRANT_DB}`.`f`(x INT) RETURNS INT DETERMINISTIC NO SQL RETURN x"
        ),
    )
    .await?;
    for (keyword, name) in [("PROCEDURE", "p"), ("FUNCTION", "f")] {
        exec(
            s,
            &format!(
                "GRANT EXECUTE, ALTER ROUTINE ON {keyword} `{MY_GRANT_DB}`.`{name}` TO '{MY_USER}'@'%'"
            ),
        )
        .await?;
    }
    let before_p = routine_grants(s, "PROCEDURE", "p").await?;
    let before_f = routine_grants(s, "FUNCTION", "f").await?;
    assert_eq!(before_p.len(), 1, "{before_p:?}");
    assert_eq!(before_f.len(), 1, "{before_f:?}");

    let plan = my_run(
        s,
        json!({"change": "alterRoutine", "database": MY_GRANT_DB, "name": "p", "routine": "procedure",
            "comment": COMMENT}),
    )
    .await?;
    assert_eq!(plan.statements.len(), 1, "{plan:?}");
    assert!(
        plan.statements[0].starts_with("ALTER PROCEDURE "),
        "{plan:?}"
    );
    assert!(!plan.destructive && plan.warnings.is_empty(), "{plan:?}");
    my_run(
        s,
        json!({"change": "alterRoutine", "database": MY_GRANT_DB, "name": "f", "routine": "function",
            "comment": "f's", "dataAccess": "readsSqlData", "security": "invoker"}),
    )
    .await?;
    let read = |name: &str| {
        format!(
            "SELECT CONCAT(ROUTINE_COMMENT, '|', SQL_DATA_ACCESS, '|', SECURITY_TYPE) FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = '{MY_GRANT_DB}' AND ROUTINE_NAME = '{name}'"
        )
    };
    assert_eq!(
        column(s, &read("p")).await?,
        vec![Some(format!("{COMMENT}|CONTAINS SQL|DEFINER"))]
    );
    assert_eq!(
        column(s, &read("f")).await?,
        vec![Some("f's|READS SQL DATA|INVOKER".to_owned())]
    );
    // The routines were not dropped: the grants on them are still there.
    assert_eq!(routine_grants(s, "PROCEDURE", "p").await?, before_p);
    assert_eq!(routine_grants(s, "FUNCTION", "f").await?, before_f);

    // An empty comment removes it in place on MariaDB. MySQL ignores
    // `COMMENT ''` in ALTER, so the plan refuses it there instead of
    // silently keeping the old comment.
    let clear = json!({"change": "alterRoutine", "database": MY_GRANT_DB, "name": "p", "routine": "procedure", "comment": ""});
    if engine == Engine::Mariadb {
        my_run(s, clear).await?;
        assert_eq!(
            column(s, &read("p")).await?,
            vec![Some("|CONTAINS SQL|DEFINER".to_owned())]
        );
    } else {
        let refused = varve_catalog::mysql::edit::plan(s.server_info(), &my_change(clear)?);
        assert!(
            matches!(
                refused,
                Err(varve_catalog::CatalogError::Unsupported { .. })
            ),
            "{refused:?}"
        );
    }
    assert_eq!(routine_grants(s, "PROCEDURE", "p").await?, before_p);

    exec(s, &format!("DROP USER '{MY_USER}'@'%'")).await?;
    exec(s, &format!("DROP DATABASE `{MY_GRANT_DB}`")).await?;
    session.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn mysql_and_mariadb_routine_alter_keeps_grants() -> TestResult {
    for (service, engine, port) in my_servers() {
        let result = my_alter_keeps_grants(engine, port).await;
        if result.is_err()
            && let Ok(session) = my_connect(engine, port).await
        {
            let _ = exec(
                session.as_ref(),
                &format!("DROP USER IF EXISTS '{MY_USER}'@'%'"),
            )
            .await;
            let _ = exec(
                session.as_ref(),
                &format!("DROP DATABASE IF EXISTS `{MY_GRANT_DB}`"),
            )
            .await;
            let _ = session.close().await;
        }
        result.map_err(|e| format!("{service}: {e}"))?;
    }
    Ok(())
}

async fn pg_drop_reader(port: u16) -> TestResult {
    let admin = pg_connect(port, &env_or("VARVE_PG_DATABASE", "varve")).await?;
    exec(admin.as_ref(), &format!("DROP ROLE IF EXISTS {PG_READER}")).await?;
    admin.close().await?;
    Ok(())
}

async fn pg_subscription_comment_for_readers(major: u32, port: u16) -> TestResult {
    pg_drop_database(port, PG_SUB_DB).await?;
    pg_drop_reader(port).await?;
    let session = pg_fresh_database(port, PG_SUB_DB).await?;
    let s = session.as_ref();
    exec(
        s,
        &format!("CREATE ROLE {PG_READER} LOGIN PASSWORD '{PG_READER_PASSWORD}'"),
    )
    .await?;
    exec(
        s,
        "CREATE PUBLICATION m2_cm_pub FOR ALL TABLES;
         CREATE SUBSCRIPTION m2_cm_sub CONNECTION 'host=127.0.0.1 dbname=m2_cm_none'
             PUBLICATION m2_cm_pub WITH (connect = false, slot_name = NONE);
         COMMENT ON SUBSCRIPTION m2_cm_sub IS 'feeds reporting'",
    )
    .await?;
    let object = ObjectRef {
        kind: ObjectKind::Subscription,
        database: Some(PG_SUB_DB.to_owned()),
        schema: None,
        parent: None,
        name: "m2_cm_sub".to_owned(),
        signature: None,
    };
    let reader = pg_connect_as(port, PG_SUB_DB, PG_READER, PG_READER_PASSWORD).await?;
    let r = reader.as_ref();
    let catalog = varve_catalog::catalog_for(r.server_info())?;
    let details = serde_json::to_value(catalog.describe(r, &object).await?)?;
    let inner = &details["object"];
    // The connection string stays hidden from a role without superuser rights.
    assert_eq!(inner["connectionVisible"], json!(false), "{inner}");
    if major >= 14 {
        assert_eq!(inner["commentVisible"], json!(true), "{inner}");
        assert_eq!(
            inner["meta"]["comment"],
            json!("feeds reporting"),
            "{inner}"
        );
    } else {
        // PostgreSQL 13 hides the oid: the comment is unknown, not empty.
        assert_eq!(inner["commentVisible"], json!(false), "{inner}");
        assert_eq!(inner["meta"]["comment"], Json::Null, "{inner}");
    }
    // A superuser always reads it.
    let details = serde_json::to_value(catalog.describe(s, &object).await?)?;
    assert_eq!(details["object"]["commentVisible"], json!(true));
    assert_eq!(
        details["object"]["meta"]["comment"],
        json!("feeds reporting")
    );
    reader.close().await?;
    session.close().await?;
    pg_drop_database(port, PG_SUB_DB).await?;
    pg_drop_reader(port).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn postgres_subscription_comment_for_roles_without_superuser() -> TestResult {
    for (major, port) in pg_servers() {
        let result = pg_subscription_comment_for_readers(major, port).await;
        if result.is_err() {
            let _ = pg_drop_database(port, PG_SUB_DB).await;
            let _ = pg_drop_reader(port).await;
        }
        result.map_err(|e| format!("PostgreSQL {major}: {e}"))?;
    }
    Ok(())
}
