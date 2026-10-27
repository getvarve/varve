//! Integration tests of the PostgreSQL programs catalog (functions,
//! procedures, aggregates, types, extensions, replication, foreign data,
//! collations, casts, operators, event triggers) against the M2 matrix:
//!
//! ```sh
//! cargo test -p varve-catalog --test pg_programs -- --include-ignored
//! ```
//!
//! Every test works in its own `m2_pgprog_*` databases, drops them before
//! it starts and again when it ends. Round trips create fixtures, read them
//! back, run the generated DDL in a fresh database and compare the details
//! read from both.

use std::collections::BTreeMap;
use std::time::Duration;

use varve_catalog::postgres::PgObject;
use varve_catalog::postgres::programs::{
    self, PgAggregateKind, PgArgumentMode, PgProgramObject, PgTriggerState,
};
use varve_catalog::{Catalog, CatalogError, Container, ObjectDetails, ObjectKind, ObjectRef};
use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, ExecOptions, ExecOutput, Session, StatementResult, Value,
};
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const SCHEMA: &str = "m2_pgprog";
const FIXTURE_PASSWORD: &str = "fixture pw 7Qx";
/// Every secret the fixtures hold; none may appear in details or DDL.
const SECRETS: &[&str] = &[
    FIXTURE_PASSWORD,
    "SECRETQ",
    "SECRETD",
    "SECRETAPI",
    "SECRETK",
    "SECRETV",
    "remote_user",
];

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

/// Whether the server with `dev/docker-compose.yml` service name `service`
/// (for example `postgres-18`) is tested. `VARVE_ONLY_SERVERS`, a comma
/// separated list of service names, limits a run to those servers (CI tests
/// only the newest version of each engine); without it every server is tested.
fn selected(service: &str) -> bool {
    std::env::var("VARVE_ONLY_SERVERS")
        .ok()
        .is_none_or(|list| list.split(',').any(|name| name.trim() == service))
}

/// Oldest and newest supported servers as `(major, port)` (see
/// [`selected`]), never none.
fn servers() -> Vec<(u32, u16)> {
    let servers: Vec<(u32, u16)> = [
        (13, port("VARVE_PG13_PORT", 55513)),
        (18, port("VARVE_PG18_PORT", 55518)),
    ]
    .into_iter()
    .filter(|(major, _)| selected(&format!("postgres-{major}")))
    .collect();
    assert!(
        !servers.is_empty(),
        "VARVE_ONLY_SERVERS selects no PostgreSQL server"
    );
    servers
}

fn params(port: u16, database: &str) -> ConnectParams {
    let mut params =
        ConnectParams::new(Engine::Postgres, env_or("VARVE_PG_HOST", "127.0.0.1"), port);
    params.database = Some(database.to_owned());
    params.username = Some(env_or("VARVE_PG_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_PG_PASSWORD", "varve_dev")));
    params.connect_timeout = Duration::from_secs(10);
    params.application_name = "varve-catalog pg_programs tests".into();
    params
}

async fn connect(port: u16, database: &str) -> TestResult<Box<dyn Session>> {
    Ok(PostgresDriver::new()
        .connect(&params(port, database))
        .await?)
}

fn admin_database() -> String {
    env_or("VARVE_PG_DATABASE", "varve")
}

async fn exec(session: &dyn Session, sql: &str) -> TestResult<ExecOutput> {
    Ok(session.execute(sql, &ExecOptions::default()).await?)
}

async fn texts(session: &dyn Session, sql: &str) -> TestResult<Vec<String>> {
    let output = exec(session, sql).await?;
    let mut out = Vec::new();
    for result in output.results {
        if let StatementResult::Rows { rows, .. } = result {
            for row in rows {
                if let Some(value) = row.first().and_then(Value::to_plain_text) {
                    out.push(value);
                }
            }
        }
    }
    Ok(out)
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Drops a test database, first dropping its subscriptions (which block
/// `DROP DATABASE`) without touching a remote slot.
async fn drop_database(port: u16, database: &str) -> TestResult {
    let admin = connect(port, &admin_database()).await?;
    let exists = texts(
        admin.as_ref(),
        &format!("SELECT datname FROM pg_database WHERE datname = '{database}'"),
    )
    .await?;
    if !exists.is_empty() {
        let session = connect(port, database).await?;
        let subscriptions = texts(
            session.as_ref(),
            "SELECT subname FROM pg_subscription WHERE subdbid = \
             (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .await?;
        for name in subscriptions {
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

async fn fresh_database(port: u16, database: &str) -> TestResult<Box<dyn Session>> {
    drop_database(port, database).await?;
    let admin = connect(port, &admin_database()).await?;
    exec(
        admin.as_ref(),
        &format!("CREATE DATABASE {}", quote(database)),
    )
    .await?;
    admin.close().await?;
    connect(port, database).await
}

/// Objects the fixtures depend on that are not part of this catalog
/// (schemas and tables); created in the source and the copy.
fn prerequisites(major: u32) -> String {
    let mut sql = format!(
        "CREATE SCHEMA {SCHEMA};
         CREATE TABLE {SCHEMA}.t1 (id integer PRIMARY KEY, a text, b integer);
         CREATE TABLE {SCHEMA}.t2 (id integer PRIMARY KEY);"
    );
    if major >= 15 {
        sql.push_str(
            "CREATE SCHEMA m2_pgprog_pubs;
             CREATE TABLE m2_pgprog_pubs.t3 (id integer PRIMARY KEY);",
        );
    }
    sql
}

/// Every kind of this catalog, with overloads, a procedure with `OUT`
/// (`INOUT` on 13) parameters, SQL-standard bodies on 14+, and extensions.
fn fixtures(major: u32) -> String {
    let s = SCHEMA;
    let multirange = if major >= 14 {
        format!(", multirange_type_name = {s}.floatmulti")
    } else {
        String::new()
    };
    let out_mode = if major >= 14 { "OUT" } else { "INOUT" };
    let mut sql = format!(
        "CREATE EXTENSION citext WITH SCHEMA {s};
         CREATE EXTENSION postgres_fdw WITH SCHEMA {s};
         CREATE TYPE {s}.mood AS ENUM ('sad', 'it''s ok', 'happy');
         COMMENT ON TYPE {s}.mood IS 'feelings';
         CREATE TYPE {s}.pair AS (a integer, b text COLLATE \"C\", c numeric(10,2));
         COMMENT ON COLUMN {s}.pair.b IS 'bee';
         CREATE DOMAIN {s}.posint AS integer DEFAULT 1 NOT NULL CONSTRAINT positive CHECK (VALUE > 0);
         ALTER DOMAIN {s}.posint ADD CONSTRAINT small CHECK (VALUE < 1000000) NOT VALID;
         COMMENT ON CONSTRAINT positive ON DOMAIN {s}.posint IS 'above zero';
         CREATE DOMAIN {s}.code AS text COLLATE \"C\" CHECK (VALUE ~ '^[A-Z]+$');
         CREATE TYPE {s}.floatrange AS RANGE (subtype = float8, subtype_diff = float8mi{multirange});
         CREATE TYPE {s}.textrange AS RANGE (subtype = text, collation = \"C\");
         CREATE FUNCTION {s}.add(a integer, b integer DEFAULT 1) RETURNS integer
             LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$ SELECT a + b $$;
         COMMENT ON FUNCTION {s}.add(integer, integer) IS 'adds';
         CREATE FUNCTION {s}.add(a text, b text) RETURNS text LANGUAGE plpgsql STABLE
             SECURITY DEFINER COST 50 SET search_path = pg_catalog
             AS $$ BEGIN RETURN a || b; END $$;
         CREATE FUNCTION {s}.series(n integer) RETURNS TABLE(i integer, sq bigint)
             LANGUAGE sql ROWS 10 AS $$ SELECT g, g::bigint * g FROM generate_series(1, n) g $$;
         CREATE FUNCTION {s}.evt() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN END $$;
         CREATE FUNCTION {s}.eq(integer, integer) RETURNS boolean LANGUAGE sql IMMUTABLE
             LEAKPROOF AS 'SELECT $1 = $2';
         CREATE FUNCTION {s}.ne(integer, integer) RETURNS boolean LANGUAGE sql IMMUTABLE
             AS 'SELECT $1 <> $2';
         CREATE FUNCTION {s}.neg(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT -$1';
         CREATE FUNCTION {s}.mood_int({s}.mood) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT 1';
         CREATE FUNCTION {s}.sum_state(bigint, integer) RETURNS bigint LANGUAGE sql IMMUTABLE
             AS 'SELECT $1 + $2';
         CREATE FUNCTION {s}.sum_inv(bigint, integer) RETURNS bigint LANGUAGE sql IMMUTABLE
             AS 'SELECT $1 - $2';
         CREATE FUNCTION {s}.sum_final(bigint) RETURNS text LANGUAGE sql IMMUTABLE
             AS 'SELECT $1::text';
         CREATE FUNCTION {s}.os_state(integer[], integer) RETURNS integer[] LANGUAGE sql IMMUTABLE
             AS 'SELECT $1 || $2';
         CREATE FUNCTION {s}.os_final(integer[], text, integer) RETURNS text LANGUAGE sql IMMUTABLE
             AS 'SELECT $2 || array_length($1, 1)';
         CREATE FUNCTION {s}.hyp_final(integer[], integer, integer) RETURNS integer
             LANGUAGE sql IMMUTABLE AS 'SELECT coalesce(array_length($1, 1), 0)';
         CREATE PROCEDURE {s}.double_it(IN a integer, {out_mode} b integer) LANGUAGE plpgsql
             AS $$ BEGIN b := a * 2; END $$;
         COMMENT ON PROCEDURE {s}.double_it IS 'doubles';
         CREATE AGGREGATE {s}.total(integer) (SFUNC = {s}.sum_state, STYPE = bigint,
             FINALFUNC = {s}.sum_final, INITCOND = '0', MSFUNC = {s}.sum_state,
             MINVFUNC = {s}.sum_inv, MSTYPE = bigint, MFINALFUNC = {s}.sum_final, MINITCOND = '0', PARALLEL = SAFE);
         COMMENT ON AGGREGATE {s}.total(integer) IS 'running total';
         CREATE AGGREGATE {s}.cnt(*) (SFUNC = int8inc, STYPE = int8, INITCOND = '0');
         CREATE AGGREGATE {s}.os_collect(text ORDER BY integer) (SFUNC = {s}.os_state,
             STYPE = integer[], FINALFUNC = {s}.os_final, FINALFUNC_EXTRA, INITCOND = '{{}}');
         CREATE AGGREGATE {s}.hyp(integer ORDER BY integer) (SFUNC = {s}.os_state,
             STYPE = integer[], FINALFUNC = {s}.hyp_final, FINALFUNC_EXTRA, INITCOND = '{{}}',
             HYPOTHETICAL);
         CREATE AGGREGATE {s}.maxint(integer) (SFUNC = int4larger, STYPE = integer,
             COMBINEFUNC = int4larger, SORTOP = >);
         CREATE OPERATOR {s}.=== (FUNCTION = {s}.eq, LEFTARG = integer, RIGHTARG = integer,
             COMMUTATOR = OPERATOR({s}.===), NEGATOR = OPERATOR({s}.!==),
             RESTRICT = eqsel, JOIN = eqjoinsel, HASHES, MERGES);
         CREATE OPERATOR {s}.!== (FUNCTION = {s}.ne, LEFTARG = integer, RIGHTARG = integer,
             COMMUTATOR = OPERATOR({s}.!==), NEGATOR = OPERATOR({s}.===));
         CREATE OPERATOR {s}.~~~ (FUNCTION = {s}.neg, RIGHTARG = integer);
         COMMENT ON OPERATOR {s}.~~~ (NONE, integer) IS 'negate';
         CREATE COLLATION {s}.ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE COLLATION {s}.c_copy (provider = libc, locale = 'C');
         COMMENT ON COLLATION {s}.ci IS 'case insensitive';
         CREATE CAST ({s}.mood AS integer) WITH FUNCTION {s}.mood_int({s}.mood) AS IMPLICIT;
         CREATE CAST ({s}.pair AS text) WITH INOUT AS ASSIGNMENT;
         CREATE CAST ({s}.citext AS bytea) WITHOUT FUNCTION;
         COMMENT ON CAST ({s}.pair AS text) IS 'as text';
         CREATE EVENT TRIGGER m2_pgprog_evt ON ddl_command_start
             WHEN TAG IN ('CREATE TABLE', 'DROP TABLE') EXECUTE FUNCTION {s}.evt();
         ALTER EVENT TRIGGER m2_pgprog_evt DISABLE;
         COMMENT ON EVENT TRIGGER m2_pgprog_evt IS 'audit';
         CREATE PUBLICATION m2_pgprog_pub FOR TABLE {s}.t1, {s}.t2
             WITH (publish = 'insert, update', publish_via_partition_root = true);
         COMMENT ON PUBLICATION m2_pgprog_pub IS 'two tables';
         CREATE PUBLICATION m2_pgprog_all FOR ALL TABLES WITH (publish = '');
         CREATE FOREIGN DATA WRAPPER m2_pgprog_fdw OPTIONS (debug 'true');
         COMMENT ON FOREIGN DATA WRAPPER m2_pgprog_fdw IS 'plain';
         CREATE FOREIGN DATA WRAPPER m2_pgprog_pgfdw HANDLER {s}.postgres_fdw_handler
             VALIDATOR {s}.postgres_fdw_validator;
         CREATE SERVER m2_pgprog_srv TYPE 'pg' VERSION '16' FOREIGN DATA WRAPPER m2_pgprog_pgfdw
             OPTIONS (host 'db.example', dbname 'x', port '5432');
         COMMENT ON SERVER m2_pgprog_srv IS 'remote';
         CREATE USER MAPPING FOR CURRENT_USER SERVER m2_pgprog_srv
             OPTIONS (user 'remote_user', password '{FIXTURE_PASSWORD}');
         CREATE SERVER m2_pgprog_api FOREIGN DATA WRAPPER m2_pgprog_fdw
             OPTIONS (endpoint 'https://api.example', apikey 'SECRETAPI', host 'api.example',
                 key 'SECRETK');
         CREATE USER MAPPING FOR PUBLIC SERVER m2_pgprog_srv OPTIONS (user 'anon');"
    );
    if major >= 14 {
        sql.push_str(&format!(
            "CREATE FUNCTION {s}.twice(x integer) RETURNS integer LANGUAGE sql IMMUTABLE
                 RETURN x * 2;
             CREATE FUNCTION {s}.atomic_inc(x integer) RETURNS integer LANGUAGE sql
                 BEGIN ATOMIC SELECT x + 1; END;
             CREATE PROCEDURE {s}.remember(a integer) LANGUAGE sql
                 BEGIN ATOMIC INSERT INTO {s}.t2 VALUES (a); INSERT INTO {s}.t2 VALUES (a + 1); END;"
        ));
    }
    if major >= 15 {
        sql.push_str(&format!(
            "CREATE PUBLICATION m2_pgprog_filtered FOR TABLE {s}.t1 (id, a) WHERE (b > 5)
                 WITH (publish = 'insert');
             CREATE PUBLICATION m2_pgprog_schema FOR TABLES IN SCHEMA m2_pgprog_pubs;"
        ));
    }
    if major >= 18 {
        sql.push_str(&format!(
            "CREATE PUBLICATION m2_pgprog_gen FOR TABLE {s}.t2
                 WITH (publish_generated_columns = stored);"
        ));
    }
    let streaming = if major >= 14 {
        ", binary = true, streaming = on"
    } else {
        ""
    };
    sql.push_str(&format!(
        "CREATE SUBSCRIPTION m2_pgprog_sub
             CONNECTION 'host=127.0.0.1 port=5432 dbname=varve user=varve password=''{FIXTURE_PASSWORD}'' application_name=m2'
             PUBLICATION m2_pgprog_pub, m2_pgprog_all
             WITH (connect = false, slot_name = 'm2_pgprog_slot', synchronous_commit = 'local'{streaming});
         COMMENT ON SUBSCRIPTION m2_pgprog_sub IS 'sub';
         CREATE SUBSCRIPTION m2_pgprog_uri CONNECTION 'postgresql://repl:pa?SECRETQ@h:1/db'
             PUBLICATION m2_pgprog_pub WITH (connect = false, slot_name = NONE);
         CREATE SUBSCRIPTION m2_pgprog_nested
             CONNECTION 'host=h dbname=''postgresql://u:SECRETD@h/db'''
             PUBLICATION m2_pgprog_pub WITH (connect = false, slot_name = NONE);
         CREATE SUBSCRIPTION m2_pgprog_vt
             CONNECTION E'host=h port=1\\x0Bpassword=SECRETV dbname=x'
             PUBLICATION m2_pgprog_pub WITH (connect = false, slot_name = NONE);
         CREATE SUBSCRIPTION m2_pgprog_trailing CONNECTION 'postgresql://u@h/db?sslmode=require&'
             PUBLICATION m2_pgprog_pub WITH (connect = false, slot_name = NONE);
         CREATE SUBSCRIPTION \"m2_pgprog Spaced\" CONNECTION 'host=127.0.0.1 dbname=varve'
             PUBLICATION m2_pgprog_pub WITH (connect = false);"
    ));
    sql
}

/// The order generated DDL is replayed in, so dependencies exist first.
const REPLAY_ORDER: &[ObjectKind] = &[
    ObjectKind::Extension,
    ObjectKind::Collation,
    ObjectKind::EnumType,
    ObjectKind::CompositeType,
    ObjectKind::Domain,
    ObjectKind::RangeType,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Aggregate,
    ObjectKind::Operator,
    ObjectKind::Cast,
    ObjectKind::EventTrigger,
    ObjectKind::Publication,
    ObjectKind::ForeignDataWrapper,
    ObjectKind::ForeignServer,
    ObjectKind::Subscription,
];

type Key = (ObjectKind, Option<String>, String, Option<String>);

fn key(object: &ObjectRef) -> Key {
    (
        object.kind,
        object.schema.clone(),
        object.name.clone(),
        object.signature.clone(),
    )
}

fn container(database: &str, kind: ObjectKind) -> Container {
    Container {
        database: Some(database.to_owned()),
        schema: programs::SCHEMA_KINDS
            .contains(&kind)
            .then(|| SCHEMA.to_owned()),
        parent: None,
    }
}

/// Every user object of this catalog in the fixture schema and database.
async fn list_all(
    catalog: &dyn Catalog,
    session: &dyn Session,
    database: &str,
) -> TestResult<Vec<ObjectRef>> {
    let mut objects = Vec::new();
    for &kind in programs::KINDS {
        for entry in catalog
            .list(session, &container(database, kind), kind, false)
            .await?
        {
            assert!(!entry.system, "{entry:?} is listed as a user object");
            assert_eq!(entry.object.kind, kind);
            objects.push(entry.object);
        }
    }
    Ok(objects)
}

fn program(details: ObjectDetails) -> TestResult<PgProgramObject> {
    match details {
        ObjectDetails::Postgres(object) => match *object {
            PgObject::Program(object) => Ok(object),
            other => Err(format!("unexpected details {other:?}").into()),
        },
        other => Err(format!("unexpected details {other:?}").into()),
    }
}

async fn describe_all(
    catalog: &dyn Catalog,
    session: &dyn Session,
    objects: &[ObjectRef],
) -> TestResult<BTreeMap<Key, (PgProgramObject, String)>> {
    let mut out = BTreeMap::new();
    for object in objects {
        let details = program(catalog.describe(session, object).await?)?;
        let ddl = catalog.ddl(session, object).await?;
        let debug = format!("{details:?}");
        for secret in SECRETS {
            assert!(!ddl.contains(secret), "DDL of {object:?} leaks {secret}");
            assert!(
                !debug.contains(secret),
                "details of {object:?} leak {secret}"
            );
        }
        out.insert(key(object), (details, ddl));
    }
    Ok(out)
}

fn names(objects: &[ObjectRef], kind: ObjectKind) -> Vec<String> {
    objects
        .iter()
        .filter(|o| o.kind == kind)
        .map(|o| match &o.signature {
            Some(signature) if kind != ObjectKind::Cast => format!("{}({signature})", o.name),
            _ => o.name.clone(),
        })
        .collect()
}

fn get<'a>(
    details: &'a BTreeMap<Key, (PgProgramObject, String)>,
    kind: ObjectKind,
    name: &str,
    signature: Option<&str>,
) -> TestResult<&'a PgProgramObject> {
    details
        .iter()
        .find(|((k, _, n, s), _)| {
            *k == kind && n == name && signature.is_none_or(|sig| s.as_deref() == Some(sig))
        })
        .map(|(_, (object, _))| object)
        .ok_or_else(|| format!("{kind:?} {name} is missing").into())
}

fn check_source_details(
    major: u32,
    objects: &[ObjectRef],
    details: &BTreeMap<Key, (PgProgramObject, String)>,
) -> TestResult {
    let mut functions = names(objects, ObjectKind::Function);
    functions.sort();
    let mut expected = vec![
        "add(a integer, b integer)",
        "add(a text, b text)",
        "eq(integer, integer)",
        "evt()",
        "hyp_final(integer[], integer, integer)",
        "mood_int(m2_pgprog.mood)",
        "ne(integer, integer)",
        "neg(integer)",
        "os_final(integer[], text, integer)",
        "os_state(integer[], integer)",
        "series(n integer)",
        "sum_final(bigint)",
        "sum_inv(bigint, integer)",
        "sum_state(bigint, integer)",
    ];
    if major >= 14 {
        expected.extend(["atomic_inc(x integer)", "twice(x integer)"]);
    }
    expected.sort_unstable();
    assert_eq!(functions, expected, "extension functions must be hidden");
    assert_eq!(
        names(objects, ObjectKind::Aggregate),
        vec![
            "cnt()",
            "hyp(integer ORDER BY integer)",
            "maxint(integer)",
            "os_collect(text ORDER BY integer)",
            "total(integer)",
        ]
    );
    assert_eq!(
        names(objects, ObjectKind::Extension),
        vec!["citext", "postgres_fdw"],
        "plpgsql is a system extension"
    );
    assert_eq!(
        names(objects, ObjectKind::ForeignDataWrapper),
        vec!["m2_pgprog_fdw", "m2_pgprog_pgfdw"],
        "the postgres_fdw wrapper belongs to its extension"
    );
    assert_eq!(names(objects, ObjectKind::Cast).len(), 3);
    assert_eq!(
        names(objects, ObjectKind::Operator),
        vec![
            "!==(integer, integer)",
            "===(integer, integer)",
            "~~~(NONE, integer)"
        ]
    );

    let PgProgramObject::Function(add) =
        get(details, ObjectKind::Function, "add", Some("a text, b text"))?
    else {
        return Err("add is not a function".into());
    };
    assert!(add.security_definer);
    assert_eq!(add.config, vec!["search_path=pg_catalog"]);
    assert!((add.cost - 50.0).abs() < f64::EPSILON);
    assert_eq!(add.language, "plpgsql");
    let PgProgramObject::Function(add_int) = get(
        details,
        ObjectKind::Function,
        "add",
        Some("a integer, b integer"),
    )?
    else {
        return Err("add is not a function".into());
    };
    assert!(add_int.strict);
    assert_eq!(add_int.parallel, programs::PgParallel::Safe);
    assert_eq!(add_int.arguments[1].default.as_deref(), Some("1"));
    assert_eq!(add_int.meta.comment.as_deref(), Some("adds"));
    let PgProgramObject::Function(series) = get(details, ObjectKind::Function, "series", None)?
    else {
        return Err("series is not a function".into());
    };
    assert_eq!(
        series.result.as_deref(),
        Some("TABLE(i integer, sq bigint)")
    );
    assert!(series.returns_set);
    assert_eq!(series.rows, Some(10.0));
    assert_eq!(series.arguments[2].mode, PgArgumentMode::Table);
    if major >= 14 {
        let PgProgramObject::Function(twice) = get(details, ObjectKind::Function, "twice", None)?
        else {
            return Err("twice is not a function".into());
        };
        assert!(twice.sql_standard_body);
        assert!(twice.source.is_none());
        assert!(
            twice.definition.contains("RETURN (x * 2)"),
            "{}",
            twice.definition
        );
    }
    if major >= 14 {
        let PgProgramObject::Function(atomic) =
            get(details, ObjectKind::Function, "atomic_inc", None)?
        else {
            return Err("atomic_inc is not a function".into());
        };
        assert!(atomic.sql_standard_body);
        assert!(
            atomic.definition.contains("BEGIN ATOMIC"),
            "{}",
            atomic.definition
        );
        let PgProgramObject::Procedure(remember) =
            get(details, ObjectKind::Procedure, "remember", None)?
        else {
            return Err("remember is not a procedure".into());
        };
        assert!(remember.sql_standard_body);
        assert_eq!(
            names(objects, ObjectKind::Procedure),
            vec![
                "double_it(IN a integer, OUT b integer)",
                "remember(IN a integer)"
            ]
        );
    }
    let PgProgramObject::Procedure(procedure) =
        get(details, ObjectKind::Procedure, "double_it", None)?
    else {
        return Err("double_it is not a procedure".into());
    };
    let expected_mode = if major >= 14 {
        PgArgumentMode::Out
    } else {
        PgArgumentMode::InOut
    };
    assert_eq!(procedure.arguments[1].mode, expected_mode);
    assert!(procedure.result.is_none());
    let PgProgramObject::Aggregate(hyp) = get(details, ObjectKind::Aggregate, "hyp", None)? else {
        return Err("hyp is not an aggregate".into());
    };
    assert_eq!(hyp.aggregate_kind, PgAggregateKind::Hypothetical);
    assert_eq!(hyp.direct_argument_count, 1);
    let PgProgramObject::Aggregate(total) = get(details, ObjectKind::Aggregate, "total", None)?
    else {
        return Err("total is not an aggregate".into());
    };
    assert_eq!(
        total.moving_inverse_function.as_deref(),
        Some("m2_pgprog.sum_inv")
    );
    let PgProgramObject::Aggregate(maxint) = get(details, ObjectKind::Aggregate, "maxint", None)?
    else {
        return Err("maxint is not an aggregate".into());
    };
    assert_eq!(maxint.sort_operator.as_deref(), Some("pg_catalog.>"));
    let PgProgramObject::EnumType(mood) = get(details, ObjectKind::EnumType, "mood", None)? else {
        return Err("mood is not an enum".into());
    };
    assert_eq!(mood.labels, vec!["sad", "it's ok", "happy"]);
    let PgProgramObject::Domain(posint) = get(details, ObjectKind::Domain, "posint", None)? else {
        return Err("posint is not a domain".into());
    };
    assert!(posint.not_null);
    assert!(
        posint
            .constraints
            .iter()
            .any(|c| c.name == "small" && !c.validated)
    );
    let PgProgramObject::RangeType(range) =
        get(details, ObjectKind::RangeType, "floatrange", None)?
    else {
        return Err("floatrange is not a range".into());
    };
    assert_eq!(range.subtype, "double precision");
    assert_eq!(
        range.multirange.as_deref(),
        (major >= 14).then_some("m2_pgprog.floatmulti")
    );
    let PgProgramObject::Extension(citext) = get(details, ObjectKind::Extension, "citext", None)?
    else {
        return Err("citext is not an extension".into());
    };
    assert_eq!(citext.schema, SCHEMA);
    assert!(citext.members.iter().any(|m| m.starts_with("type ")));
    assert!(citext.default_version.is_some());
    let PgProgramObject::EventTrigger(trigger) =
        get(details, ObjectKind::EventTrigger, "m2_pgprog_evt", None)?
    else {
        return Err("m2_pgprog_evt is not an event trigger".into());
    };
    assert_eq!(trigger.state, PgTriggerState::Disabled);
    assert_eq!(trigger.tags, vec!["CREATE TABLE", "DROP TABLE"]);
    assert_eq!(trigger.function, "m2_pgprog.evt");
    let PgProgramObject::Subscription(subscription) =
        get(details, ObjectKind::Subscription, "m2_pgprog_sub", None)?
    else {
        return Err("m2_pgprog_sub is not a subscription".into());
    };
    assert!(subscription.connection_visible);
    let connection = subscription.connection.as_deref().unwrap_or_default();
    assert!(connection.contains("host=127.0.0.1"), "{connection}");
    assert!(!connection.contains("password"), "{connection}");
    assert_eq!(subscription.slot_name.as_deref(), Some("m2_pgprog_slot"));
    assert_eq!(subscription.binary, (major >= 14).then_some(true));
    let PgProgramObject::ForeignServer(server) =
        get(details, ObjectKind::ForeignServer, "m2_pgprog_srv", None)?
    else {
        return Err("m2_pgprog_srv is not a server".into());
    };
    assert_eq!(server.user_mappings.len(), 2);
    let mapping_options: Vec<&programs::PgOption> = server
        .user_mappings
        .iter()
        .flat_map(|m| &m.options)
        .collect();
    assert!(mapping_options.iter().any(|o| o.name == "password"));
    assert!(
        mapping_options.iter().all(|o| o.value.is_none()),
        "user mapping values are never returned: {mapping_options:?}"
    );
    assert!(server.options.iter().all(|o| o.value.is_some()));
    let PgProgramObject::ForeignServer(api) =
        get(details, ObjectKind::ForeignServer, "m2_pgprog_api", None)?
    else {
        return Err("m2_pgprog_api is not a server".into());
    };
    let shown: Vec<(&str, Option<&str>)> = api
        .options
        .iter()
        .map(|o| (o.name.as_str(), o.value.as_deref()))
        .collect();
    assert_eq!(
        shown,
        vec![
            ("endpoint", None),
            ("apikey", None),
            ("host", None),
            ("key", None)
        ],
        "a wrapper without a known implementation shows no values"
    );
    let subscription_connection = |name: &str| -> TestResult<Option<String>> {
        match get(details, ObjectKind::Subscription, name, None)? {
            PgProgramObject::Subscription(s) => Ok(s.connection.clone()),
            _ => Err(format!("{name} is not a subscription").into()),
        }
    };
    assert_eq!(
        subscription_connection("m2_pgprog_uri")?.as_deref(),
        Some("user=repl host=h port=1 dbname=db")
    );
    assert_eq!(
        subscription_connection("m2_pgprog_nested")?.as_deref(),
        Some("host=h dbname='user=u host=h dbname=db'")
    );
    assert_eq!(
        subscription_connection("m2_pgprog_vt")?.as_deref(),
        Some("host=h port=1 dbname=x"),
        "a vertical tab separates keywords as in libpq"
    );
    assert_eq!(
        subscription_connection("m2_pgprog_trailing")?.as_deref(),
        Some("user=u host=h dbname=db sslmode=require")
    );
    let PgProgramObject::Subscription(spaced) =
        get(details, ObjectKind::Subscription, "m2_pgprog Spaced", None)?
    else {
        return Err("the spaced subscription is missing".into());
    };
    assert_eq!(spaced.slot_name.as_deref(), Some("m2_pgprog Spaced"));
    let PgProgramObject::Collation(ci) = get(details, ObjectKind::Collation, "ci", None)? else {
        return Err("ci is not a collation".into());
    };
    assert!(!ci.deterministic);
    assert_eq!(ci.locale.as_deref(), Some("und-u-ks-level2"));
    if major >= 15 {
        let PgProgramObject::Publication(filtered) =
            get(details, ObjectKind::Publication, "m2_pgprog_filtered", None)?
        else {
            return Err("m2_pgprog_filtered is not a publication".into());
        };
        assert_eq!(filtered.tables[0].columns, vec!["id", "a"]);
        assert_eq!(filtered.tables[0].row_filter.as_deref(), Some("(b > 5)"));
        let PgProgramObject::Publication(schema) =
            get(details, ObjectKind::Publication, "m2_pgprog_schema", None)?
        else {
            return Err("m2_pgprog_schema is not a publication".into());
        };
        assert_eq!(schema.schemas, vec!["m2_pgprog_pubs"]);
    }
    Ok(())
}

async fn check_counts(
    catalog: &dyn Catalog,
    session: &dyn Session,
    database: &str,
    objects: &[ObjectRef],
) -> TestResult {
    for scope in [Some(SCHEMA.to_owned()), None] {
        let container = Container {
            database: Some(database.to_owned()),
            schema: scope.clone(),
            parent: None,
        };
        let counts = programs::counts(session, session.server_info(), &container, false).await?;
        for count in counts {
            let listed = objects.iter().filter(|o| o.kind == count.kind).count();
            assert_eq!(count.count, listed as u64, "{:?} in {scope:?}", count.kind);
        }
        let kinds = catalog.kinds(session, &container, false).await?;
        assert!(kinds.iter().all(|k| k.count > 0));
    }
    let with_system = Container {
        database: Some(database.to_owned()),
        schema: Some(SCHEMA.to_owned()),
        parent: None,
    };
    let all = catalog
        .list(session, &with_system, ObjectKind::Function, true)
        .await?;
    let citext = all
        .iter()
        .find(|f| f.object.name == "citext_eq")
        .ok_or("citext_eq is not listed with system objects")?;
    assert!(citext.system);
    let described = program(catalog.describe(session, &citext.object).await?)?;
    let PgProgramObject::Function(function) = described else {
        return Err("citext_eq is not a function".into());
    };
    assert_eq!(function.meta.extension.as_deref(), Some("citext"));
    assert!(function.meta.system);
    Ok(())
}

async fn round_trip(major: u32, port: u16) -> TestResult {
    let source_db = "m2_pgprog_rt_src";
    let copy_db = "m2_pgprog_rt_dst";
    drop_database(port, copy_db).await?;
    let source = fresh_database(port, source_db).await?;
    assert_eq!(source.server_info().major, major);
    exec(source.as_ref(), &prerequisites(major)).await?;
    exec(source.as_ref(), &fixtures(major)).await?;
    let catalog = varve_catalog::catalog_for(source.server_info())?;
    // Names must not depend on the session's search_path, and the catalog
    // must leave the session's own value alone.
    exec(
        source.as_ref(),
        &format!("SET search_path = {SCHEMA}, public"),
    )
    .await?;

    let objects = list_all(catalog.as_ref(), source.as_ref(), source_db).await?;
    let details = describe_all(catalog.as_ref(), source.as_ref(), &objects).await?;
    check_source_details(major, &objects, &details)?;
    check_counts(catalog.as_ref(), source.as_ref(), source_db, &objects).await?;
    assert_eq!(
        texts(source.as_ref(), "SHOW search_path").await?,
        vec![format!("{SCHEMA}, public")]
    );

    let copy = fresh_database(port, copy_db).await?;
    exec(copy.as_ref(), &prerequisites(major)).await?;
    for &kind in REPLAY_ORDER {
        for ((k, _, name, _), (_, ddl)) in &details {
            if *k == kind {
                exec(copy.as_ref(), ddl)
                    .await
                    .map_err(|e| format!("DDL of {kind:?} {name} failed: {e}\n{ddl}"))?;
            }
        }
    }

    let copied = list_all(catalog.as_ref(), copy.as_ref(), copy_db).await?;
    let source_keys: Vec<Key> = objects.iter().map(key).collect();
    let copy_keys: Vec<Key> = copied.iter().map(key).collect();
    assert_eq!(copy_keys, source_keys, "the copy lists other objects");
    let copy_details = describe_all(catalog.as_ref(), copy.as_ref(), &copied).await?;
    for (key, (object, ddl)) in &details {
        let (copied_object, copied_ddl) = copy_details
            .get(key)
            .ok_or_else(|| format!("{key:?} is missing in the copy"))?;
        assert_eq!(
            copied_object, object,
            "{key:?} differs after the round trip"
        );
        assert_eq!(
            copied_ddl, ddl,
            "DDL of {key:?} differs after the round trip"
        );
    }

    source.close().await?;
    copy.close().await?;
    drop_database(port, source_db).await?;
    drop_database(port, copy_db).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn round_trips_every_kind() -> TestResult {
    for (major, port) in servers() {
        round_trip(major, port).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn subscriptions_without_privileges_hide_the_connection() -> TestResult {
    const ROLE: &str = "m2_pgprog_reader";
    for (_, port) in servers() {
        let database = "m2_pgprog_priv";
        let session = fresh_database(port, database).await?;
        exec(
            session.as_ref(),
            &format!(
                "DROP ROLE IF EXISTS {ROLE};
                 CREATE ROLE {ROLE} NOLOGIN;
                 CREATE SUBSCRIPTION m2_pgprog_hidden
                     CONNECTION 'host=127.0.0.1 dbname=varve password={FIXTURE_PASSWORD_UNQUOTED}'
                     PUBLICATION p WITH (connect = false, slot_name = NONE);",
                FIXTURE_PASSWORD_UNQUOTED = "fixturepw"
            ),
        )
        .await?;
        let catalog = varve_catalog::catalog_for(session.server_info())?;
        let container = Container {
            database: Some(database.to_owned()),
            schema: None,
            parent: None,
        };
        let object = ObjectRef {
            kind: ObjectKind::Subscription,
            database: Some(database.to_owned()),
            schema: None,
            parent: None,
            name: "m2_pgprog_hidden".into(),
            signature: None,
        };
        exec(session.as_ref(), &format!("SET ROLE {ROLE}")).await?;
        let listed = catalog
            .list(
                session.as_ref(),
                &container,
                ObjectKind::Subscription,
                false,
            )
            .await?;
        // Every column but subconninfo is readable, so the list works.
        assert_eq!(listed.len(), 1, "{listed:?}");
        catalog.kinds(session.as_ref(), &container, false).await?;
        {
            let PgProgramObject::Subscription(details) =
                program(catalog.describe(session.as_ref(), &object).await?)?
            else {
                return Err("not a subscription".into());
            };
            assert!(!details.connection_visible);
            assert!(details.connection.is_none());
            assert!(matches!(
                catalog.ddl(session.as_ref(), &object).await,
                Err(CatalogError::Unsupported { .. })
            ));
        }
        exec(session.as_ref(), "RESET ROLE").await?;
        let PgProgramObject::Subscription(details) =
            program(catalog.describe(session.as_ref(), &object).await?)?
        else {
            return Err("not a subscription".into());
        };
        assert!(details.connection_visible);
        assert_eq!(
            details.connection.as_deref(),
            Some("host=127.0.0.1 dbname=varve")
        );
        session.close().await?;
        drop_database(port, database).await?;
        let admin = connect(port, &admin_database()).await?;
        exec(admin.as_ref(), &format!("DROP ROLE IF EXISTS {ROLE}")).await?;
        admin.close().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn capabilities_follow_the_server_version() -> TestResult {
    for (major, port) in servers() {
        let session = connect(port, &admin_database()).await?;
        let catalog = varve_catalog::catalog_for(session.server_info())?;
        let capabilities = catalog.capabilities();
        for kind in programs::KINDS {
            assert!(capabilities.kinds.contains(kind), "{kind:?}");
        }
        assert!(capabilities.editable.contains(&ObjectKind::Extension));
        assert!(!capabilities.editable.contains(&ObjectKind::Cast));
        let has = |name: &str| capabilities.features.iter().any(|f| f == name);
        assert_eq!(has("sqlStandardFunctionBody"), major >= 14);
        assert_eq!(has("publicationSchemas"), major >= 15);
        assert_eq!(has("publicationGeneratedColumns"), major >= 18);
        let missing = ObjectRef {
            kind: ObjectKind::Function,
            database: None,
            schema: Some("pg_catalog".into()),
            parent: None,
            name: "m2_pgprog_no_such_function".into(),
            signature: Some(String::new()),
        };
        assert!(matches!(
            catalog.describe(session.as_ref(), &missing).await,
            Err(CatalogError::NotFound { .. })
        ));
        session.close().await?;
    }
    Ok(())
}

/// The pinned `search_path` must never reach the session: not when the
/// caller drops the future half way (a timeout), and not inside the user's
/// transaction, where the catalog refuses instead.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn search_path_survives_dropped_calls_and_transactions() -> TestResult {
    for (_, port) in servers() {
        let session = connect(port, &admin_database()).await?;
        exec(session.as_ref(), "SET search_path = public").await?;
        let catalog = varve_catalog::catalog_for(session.server_info())?;
        let object = ObjectRef {
            kind: ObjectKind::Function,
            database: Some(admin_database()),
            schema: Some("pg_catalog".into()),
            parent: None,
            name: "lower".into(),
            signature: Some("text".into()),
        };
        for micros in [0, 20, 100, 300, 1_000, 3_000, 10_000] {
            let dropped = tokio::time::timeout(
                Duration::from_micros(micros),
                catalog.describe(session.as_ref(), &object),
            )
            .await;
            if let Ok(result) = dropped {
                result?;
            }
            assert_eq!(
                texts(session.as_ref(), "SHOW search_path").await?,
                vec!["public"],
                "after dropping the call at {micros}us"
            );
        }
        let PgProgramObject::Function(lower) =
            program(catalog.describe(session.as_ref(), &object).await?)?
        else {
            return Err("lower is not a function".into());
        };
        assert!(lower.meta.system);

        exec(session.as_ref(), "BEGIN").await?;
        let refused = |result: Result<_, CatalogError>| {
            matches!(result, Err(CatalogError::Invalid { message })
                if message == "the catalog cannot be read inside an open transaction: \
                               finish the transaction first")
        };
        assert!(refused(
            catalog
                .describe(session.as_ref(), &object)
                .await
                .map(|_| ())
        ));
        assert!(refused(
            catalog.ddl(session.as_ref(), &object).await.map(|_| ())
        ));
        let schema = Container {
            database: Some(admin_database()),
            schema: Some("pg_catalog".into()),
            parent: None,
        };
        assert!(refused(
            catalog
                .list(session.as_ref(), &schema, ObjectKind::Function, true)
                .await
                .map(|_| ())
        ));
        assert!(refused(
            programs::counts(session.as_ref(), session.server_info(), &schema, true)
                .await
                .map(|_| ())
        ));
        assert_eq!(
            texts(session.as_ref(), "SHOW search_path").await?,
            vec!["public"]
        );
        exec(session.as_ref(), "ROLLBACK").await?;

        // In a transaction that already failed the refusal is the same.
        exec(session.as_ref(), "BEGIN").await?;
        assert!(exec(session.as_ref(), "SELECT 1 / 0").await.is_err());
        assert!(refused(
            catalog
                .describe(session.as_ref(), &object)
                .await
                .map(|_| ())
        ));
        assert!(refused(
            catalog
                .list(session.as_ref(), &schema, ObjectKind::Function, true)
                .await
                .map(|_| ())
        ));
        assert!(refused(
            catalog
                .list(session.as_ref(), &schema, ObjectKind::Table, true)
                .await
                .map(|_| ())
        ));
        exec(session.as_ref(), "ROLLBACK").await?;

        let server_wide = Container::default();
        assert!(
            programs::counts(session.as_ref(), session.server_info(), &server_wide, true)
                .await?
                .is_empty()
        );
        for kind in programs::DATABASE_KINDS {
            assert!(
                catalog
                    .list(session.as_ref(), &server_wide, *kind, true)
                    .await?
                    .is_empty(),
                "{kind:?} listed without a database"
            );
        }
        session.close().await?;
    }
    Ok(())
}

/// Catalog statements wait at most `LOCK_TIMEOUT` for a lock another
/// session holds, and report it as a typed error instead of hanging.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn locked_objects_never_hang_the_catalog() -> TestResult {
    let database = "m2_pgprog_lock";
    for (major, port) in servers() {
        let reader = fresh_database(port, database).await?;
        let mut setup = format!(
            "CREATE SCHEMA {SCHEMA};
             CREATE TABLE {SCHEMA}.t1 (id integer PRIMARY KEY, b integer);
             CREATE FUNCTION {SCHEMA}.f() RETURNS integer LANGUAGE sql AS 'SELECT 1';
             CREATE TYPE {SCHEMA}.mood AS ENUM ('a', 'b');"
        );
        if major >= 15 {
            setup.push_str(&format!(
                "CREATE PUBLICATION m2_pgprog_locked FOR TABLE {SCHEMA}.t1 WHERE (b > 1)
                     WITH (publish = 'insert');"
            ));
        }
        exec(reader.as_ref(), &setup).await?;
        let holder = connect(port, database).await?;
        exec(holder.as_ref(), "BEGIN").await?;
        exec(
            holder.as_ref(),
            &format!(
                "LOCK TABLE {SCHEMA}.t1 IN ACCESS EXCLUSIVE MODE;
                 ALTER FUNCTION {SCHEMA}.f() COST 7;
                 ALTER TYPE {SCHEMA}.mood ADD VALUE 'c';"
            ),
        )
        .await?;

        let catalog = varve_catalog::catalog_for(reader.server_info())?;
        let reference = |kind, name: &str, signature: Option<&str>| ObjectRef {
            kind,
            database: Some(database.to_owned()),
            schema: (kind != ObjectKind::Publication).then(|| SCHEMA.to_owned()),
            parent: None,
            name: name.to_owned(),
            signature: signature.map(str::to_owned),
        };
        let limit = Duration::from_secs(5);
        for object in [
            reference(ObjectKind::Function, "f", Some("")),
            reference(ObjectKind::EnumType, "mood", None),
        ] {
            let details = tokio::time::timeout(limit, catalog.describe(reader.as_ref(), &object))
                .await
                .map_err(|_| format!("describe of {object:?} hangs behind a lock"))??;
            let ddl = tokio::time::timeout(limit, catalog.ddl(reader.as_ref(), &object))
                .await
                .map_err(|_| format!("ddl of {object:?} hangs behind a lock"))??;
            // The holder's uncommitted change is not visible.
            if let PgProgramObject::EnumType(mood) = program(details)? {
                assert_eq!(mood.labels, vec!["a", "b"]);
            }
            assert!(!ddl.contains("COST 7"), "{ddl}");
        }

        let started = std::time::Instant::now();
        let locked = tokio::time::timeout(
            limit,
            programs::fetch_pinned(
                reader.as_ref(),
                // Locks taken while the statement runs (sizes, deparsing)
                // are covered; catalog SQL never names user tables directly,
                // which would lock them while parsing, before the pin.
                &[(
                    format!("SELECT pg_catalog.pg_relation_size('{SCHEMA}.t1'::regclass) AS n"),
                    "q.n".to_owned(),
                )],
            ),
        )
        .await
        .map_err(|_| "a locked table hangs the catalog")?;
        assert!(
            matches!(
                &locked,
                Err(CatalogError::Driver { error })
                    if error.kind == varve_driver::ErrorKind::Timeout
                        && error.code.as_deref() == Some("55P03")
                        && error.message.contains("locked")
            ),
            "{locked:?}"
        );
        assert!(started.elapsed() >= Duration::from_millis(1_500));
        if major >= 15 {
            let publication = reference(ObjectKind::Publication, "m2_pgprog_locked", None);
            let result =
                tokio::time::timeout(limit, catalog.describe(reader.as_ref(), &publication))
                    .await
                    .map_err(|_| "describe of a publication hangs behind a lock")?;
            assert!(
                matches!(
                    &result,
                    Err(CatalogError::Driver { error })
                        if error.kind == varve_driver::ErrorKind::Timeout
                            && error.code.as_deref() == Some("55P03")
                            && error.message.contains("locked")
                ),
                "{result:?}"
            );
        }
        // The session's own lock_timeout is untouched.
        assert_eq!(
            texts(reader.as_ref(), "SHOW lock_timeout").await?,
            vec!["0"]
        );

        exec(holder.as_ref(), "ROLLBACK").await?;
        holder.close().await?;
        reader.close().await?;
        drop_database(port, database).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Name hijacking

/// The sequence every planted object advances before it raises; sequence
/// advances survive the error and the rollback that follows it.
const HIJACK_COUNTER: &str = "public.m2_pgprog_hijacked";

/// Plants, in `public`, an object for every function, aggregate, operator,
/// base type and relation of `pg_catalog` that takes its place whenever a
/// statement names it without `pg_catalog.`: functions, aggregates and
/// operators with the identical signature (they win with `public` before
/// `pg_catalog` in `search_path`), domains named after the base types and
/// views named after the catalog relations. With the default
/// `search_path` (`pg_catalog` implicitly first) better matching overloads
/// win instead; the classic ones are planted too. Every planted object
/// advances [`HIJACK_COUNTER`] and raises `HIJACKED ...`. How many of each
/// were planted is written to `public.m2_pgprog_planted`.
fn plant_hijacks() -> String {
    format!(
        r#"CREATE SEQUENCE {HIJACK_COUNTER};
CREATE FUNCTION public.m2_pgprog_trap() RETURNS pg_catalog.bool LANGUAGE plpgsql AS $t$
BEGIN
    PERFORM pg_catalog.nextval('{HIJACK_COUNTER}'::pg_catalog.regclass);
    RAISE EXCEPTION USING MESSAGE = 'HIJACKED relation or type';
END $t$;
CREATE TABLE public.m2_pgprog_planted (what pg_catalog.text, n pg_catalog.int4);
DO $plant$
DECLARE
    item record;
    planted pg_catalog.int4;
    body pg_catalog.text;
    trap pg_catalog.text;
BEGIN
    -- Functions: identical signatures.
    planted := 0;
    FOR item IN
        SELECT p.oid, p.proname, pg_catalog.pg_get_function_arguments(p.oid) AS args,
               pg_catalog.pg_get_function_result(p.oid) AS result
        FROM pg_catalog.pg_proc p
        WHERE p.pronamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
          AND p.prokind OPERATOR(pg_catalog.=) 'f'
    LOOP
        body := pg_catalog.format(
            'BEGIN PERFORM pg_catalog.nextval(%L::pg_catalog.regclass); '
            'RAISE EXCEPTION USING MESSAGE = %L; END',
            '{HIJACK_COUNTER}', pg_catalog.concat('HIJACKED function ', item.proname));
        BEGIN
            EXECUTE pg_catalog.format('CREATE FUNCTION public.%I(%s) RETURNS %s LANGUAGE plpgsql AS %L',
                item.proname, item.args, item.result, body);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN OTHERS THEN NULL;
        END;
    END LOOP;
    INSERT INTO public.m2_pgprog_planted VALUES ('functions', planted);

    -- Aggregates: identical signatures, with a raising state function.
    planted := 0;
    FOR item IN
        SELECT p.oid, p.proname, p.pronargs,
               pg_catalog.pg_get_function_identity_arguments(p.oid) AS args,
               (SELECT pg_catalog.string_agg(pg_catalog.format_type(t.oid, NULL), ', ' ORDER BY t.ord)
                FROM pg_catalog.unnest(p.proargtypes::pg_catalog.oid[]) WITH ORDINALITY t(oid, ord)) AS types
        FROM pg_catalog.pg_proc p
        JOIN pg_catalog.pg_aggregate a ON a.aggfnoid OPERATOR(pg_catalog.=) p.oid
        WHERE p.pronamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
          AND a.aggkind OPERATOR(pg_catalog.=) 'n'
    LOOP
        body := pg_catalog.format(
            'BEGIN PERFORM pg_catalog.nextval(%L::pg_catalog.regclass); '
            'RAISE EXCEPTION USING MESSAGE = %L; END',
            '{HIJACK_COUNTER}', pg_catalog.concat('HIJACKED aggregate ', item.proname));
        BEGIN
            EXECUTE pg_catalog.format(
                'CREATE FUNCTION public.m2_pgprog_agg_%s(pg_catalog.int8%s) RETURNS pg_catalog.int8 '
                'LANGUAGE plpgsql AS %L',
                item.oid, CASE WHEN item.pronargs OPERATOR(pg_catalog.=) 0 THEN ''
                               ELSE pg_catalog.concat(', ', item.types) END, body);
            EXECUTE pg_catalog.format(
                'CREATE AGGREGATE public.%I(%s) (SFUNC = public.m2_pgprog_agg_%s, STYPE = pg_catalog.int8)',
                item.proname, CASE WHEN item.pronargs OPERATOR(pg_catalog.=) 0 THEN '*' ELSE item.types END,
                item.oid);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN OTHERS THEN NULL;
        END;
    END LOOP;
    INSERT INTO public.m2_pgprog_planted VALUES ('aggregates', planted);

    -- Operators: identical signatures.
    planted := 0;
    FOR item IN
        SELECT o.oid, o.oprname, o.oprleft, o.oprright,
               pg_catalog.format_type(o.oprleft, NULL) AS lefttype,
               pg_catalog.format_type(o.oprright, NULL) AS righttype,
               pg_catalog.format_type(o.oprresult, NULL) AS result
        FROM pg_catalog.pg_operator o
        WHERE o.oprnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
          AND o.oprcode OPERATOR(pg_catalog.<>) 0::pg_catalog.regproc
    LOOP
        body := pg_catalog.format(
            'BEGIN PERFORM pg_catalog.nextval(%L::pg_catalog.regclass); '
            'RAISE EXCEPTION USING MESSAGE = %L; END',
            '{HIJACK_COUNTER}', pg_catalog.concat('HIJACKED operator ', item.oprname));
        BEGIN
            EXECUTE pg_catalog.format('CREATE FUNCTION public.m2_pgprog_op_%s(%s) RETURNS %s LANGUAGE plpgsql AS %L',
                item.oid,
                pg_catalog.concat_ws(', ',
                    CASE WHEN item.oprleft OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN item.lefttype END,
                    CASE WHEN item.oprright OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN item.righttype END),
                item.result, body);
            EXECUTE pg_catalog.format('CREATE OPERATOR public.%s (FUNCTION = public.m2_pgprog_op_%s%s%s)',
                item.oprname, item.oid,
                CASE WHEN item.oprleft OPERATOR(pg_catalog.<>) 0::pg_catalog.oid
                     THEN pg_catalog.concat(', LEFTARG = ', item.lefttype) ELSE '' END,
                CASE WHEN item.oprright OPERATOR(pg_catalog.<>) 0::pg_catalog.oid
                     THEN pg_catalog.concat(', RIGHTARG = ', item.righttype) ELSE '' END);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN OTHERS THEN NULL;
        END;
    END LOOP;
    INSERT INTO public.m2_pgprog_planted VALUES ('operators', planted);

    -- Better matches for the default search_path, where pg_catalog comes
    -- first and wins every tie.
    planted := 0;
    FOR item IN SELECT * FROM (VALUES
        ('FUNCTION public.obj_description(pg_catalog.oid, pg_catalog.text) RETURNS pg_catalog.text'),
        ('FUNCTION public.shobj_description(pg_catalog.oid, pg_catalog.text) RETURNS pg_catalog.text'),
        ('FUNCTION public.format_type(pg_catalog.oid, pg_catalog.text) RETURNS pg_catalog.text'),
        ('FUNCTION public.pg_get_expr(pg_catalog.text, pg_catalog.oid) RETURNS pg_catalog.text'),
        ('FUNCTION public.pg_get_userbyid(pg_catalog.int4) RETURNS pg_catalog.name'),
        ('FUNCTION public.quote_ident(pg_catalog.name) RETURNS pg_catalog.text'),
        ('FUNCTION public.set_config(pg_catalog.text, pg_catalog.text, pg_catalog.text) RETURNS pg_catalog.text'),
        ('FUNCTION public.m2_pgprog_cat(pg_catalog.name, pg_catalog.text) RETURNS pg_catalog.text'),
        ('FUNCTION public.m2_pgprog_eq(pg_catalog."char", pg_catalog.text) RETURNS pg_catalog.bool'),
        ('FUNCTION public.m2_pgprog_like(pg_catalog.name, pg_catalog.text) RETURNS pg_catalog.bool')
    ) AS v(head)
    LOOP
        body := 'BEGIN PERFORM pg_catalog.nextval(''public.m2_pgprog_hijacked''::pg_catalog.regclass); '
                'RAISE EXCEPTION USING MESSAGE = ''HIJACKED better match''; END';
        EXECUTE pg_catalog.format('CREATE %s LANGUAGE plpgsql AS %L', item.head, body);
        planted := planted OPERATOR(pg_catalog.+) 1;
    END LOOP;
    -- The same signature may already be planted above (PostgreSQL 12 and
    -- newer have ~~(name, text) themselves).
    FOR item IN SELECT * FROM (VALUES
        ('||', 'public.m2_pgprog_cat', 'pg_catalog.name', 'pg_catalog.text'),
        ('=', 'public.m2_pgprog_eq', 'pg_catalog."char"', 'pg_catalog.text'),
        ('~~', 'public.m2_pgprog_like', 'pg_catalog.name', 'pg_catalog.text')
    ) AS v(op, fn, lefttype, righttype)
    LOOP
        BEGIN
            EXECUTE pg_catalog.format('CREATE OPERATOR public.%s (FUNCTION = %s, LEFTARG = %s, RIGHTARG = %s)',
                item.op, item.fn, item.lefttype, item.righttype);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN duplicate_function THEN NULL;
        END;
    END LOOP;
    INSERT INTO public.m2_pgprog_planted VALUES ('better matches', planted);

    -- Catalog relations: views of the same name that raise when read.
    planted := 0;
    FOR item IN
        SELECT c.relname FROM pg_catalog.pg_class c
        WHERE c.relnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
          AND c.relkind OPERATOR(pg_catalog.=) ANY (ARRAY['r', 'v']::pg_catalog."char"[])
    LOOP
        BEGIN
            EXECUTE pg_catalog.format(
                'CREATE VIEW public.%I AS SELECT * FROM pg_catalog.%I WHERE public.m2_pgprog_trap()',
                item.relname, item.relname);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN OTHERS THEN NULL;
        END;
    END LOOP;
    INSERT INTO public.m2_pgprog_planted VALUES ('relations', planted);

    -- Base, range, multirange and composite types: domains of the same name
    -- that raise on every value (after the views, which own the row type
    -- names of the catalog relations).
    planted := 0;
    trap := 'CHECK (public.m2_pgprog_trap())';
    FOR item IN
        SELECT t.typname FROM pg_catalog.pg_type t
        WHERE t.typnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
          AND t.typtype OPERATOR(pg_catalog.=) ANY (ARRAY['b', 'r', 'm', 'c']::pg_catalog."char"[])
          AND pg_catalog.left(t.typname, 1) OPERATOR(pg_catalog.<>) '_'
    LOOP
        BEGIN
            EXECUTE pg_catalog.format('CREATE DOMAIN public.%I AS pg_catalog.%I %s',
                item.typname, item.typname, trap);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN OTHERS THEN NULL;
        END;
    END LOOP;
    INSERT INTO public.m2_pgprog_planted VALUES ('types', planted);
END
$plant$;"#
    )
}

/// Relations and their children: what the relations catalog lists and
/// describes, with expressions everywhere a deparser is involved.
fn relation_fixtures() -> String {
    let s = SCHEMA;
    format!(
        "CREATE TABLE {s}.rel (
             id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             a integer DEFAULT 1 + 2 CONSTRAINT positive CHECK (a > 0),
             b text COLLATE \"C\" NOT NULL,
             g integer GENERATED ALWAYS AS (a * 2) STORED,
             r integer REFERENCES {s}.t2 (id) ON DELETE SET NULL,
             span int4range,
             UNIQUE (a, b),
             EXCLUDE USING gist (span WITH &&)
         );
         COMMENT ON TABLE {s}.rel IS 'relation';
         COMMENT ON COLUMN {s}.rel.a IS 'column';
         CREATE INDEX rel_expr ON {s}.rel (lower(b) DESC NULLS LAST) INCLUDE (a) WHERE a > 5;
         COMMENT ON INDEX {s}.rel_expr IS 'index';
         CREATE FUNCTION {s}.trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
         CREATE TRIGGER rel_trg BEFORE UPDATE OF a ON {s}.rel FOR EACH ROW
             WHEN (OLD.a IS DISTINCT FROM NEW.a) EXECUTE FUNCTION {s}.trg();
         COMMENT ON TRIGGER rel_trg ON {s}.rel IS 'trigger';
         CREATE RULE rel_rule AS ON DELETE TO {s}.rel WHERE OLD.a > 100 DO INSTEAD NOTHING;
         ALTER TABLE {s}.rel ENABLE ROW LEVEL SECURITY;
         CREATE POLICY rel_pol ON {s}.rel AS RESTRICTIVE USING (a > 0) WITH CHECK (a < 1000);
         CREATE VIEW {s}.v AS SELECT id, a FROM {s}.rel WHERE a > 1;
         CREATE MATERIALIZED VIEW {s}.mv AS SELECT id FROM {s}.rel;
         CREATE SEQUENCE {s}.seq START 5 OWNED BY {s}.rel.a;
         CREATE TABLE {s}.parts (k integer, d date) PARTITION BY RANGE (d);
         CREATE TABLE {s}.parts_2020 PARTITION OF {s}.parts
             FOR VALUES FROM ('2020-01-01') TO ('2021-01-01');
         CREATE TABLE {s}.parts_default PARTITION OF {s}.parts DEFAULT;
         CREATE INDEX ON {s}.parts (k);
         CREATE FOREIGN TABLE {s}.ft (id integer OPTIONS (column_name 'id'))
             SERVER m2_pgprog_srv OPTIONS (table_name 'remote');"
    )
}

/// Drops the hijack test database, first taking out the planted objects and
/// the `search_path` that makes them win (the helpers' own SQL is not
/// written against planted objects).
async fn drop_hijack_database(port: u16, database: &str) -> TestResult {
    let admin = connect(port, &admin_database()).await?;
    let exists = texts(
        admin.as_ref(),
        &format!("SELECT datname FROM pg_catalog.pg_database WHERE datname OPERATOR(pg_catalog.=) '{database}'"),
    )
    .await?;
    if !exists.is_empty() {
        exec(
            admin.as_ref(),
            &format!("ALTER DATABASE {database} RESET search_path"),
        )
        .await?;
        let session = connect(port, database).await?;
        exec(session.as_ref(), "DROP SCHEMA IF EXISTS public CASCADE").await?;
        session.close().await?;
    }
    admin.close().await?;
    drop_database(port, database).await
}

/// Records a failed call, or a result that shows a planted object ran.
fn note(failures: &mut Vec<String>, what: String, result: Result<String, CatalogError>) {
    match result {
        Ok(text) if text.contains("HIJACKED") => failures.push(format!("{what}: {text}")),
        Ok(_) => {}
        Err(error) => failures.push(format!("{what}: {error}")),
    }
}

/// Calls every catalog entry point on everything below `database` that the
/// fixtures created: kinds and lists on every level (with and without
/// system objects), and `describe` and `ddl` of every user object, down to
/// the children of relations. Returns the failures, the number of calls
/// and what `describe` and `ddl` returned for the objects of [`SCHEMA`]
/// (keyed by call), so runs under different `search_path` settings can be
/// compared: a name lookup that went to a planted object instead of the
/// catalog (`'pg_class'::regclass` reaching a view `public.pg_class`) runs
/// no planted code, but changes what is read.
async fn walk(
    catalog: &dyn Catalog,
    session: &dyn Session,
    database: &str,
) -> (Vec<String>, usize, BTreeMap<String, String>) {
    let mut failures = Vec::new();
    let mut calls = 0usize;
    let mut outputs = BTreeMap::new();
    calls += 1;
    note(
        &mut failures,
        "databases".into(),
        catalog
            .databases(session)
            .await
            .map(|list| format!("{list:?}")),
    );
    calls += 1;
    note(
        &mut failures,
        "schemas".into(),
        catalog
            .schemas(session, database)
            .await
            .map(|list| format!("{list:?}")),
    );
    let mut containers = vec![
        Container::default(),
        Container {
            database: Some(database.to_owned()),
            schema: None,
            parent: None,
        },
        Container {
            database: Some(database.to_owned()),
            schema: Some(SCHEMA.to_owned()),
            parent: None,
        },
        Container {
            database: Some(database.to_owned()),
            schema: Some("pg_catalog".to_owned()),
            parent: None,
        },
    ];
    let mut seen = 0;
    while seen < containers.len() {
        let container = containers[seen].clone();
        seen += 1;
        for include_system in [false, true] {
            calls += 1;
            let kinds = match catalog.kinds(session, &container, include_system).await {
                Ok(kinds) => kinds,
                Err(error) => {
                    failures.push(format!("kinds of {container:?}: {error}"));
                    continue;
                }
            };
            for count in kinds {
                calls += 1;
                let list = match catalog
                    .list(session, &container, count.kind, include_system)
                    .await
                {
                    Ok(list) => list,
                    Err(error) => {
                        failures.push(format!("list {:?} of {container:?}: {error}", count.kind));
                        continue;
                    }
                };
                note(
                    &mut failures,
                    format!("list {:?} of {container:?}", count.kind),
                    Ok(format!("{list:?}")),
                );
                if include_system {
                    continue;
                }
                for entry in list {
                    let object = entry.object;
                    // Server-wide lists hold every database and tablespace
                    // of the shared matrix: only ours is described.
                    if container.database.is_none() && object.name != database {
                        continue;
                    }
                    calls += 2;
                    let described = catalog
                        .describe(session, &object)
                        .await
                        .map(|details| format!("{details:?}"));
                    let ddl = catalog.ddl(session, &object).await;
                    if object.schema.as_deref() == Some(SCHEMA) {
                        for (call, result) in [("describe", &described), ("ddl", &ddl)] {
                            if let Ok(text) = result {
                                outputs.insert(format!("{call} {object:?}"), text.clone());
                            }
                        }
                    }
                    note(&mut failures, format!("describe {object:?}"), described);
                    note(&mut failures, format!("ddl {object:?}"), ddl);
                    if entry.has_children && container.schema.is_some() {
                        containers.push(Container {
                            parent: Some(object),
                            ..container.clone()
                        });
                    }
                }
            }
        }
    }
    (failures, calls, outputs)
}

/// No catalog statement calls a function, aggregate or operator, or reads
/// a type or relation, that a user planted in another schema: on 13 and 18,
/// with `public` before `pg_catalog` in `search_path` (identical signatures
/// win) and with the default `search_path` (better matches win), across
/// every list, `describe` and `ddl` of both PostgreSQL catalog modules.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn planted_functions_and_operators_never_run() -> TestResult {
    let database = "m2_pgprog_hijack";
    for (major, port) in servers() {
        drop_hijack_database(port, database).await?;
        let setup = fresh_database(port, database).await?;
        exec(setup.as_ref(), &prerequisites(major)).await?;
        exec(setup.as_ref(), &fixtures(major)).await?;
        exec(setup.as_ref(), &relation_fixtures()).await?;
        // Statistics are taken now, so autovacuum has nothing to change
        // between the baseline and the planted runs.
        exec(setup.as_ref(), "ANALYZE").await?;
        let baseline = {
            let catalog = varve_catalog::catalog_for(setup.server_info())?;
            let (failures, _, outputs) = walk(catalog.as_ref(), setup.as_ref(), database).await;
            assert!(
                failures.is_empty(),
                "PostgreSQL {major}, before planting:\n{}",
                failures.join("\n")
            );
            outputs
        };
        assert!(
            baseline.len() > 50,
            "only {} baseline outputs",
            baseline.len()
        );
        exec(setup.as_ref(), &plant_hijacks()).await?;
        let planted: BTreeMap<String, i64> = {
            let output = exec(
                setup.as_ref(),
                "SELECT what, n FROM public.m2_pgprog_planted",
            )
            .await?;
            let mut planted = BTreeMap::new();
            for result in output.results {
                if let StatementResult::Rows { rows, .. } = result {
                    for row in rows {
                        if let (Some(what), Some(Value::Int(n))) =
                            (row.first().and_then(Value::to_plain_text), row.get(1))
                        {
                            planted.insert(what, *n);
                        }
                    }
                }
            }
            planted
        };
        for (what, at_least) in [
            ("functions", 1500),
            ("aggregates", 40),
            ("operators", 500),
            ("better matches", 10),
            ("types", 40),
            ("relations", 60),
        ] {
            let n = planted.get(what).copied().unwrap_or(0);
            assert!(n >= at_least, "PostgreSQL {major}: only {n} {what} planted");
        }
        // Base types with an element type (`name`, `point`) and range
        // types are planted too.
        let domains = texts(
            setup.as_ref(),
            "SELECT t.typname::pg_catalog.text FROM pg_catalog.pg_type t \
             WHERE t.typnamespace OPERATOR(pg_catalog.=) 'public'::pg_catalog.regnamespace \
               AND t.typname OPERATOR(pg_catalog.=) ANY (ARRAY['name', 'point', 'int4range']::pg_catalog.name[]) \
             ORDER BY 1",
        )
        .await?;
        assert_eq!(
            domains,
            ["int4range", "name", "point"],
            "PostgreSQL {major}"
        );
        setup.close().await?;

        for search_path in ["\"$user\", public", "public, pg_catalog"] {
            let admin = connect(port, &admin_database()).await?;
            exec(
                admin.as_ref(),
                &format!("ALTER DATABASE {database} SET search_path = {search_path}"),
            )
            .await?;
            admin.close().await?;
            let session = connect(port, database).await?;
            assert_eq!(
                texts(session.as_ref(), "SHOW search_path").await?,
                vec![search_path.to_owned()]
            );
            let catalog = varve_catalog::catalog_for(session.server_info())?;
            let (failures, calls, outputs) =
                walk(catalog.as_ref(), session.as_ref(), database).await;
            assert!(
                failures.is_empty(),
                "PostgreSQL {major}, search_path {search_path}: {} of {calls} calls failed:\n{}",
                failures.len(),
                failures.join("\n")
            );
            assert!(calls > 150, "only {calls} calls");
            // The same answers as before planting: no name lookup reached
            // a planted object.
            let differences: Vec<String> = baseline
                .iter()
                .filter(|(call, text)| outputs.get(*call) != Some(*text))
                .map(|(call, text)| {
                    format!(
                        "{call}\n  before: {text}\n  now:    {}",
                        outputs.get(call).map_or("(missing)", String::as_str)
                    )
                })
                .collect();
            assert!(
                differences.is_empty() && outputs.len() == baseline.len(),
                "PostgreSQL {major}, search_path {search_path}: {} of {} answers changed:\n{}",
                differences.len(),
                baseline.len(),
                differences.join("\n")
            );
            session.close().await?;
            let admin = connect(port, database).await?;
            assert_eq!(
                texts(
                    admin.as_ref(),
                    &format!("SELECT is_called::pg_catalog.text FROM {HIJACK_COUNTER}")
                )
                .await
                .unwrap_or_default(),
                vec!["false"],
                "PostgreSQL {major}, search_path {search_path}: a planted object ran"
            );
            admin.close().await?;
        }

        drop_hijack_database(port, database).await?;
    }
    Ok(())
}
