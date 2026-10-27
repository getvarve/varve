//! Round trips of the MySQL and MariaDB editors against the M2 database
//! matrix: every plan runs on the server and the result is read back from
//! `information_schema` and `SHOW CREATE` output.
//!
//! The servers come from the shared M2 compose project. Endpoints come
//! from environment variables: `VARVE_M2_MYSQL_HOST` (default `127.0.0.1`),
//! `VARVE_M2_MYSQL_USER` (`root`), `VARVE_M2_MYSQL_PASSWORD` (`varve_dev`)
//! and one port per server: `VARVE_M2_MYSQL_8_0_PORT` (35080),
//! `VARVE_M2_MYSQL_9_7_PORT` (35097), `VARVE_M2_MARIADB_10_6_PORT` (35106),
//! `VARVE_M2_MARIADB_12_3_PORT` (35123). Every test works in its own
//! `m2_myedit_*` database, created fresh and dropped at the end.
//!
//! Run with `cargo test -p varve-catalog --test mysql_edit -- --include-ignored`.

use std::error::Error;

use serde_json::{Value as Json, json};
use varve_catalog::EditPlan;
use varve_catalog::mysql::edit::{MyChange, plan};
use varve_core::Engine;
use varve_driver::{
    ConnectParams, Dialect, ExecOptions, Session, StatementResult, TlsMode, split_statements,
};
use varve_driver_mysql::{MysqlDriver, MysqlSession};
use varve_secrets::Secret;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[derive(Debug, Clone, Copy)]
struct Target {
    name: &'static str,
    engine: Engine,
    port: u16,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
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

/// The servers to test (see [`selected`]), never none.
fn targets() -> Vec<Target> {
    let targets: Vec<Target> = every_target()
        .into_iter()
        .filter(|target| selected(&target.name.to_lowercase().replace(' ', "-")))
        .collect();
    assert!(
        !targets.is_empty(),
        "VARVE_ONLY_SERVERS selects no MySQL or MariaDB server"
    );
    targets
}

fn every_target() -> Vec<Target> {
    let port = |name: &str, default: u16| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    vec![
        Target {
            name: "MySQL 8.0",
            engine: Engine::Mysql,
            port: port("VARVE_M2_MYSQL_8_0_PORT", 35080),
        },
        Target {
            name: "MySQL 9.7",
            engine: Engine::Mysql,
            port: port("VARVE_M2_MYSQL_9_7_PORT", 35097),
        },
        Target {
            name: "MariaDB 10.6",
            engine: Engine::Mariadb,
            port: port("VARVE_M2_MARIADB_10_6_PORT", 35106),
        },
        Target {
            name: "MariaDB 12.3",
            engine: Engine::Mariadb,
            port: port("VARVE_M2_MARIADB_12_3_PORT", 35123),
        },
    ]
}

async fn connect(target: &Target) -> TestResult<MysqlSession> {
    let mut params = ConnectParams::new(
        target.engine,
        env_or("VARVE_M2_MYSQL_HOST", "127.0.0.1"),
        target.port,
    );
    params.username = Some(env_or("VARVE_M2_MYSQL_USER", "root"));
    params.password = Some(Secret::new(env_or("VARVE_M2_MYSQL_PASSWORD", "varve_dev")));
    params.tls.mode = TlsMode::Disable;
    params.application_name = "varve-catalog-tests".into();
    MysqlDriver::new()
        .open(&params)
        .await
        .map_err(|error| format!("{}: {error}", target.name).into())
}

/// Runs one statement.
async fn exec(s: &MysqlSession, sql: &str) -> TestResult {
    s.execute(sql, &ExecOptions::default())
        .await
        .map_err(|error| format!("{sql}\n  failed: {error}"))?;
    Ok(())
}

/// The first result set, every value as text.
async fn rows(s: &MysqlSession, sql: &str) -> TestResult<Vec<Vec<Option<String>>>> {
    let output = s
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|error| format!("{sql}\n  failed: {error}"))?;
    for result in output.results {
        if let StatementResult::Rows { rows, .. } = result {
            return Ok(rows
                .into_iter()
                .map(|row| row.into_iter().map(|v| v.to_plain_text()).collect())
                .collect());
        }
    }
    Err(format!("{sql}: no result set").into())
}

/// The first column of the first row; an error when there is no row.
async fn one(s: &MysqlSession, sql: &str) -> TestResult<String> {
    rows(s, sql)
        .await?
        .into_iter()
        .next()
        .and_then(|row| row.into_iter().next())
        .flatten()
        .ok_or_else(|| format!("{sql}: no value").into())
}

/// One row as text, `NULL` as empty text.
async fn row(s: &MysqlSession, sql: &str) -> TestResult<Vec<String>> {
    rows(s, sql)
        .await?
        .into_iter()
        .next()
        .map(|row| row.into_iter().map(Option::unwrap_or_default).collect())
        .ok_or_else(|| format!("{sql}: no row").into())
}

/// The first column of every row.
async fn column(s: &MysqlSession, sql: &str) -> TestResult<Vec<String>> {
    Ok(rows(s, sql)
        .await?
        .into_iter()
        .map(|row| row.into_iter().next().flatten().unwrap_or_default())
        .collect())
}

async fn count(s: &MysqlSession, sql: &str) -> TestResult<i64> {
    Ok(one(s, sql).await?.parse()?)
}

/// The `SHOW CREATE ...` text (second column).
async fn show_create(s: &MysqlSession, sql: &str) -> TestResult<String> {
    let row = row(s, sql).await?;
    row.get(1)
        .cloned()
        .ok_or_else(|| format!("{sql}: no definition").into())
}

/// Plans a change given as JSON (the shape the UI sends), runs every
/// statement and returns the plan.
async fn apply(s: &MysqlSession, change: Json) -> TestResult<EditPlan> {
    let change: MyChange = serde_json::from_value(change)?;
    let plan = plan(s.server_info(), &change)?;
    assert!(
        !plan.transactional,
        "MySQL DDL plans are never transactional"
    );
    for statement in &plan.statements {
        // Each plan statement must stay one statement for the script
        // splitter too (compound bodies included).
        let parts = split_statements(statement, Dialect::Mysql).len();
        if parts != 1 {
            return Err(format!(
                "the splitter cut this statement into {parts} parts:\n{statement}"
            )
            .into());
        }
        exec(s, statement).await?;
    }
    Ok(plan)
}

fn contains(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn check(condition: bool, what: impl std::fmt::Display) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(format!("check failed: {what}").into())
    }
}

fn mariadb(s: &MysqlSession) -> bool {
    s.server_info().engine == Engine::Mariadb
}

/// Runs `area` on every server in a fresh database `db`, dropping it
/// afterwards also when the area failed.
async fn on_every_server<F>(db: &str, area: F) -> TestResult
where
    F: AsyncFn(&MysqlSession, &str) -> TestResult,
{
    for target in targets() {
        let s = connect(&target).await?;
        exec(&s, &format!("DROP DATABASE IF EXISTS `{db}`")).await?;
        exec(&s, &format!("CREATE DATABASE `{db}`")).await?;
        let result = area(&s, db).await;
        let cleanup = exec(&s, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
        result
            .map_err(|error| format!("{} ({}): {error}", target.name, s.server_info().version))?;
        cleanup?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn databases() -> TestResult {
    for target in targets() {
        let s = connect(&target).await?;
        let db = "m2_myedit_database";
        exec(&s, &format!("DROP DATABASE IF EXISTS `{db}`")).await?;
        let result = database_round_trip(&s, db).await;
        exec(&s, &format!("DROP DATABASE IF EXISTS `{db}`")).await?;
        result.map_err(|error| format!("{}: {error}", target.name))?;
    }
    Ok(())
}

async fn database_round_trip(s: &MysqlSession, db: &str) -> TestResult {
    let maria = mariadb(s);
    let mut create = json!({"change": "createDatabase", "name": db, "ifNotExists": true,
        "charset": "utf8mb4", "collation": "utf8mb4_bin"});
    if maria {
        create["comment"] = json!("shop's data");
    } else {
        create["encryption"] = json!(false);
    }
    apply(s, create).await?;
    let schema = format!(
        "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{db}'"
    );
    check(
        row(s, &schema).await? == ["utf8mb4", "utf8mb4_bin"],
        "charset and collation after create",
    )?;
    if maria {
        check(
            one(s, &format!("SELECT SCHEMA_COMMENT FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{db}'")).await? == "shop's data",
            "database comment",
        )?;
    } else {
        check(
            one(s, &format!("SELECT DEFAULT_ENCRYPTION FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{db}'")).await? == "NO",
            "database encryption",
        )?;
    }

    let mut alter =
        json!({"change": "alterDatabase", "name": db, "collation": "utf8mb4_general_ci"});
    if maria {
        alter["comment"] = json!("changed");
    } else {
        alter["readOnly"] = json!(true);
    }
    let plan = apply(s, alter).await?;
    check(!plan.warnings.is_empty(), "collation change warns")?;
    check(
        row(s, &schema).await?[1] == "utf8mb4_general_ci",
        "collation after alter",
    )?;
    if maria {
        check(
            one(s, &format!("SELECT SCHEMA_COMMENT FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{db}'")).await? == "changed",
            "comment after alter",
        )?;
    } else {
        let options = format!(
            "SELECT IFNULL(OPTIONS, '') FROM information_schema.SCHEMATA_EXTENSIONS WHERE SCHEMA_NAME = '{db}'"
        );
        check(
            contains(&one(s, &options).await?, "READ ONLY=1"),
            "read only on",
        )?;
        apply(
            s,
            json!({"change": "alterDatabase", "name": db, "readOnly": false}),
        )
        .await?;
        check(
            !contains(&one(s, &options).await?, "READ ONLY=1"),
            "read only off",
        )?;
    }

    let plan = apply(s, json!({"change": "dropDatabase", "name": db})).await?;
    check(plan.destructive, "drop is destructive")?;
    check(
        count(
            s,
            &format!("SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{db}'"),
        )
        .await?
            == 0,
        "database dropped",
    )
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn tables() -> TestResult {
    on_every_server("m2_myedit_tables", async |s: &MysqlSession, db: &str| {
        table_round_trip(s, db).await
    })
    .await
}

/// `COLUMN_NAME -> [COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA,
/// GENERATION_EXPRESSION, COLUMN_COMMENT, COLLATION_NAME]`.
async fn columns(s: &MysqlSession, db: &str, table: &str) -> TestResult<Vec<Vec<String>>> {
    Ok(rows(
        s,
        &format!(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, IFNULL(COLUMN_DEFAULT, ''), EXTRA, IFNULL(GENERATION_EXPRESSION, ''), COLUMN_COMMENT, IFNULL(COLLATION_NAME, '') \
             FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{table}' ORDER BY ORDINAL_POSITION"
        ),
    )
    .await?
    .into_iter()
    .map(|r| r.into_iter().map(Option::unwrap_or_default).collect())
    .collect())
}

fn find<'a>(columns: &'a [Vec<String>], name: &str) -> TestResult<&'a Vec<String>> {
    columns
        .iter()
        .find(|c| c.first().is_some_and(|n| n == name))
        .ok_or_else(|| format!("column {name} is missing").into())
}

async fn constraint_names(s: &MysqlSession, db: &str, table: &str) -> TestResult<Vec<String>> {
    column(
        s,
        &format!(
            "SELECT CONCAT(CONSTRAINT_TYPE, ':', CONSTRAINT_NAME) FROM information_schema.TABLE_CONSTRAINTS \
             WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{table}' ORDER BY 1"
        ),
    )
    .await
}

/// Index name and its columns in order, `column(sub_part)` with a `-`
/// suffix for descending parts.
async fn index_columns(
    s: &MysqlSession,
    db: &str,
    table: &str,
    index: &str,
) -> TestResult<Vec<String>> {
    column(
        s,
        &format!(
            "SELECT CONCAT(IFNULL(COLUMN_NAME, 'expr'), IF(SUB_PART IS NULL, '', CONCAT('(', SUB_PART, ')')), IF(COLLATION = 'D', '-', '')) \
             FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{table}' AND INDEX_NAME = '{index}' ORDER BY SEQ_IN_INDEX"
        ),
    )
    .await
}

async fn table_round_trip(s: &MysqlSession, db: &str) -> TestResult {
    let maria = mariadb(s);
    apply(
        s,
        json!({"change": "createTable", "database": db, "name": "customers",
            "columns": [{"name": "id", "dataType": {"name": "int"}, "nullable": false}],
            "constraints": [{"kind": "primaryKey", "parts": [{"kind": "column", "name": "id"}]}]}),
    )
    .await?;
    let create = json!({
        "change": "createTable", "database": db, "name": "orders",
        "columns": [
            {"name": "id", "dataType": {"name": "bigint", "unsigned": true}, "nullable": false, "autoIncrement": true},
            {"name": "customer", "dataType": {"name": "int"}, "nullable": false},
            {"name": "code", "dataType": {"name": "int", "length": 6, "zerofill": true}},
            {"name": "status", "dataType": {"name": "enum", "values": ["new", "paid"], "charset": "utf8mb4", "collation": "utf8mb4_bin"},
             "nullable": false, "default": {"kind": "literal", "value": {"t": "text", "v": "new"}}},
            {"name": "total", "dataType": {"name": "decimal", "length": 10, "scale": 2}, "nullable": false,
             "default": {"kind": "literal", "value": {"t": "decimal", "v": "0.00"}}, "comment": "gross's"},
            {"name": "note", "dataType": {"name": "text"}, "default": {"kind": "literal", "value": {"t": "text", "v": "n/a"}}},
            {"name": "path", "dataType": {"name": "varchar", "length": 20}, "default": {"kind": "literal", "value": {"t": "text", "v": "C:\\tmp"}}},
            {"name": "created", "dataType": {"name": "datetime", "length": 3}, "nullable": false,
             "default": {"kind": "currentTimestamp", "precision": 3}, "onUpdateCurrentTimestamp": 3},
            {"name": "doubled", "dataType": {"name": "int"}, "generated": {"expression": "total * 2", "storage": "virtual"}},
            {"name": "plus_one", "dataType": {"name": "int"}, "generated": {"expression": "total + 1", "storage": "stored"}},
            {"name": "secret", "dataType": {"name": "int"}, "invisible": true},
            {"name": "pos", "dataType": {"name": "point"}, "nullable": false, "srid": 4326},
            {"name": "doc", "dataType": {"name": "json"}, "default": {"kind": "expression", "sql": "json_array()"}}
        ],
        "constraints": [
            {"kind": "primaryKey", "parts": [{"kind": "column", "name": "id"}]},
            {"kind": "foreignKey", "name": "fk_customer", "columns": ["customer"], "referencesTable": "customers",
             "referencesColumns": ["id"], "onDelete": "cascade", "onUpdate": "restrict"},
            {"kind": "check", "name": "ck_total", "expression": "total >= 0"}
        ],
        "indexes": [
            {"name": "uq_code", "kind": "unique", "parts": [{"kind": "column", "name": "code"}]},
            {"name": "ix_status_created", "kind": "index", "method": "btree", "comment": "list",
             "parts": [{"kind": "column", "name": "status"}, {"kind": "column", "name": "created", "descending": true}]},
            {"name": "sx_pos", "kind": "spatial", "parts": [{"kind": "column", "name": "pos"}]},
            {"name": "ft_note", "kind": "fulltext", "parts": [{"kind": "column", "name": "note"}]}
        ],
        "options": {"engine": "InnoDB", "charset": "utf8mb4", "collation": "utf8mb4_unicode_ci", "comment": "orders",
                    "autoIncrement": 1000, "rowFormat": "dynamic", "statsPersistent": "on"}
    });
    apply(s, create).await?;

    let cols = columns(s, db, "orders").await?;
    let id = find(&cols, "id")?;
    check(
        contains(&id[1], "unsigned") && contains(&id[4], "auto_increment"),
        format!("id {id:?}"),
    )?;
    check(
        contains(&find(&cols, "code")?[1], "zerofill"),
        "code zerofill",
    )?;
    let status = find(&cols, "status")?;
    check(
        status[1] == "enum('new','paid')"
            && status[2] == "NO"
            && status[3].trim_matches('\'') == "new"
            && status[7] == "utf8mb4_bin",
        format!("status {status:?}"),
    )?;
    let total = find(&cols, "total")?;
    check(
        total[1] == "decimal(10,2)" && total[6] == "gross's",
        format!("total {total:?}"),
    )?;
    check(contains(&find(&cols, "note")?[3], "n/a"), "note default")?;
    // The backslash default went in through the hex literal form: the
    // stored value is exactly `C:\tmp`.
    exec(s, &format!("INSERT INTO `{db}`.customers VALUES (1)")).await?;
    exec(
        s,
        &format!("INSERT INTO `{db}`.orders (customer, code, pos) VALUES (1, 1, ST_GeomFromText('POINT(1 1)', 4326))"),
    )
    .await?;
    check(
        one(s, &format!("SELECT HEX(path) FROM `{db}`.orders")).await? == "433A5C746D70",
        "path default with a backslash",
    )?;
    check(
        one(s, &format!("SELECT id FROM `{db}`.orders")).await? == "1000",
        "AUTO_INCREMENT start",
    )?;
    let created = find(&cols, "created")?;
    check(
        contains(&created[3], "current_timestamp(3)")
            && contains(&created[4], "on update current_timestamp(3)"),
        format!("created {created:?}"),
    )?;
    let doubled = find(&cols, "doubled")?;
    check(
        contains(&doubled[4], "VIRTUAL GENERATED") && contains(&doubled[5], "total"),
        format!("doubled {doubled:?}"),
    )?;
    check(
        contains(&find(&cols, "plus_one")?[4], "STORED GENERATED"),
        "plus_one stored",
    )?;
    check(
        contains(&find(&cols, "secret")?[4], "INVISIBLE"),
        "secret invisible",
    )?;
    check(
        contains(&find(&cols, "doc")?[3], "json_array"),
        "doc default",
    )?;
    let srid = if maria {
        one(s, &format!("SELECT SRID FROM information_schema.GEOMETRY_COLUMNS WHERE F_TABLE_SCHEMA = '{db}' AND F_TABLE_NAME = 'orders' AND G_GEOMETRY_COLUMN = 'pos'")).await?
    } else {
        one(s, &format!("SELECT SRS_ID FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'orders' AND COLUMN_NAME = 'pos'")).await?
    };
    check(srid == "4326", format!("srid {srid}"))?;

    let constraints = constraint_names(s, db, "orders").await?;
    for expected in [
        "CHECK:ck_total",
        "FOREIGN KEY:fk_customer",
        "PRIMARY KEY:PRIMARY",
        "UNIQUE:uq_code",
    ] {
        check(
            constraints.iter().any(|c| c == expected),
            format!("{expected} in {constraints:?}"),
        )?;
    }
    check(
        row(s, &format!("SELECT DELETE_RULE, UPDATE_RULE, REFERENCED_TABLE_NAME FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = '{db}' AND CONSTRAINT_NAME = 'fk_customer'")).await?
            == ["CASCADE", "RESTRICT", "customers"],
        "foreign key rules",
    )?;
    check(
        contains(&one(s, &format!("SELECT CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = '{db}' AND CONSTRAINT_NAME = 'ck_total'")).await?, "total"),
        "check clause",
    )?;
    let desc_honoured = !maria || s.server_info().at_least(10, 8);
    check(
        index_columns(s, db, "orders", "ix_status_created").await?
            == ["status", if desc_honoured { "created-" } else { "created" }],
        "ix_status_created columns",
    )?;
    check(
        column(s, &format!("SELECT DISTINCT CONCAT(INDEX_NAME, ':', INDEX_TYPE, ':', INDEX_COMMENT) FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'orders' AND INDEX_NAME IN ('sx_pos', 'ft_note', 'ix_status_created') ORDER BY 1")).await?
            == ["ft_note:FULLTEXT:", "ix_status_created:BTREE:list", "sx_pos:SPATIAL:"],
        "index types and comment",
    )?;
    check(
        row(s, &format!("SELECT ENGINE, TABLE_COLLATION, TABLE_COMMENT, ROW_FORMAT FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'orders'")).await?
            == ["InnoDB", "utf8mb4_unicode_ci", "orders", "Dynamic"],
        "table options",
    )?;
    let ddl = show_create(s, &format!("SHOW CREATE TABLE `{db}`.`orders`")).await?;
    check(
        contains(&ddl, "STATS_PERSISTENT=1"),
        format!("table ddl {ddl}"),
    )?;

    // Alter 1: renames force a second statement.
    let plan = apply(
        s,
        json!({"change": "alterTable", "database": db, "name": "orders", "operations": [
            {"op": "addColumn", "column": {"name": "extra", "dataType": {"name": "varchar", "length": 20}}, "position": {"kind": "after", "column": "customer"}},
            {"op": "modifyColumn",
                "previous": {"name": "total", "dataType": {"name": "decimal", "length": 10, "scale": 2}, "nullable": false,
                    "default": {"kind": "literal", "value": {"t": "decimal", "v": "0.00"}}, "comment": "gross's"},
                "column": {"name": "total", "dataType": {"name": "decimal", "length": 12, "scale": 2}, "nullable": false,
                "default": {"kind": "literal", "value": {"t": "int", "v": 0}}, "comment": "gross"}},
            {"op": "renameColumn", "oldName": "note", "newName": "remark"},
            {"op": "addIndex", "index": {"name": "ix_remark", "kind": "index", "parts": [{"kind": "column", "name": "remark", "length": 10}]}},
            {"op": "setColumnDefault", "name": "extra", "columnType": "varchar", "default": {"kind": "literal", "value": {"t": "text", "v": "x"}}},
            {"op": "setIndexVisibility", "name": "uq_code", "invisible": true},
            {"op": "dropCheck", "name": "ck_total"},
            {"op": "addConstraint", "constraint": {"kind": "check", "name": "ck_total2", "expression": "total < 1000000"}},
            {"op": "setOptions", "options": {"comment": "orders v2"}},
            {"op": "renameIndex", "oldName": "ix_status_created", "newName": "ix_sc"}
        ]}),
    )
    .await?;
    check(
        plan.statements.len() >= 2 && plan.destructive,
        format!("alter 1 plan {:?}", plan.statements),
    )?;
    let cols = columns(s, db, "orders").await?;
    let names: Vec<&str> = cols.iter().map(|c| c[0].as_str()).collect();
    check(
        names[..3] == ["id", "customer", "extra"],
        format!("column order {names:?}"),
    )?;
    check(
        find(&cols, "extra")?[3].trim_matches('\'') == "x",
        "extra default",
    )?;
    check(
        find(&cols, "total")?[1] == "decimal(12,2)",
        "total modified",
    )?;
    check(
        find(&cols, "remark").is_ok() && find(&cols, "note").is_err(),
        "note renamed",
    )?;
    check(
        index_columns(s, db, "orders", "ix_remark").await? == ["remark(10)"],
        "ix_remark",
    )?;
    check(
        !index_columns(s, db, "orders", "ix_sc").await?.is_empty(),
        "ix_sc renamed",
    )?;
    let hidden = if maria {
        one(s, &format!("SELECT IGNORED FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'orders' AND INDEX_NAME = 'uq_code'")).await? == "YES"
    } else {
        one(s, &format!("SELECT IS_VISIBLE FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'orders' AND INDEX_NAME = 'uq_code'")).await? == "NO"
    };
    check(hidden, "uq_code invisible")?;
    let constraints = constraint_names(s, db, "orders").await?;
    check(
        constraints.contains(&"CHECK:ck_total2".to_string())
            && !constraints.contains(&"CHECK:ck_total".to_string()),
        format!("checks {constraints:?}"),
    )?;
    check(
        one(s, &format!("SELECT TABLE_COMMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'orders'")).await? == "orders v2",
        "comment changed",
    )?;

    // Alter 2: change column, replace a foreign key, engine specific parts.
    let mut ops = vec![
        json!({"op": "changeColumn", "previous": {"name": "extra", "dataType": {"name": "varchar", "length": 20}}, "column": {"name": "extra2", "dataType": {"name": "varchar", "length": 30}}, "position": {"kind": "first"}}),
        json!({"op": "dropForeignKey", "name": "fk_customer"}),
        json!({"op": "addConstraint", "constraint": {"kind": "foreignKey", "name": "fk_customer", "columns": ["customer"],
            "referencesTable": "customers", "referencesColumns": ["id"], "onDelete": "restrict"}}),
        json!({"op": "dropIndex", "name": "ft_note"}),
    ];
    if !maria {
        ops.push(json!({"op": "setColumnVisibility", "name": "secret", "invisible": false}));
        ops.push(json!({"op": "setCheckEnforced", "name": "ck_total2", "enforced": false}));
    }
    let plan = apply(
        s,
        json!({"change": "alterTable", "database": db, "name": "orders", "operations": ops}),
    )
    .await?;
    check(
        plan.statements.len() >= 2,
        format!("alter 2 plan {:?}", plan.statements),
    )?;
    let cols = columns(s, db, "orders").await?;
    check(
        cols[0][0] == "extra2" && cols[0][1] == "varchar(30)",
        format!("extra2 first {:?}", cols[0]),
    )?;
    check(
        one(s, &format!("SELECT DELETE_RULE FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = '{db}' AND CONSTRAINT_NAME = 'fk_customer'")).await? == "RESTRICT",
        "foreign key replaced",
    )?;
    check(
        index_columns(s, db, "orders", "ft_note").await?.is_empty(),
        "ft_note dropped",
    )?;
    if !maria {
        check(
            !contains(&find(&cols, "secret")?[4], "INVISIBLE"),
            "secret visible",
        )?;
        check(
            one(s, &format!("SELECT ENFORCED FROM information_schema.TABLE_CONSTRAINTS WHERE TABLE_SCHEMA = '{db}' AND CONSTRAINT_NAME = 'ck_total2'")).await? == "NO",
            "check not enforced",
        )?;
    }

    // Alter 3: online add with algorithm and lock, then key and name changes.
    apply(
        s,
        json!({"change": "alterTable", "database": db, "name": "orders", "algorithm": "inplace", "lock": "shared",
            "operations": [{"op": "addColumn", "column": {"name": "online", "dataType": {"name": "int"}}}]}),
    )
    .await?;
    check(
        find(&columns(s, db, "orders").await?, "online").is_ok(),
        "online column",
    )?;
    let plan = apply(
        s,
        json!({"change": "alterTable", "database": db, "name": "orders", "operations": [
            {"op": "dropPrimaryKey"},
            {"op": "addConstraint", "constraint": {"kind": "primaryKey", "parts": [{"kind": "column", "name": "id"}, {"kind": "column", "name": "customer"}]}},
            {"op": "convertCharset", "charset": "utf8mb4", "collation": "utf8mb4_bin"},
            {"op": "setOptions", "options": {"engine": "InnoDB", "rowFormat": "compact"}},
            {"op": "rename", "name": "orders2"}
        ]}),
    )
    .await?;
    check(
        plan.destructive && plan.statements.len() == 1,
        format!("alter 3 plan {:?}", plan.statements),
    )?;
    check(
        index_columns(s, db, "orders2", "PRIMARY").await? == ["id", "customer"],
        "new primary key",
    )?;
    check(
        row(s, &format!("SELECT TABLE_COLLATION, ROW_FORMAT FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'orders2'")).await?
            == ["utf8mb4_bin", "Compact"],
        "converted and renamed",
    )?;

    let plan = apply(
        s,
        json!({"change": "dropTable", "database": db, "name": "orders2"}),
    )
    .await?;
    check(plan.destructive, "drop table destructive")?;
    check(
        count(s, &format!("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'orders2'")).await? == 0,
        "table dropped",
    )
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn partitions() -> TestResult {
    on_every_server("m2_myedit_parts", async |s: &MysqlSession, db: &str| {
        partition_round_trip(s, db).await
    })
    .await
}

async fn partition_names(s: &MysqlSession, db: &str, table: &str) -> TestResult<Vec<String>> {
    column(
        s,
        &format!(
            "SELECT PARTITION_NAME FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{table}' AND PARTITION_NAME IS NOT NULL GROUP BY PARTITION_NAME ORDER BY MIN(PARTITION_ORDINAL_POSITION)"
        ),
    )
    .await
}

fn bound(v: i64) -> Json {
    json!({"kind": "value", "value": {"t": "int", "v": v}})
}

async fn partition_round_trip(s: &MysqlSession, db: &str) -> TestResult {
    let maria = mariadb(s);
    apply(
        s,
        json!({"change": "createTable", "database": db, "name": "pr",
            "columns": [{"name": "id", "dataType": {"name": "int"}, "nullable": false}, {"name": "at", "dataType": {"name": "date"}, "nullable": false}],
            "constraints": [{"kind": "primaryKey", "parts": [{"kind": "column", "name": "id"}, {"kind": "column", "name": "at"}]}],
            "partitioning": {
                "method": {"kind": "range", "expression": "YEAR(at)"},
                "subpartition": {"method": {"kind": "key", "columns": ["id"], "linear": true, "algorithm": 2}, "count": 2},
                "partitions": [
                    {"name": "p2020", "values": {"kind": "lessThan", "bounds": [bound(2020)]}, "comment": "old"},
                    {"name": "p2030", "values": {"kind": "lessThan", "bounds": [bound(2030)]}},
                    {"name": "pmax", "values": {"kind": "lessThan", "bounds": [{"kind": "maxValue"}]}}
                ]}}),
    )
    .await?;
    check(
        row(s, &format!("SELECT COUNT(*), MIN(PARTITION_METHOD), MIN(SUBPARTITION_METHOD) FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'pr'")).await?
            == ["6", "RANGE", "LINEAR KEY"],
        "range with subpartitions",
    )?;
    check(
        one(s, &format!("SELECT DISTINCT PARTITION_COMMENT FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'pr' AND PARTITION_NAME = 'p2020'")).await? == "old",
        "partition comment",
    )?;
    exec(
        s,
        &format!(
            "INSERT INTO `{db}`.pr VALUES (1, '2019-05-01'), (2, '2025-01-01'), (3, '2035-01-01')"
        ),
    )
    .await?;
    let plan = apply(
        s,
        json!({"change": "alterTable", "database": db, "name": "pr", "operations": [
            {"op": "reorganizePartitions", "names": ["pmax"], "into": [
                {"name": "p2040", "values": {"kind": "lessThan", "bounds": [bound(2040)]}},
                {"name": "pmax", "values": {"kind": "lessThan", "bounds": [{"kind": "maxValue"}]}}]},
            {"op": "truncatePartitions", "names": ["p2020"]},
            {"op": "dropPartitions", "names": ["p2020"]}
        ]}),
    )
    .await?;
    check(
        plan.statements.len() == 3 && plan.destructive,
        "partition maintenance plan",
    )?;
    check(
        partition_names(s, db, "pr").await? == ["p2030", "p2040", "pmax"],
        "partitions after maintenance",
    )?;
    check(
        count(s, &format!("SELECT COUNT(*) FROM `{db}`.pr")).await? == 2,
        "rows after drop",
    )?;

    apply(
        s,
        json!({"change": "createTable", "database": db, "name": "pl",
            "columns": [{"name": "a", "dataType": {"name": "int"}}, {"name": "b", "dataType": {"name": "varchar", "length": 5}}],
            "partitioning": {"method": {"kind": "listColumns", "columns": ["a", "b"]}, "partitions": [
                {"name": "x", "values": {"kind": "in", "rows": [[bound(1), {"kind": "value", "value": {"t": "text", "v": "a"}}],
                                                              [bound(2), {"kind": "value", "value": {"t": "text", "v": "b"}}]]}},
                {"name": "y", "values": {"kind": "in", "rows": [[bound(3), {"kind": "value", "value": {"t": "null"}}]]}}
            ]}}),
    )
    .await?;
    apply(
        s,
        json!({"change": "alterTable", "database": db, "name": "pl", "operations": [
            {"op": "addPartitions", "partitions": [{"name": "z", "values": {"kind": "in", "rows": [[bound(4), {"kind": "value", "value": {"t": "text", "v": "d"}}]]}}]},
            {"op": "truncatePartitions"}
        ]}),
    )
    .await?;
    check(
        partition_names(s, db, "pl").await? == ["x", "y", "z"],
        "list columns partitions",
    )?;
    check(
        one(s, &format!("SELECT DISTINCT PARTITION_METHOD FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'pl'")).await? == "LIST COLUMNS",
        "list columns method",
    )?;
    if maria {
        apply(
            s,
            json!({"change": "createTable", "database": db, "name": "pd",
                "columns": [{"name": "a", "dataType": {"name": "int"}}],
                "partitioning": {"method": {"kind": "list", "expression": "a"}, "partitions": [
                    {"name": "small", "values": {"kind": "in", "rows": [[bound(1)], [bound(2)]]}},
                    {"name": "rest", "values": {"kind": "default"}}]}}),
        )
        .await?;
        check(
            contains(
                &show_create(s, &format!("SHOW CREATE TABLE `{db}`.pd")).await?,
                "PARTITION `rest` DEFAULT",
            ),
            "default partition",
        )?;
    }

    apply(
        s,
        json!({"change": "createTable", "database": db, "name": "ph",
            "columns": [{"name": "id", "dataType": {"name": "int"}, "nullable": false}],
            "partitioning": {"method": {"kind": "hash", "expression": "id", "linear": true}, "count": 4}}),
    )
    .await?;
    check(
        partition_names(s, db, "ph").await?.len() == 4,
        "4 hash partitions",
    )?;
    apply(
        s,
        json!({"change": "alterTable", "database": db, "name": "ph", "operations": [
            {"op": "coalescePartitions", "count": 2},
            {"op": "addPartitionCount", "count": 1}
        ]}),
    )
    .await?;
    check(
        partition_names(s, db, "ph").await?.len() == 3,
        "3 hash partitions",
    )?;
    apply(s, json!({"change": "alterTable", "database": db, "name": "ph", "operations": [{"op": "removePartitioning"}]})).await?;
    check(
        partition_names(s, db, "ph").await?.is_empty(),
        "partitioning removed",
    )?;
    apply(
        s,
        json!({"change": "alterTable", "database": db, "name": "ph", "operations": [
            {"op": "partitionBy", "partitioning": {"method": {"kind": "key", "columns": ["id"]}, "count": 2}}]}),
    )
    .await?;
    check(
        row(s, &format!("SELECT COUNT(*), MIN(PARTITION_METHOD) FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'ph'")).await?
            == ["2", "KEY"],
        "key partitioning",
    )
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn indexes() -> TestResult {
    on_every_server("m2_myedit_indexes", async |s: &MysqlSession, db: &str| {
        index_round_trip(s, db).await
    })
    .await
}

async fn index_round_trip(s: &MysqlSession, db: &str) -> TestResult {
    let maria = mariadb(s);
    exec(
        s,
        &format!("CREATE TABLE `{db}`.t (id INT PRIMARY KEY, email VARCHAR(100), body TEXT, a INT, b INT, pos POINT NOT NULL SRID 4326)")
            .replace(" SRID 4326", if maria { "" } else { " SRID 4326" }),
    )
    .await?;
    let stat = |index: &str, column: &str| {
        format!(
            "SELECT {column} FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 't' AND INDEX_NAME = '{index}' LIMIT 1"
        )
    };
    let hidden = |index: &str| {
        if maria {
            (stat(index, "IGNORED"), "YES")
        } else {
            (stat(index, "IS_VISIBLE"), "NO")
        }
    };

    apply(
        s,
        json!({"change": "createIndex", "database": db, "table": "t", "algorithm": "inplace", "lock": "none",
            "index": {"name": "ix_ab", "kind": "index", "comment": "pair", "invisible": true,
                      "parts": [{"kind": "column", "name": "a"}, {"kind": "column", "name": "b", "descending": true}]}}),
    )
    .await?;
    let desc_honoured = !maria || s.server_info().at_least(10, 8);
    check(
        index_columns(s, db, "t", "ix_ab").await? == ["a", if desc_honoured { "b-" } else { "b" }],
        "ix_ab columns",
    )?;
    check(
        one(s, &stat("ix_ab", "INDEX_COMMENT")).await? == "pair",
        "ix_ab comment",
    )?;
    let (sql, value) = hidden("ix_ab");
    check(one(s, &sql).await? == value, "ix_ab invisible")?;

    apply(
        s,
        json!({"change": "createIndex", "database": db, "table": "t",
            "index": {"name": "uq_email", "kind": "unique", "parts": [{"kind": "column", "name": "email", "length": 20}]}}),
    )
    .await?;
    check(
        row(s, &format!("SELECT NON_UNIQUE, SUB_PART FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 't' AND INDEX_NAME = 'uq_email'")).await?
            == ["0", "20"],
        "unique prefix index",
    )?;
    let mut fulltext = json!({"name": "ft_body", "kind": "fulltext", "parts": [{"kind": "column", "name": "body"}]});
    if !maria {
        fulltext["parser"] = json!("ngram");
    }
    apply(
        s,
        json!({"change": "createIndex", "database": db, "table": "t", "index": fulltext}),
    )
    .await?;
    check(
        one(s, &stat("ft_body", "INDEX_TYPE")).await? == "FULLTEXT",
        "fulltext index",
    )?;
    if !maria {
        check(
            contains(
                &show_create(s, &format!("SHOW CREATE TABLE `{db}`.t")).await?,
                "WITH PARSER `ngram`",
            ),
            "fulltext parser",
        )?;
    }
    apply(
        s,
        json!({"change": "createIndex", "database": db, "table": "t",
            "index": {"name": "sx_pos", "kind": "spatial", "parts": [{"kind": "column", "name": "pos"}]}}),
    )
    .await?;
    check(
        one(s, &stat("sx_pos", "INDEX_TYPE")).await? == "SPATIAL",
        "spatial index",
    )?;
    if !maria {
        apply(
            s,
            json!({"change": "createIndex", "database": db, "table": "t",
                "index": {"name": "fx_email", "kind": "index", "parts": [{"kind": "expression", "sql": "lower(email)", "descending": true}]}}),
        )
        .await?;
        check(
            contains(&one(s, &stat("fx_email", "EXPRESSION")).await?, "lower"),
            "functional index",
        )?;
    }

    apply(s, json!({"change": "setIndexVisibility", "database": db, "table": "t", "name": "ix_ab", "invisible": false})).await?;
    let (sql, value) = hidden("ix_ab");
    check(one(s, &sql).await? != value, "ix_ab visible again")?;
    apply(s, json!({"change": "renameIndex", "database": db, "table": "t", "oldName": "ix_ab", "newName": "ix_ab2"})).await?;
    check(
        index_columns(s, db, "t", "ix_ab").await?.is_empty(),
        "ix_ab renamed",
    )?;
    let plan = apply(
        s,
        json!({"change": "replaceIndex", "database": db, "table": "t", "oldName": "ix_ab2",
            "index": {"name": "ix_ab2", "kind": "unique", "parts": [{"kind": "column", "name": "a"}]}}),
    )
    .await?;
    check(
        !plan.destructive && plan.statements.len() == 1,
        "replace plan",
    )?;
    check(
        index_columns(s, db, "t", "ix_ab2").await? == ["a"],
        "ix_ab2 replaced",
    )?;
    check(
        one(s, &stat("ix_ab2", "NON_UNIQUE")).await? == "0",
        "ix_ab2 unique",
    )?;
    let plan = apply(s, json!({"change": "dropIndex", "database": db, "table": "t", "name": "uq_email", "lock": "default"})).await?;
    check(plan.destructive, "drop index destructive")?;
    check(
        index_columns(s, db, "t", "uq_email").await?.is_empty(),
        "uq_email dropped",
    )
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn views() -> TestResult {
    on_every_server("m2_myedit_views", async |s: &MysqlSession, db: &str| {
        exec(s, &format!("CREATE TABLE `{db}`.t (x INT, y INT)")).await?;
        let query = format!("SELECT x, y FROM `{db}`.t WHERE x > 0");
        apply(
            s,
            json!({"change": "createView", "database": db, "name": "v", "algorithm": "merge",
                "definer": {"kind": "currentUser"}, "security": "invoker", "columns": ["a", "b"],
                "query": query, "checkOption": "local"}),
        )
        .await?;
        let info = format!(
            "SELECT CHECK_OPTION, SECURITY_TYPE, IS_UPDATABLE, DEFINER FROM information_schema.VIEWS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'v'"
        );
        let first = row(s, &info).await?;
        check(first[..3] == ["LOCAL", "INVOKER", "YES"], format!("view info {first:?}"))?;
        check(
            contains(&show_create(s, &format!("SHOW CREATE VIEW `{db}`.v")).await?, "ALGORITHM=MERGE"),
            "view algorithm",
        )?;
        check(
            column(s, &format!("SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'v' ORDER BY ORDINAL_POSITION")).await?
                == ["a", "b"],
            "view columns",
        )?;
        apply(
            s,
            json!({"change": "createView", "database": db, "name": "v", "replace": true, "algorithm": "temptable",
                "definer": {"kind": "account", "user": "root", "host": "%"}, "security": "definer",
                "query": format!("SELECT x FROM `{db}`.t")}),
        )
        .await?;
        let second = row(s, &info).await?;
        check(second == ["NONE", "DEFINER", "NO", "root@%"], format!("replaced view {second:?}"))?;
        let plan = apply(s, json!({"change": "dropView", "database": db, "name": "v", "ifExists": true})).await?;
        check(plan.destructive, "drop view destructive")?;
        check(
            count(s, &format!("SELECT COUNT(*) FROM information_schema.VIEWS WHERE TABLE_SCHEMA = '{db}'")).await? == 0,
            "view dropped",
        )
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn routines() -> TestResult {
    on_every_server("m2_myedit_routines", async |s: &MysqlSession, db: &str| {
        let procedure = |body: &str, replace: bool| {
            json!({"change": "createRoutine", "database": db, "name": "p", "routine": "procedure", "replace": replace,
                "definer": {"kind": "currentUser"},
                "parameters": [
                    {"mode": "in", "name": "a", "dataType": {"name": "int"}},
                    {"mode": "out", "name": "b", "dataType": {"name": "varchar", "length": 10, "charset": "utf8mb4"}}
                ],
                "characteristics": {"comment": "demo's", "deterministic": true, "dataAccess": "containsSql", "security": "invoker"},
                "body": body})
        };
        apply(s, procedure("BEGIN SET b = CONCAT('x', a); END;", false)).await?;
        check(
            row(s, &format!("SELECT ROUTINE_TYPE, IS_DETERMINISTIC, SECURITY_TYPE, SQL_DATA_ACCESS, ROUTINE_COMMENT FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = '{db}' AND ROUTINE_NAME = 'p'")).await?
                == ["PROCEDURE", "YES", "INVOKER", "CONTAINS SQL", "demo's"],
            "procedure characteristics",
        )?;
        check(
            column(s, &format!("SELECT CONCAT(PARAMETER_MODE, ' ', PARAMETER_NAME, ' ', DTD_IDENTIFIER) FROM information_schema.PARAMETERS WHERE SPECIFIC_SCHEMA = '{db}' AND SPECIFIC_NAME = 'p' ORDER BY ORDINAL_POSITION")).await?
                .iter()
                .map(|p| p.split(' ').take(2).collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
                == ["IN a", "OUT b"],
            "procedure parameters",
        )?;
        exec(s, &format!("CALL `{db}`.p(5, @r)")).await?;
        check(one(s, "SELECT @r").await? == "x5", "procedure result")?;

        let plan = apply(s, procedure("BEGIN SET b = CONCAT('y', a); END", true)).await?;
        check(plan.destructive == !mariadb(s), "replace is destructive only on MySQL")?;
        exec(s, &format!("CALL `{db}`.p(5, @r)")).await?;
        check(one(s, "SELECT @r").await? == "y5", "replaced procedure result")?;

        apply(
            s,
            json!({"change": "createRoutine", "database": db, "name": "f", "routine": "function",
                "parameters": [{"name": "x", "dataType": {"name": "decimal", "length": 10, "scale": 2}}],
                "returns": {"name": "decimal", "length": 12, "scale": 2},
                "characteristics": {"deterministic": true, "dataAccess": "noSql"},
                "body": "RETURN x * 2"}),
        )
        .await?;
        check(one(s, &format!("SELECT `{db}`.f(1.25)")).await? == "2.50", "function result")?;
        check(
            one(s, &format!("SELECT DTD_IDENTIFIER FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = '{db}' AND ROUTINE_NAME = 'f'")).await? == "decimal(12,2)",
            "function return type",
        )?;
        // Compound bodies: labels, nested blocks, loops, IF, CASE statements
        // and CASE expressions must reach the server as one statement.
        apply(
            s,
            json!({"change": "createRoutine", "database": db, "name": "sum_to", "routine": "procedure",
                "parameters": [{"mode": "in", "name": "n", "dataType": {"name": "int"}},
                               {"mode": "out", "name": "total", "dataType": {"name": "int"}},
                               {"mode": "out", "name": "kind", "dataType": {"name": "varchar", "length": 10}}],
                "body": "outer_block: BEGIN\n  DECLARE i INT DEFAULT 0;\n  SET total = 0;\n  IF n < 0 THEN\n    LEAVE outer_block;\n  END IF;\n  counting: WHILE i < n DO\n    SET i = i + 1;\n    IF i = 100 THEN LEAVE counting; END IF;\n    BEGIN\n      DECLARE step INT DEFAULT i;\n      SET total = total + step;\n    END;\n  END WHILE counting;\n  REPEAT SET i = i - 1; UNTIL i <= 0 END REPEAT;\n  CASE WHEN total > 10 THEN SET kind = 'big'; ELSE SET kind = 'small'; END CASE;\n  SET kind = CASE kind WHEN 'big' THEN 'BIG' ELSE kind END;\nEND outer_block;"}),
        )
        .await?;
        exec(s, &format!("CALL `{db}`.sum_to(5, @t, @k)")).await?;
        check(row(s, "SELECT @t, @k").await? == ["15", "BIG"], "compound procedure result")?;
        apply(
            s,
            json!({"change": "createRoutine", "database": db, "name": "grade", "routine": "function",
                "parameters": [{"name": "x", "dataType": {"name": "int"}}],
                "returns": {"name": "varchar", "length": 5},
                "characteristics": {"deterministic": true, "dataAccess": "noSql"},
                "body": "BEGIN\n  DECLARE r VARCHAR(5);\n  LOOP\n    SET r = CASE WHEN x >= 5 THEN 'high' ELSE 'low' END;\n    RETURN r;\n  END LOOP;\nEND"}),
        )
        .await?;
        check(one(s, &format!("SELECT `{db}`.grade(7)")).await? == "high", "compound function result")?;
        apply(s, json!({"change": "dropRoutine", "database": db, "name": "grade", "routine": "function"})).await?;
        apply(s, json!({"change": "dropRoutine", "database": db, "name": "sum_to", "routine": "procedure"})).await?;
        apply(s, json!({"change": "dropRoutine", "database": db, "name": "f", "routine": "function"})).await?;
        apply(s, json!({"change": "dropRoutine", "database": db, "name": "p", "routine": "procedure", "ifExists": true})).await?;
        check(
            count(s, &format!("SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = '{db}'")).await? == 0,
            "routines dropped",
        )
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn triggers() -> TestResult {
    on_every_server("m2_myedit_triggers", async |s: &MysqlSession, db: &str| {
        exec(s, &format!("CREATE TABLE `{db}`.t (id INT, v INT, log INT DEFAULT 0)")).await?;
        apply(
            s,
            json!({"change": "createTrigger", "database": db, "table": "t", "name": "tr1",
                "definer": {"kind": "currentUser"}, "timing": "before", "events": ["insert"],
                "body": "SET NEW.v = NEW.v + 1"}),
        )
        .await?;
        let second = |body: &str, replace: bool| {
            json!({"change": "createTrigger", "database": db, "table": "t", "name": "tr2", "replace": replace,
                "timing": "before", "events": ["insert"], "order": {"kind": "follows", "trigger": "tr1"},
                "body": body})
        };
        apply(s, second("BEGIN SET NEW.log = NEW.v * 10; END", false)).await?;
        check(
            column(s, &format!("SELECT CONCAT(TRIGGER_NAME, ':', ACTION_TIMING, ':', EVENT_MANIPULATION, ':', ACTION_ORDER) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = '{db}' ORDER BY ACTION_ORDER")).await?
                == ["tr1:BEFORE:INSERT:1", "tr2:BEFORE:INSERT:2"],
            "trigger order",
        )?;
        exec(s, &format!("INSERT INTO `{db}`.t (id, v) VALUES (1, 1)")).await?;
        check(row(s, &format!("SELECT v, log FROM `{db}`.t WHERE id = 1")).await? == ["2", "20"], "triggers fired")?;
        let plan = apply(s, second("SET NEW.log = 7", true)).await?;
        check(plan.destructive == !mariadb(s), "trigger replace destructive only on MySQL")?;
        exec(s, &format!("INSERT INTO `{db}`.t (id, v) VALUES (2, 1)")).await?;
        check(row(s, &format!("SELECT v, log FROM `{db}`.t WHERE id = 2")).await? == ["2", "7"], "replaced trigger fired")?;
        apply(
            s,
            json!({"change": "createTrigger", "database": db, "table": "t", "name": "tr3",
                "timing": "before", "events": ["update"],
                "body": "BEGIN\n  IF NEW.v < 0 THEN\n    SET NEW.v = 0;\n  ELSEIF NEW.v > 100 THEN\n    BEGIN\n      SET NEW.v = 100;\n    END;\n  END IF;\n  CASE WHEN NEW.v = 50 THEN SET NEW.log = -1; ELSE BEGIN END; END CASE;\nEND"}),
        )
        .await?;
        exec(s, &format!("UPDATE `{db}`.t SET v = 500 WHERE id = 1")).await?;
        check(one(s, &format!("SELECT v FROM `{db}`.t WHERE id = 1")).await? == "100", "compound trigger fired")?;
        apply(s, json!({"change": "dropTrigger", "database": db, "name": "tr3"})).await?;
        apply(s, json!({"change": "dropTrigger", "database": db, "name": "tr2"})).await?;
        apply(s, json!({"change": "dropTrigger", "database": db, "name": "tr1", "ifExists": true})).await?;
        check(
            count(s, &format!("SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = '{db}'")).await? == 0,
            "triggers dropped",
        )
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn events() -> TestResult {
    on_every_server("m2_myedit_events", async |s: &MysqlSession, db: &str| {
        let plan = apply(
            s,
            json!({"change": "createEvent", "database": db, "name": "e", "ifNotExists": true,
                "schedule": {"kind": "every", "interval": {"quantity": "1:30", "unit": "hourMinute"},
                    "starts": {"base": {"kind": "currentTimestamp"}, "plus": [{"quantity": "1", "unit": "hour"}]},
                    "ends": {"base": {"kind": "timestamp", "value": "2037-01-01 00:00:00"}}},
                "onCompletionPreserve": true, "status": "disableOnReplica", "comment": "cleanup",
                "body": "SELECT 1"}),
        )
        .await?;
        check(!plan.warnings.is_empty(), "scheduler warning")?;
        let info = |name: &str| {
            format!("SELECT EVENT_TYPE, STATUS, IFNULL(INTERVAL_VALUE, ''), IFNULL(INTERVAL_FIELD, ''), ON_COMPLETION, EVENT_COMMENT, IFNULL(ENDS, ''), IFNULL(EXECUTE_AT, ''), EVENT_DEFINITION FROM information_schema.EVENTS WHERE EVENT_SCHEMA = '{db}' AND EVENT_NAME = '{name}'")
        };
        let e = row(s, &info("e")).await?;
        check(
            e[0] == "RECURRING"
                && contains(&e[1], "SIDE_DISABLED")
                && e[2].trim_matches('\'') == "1:30"
                && e[3] == "HOUR_MINUTE"
                && e[4] == "PRESERVE"
                && e[5] == "cleanup"
                && e[6].starts_with("2037-01-01"),
            format!("event e {e:?}"),
        )?;
        apply(
            s,
            json!({"change": "createEvent", "database": db, "name": "once",
                "schedule": {"kind": "at", "at": {"base": {"kind": "currentTimestamp"}, "plus": [{"quantity": "2", "unit": "day"}]}},
                "body": "SELECT 1"}),
        )
        .await?;
        check(row(s, &info("once")).await?[0] == "ONE TIME", "one-time event")?;
        apply(
            s,
            json!({"change": "alterEvent", "database": db, "name": "e",
                "schedule": {"kind": "at", "at": {"base": {"kind": "timestamp", "value": "2030-06-01 12:00:00"}}},
                "onCompletionPreserve": false, "renameTo": "e2", "status": "enable", "comment": "moved", "body": "SELECT 2"}),
        )
        .await?;
        let e2 = row(s, &info("e2")).await?;
        check(
            e2[0] == "ONE TIME"
                && e2[1] == "ENABLED"
                && e2[4] == "NOT PRESERVE"
                && e2[5] == "moved"
                && e2[7].starts_with("2030-06-01")
                && e2[8] == "SELECT 2",
            format!("event e2 {e2:?}"),
        )?;
        check(rows(s, &info("e")).await?.is_empty(), "e renamed")?;
        exec(s, &format!("CREATE TABLE `{db}`.log (n INT)")).await?;
        let body = format!("BEGIN\n  DECLARE c INT DEFAULT 0;\n  SELECT COUNT(*) INTO c FROM `{db}`.log;\n  IF c > 1000 THEN\n    DELETE FROM `{db}`.log;\n  ELSE\n    INSERT INTO `{db}`.log VALUES (c);\n  END IF;\nEND");
        apply(
            s,
            json!({"change": "createEvent", "database": db, "name": "compound",
                "schedule": {"kind": "every", "interval": {"quantity": "1", "unit": "day"}},
                "status": "disable", "body": body}),
        )
        .await?;
        apply(
            s,
            json!({"change": "alterEvent", "database": db, "name": "compound",
                "body": format!("BEGIN\n  label1: LOOP\n    INSERT INTO `{db}`.log VALUES (1);\n    LEAVE label1;\n  END LOOP label1;\nEND")}),
        )
        .await?;
        check(
            contains(&one(s, &format!("SELECT EVENT_DEFINITION FROM information_schema.EVENTS WHERE EVENT_SCHEMA = '{db}' AND EVENT_NAME = 'compound'")).await?, "END LOOP label1"),
            "compound event body",
        )?;
        apply(s, json!({"change": "dropEvent", "database": db, "name": "compound"})).await?;
        apply(s, json!({"change": "dropEvent", "database": db, "name": "e2"})).await?;
        apply(s, json!({"change": "dropEvent", "database": db, "name": "once", "ifExists": true})).await?;
        check(
            count(s, &format!("SELECT COUNT(*) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = '{db}'")).await? == 0,
            "events dropped",
        )
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn sequences() -> TestResult {
    on_every_server("m2_myedit_sequences", async |s: &MysqlSession, db: &str| {
        let create = json!({"change": "createSequence", "database": db, "name": "s", "ifNotExists": true,
            "start": 10, "increment": 5, "minValue": {"kind": "value", "value": 1}, "maxValue": {"kind": "value", "value": 1000},
            "cache": 20, "cycle": true, "comment": "ids"});
        if !mariadb(s) {
            let change: MyChange = serde_json::from_value(create)?;
            check(plan(s.server_info(), &change).is_err(), "MySQL refuses sequences")?;
            return Ok(());
        }
        apply(s, create).await?;
        let state = format!("SELECT start_value, minimum_value, maximum_value, increment, cache_size, cycle_option FROM `{db}`.s");
        check(row(s, &state).await? == ["10", "1", "1000", "5", "20", "1"], "sequence options")?;
        check(
            one(s, &format!("SELECT TABLE_COMMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 's'")).await? == "ids",
            "sequence comment",
        )?;
        check(one(s, &format!("SELECT NEXTVAL(`{db}`.s)")).await? == "10", "first value")?;
        apply(
            s,
            json!({"change": "alterSequence", "database": db, "name": "s", "cycle": false, "cache": 0,
                "restart": {"kind": "with", "value": 50}}),
        )
        .await?;
        let after = row(s, &state).await?;
        check(after[4] == "0" && after[5] == "0", format!("altered options {after:?}"))?;
        check(one(s, &format!("SELECT NEXTVAL(`{db}`.s)")).await? == "50", "restarted")?;
        apply(s, json!({"change": "setSequenceValue", "database": db, "name": "s", "value": 70})).await?;
        check(one(s, &format!("SELECT NEXTVAL(`{db}`.s)")).await? == "75", "after setval")?;
        let plan = apply(
            s,
            json!({"change": "createSequence", "database": db, "name": "s", "replace": true, "start": 1}),
        )
        .await?;
        check(plan.destructive, "replace destructive")?;
        check(one(s, &format!("SELECT NEXTVAL(`{db}`.s)")).await? == "1", "replaced sequence")?;
        if s.server_info().at_least(11, 5) {
            apply(
                s,
                json!({"change": "createSequence", "database": db, "name": "big",
                    "dataType": {"name": "bigint", "unsigned": true}, "maxValue": {"kind": "value", "value": 18446744073709551614u64}}),
            )
            .await?;
            let ddl = show_create(s, &format!("SHOW CREATE SEQUENCE `{db}`.big")).await?;
            check(contains(&ddl, "as bigint unsigned") && contains(&ddl, "18446744073709551614"), format!("typed sequence {ddl}"))?;
            apply(s, json!({"change": "dropSequence", "database": db, "name": "big"})).await?;
        }
        apply(s, json!({"change": "dropSequence", "database": db, "name": "s", "ifExists": true})).await?;
        check(
            count(s, &format!("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{db}'")).await? == 0,
            "sequences dropped",
        )
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn system_versioning() -> TestResult {
    on_every_server("m2_myedit_versioning", async |s: &MysqlSession, db: &str| {
        let create = json!({
            "change": "createTable", "database": db, "name": "v",
            "columns": [
                {"name": "id", "dataType": {"name": "int"}, "nullable": false},
                {"name": "valid_from", "dataType": {"name": "date"}, "nullable": false},
                {"name": "valid_to", "dataType": {"name": "date"}, "nullable": false},
                {"name": "note", "dataType": {"name": "text"}, "withoutSystemVersioning": true},
                {"name": "rs", "dataType": {"name": "timestamp", "length": 6}, "rowPeriod": "start", "invisible": true},
                {"name": "re", "dataType": {"name": "timestamp", "length": 6}, "rowPeriod": "end", "invisible": true}
            ],
            "systemVersioning": {"period": {"start": "rs", "end": "re"}},
            "periods": [{"name": "validity", "start": "valid_from", "end": "valid_to"}]
        });
        if !mariadb(s) {
            let change: MyChange = serde_json::from_value(create)?;
            check(plan(s.server_info(), &change).is_err(), "MySQL refuses system versioning")?;
            return Ok(());
        }
        apply(s, create).await?;
        let table_type = |table: &str| {
            format!("SELECT TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{table}'")
        };
        check(one(s, &table_type("v")).await? == "SYSTEM VERSIONED", "versioned table")?;
        let ddl = show_create(s, &format!("SHOW CREATE TABLE `{db}`.v")).await?;
        check(
            contains(&ddl, "PERIOD FOR SYSTEM_TIME (`rs`, `re`)")
                && contains(&ddl, "PERIOD FOR `validity` (`valid_from`, `valid_to`)")
                && contains(&ddl, "WITHOUT SYSTEM VERSIONING")
                && contains(&ddl, "WITH SYSTEM VERSIONING"),
            format!("versioned ddl {ddl}"),
        )?;
        apply(
            s,
            json!({"change": "alterTable", "database": db, "name": "v", "systemVersioned": true, "operations": [
                {"op": "addColumn", "column": {"name": "z", "dataType": {"name": "int"}}},
                {"op": "dropPeriod", "name": "validity"},
                {"op": "addPeriod", "period": {"name": "validity2", "start": "valid_from", "end": "valid_to"}}
            ]}),
        )
        .await?;
        let ddl = show_create(s, &format!("SHOW CREATE TABLE `{db}`.v")).await?;
        check(contains(&ddl, "`z` int") && contains(&ddl, "PERIOD FOR `validity2`") && !contains(&ddl, "PERIOD FOR `validity` "), format!("altered versioned ddl {ddl}"))?;
        let plan = apply(
            s,
            json!({"change": "alterTable", "database": db, "name": "v", "operations": [
                {"op": "dropSystemVersioning", "columns": {"start": "rs", "end": "re"}}]}),
        )
        .await?;
        check(plan.destructive, "drop versioning destructive")?;
        check(one(s, &table_type("v")).await? == "BASE TABLE", "versioning dropped")?;
        check(find(&columns(s, db, "v").await?, "rs").is_err(), "row start column dropped")?;

        exec(s, &format!("CREATE TABLE `{db}`.w (id INT)")).await?;
        apply(s, json!({"change": "alterTable", "database": db, "name": "w", "operations": [{"op": "addSystemVersioning"}]})).await?;
        check(one(s, &table_type("w")).await? == "SYSTEM VERSIONED", "implicit versioning added")?;
        apply(s, json!({"change": "alterTable", "database": db, "name": "w", "operations": [{"op": "dropSystemVersioning"}]})).await?;
        check(one(s, &table_type("w")).await? == "BASE TABLE", "implicit versioning dropped")?;
        exec(s, &format!("CREATE TABLE `{db}`.x (id INT)")).await?;
        apply(
            s,
            json!({"change": "alterTable", "database": db, "name": "x", "operations": [{"op": "addSystemVersioning", "columns": [
                {"name": "vs", "dataType": {"name": "timestamp", "length": 6}, "rowPeriod": "start", "invisible": true},
                {"name": "ve", "dataType": {"name": "timestamp", "length": 6}, "rowPeriod": "end", "invisible": true}]}]}),
        )
        .await?;
        check(one(s, &table_type("x")).await? == "SYSTEM VERSIONED", "explicit versioning added")?;
        check(
            contains(&show_create(s, &format!("SHOW CREATE TABLE `{db}`.x")).await?, "PERIOD FOR SYSTEM_TIME (`vs`, `ve`)"),
            "explicit versioning period",
        )?;
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn renames_carry_over_to_later_statements() -> TestResult {
    on_every_server("m2_myedit_renames", async |s: &MysqlSession, db: &str| {
        let maria = mariadb(s);
        exec(s, &format!("CREATE DATABASE IF NOT EXISTS `{db}_other`")).await?;
        let result = async {
            exec(s, &format!("CREATE TABLE `{db}`.t (id INT PRIMARY KEY, a INT)")).await?;
            let mut ops = vec![
                json!({"op": "rename", "name": "t2"}),
                json!({"op": "renameColumn", "oldName": "a", "newName": "b"}),
                json!({"op": "addIndex", "index": {"name": "ixb", "kind": "index", "parts": [{"kind": "column", "name": "b"}]}}),
                json!({"op": "partitionBy", "partitioning": {"method": {"kind": "key", "columns": ["id"]}, "count": 2}}),
                json!({"op": "rename", "database": format!("{db}_other"), "name": "t3"}),
                json!({"op": "truncatePartitions"}),
                json!({"op": "removePartitioning"}),
            ];
            if maria {
                ops.push(json!({"op": "addSystemVersioning"}));
            }
            let plan = apply(s, json!({"change": "alterTable", "database": db, "name": "t", "operations": ops})).await?;
            check(plan.statements.len() >= 5, format!("statements {:?}", plan.statements))?;
            let other = format!("{db}_other");
            check(
                count(s, &format!("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA IN ('{db}', '{other}')")).await? == 1,
                "one table left",
            )?;
            check(index_columns(s, &other, "t3", "ixb").await? == ["b"], "index on the renamed column")?;
            if maria {
                check(
                    one(s, &format!("SELECT TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{other}' AND TABLE_NAME = 't3'")).await? == "SYSTEM VERSIONED",
                    "versioning on the renamed table",
                )?;
            }
            Ok(())
        }
        .await;
        exec(s, &format!("DROP DATABASE IF EXISTS `{db}_other`")).await?;
        result
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn text_defaults_are_set_per_engine() -> TestResult {
    on_every_server("m2_myedit_text_defaults", async |s: &MysqlSession, db: &str| {
        exec(s, &format!("CREATE TABLE `{db}`.t (id INT PRIMARY KEY, tx TEXT, j JSON, b BLOB)")).await?;
        let constant = |name: &str, ty: &str, value: Json| {
            json!({"op": "setColumnDefault", "name": name, "columnType": ty, "default": {"kind": "literal", "value": value}})
        };
        let ops = vec![
            constant("tx", "text", json!({"t": "text", "v": "x"})),
            constant("j", "json", json!({"t": "text", "v": "[]"})),
            constant("b", "blob", json!({"t": "bytes", "v": "00ff"})),
        ];
        if mariadb(s) {
            apply(s, json!({"change": "alterTable", "database": db, "name": "t", "operations": ops})).await?;
        } else {
            // MySQL refuses SET DEFAULT with a constant on these types;
            // the plan says so and modifyColumn does it instead.
            let change: MyChange = serde_json::from_value(json!({"change": "alterTable", "database": db, "name": "t", "operations": ops}))?;
            check(plan(s.server_info(), &change).is_err(), "MySQL SET DEFAULT on TEXT refused")?;
            let column = |name: &str, ty: &str, value: Option<Json>| {
                let mut c = json!({"name": name, "dataType": {"name": ty}});
                if let Some(value) = value {
                    c["default"] = json!({"kind": "literal", "value": value});
                }
                c
            };
            let modify = |name: &str, ty: &str, value: Json| {
                json!({"op": "modifyColumn", "previous": column(name, ty, None), "column": column(name, ty, Some(value))})
            };
            let plan = apply(
                s,
                json!({"change": "alterTable", "database": db, "name": "t", "algorithm": "instant", "operations": [
                    modify("tx", "text", json!({"t": "text", "v": "x"})),
                    modify("j", "json", json!({"t": "text", "v": "[]"})),
                    modify("b", "blob", json!({"t": "bytes", "v": "00ff"}))
                ]}),
            )
            .await?;
            check(plan.warnings.is_empty(), format!("instant default change {:?}", plan.warnings))?;
        }
        exec(s, &format!("INSERT INTO `{db}`.t (id) VALUES (1)")).await?;
        check(
            row(s, &format!("SELECT tx, j, HEX(b) FROM `{db}`.t")).await? == ["x", "[]", "00FF"],
            "defaults applied",
        )?;
        apply(
            s,
            json!({"change": "alterTable", "database": db, "name": "t", "operations": [
                {"op": "setColumnDefault", "name": "tx", "columnType": "text"}]}),
        )
        .await?;
        // Without a default MySQL makes INSERTs name the column (strict
        // mode), even when it is nullable; MariaDB falls back to NULL.
        check(
            one(s, &format!("SELECT IFNULL(COLUMN_DEFAULT, 'none') FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 't' AND COLUMN_NAME = 'tx'")).await? != "x",
            "default dropped",
        )?;
        exec(s, &format!("INSERT INTO `{db}`.t (id, tx) VALUES (2, NULL)")).await?;
        check(count(s, &format!("SELECT COUNT(*) FROM `{db}`.t")).await? == 2, "rows")
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn multi_event_triggers_and_sequence_retyping() -> TestResult {
    on_every_server("m2_myedit_mariadb12", async |s: &MysqlSession, db: &str| {
        let multi = mariadb(s) && s.server_info().at_least(12, 0);
        exec(s, &format!("CREATE TABLE `{db}`.t (id INT, n INT)")).await?;
        let trigger = json!({"change": "createTrigger", "database": db, "table": "t", "name": "tr",
            "timing": "before", "events": ["insert", "update"], "body": "SET NEW.n = 7"});
        if !multi {
            let change: MyChange = serde_json::from_value(trigger)?;
            check(plan(s.server_info(), &change).is_err(), "multi-event triggers refused")?;
            return Ok(());
        }
        apply(s, trigger).await?;
        check(
            one(s, &format!("SELECT EVENT_MANIPULATION FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = '{db}' AND TRIGGER_NAME = 'tr'")).await? == "INSERT,UPDATE",
            "trigger events",
        )?;
        exec(s, &format!("INSERT INTO `{db}`.t VALUES (1, 0)")).await?;
        exec(s, &format!("UPDATE `{db}`.t SET n = 0")).await?;
        check(one(s, &format!("SELECT n FROM `{db}`.t")).await? == "7", "fired on update")?;

        apply(s, json!({"change": "createSequence", "database": db, "name": "s"})).await?;
        let plan = apply(
            s,
            json!({"change": "alterSequence", "database": db, "name": "s",
                "dataType": {"name": "int"}, "maxValue": {"kind": "value", "value": 2147483646}}),
        )
        .await?;
        check(plan.statements.len() == 2, "retype in two statements")?;
        let ddl = show_create(s, &format!("SHOW CREATE SEQUENCE `{db}`.s")).await?;
        check(contains(&ddl, "as int") && contains(&ddl, "maxvalue 2147483646"), format!("retyped {ddl}"))?;
        let plan = apply(s, json!({"change": "alterSequence", "database": db, "name": "s", "restart": {"kind": "start"}})).await?;
        check(plan.destructive, "restart destructive")
    })
    .await
}

/// One `ALTER TABLE` case of the algorithm matrix: setup statements run
/// on the fixture table first, then the operations.
struct AlgorithmCase {
    name: &'static str,
    setup: &'static str,
    ops: Vec<Json>,
    mysql_only: bool,
    mariadb_only: bool,
}

fn case(name: &'static str, op: Json) -> AlgorithmCase {
    AlgorithmCase {
        name,
        setup: "",
        ops: vec![op],
        mysql_only: false,
        mariadb_only: false,
    }
}

fn varchar(name: &str, length: u32) -> Json {
    json!({"name": name, "dataType": {"name": "varchar", "length": length, "charset": "utf8mb4"}})
}

fn algorithm_cases(c_collation: &str) -> Vec<AlgorithmCase> {
    let int = |name: &str| json!({"name": name, "dataType": {"name": "int"}});
    let modify = |previous: Json, column: Json| json!({"op": "modifyColumn", "previous": previous, "column": column});
    let mut not_null_a = int("a");
    not_null_a["nullable"] = json!(false);
    not_null_a["default"] = json!({"kind": "literal", "value": {"t": "int", "v": 0}});
    let mut null_a = not_null_a.clone();
    null_a["nullable"] = json!(true);
    let mut not_null_d = int("d");
    not_null_d["nullable"] = json!(false);
    let mut default_d = int("d");
    default_d["default"] = json!({"kind": "literal", "value": {"t": "int", "v": 5}});
    let mut commented = varchar("c", 10);
    commented["comment"] = json!("k");
    let mut collated_old = varchar("c", 10);
    collated_old["dataType"]["collation"] = json!(c_collation);
    let mut collated_new = varchar("c", 10);
    collated_new["dataType"]["collation"] = json!("utf8mb4_bin");
    let mut latin = varchar("c", 10);
    latin["dataType"]["charset"] = json!("latin1");
    let en =
        |values: &[&str]| json!({"name": "en", "dataType": {"name": "enum", "values": values}});
    let virt = |expr: &str| json!({"name": "g", "dataType": {"name": "int"}, "generated": {"expression": expr, "storage": "virtual"}});
    let mut invisible_d = int("d");
    invisible_d["invisible"] = json!(true);
    let mut cases = vec![
        case(
            "addColumnEnd",
            json!({"op": "addColumn", "column": int("z")}),
        ),
        case(
            "addColumnFirst",
            json!({"op": "addColumn", "column": int("z"), "position": {"kind": "first"}}),
        ),
        case(
            "addColumnStored",
            json!({"op": "addColumn", "column": {"name": "s", "dataType": {"name": "int"}, "generated": {"expression": "b + 2", "storage": "stored"}}}),
        ),
        case(
            "addColumnVirtual",
            json!({"op": "addColumn", "column": virt("b + 2")}),
        ),
        case("dropColumn", json!({"op": "dropColumn", "name": "d"})),
        case(
            "dropColumnIndexed",
            json!({"op": "dropColumn", "name": "a", "indexed": true}),
        ),
        case(
            "renameColumn",
            json!({"op": "renameColumn", "oldName": "d", "newName": "d2"}),
        ),
        case(
            "setDefault",
            json!({"op": "setColumnDefault", "name": "d", "columnType": "int", "default": {"kind": "literal", "value": {"t": "int", "v": 3}}}),
        ),
        case(
            "dropDefault",
            json!({"op": "setColumnDefault", "name": "a", "columnType": "int"}),
        ),
        case(
            "addIndex",
            json!({"op": "addIndex", "index": {"name": "i2", "kind": "index", "parts": [{"kind": "column", "name": "d"}]}}),
        ),
        case(
            "addUnique",
            json!({"op": "addIndex", "index": {"name": "u2", "kind": "unique", "parts": [{"kind": "column", "name": "d"}]}}),
        ),
        case(
            "addFulltext",
            json!({"op": "addIndex", "index": {"name": "f", "kind": "fulltext", "parts": [{"kind": "column", "name": "tx"}]}}),
        ),
        case("dropIndex", json!({"op": "dropIndex", "name": "ix"})),
        case(
            "renameIndex",
            json!({"op": "renameIndex", "oldName": "ix", "newName": "ix2"}),
        ),
        case(
            "indexVisibility",
            json!({"op": "setIndexVisibility", "name": "ix", "invisible": true}),
        ),
        case(
            "addCheck",
            json!({"op": "addConstraint", "constraint": {"kind": "check", "name": "ck2", "expression": "d < 1000"}}),
        ),
        case("dropCheck", json!({"op": "dropCheck", "name": "ck"})),
        case(
            "addFk",
            json!({"op": "addConstraint", "constraint": {"kind": "foreignKey", "name": "fk2", "columns": ["d"], "referencesTable": "p", "referencesColumns": ["id"]}}),
        ),
        case("dropFk", json!({"op": "dropForeignKey", "name": "fe"})),
        AlgorithmCase {
            ops: vec![
                json!({"op": "dropPrimaryKey"}),
                json!({"op": "addConstraint", "constraint": {"kind": "primaryKey", "parts": [{"kind": "column", "name": "id"}, {"kind": "column", "name": "a"}]}}),
            ],
            ..case("dropAddPk", Json::Null)
        },
        case("dropPk", json!({"op": "dropPrimaryKey"})),
        AlgorithmCase {
            setup: "ALTER TABLE t DROP PRIMARY KEY",
            ..case(
                "addPk",
                json!({"op": "addConstraint", "constraint": {"kind": "primaryKey", "parts": [{"kind": "column", "name": "id"}]}}),
            )
        },
        case("renameTable", json!({"op": "rename", "name": "t_new"})),
        case(
            "optComment",
            json!({"op": "setOptions", "options": {"comment": "x"}}),
        ),
        case(
            "optRowFormat",
            json!({"op": "setOptions", "options": {"rowFormat": "compact"}}),
        ),
        case(
            "optAutoIncrement",
            json!({"op": "setOptions", "options": {"autoIncrement": 100}}),
        ),
        case(
            "optStats",
            json!({"op": "setOptions", "options": {"statsPersistent": "on"}}),
        ),
        case(
            "optCharset",
            json!({"op": "setOptions", "options": {"charset": "latin1"}}),
        ),
        AlgorithmCase {
            setup: "ALTER TABLE t DROP FOREIGN KEY fe",
            ..case(
                "optEngine",
                json!({"op": "setOptions", "options": {"engine": "MyISAM"}}),
            )
        },
        case(
            "convert",
            json!({"op": "convertCharset", "charset": "latin1"}),
        ),
        case("modComment", modify(varchar("c", 10), commented)),
        case("modWiden", modify(varchar("c", 10), varchar("c", 20))),
        case(
            "modWidenBoundary",
            modify(varchar("c", 10), varchar("c", 100)),
        ),
        case("modNarrow", modify(varchar("c", 10), varchar("c", 5))),
        case(
            "modIntToBigint",
            modify(
                int("d"),
                json!({"name": "d", "dataType": {"name": "bigint"}}),
            ),
        ),
        case("modNotNull", modify(int("d"), not_null_d)),
        case("modNull", modify(not_null_a, null_a)),
        case("modDefault", modify(int("d"), default_d)),
        case(
            "modReorder",
            json!({"op": "modifyColumn", "previous": int("d"), "column": int("d"), "position": {"kind": "after", "column": "id"}}),
        ),
        case(
            "modEnumAppend",
            modify(en(&["a", "b"]), en(&["a", "b", "c"])),
        ),
        case("modEnumReorder", modify(en(&["a", "b"]), en(&["b", "a"]))),
        case("modCollation", modify(collated_old, collated_new)),
        case("modCharset", modify(varchar("c", 10), latin)),
        AlgorithmCase {
            setup: "ALTER TABLE t ADD COLUMN g INT AS (b + 1) VIRTUAL",
            ..case("modVirtualExpr", modify(virt("b + 1"), virt("b + 5")))
        },
        case(
            "changeRename",
            json!({"op": "changeColumn", "previous": int("d"), "column": int("d2")}),
        ),
        case(
            "changeRenameWiden",
            json!({"op": "changeColumn", "previous": varchar("c", 10), "column": varchar("c2", 20)}),
        ),
    ];
    let mysql_only = [
        case(
            "columnInvisible",
            json!({"op": "setColumnVisibility", "name": "d", "invisible": true}),
        ),
        case(
            "checkNotEnforced",
            json!({"op": "setCheckEnforced", "name": "ck", "enforced": false}),
        ),
        AlgorithmCase {
            setup: "ALTER TABLE t ALTER CHECK ck NOT ENFORCED",
            ..case(
                "checkEnforced",
                json!({"op": "setCheckEnforced", "name": "ck", "enforced": true}),
            )
        },
        case("modInvisible", modify(int("d"), invisible_d.clone())),
        case(
            "modTextDefault",
            modify(
                json!({"name": "tx", "dataType": {"name": "text"}}),
                json!({"name": "tx", "dataType": {"name": "text"}, "default": {"kind": "literal", "value": {"t": "text", "v": "x"}}}),
            ),
        ),
    ];
    let mariadb_only = [
        case("modInvisibleMaria", modify(int("d"), invisible_d)),
        case(
            "setDefaultText",
            json!({"op": "setColumnDefault", "name": "tx", "columnType": "text", "default": {"kind": "literal", "value": {"t": "text", "v": "x"}}}),
        ),
        AlgorithmCase {
            setup: "ALTER TABLE t ADD COLUMN ps DATE NOT NULL DEFAULT '2000-01-01', ADD COLUMN pe DATE NOT NULL DEFAULT '2001-01-01'",
            ..case(
                "addPeriod",
                json!({"op": "addPeriod", "period": {"name": "app", "start": "ps", "end": "pe"}}),
            )
        },
        AlgorithmCase {
            setup: "ALTER TABLE t ADD COLUMN ps DATE NOT NULL DEFAULT '2000-01-01', ADD COLUMN pe DATE NOT NULL DEFAULT '2001-01-01', ADD PERIOD FOR app (ps, pe)",
            ..case("dropPeriod", json!({"op": "dropPeriod", "name": "app"}))
        },
    ];
    cases.extend(mysql_only.into_iter().map(|c| AlgorithmCase {
        mysql_only: true,
        ..c
    }));
    cases.extend(mariadb_only.into_iter().map(|c| AlgorithmCase {
        mariadb_only: true,
        ..c
    }));
    cases
}

/// Runs every operation with every algorithm on every server and checks
/// that the plan warns "the server rejects ALGORITHM=..." exactly when the
/// server rejects it.
#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn algorithms() -> TestResult {
    on_every_server("m2_myedit_algorithms", async |s: &MysqlSession, db: &str| {
        let maria = mariadb(s);
        let fixture = [
            format!("DROP TABLE IF EXISTS `{db}`.t, `{db}`.t_new"),
            format!("DROP TABLE IF EXISTS `{db}`.p"),
            format!("CREATE TABLE `{db}`.p (id INT PRIMARY KEY)"),
            format!(
                "CREATE TABLE `{db}`.t (id INT PRIMARY KEY, a INT NOT NULL DEFAULT 0, b INT, c VARCHAR(10), d INT, e INT, en ENUM('a','b'), tx TEXT, \
                 KEY ix (a), CONSTRAINT ck CHECK (b > -100), CONSTRAINT fe FOREIGN KEY (e) REFERENCES `{db}`.p (id)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
            ),
        ];
        for statement in &fixture {
            exec(s, statement).await?;
        }
        let c_collation = one(s, &format!("SELECT COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 't' AND COLUMN_NAME = 'c'")).await?;
        let algorithms: &[&str] = if maria { &["instant", "nocopy", "inplace"] } else { &["instant", "inplace"] };
        let mut mismatches = Vec::new();
        let mut trials = 0;
        for case in algorithm_cases(&c_collation) {
            if (case.mysql_only && maria) || (case.mariadb_only && !maria) {
                continue;
            }
            for algorithm in algorithms {
                for statement in &fixture {
                    exec(s, statement).await?;
                }
                if !case.setup.is_empty() {
                    exec(s, &format!("USE `{db}`")).await?;
                    exec(s, case.setup).await?;
                }
                let change: MyChange = serde_json::from_value(json!({"change": "alterTable", "database": db, "name": "t",
                    "operations": case.ops, "algorithm": algorithm}))?;
                let plan = plan(s.server_info(), &change)?;
                let predicted = plan.warnings.iter().any(|w| w.contains("so the server rejects ALGORITHM="));
                let mut error = None;
                for statement in &plan.statements {
                    if let Err(e) = s.execute(statement, &ExecOptions::default()).await {
                        error = Some(e.to_string());
                        break;
                    }
                }
                trials += 1;
                if predicted != error.is_some() {
                    mismatches.push(format!(
                        "{} ALGORITHM={algorithm}: plan predicted rejection={predicted}, server said {:?}; warnings {:?}",
                        case.name, error, plan.warnings
                    ));
                }
            }
        }
        check(trials > 100, format!("only {trials} trials ran"))?;
        check(mismatches.is_empty(), mismatches.join("\n"))
    })
    .await
}
