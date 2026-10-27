//! Index redefinition round trip through the UI, against the shared M2
//! database matrix (PostgreSQL 13 and 18):
//!
//! ```sh
//! cargo test -p varve-catalog --test ui_index_round_trip -- --include-ignored
//! ```
//!
//! The table designer redefines an index from what `describe` reports
//! (drop, then create from the read keys). This test closes that loop with
//! the real UI code in the middle, through two sample files per version
//! under `packages/ui/src/api/__contract__/samples`:
//!
//! 1. Fixtures with indexes that are easy to get wrong (mixed-case and
//!    quote-containing column names, expressions, collations, operator
//!    classes from another schema and with parameters, INCLUDE, storage
//!    parameters, a tablespace, a predicate, a comment, `NULLS NOT
//!    DISTINCT`, an access method from an extension, the replica identity
//!    index, CLUSTER ON, column statistics targets, storage parameters in
//!    their own order, a partitioned table and its partition) are described.
//!    Each table must equal `pg-index-describe.<table>.pg<major>.json`; with
//!    `VARVE_UPDATE_CONTRACT=1` the files are written instead.
//! 2. The UI test `api/__contract__/indexRoundTrip.test.ts` turns that
//!    sample into the designer's changes and checks them against
//!    `pg-index-redefine.<table>.pg<major>.json`.
//! 3. Here those changes are planned and run, and every index must come
//!    back exactly as it was (`pg_get_indexdef`, comment, tablespace,
//!    replica identity, CLUSTER ON, statistics targets).
//!
//! Everything is named `m2_uicontract*` and dropped at the start and the end
//! of a run.

use std::error::Error;
use std::path::PathBuf;

use serde_json::Value as Json;
use varve_catalog::model::{ObjectKind, ObjectRef};
use varve_catalog::mysql::edit::MyChange;
use varve_catalog::postgres::edit::PgChange;
use varve_catalog::{catalog_for, open_catalog};
use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, ExecOptions, ExecOutput, Session, StatementResult, Value,
};
use varve_driver_mysql::MysqlDriver;
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const SCHEMA: &str = "m2_uicontract";
const TABLESPACE: &str = "m2_uicontract_ts";
const TS_DIR: &str = "/tmp/m2_uicontract_ts";

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

async fn connect(port: u16) -> TestResult<Box<dyn Session>> {
    let mut params = ConnectParams::new(
        Engine::Postgres,
        env_or("VARVE_M2_PG_HOST", "127.0.0.1"),
        port,
    );
    params.database = Some("varve".to_owned());
    params.username = Some(env_or("VARVE_M2_PG_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_M2_PG_PASSWORD", "varve_dev")));
    params.application_name = "varve-catalog ui_index_round_trip tests".into();
    Ok(PostgresDriver::new().connect(&params).await?)
}

async fn exec(session: &dyn Session, sql: &str) -> TestResult<ExecOutput> {
    session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}\n=> {e}").into())
}

/// Rows of the first result as text.
async fn rows(session: &dyn Session, sql: &str) -> TestResult<Vec<Vec<Option<String>>>> {
    let out = exec(session, sql).await?;
    let Some(StatementResult::Rows { rows, .. }) = out.results.into_iter().next() else {
        return Err(format!("no rows from {sql}").into());
    };
    Ok(rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|v| match v {
                    Value::Null => None,
                    Value::Text(t) => Some(t),
                    other => Some(format!("{other:?}")),
                })
                .collect()
        })
        .collect())
}

async fn cleanup(session: &dyn Session) -> TestResult {
    exec(session, &format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE")).await?;
    exec(session, &format!("DROP TABLESPACE IF EXISTS {TABLESPACE}")).await?;
    exec(
        session,
        &format!("COPY (SELECT 1) TO PROGRAM 'rm -rf {TS_DIR}'"),
    )
    .await?;
    Ok(())
}

fn fixtures(major: u32) -> String {
    let mut sql = format!(
        "CREATE SCHEMA {SCHEMA};
         CREATE EXTENSION IF NOT EXISTS bloom WITH SCHEMA {SCHEMA};
         COPY (SELECT 1) TO PROGRAM 'rm -rf {TS_DIR} && mkdir -p {TS_DIR} && chmod 700 {TS_DIR}';
         CREATE TABLESPACE {TABLESPACE} LOCATION '{TS_DIR}';
         CREATE TABLE {SCHEMA}.\"Orders\" (
           \"OrderId\" int4 NOT NULL,
           \"we\"\"ird\" text,
           label text COLLATE \"C\",
           body tsvector,
           tags int4[],
           note varchar(20),
           amount numeric,
           code int4,
           CONSTRAINT \"Orders_pkey\" PRIMARY KEY (\"OrderId\")
         );
         CREATE INDEX \"Orders_Mixed\" ON {SCHEMA}.\"Orders\"
           (\"OrderId\" DESC NULLS LAST, \"we\"\"ird\" COLLATE \"POSIX\" text_pattern_ops)
           INCLUDE (amount, label) WITH (fillfactor = 70, deduplicate_items = off)
           TABLESPACE {TABLESPACE} WHERE amount > 0;
         COMMENT ON INDEX {SCHEMA}.\"Orders_Mixed\" IS 'mixed keys';
         CREATE INDEX orders_expr ON {SCHEMA}.\"Orders\" (lower(\"we\"\"ird\"), (amount + 1), note, label);
         CREATE INDEX orders_body ON {SCHEMA}.\"Orders\" USING gist (body tsvector_ops (siglen = 100));
         CREATE INDEX orders_tags ON {SCHEMA}.\"Orders\" USING gin (tags);
         CREATE INDEX orders_bloom ON {SCHEMA}.\"Orders\" USING bloom (\"OrderId\") WITH (length = 80);
         CREATE UNIQUE INDEX orders_replid ON {SCHEMA}.\"Orders\" (\"OrderId\" DESC);
         ALTER TABLE {SCHEMA}.\"Orders\" REPLICA IDENTITY USING INDEX orders_replid;
         CREATE UNIQUE INDEX orders_replid_varve_swap ON {SCHEMA}.\"Orders\" (\"OrderId\");
         CREATE INDEX orders_clu ON {SCHEMA}.\"Orders\" (note);
         ALTER TABLE {SCHEMA}.\"Orders\" CLUSTER ON orders_clu;
         CREATE INDEX orders_stats ON {SCHEMA}.\"Orders\" ((amount * 2), (lower(label) COLLATE \"default\"));
         ALTER INDEX {SCHEMA}.orders_stats ALTER COLUMN 1 SET STATISTICS 500;
         CREATE INDEX orders_brin ON {SCHEMA}.\"Orders\" USING brin (amount) WITH (pages_per_range = 32, autosummarize = on);
         CREATE UNIQUE INDEX orders_ref ON {SCHEMA}.\"Orders\" (code);
         CREATE TABLE {SCHEMA}.refs (code int4 REFERENCES {SCHEMA}.\"Orders\" (code));
         CREATE TABLE {SCHEMA}.parted (k int4, v int4) PARTITION BY RANGE (k);
         CREATE TABLE {SCHEMA}.parted_p1 PARTITION OF {SCHEMA}.parted FOR VALUES FROM (0) TO (100);
         CREATE INDEX parted_k ON {SCHEMA}.parted (k);"
    );
    if major >= 15 {
        sql.push_str(&format!(
            "\nCREATE UNIQUE INDEX orders_nnd ON {SCHEMA}.\"Orders\" (note) NULLS NOT DISTINCT;"
        ));
    }
    sql
}

/// Sizes and estimates change from run to run.
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
                // MySQL and MariaDB table statistics and times.
                "created",
                "updated",
                "checked",
                "dataLength",
                "indexLength",
                "dataFree",
                "avgRowLength",
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

/// Everything an index is, for every index of the schema: definition,
/// comment, tablespace, replica identity, CLUSTER ON, column statistics
/// targets, and the table's replica identity setting.
async fn index_state(session: &dyn Session) -> TestResult<Vec<Vec<Option<String>>>> {
    rows(
        session,
        &format!(
            "SELECT c.relname::text, pg_catalog.pg_get_indexdef(c.oid), pg_catalog.obj_description(c.oid, 'pg_class'),
                    COALESCE(ts.spcname::text, ''), i.indisreplident::text, i.indisclustered::text,
                    (SELECT pg_catalog.string_agg(COALESCE(a.attstattarget::int4, -1)::text, ',' ORDER BY a.attnum)
                       FROM pg_catalog.pg_attribute a WHERE a.attrelid = c.oid),
                    t.relreplident::text
             FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
             JOIN pg_catalog.pg_class t ON t.oid = i.indrelid
             LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid = c.reltablespace
             WHERE t.relnamespace = '{SCHEMA}'::regnamespace
               AND c.relname::text NOT LIKE '%varve\\_swap%' ORDER BY 1"
        ),
    )
    .await
}

/// The fixture tables, with the sample name and how many of their indexes
/// the designer may redefine (all but the constraint indexes, the index a
/// foreign key needs, and a partition's index attached to its parent).
fn tables(major: u32) -> [(&'static str, &'static str, usize); 3] {
    [
        // Plus the leftover of a failed earlier swap, which the plan drops.
        ("Orders", "orders", if major >= 15 { 11 } else { 10 }),
        ("parted", "parted", 1),
        ("parted_p1", "parted_p1", 0),
    ]
}

async fn round_trip(major: u32, port: u16) -> TestResult {
    let session = connect(port).await?;
    assert_eq!(session.server_info().major, major);
    cleanup(session.as_ref()).await?;
    let result = run(major, session.as_ref()).await;
    let cleaned = cleanup(session.as_ref()).await;
    result?;
    cleaned
}

async fn run(major: u32, session: &dyn Session) -> TestResult {
    exec(session, &fixtures(major)).await?;
    let catalog = catalog_for(session.server_info())?;
    let update = std::env::var("VARVE_UPDATE_CONTRACT").is_ok_and(|v| v == "1");
    let before = index_state(session).await?;
    let mut all_changes = Vec::new();
    let mut missing = Vec::new();
    for (table, slug, redefinable) in tables(major) {
        // 1. What describe reports equals the sample the UI test reads.
        let object = ObjectRef {
            kind: ObjectKind::Table,
            database: Some("varve".into()),
            schema: Some(SCHEMA.into()),
            parent: None,
            name: table.into(),
            signature: None,
        };
        let mut described = serde_json::to_value(catalog.describe(session, &object).await?)?;
        normalize(&mut described);
        let describe_file = samples().join(format!("pg-index-describe.{slug}.pg{major}.json"));
        if update {
            std::fs::create_dir_all(samples())?;
            std::fs::write(
                &describe_file,
                serde_json::to_string_pretty(&described)? + "\n",
            )?;
        }
        let sample: Json =
            serde_json::from_str(&std::fs::read_to_string(&describe_file).map_err(|e| {
                format!(
                    "{}: {e} (run with VARVE_UPDATE_CONTRACT=1 to write it)",
                    describe_file.display()
                )
            })?)?;
        assert_eq!(
            described,
            sample,
            "describe differs from {}; run with VARVE_UPDATE_CONTRACT=1 and review the UI test",
            describe_file.display()
        );

        // 2. The changes the UI builds from that sample.
        let redefine_file = samples().join(format!("pg-index-redefine.{slug}.pg{major}.json"));
        if !redefine_file.exists() {
            missing.push(redefine_file.display().to_string());
            continue;
        }
        let changes: Vec<Json> =
            serde_json::from_str(&std::fs::read_to_string(&redefine_file).map_err(|e| {
                format!(
                    "{}: {e} (the UI test api/__contract__/indexRoundTrip.test.ts writes it)",
                    redefine_file.display()
                )
            })?)?;
        let redefined: Vec<&str> = changes
            .iter()
            .filter(|c| c["change"]["action"] == "drop")
            .filter_map(|c| c["change"]["name"]["name"].as_str())
            .collect();
        assert_eq!(redefined.len(), redefinable, "{table}: {redefined:?}");
        all_changes.extend(changes);
    }
    if !missing.is_empty() {
        return Err(format!(
            "the UI test api/__contract__/indexRoundTrip.test.ts writes {missing:?}"
        )
        .into());
    }

    // 3. Run them; every index comes back exactly as it was.
    for change in all_changes {
        assert_eq!(change["engine"], "postgres");
        let change: PgChange = serde_json::from_value(change["change"].clone())?;
        let plan = varve_catalog::postgres::edit::plan(session.server_info(), &change)?;
        if plan.transactional && plan.statements.len() > 1 {
            exec(session, "BEGIN").await?;
            for statement in &plan.statements {
                exec(session, statement).await?;
            }
            exec(session, "COMMIT").await?;
        } else {
            for statement in &plan.statements {
                exec(session, statement).await?;
            }
        }
    }
    let after = index_state(session).await?;
    assert_eq!(before, after);
    // No swap index is left behind, also not the earlier attempt's.
    let swaps = rows(
        session,
        &format!(
            "SELECT c.relname::text FROM pg_catalog.pg_class c \
             WHERE c.relnamespace = '{SCHEMA}'::regnamespace AND c.relname::text LIKE '%varve\\_swap%'"
        ),
    )
    .await?;
    assert!(swaps.is_empty(), "{swaps:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn index_redefinition_round_trips_on_postgres_13() -> TestResult {
    round_trip(13, port("VARVE_M2_PG13_PORT", 55513)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn index_redefinition_round_trips_on_postgres_18() -> TestResult {
    round_trip(18, port("VARVE_M2_PG18_PORT", 55518)).await
}

// ---------------------------------------------------------------------------
// MySQL and MariaDB

const MY_DB: &str = "m2_uicontract";

async fn my_connect(engine: Engine, port: u16) -> TestResult<Box<dyn Session>> {
    let mut params = ConnectParams::new(engine, env_or("VARVE_M2_MYSQL_HOST", "127.0.0.1"), port);
    params.username = Some(env_or("VARVE_M2_MYSQL_USER", "root"));
    params.password = Some(Secret::new(env_or("VARVE_M2_MYSQL_PASSWORD", "varve_dev")));
    params.tls.mode = varve_driver::TlsMode::Disable;
    params.application_name = "varve-catalog ui_index_round_trip tests".into();
    Ok(MysqlDriver::new().connect(&params).await?)
}

/// Indexes the MySQL designer gets wrong easily: a MEMORY table whose
/// BTREE index must stay BTREE (HASH is the default there), a unique index
/// with USING BTREE, a spatial index (SUB_PART reports 32), a full-text
/// index with the ngram parser and functional key parts with string
/// literals (MySQL; STATISTICS escapes their quotes).
fn my_fixtures(mariadb: bool) -> Vec<String> {
    let mut sql = vec![
        format!("CREATE DATABASE {MY_DB}"),
        format!(
            "CREATE TABLE {MY_DB}.mem (k int, v int, KEY kb (k) USING BTREE, KEY kh (v) USING HASH) ENGINE=MEMORY"
        ),
    ];
    if mariadb {
        sql.push(format!(
            "CREATE TABLE {MY_DB}.items (id int PRIMARY KEY, a int, c varchar(20), f text, g geometry NOT NULL, \
             UNIQUE KEY u_ac (a, c) USING BTREE, FULLTEXT KEY ft (f), SPATIAL KEY sp (g)) ENGINE=InnoDB"
        ));
    } else {
        sql.push(format!(
            "CREATE TABLE {MY_DB}.items (id int PRIMARY KEY, a int, c varchar(20), f text, j json, g geometry NOT NULL SRID 0, \
             UNIQUE KEY u_ac (a, c) USING BTREE, \
             KEY k_json ((cast(json_extract(j, '$.tags') as unsigned array))), \
             KEY k_concat ((concat(c, 'x'))), \
             FULLTEXT KEY ft (f) WITH PARSER ngram, SPATIAL KEY sp (g)) ENGINE=InnoDB"
        ));
    }
    sql
}

/// The key lines of `SHOW CREATE TABLE`, sorted (a redefined index moves
/// to the end of the list).
async fn my_keys(session: &dyn Session, table: &str) -> TestResult<Vec<String>> {
    let out = rows(session, &format!("SHOW CREATE TABLE {MY_DB}.{table}")).await?;
    let ddl = out
        .first()
        .and_then(|r| r.get(1).cloned().flatten())
        .ok_or("no SHOW CREATE TABLE")?;
    let mut keys: Vec<String> = ddl
        .lines()
        .map(|l| l.trim().trim_end_matches(',').to_owned())
        .filter(|l| l.contains("KEY "))
        .collect();
    keys.sort();
    Ok(keys)
}

async fn my_round_trip(engine: Engine, port: u16, server: &str) -> TestResult {
    let session = my_connect(engine, port).await?;
    let session = session.as_ref();
    let mariadb = engine == Engine::Mariadb;
    exec(session, &format!("DROP DATABASE IF EXISTS {MY_DB}")).await?;
    let result = my_run(session, mariadb, server).await;
    let cleaned = exec(session, &format!("DROP DATABASE IF EXISTS {MY_DB}")).await;
    result?;
    cleaned?;
    Ok(())
}

async fn my_run(session: &dyn Session, mariadb: bool, server: &str) -> TestResult {
    for sql in my_fixtures(mariadb) {
        exec(session, &sql).await?;
    }
    let catalog = open_catalog(session).await?;
    let update = std::env::var("VARVE_UPDATE_CONTRACT").is_ok_and(|v| v == "1");
    let engine = if mariadb { "ma" } else { "my" };
    let tables = [("mem", 2), ("items", if mariadb { 3 } else { 5 })];
    // Every describe sample first, so an update writes them all.
    for (table, _) in tables {
        let object = ObjectRef {
            kind: ObjectKind::Table,
            database: Some(MY_DB.into()),
            schema: None,
            parent: None,
            name: table.into(),
            signature: None,
        };
        let mut described = serde_json::to_value(catalog.describe(session, &object).await?)?;
        normalize(&mut described);
        let describe_file =
            samples().join(format!("{engine}-index-describe.{table}.{server}.json"));
        if update {
            std::fs::write(
                &describe_file,
                serde_json::to_string_pretty(&described)? + "\n",
            )?;
        }
        let sample: Json =
            serde_json::from_str(&std::fs::read_to_string(&describe_file).map_err(|e| {
                format!(
                    "{}: {e} (run with VARVE_UPDATE_CONTRACT=1 to write it)",
                    describe_file.display()
                )
            })?)?;
        assert_eq!(
            described,
            sample,
            "describe differs from {}",
            describe_file.display()
        );
    }
    for (table, redefinable) in tables {
        let redefine_file =
            samples().join(format!("{engine}-index-redefine.{table}.{server}.json"));
        let changes: Vec<Json> =
            serde_json::from_str(&std::fs::read_to_string(&redefine_file).map_err(|e| {
                format!(
                    "{}: {e} (the UI test api/__contract__/indexRoundTrip.test.ts writes it)",
                    redefine_file.display()
                )
            })?)?;
        let drops = changes
            .iter()
            .flat_map(|c| {
                c["change"]["operations"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter(|op| op["op"] == "dropIndex")
            .count();
        assert_eq!(drops, redefinable, "{table}");

        let before = my_keys(session, table).await?;
        for change in changes {
            assert_eq!(change["engine"], "mysql");
            let change: MyChange = serde_json::from_value(change["change"].clone())?;
            let plan = varve_catalog::mysql::edit::plan(session.server_info(), &change)?;
            for statement in &plan.statements {
                exec(session, statement).await?;
            }
        }
        let after = my_keys(session, table).await?;
        assert_eq!(before, after, "{table} on {server}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn index_redefinition_round_trips_on_mysql_8_0() -> TestResult {
    my_round_trip(
        Engine::Mysql,
        port("VARVE_M2_MYSQL_8_0_PORT", 35080),
        "my80",
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn index_redefinition_round_trips_on_mysql_9_7() -> TestResult {
    my_round_trip(
        Engine::Mysql,
        port("VARVE_M2_MYSQL_9_7_PORT", 35097),
        "my97",
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn index_redefinition_round_trips_on_mariadb_10_6() -> TestResult {
    my_round_trip(
        Engine::Mariadb,
        port("VARVE_M2_MARIADB_10_6_PORT", 35106),
        "ma106",
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn index_redefinition_round_trips_on_mariadb_12_3() -> TestResult {
    my_round_trip(
        Engine::Mariadb,
        port("VARVE_M2_MARIADB_12_3_PORT", 35123),
        "ma123",
    )
    .await
}
