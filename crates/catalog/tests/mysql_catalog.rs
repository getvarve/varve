//! Integration tests of the MySQL and MariaDB catalog against the M2
//! database matrix.
//!
//! Run with `cargo test -p varve-catalog --test mysql_catalog -- --include-ignored`.
//! Endpoints come from environment variables: `VARVE_M2_MYSQL_HOST`
//! (default `127.0.0.1`), `VARVE_M2_MYSQL_USER` (`root`),
//! `VARVE_M2_MYSQL_PASSWORD` (`varve_dev`) and one port per server:
//! `VARVE_M2_MYSQL_8_0_PORT` (35080), `VARVE_M2_MYSQL_9_7_PORT` (35097),
//! `VARVE_M2_MARIADB_10_6_PORT` (35106), `VARVE_M2_MARIADB_12_3_PORT`
//! (35123).
//!
//! Each server gets its own databases named `m2_mycat_*`, dropped before
//! and after the run. The catalog session's default database is `varve`,
//! never the fixture database, so the DDL must name its database itself.
//!
//! The round trip: every object's details and DDL are read, the fixture
//! database is dropped and rebuilt from the DDL alone, and every object
//! must read back the same. Re-running each DDL while the object exists
//! must succeed and change nothing, and columns, indexes, constraints and
//! partitions are dropped one by one and re-added from their DDL.

use std::error::Error;

use serde_json::Value as Json;
use varve_catalog::ObjectDetails;
use varve_catalog::model::{Catalog, CatalogError, Container, ObjectKind, ObjectRef};
use varve_catalog::mysql::{
    MyCatalog, MyConstraintType, MyDefault, MyEventScheduler, MyGeneratedKind, MyIndexKind,
    MyObject, ddl_session_header, strip_ddl_session_header,
};
use varve_core::Engine;
use varve_driver::{ConnectParams, ExecOptions, Session, StatementResult, TlsMode};
use varve_driver_mysql::{MysqlDriver, MysqlSession, SessionOverrides};
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn Error>>;

#[derive(Debug, Clone, Copy)]
struct Target {
    name: &'static str,
    tag: &'static str,
    engine: Engine,
    port_var: &'static str,
    port: u16,
}

const TARGETS: &[Target] = &[
    Target {
        name: "MySQL 8.0",
        tag: "m80",
        engine: Engine::Mysql,
        port_var: "VARVE_M2_MYSQL_8_0_PORT",
        port: 35080,
    },
    Target {
        name: "MySQL 9.7",
        tag: "m97",
        engine: Engine::Mysql,
        port_var: "VARVE_M2_MYSQL_9_7_PORT",
        port: 35097,
    },
    Target {
        name: "MariaDB 10.6",
        tag: "d106",
        engine: Engine::Mariadb,
        port_var: "VARVE_M2_MARIADB_10_6_PORT",
        port: 35106,
    },
    Target {
        name: "MariaDB 12.3",
        tag: "d123",
        engine: Engine::Mariadb,
        port_var: "VARVE_M2_MARIADB_12_3_PORT",
        port: 35123,
    },
];

/// Whether the server with `dev/docker-compose.yml` service name `service`
/// (for example `mysql-9.7`) is tested. `VARVE_ONLY_SERVERS`, a comma
/// separated list of service names, limits a run to those servers (CI tests
/// only the newest version of each engine); without it every server is tested.
fn selected(service: &str) -> bool {
    std::env::var("VARVE_ONLY_SERVERS")
        .ok()
        .is_none_or(|list| list.split(',').any(|name| name.trim() == service))
}

/// The [`TARGETS`] to test (see [`selected`]), never none.
fn targets() -> Vec<&'static Target> {
    let targets: Vec<&'static Target> = TARGETS
        .iter()
        .filter(|target| selected(&target.name.to_lowercase().replace(' ', "-")))
        .collect();
    assert!(
        !targets.is_empty(),
        "VARVE_ONLY_SERVERS selects no MySQL or MariaDB server"
    );
    targets
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

async fn connect(target: &Target, database: &str) -> Result<MysqlSession, Box<dyn Error>> {
    let port = std::env::var(target.port_var)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(target.port);
    let mut params = ConnectParams::new(
        target.engine,
        env_or("VARVE_M2_MYSQL_HOST", "127.0.0.1"),
        port,
    );
    params.username = Some(env_or("VARVE_M2_MYSQL_USER", "root"));
    params.password = Some(Secret::new(env_or("VARVE_M2_MYSQL_PASSWORD", "varve_dev")));
    params.database = Some(database.to_owned());
    params.tls.mode = TlsMode::Disable;
    params.application_name = "varve-catalog-tests".into();
    MysqlDriver::new()
        .open(&params)
        .await
        .map_err(|e| format!("{}: connect: {e}", target.name).into())
}

async fn exec(session: &dyn Session, sql: &str) -> TestResult {
    session
        .execute(sql, &ExecOptions::default())
        .await
        .map(|_| ())
        .map_err(|e| format!("{sql}\n=> {e:?}").into())
}

/// The first value of the first row, as text.
async fn scalar_text(session: &dyn Session, sql: &str) -> Result<Option<String>, Box<dyn Error>> {
    let output = session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}\n=> {e:?}"))?;
    for result in output.results {
        if let StatementResult::Rows { rows, .. } = result {
            return Ok(rows
                .first()
                .and_then(|row| row.first())
                .and_then(varve_driver::Value::to_plain_text));
        }
    }
    Ok(None)
}

/// Runs catalog DDL like the app does: the session header's settings go
/// to the driver's dedicated path, which applies them for this call only,
/// and the database collation (routines keep the one their database had)
/// is set with ALTER DATABASE around the call.
async fn run_ddl(session: &MysqlSession, database: &str, ddl: &str) -> TestResult {
    let Some(header) = ddl_session_header(ddl)? else {
        return exec(session, ddl).await;
    };
    let sql = strip_ddl_session_header(ddl);
    let mut restore = None;
    if let Some(collation) = &header.collation_database {
        let current = scalar_text(
            session,
            &format!(
                "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA \
                 WHERE SCHEMA_NAME = '{database}'"
            ),
        )
        .await?;
        if let Some(current) = current {
            exec(
                session,
                &format!("ALTER DATABASE `{database}` COLLATE {collation}"),
            )
            .await?;
            restore = Some(format!("ALTER DATABASE `{database}` COLLATE {current}"));
        }
    }
    let overrides = SessionOverrides {
        character_set_client: header.character_set_client.clone(),
        // character_set_results is recorded but not applied: results stay
        // utf8mb4 (see varve_catalog::mysql::session).
        collation_connection: header.collation_connection.clone(),
        sql_mode: header.sql_mode.clone(),
        time_zone: header.time_zone.clone(),
    };
    let result = session
        .execute_with_session(&overrides, sql, &ExecOptions::default())
        .await
        .map(|_| ())
        .map_err(|e| format!("{ddl}\n=> {e:?}").into());
    if let Some(statement) = restore {
        exec(session, &statement).await?;
    }
    result
}

fn obj(kind: ObjectKind, db: &str, parent: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind,
        database: Some(db.to_owned()),
        schema: None,
        parent: parent.map(str::to_owned),
        name: name.to_owned(),
        signature: None,
    }
}

fn container(db: &str, parent: Option<ObjectRef>) -> Container {
    Container {
        database: Some(db.to_owned()),
        schema: None,
        parent,
    }
}

/// Details as JSON without values that legitimately change when an object
/// is re-created (times, sizes, statistics).
fn normalized(details: &ObjectDetails) -> Result<Json, Box<dyn Error>> {
    fn strip(value: &mut Json) {
        match value {
            Json::Object(map) => {
                for key in [
                    "created",
                    "altered",
                    "updated",
                    "lastExecuted",
                    "rowEstimate",
                    "avgRowLength",
                    "dataLength",
                    "indexLength",
                    "dataFree",
                    "rows",
                    "sizeBytes",
                ] {
                    map.remove(key);
                }
                map.values_mut().for_each(strip);
            }
            Json::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut json = serde_json::to_value(details)?;
    strip(&mut json);
    Ok(json)
}

fn my(details: ObjectDetails) -> Result<MyObject, Box<dyn Error>> {
    match details {
        ObjectDetails::Mysql(object) => Ok(*object),
        other => Err(format!("not a MySQL object: {other:?}").into()),
    }
}

fn check(target: &Target, ok: bool, what: impl std::fmt::Display) -> TestResult {
    if ok {
        Ok(())
    } else {
        Err(format!("{}: {what}", target.name).into())
    }
}

fn mysql_fixtures() -> Vec<&'static str> {
    vec![
        "CREATE TABLE p (id INT PRIMARY KEY) ENGINE=InnoDB",
        "CREATE TABLE t (
           id INT NOT NULL AUTO_INCREMENT,
           code VARCHAR(20) NOT NULL DEFAULT 'a''b\\\\c' COMMENT 'code, it''s',
           note VARCHAR(20) COLLATE utf8mb4_bin DEFAULT 'n' INVISIBLE,
           qty INT DEFAULT NULL,
           price DECIMAL(8,2) DEFAULT 1.50,
           doubled INT GENERATED ALWAYS AS (qty * 2) VIRTUAL,
           tripled INT AS (qty * 3) STORED,
           made DATETIME DEFAULT (NOW()),
           changed TIMESTAMP NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
           doc JSON,
           body TEXT,
           place POINT NOT NULL SRID 4326,
           parent_id INT,
           PRIMARY KEY (id),
           UNIQUE KEY uq_code (code),
           KEY ix_code_qty (code(5) DESC, qty) COMMENT 'ix',
           KEY ix_hidden (qty) INVISIBLE,
           KEY ix_func ((LOWER(code))),
           FULLTEXT KEY ft_body (body),
           SPATIAL KEY sp_place (place),
           CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES p (id) ON DELETE SET NULL ON UPDATE CASCADE,
           CONSTRAINT ck_qty CHECK (qty >= 0),
           CONSTRAINT ck_soft CHECK (price < 1000) NOT ENFORCED
         ) ENGINE=InnoDB ROW_FORMAT=DYNAMIC COMMENT='main table'",
    ]
}

fn mariadb_fixtures() -> Vec<&'static str> {
    vec![
        "CREATE TABLE p (id INT PRIMARY KEY) ENGINE=InnoDB",
        "CREATE TABLE t (
           id INT NOT NULL AUTO_INCREMENT,
           code VARCHAR(20) NOT NULL DEFAULT 'a''b\\\\c' COMMENT 'code, it''s',
           note VARCHAR(20) COLLATE utf8mb4_bin INVISIBLE DEFAULT 'n',
           qty INT DEFAULT NULL CHECK (qty < 100000),
           price DECIMAL(8,2) DEFAULT 1.50,
           doubled INT GENERATED ALWAYS AS (qty * 2) VIRTUAL,
           tripled INT AS (qty * 3) PERSISTENT,
           made DATETIME DEFAULT NOW(),
           changed TIMESTAMP NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
           doc JSON,
           body TEXT,
           place POINT REF_SYSTEM_ID=4326 NOT NULL,
           parent_id INT,
           PRIMARY KEY (id),
           UNIQUE KEY uq_code (code),
           KEY ix_code_qty (code(5) DESC, qty) COMMENT 'ix',
           KEY ix_hidden (qty) IGNORED,
           FULLTEXT KEY ft_body (body),
           SPATIAL KEY sp_place (place),
           CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES p (id) ON DELETE SET NULL ON UPDATE CASCADE,
           CONSTRAINT ck_qty CHECK (qty >= 0)
         ) ENGINE=InnoDB ROW_FORMAT=DYNAMIC COMMENT='main table'",
        "CREATE SEQUENCE sq START WITH 100 INCREMENT BY 5 MINVALUE 1 MAXVALUE 100000 CACHE 10 CYCLE COMMENT 'seq'",
        "CREATE TABLE sv (
           id INT PRIMARY KEY,
           val INT,
           rs TIMESTAMP(6) GENERATED ALWAYS AS ROW START INVISIBLE,
           re TIMESTAMP(6) GENERATED ALWAYS AS ROW END INVISIBLE,
           PERIOD FOR SYSTEM_TIME (rs, re)
         ) WITH SYSTEM VERSIONING
         PARTITION BY SYSTEM_TIME (PARTITION p_hist HISTORY, PARTITION p_cur CURRENT)",
        "CREATE TABLE ap (id INT, ds DATE NOT NULL, de DATE NOT NULL, PERIOD FOR valid (ds, de), UNIQUE KEY uk (id, valid WITHOUT OVERLAPS))",
        "SET @saved_mode = @@SESSION.sql_mode",
        "SET SESSION sql_mode = 'ORACLE'",
        "CREATE PACKAGE pk AS FUNCTION f(a INT) RETURN INT; END",
        "CREATE PACKAGE BODY pk AS FUNCTION f(a INT) RETURN INT AS BEGIN RETURN a + 1; END; END",
        "SET SESSION sql_mode = @saved_mode",
    ]
}

fn common_fixtures() -> Vec<&'static str> {
    vec![
        "CREATE TABLE pt (id INT NOT NULL, y INT NOT NULL, PRIMARY KEY (id, y))
         PARTITION BY RANGE (y) SUBPARTITION BY HASH (id) SUBPARTITIONS 2
         (PARTITION p2020 VALUES LESS THAN (2021) COMMENT = 'old',
          PARTITION p2021 VALUES LESS THAN (2022),
          PARTITION pmax VALUES LESS THAN MAXVALUE)",
        "CREATE TABLE lt (id INT NOT NULL, region INT NOT NULL)
         PARTITION BY LIST (region) (PARTITION pa VALUES IN (1,2), PARTITION pb VALUES IN (3))",
        "CREATE TABLE kt (id INT NOT NULL PRIMARY KEY) PARTITION BY LINEAR KEY (id) PARTITIONS 3",
        "CREATE ALGORITHM=MERGE SQL SECURITY INVOKER VIEW v (vid, vcode) AS
         SELECT id, code FROM t WHERE qty > 0 WITH CASCADED CHECK OPTION",
        "CREATE PROCEDURE pr(IN a INT, OUT b VARCHAR(10), INOUT c INT) COMMENT 'proc' READS SQL DATA
         BEGIN SET b = 'x'; SET c = c + a; END",
        "CREATE FUNCTION fn(a INT) RETURNS INT DETERMINISTIC NO SQL SQL SECURITY INVOKER RETURN a * 2",
        "CREATE TRIGGER tr1 BEFORE INSERT ON t FOR EACH ROW SET NEW.qty = COALESCE(NEW.qty, 0)",
        "CREATE TRIGGER tr2 BEFORE INSERT ON t FOR EACH ROW FOLLOWS tr1 SET NEW.price = NEW.price + 0",
        "CREATE EVENT ev ON SCHEDULE EVERY 1 HOUR STARTS '2030-01-01 00:00:00' ENDS '2031-01-01 00:00:00'
         ON COMPLETION PRESERVE DISABLE COMMENT 'hourly' DO UPDATE t SET qty = qty",
        "SET SESSION time_zone = '+02:00'",
        "CREATE EVENT ev2 ON SCHEDULE AT '2030-06-01 12:00:00' DO DELETE FROM p WHERE id < 0",
        "SET SESSION time_zone = '+00:00'",
    ]
}

/// Creates objects with other character sets and collations than the
/// catalog session's, then changes the database's default collation, so
/// routines keep an older database collation.
async fn charset_fixtures(target: &Target, session: &MysqlSession) -> TestResult {
    let latin1 = SessionOverrides {
        character_set_client: Some("latin1".into()),
        collation_connection: Some("latin1_swedish_ci".into()),
        ..SessionOverrides::default()
    };
    session
        .execute_with_session(
            &latin1,
            "CREATE PROCEDURE pl() COMMENT 'latin1' SELECT 'x' AS x",
            &ExecOptions::default(),
        )
        .await
        .map_err(|e| format!("pl: {e:?}"))?;
    // Non-ASCII text under latin1: the driver transcodes the UTF-8 SQL.
    session
        .execute_with_session(
            &latin1,
            "CREATE PROCEDURE pl2() COMMENT 'caf\u{e9}' SELECT 'gr\u{fc}\u{df}e' AS x",
            &ExecOptions::default(),
        )
        .await
        .map_err(|e| format!("pl2: {e:?}"))?;
    // Other single-byte client character sets: Cyrillic under cp1251 and
    // Polish under latin2.
    for (charset, collation, sql) in [
        (
            "cp1251",
            "cp1251_general_ci",
            "CREATE PROCEDURE pc() COMMENT '\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} \u{43c}\u{438}\u{440}' \
             SELECT '\u{417}\u{434}\u{440}\u{430}\u{432}\u{441}\u{442}\u{432}\u{443}\u{439}' AS x",
        ),
        (
            "latin2",
            "latin2_general_ci",
            "CREATE PROCEDURE pg() COMMENT 'Za\u{17c}\u{f3}\u{142}\u{107}' \
             SELECT 'g\u{119}\u{15b}l\u{105}' AS x",
        ),
    ] {
        let overrides = SessionOverrides {
            character_set_client: Some(charset.into()),
            collation_connection: Some(collation.into()),
            ..SessionOverrides::default()
        };
        session
            .execute_with_session(&overrides, sql, &ExecOptions::default())
            .await
            .map_err(|e| format!("{charset}: {e:?}"))?;
    }
    let collation = SessionOverrides {
        collation_connection: Some(
            if target.engine == Engine::Mariadb {
                "utf8mb4_unicode_ci"
            } else {
                "utf8mb4_0900_ai_ci"
            }
            .into(),
        ),
        ..SessionOverrides::default()
    };
    session
        .execute_with_session(
            &collation,
            "CREATE VIEW vc AS SELECT 'a' AS a",
            &ExecOptions::default(),
        )
        .await
        .map_err(|e| format!("vc: {e:?}"))?;
    exec(session, "ALTER DATABASE COLLATE utf8mb4_unicode_ci").await
}

/// Every object of the fixture database, in an order that DDL can be
/// replayed in.
async fn all_objects(
    catalog: &MyCatalog,
    session: &dyn Session,
    db: &str,
) -> Result<Vec<ObjectRef>, Box<dyn Error>> {
    let mut out = vec![obj(ObjectKind::Database, db, None, db)];
    let top = container(db, None);
    let kinds = catalog.kinds(session, &top, false).await?;
    for kind in [
        ObjectKind::Table,
        ObjectKind::Sequence,
        ObjectKind::View,
        ObjectKind::Procedure,
        ObjectKind::Function,
        ObjectKind::Trigger,
        ObjectKind::Event,
        ObjectKind::Package,
        ObjectKind::PackageBody,
    ] {
        if !kinds.iter().any(|k| k.kind == kind) {
            continue;
        }
        let mut objects: Vec<ObjectRef> = catalog
            .list(session, &top, kind, false)
            .await?
            .into_iter()
            .map(|s| s.object)
            .collect();
        if kind == ObjectKind::Table {
            // The referenced table first.
            objects.sort_by_key(|o| o.name != "p");
        }
        if kind == ObjectKind::Trigger {
            // Each trigger's DDL places it before the next one, so they
            // are rebuilt from the last to the first.
            objects.reverse();
        }
        out.extend(objects);
    }
    Ok(out)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn catalog_round_trips_every_kind() -> TestResult {
    for target in targets() {
        let result = run_target(target).await;
        // Clean up also when the run failed.
        if let Ok(admin) = connect(target, "varve").await {
            for suffix in ["", "_ro"] {
                let db = format!("m2_mycat_{}{suffix}", target.tag);
                if suffix == "_ro" && target.engine == Engine::Mysql {
                    let _ = admin
                        .execute(
                            &format!("ALTER DATABASE `{db}` READ ONLY = 0"),
                            &ExecOptions::default(),
                        )
                        .await;
                }
                let _ = admin
                    .execute(
                        &format!("DROP DATABASE IF EXISTS `{db}`"),
                        &ExecOptions::default(),
                    )
                    .await;
            }
        }
        result?;
    }
    Ok(())
}

async fn run_target(target: &Target) -> TestResult {
    let maria = target.engine == Engine::Mariadb;
    let db = format!("m2_mycat_{}", target.tag);
    let admin = connect(target, "varve").await?;
    exec(&admin, &format!("DROP DATABASE IF EXISTS `{db}`")).await?;
    exec(
        &admin,
        &format!("CREATE DATABASE `{db}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"),
    )
    .await?;
    let fixture = connect(target, &db).await?;
    let mut statements = if maria {
        mariadb_fixtures()
    } else {
        mysql_fixtures()
    };
    statements.extend(common_fixtures());
    for sql in statements {
        exec(&fixture, sql).await?;
    }
    charset_fixtures(target, &fixture).await?;

    // The catalog session's default database is not the fixture database.
    let session = connect(target, "varve").await?;
    let catalog = MyCatalog::probe(&session).await?;
    let caps = varve_catalog::Catalog::capabilities(&catalog);
    check(target, caps.engine == target.engine, "engine")?;
    let feature = |f: &str| caps.features.iter().any(|x| x == f);
    check(target, feature("invisibleColumns"), "invisibleColumns")?;
    check(target, feature("checkConstraints"), "checkConstraints")?;
    check(target, feature("setPersist") != maria, "setPersist")?;
    check(
        target,
        feature("invisibleIndexes") != maria,
        "invisibleIndexes",
    )?;
    check(target, feature("ignoredIndexes") == maria, "ignoredIndexes")?;
    check(
        target,
        feature("systemVersioning") == maria,
        "systemVersioning",
    )?;
    check(
        target,
        feature("analyzeFormatJson") == maria,
        "analyzeFormatJson",
    )?;
    check(target, feature("explainAnalyze") != maria, "explainAnalyze")?;
    check(target, !feature("galera"), "galera is off")?;
    let scheduler = catalog.event_scheduler(&session).await?;
    check(
        target,
        feature("eventScheduler") == (scheduler == MyEventScheduler::On),
        "eventScheduler feature follows the scheduler state",
    )?;

    browse_checks(target, &catalog, &session, &db).await?;
    detail_checks(target, &catalog, &session, &db).await?;
    read_only_database(target, &catalog, &session).await?;

    // Read everything, then rebuild the database from DDL alone.
    let objects = all_objects(&catalog, &session, &db).await?;
    check(target, objects.len() >= 13, format!("objects: {objects:?}"))?;
    let mut before = Vec::new();
    for object in &objects {
        let details = normalized(&catalog.describe(&session, object).await?)?;
        let ddl = catalog.ddl(&session, object).await?;
        before.push((object.clone(), details, ddl));
    }
    // Re-running all DDL while the objects exist succeeds and changes
    // nothing (compared once at the end: re-creating the first trigger
    // moves it last until the next one is re-created after it).
    for (_, _, ddl) in &before {
        run_ddl(&session, &db, ddl).await?;
    }
    for (object, details, _) in &before {
        let again = normalized(&catalog.describe(&session, object).await?)?;
        check(
            target,
            &again == details,
            format!("re-running the DDL of {object:?} changed it:\n{details}\n{again}"),
        )?;
    }
    // Re-running one trigger's DDL on its own keeps the trigger order.
    for (object, _, ddl) in before
        .iter()
        .filter(|(o, _, _)| o.kind == ObjectKind::Trigger)
    {
        run_ddl(&session, &db, ddl).await?;
        for (other, details, _) in before
            .iter()
            .filter(|(o, _, _)| o.kind == ObjectKind::Trigger)
        {
            let again = normalized(&catalog.describe(&session, other).await?)?;
            check(
                target,
                &again == details,
                format!("re-running {object:?} alone changed {other:?}:\n{details}\n{again}"),
            )?;
        }
    }
    exec(&admin, &format!("DROP DATABASE `{db}`")).await?;
    for (_, _, ddl) in &before {
        run_ddl(&session, &db, ddl).await?;
    }
    for (object, details, ddl) in &before {
        let after = normalized(&catalog.describe(&session, object).await?)?;
        check(
            target,
            &after == details,
            format!(
                "{object:?} differs after the round trip\nDDL: {ddl}\nbefore: {details}\nafter:  {after}"
            ),
        )?;
        // Replaying the DDL of stored code gives byte-identical DDL (the
        // body included). Table DDL is compared by its details above: MySQL
        // writes CHARACTER SET next to a column COLLATE or not depending on
        // how the column was declared.
        let code = matches!(
            object.kind,
            ObjectKind::View
                | ObjectKind::Procedure
                | ObjectKind::Function
                | ObjectKind::Trigger
                | ObjectKind::Event
                | ObjectKind::Package
                | ObjectKind::PackageBody
        );
        let again = catalog.ddl(&session, object).await?;
        check(
            target,
            !code || &again == ddl,
            format!("{object:?}: DDL changed after the round trip\nbefore: {ddl}\nafter:  {again}"),
        )?;
    }
    charset_checks(target, &before)?;

    child_round_trips(target, &catalog, &session, &db).await?;
    exec(&admin, &format!("DROP DATABASE `{db}`")).await?;
    Ok(())
}

/// The objects created under other character sets keep them.
fn charset_checks(target: &Target, before: &[(ObjectRef, Json, String)]) -> TestResult {
    let find = |name: &str| {
        before
            .iter()
            .find(|(o, _, _)| o.name == name)
            .ok_or_else(|| format!("{}: no object {name}", target.name))
    };
    let (_, pl, pl_ddl) = find("pl")?;
    check(
        target,
        pl["object"]["charsetClient"] == "latin1",
        format!("pl details {pl}"),
    )?;
    let header = ddl_session_header(pl_ddl)?.unwrap_or_default();
    check(
        target,
        header.character_set_client.as_deref() == Some("latin1")
            && header.character_set_results.as_deref() == Some("latin1")
            && header.collation_database.as_deref() == Some("utf8mb4_bin"),
        format!("pl header {header:?} in {pl_ddl}"),
    )?;
    let (_, pl2, pl2_ddl) = find("pl2")?;
    check(
        target,
        pl2["object"]["charsetClient"] == "latin1"
            && pl2["object"]["comment"] == "caf\u{e9}"
            && pl2["object"]["body"]
                .as_str()
                .is_some_and(|b| b.contains("gr\u{fc}\u{df}e"))
            && pl2_ddl.contains("gr\u{fc}\u{df}e"),
        format!("pl2 details {pl2}\n{pl2_ddl}"),
    )?;
    for (name, charset, comment, body) in [
        (
            "pc",
            "cp1251",
            "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} \u{43c}\u{438}\u{440}",
            "\u{417}\u{434}\u{440}\u{430}\u{432}\u{441}\u{442}\u{432}\u{443}\u{439}",
        ),
        (
            "pg",
            "latin2",
            "Za\u{17c}\u{f3}\u{142}\u{107}",
            "g\u{119}\u{15b}l\u{105}",
        ),
    ] {
        let (_, details, ddl) = find(name)?;
        let header = ddl_session_header(ddl)?.unwrap_or_default();
        check(
            target,
            details["object"]["charsetClient"] == charset
                && details["object"]["comment"] == comment
                && details["object"]["body"]
                    .as_str()
                    .is_some_and(|b| b.contains(body))
                && header.character_set_client.as_deref() == Some(charset)
                && ddl.contains(&format!("COMMENT '{comment}'"))
                && ddl.contains(body),
            format!("{name} details {details}\n{ddl}"),
        )?;
    }
    let (_, vc, _) = find("vc")?;
    let expected = if target.engine == Engine::Mariadb {
        "utf8mb4_unicode_ci"
    } else {
        "utf8mb4_0900_ai_ci"
    };
    check(
        target,
        vc["object"]["collationConnection"] == expected,
        format!("vc details {vc}"),
    )?;
    let (_, _, t_ddl) = find("t")?;
    check(
        target,
        ddl_session_header(t_ddl)?.is_none(),
        "tables need no session header",
    )?;
    Ok(())
}

async fn browse_checks(
    target: &Target,
    catalog: &MyCatalog,
    session: &dyn Session,
    db: &str,
) -> TestResult {
    let maria = target.engine == Engine::Mariadb;
    check(
        target,
        catalog.schemas(session, db).await?.is_empty(),
        "no schemas",
    )?;
    let databases = catalog.databases(session).await?;
    check(
        target,
        databases.iter().any(|d| d.object.name == db && !d.system),
        "fixture database listed",
    )?;
    check(
        target,
        databases
            .iter()
            .any(|d| d.object.name == "mysql" && d.system),
        "mysql database is system",
    )?;
    let server_kinds = catalog.kinds(session, &Container::default(), false).await?;
    check(
        target,
        server_kinds
            .iter()
            .any(|k| k.kind == ObjectKind::Database && k.count >= 1),
        "database count",
    )?;
    let listed = catalog
        .list(session, &Container::default(), ObjectKind::Database, false)
        .await?;
    check(
        target,
        listed.iter().all(|d| !d.system),
        "system databases hidden",
    )?;
    let mysql_db = container("mysql", None);
    check(
        target,
        catalog
            .list(session, &mysql_db, ObjectKind::Table, false)
            .await?
            .is_empty(),
        "system tables hidden",
    )?;
    let system_tables = catalog
        .list(session, &mysql_db, ObjectKind::Table, true)
        .await?;
    check(
        target,
        !system_tables.is_empty() && system_tables.iter().all(|t| t.system),
        "system tables shown on request",
    )?;

    let top = container(db, None);
    let kinds = catalog.kinds(session, &top, false).await?;
    let count = |kind: ObjectKind| kinds.iter().find(|k| k.kind == kind).map_or(0, |k| k.count);
    check(
        target,
        count(ObjectKind::Table) == if maria { 7 } else { 5 },
        format!("table count {kinds:?}"),
    )?;
    check(target, count(ObjectKind::View) == 2, "view count")?;
    check(target, count(ObjectKind::Procedure) == 5, "procedure count")?;
    check(target, count(ObjectKind::Function) == 1, "function count")?;
    check(target, count(ObjectKind::Trigger) == 2, "trigger count")?;
    check(target, count(ObjectKind::Event) == 2, "event count")?;
    check(
        target,
        count(ObjectKind::Sequence) == u64::from(maria),
        "sequence count",
    )?;
    check(
        target,
        count(ObjectKind::Package) == u64::from(maria)
            && count(ObjectKind::PackageBody) == u64::from(maria),
        "package counts",
    )?;

    let t = obj(ObjectKind::Table, db, None, "t");
    let table_kinds = catalog
        .kinds(session, &container(db, Some(t.clone())), false)
        .await?;
    let tcount = |kind: ObjectKind| {
        table_kinds
            .iter()
            .find(|k| k.kind == kind)
            .map_or(0, |k| k.count)
    };
    check(target, tcount(ObjectKind::Column) == 13, "column count")?;
    check(
        target,
        tcount(ObjectKind::Trigger) == 2,
        "table trigger count",
    )?;
    check(target, tcount(ObjectKind::Index) >= 7, "index count")?;
    let tables = catalog
        .list(session, &top, ObjectKind::Table, false)
        .await?;
    let table_t = tables
        .iter()
        .find(|s| s.object.name == "t")
        .ok_or("table t not listed")?;
    check(
        target,
        table_t.comment.as_deref() == Some("main table"),
        "table comment",
    )?;
    // Row estimate and size come with the list; views have neither.
    check(
        target,
        table_t.row_estimate.is_some() && table_t.size_bytes.is_some_and(|b| b > 0),
        format!(
            "table t estimate {:?}, size {:?}",
            table_t.row_estimate, table_t.size_bytes
        ),
    )?;
    let views = catalog.list(session, &top, ObjectKind::View, false).await?;
    check(
        target,
        !views.is_empty()
            && views
                .iter()
                .all(|v| v.row_estimate.is_none() && v.size_bytes.is_none()),
        format!("view estimates {views:?}"),
    )?;
    let pt_summary = tables.iter().find(|s| s.object.name == "pt");
    // A partitioned table carries the server's sums over its partitions.
    check(
        target,
        pt_summary.is_some_and(|s| s.row_estimate.is_some() && s.size_bytes.is_some()),
        format!("pt estimate and size {pt_summary:?}"),
    )?;
    let pt_flags = pt_summary.map(|s| s.flags.clone()).unwrap_or_default();
    check(
        target,
        pt_flags.contains(&"partitioned".to_owned()),
        "pt partitioned",
    )?;

    let pt = obj(ObjectKind::Table, db, None, "pt");
    let partitions = catalog
        .list(
            session,
            &container(db, Some(pt.clone())),
            ObjectKind::Partition,
            false,
        )
        .await?;
    let names: Vec<&str> = partitions.iter().map(|p| p.object.name.as_str()).collect();
    check(
        target,
        names == ["p2020", "p2021", "pmax"],
        format!("partitions {names:?}"),
    )?;
    check(
        target,
        partitions.iter().all(|p| p.has_children),
        "subpartitions",
    )?;
    let p2020 = obj(ObjectKind::Partition, db, Some("pt"), "p2020");
    let subs = catalog
        .list(
            session,
            &container(db, Some(p2020)),
            ObjectKind::Partition,
            false,
        )
        .await?;
    check(target, subs.len() == 2, format!("subpartitions {subs:?}"))?;
    let missing = catalog
        .describe(session, &obj(ObjectKind::Table, db, None, "nope"))
        .await;
    check(
        target,
        matches!(missing, Err(CatalogError::NotFound { .. })),
        format!("missing table: {missing:?}"),
    )?;
    let missing = catalog
        .ddl(session, &obj(ObjectKind::Procedure, db, None, "nope"))
        .await;
    check(
        target,
        matches!(missing, Err(CatalogError::NotFound { .. })),
        format!("missing procedure DDL: {missing:?}"),
    )?;
    Ok(())
}

async fn detail_checks(
    target: &Target,
    catalog: &MyCatalog,
    session: &dyn Session,
    db: &str,
) -> TestResult {
    let maria = target.engine == Engine::Mariadb;
    let MyObject::Table(t) = my(catalog
        .describe(session, &obj(ObjectKind::Table, db, None, "t"))
        .await?)?
    else {
        return Err("t is not a table".into());
    };
    check(target, t.engine.as_deref() == Some("InnoDB"), "engine")?;
    check(
        target,
        t.row_format.as_deref() == Some("Dynamic"),
        "row format",
    )?;
    check(
        target,
        t.collation.as_deref() == Some("utf8mb4_bin"),
        "collation",
    )?;
    check(target, t.charset.as_deref() == Some("utf8mb4"), "charset")?;
    check(target, t.triggers == ["tr1", "tr2"], "trigger names")?;
    let column = |name: &str| {
        t.columns
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| format!("{}: no column {name}", target.name))
    };
    check(
        target,
        column("code")?.default == Some(MyDefault::Literal("a'b\\c".into())),
        format!("code default {:?}", column("code")?.default),
    )?;
    check(
        target,
        column("code")?.comment.as_deref() == Some("code, it's"),
        "code comment",
    )?;
    check(target, column("note")?.invisible, "invisible column")?;
    check(
        target,
        column("note")?.collation.as_deref() == Some("utf8mb4_bin"),
        "column collation",
    )?;
    check(target, column("id")?.auto_increment, "auto increment")?;
    check(
        target,
        column("qty")?.default == Some(MyDefault::Null),
        "null default",
    )?;
    check(
        target,
        column("price")?.default == Some(MyDefault::Literal("1.50".into())),
        format!("price default {:?}", column("price")?.default),
    )?;
    check(
        target,
        matches!(column("made")?.default, Some(MyDefault::Expression(_))),
        "expression default",
    )?;
    check(
        target,
        column("changed")?
            .on_update
            .as_deref()
            .is_some_and(|u| u.to_ascii_uppercase().starts_with("CURRENT_TIMESTAMP")),
        "on update",
    )?;
    check(
        target,
        column("doubled")?.generated.as_ref().is_some_and(|g| {
            g.generated_kind == MyGeneratedKind::Virtual && g.expression.is_some()
        }),
        "virtual column",
    )?;
    check(
        target,
        column("tripled")?
            .generated
            .as_ref()
            .is_some_and(|g| g.generated_kind == MyGeneratedKind::Stored),
        "stored column",
    )?;
    check(target, column("doc")?.json, "json column")?;
    check(target, column("doc")?.check.is_none(), "json check folded")?;
    check(target, column("place")?.srid == Some(4326), "srid")?;
    if maria {
        check(
            target,
            column("qty")?.check.as_deref() == Some("`qty` < 100000"),
            format!("column check {:?}", column("qty")?.check),
        )?;
    }
    let index = |name: &str| {
        t.indexes
            .iter()
            .find(|i| i.name == name)
            .ok_or_else(|| format!("{}: no index {name}", target.name))
    };
    check(
        target,
        index("PRIMARY")?.index_kind == MyIndexKind::Primary,
        "primary",
    )?;
    check(
        target,
        index("uq_code")?.index_kind == MyIndexKind::Unique,
        "unique",
    )?;
    check(
        target,
        index("ft_body")?.index_kind == MyIndexKind::Fulltext,
        "fulltext",
    )?;
    check(
        target,
        index("sp_place")?.index_kind == MyIndexKind::Spatial,
        "spatial",
    )?;
    let ix = index("ix_code_qty")?;
    check(
        target,
        ix.parts.len() == 2
            && ix.parts[0].prefix_length == Some(5)
            && ix.comment.as_deref() == Some("ix"),
        format!("prefix index {ix:?}"),
    )?;
    // MariaDB 10.6 parses DESC but ignores it; 10.8+ and MySQL keep it.
    let keeps_desc = !maria || target.tag != "d106";
    check(
        target,
        ix.parts[0].descending == keeps_desc,
        "descending part",
    )?;
    let hidden = index("ix_hidden")?;
    check(
        target,
        hidden.invisible != maria && hidden.ignored == maria,
        "invisible vs ignored index",
    )?;
    if !maria {
        let func = index("ix_func")?;
        check(
            target,
            func.parts[0].column.is_none() && func.parts[0].expression.is_some(),
            "functional index",
        )?;
    }
    let constraint = |name: &str| {
        t.constraints
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| format!("{}: no constraint {name}", target.name))
    };
    let fk = constraint("fk_parent")?;
    let reference = fk.references.as_ref().ok_or("fk without reference")?;
    check(
        target,
        fk.constraint_type == MyConstraintType::ForeignKey
            && fk.columns == ["parent_id"]
            && reference.table == "p"
            && reference.database == db
            && reference.columns == ["id"]
            && reference.on_delete.as_deref() == Some("SET NULL")
            && reference.on_update.as_deref() == Some("CASCADE"),
        format!("foreign key {fk:?}"),
    )?;
    let ck = constraint("ck_qty")?;
    check(
        target,
        ck.constraint_type == MyConstraintType::Check && ck.check_clause.is_some(),
        "check",
    )?;
    if maria {
        check(target, constraint("doc")?.json, "json alias check")?;
    } else {
        check(
            target,
            constraint("ck_soft")?.enforced == Some(false) && ck.enforced == Some(true),
            "enforced flag",
        )?;
    }
    check(
        target,
        constraint("PRIMARY")?.constraint_type == MyConstraintType::PrimaryKey,
        "pk constraint",
    )?;

    let MyObject::Table(pt) = my(catalog
        .describe(session, &obj(ObjectKind::Table, db, None, "pt"))
        .await?)?
    else {
        return Err("pt is not a table".into());
    };
    let partitioning = pt.partitioning.ok_or("pt has no partitioning")?;
    check(
        target,
        partitioning.method == "RANGE"
            && partitioning.subpartition_method.as_deref() == Some("HASH")
            && partitioning.partitions.len() == 3
            && partitioning.partitions[0].values.as_deref() == Some("VALUES LESS THAN (2021)")
            && partitioning.partitions[0].comment.as_deref() == Some("old")
            && partitioning.partitions[0].subpartitions.len() == 2,
        format!("range partitioning {partitioning:?}"),
    )?;
    let MyObject::Table(kt) = my(catalog
        .describe(session, &obj(ObjectKind::Table, db, None, "kt"))
        .await?)?
    else {
        return Err("kt is not a table".into());
    };
    let kp = kt.partitioning.ok_or("kt has no partitioning")?;
    check(
        target,
        kp.linear && kp.partitions.len() == 3,
        format!("linear key {kp:?}"),
    )?;

    let MyObject::View(v) = my(catalog
        .describe(session, &obj(ObjectKind::View, db, None, "v"))
        .await?)?
    else {
        return Err("v is not a view".into());
    };
    check(
        target,
        v.algorithm.as_deref() == Some("MERGE")
            && v.security.as_deref() == Some("INVOKER")
            && v.check_option.as_deref() == Some("CASCADED")
            && v.updatable == Some(true)
            && v.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                == ["vid", "vcode"],
        format!("view {v:?}"),
    )?;

    let MyObject::Procedure(pr) = my(catalog
        .describe(session, &obj(ObjectKind::Procedure, db, None, "pr"))
        .await?)?
    else {
        return Err("pr is not a procedure".into());
    };
    let modes: Vec<Option<&str>> = pr.parameters.iter().map(|p| p.mode.as_deref()).collect();
    check(
        target,
        modes == [Some("IN"), Some("OUT"), Some("INOUT")]
            && pr.data_access.as_deref() == Some("READS SQL DATA")
            && pr.comment.as_deref() == Some("proc")
            && pr
                .body
                .as_deref()
                .is_some_and(|b| b.contains("SET c = c + a")),
        format!("procedure {pr:?}"),
    )?;
    let MyObject::Function(f) = my(catalog
        .describe(session, &obj(ObjectKind::Function, db, None, "fn"))
        .await?)?
    else {
        return Err("fn is not a function".into());
    };
    check(
        target,
        f.deterministic
            && f.security.as_deref() == Some("INVOKER")
            && f.returns.as_deref().is_some_and(|r| r.starts_with("int"))
            && f.parameters.len() == 1,
        format!("function {f:?}"),
    )?;

    let MyObject::Trigger(tr2) = my(catalog
        .describe(session, &obj(ObjectKind::Trigger, db, Some("t"), "tr2"))
        .await?)?
    else {
        return Err("tr2 is not a trigger".into());
    };
    check(
        target,
        tr2.action_order == 2
            && tr2.follows.as_deref() == Some("tr1")
            && tr2.timing == "BEFORE"
            && tr2.event == "INSERT",
        format!("trigger {tr2:?}"),
    )?;

    let MyObject::Event(ev) = my(catalog
        .describe(session, &obj(ObjectKind::Event, db, None, "ev"))
        .await?)?
    else {
        return Err("ev is not an event".into());
    };
    check(
        target,
        ev.status == varve_catalog::mysql::MyEventStatus::Disabled
            && ev.preserve
            && ev.comment.as_deref() == Some("hourly")
            && matches!(
                &ev.schedule,
                varve_catalog::mysql::MySchedule::Every { interval_field: Some(f), ends: Some(_), .. } if f == "HOUR"
            )
            && ev.scheduler == catalog.event_scheduler(session).await?,
        format!("event {ev:?}"),
    )?;
    let ev2_ddl = catalog
        .ddl(session, &obj(ObjectKind::Event, db, None, "ev2"))
        .await?;
    check(
        target,
        ddl_session_header(&ev2_ddl)?
            .and_then(|h| h.time_zone)
            .as_deref()
            == Some("+02:00"),
        format!("event time zone header: {ev2_ddl}"),
    )?;

    if maria {
        let MyObject::Table(sv) = my(catalog
            .describe(session, &obj(ObjectKind::Table, db, None, "sv"))
            .await?)?
        else {
            return Err("sv is not a table".into());
        };
        let versioning = sv.system_versioning.clone().ok_or("sv not versioned")?;
        check(
            target,
            versioning.row_start.as_deref() == Some("rs")
                && versioning.row_end.as_deref() == Some("re")
                && sv.columns.iter().any(|c| {
                    c.name == "rs"
                        && c.invisible
                        && c.generated.as_ref().map(|g| g.generated_kind)
                            == Some(MyGeneratedKind::RowStart)
                })
                && sv.partitioning.as_ref().is_some_and(|p| {
                    p.method == "SYSTEM_TIME"
                        && p.partitions
                            .iter()
                            .map(|x| x.values.clone())
                            .collect::<Vec<_>>()
                            == [Some("HISTORY".into()), Some("CURRENT".into())]
                }),
            format!("system versioned table {sv:?}"),
        )?;
        let summaries = catalog
            .list(session, &container(db, None), ObjectKind::Table, false)
            .await?;
        check(
            target,
            summaries
                .iter()
                .any(|s| s.object.name == "sv" && s.flags.contains(&"system versioned".to_owned())),
            "system versioned flag",
        )?;
        let MyObject::Table(ap) = my(catalog
            .describe(session, &obj(ObjectKind::Table, db, None, "ap"))
            .await?)?
        else {
            return Err("ap is not a table".into());
        };
        check(
            target,
            ap.periods.len() == 1
                && ap.periods[0].name == "valid"
                && ap.periods[0].start_column == "ds",
            format!("periods {:?}", ap.periods),
        )?;
        let MyObject::Sequence(sq) = my(catalog
            .describe(session, &obj(ObjectKind::Sequence, db, None, "sq"))
            .await?)?
        else {
            return Err("sq is not a sequence".into());
        };
        check(
            target,
            sq.start == "100"
                && sq.increment == "5"
                && sq.maximum == "100000"
                && sq.cache == "10"
                && sq.cycle
                && sq.next_value == "100"
                && sq.comment.as_deref() == Some("seq"),
            format!("sequence {sq:?}"),
        )?;
        let MyObject::PackageBody(body) = my(catalog
            .describe(session, &obj(ObjectKind::PackageBody, db, None, "pk"))
            .await?)?
        else {
            return Err("pk body is not a package body".into());
        };
        check(
            target,
            body.sql_mode
                .as_deref()
                .is_some_and(|m| m.contains("ORACLE"))
                && body
                    .body
                    .as_deref()
                    .is_some_and(|b| b.contains("RETURN a + 1")),
            format!("package body {body:?}"),
        )?;
        let pk_ddl = catalog
            .ddl(session, &obj(ObjectKind::Package, db, None, "pk"))
            .await?;
        check(
            target,
            pk_ddl.contains("CREATE OR REPLACE")
                && ddl_session_header(&pk_ddl)?
                    .and_then(|h| h.sql_mode)
                    .is_some_and(|m| m.contains("ORACLE")),
            format!("package DDL: {pk_ddl}"),
        )?;
    }
    Ok(())
}

async fn read_only_database(
    target: &Target,
    catalog: &MyCatalog,
    session: &MysqlSession,
) -> TestResult {
    if target.engine != Engine::Mysql {
        return Ok(());
    }
    let db = format!("m2_mycat_{}_ro", target.tag);
    exec(session, &format!("DROP DATABASE IF EXISTS `{db}`")).await?;
    exec(
        session,
        &format!("CREATE DATABASE `{db}` DEFAULT ENCRYPTION = 'N'"),
    )
    .await?;
    exec(session, &format!("ALTER DATABASE `{db}` READ ONLY = 1")).await?;
    let object = obj(ObjectKind::Database, &db, None, &db);
    let MyObject::Database(before) = my(catalog.describe(session, &object).await?)? else {
        return Err("not a database".into());
    };
    check(
        target,
        before.read_only == Some(true) && before.encryption == Some(false),
        format!("read only database {before:?}"),
    )?;
    let ddl = catalog.ddl(session, &object).await?;
    check(target, ddl.contains("READ ONLY = 1"), format!("DDL {ddl}"))?;
    exec(session, &format!("ALTER DATABASE `{db}` READ ONLY = 0")).await?;
    exec(session, &format!("DROP DATABASE `{db}`")).await?;
    run_ddl(session, &db, &ddl).await?;
    let MyObject::Database(after) = my(catalog.describe(session, &object).await?)? else {
        return Err("not a database".into());
    };
    check(target, after == before, format!("{before:?} != {after:?}"))?;
    exec(session, &format!("ALTER DATABASE `{db}` READ ONLY = 0")).await?;
    exec(session, &format!("DROP DATABASE `{db}`")).await?;
    Ok(())
}

/// Drops columns, indexes, constraints and partitions one at a time and
/// re-adds them from their DDL.
async fn child_round_trips(
    target: &Target,
    catalog: &MyCatalog,
    session: &MysqlSession,
    db: &str,
) -> TestResult {
    let maria = target.engine == Engine::Mariadb;
    let mut cases: Vec<(Vec<ObjectRef>, String)> = vec![
        (
            vec![obj(ObjectKind::Column, db, Some("t"), "note")],
            format!("ALTER TABLE `{db}`.`t` DROP COLUMN `note`"),
        ),
        (
            vec![
                obj(ObjectKind::Column, db, Some("t"), "place"),
                obj(ObjectKind::Index, db, Some("t"), "sp_place"),
            ],
            format!("ALTER TABLE `{db}`.`t` DROP INDEX `sp_place`, DROP COLUMN `place`"),
        ),
        (
            vec![obj(ObjectKind::Index, db, Some("t"), "ix_code_qty")],
            format!("ALTER TABLE `{db}`.`t` DROP INDEX `ix_code_qty`"),
        ),
        (
            vec![obj(ObjectKind::Index, db, Some("t"), "ix_hidden")],
            format!("ALTER TABLE `{db}`.`t` DROP INDEX `ix_hidden`"),
        ),
        (
            vec![obj(ObjectKind::Index, db, Some("t"), "ft_body")],
            format!("ALTER TABLE `{db}`.`t` DROP INDEX `ft_body`"),
        ),
        (
            vec![obj(ObjectKind::Constraint, db, Some("t"), "fk_parent")],
            format!("ALTER TABLE `{db}`.`t` DROP FOREIGN KEY `fk_parent`"),
        ),
        (
            vec![obj(ObjectKind::Constraint, db, Some("t"), "ck_qty")],
            format!("ALTER TABLE `{db}`.`t` DROP CONSTRAINT `ck_qty`"),
        ),
        (
            vec![obj(ObjectKind::Constraint, db, Some("t"), "uq_code")],
            format!("ALTER TABLE `{db}`.`t` DROP INDEX `uq_code`"),
        ),
        (
            vec![obj(ObjectKind::Constraint, db, Some("p"), "PRIMARY")],
            format!("ALTER TABLE `{db}`.`p` DROP PRIMARY KEY"),
        ),
        (
            vec![obj(ObjectKind::Partition, db, Some("lt"), "pb")],
            format!("ALTER TABLE `{db}`.`lt` DROP PARTITION `pb`"),
        ),
        (
            vec![obj(ObjectKind::Partition, db, Some("pt"), "pmax")],
            format!("ALTER TABLE `{db}`.`pt` DROP PARTITION `pmax`"),
        ),
        // Partitions that are not last go back into their place.
        (
            vec![obj(ObjectKind::Partition, db, Some("pt"), "p2021")],
            format!("ALTER TABLE `{db}`.`pt` DROP PARTITION `p2021`"),
        ),
        (
            vec![obj(ObjectKind::Partition, db, Some("lt"), "pa")],
            format!("ALTER TABLE `{db}`.`lt` DROP PARTITION `pa`"),
        ),
    ];
    if maria {
        cases.push((
            vec![obj(ObjectKind::Constraint, db, Some("t"), "qty")],
            // A column-level check cannot be dropped by name; redefining
            // the column without it removes it.
            format!("ALTER TABLE `{db}`.`t` MODIFY COLUMN `qty` INT DEFAULT NULL"),
        ));
        // A column of a system-versioned table.
        cases.push((
            vec![obj(ObjectKind::Column, db, Some("sv"), "val")],
            format!(
                "SET STATEMENT system_versioning_alter_history = KEEP FOR \
                 ALTER TABLE `{db}`.`sv` DROP COLUMN `val`"
            ),
        ));
        // The check an application-time period adds is the period.
        cases.push((
            vec![
                obj(ObjectKind::Constraint, db, Some("ap"), "valid"),
                obj(ObjectKind::Index, db, Some("ap"), "uk"),
            ],
            format!("ALTER TABLE `{db}`.`ap` DROP INDEX `uk`, DROP PERIOD FOR `valid`"),
        ));
    } else {
        cases.push((
            vec![obj(ObjectKind::Index, db, Some("t"), "ix_func")],
            format!("ALTER TABLE `{db}`.`t` DROP INDEX `ix_func`"),
        ));
        cases.push((
            vec![obj(ObjectKind::Constraint, db, Some("t"), "ck_soft")],
            format!("ALTER TABLE `{db}`.`t` DROP CONSTRAINT `ck_soft`"),
        ));
    }
    let table_of = |object: &ObjectRef| {
        obj(
            ObjectKind::Table,
            db,
            None,
            object.parent.as_deref().unwrap_or_default(),
        )
    };
    for (objects, drop) in cases {
        let object = objects.first().cloned().ok_or("empty case")?;
        // The primary key of p is referenced by t's foreign key.
        if object.name == "PRIMARY" {
            exec(
                session,
                &format!("ALTER TABLE `{db}`.`t` DROP FOREIGN KEY `fk_parent`"),
            )
            .await?;
        }
        let table = table_of(&object);
        let before = normalized(&catalog.describe(session, &table).await?)?;
        let mut statements = Vec::new();
        for child in &objects {
            statements.push(catalog.ddl(session, child).await?);
        }
        let ddl = statements.join(";\n");
        // MariaDB child DDL is re-runnable, except a primary key (which
        // has no IF NOT EXISTS).
        // A REORGANIZE PARTITION runs only while the partition is absent.
        let rerunnable = maria && object.name != "PRIMARY" && !ddl.contains("REORGANIZE");
        if rerunnable {
            run_ddl(session, db, &ddl).await?;
        }
        exec(session, &drop).await?;
        run_ddl(session, db, &ddl).await?;
        if rerunnable {
            run_ddl(session, db, &ddl).await?;
        }
        let after = normalized(&catalog.describe(session, &table).await?)?;
        check(
            target,
            after == before,
            format!(
                "{object:?} differs after drop and re-add\nDDL: {ddl}\nbefore: {before}\nafter:  {after}"
            ),
        )?;
        if object.name == "PRIMARY" {
            exec(
                session,
                &format!(
                    "ALTER TABLE `{db}`.`t` ADD CONSTRAINT `fk_parent` FOREIGN KEY (`parent_id`) \
                     REFERENCES `p` (`id`) ON DELETE SET NULL ON UPDATE CASCADE"
                ),
            )
            .await?;
        }
    }
    Ok(())
}

/// The trigger list of a table: name, events and order, in server order.
async fn trigger_order(
    session: &dyn Session,
    db: &str,
    table: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let output = session
        .execute(
            &format!(
                "SELECT CONCAT(TRIGGER_NAME, ' ', ACTION_TIMING, ' ', EVENT_MANIPULATION, ' ', ACTION_ORDER) \
                 FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = '{db}' AND EVENT_OBJECT_TABLE = '{table}' \
                 ORDER BY ACTION_TIMING, EVENT_MANIPULATION, ACTION_ORDER, TRIGGER_NAME"
            ),
            &ExecOptions::default(),
        )
        .await?;
    let mut out = Vec::new();
    for result in output.results {
        if let StatementResult::Rows { rows, .. } = result {
            for row in rows {
                out.extend(row.first().and_then(varve_driver::Value::to_plain_text));
            }
        }
    }
    Ok(out)
}

/// Triggers for several events (MariaDB 12) and triggers placed with
/// FOLLOWS / PRECEDES keep their order when any one of them is re-created
/// from its DDL, and when all are rebuilt from scratch.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn trigger_order_survives_ddl() -> TestResult {
    for target in targets() {
        let db = format!("m2_mycat_{}_trg", target.tag);
        let session = connect(target, "varve").await?;
        let result = trigger_order_target(target, &session, &db).await;
        let _ = session
            .execute(
                &format!("DROP DATABASE IF EXISTS `{db}`"),
                &ExecOptions::default(),
            )
            .await;
        result?;
    }
    Ok(())
}

async fn trigger_order_target(target: &Target, session: &MysqlSession, db: &str) -> TestResult {
    exec(session, &format!("DROP DATABASE IF EXISTS `{db}`")).await?;
    exec(session, &format!("CREATE DATABASE `{db}`")).await?;
    let mut statements = vec![
        format!("CREATE TABLE `{db}`.mt (a INT)"),
        format!("CREATE TRIGGER `{db}`.m1 BEFORE INSERT ON `{db}`.mt FOR EACH ROW SET NEW.a = 1"),
        format!("CREATE TRIGGER `{db}`.m3 BEFORE UPDATE ON `{db}`.mt FOR EACH ROW SET NEW.a = 3"),
        format!(
            "CREATE TRIGGER `{db}`.m4 BEFORE INSERT ON `{db}`.mt FOR EACH ROW PRECEDES m1 SET NEW.a = 4"
        ),
        format!(
            "CREATE TRIGGER `{db}`.m5 BEFORE INSERT ON `{db}`.mt FOR EACH ROW FOLLOWS m4 SET NEW.a = 5"
        ),
    ];
    if target.tag == "d123" {
        // A trigger for two events (last in both lists), and one placed
        // before it.
        statements.push(format!(
            "CREATE TRIGGER `{db}`.m2 BEFORE INSERT OR UPDATE ON `{db}`.mt FOR EACH ROW SET NEW.a = 2"
        ));
        statements.push(format!(
            "CREATE TRIGGER `{db}`.m0 BEFORE UPDATE ON `{db}`.mt FOR EACH ROW PRECEDES m2 SET NEW.a = 0"
        ));
    }
    for sql in &statements {
        exec(session, sql).await?;
    }
    let catalog = MyCatalog::probe(session).await?;
    let original = trigger_order(session, db, "mt").await?;
    let triggers = catalog
        .list(session, &container(db, None), ObjectKind::Trigger, false)
        .await?;
    let expected = if target.tag == "d123" { 6 } else { 4 };
    check(
        target,
        triggers.len() == expected && original.len() >= expected,
        format!("triggers {original:?}"),
    )?;
    let mut ddl = Vec::new();
    for t in &triggers {
        ddl.push(catalog.ddl(session, &t.object).await?);
    }
    for (t, text) in triggers.iter().zip(&ddl) {
        run_ddl(session, db, text).await?;
        let now = trigger_order(session, db, "mt").await?;
        check(
            target,
            now == original,
            format!(
                "re-creating {} moved triggers:\n{original:?}\n{now:?}\n{text}",
                t.object.name
            ),
        )?;
    }
    for t in &triggers {
        exec(session, &format!("DROP TRIGGER `{db}`.`{}`", t.object.name)).await?;
    }
    for text in ddl.iter().rev() {
        run_ddl(session, db, text).await?;
    }
    let rebuilt = trigger_order(session, db, "mt").await?;
    check(
        target,
        rebuilt == original,
        format!("rebuilt trigger order:\n{original:?}\n{rebuilt:?}"),
    )?;
    Ok(())
}

/// A database with 10,000 tables: browsing and describing stay fast
/// because every catalog query filters each information_schema table it
/// reads by database and table.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn catalog_scales_to_10000_tables() -> TestResult {
    for target in targets() {
        let db = format!("m2_mycat_{}_big", target.tag);
        let session = connect(target, "varve").await?;
        let result = big_target(target, &session, &db).await;
        let _ = session
            .execute(
                &format!("DROP DATABASE IF EXISTS `{db}`"),
                &ExecOptions::default(),
            )
            .await;
        result?;
    }
    Ok(())
}

async fn big_target(target: &Target, session: &MysqlSession, db: &str) -> TestResult {
    exec(session, &format!("DROP DATABASE IF EXISTS `{db}`")).await?;
    exec(session, &format!("CREATE DATABASE `{db}`")).await?;
    let mut batch = Vec::new();
    for i in 0..10_000 {
        batch.push(format!(
            "CREATE TABLE `{db}`.t{i:05} (id INT PRIMARY KEY, v VARCHAR(20), KEY (v))"
        ));
        if batch.len() == 500 {
            exec(session, &batch.join(";\n")).await?;
            batch.clear();
        }
    }
    exec(
        session,
        &format!(
            "CREATE TABLE `{db}`.c (id INT PRIMARY KEY, p INT, CHECK (p > 0), \
             CONSTRAINT fk FOREIGN KEY (p) REFERENCES `{db}`.t00001 (id))"
        ),
    )
    .await?;
    let catalog = MyCatalog::probe(session).await?;
    let table = obj(ObjectKind::Table, db, None, "c");
    // Warm the caches once, then measure.
    catalog.describe(session, &table).await?;
    let mut best = std::time::Duration::MAX;
    for _ in 0..3 {
        let start = std::time::Instant::now();
        let details = my(catalog.describe(session, &table).await?)?;
        best = best.min(start.elapsed());
        let MyObject::Table(t) = details else {
            return Err("c is not a table".into());
        };
        check(
            target,
            t.constraints.len() == 3,
            format!("constraints {:?}", t.constraints),
        )?;
    }
    eprintln!(
        "{}: describe of one table among 10,000 took {best:?}",
        target.name
    );
    // The target is 50 ms; the bound leaves room for a busy machine.
    check(
        target,
        best < std::time::Duration::from_millis(400),
        format!("describe took {best:?}"),
    )?;
    let start = std::time::Instant::now();
    let columns = catalog
        .list(
            session,
            &container(db, Some(table.clone())),
            ObjectKind::Column,
            false,
        )
        .await?;
    let constraints = catalog
        .list(
            session,
            &container(db, Some(table.clone())),
            ObjectKind::Constraint,
            false,
        )
        .await?;
    let elapsed = start.elapsed();
    eprintln!(
        "{}: columns and constraints of one table took {elapsed:?}",
        target.name
    );
    check(
        target,
        columns.len() == 2
            && constraints.len() == 3
            && elapsed < std::time::Duration::from_millis(400),
        format!("child lists took {elapsed:?}"),
    )?;
    Ok(())
}
