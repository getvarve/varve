//! Round trip tests of the PostgreSQL relations catalog against the shared
//! M2 database matrix (PostgreSQL 13 and 18):
//!
//! ```sh
//! cargo test -p varve-catalog --test pg_relations -- --include-ignored
//! ```
//!
//! Each run creates fixtures covering every relations kind and feature in
//! a database `m2_pgrel_src`, reads them back, runs the generated DDL in a
//! fresh database `m2_pgrel_dst` and compares what `describe` reports on
//! both sides. Databases and the tablespace are round tripped by dropping
//! and recreating them from their DDL. Everything is named `m2_pgrel*` and
//! dropped at the start and the end of a run.

use std::error::Error;

use serde_json::Value as Json;
use std::time::{Duration, Instant};
use varve_catalog::model::{Catalog, Container, ObjectDetails, ObjectKind, ObjectRef};
use varve_catalog::postgres::PgObject;
use varve_catalog::postgres::relations::{KINDS as RELATION_KINDS, PgRelationObject, PgTable};
use varve_catalog::{CatalogError, catalog_for};
use varve_core::Engine;

use varve_driver::{
    ConnectParams, Driver, ErrorKind, ExecOptions, Session, StatementResult, Value,
};
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const SCHEMA: &str = "m2_pgrel_s";
const OTHER: &str = "m2_pgrel Other";
const MAIN: &str = "Main \"Table\"";
const SRC: &str = "m2_pgrel_src";
const DST: &str = "m2_pgrel_dst";
const DB1: &str = "m2_pgrel_db1";
const DB2: &str = "m2_pgrel_db2";
const TABLESPACE: &str = "m2_pgrel_ts";
const TS_DIR: &str = "/tmp/m2_pgrel_ts";

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

async fn connect(port: u16, database: &str) -> TestResult<Box<dyn Session>> {
    let mut params = ConnectParams::new(
        Engine::Postgres,
        env_or("VARVE_M2_PG_HOST", "127.0.0.1"),
        port,
    );
    params.database = Some(database.to_owned());
    params.username = Some(env_or("VARVE_M2_PG_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_M2_PG_PASSWORD", "varve_dev")));
    params.application_name = "varve-catalog pg_relations tests".into();
    Ok(PostgresDriver::new().connect(&params).await?)
}

async fn exec(session: &dyn Session, sql: &str) -> TestResult {
    session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| {
            let head: String = sql.chars().take(600).collect();
            format!("{e} (detail: {:?})\nin:\n{head}", e.detail)
        })?;
    Ok(())
}

async fn scalar(session: &dyn Session, sql: &str) -> TestResult<Value> {
    let output = session.execute(sql, &ExecOptions::default()).await?;
    for result in output.results {
        if let StatementResult::Rows { rows, .. } = result {
            let value = rows
                .into_iter()
                .next()
                .and_then(|row| row.into_iter().next())
                .ok_or("no value")?;
            return Ok(value);
        }
    }
    Err("no rows".into())
}

fn obj(kind: ObjectKind, schema: &str, parent: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind,
        database: None,
        schema: Some(schema.to_owned()),
        parent: parent.map(str::to_owned),
        name: name.to_owned(),
        signature: None,
    }
}

fn server_obj(kind: ObjectKind, name: &str) -> ObjectRef {
    ObjectRef {
        kind,
        database: None,
        schema: None,
        parent: None,
        name: name.to_owned(),
        signature: None,
    }
}

/// Fields that legitimately differ between two copies of the same
/// structure (sizes, planner estimates, sequence positions).
const VOLATILE: &[&str] = &[
    "sizeBytes",
    "totalBytes",
    "tableBytes",
    "indexBytes",
    "rowEstimate",
    "lastValue",
    "isCalled",
];

fn normalize(value: &mut Json, extra: &[&str]) {
    match value {
        Json::Object(map) => {
            for key in VOLATILE.iter().chain(extra) {
                map.remove(*key);
            }
            for v in map.values_mut() {
                normalize(v, extra);
            }
        }
        Json::Array(items) => items.iter_mut().for_each(|v| normalize(v, extra)),
        _ => {}
    }
}

async fn described(
    catalog: &dyn Catalog,
    session: &dyn Session,
    object: &ObjectRef,
    extra: &[&str],
) -> TestResult<Json> {
    let details = catalog
        .describe(session, object)
        .await
        .map_err(|e| format!("describe {object:?}: {e}"))?;
    let mut json = serde_json::to_value(&details)?;
    normalize(&mut json, extra);
    Ok(json)
}

async fn same(
    catalog: &dyn Catalog,
    src: &dyn Session,
    dst: &dyn Session,
    object: &ObjectRef,
    extra: &[&str],
) -> TestResult {
    let a = described(catalog, src, object, extra).await?;
    let b = described(catalog, dst, object, extra).await?;
    if a != b {
        let mut differences = Vec::new();
        json_diff(&a, &b, "", &mut differences);
        return Err(format!(
            "{object:?} differs after the round trip:\n{}",
            differences.join("\n")
        )
        .into());
    }
    Ok(())
}

/// The paths where two JSON documents differ, with both values.
fn json_diff(a: &Json, b: &Json, path: &str, out: &mut Vec<String>) {
    match (a, b) {
        (Json::Object(x), Json::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let (l, r) = (
                    x.get(key).unwrap_or(&Json::Null),
                    y.get(key).unwrap_or(&Json::Null),
                );
                json_diff(l, r, &format!("{path}.{key}"), out);
            }
        }
        (Json::Array(x), Json::Array(y)) if x.len() == y.len() => {
            for (i, (l, r)) in x.iter().zip(y).enumerate() {
                json_diff(l, r, &format!("{path}[{i}]"), out);
            }
        }
        _ if a != b => out.push(format!("{path}: source {a} / copy {b}")),
        _ => {}
    }
}

async fn ddl(
    catalog: &dyn Catalog,
    session: &dyn Session,
    object: &ObjectRef,
) -> TestResult<String> {
    Ok(catalog
        .ddl(session, object)
        .await
        .map_err(|e| format!("ddl {object:?}: {e}"))?)
}

fn relation(details: ObjectDetails) -> TestResult<PgRelationObject> {
    match details {
        ObjectDetails::Postgres(object) => match *object {
            PgObject::Relation(object) => Ok(object),
            other => Err(format!("not a relations object: {other:?}").into()),
        },
        other => Err(format!("not a PostgreSQL object: {other:?}").into()),
    }
}

fn table_of(object: PgRelationObject) -> TestResult<PgTable> {
    match object {
        PgRelationObject::Table(t)
        | PgRelationObject::PartitionedTable(t)
        | PgRelationObject::Partition(t)
        | PgRelationObject::ForeignTable(t) => Ok(t),
        other => Err(format!("not a table: {other:?}").into()),
    }
}

async fn cleanup(admin: &dyn Session) -> TestResult {
    for db in [SRC, DST, DB1, DB2] {
        exec(
            admin,
            &format!(
                "DO $$BEGIN IF EXISTS (SELECT 1 FROM pg_database WHERE datname = '{db}') THEN \
                 EXECUTE 'ALTER DATABASE {db} IS_TEMPLATE false'; END IF; END$$"
            ),
        )
        .await?;
        exec(admin, &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await?;
    }
    exec(admin, &format!("DROP TABLESPACE IF EXISTS {TABLESPACE}")).await?;
    exec(
        admin,
        &format!("COPY (SELECT 1) TO PROGRAM 'rm -rf {TS_DIR}'"),
    )
    .await?;
    exec(admin, "DROP ROLE IF EXISTS m2_pgrel_owner").await?;
    exec(admin, "DROP ROLE IF EXISTS m2_pgrel_reader").await?;
    Ok(())
}

/// Prerequisites owned by other catalog modules (extensions, foreign
/// server, composite type, trigger function), created by hand on both
/// sides.
fn prerequisites() -> String {
    format!(
        "CREATE EXTENSION btree_gist;
         CREATE EXTENSION postgres_fdw;
         CREATE EXTENSION pg_stat_statements;
         CREATE SERVER m2_pgrel_srv FOREIGN DATA WRAPPER postgres_fdw OPTIONS (dbname 'varve');
         CREATE TYPE {SCHEMA}.pair AS (a int, b text);
         CREATE FUNCTION {SCHEMA}.trg() RETURNS trigger LANGUAGE plpgsql AS $f$BEGIN RETURN NEW; END$f$;"
    )
}

fn fixtures(major: u32) -> String {
    let m = "\"Main \"\"Table\"\"\"";
    let mut sql = format!(
        "CREATE SCHEMA {SCHEMA} AUTHORIZATION m2_pgrel_owner;
         COMMENT ON SCHEMA {SCHEMA} IS 'fixture schema';
         CREATE SCHEMA \"m2_pgrel Other\";
         {prereq}
         SET search_path = {SCHEMA};
         CREATE SEQUENCE seq_plain AS integer START WITH 5 INCREMENT BY 2 MINVALUE 1 MAXVALUE 1000 CACHE 3 CYCLE;
         COMMENT ON SEQUENCE seq_plain IS 'plain sequence';
         SELECT nextval('seq_plain');
         CREATE TABLE ref (id int PRIMARY KEY, code text UNIQUE);
         INSERT INTO ref VALUES (1, 'a'), (2, 'b');
         CREATE SEQUENCE seq_owned;
         ALTER SEQUENCE seq_owned OWNED BY ref.code;
         CREATE TABLE {m} (
           id bigint GENERATED ALWAYS AS IDENTITY (START WITH 10 INCREMENT BY 5 CACHE 2) PRIMARY KEY,
           serial_col serial,
           code varchar(20) COLLATE \"C\" NOT NULL DEFAULT 'x',
           amount numeric(10,2) CONSTRAINT amount_positive CHECK (amount >= 0),
           doubled numeric GENERATED ALWAYS AS (amount * 2) STORED,
           big text,
           b int,
           during tsrange,
           ref_id int,
           ref_code text,
           tags int[],
           pt point,
           created timestamptz DEFAULT now(),
           seq_val int DEFAULT nextval('seq_plain'),
           CONSTRAINT code_unique UNIQUE (code) DEFERRABLE INITIALLY DEFERRED,
           CONSTRAINT no_overlap EXCLUDE USING gist (b WITH =, during WITH &&),
           CONSTRAINT ref_fk FOREIGN KEY (ref_id) REFERENCES ref (id) MATCH FULL
             ON UPDATE CASCADE ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED
         ) WITH (fillfactor = 70, toast.autovacuum_enabled = false) TABLESPACE {TABLESPACE};
         ALTER TABLE {m} ADD CONSTRAINT ref_code_fk FOREIGN KEY (ref_code) REFERENCES ref (code) NOT VALID;
         ALTER TABLE {m} ADD CONSTRAINT b_check CHECK (b > 0) NOT VALID;
         ALTER TABLE {m} ADD CONSTRAINT b_noinherit CHECK (b < 1000000) NO INHERIT;
         ALTER TABLE {m} ALTER COLUMN big SET STORAGE EXTERNAL;
         ALTER TABLE {m} ALTER COLUMN big SET STATISTICS 500;
         ALTER TABLE {m} ALTER COLUMN amount SET (n_distinct = 100);
         COMMENT ON TABLE {m} IS 'main table';
         COMMENT ON COLUMN {m}.code IS 'the code';
         COMMENT ON COLUMN {m}.big IS 'big text';
         COMMENT ON CONSTRAINT amount_positive ON {m} IS 'positive amounts';
         CREATE INDEX main_hash ON {m} USING hash (code);
         CREATE INDEX main_gin ON {m} USING gin (tags);
         CREATE INDEX main_gist ON {m} USING gist (during);
         CREATE INDEX main_spgist ON {m} USING spgist (pt);
         CREATE INDEX main_brin ON {m} USING brin (created) WITH (pages_per_range = 16);
         CREATE INDEX main_partial ON {m} (amount) WHERE amount > 100;
         CREATE INDEX main_expr ON {m} (lower(code::text), (amount + 1) DESC NULLS LAST);
         CREATE UNIQUE INDEX main_include ON {m} (ref_id) INCLUDE (ref_code) WITH (fillfactor = 80) TABLESPACE {TABLESPACE};
         CREATE INDEX main_desc ON {m} (created DESC NULLS FIRST);
         COMMENT ON INDEX main_hash IS 'hash index';
         ALTER TABLE {m} CLUSTER ON main_desc;
         CREATE TRIGGER t_before BEFORE UPDATE OF code, amount ON {m} FOR EACH ROW
           WHEN (OLD.code IS DISTINCT FROM NEW.code AND NEW.code <> ') WHEN (') EXECUTE FUNCTION trg();
         CREATE TRIGGER t_after_stmt AFTER INSERT ON {m} REFERENCING NEW TABLE AS inserted
           FOR EACH STATEMENT EXECUTE FUNCTION trg();
         CREATE TRIGGER t_disabled AFTER DELETE ON {m} FOR EACH ROW EXECUTE FUNCTION trg();
         ALTER TABLE {m} DISABLE TRIGGER t_disabled;
         CREATE TRIGGER t_replica BEFORE INSERT ON {m} FOR EACH ROW EXECUTE FUNCTION trg();
         ALTER TABLE {m} ENABLE REPLICA TRIGGER t_replica;
         CREATE CONSTRAINT TRIGGER t_constraint AFTER INSERT ON {m} DEFERRABLE INITIALLY DEFERRED
           FOR EACH ROW EXECUTE FUNCTION trg();
         COMMENT ON TRIGGER t_before ON {m} IS 'before update';
         CREATE RULE r_notify AS ON INSERT TO {m} DO ALSO NOTIFY m2_pgrel;
         CREATE RULE r_disabled AS ON DELETE TO {m} DO ALSO NOTIFY m2_pgrel_del;
         ALTER TABLE {m} DISABLE RULE r_disabled;
         COMMENT ON RULE r_notify ON {m} IS 'notify on insert';
         ALTER TABLE {m} ENABLE ROW LEVEL SECURITY;
         ALTER TABLE {m} FORCE ROW LEVEL SECURITY;
         CREATE POLICY p_all ON {m} USING (amount > 0);
         CREATE POLICY p_upd ON {m} AS RESTRICTIVE FOR UPDATE TO m2_pgrel_owner, m2_pgrel_reader
           USING (true) WITH CHECK (amount < 1000);
         COMMENT ON POLICY p_all ON {m} IS 'all rows';
         ALTER TABLE {m} REPLICA IDENTITY FULL;
         ALTER TABLE {m} OWNER TO m2_pgrel_owner;
         CREATE UNLOGGED TABLE unlogged_t (id int PRIMARY KEY, note text);
         ALTER TABLE unlogged_t REPLICA IDENTITY USING INDEX unlogged_t_pkey;
         CREATE TABLE inh_parent (id int NOT NULL, note text DEFAULT 'p', shared int, CONSTRAINT inh_ck CHECK (id > 0));
         CREATE TABLE inh_child (extra int, note text) INHERITS (inh_parent);
         ALTER TABLE inh_child ALTER COLUMN id SET DEFAULT 7;
         CREATE FOREIGN DATA WRAPPER m2_pgrel_nofdw;
         CREATE SERVER m2_pgrel_secret_srv FOREIGN DATA WRAPPER m2_pgrel_nofdw;
         CREATE SCHEMA m2_pgrel_secret;
         CREATE FOREIGN TABLE m2_pgrel_secret.leaky (
           id int OPTIONS (password 'colpw', column_name 'c'),
           u text
         ) SERVER m2_pgrel_secret_srv
           OPTIONS (url 'https://u:SECRET@x/', table_name 'items', schema_name 'https://a:b@h/');
         CREATE TABLE typed_t OF pair (a WITH OPTIONS NOT NULL DEFAULT 1, PRIMARY KEY (a));
         CREATE TABLE measures (id int NOT NULL, logdate date NOT NULL, v int,
           CONSTRAINT measures_v CHECK (v >= 0)) PARTITION BY RANGE (logdate);
         ALTER TABLE measures ADD CONSTRAINT measures_pk PRIMARY KEY (id, logdate);
         CREATE INDEX measures_v_idx ON measures (v);
         ALTER TABLE measures ADD CONSTRAINT measures_ref FOREIGN KEY (id) REFERENCES ref (id) ON DELETE CASCADE;
         CREATE TRIGGER measures_trg BEFORE INSERT ON measures FOR EACH ROW EXECUTE FUNCTION trg();
         COMMENT ON TABLE measures IS 'partitioned';
         CREATE TABLE measures_2024 PARTITION OF measures FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
         CREATE TABLE measures_2025 (id int NOT NULL, logdate date NOT NULL, v int,
           CONSTRAINT measures_v CHECK (v >= 0));
         CREATE INDEX m2025_custom ON measures_2025 (v);
         ALTER TABLE measures ATTACH PARTITION measures_2025 FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
         CREATE TABLE measures_default PARTITION OF measures DEFAULT;
         CREATE TABLE measures_2026 PARTITION OF measures FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')
           PARTITION BY LIST (id);
         CREATE TABLE measures_2026_a PARTITION OF measures_2026 FOR VALUES IN (1, 2);
         CREATE TABLE hashed (id int, payload text) PARTITION BY HASH (id);
         CREATE TABLE hashed_0 PARTITION OF hashed FOR VALUES WITH (MODULUS 2, REMAINDER 0);
         CREATE TABLE hashed_1 PARTITION OF hashed FOR VALUES WITH (MODULUS 2, REMAINDER 1);
         CREATE TABLE pref (id int PRIMARY KEY) PARTITION BY RANGE (id);
         CREATE TABLE pref_1 PARTITION OF pref FOR VALUES FROM (0) TO (10);
         CREATE TABLE pref_2 PARTITION OF pref FOR VALUES FROM (10) TO (20);
         CREATE TABLE pref_user (x int CONSTRAINT pref_user_fk REFERENCES pref);
         CREATE TABLE pref_log (x int CONSTRAINT pref_log_fk REFERENCES pref)
           PARTITION BY RANGE (x);
         CREATE TABLE pref_log_1 PARTITION OF pref_log FOR VALUES FROM (0) TO (100);
         CREATE TABLE spans (lo int, hi int,
           CONSTRAINT spans_ex EXCLUDE USING gist (int4range(lo, hi) WITH &&),
           CONSTRAINT spans_mixed EXCLUDE USING gist (lo WITH =, int4range(lo, hi) WITH &&));
         CREATE TABLE regions (code text, name text) PARTITION BY LIST (code);
         CREATE TABLE regions_eu PARTITION OF regions FOR VALUES IN ('nl', 'de');
         CREATE FOREIGN TABLE regions_us PARTITION OF regions FOR VALUES IN ('us')
           SERVER m2_pgrel_srv OPTIONS (table_name 'regions_us');
         CREATE FOREIGN TABLE remote_items (
           id int OPTIONS (column_name 'item_id') NOT NULL,
           label text COLLATE \"C\" DEFAULT 'none',
           CONSTRAINT remote_ck CHECK (id > 0)
         ) SERVER m2_pgrel_srv OPTIONS (schema_name 'public', table_name 'items');
         COMMENT ON FOREIGN TABLE remote_items IS 'remote';
         CREATE VIEW v_main WITH (security_barrier = true) AS
           SELECT id, code, amount FROM {m} WHERE amount > 0 WITH LOCAL CHECK OPTION;
         ALTER VIEW v_main ALTER COLUMN amount SET DEFAULT 1;
         COMMENT ON VIEW v_main IS 'a view';
         COMMENT ON COLUMN v_main.code IS 'view code';
         CREATE VIEW v_ref AS SELECT * FROM ref;
         CREATE TRIGGER v_ref_ins INSTEAD OF INSERT ON v_ref FOR EACH ROW EXECUTE FUNCTION trg();
         CREATE RULE v_ref_del AS ON DELETE TO v_ref DO INSTEAD NOTHING;
         CREATE MATERIALIZED VIEW mv_ref WITH (fillfactor = 90) AS SELECT id, code FROM ref WITH DATA;
         CREATE UNIQUE INDEX mv_ref_id ON mv_ref (id);
         COMMENT ON MATERIALIZED VIEW mv_ref IS 'a materialized view';
         CREATE MATERIALIZED VIEW mv_empty AS SELECT 1 AS one WITH NO DATA;
         CREATE TABLE \"m2_pgrel Other\".tiny (id int);
        ",
        prereq = prerequisites(),
    );
    if major >= 14 {
        sql.push_str(&format!(
            "ALTER TABLE {m} ALTER COLUMN big SET COMPRESSION pglz;\n"
        ));
    }
    if major >= 15 {
        sql.push_str(&format!(
            "CREATE UNIQUE INDEX main_nnd ON {m} (seq_val) NULLS NOT DISTINCT;
             CREATE VIEW v_invoker WITH (security_invoker = true) AS SELECT 1 AS one;
             CREATE UNLOGGED SEQUENCE seq_unlogged;\n"
        ));
    }
    sql
}

/// Order in which the generated DDL of the fixtures is applied to the
/// copy: referenced tables before referencing ones, parents before
/// children.
const TABLE_ORDER: &[&str] = &[
    "ref",
    MAIN,
    "unlogged_t",
    "inh_parent",
    "inh_child",
    "typed_t",
    "measures",
    "measures_2024",
    "measures_2025",
    "measures_default",
    "measures_2026",
    "measures_2026_a",
    "hashed",
    "hashed_0",
    "hashed_1",
    "regions",
    "regions_eu",
    "regions_us",
    "remote_items",
    "pref",
    "pref_1",
    "pref_2",
    "pref_user",
    "pref_log",
    "pref_log_1",
    "spans",
];

async fn round_trip(major: u32, port: u16) -> TestResult {
    let admin = connect(port, "varve").await?;
    assert_eq!(admin.server_info().major, major);
    cleanup(admin.as_ref()).await?;
    exec(
        admin.as_ref(),
        &format!(
            "CREATE ROLE m2_pgrel_owner NOLOGIN;
             CREATE ROLE m2_pgrel_reader NOLOGIN;
             COPY (SELECT 1) TO PROGRAM 'rm -rf {TS_DIR} && mkdir -p {TS_DIR} && chmod 700 {TS_DIR}';
             CREATE TABLESPACE {TABLESPACE} OWNER m2_pgrel_owner LOCATION '{TS_DIR}'
               WITH (random_page_cost = 2, seq_page_cost = 1.5);
             COMMENT ON TABLESPACE {TABLESPACE} IS 'fixture tablespace';
             CREATE DATABASE {SRC};
             CREATE DATABASE {DST};"
        ),
    )
    .await?;
    let result = run_all(major, port, admin.as_ref()).await;
    let cleaned = cleanup(admin.as_ref()).await;
    result?;
    cleaned?;
    Ok(())
}

async fn run_all(major: u32, port: u16, admin: &dyn Session) -> TestResult {
    let src = connect(port, SRC).await?;
    exec(src.as_ref(), &fixtures(major)).await?;
    // A fresh session: the fixture session changed its search path.
    let src = connect(port, SRC).await?;
    let dst = connect(port, DST).await?;
    let catalog = catalog_for(src.server_info())?;
    let catalog = catalog.as_ref();
    let (src, dst) = (src.as_ref(), dst.as_ref());

    capabilities(catalog, major);
    tree(catalog, src, major).await?;
    details(catalog, src, major).await?;
    redaction(catalog, src).await?;
    containers(catalog, src).await?;
    in_transaction(catalog, src).await?;
    locked(catalog, src, port).await?;

    // Copy everything to the fresh database through the generated DDL.
    for schema in [SCHEMA, OTHER] {
        exec(
            dst,
            &ddl(catalog, src, &server_obj(ObjectKind::Schema, schema)).await?,
        )
        .await?;
    }
    exec(dst, &prerequisites()).await?;
    let mut sequences = vec!["seq_plain"];
    if major >= 15 {
        sequences.push("seq_unlogged");
    }
    for seq in &sequences {
        exec(
            dst,
            &ddl(catalog, src, &obj(ObjectKind::Sequence, SCHEMA, None, seq)).await?,
        )
        .await?;
    }
    for table in TABLE_ORDER {
        let script = ddl(catalog, src, &obj(ObjectKind::Table, SCHEMA, None, table)).await?;
        exec(dst, &script).await?;
    }
    exec(
        dst,
        &ddl(catalog, src, &obj(ObjectKind::Table, OTHER, None, "tiny")).await?,
    )
    .await?;
    let mut views = vec!["v_main", "v_ref"];
    if major >= 15 {
        views.push("v_invoker");
    }
    for view in &views {
        exec(
            dst,
            &ddl(catalog, src, &obj(ObjectKind::View, SCHEMA, None, view)).await?,
        )
        .await?;
    }
    for view in ["mv_ref", "mv_empty"] {
        let object = obj(ObjectKind::MaterializedView, SCHEMA, None, view);
        exec(dst, &ddl(catalog, src, &object).await?).await?;
    }

    compare_everything(catalog, src, dst).await?;
    // The copied foreign key accepts a row of any referenced partition
    // (wrong extra foreign keys to single partitions would refuse 15).
    exec(
        dst,
        &format!(
            "INSERT INTO {SCHEMA}.pref VALUES (5), (15); \
             INSERT INTO {SCHEMA}.pref_user VALUES (5), (15); \
             INSERT INTO {SCHEMA}.pref_log VALUES (5), (15); \
             DELETE FROM {SCHEMA}.pref_log; DELETE FROM {SCHEMA}.pref_user; \
             DELETE FROM {SCHEMA}.pref"
        ),
    )
    .await?;
    children_round_trip(catalog, src, dst, major).await?;
    audit_round_trips(catalog, src, dst).await?;
    sequence_round_trip(catalog, src, dst).await?;
    databases_round_trip(catalog, src, admin, major).await?;
    tablespace_round_trip(catalog, admin).await?;
    Ok(())
}

fn capabilities(catalog: &dyn Catalog, major: u32) {
    let caps = catalog.capabilities();
    for kind in [
        ObjectKind::Table,
        ObjectKind::MaterializedView,
        ObjectKind::Policy,
        ObjectKind::Tablespace,
    ] {
        assert!(caps.kinds.contains(&kind), "{kind:?}");
    }
    assert!(caps.editable.contains(&ObjectKind::PartitionedTable));
    assert!(!caps.editable.contains(&ObjectKind::ForeignTable));
    assert!(caps.features.contains(&"refreshConcurrently".to_owned()));
    assert_eq!(
        caps.features.contains(&"nullsNotDistinct".to_owned()),
        major >= 15
    );
    assert_eq!(
        caps.features.contains(&"columnCompression".to_owned()),
        major >= 14
    );
}

fn schema_container(schema: &str) -> Container {
    Container {
        database: Some(SRC.to_owned()),
        schema: Some(schema.to_owned()),
        parent: None,
    }
}

fn names(list: &[varve_catalog::ObjectSummary]) -> Vec<String> {
    list.iter().map(|s| s.object.name.clone()).collect()
}

async fn tree(catalog: &dyn Catalog, src: &dyn Session, major: u32) -> TestResult {
    // Server level.
    let server = Container::default();
    let dbs = catalog
        .list(src, &server, ObjectKind::Database, true)
        .await?;
    assert!(dbs.iter().any(|d| d.object.name == "template0" && d.system));
    assert!(dbs.iter().any(|d| d.object.name == SRC && !d.system));
    let visible = catalog
        .list(src, &server, ObjectKind::Database, false)
        .await?;
    assert!(!names(&visible).contains(&"template0".to_owned()));
    let counts = catalog.kinds(src, &server, false).await?;
    // Other test runs create databases concurrently, so only check that
    // the count is there and excludes the system databases.
    let db_count = counts
        .iter()
        .find(|c| c.kind == ObjectKind::Database)
        .ok_or("no database count")?;
    assert!(db_count.count >= 2, "{counts:?}");
    let spaces = catalog
        .list(src, &server, ObjectKind::Tablespace, false)
        .await?;
    assert!(names(&spaces).contains(&TABLESPACE.to_owned()));
    assert!(!names(&spaces).contains(&"pg_default".to_owned()));

    // Database level.
    let database = Container {
        database: Some(SRC.to_owned()),
        schema: None,
        parent: None,
    };
    let schemas = catalog
        .list(src, &database, ObjectKind::Schema, false)
        .await?;
    assert!(names(&schemas).contains(&SCHEMA.to_owned()));
    assert!(!names(&schemas).contains(&"pg_catalog".to_owned()));
    let all = catalog
        .list(src, &database, ObjectKind::Schema, true)
        .await?;
    assert!(
        all.iter()
            .any(|s| s.object.name == "pg_catalog" && s.system)
    );
    let counts = catalog.kinds(src, &database, false).await?;
    let schema_count = counts
        .iter()
        .find(|c| c.kind == ObjectKind::Schema)
        .ok_or("no schema count")?;
    assert_eq!(schema_count.count as usize, schemas.len());

    // Schema level: counts agree with the lists.
    let container = schema_container(SCHEMA);
    let counts = catalog.kinds(src, &container, false).await?;
    for count in &counts {
        let list = catalog.list(src, &container, count.kind, false).await?;
        assert_eq!(list.len() as u64, count.count, "{:?}", count.kind);
    }
    let expect = |kind: ObjectKind, n: u64| -> TestResult {
        let found = counts
            .iter()
            .find(|c| c.kind == kind)
            .map_or(0, |c| c.count);
        if found != n {
            return Err(format!("{kind:?}: {found} instead of {n}").into());
        }
        Ok(())
    };
    expect(ObjectKind::Table, 8)?;
    expect(ObjectKind::PartitionedTable, 5)?;
    expect(ObjectKind::ForeignTable, 1)?;
    expect(ObjectKind::View, if major >= 15 { 3 } else { 2 })?;
    expect(ObjectKind::MaterializedView, 2)?;
    expect(ObjectKind::Sequence, if major >= 15 { 4 } else { 3 })?;
    let tables = catalog
        .list(src, &container, ObjectKind::Table, false)
        .await?;
    let unlogged = tables
        .iter()
        .find(|t| t.object.name == "unlogged_t")
        .ok_or("unlogged_t")?;
    assert!(unlogged.flags.contains(&"unlogged".to_owned()));
    assert!(tables.iter().all(|t| t.has_children));
    let matviews = catalog
        .list(src, &container, ObjectKind::MaterializedView, false)
        .await?;
    let empty = matviews
        .iter()
        .find(|m| m.object.name == "mv_empty")
        .ok_or("mv_empty")?;
    assert!(empty.flags.contains(&"not populated".to_owned()));

    // Row estimate and size come with the list, for relations that hold
    // rows themselves.
    exec(src, &format!("ANALYZE {SCHEMA}.\"Main \"\"Table\"\"\"")).await?;
    let rows = scalar(
        src,
        &format!("SELECT count(*) FROM {SCHEMA}.\"Main \"\"Table\"\"\""),
    )
    .await?;
    let tables = catalog
        .list(src, &container, ObjectKind::Table, false)
        .await?;
    let main = tables
        .iter()
        .find(|t| t.object.name == MAIN)
        .ok_or("main table not listed")?;
    assert_eq!(
        main.row_estimate
            .map(|r| Value::Int(i64::try_from(r).unwrap_or(-1))),
        Some(rows),
        "{main:?}"
    );
    assert!(main.size_bytes.is_some_and(|b| b > 0), "{main:?}");
    for kind in [
        ObjectKind::View,
        ObjectKind::PartitionedTable,
        ObjectKind::ForeignTable,
        ObjectKind::Sequence,
    ] {
        let list = catalog.list(src, &container, kind, false).await?;
        assert!(
            list.iter()
                .all(|o| o.row_estimate.is_none() && o.size_bytes.is_none()),
            "{kind:?}: {list:?}"
        );
    }
    assert!(
        matviews
            .iter()
            .filter(|m| m.object.name != "mv_empty")
            .all(|m| m.size_bytes.is_some()),
        "{matviews:?}"
    );

    // Extension-owned objects are system objects.
    let public = schema_container("public");
    let views = catalog.list(src, &public, ObjectKind::View, false).await?;
    assert!(views.is_empty(), "{views:?}");
    let views = catalog.list(src, &public, ObjectKind::View, true).await?;
    assert!(
        views
            .iter()
            .any(|v| v.object.name == "pg_stat_statements" && v.system)
    );

    // Relation level.
    let main = Container {
        database: Some(SRC.to_owned()),
        schema: Some(SCHEMA.to_owned()),
        parent: Some(obj(ObjectKind::Table, SCHEMA, None, MAIN)),
    };
    let counts = catalog.kinds(src, &main, false).await?;
    for count in &counts {
        let list = catalog.list(src, &main, count.kind, false).await?;
        assert_eq!(list.len() as u64, count.count, "{:?}", count.kind);
        for item in &list {
            assert_eq!(item.object.parent.as_deref(), Some(MAIN));
        }
    }
    // A foreign key that references a partitioned table is one constraint:
    // the clones the server keeps per referenced partition are neither
    // listed nor counted, on a plain table and on a partition.
    // The partition's copy of the foreign key comes with its ATTACH, so its
    // own DDL writes none.
    for (table, fk, in_ddl) in [
        ("pref_user", "pref_user_fk", 1),
        ("pref_log", "pref_log_fk", 1),
        ("pref_log_1", "pref_log_fk", 0),
    ] {
        let container = Container {
            database: Some(SRC.to_owned()),
            schema: Some(SCHEMA.to_owned()),
            parent: Some(obj(ObjectKind::Table, SCHEMA, None, table)),
        };
        let constraints = catalog
            .list(src, &container, ObjectKind::Constraint, false)
            .await?;
        assert_eq!(names(&constraints), vec![fk.to_owned()], "{table}");
        let counted = catalog
            .kinds(src, &container, false)
            .await?
            .into_iter()
            .find(|c| c.kind == ObjectKind::Constraint)
            .map(|c| c.count);
        assert_eq!(counted, Some(1), "{table}");
        let script = ddl(catalog, src, &obj(ObjectKind::Table, SCHEMA, None, table)).await?;
        assert_eq!(
            script.matches("FOREIGN KEY").count(),
            in_ddl,
            "{table}: {script}"
        );
    }
    // Expression elements of exclusion constraints say so.
    let spans = Container {
        database: Some(SRC.to_owned()),
        schema: Some(SCHEMA.to_owned()),
        parent: Some(obj(ObjectKind::Table, SCHEMA, None, "spans")),
    };
    let exclusions = catalog
        .list(src, &spans, ObjectKind::Constraint, false)
        .await?;
    assert_eq!(
        exclusions
            .iter()
            .map(|c| c.detail.clone().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![
            "exclusion (expression)".to_owned(),
            "exclusion (lo, expression)".to_owned()
        ]
    );
    // Primary keys, unique and foreign keys carry no "no inherit" flag.
    for constraint in catalog
        .list(src, &main, ObjectKind::Constraint, false)
        .await?
    {
        let detail = constraint.detail.as_deref().unwrap_or_default();
        if !detail.starts_with("check") && !detail.starts_with("not null") {
            assert!(
                !constraint.flags.iter().any(|f| f == "no inherit"),
                "{constraint:?}"
            );
        }
    }
    let triggers = catalog.list(src, &main, ObjectKind::Trigger, false).await?;
    assert_eq!(triggers.len(), 5);
    let disabled = triggers
        .iter()
        .find(|t| t.object.name == "t_disabled")
        .ok_or("t_disabled")?;
    assert!(disabled.flags.contains(&"disabled".to_owned()));
    let indexes = catalog.list(src, &main, ObjectKind::Index, false).await?;
    let partial = indexes
        .iter()
        .find(|i| i.object.name == "main_partial")
        .ok_or("main_partial")?;
    assert!(partial.flags.contains(&"partial".to_owned()));

    let measures = Container {
        parent: Some(obj(ObjectKind::PartitionedTable, SCHEMA, None, "measures")),
        ..schema_container(SCHEMA)
    };
    let partitions = catalog
        .list(src, &measures, ObjectKind::Partition, false)
        .await?;
    assert_eq!(
        names(&partitions),
        vec![
            "measures_2024",
            "measures_2025",
            "measures_2026",
            "measures_default"
        ]
    );
    let default = partitions.last().ok_or("no default partition")?;
    assert!(default.flags.contains(&"default".to_owned()));
    assert_eq!(default.object.parent.as_deref(), Some("measures"));

    // Lists need the right container.
    let error = catalog
        .list(src, &container, ObjectKind::Column, false)
        .await
        .err()
        .ok_or("columns listed without a relation")?;
    assert!(matches!(error, CatalogError::Invalid { .. }), "{error:?}");
    Ok(())
}

async fn details(catalog: &dyn Catalog, src: &dyn Session, major: u32) -> TestResult {
    let main = table_of(relation(
        catalog
            .describe(src, &obj(ObjectKind::Table, SCHEMA, None, MAIN))
            .await?,
    )?)?;
    assert_eq!(main.owner, "m2_pgrel_owner");
    assert_eq!(main.tablespace.as_deref(), Some(TABLESPACE));
    assert_eq!(main.access_method.as_deref(), Some("heap"));
    assert!(main.rls_enabled && main.rls_forced);
    assert_eq!(main.replica_identity, "full");
    assert!(
        main.storage_parameters
            .iter()
            .any(|o| o.name == "toast.autovacuum_enabled" && o.value == "false")
    );
    let column = |name: &str| main.columns.iter().find(|c| c.name == name);
    let id = column("id").ok_or("id")?;
    let identity = id.identity.as_ref().ok_or("identity")?;
    assert_eq!(identity.generation, "always");
    assert_eq!(
        (identity.start, identity.increment, identity.cache),
        (Some(10), Some(5), Some(2))
    );
    let code = column("code").ok_or("code")?;
    assert_eq!(code.data_type, "character varying(20)");
    assert_eq!(code.collation.as_ref().map(|c| c.name.as_str()), Some("C"));
    assert_eq!(code.default.as_deref(), Some("'x'::character varying"));
    assert_eq!(code.comment.as_deref(), Some("the code"));
    let doubled = column("doubled").ok_or("doubled")?;
    assert!(doubled.generated.as_ref().is_some_and(|g| g.stored));
    let big = column("big").ok_or("big")?;
    assert_eq!(
        (big.storage.as_str(), big.statistics_target),
        ("external", Some(500))
    );
    assert_eq!(
        big.compression.as_deref(),
        if major >= 14 { Some("pglz") } else { None }
    );
    assert_eq!(code.not_null_constraint.is_some(), major >= 18);

    let constraint = |name: &str| main.constraints.iter().find(|c| c.name == name);
    let fk = constraint("ref_fk").ok_or("ref_fk")?;
    let key = fk.foreign_key.as_ref().ok_or("fk details")?;
    assert_eq!(
        (
            key.on_update.as_str(),
            key.on_delete.as_str(),
            key.match_type.as_str()
        ),
        ("cascade", "setNull", "full")
    );
    assert!(fk.deferrable && fk.deferred);
    assert!(!constraint("ref_code_fk").ok_or("ref_code_fk")?.validated);
    assert!(constraint("b_noinherit").ok_or("b_noinherit")?.no_inherit);
    assert_eq!(
        constraint("no_overlap")
            .ok_or("no_overlap")?
            .constraint_type,
        "exclusion"
    );

    let index = |name: &str| main.indexes.iter().find(|i| i.name == name);
    for (name, method) in [
        ("main_hash", "hash"),
        ("main_gin", "gin"),
        ("main_gist", "gist"),
        ("main_spgist", "spgist"),
        ("main_brin", "brin"),
    ] {
        assert_eq!(index(name).ok_or(name)?.method, method);
    }
    let include = index("main_include").ok_or("main_include")?;
    assert_eq!(include.include_columns, vec!["ref_code"]);
    assert_eq!(include.tablespace.as_deref(), Some(TABLESPACE));
    let expr = index("main_expr").ok_or("main_expr")?;
    assert!(expr.keys[0].expression && expr.keys[1].descending && !expr.keys[1].nulls_first);
    assert!(index("main_desc").ok_or("main_desc")?.clustered);
    assert_eq!(
        index("main_partial")
            .ok_or("main_partial")?
            .predicate
            .as_deref(),
        Some("amount > 100::numeric")
    );
    if major >= 15 {
        assert_eq!(
            index("main_nnd").ok_or("main_nnd")?.nulls_not_distinct,
            Some(true)
        );
    }

    let trigger = |name: &str| main.triggers.iter().find(|t| t.name == name);
    let before = trigger("t_before").ok_or("t_before")?;
    assert_eq!(before.update_columns, vec!["code", "amount"]);
    assert_eq!(before.timing, "before");
    let condition = before.condition.as_deref().ok_or("no condition")?;
    assert!(condition.contains("IS DISTINCT FROM") && condition.contains("') WHEN ('"));
    let stmt = trigger("t_after_stmt").ok_or("t_after_stmt")?;
    assert_eq!(
        (stmt.level.as_str(), stmt.new_table.as_deref()),
        ("statement", Some("inserted"))
    );
    assert_eq!(trigger("t_replica").ok_or("t_replica")?.enabled, "replica");
    assert!(
        trigger("t_constraint")
            .ok_or("t_constraint")?
            .constraint_trigger
    );
    assert_eq!(main.rules.len(), 2);
    let restrictive = main
        .policies
        .iter()
        .find(|p| p.name == "p_upd")
        .ok_or("p_upd")?;
    assert!(!restrictive.permissive);
    assert_eq!(restrictive.command, "update");
    assert_eq!(restrictive.roles, vec!["m2_pgrel_owner", "m2_pgrel_reader"]);
    let all = main
        .policies
        .iter()
        .find(|p| p.name == "p_all")
        .ok_or("p_all")?;
    assert_eq!(all.roles, vec!["public"]);
    assert_eq!(main.owned_sequences.len(), 1);

    let measures = table_of(relation(
        catalog
            .describe(
                src,
                &obj(ObjectKind::PartitionedTable, SCHEMA, None, "measures"),
            )
            .await?,
    )?)?;
    let partitioning = measures.partitioning.as_ref().ok_or("partitioning")?;
    assert_eq!(partitioning.strategy, "range");
    assert_eq!(partitioning.key, "RANGE (logdate)");
    assert_eq!(partitioning.partitions.len(), 4);
    assert_eq!(
        partitioning
            .default_partition
            .as_ref()
            .map(|p| p.name.as_str()),
        Some("measures_default")
    );
    let part = relation(
        catalog
            .describe(
                src,
                &obj(
                    ObjectKind::Partition,
                    SCHEMA,
                    Some("measures"),
                    "measures_2025",
                ),
            )
            .await?,
    )?;
    let PgRelationObject::Partition(part) = part else {
        return Err("measures_2025 is not a partition".into());
    };
    let of = part.partition_of.as_ref().ok_or("partition_of")?;
    assert_eq!(of.bound, "FOR VALUES FROM ('2025-01-01') TO ('2026-01-01')");
    assert!(part.columns.iter().all(|c| !c.local));

    let foreign = table_of(relation(
        catalog
            .describe(
                src,
                &obj(ObjectKind::ForeignTable, SCHEMA, None, "remote_items"),
            )
            .await?,
    )?)?;
    let info = foreign.foreign.as_ref().ok_or("foreign info")?;
    assert_eq!(info.server, "m2_pgrel_srv");
    assert_eq!(info.options.len(), 2);
    assert_eq!(
        foreign.columns[0].fdw_options[0].value.as_deref(),
        Some("item_id")
    );

    match relation(
        catalog
            .describe(src, &obj(ObjectKind::View, SCHEMA, None, "v_main"))
            .await?,
    )? {
        PgRelationObject::View(view) => {
            assert_eq!(view.check_option.as_deref(), Some("local"));
            assert!(view.security_barrier && !view.security_invoker);
            assert!(view.definition.contains("amount > 0"));
            assert!(!view.definition.ends_with(';'));
        }
        other => return Err(format!("not a view: {other:?}").into()),
    }
    match relation(
        catalog
            .describe(
                src,
                &obj(ObjectKind::MaterializedView, SCHEMA, None, "mv_ref"),
            )
            .await?,
    )? {
        PgRelationObject::MaterializedView(view) => {
            assert!(view.populated && view.refresh_concurrently);
            assert_eq!(view.indexes.len(), 1);
            assert!(!view.last_refresh_note.is_empty());
        }
        other => return Err(format!("not a materialized view: {other:?}").into()),
    }
    match relation(
        catalog
            .describe(src, &obj(ObjectKind::Sequence, SCHEMA, None, "seq_plain"))
            .await?,
    )? {
        PgRelationObject::Sequence(seq) => {
            assert_eq!(
                (
                    seq.data_type.as_str(),
                    seq.start,
                    seq.increment,
                    seq.min,
                    seq.max,
                    seq.cache,
                    seq.cycle
                ),
                ("integer", 5, 2, 1, 1000, 3, true)
            );
            // With CACHE 3 the value written to disk runs ahead of the 5 that
            // was handed out.
            assert_eq!(seq.last_value, Some(9));
            assert_eq!(seq.is_called, Some(true));
        }
        other => return Err(format!("not a sequence: {other:?}").into()),
    }
    // Identity sequences are described and owned by their column.
    let identity_seq = format!("{MAIN}_id_seq");
    match relation(
        catalog
            .describe(src, &obj(ObjectKind::Sequence, SCHEMA, None, &identity_seq))
            .await?,
    )? {
        PgRelationObject::Sequence(seq) => {
            assert_eq!(seq.identity.as_deref(), Some("always"));
            assert_eq!(seq.owned_by.as_ref().map(|o| o.column.as_str()), Some("id"));
        }
        other => return Err(format!("not a sequence: {other:?}").into()),
    }

    // Kind mismatches and missing objects.
    let wrong = catalog
        .describe(src, &obj(ObjectKind::View, SCHEMA, None, MAIN))
        .await
        .err()
        .ok_or("a table was described as a view")?;
    assert!(matches!(wrong, CatalogError::NotFound { .. }), "{wrong:?}");
    let builtin = catalog
        .ddl(src, &server_obj(ObjectKind::Tablespace, "pg_default"))
        .await
        .err()
        .ok_or("pg_default has DDL")?;
    assert!(
        matches!(builtin, CatalogError::Invalid { .. }),
        "{builtin:?}"
    );

    // The session's search path is left as it was.
    assert_eq!(
        scalar(src, "SHOW search_path").await?,
        Value::Text("\"$user\", public".into())
    );
    Ok(())
}

async fn compare_everything(
    catalog: &dyn Catalog,
    src: &dyn Session,
    dst: &dyn Session,
) -> TestResult {
    for schema in [SCHEMA, OTHER] {
        same(
            catalog,
            src,
            dst,
            &server_obj(ObjectKind::Schema, schema),
            &[],
        )
        .await?;
        let container = schema_container(schema);
        for count in catalog.kinds(src, &container, false).await? {
            // Kinds of the programs module (functions, types, ...) are
            // tested by that module.
            if !RELATION_KINDS.contains(&count.kind) {
                continue;
            }
            let in_src = catalog.list(src, &container, count.kind, false).await?;
            let in_dst = catalog.list(dst, &container, count.kind, false).await?;
            assert_eq!(
                names(&in_src),
                names(&in_dst),
                "{:?} in {schema}",
                count.kind
            );
            for item in &in_src {
                same(catalog, src, dst, &item.object, &[]).await?;
                if item.object.kind != ObjectKind::Sequence {
                    compare_children(catalog, src, dst, &item.object).await?;
                }
            }
        }
    }
    Ok(())
}

async fn compare_children(
    catalog: &dyn Catalog,
    src: &dyn Session,
    dst: &dyn Session,
    parent: &ObjectRef,
) -> TestResult {
    let container = Container {
        database: None,
        schema: parent.schema.clone(),
        parent: Some(parent.clone()),
    };
    for count in catalog.kinds(src, &container, false).await? {
        if !RELATION_KINDS.contains(&count.kind) {
            continue;
        }
        let in_src = catalog.list(src, &container, count.kind, false).await?;
        let in_dst = catalog.list(dst, &container, count.kind, false).await?;
        assert_eq!(
            names(&in_src),
            names(&in_dst),
            "{:?} of {}",
            count.kind,
            parent.name
        );
        for item in &in_src {
            same(catalog, src, dst, &item.object, &[]).await?;
            if count.kind == ObjectKind::Partition {
                Box::pin(compare_children(catalog, src, dst, &item.object)).await?;
            }
        }
    }
    Ok(())
}

/// Drops every child of the main table in the copy, one at a time,
/// recreates it from its own DDL and compares.
async fn children_round_trip(
    catalog: &dyn Catalog,
    src: &dyn Session,
    dst: &dyn Session,
    major: u32,
) -> TestResult {
    let table = format!("{SCHEMA}.\"Main \"\"Table\"\"\"");
    let main = Container {
        database: None,
        schema: Some(SCHEMA.to_owned()),
        parent: Some(obj(ObjectKind::Table, SCHEMA, None, MAIN)),
    };
    let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
    for constraint in catalog
        .list(src, &main, ObjectKind::Constraint, false)
        .await?
    {
        let name = &constraint.object.name;
        let is_not_null = constraint
            .detail
            .as_deref()
            .is_some_and(|d| d.starts_with("not null"));
        if is_not_null && !name.ends_with("_code_not_null") {
            // Identity and primary key columns keep their not-null
            // constraints; one plain one is enough.
            continue;
        }
        exec(
            dst,
            &format!("ALTER TABLE {table} DROP CONSTRAINT {}", quote(name)),
        )
        .await?;
        exec(dst, &ddl(catalog, src, &constraint.object).await?).await?;
        same(catalog, src, dst, &constraint.object, &[]).await?;
    }
    for index in catalog.list(src, &main, ObjectKind::Index, false).await? {
        let details = relation(catalog.describe(src, &index.object).await?)?;
        let PgRelationObject::Index(details) = details else {
            return Err("not an index".into());
        };
        match &details.constraint {
            Some(constraint) => {
                exec(
                    dst,
                    &format!("ALTER TABLE {table} DROP CONSTRAINT {}", quote(constraint)),
                )
                .await?
            }
            None => {
                exec(
                    dst,
                    &format!("DROP INDEX {SCHEMA}.{}", quote(&index.object.name)),
                )
                .await?
            }
        }
        exec(dst, &ddl(catalog, src, &index.object).await?).await?;
        same(catalog, src, dst, &index.object, &[]).await?;
    }
    for trigger in catalog.list(src, &main, ObjectKind::Trigger, false).await? {
        exec(
            dst,
            &format!("DROP TRIGGER {} ON {table}", quote(&trigger.object.name)),
        )
        .await?;
        exec(dst, &ddl(catalog, src, &trigger.object).await?).await?;
        same(catalog, src, dst, &trigger.object, &[]).await?;
    }
    for rule in catalog.list(src, &main, ObjectKind::Rule, false).await? {
        exec(
            dst,
            &format!("DROP RULE {} ON {table}", quote(&rule.object.name)),
        )
        .await?;
        exec(dst, &ddl(catalog, src, &rule.object).await?).await?;
        same(catalog, src, dst, &rule.object, &[]).await?;
    }
    for policy in catalog.list(src, &main, ObjectKind::Policy, false).await? {
        exec(
            dst,
            &format!("DROP POLICY {} ON {table}", quote(&policy.object.name)),
        )
        .await?;
        exec(dst, &ddl(catalog, src, &policy.object).await?).await?;
        same(catalog, src, dst, &policy.object, &[]).await?;
    }
    // A column comes back at the end of the table.
    let big = obj(ObjectKind::Column, SCHEMA, Some(MAIN), "big");
    exec(dst, &format!("ALTER TABLE {table} DROP COLUMN big")).await?;
    exec(dst, &ddl(catalog, src, &big).await?).await?;
    same(catalog, src, dst, &big, &["position"]).await?;
    // A view column's DDL is its settable parts.
    let view_column = obj(ObjectKind::Column, SCHEMA, Some("v_main"), "amount");
    let script = ddl(catalog, src, &view_column).await?;
    assert!(script.starts_with("-- Column"), "{script}");
    assert!(script.contains("SET DEFAULT 1"), "{script}");
    if major >= 18 {
        let code = relation(
            catalog
                .describe(dst, &obj(ObjectKind::Column, SCHEMA, Some(MAIN), "code"))
                .await?,
        )?;
        let PgRelationObject::Column(code) = code else {
            return Err("not a column".into());
        };
        assert!(code.not_null && code.not_null_constraint.is_some());
    }
    Ok(())
}

async fn sequence_round_trip(
    catalog: &dyn Catalog,
    src: &dyn Session,
    dst: &dyn Session,
) -> TestResult {
    let table = format!("{SCHEMA}.\"Main \"\"Table\"\"\"");
    let plain = obj(ObjectKind::Sequence, SCHEMA, None, "seq_plain");
    exec(dst, &format!("DROP SEQUENCE {SCHEMA}.seq_plain CASCADE")).await?;
    exec(dst, &ddl(catalog, src, &plain).await?).await?;
    same(catalog, src, dst, &plain, &[]).await?;

    let owned = obj(ObjectKind::Sequence, SCHEMA, None, "seq_owned");
    exec(dst, &format!("DROP SEQUENCE {SCHEMA}.seq_owned")).await?;
    let script = ddl(catalog, src, &owned).await?;
    assert!(script.contains("OWNED BY"), "{script}");
    exec(dst, &script).await?;
    same(catalog, src, dst, &owned, &[]).await?;

    let identity = obj(
        ObjectKind::Sequence,
        SCHEMA,
        None,
        &format!("{MAIN}_id_seq"),
    );
    exec(
        dst,
        &format!("ALTER TABLE {table} ALTER COLUMN id DROP IDENTITY"),
    )
    .await?;
    exec(dst, &ddl(catalog, src, &identity).await?).await?;
    same(catalog, src, dst, &identity, &[]).await?;
    same(
        catalog,
        src,
        dst,
        &obj(ObjectKind::Column, SCHEMA, Some(MAIN), "id"),
        &[],
    )
    .await?;
    Ok(())
}

async fn databases_round_trip(
    catalog: &dyn Catalog,
    src: &dyn Session,
    admin: &dyn Session,
    major: u32,
) -> TestResult {
    exec(
        admin,
        &format!(
            "CREATE DATABASE {DB1} WITH OWNER m2_pgrel_owner TEMPLATE template0 ENCODING 'UTF8'
               LC_COLLATE 'C' LC_CTYPE 'C' CONNECTION LIMIT 5 TABLESPACE {TABLESPACE};
             COMMENT ON DATABASE {DB1} IS 'database one';
             ALTER DATABASE {DB1} SET work_mem = '8MB';
             ALTER DATABASE {DB1} SET search_path = \"$user\", public, \"Odd \"\"x\"\"\";
             ALTER ROLE m2_pgrel_owner IN DATABASE {DB1} SET statement_timeout = '5s';"
        ),
    )
    .await?;
    let mut dbs = vec![DB1];
    if major >= 15 {
        let rules = if major >= 16 {
            "ICU_RULES '&a < b'"
        } else {
            ""
        };
        exec(
            admin,
            &format!(
                "CREATE DATABASE {DB2} WITH TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER icu
                   ICU_LOCALE 'en-US' {rules} LC_COLLATE 'C' LC_CTYPE 'C' IS_TEMPLATE true
                   ALLOW_CONNECTIONS false"
            ),
        )
        .await?;
        dbs.push(DB2);
    }
    for db in dbs {
        let object = server_obj(ObjectKind::Database, db);
        let before = described(catalog, src, &object, &[]).await?;
        let script = ddl(catalog, src, &object).await?;
        exec(admin, &format!("ALTER DATABASE {db} IS_TEMPLATE false")).await?;
        exec(admin, &format!("DROP DATABASE {db}")).await?;
        exec(admin, &script).await?;
        let after = described(catalog, src, &object, &[]).await?;
        assert_eq!(before, after, "database {db}\n{script}");
        if db == DB1 {
            let PgRelationObject::Database(details) =
                relation(catalog.describe(src, &object).await?)?
            else {
                return Err("not a database".into());
            };
            assert_eq!(details.settings.len(), 3);
            assert_eq!(details.connection_limit, 5);
            assert_eq!(details.tablespace, TABLESPACE);
        } else {
            let PgRelationObject::Database(details) =
                relation(catalog.describe(src, &object).await?)?
            else {
                return Err("not a database".into());
            };
            assert_eq!(details.locale_provider, "icu");
            assert!(details.is_template && !details.allow_connections);
        }
    }
    Ok(())
}

async fn tablespace_round_trip(catalog: &dyn Catalog, admin: &dyn Session) -> TestResult {
    for db in [SRC, DST, DB1] {
        exec(admin, &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await?;
    }
    let object = server_obj(ObjectKind::Tablespace, TABLESPACE);
    let before = described(catalog, admin, &object, &[]).await?;
    let script = ddl(catalog, admin, &object).await?;
    exec(admin, &format!("DROP TABLESPACE {TABLESPACE}")).await?;
    exec(admin, &script).await?;
    let after = described(catalog, admin, &object, &[]).await?;
    assert_eq!(before, after, "{script}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn round_trip_postgres_13() -> TestResult {
    round_trip(13, port("VARVE_M2_PG13_PORT", 55513)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn round_trip_postgres_18() -> TestResult {
    round_trip(18, port("VARVE_M2_PG18_PORT", 55518)).await
}

/// Foreign table and column options keep only values that are known to be
/// harmless and carry no credentials, in details and in DDL.
async fn redaction(catalog: &dyn Catalog, src: &dyn Session) -> TestResult {
    let object = obj(ObjectKind::ForeignTable, "m2_pgrel_secret", None, "leaky");
    let table = table_of(relation(catalog.describe(src, &object).await?)?)?;
    let options = &table.foreign.as_ref().ok_or("foreign info")?.options;
    let value = |name: &str| {
        options
            .iter()
            .find(|o| o.name == name)
            .map(|o| o.value.clone())
    };
    assert_eq!(value("url"), Some(None), "{options:?}");
    assert_eq!(value("schema_name"), Some(None), "{options:?}");
    // A wrapper without a known extension shows no value at all.
    assert_eq!(value("table_name"), Some(None), "{options:?}");
    let column = &table.columns[0].fdw_options;
    assert!(
        column
            .iter()
            .any(|o| o.name == "password" && o.value.is_none()),
        "{column:?}"
    );
    assert!(
        column
            .iter()
            .any(|o| o.name == "column_name" && o.value.is_none()),
        "{column:?}"
    );
    let json = serde_json::to_string(&catalog.describe(src, &object).await?)?;
    let script = ddl(catalog, src, &object).await?;
    for secret in ["SECRET", "colpw", "a:b@"] {
        assert!(!json.contains(secret), "{secret} in {json}");
        assert!(!script.contains(secret), "{secret} in {script}");
    }
    assert!(
        script.starts_with("-- Option values that may hold secrets"),
        "{script}"
    );
    assert!(script.contains("'<redacted>'"), "{script}");
    let column = obj(ObjectKind::Column, "m2_pgrel_secret", Some("leaky"), "id");
    let script = ddl(catalog, src, &column).await?;
    assert!(
        !script.contains("colpw") && script.contains("'<redacted>'"),
        "{script}"
    );
    Ok(())
}

/// Every kind is listed exactly where `kinds` counts it.
async fn containers(catalog: &dyn Catalog, src: &dyn Session) -> TestResult {
    let server = Container::default();
    let database = Container {
        database: Some(SRC.to_owned()),
        schema: None,
        parent: None,
    };
    let schema = schema_container(SCHEMA);
    for (container, kind) in [
        (&schema, ObjectKind::Partition),
        (&schema, ObjectKind::Database),
        (&schema, ObjectKind::Schema),
        (&server, ObjectKind::Table),
        (&server, ObjectKind::Schema),
        (&database, ObjectKind::Database),
        (&database, ObjectKind::Tablespace),
        (&database, ObjectKind::Table),
    ] {
        let error = catalog
            .list(src, container, kind, false)
            .await
            .err()
            .ok_or_else(|| format!("{kind:?} listed in {container:?}"))?;
        assert!(matches!(error, CatalogError::Invalid { .. }), "{error:?}");
    }
    for (container, counts) in [
        (&server, catalog.kinds(src, &server, false).await?),
        (&database, catalog.kinds(src, &database, false).await?),
        (&schema, catalog.kinds(src, &schema, false).await?),
    ] {
        for count in counts.iter().filter(|c| RELATION_KINDS.contains(&c.kind)) {
            catalog.list(src, container, count.kind, false).await?;
        }
    }
    Ok(())
}

/// Inside a user's transaction block, details and DDL are refused without
/// touching the transaction, lists still work, and the search path is
/// never changed.
async fn in_transaction(catalog: &dyn Catalog, src: &dyn Session) -> TestResult {
    const MESSAGE: &str =
        "the catalog cannot be read inside an open transaction: finish the transaction first";
    let refused = |error: Option<CatalogError>, what: &str| -> TestResult {
        match error {
            Some(CatalogError::Invalid { message }) if message == MESSAGE => Ok(()),
            other => Err(format!("{what} inside a transaction: {other:?}").into()),
        }
    };
    let path = scalar(src, "SHOW search_path").await?;
    exec(src, "BEGIN").await?;
    let main = obj(ObjectKind::Table, SCHEMA, None, MAIN);
    let schema = schema_container(SCHEMA);
    let children = Container {
        parent: Some(main.clone()),
        ..schema_container(SCHEMA)
    };
    refused(catalog.describe(src, &main).await.err(), "describe")?;
    refused(catalog.ddl(src, &main).await.err(), "ddl")?;
    refused(
        catalog
            .list(src, &schema, ObjectKind::Table, false)
            .await
            .err(),
        "list(Table)",
    )?;
    refused(
        catalog
            .list(src, &children, ObjectKind::Index, false)
            .await
            .err(),
        "list(Index)",
    )?;
    refused(catalog.kinds(src, &schema, false).await.err(), "kinds")?;
    refused(
        catalog
            .list(src, &Container::default(), ObjectKind::Database, false)
            .await
            .err(),
        "list(Database)",
    )?;
    // The transaction is still usable and its search path untouched.
    assert_eq!(scalar(src, "SELECT 1").await?, Value::Int(1));
    assert_eq!(scalar(src, "SHOW search_path").await?, path);
    exec(src, "ROLLBACK").await?;
    assert_eq!(scalar(src, "SHOW search_path").await?, path);

    // A transaction that already failed gets the same refusal, not the
    // server's "current transaction is aborted".
    exec(src, "BEGIN").await?;
    assert!(exec(src, "SELECT 1 / 0").await.is_err());
    refused(
        catalog.describe(src, &main).await.err(),
        "describe (failed)",
    )?;
    refused(catalog.ddl(src, &main).await.err(), "ddl (failed)")?;
    refused(
        catalog
            .list(src, &children, ObjectKind::Column, false)
            .await
            .err(),
        "list(Column) (failed)",
    )?;
    refused(
        catalog.kinds(src, &schema, false).await.err(),
        "kinds (failed)",
    )?;
    exec(src, "ROLLBACK").await?;
    assert_eq!(scalar(src, "SHOW search_path").await?, path);
    Ok(())
}

/// A table held in ACCESS EXCLUSIVE mode by another session: lists do not
/// wait, details fail fast with a lock error, a dropped call leaves the
/// session's search path alone, and exact sizes come back afterwards.
async fn locked(catalog: &dyn Catalog, src: &dyn Session, port: u16) -> TestResult {
    let limit = Duration::from_secs(10);
    let path = scalar(src, "SHOW search_path").await?;
    let locker = connect(port, SRC).await?;
    exec(
        locker.as_ref(),
        &format!(
            "BEGIN;
             LOCK TABLE {SCHEMA}.\"Main \"\"Table\"\"\", {SCHEMA}.measures, {SCHEMA}.measures_2024
               IN ACCESS EXCLUSIVE MODE;
             ALTER SEQUENCE {SCHEMA}.seq_plain RENAME TO seq_plain_locked"
        ),
    )
    .await?;
    let schema = schema_container(SCHEMA);
    let result: TestResult = async {
        // Every list and count of the schema, and the partitions of the
        // locked partitioned table, return at once: no list deparses
        // anything that takes a lock.
        let quick = Duration::from_millis(1500);
        let started = Instant::now();
        let counts = catalog
            .kinds(src, &schema, false)
            .await
            .map_err(|e| format!("kinds: {e}"))?;
        assert!(
            started.elapsed() < quick,
            "kinds took {:?}",
            started.elapsed()
        );
        for count in counts.iter().filter(|c| RELATION_KINDS.contains(&c.kind)) {
            let started = Instant::now();
            let list = catalog
                .list(src, &schema, count.kind, false)
                .await
                .map_err(|e| format!("list({:?}): {e}", count.kind))?;
            assert!(
                started.elapsed() < quick,
                "list({:?}) took {:?}",
                count.kind,
                started.elapsed()
            );
            assert_eq!(list.len() as u64, count.count, "{:?}", count.kind);
        }
        let measures = Container {
            parent: Some(obj(ObjectKind::PartitionedTable, SCHEMA, None, "measures")),
            ..schema_container(SCHEMA)
        };
        let started = Instant::now();
        let partitions = catalog
            .list(src, &measures, ObjectKind::Partition, false)
            .await
            .map_err(|e| format!("list(Partition): {e}"))?;
        assert!(
            started.elapsed() < quick,
            "partitions took {:?}",
            started.elapsed()
        );
        let first = partitions.first().ok_or("no partitions")?;
        assert_eq!(
            first.detail.as_deref(),
            Some("FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')")
        );
        let tables =
            tokio::time::timeout(limit, catalog.list(src, &schema, ObjectKind::Table, false))
                .await
                .map_err(|_| "list(Table) waited for the lock")??;
        let main = tables
            .iter()
            .find(|t| t.object.name == MAIN)
            .ok_or("main table not listed")?;
        assert!(main.detail.is_some());
        // The size comes from pg_class, which no lock blocks.
        assert!(main.size_bytes.is_some(), "{main:?}");
        tokio::time::timeout(limit, catalog.kinds(src, &schema, false))
            .await
            .map_err(|_| "kinds waited for the lock")??;
        tokio::time::timeout(
            limit,
            catalog.list(src, &schema, ObjectKind::Sequence, false),
        )
        .await
        .map_err(|_| "list(Sequence) waited for the lock")??;
        let object = obj(ObjectKind::Table, SCHEMA, None, MAIN);
        let started = Instant::now();
        let error = tokio::time::timeout(limit, catalog.describe(src, &object))
            .await
            .map_err(|_| "describe waited for the lock")?
            .err()
            .ok_or("a locked table was described")?;
        assert!(started.elapsed() < Duration::from_secs(5));
        match &error {
            CatalogError::Driver { error } => {
                assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
                assert_eq!(error.code.as_deref(), Some("55P03"));
                assert!(error.message.contains("locked by another session"));
            }
            other => return Err(format!("unexpected error {other:?}").into()),
        }
        // The children of the locked table are listed at once too: the
        // child lists read catalog columns only and leave deparsing (which
        // opens the table) to describe.
        let children = Container {
            parent: Some(object.clone()),
            ..schema_container(SCHEMA)
        };
        let started = Instant::now();
        let counts = catalog
            .kinds(src, &children, false)
            .await
            .map_err(|e| format!("kinds of the locked table: {e}"))?;
        let listed: Vec<ObjectKind> = counts.iter().map(|c| c.kind).collect();
        for kind in [
            ObjectKind::Column,
            ObjectKind::Index,
            ObjectKind::Constraint,
            ObjectKind::Trigger,
            ObjectKind::Rule,
            ObjectKind::Policy,
        ] {
            assert!(listed.contains(&kind), "{kind:?} missing in {listed:?}");
        }
        for count in &counts {
            let list = catalog
                .list(src, &children, count.kind, false)
                .await
                .map_err(|e| format!("list({:?}) of the locked table: {e}", count.kind))?;
            assert_eq!(list.len() as u64, count.count, "{:?}", count.kind);
            assert!(list.iter().all(|entry| entry.detail.is_some()), "{list:?}");
        }
        assert!(
            started.elapsed() < quick,
            "the children took {:?}",
            started.elapsed()
        );
        // Describing an index deparses it (pg_get_indexdef opens the
        // table) and fails fast on the lock.
        let index = catalog
            .list(src, &children, ObjectKind::Index, false)
            .await?
            .into_iter()
            .next()
            .ok_or("no indexes")?;
        let error = tokio::time::timeout(limit, catalog.describe(src, &index.object))
            .await
            .map_err(|_| "describe(Index) waited for the lock")?
            .err()
            .ok_or("an index of a locked table was described")?;
        assert!(
            matches!(&error, CatalogError::Driver { error } if error.code.as_deref() == Some("55P03")),
            "{error:?}"
        );
        // Unrelated objects are still described.
        catalog
            .describe(src, &obj(ObjectKind::Table, SCHEMA, None, "ref"))
            .await
            .map_err(|e| format!("describe(ref): {e}"))?;
        // A call dropped while it waits leaves nothing behind.
        for wait in [0, 5, 200] {
            let dropped =
                tokio::time::timeout(Duration::from_millis(wait), catalog.describe(src, &object))
                    .await;
            assert!(dropped.is_err() || wait == 0, "finished within {wait} ms");
            assert_eq!(scalar(src, "SHOW search_path").await?, path);
            assert_eq!(
                scalar(src, "SHOW lock_timeout").await?,
                Value::Text("0".into())
            );
        }
        Ok(())
    }
    .await;
    // Roll back: the holder renamed the sequence only to lock it.
    exec(locker.as_ref(), "ROLLBACK").await?;
    result?;
    let table = table_of(relation(
        catalog
            .describe(src, &obj(ObjectKind::Table, SCHEMA, None, MAIN))
            .await?,
    )?)?;
    assert!(
        table.total_bytes.is_some_and(|b| b > 0),
        "{:?}",
        table.total_bytes
    );
    assert_eq!(table.size_note, None);
    Ok(())
}

/// DDL fixes from the audit, each replayed in the copy and compared.
async fn audit_round_trips(
    catalog: &dyn Catalog,
    src: &dyn Session,
    dst: &dyn Session,
) -> TestResult {
    // A partitioned index comes back valid with every attached index.
    let index = obj(
        ObjectKind::Index,
        SCHEMA,
        Some("measures"),
        "measures_v_idx",
    );
    exec(dst, &format!("DROP INDEX {SCHEMA}.measures_v_idx")).await?;
    let script = ddl(catalog, src, &index).await?;
    assert!(script.contains("ATTACH PARTITION"), "{script}");
    exec(dst, &script).await?;
    same(catalog, src, dst, &index, &[]).await?;
    let measures = obj(ObjectKind::PartitionedTable, SCHEMA, None, "measures");
    same(catalog, src, dst, &measures, &[]).await?;
    compare_children(catalog, src, dst, &measures).await?;

    // Column and check constraint of an INHERITS parent reach the child.
    exec(
        dst,
        &format!("ALTER TABLE {SCHEMA}.inh_parent DROP COLUMN shared"),
    )
    .await?;
    let shared = obj(ObjectKind::Column, SCHEMA, Some("inh_parent"), "shared");
    exec(dst, &ddl(catalog, src, &shared).await?).await?;
    same(catalog, src, dst, &shared, &["position"]).await?;
    // Re-added columns go to the end of the table and its children.
    same(
        catalog,
        src,
        dst,
        &obj(ObjectKind::Column, SCHEMA, Some("inh_child"), "shared"),
        &["position"],
    )
    .await?;
    exec(
        dst,
        &format!("ALTER TABLE {SCHEMA}.inh_parent DROP CONSTRAINT inh_ck"),
    )
    .await?;
    let check = obj(ObjectKind::Constraint, SCHEMA, Some("inh_parent"), "inh_ck");
    exec(dst, &ddl(catalog, src, &check).await?).await?;
    same(catalog, src, dst, &check, &[]).await?;
    same(
        catalog,
        src,
        dst,
        &obj(ObjectKind::Constraint, SCHEMA, Some("inh_child"), "inh_ck"),
        &[],
    )
    .await?;

    // A serial column brings its sequence.
    let table = format!("{SCHEMA}.\"Main \"\"Table\"\"\"");
    exec(dst, &format!("ALTER TABLE {table} DROP COLUMN serial_col")).await?;
    let serial = obj(ObjectKind::Column, SCHEMA, Some(MAIN), "serial_col");
    exec(dst, &ddl(catalog, src, &serial).await?).await?;
    same(catalog, src, dst, &serial, &["position"]).await?;
    same(
        catalog,
        src,
        dst,
        &obj(
            ObjectKind::Sequence,
            SCHEMA,
            None,
            &format!("{MAIN}_serial_col_seq"),
        ),
        &[],
    )
    .await?;

    // An index used as replica identity keeps that role.
    exec(
        dst,
        &format!("ALTER TABLE {SCHEMA}.unlogged_t DROP CONSTRAINT unlogged_t_pkey"),
    )
    .await?;
    let pkey = obj(
        ObjectKind::Index,
        SCHEMA,
        Some("unlogged_t"),
        "unlogged_t_pkey",
    );
    let script = ddl(catalog, src, &pkey).await?;
    assert!(script.contains("REPLICA IDENTITY USING INDEX"), "{script}");
    exec(dst, &script).await?;
    same(
        catalog,
        src,
        dst,
        &obj(ObjectKind::Table, SCHEMA, None, "unlogged_t"),
        &[],
    )
    .await?;
    Ok(())
}
