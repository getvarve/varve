//! Name hijacking tests for the SQL the catalog crate sends outside the
//! catalog modules (which `pg_programs` covers): grid data editing
//! (`editable_key`, `plan_edits`, `apply`), the PostgreSQL editors' plans
//! and the `EXPLAIN` wrapper statements, on the oldest and newest servers of
//! the M2 matrix (the `EXPLAIN ANALYZE` safety check has its own test,
//! `explain_safety`):
//!
//! ```sh
//! cargo test -p varve-catalog --test pg_hijack -- --include-ignored
//! ```
//!
//! Every function, aggregate and operator of `pg_catalog` gets an
//! identical-signature copy in `public`, base types get domains of the same
//! name and catalog relations views of the same name, each advancing a
//! sequence and raising `HIJACKED`. Everything then runs with
//! `search_path` set to `"$user", public` (better matches win) and to
//! `public, pg_catalog` (identical signatures win). Nothing planted may
//! run. The test database `m2_pghijack_catalog` is dropped before and after.

use std::collections::BTreeMap;
use std::time::Duration;

use varve_catalog::data_edit::{
    ApplyOutcome, CellValue, PlanOptions, QualifiedOperator, RowChange, RowKey, TableTarget, apply,
    editable_key, plan_edits,
};
use varve_catalog::explain::{ExplainOptions, explain};
use varve_catalog::model::{EditPlan, ObjectKind, ObjectRef};
use varve_catalog::postgres::edit::*;
use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, ExecOptions, ExecOutput, Session, StatementResult, Value,
};
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type R<T = ()> = Result<T, Box<dyn std::error::Error>>;

const DATABASE: &str = "m2_pghijack_catalog";
/// Schema of the fixtures.
const S: &str = "m2_pghijack";
/// The sequence every planted object advances before it raises; sequence
/// advances survive the error and the rollback that follows it.
const HIJACK_COUNTER: &str = "public.m2_pghijack_hijacked";
/// The two `search_path` settings every check runs under.
const SEARCH_PATHS: [&str; 2] = ["\"$user\", public", "public, pg_catalog"];

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

async fn connect(port: u16, database: &str) -> R<Box<dyn Session>> {
    let mut params =
        ConnectParams::new(Engine::Postgres, env_or("VARVE_PG_HOST", "127.0.0.1"), port);
    params.database = Some(database.to_owned());
    params.username = Some(env_or("VARVE_PG_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_PG_PASSWORD", "varve_dev")));
    params.connect_timeout = Duration::from_secs(10);
    params.application_name = "varve-catalog pg_hijack tests".into();
    Ok(PostgresDriver::new().connect(&params).await?)
}

fn admin_database() -> String {
    env_or("VARVE_PG_DATABASE", "varve")
}

async fn exec(session: &dyn Session, sql: &str) -> R<ExecOutput> {
    Ok(session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}\n=> {e}"))?)
}

/// The first column of every row of the first result set, as text.
async fn texts(session: &dyn Session, sql: &str) -> R<Vec<String>> {
    let output = exec(session, sql).await?;
    for result in output.results {
        if let StatementResult::Rows { rows, .. } = result {
            return Ok(rows
                .iter()
                .filter_map(|row| row.first().and_then(Value::to_plain_text))
                .collect());
        }
    }
    Ok(Vec::new())
}

/// Drops the test database from the admin database (the helpers' own SQL
/// never runs inside the planted database's `search_path`).
async fn drop_database(port: u16) -> R {
    let admin = connect(port, &admin_database()).await?;
    exec(
        admin.as_ref(),
        &format!("DROP DATABASE IF EXISTS {DATABASE} WITH (FORCE)"),
    )
    .await?;
    admin.close().await?;
    Ok(())
}

async fn set_search_path(port: u16, search_path: &str) -> R {
    let admin = connect(port, &admin_database()).await?;
    exec(
        admin.as_ref(),
        &format!("ALTER DATABASE {DATABASE} SET search_path = {search_path}"),
    )
    .await?;
    admin.close().await?;
    Ok(())
}

/// True while no planted object has run.
async fn counter_untouched(port: u16) -> R<bool> {
    let session = connect(port, DATABASE).await?;
    let called = texts(
        session.as_ref(),
        &format!("SELECT is_called::pg_catalog.text FROM {HIJACK_COUNTER}"),
    )
    .await?;
    session.close().await?;
    Ok(called == ["false"])
}

/// Plants, in `public`, an object for every function, aggregate, operator,
/// base type and relation of `pg_catalog` that takes its place whenever a
/// statement names it without `pg_catalog.`, plus better matching
/// overloads for the default `search_path`. Every planted object advances
/// [`HIJACK_COUNTER`] and raises `HIJACKED ...`. How many of each were
/// planted is written to `public.m2_pghijack_planted`.
fn plant_hijacks() -> String {
    format!(
        r#"CREATE SEQUENCE {HIJACK_COUNTER};
CREATE FUNCTION public.m2_pghijack_trap() RETURNS pg_catalog.bool LANGUAGE plpgsql AS $t$
BEGIN
    PERFORM pg_catalog.nextval('{HIJACK_COUNTER}'::pg_catalog.regclass);
    RAISE EXCEPTION USING MESSAGE = 'HIJACKED relation or type';
END $t$;
CREATE TABLE public.m2_pghijack_planted (what pg_catalog.text, n pg_catalog.int4);
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
    INSERT INTO public.m2_pghijack_planted VALUES ('functions', planted);

    -- Aggregates: identical signatures, with a raising state function.
    planted := 0;
    FOR item IN
        SELECT p.oid, p.proname, p.pronargs,
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
                'CREATE FUNCTION public.m2_pghijack_agg_%s(pg_catalog.int8%s) RETURNS pg_catalog.int8 '
                'LANGUAGE plpgsql AS %L',
                item.oid, CASE WHEN item.pronargs OPERATOR(pg_catalog.=) 0 THEN ''
                               ELSE pg_catalog.concat(', ', item.types) END, body);
            EXECUTE pg_catalog.format(
                'CREATE AGGREGATE public.%I(%s) (SFUNC = public.m2_pghijack_agg_%s, STYPE = pg_catalog.int8)',
                item.proname, CASE WHEN item.pronargs OPERATOR(pg_catalog.=) 0 THEN '*' ELSE item.types END,
                item.oid);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN OTHERS THEN NULL;
        END;
    END LOOP;
    INSERT INTO public.m2_pghijack_planted VALUES ('aggregates', planted);

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
            EXECUTE pg_catalog.format('CREATE FUNCTION public.m2_pghijack_op_%s(%s) RETURNS %s LANGUAGE plpgsql AS %L',
                item.oid,
                pg_catalog.concat_ws(', ',
                    CASE WHEN item.oprleft OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN item.lefttype END,
                    CASE WHEN item.oprright OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN item.righttype END),
                item.result, body);
            EXECUTE pg_catalog.format('CREATE OPERATOR public.%s (FUNCTION = public.m2_pghijack_op_%s%s%s)',
                item.oprname, item.oid,
                CASE WHEN item.oprleft OPERATOR(pg_catalog.<>) 0::pg_catalog.oid
                     THEN pg_catalog.concat(', LEFTARG = ', item.lefttype) ELSE '' END,
                CASE WHEN item.oprright OPERATOR(pg_catalog.<>) 0::pg_catalog.oid
                     THEN pg_catalog.concat(', RIGHTARG = ', item.righttype) ELSE '' END);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN OTHERS THEN NULL;
        END;
    END LOOP;
    INSERT INTO public.m2_pghijack_planted VALUES ('operators', planted);

    -- Better matches for the default search_path, where pg_catalog comes
    -- first and wins every tie: text comparisons with an unknown or
    -- name-typed side, and the functions the code under test calls.
    planted := 0;
    FOR item IN SELECT * FROM (VALUES
        ('FUNCTION public.m2_pghijack_eq(pg_catalog.name, pg_catalog.text) RETURNS pg_catalog.bool'),
        ('FUNCTION public.m2_pghijack_eq_char(pg_catalog."char", pg_catalog.text) RETURNS pg_catalog.bool'),
        ('FUNCTION public.m2_pghijack_ne(pg_catalog.timestamptz, pg_catalog.text) RETURNS pg_catalog.bool'),
        ('FUNCTION public.json_agg(pg_catalog.name) RETURNS pg_catalog.json'),
        ('FUNCTION public.json_build_object(pg_catalog.text, pg_catalog.name, pg_catalog.text, pg_catalog.name, pg_catalog.text, pg_catalog.name) RETURNS pg_catalog.json'),
        ('FUNCTION public.format_type(pg_catalog.oid, pg_catalog.int8) RETURNS pg_catalog.text'),
        ('FUNCTION public.setval(pg_catalog.text, pg_catalog.int8, pg_catalog.bool) RETURNS pg_catalog.int8')
    ) AS v(head)
    LOOP
        body := 'BEGIN PERFORM pg_catalog.nextval(''public.m2_pghijack_hijacked''::pg_catalog.regclass); '
                'RAISE EXCEPTION USING MESSAGE = ''HIJACKED better match''; END';
        BEGIN
            EXECUTE pg_catalog.format('CREATE %s LANGUAGE plpgsql AS %L', item.head, body);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN OTHERS THEN NULL;
        END;
    END LOOP;
    FOR item IN SELECT * FROM (VALUES
        ('=', 'public.m2_pghijack_eq', 'pg_catalog.name', 'pg_catalog.text'),
        ('=', 'public.m2_pghijack_eq_char', 'pg_catalog."char"', 'pg_catalog.text'),
        ('<>', 'public.m2_pghijack_ne', 'pg_catalog.timestamptz', 'pg_catalog.text')
    ) AS v(op, fn, lefttype, righttype)
    LOOP
        BEGIN
            EXECUTE pg_catalog.format('CREATE OPERATOR public.%s (FUNCTION = %s, LEFTARG = %s, RIGHTARG = %s)',
                item.op, item.fn, item.lefttype, item.righttype);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN OTHERS THEN NULL;
        END;
    END LOOP;
    INSERT INTO public.m2_pghijack_planted VALUES ('better matches', planted);

    -- Catalog relations: views of the same name that raise when read.
    planted := 0;
    FOR item IN
        SELECT c.relname FROM pg_catalog.pg_class c
        WHERE c.relnamespace OPERATOR(pg_catalog.=) 'pg_catalog'::pg_catalog.regnamespace
          AND c.relkind OPERATOR(pg_catalog.=) ANY (ARRAY['r', 'v']::pg_catalog."char"[])
    LOOP
        BEGIN
            EXECUTE pg_catalog.format(
                'CREATE VIEW public.%I AS SELECT * FROM pg_catalog.%I WHERE public.m2_pghijack_trap()',
                item.relname, item.relname);
            planted := planted OPERATOR(pg_catalog.+) 1;
        EXCEPTION WHEN OTHERS THEN NULL;
        END;
    END LOOP;
    INSERT INTO public.m2_pghijack_planted VALUES ('relations', planted);

    -- Base, range, multirange and composite types: domains of the same name
    -- that raise on every value (after the views, which own the row type
    -- names of the catalog relations).
    planted := 0;
    trap := 'CHECK (public.m2_pghijack_trap())';
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
    INSERT INTO public.m2_pghijack_planted VALUES ('types', planted);
END
$plant$;"#
    )
}

/// The tables the grid edits, created before anything is planted: one with
/// a column of most kinds of type, a composite key, an enum key and a key
/// whose unique index uses a case-insensitive operator class of this
/// schema (its `=` is `m2_pghijack.#=#`, not `pg_catalog`'s).
fn fixtures() -> String {
    format!(
        "CREATE SCHEMA {S};
         CREATE TYPE {S}.mood AS ENUM ('sad', 'ok', 'happy');
         CREATE TABLE {S}.items (
             id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
             name text, doc json, tags text[], weight real, mood {S}.mood,
             amount numeric, raw bytea, at timestamptz, uid uuid, span int4range,
             flag boolean, day date
         );
         CREATE TABLE {S}.pair (a integer, b text, v text, PRIMARY KEY (a, b));
         CREATE TABLE {S}.moods (m {S}.mood PRIMARY KEY, n integer);
         CREATE FUNCTION {S}.ci_cmp(a pg_catalog.text, b pg_catalog.text) RETURNS pg_catalog.int4
             LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog, pg_temp
             AS $$ SELECT pg_catalog.bttextcmp(pg_catalog.lower(a), pg_catalog.lower(b)) $$;
         CREATE FUNCTION {S}.ci_lt(a pg_catalog.text, b pg_catalog.text) RETURNS pg_catalog.bool
             LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog, pg_temp
             AS $$ SELECT {S}.ci_cmp(a, b) OPERATOR(pg_catalog.<) 0 $$;
         CREATE FUNCTION {S}.ci_le(a pg_catalog.text, b pg_catalog.text) RETURNS pg_catalog.bool
             LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog, pg_temp
             AS $$ SELECT {S}.ci_cmp(a, b) OPERATOR(pg_catalog.<=) 0 $$;
         CREATE FUNCTION {S}.ci_eq(a pg_catalog.text, b pg_catalog.text) RETURNS pg_catalog.bool
             LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog, pg_temp
             AS $$ SELECT {S}.ci_cmp(a, b) OPERATOR(pg_catalog.=) 0 $$;
         CREATE FUNCTION {S}.ci_ge(a pg_catalog.text, b pg_catalog.text) RETURNS pg_catalog.bool
             LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog, pg_temp
             AS $$ SELECT {S}.ci_cmp(a, b) OPERATOR(pg_catalog.>=) 0 $$;
         CREATE FUNCTION {S}.ci_gt(a pg_catalog.text, b pg_catalog.text) RETURNS pg_catalog.bool
             LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog, pg_temp
             AS $$ SELECT {S}.ci_cmp(a, b) OPERATOR(pg_catalog.>) 0 $$;
         CREATE OPERATOR {S}.#<# (FUNCTION = {S}.ci_lt, LEFTARG = text, RIGHTARG = text);
         CREATE OPERATOR {S}.#<=# (FUNCTION = {S}.ci_le, LEFTARG = text, RIGHTARG = text);
         CREATE OPERATOR {S}.#=# (FUNCTION = {S}.ci_eq, LEFTARG = text, RIGHTARG = text);
         CREATE OPERATOR {S}.#>=# (FUNCTION = {S}.ci_ge, LEFTARG = text, RIGHTARG = text);
         CREATE OPERATOR {S}.#># (FUNCTION = {S}.ci_gt, LEFTARG = text, RIGHTARG = text);
         CREATE OPERATOR CLASS {S}.ci_ops FOR TYPE text USING btree AS
             OPERATOR 1 {S}.#<#, OPERATOR 2 {S}.#<=#, OPERATOR 3 {S}.#=#,
             OPERATOR 4 {S}.#>=#, OPERATOR 5 {S}.#>#,
             FUNCTION 1 {S}.ci_cmp(text, text);
         CREATE TABLE {S}.ci (code text NOT NULL, v integer);
         CREATE UNIQUE INDEX ci_code ON {S}.ci (code {S}.ci_ops);
         INSERT INTO {S}.ci VALUES ('ABC', 1);
         INSERT INTO {S}.pair VALUES (1, 'x', 'old');
         INSERT INTO {S}.moods VALUES ('ok', 1);"
    )
}

fn target(table: &str) -> TableTarget {
    TableTarget {
        database: DATABASE.into(),
        schema: Some(S.into()),
        table: table.into(),
    }
}

fn cells(values: &[(&str, Value)]) -> Vec<(String, CellValue)> {
    values
        .iter()
        .map(|(name, value)| ((*name).to_owned(), CellValue::Value(value.clone())))
        .collect()
}

fn row_key(values: &[(&str, Value)]) -> RowKey {
    RowKey {
        columns: values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect(),
    }
}

fn operator(schema: &str, name: &str) -> QualifiedOperator {
    QualifiedOperator {
        schema: schema.into(),
        name: name.into(),
    }
}

/// Plans and applies `changes`; every change must be applied. Returns the
/// rows as stored.
async fn edit(
    session: &dyn Session,
    table: &str,
    changes: &[RowChange],
    check_original: bool,
) -> R<Vec<Option<Vec<(String, Value)>>>> {
    let server = session.server_info().clone();
    let key = editable_key(session, &server, &target(table))
        .await?
        .ok_or_else(|| format!("{table} is not editable"))?;
    let options = PlanOptions {
        check_original,
        key,
    };
    let plan = plan_edits(&server, &target(table), changes, &options)?;
    match apply(session, &server, &plan).await? {
        ApplyOutcome::Applied { rows } => Ok(rows.into_iter().map(|r| r.row).collect()),
        other => Err(format!("{table}: {other:?}\n{}", plan.script()).into()),
    }
}

/// Grid editing on every fixture table: keys, inserts, updates and deletes
/// with original value checks.
async fn data_edits(session: &dyn Session) -> R {
    let server = session.server_info().clone();
    let key = editable_key(session, &server, &target("items"))
        .await?
        .ok_or("items is not editable")?;
    assert_eq!(key.equality_operators, vec![operator("pg_catalog", "=")]);
    let ci = editable_key(session, &server, &target("ci"))
        .await?
        .ok_or("ci is not editable")?;
    assert_eq!(ci.equality_operators, vec![operator(S, "#=#")]);

    let inserted = edit(
        session,
        "items",
        &[RowChange::Insert {
            values: cells(&[
                ("name", Value::Text("it's".into())),
                ("doc", Value::json(r#"{"b": 1, "a": [2]}"#)),
                (
                    "tags",
                    Value::Array(vec![Value::Text("x".into()), Value::Null]),
                ),
                ("weight", Value::Float(1.5)),
                ("mood", Value::Text("ok".into())),
                ("amount", Value::Decimal("12.50".into())),
                ("raw", Value::Bytes(vec![0, 0xff])),
                ("at", Value::TimestampTz("2026-10-19T11:45:00.5Z".into())),
                (
                    "uid",
                    Value::Uuid("6f1c2b8e-0000-4000-8000-000000000001".into()),
                ),
                ("span", Value::Text("[1,5)".into())),
                ("flag", Value::Bool(true)),
                ("day", Value::Date("2026-10-19".into())),
            ]),
        }],
        false,
    )
    .await?;
    let original = inserted
        .into_iter()
        .next()
        .flatten()
        .ok_or("the insert returned no row")?;
    let id = original
        .iter()
        .find(|(name, _)| name == "id")
        .map(|(_, value)| value.clone())
        .ok_or("no id")?;
    let updated = edit(
        session,
        "items",
        &[RowChange::Update {
            key: row_key(&[("id", id.clone())]),
            original: original.clone(),
            set: cells(&[
                ("name", Value::Text("new".into())),
                ("doc", Value::json("[1]")),
                ("weight", Value::Float(2.25)),
                ("mood", Value::Text("happy".into())),
                ("raw", Value::Bytes(vec![1, 2])),
                ("flag", Value::Null),
            ]),
        }],
        true,
    )
    .await?;
    let current = updated
        .into_iter()
        .next()
        .flatten()
        .ok_or("the update returned no row")?;
    edit(
        session,
        "items",
        &[RowChange::Delete {
            key: row_key(&[("id", id)]),
            original: current,
        }],
        true,
    )
    .await?;

    // The case-insensitive key finds 'ABC' by 'abc': the index's own
    // operator class compared, not text equality.
    edit(
        session,
        "ci",
        &[RowChange::Update {
            key: row_key(&[("code", Value::Text("abc".into()))]),
            original: vec![("v".into(), Value::Int(1))],
            set: cells(&[("v", Value::Int(2))]),
        }],
        true,
    )
    .await?;
    edit(
        session,
        "ci",
        &[RowChange::Update {
            key: row_key(&[("code", Value::Text("aBc".into()))]),
            original: vec![("v".into(), Value::Int(2))],
            set: cells(&[("v", Value::Int(1))]),
        }],
        true,
    )
    .await?;

    edit(
        session,
        "pair",
        &[RowChange::Update {
            key: row_key(&[("a", Value::Int(1)), ("b", Value::Text("x".into()))]),
            original: vec![("v".into(), Value::Text("old".into()))],
            set: cells(&[("v", Value::Text("new".into()))]),
        }],
        true,
    )
    .await?;
    edit(
        session,
        "pair",
        &[RowChange::Update {
            key: row_key(&[("a", Value::Int(1)), ("b", Value::Text("x".into()))]),
            original: vec![("v".into(), Value::Text("new".into()))],
            set: cells(&[("v", Value::Text("old".into()))]),
        }],
        true,
    )
    .await?;
    edit(
        session,
        "moods",
        &[RowChange::Update {
            key: row_key(&[("m", Value::Text("ok".into()))]),
            original: vec![("n".into(), Value::Int(1))],
            set: cells(&[("n", Value::Int(1))]),
        }],
        true,
    )
    .await?;
    Ok(())
}

/// `EXPLAIN` alone, `EXPLAIN ANALYZE` in its own transaction and inside an
/// open transaction (savepoint).
async fn explains(session: &dyn Session) -> R {
    let server = session.server_info().clone();
    let statement = format!("SELECT * FROM {S}.pair");
    explain(session, &server, &statement, &ExplainOptions::default()).await?;
    let analyze = ExplainOptions {
        analyze: true,
        buffers: true,
        ..ExplainOptions::default()
    };
    explain(session, &server, &statement, &analyze).await?;
    // Inside a transaction a run needs confirmation (it cannot be made
    // read-only); confirmed, it runs in a savepoint.
    let confirmed = ExplainOptions {
        confirmed: true,
        ..analyze
    };
    exec(session, "BEGIN").await?;
    let in_transaction = explain(session, &server, &statement, &confirmed).await;
    exec(session, "ROLLBACK").await?;
    in_transaction?;
    Ok(())
}

fn q(schema: &str, name: &str) -> QualifiedName {
    QualifiedName::new(schema, name)
}

fn plain_table(name: QualifiedName, columns: Vec<ColumnDef>) -> CreateTable {
    CreateTable {
        name,
        if_not_exists: false,
        unlogged: false,
        columns,
        constraints: vec![],
        inherits: vec![],
        partition_of: None,
        partition_by: None,
        storage_params: vec![],
        tablespace: None,
        comment: None,
    }
}

fn routine(name: QualifiedName, language: &str, source: &str) -> CreateRoutine {
    CreateRoutine {
        routine: RoutineKind::Function,
        name,
        or_replace: false,
        arguments: vec![],
        returns: None,
        language: language.into(),
        body: RoutineBody::Definition {
            source: source.into(),
        },
        volatility: None,
        strict: None,
        security_definer: None,
        leakproof: None,
        parallel: None,
        cost: None,
        rows: None,
        window: false,
        settings: vec![],
        comment: None,
    }
}

fn object(kind: ObjectKind, schema: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind,
        database: Some(DATABASE.into()),
        schema: Some(schema.into()),
        parent: None,
        name: name.into(),
        signature: None,
    }
}

/// Editor changes of most kinds, in a schema of their own. Every type the
/// test writes is a SQL keyword type or `pg_catalog`-qualified and every
/// expression names `pg_catalog`'s operators: only what the editors add
/// themselves is under test.
fn ddl_changes(major: u32, d: &str) -> Vec<PgChange> {
    let check = |name: &str, expression: &str| TableConstraint {
        name: Some(name.into()),
        ..TableConstraint::new(ConstraintKind::Check {
            expression: expression.into(),
            no_inherit: false,
        })
    };
    let mut items = plain_table(
        q(d, "items"),
        vec![
            ColumnDef {
                not_null: true,
                identity: Some(Identity {
                    generation: IdentityGeneration::Always,
                    options: SequenceOptions {
                        start: Some(100),
                        ..SequenceOptions::default()
                    },
                }),
                comment: Some("key".into()),
                ..ColumnDef::new("id", "bigint")
            },
            ColumnDef {
                default: Some("'x'".into()),
                ..ColumnDef::new("name", "pg_catalog.text")
            },
            ColumnDef::new("ref_id", "integer"),
            ColumnDef {
                not_null: true,
                default: Some("1".into()),
                ..ColumnDef::new("qty", "integer")
            },
        ],
    );
    items.constraints = vec![
        TableConstraint {
            name: Some("items_pk".into()),
            ..TableConstraint::new(ConstraintKind::PrimaryKey {
                columns: vec!["id".into()],
                include: vec![],
                storage_params: vec![StorageParam::new("fillfactor", "90")],
                index_tablespace: None,
            })
        },
        TableConstraint {
            name: Some("items_name_uq".into()),
            ..TableConstraint::new(ConstraintKind::Unique {
                columns: vec!["name".into()],
                include: vec![],
                nulls_not_distinct: major >= 15,
                storage_params: vec![],
                index_tablespace: None,
            })
        },
        check("qty_pos", "qty OPERATOR(pg_catalog.>) 0"),
        TableConstraint {
            name: Some("items_ref".into()),
            ..TableConstraint::new(ConstraintKind::ForeignKey {
                columns: vec!["ref_id".into()],
                references: q(d, "ref"),
                referenced_columns: vec!["id".into()],
                match_type: None,
                on_delete: Some(ReferentialAction::Cascade),
                on_update: None,
            })
        },
    ];
    items.storage_params = vec![StorageParam::new("fillfactor", "80")];
    items.comment = Some("items".into());
    let mut reference = plain_table(q(d, "ref"), vec![ColumnDef::new("id", "integer")]);
    reference.constraints = vec![TableConstraint::new(ConstraintKind::PrimaryKey {
        columns: vec!["id".into()],
        include: vec![],
        storage_params: vec![],
        index_tablespace: None,
    })];
    let trigger_fn = CreateRoutine {
        returns: Some(Returns::Type {
            data_type: "pg_catalog.trigger".into(),
            setof: false,
        }),
        ..routine(q(d, "touch"), "plpgsql", "BEGIN RETURN NEW; END")
    };
    let add_one = CreateRoutine {
        arguments: vec![RoutineArg::new("a", "integer")],
        returns: Some(Returns::Type {
            data_type: "integer".into(),
            setof: false,
        }),
        volatility: Some(Volatility::Immutable),
        strict: Some(true),
        comment: Some("adds one".into()),
        ..routine(q(d, "add_one"), "sql", "SELECT a OPERATOR(pg_catalog.+) 1")
    };
    vec![
        PgChange::Schema(SchemaChange::Create {
            name: d.into(),
            owner: None,
            if_not_exists: false,
            comment: Some("hijack ddl".into()),
        }),
        PgChange::Table(TableChange::Create(Box::new(reference))),
        PgChange::Table(TableChange::Create(Box::new(items))),
        PgChange::Table(TableChange::Alter {
            name: q(d, "items"),
            operations: vec![
                TableOperation::AddColumn {
                    column: ColumnDef {
                        default: Some("'n'".into()),
                        comment: Some("a note".into()),
                        ..ColumnDef::new("note", "pg_catalog.text")
                    },
                    if_not_exists: true,
                },
                TableOperation::RenameColumn {
                    name: "note".into(),
                    new_name: "memo".into(),
                },
                TableOperation::SetDefault {
                    column: "qty".into(),
                    expression: "5".into(),
                },
                TableOperation::AddConstraint {
                    constraint: check("qty_small", "qty OPERATOR(pg_catalog.<) 1000"),
                    not_valid: true,
                },
                TableOperation::ValidateConstraint {
                    name: "qty_small".into(),
                },
                TableOperation::AlterIdentity {
                    column: "id".into(),
                    generation: Some(IdentityGeneration::ByDefault),
                    options: SequenceOptions {
                        increment: Some(5),
                        ..SequenceOptions::default()
                    },
                    restart: Some(Restart { with: Some(1000) }),
                },
                TableOperation::SetStorageParams {
                    params: vec![StorageParam::new("autovacuum_enabled", "false")],
                },
                TableOperation::ResetStorageParams {
                    names: vec!["fillfactor".into()],
                },
                TableOperation::RowLevelSecurity { enabled: true },
            ],
        }),
        PgChange::Index(IndexChange::Create(CreateIndex {
            name: Some("items_qty".into()),
            table: q(d, "items"),
            unique: false,
            nulls_not_distinct: false,
            concurrently: false,
            if_not_exists: false,
            only: false,
            method: Some(IndexMethod::Btree),
            elements: vec![IndexElement::column("qty")],
            include: vec!["name".into()],
            storage_params: vec![StorageParam::new("fillfactor", "70")],
            tablespace: None,
            predicate: Some("qty OPERATOR(pg_catalog.>) 1".into()),
            comment: Some("by quantity".into()),
        })),
        PgChange::View(ViewChange::Create(CreateView {
            name: q(d, "v"),
            or_replace: false,
            columns: vec!["id".into(), "qty".into()],
            query: format!("SELECT id, qty FROM {d}.items WHERE qty OPERATOR(pg_catalog.>) 0"),
            check_option: Some(CheckOption::Cascaded),
            security_barrier: Some(true),
            security_invoker: (major >= 15).then_some(true),
            comment: Some("positive".into()),
        })),
        PgChange::MaterializedView(MaterializedViewChange::Create(CreateMaterializedView {
            name: q(d, "mv"),
            if_not_exists: false,
            columns: vec!["id".into()],
            query: format!("SELECT id FROM {d}.items"),
            with_data: false,
            storage_params: vec![StorageParam::new("fillfactor", "50")],
            tablespace: None,
            comment: Some("mat".into()),
        })),
        PgChange::MaterializedView(MaterializedViewChange::Refresh {
            name: q(d, "mv"),
            concurrently: false,
            has_unique_index: false,
            with_data: true,
        }),
        PgChange::Sequence(SequenceChange::Create(CreateSequence {
            name: q(d, "ids"),
            if_not_exists: false,
            unlogged: false,
            options: SequenceOptions {
                data_type: Some(SequenceType::Integer),
                increment: Some(2),
                start: Some(10),
                ..SequenceOptions::default()
            },
            owned_by: Some(ColumnRef {
                table: q(d, "items"),
                column: "qty".into(),
            }),
            comment: Some("ids".into()),
        })),
        PgChange::Sequence(SequenceChange::Alter {
            name: q(d, "ids"),
            operations: vec![
                SequenceOperation::Restart { with: Some(50) },
                SequenceOperation::SetValue {
                    value: 42,
                    is_called: false,
                },
                SequenceOperation::Rename {
                    new_name: "ids2".into(),
                },
            ],
        }),
        PgChange::Routine(RoutineChange::Create(add_one)),
        PgChange::Routine(RoutineChange::Create(trigger_fn)),
        PgChange::Trigger(TriggerChange::Create(CreateTrigger {
            name: "touch".into(),
            table: q(d, "items"),
            or_replace: false,
            constraint: false,
            timing: TriggerTiming::Before,
            events: vec![TriggerEvent::Insert],
            level: TriggerLevel::Row,
            old_table: None,
            new_table: None,
            condition: Some("NEW.qty IS NOT NULL".into()),
            function: q(d, "touch"),
            arguments: vec!["it's".into()],
            deferrable: false,
            initially_deferred: false,
            comment: Some("touches".into()),
        })),
        PgChange::Type(TypeChange::CreateEnum {
            name: q(d, "mood"),
            labels: vec!["sad".into(), "ok".into()],
            comment: Some("moods".into()),
        }),
        PgChange::Domain(DomainChange::Create(CreateDomain {
            name: q(d, "posint"),
            data_type: "integer".into(),
            collation: None,
            default: Some("1".into()),
            not_null: true,
            checks: vec![DomainCheck {
                name: Some("positive".into()),
                expression: "VALUE OPERATOR(pg_catalog.>) 0".into(),
            }],
            comment: Some("positive ints".into()),
        })),
        PgChange::Policy(PolicyChange::Create(CreatePolicy {
            name: "everything".into(),
            table: q(d, "items"),
            restrictive: false,
            command: PolicyCommand::All,
            roles: vec![RoleSpec::Public],
            using: Some("true".into()),
            with_check: Some("qty OPERATOR(pg_catalog.>=) 0".into()),
            comment: Some("all rows".into()),
        })),
        PgChange::Comment(CommentChange {
            target: object(ObjectKind::Table, d, "items"),
            comment: Some("commented".into()),
        }),
        PgChange::Owner(OwnerChange {
            target: object(ObjectKind::View, d, "v"),
            owner: RoleSpec::CurrentUser,
        }),
        PgChange::Schema(SchemaChange::Drop {
            name: d.into(),
            if_exists: false,
            behavior: DropBehavior::Cascade,
        }),
    ]
}

/// Runs a plan the way the app does: in one transaction when it says so.
async fn run_plan(session: &dyn Session, plan: &EditPlan) -> R {
    if plan.transactional {
        exec(session, "BEGIN").await?;
        for statement in &plan.statements {
            if let Err(error) = exec(session, statement).await {
                exec(session, "ROLLBACK").await?;
                return Err(error);
            }
        }
        exec(session, "COMMIT").await?;
    } else {
        for statement in &plan.statements {
            exec(session, statement).await?;
        }
    }
    Ok(())
}

async fn ddl(session: &dyn Session, major: u32, schema: &str) -> R {
    for change in ddl_changes(major, schema) {
        let plan = varve_catalog::postgres::edit::plan(session.server_info(), &change)?;
        run_plan(session, &plan).await?;
    }
    Ok(())
}

/// Reads the plant counts.
async fn planted(port: u16) -> R<BTreeMap<String, i64>> {
    let session = connect(port, DATABASE).await?;
    let output = exec(
        session.as_ref(),
        "SELECT what, n FROM public.m2_pghijack_planted",
    )
    .await?;
    session.close().await?;
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
    Ok(planted)
}

/// Grid editing, editor plans and `EXPLAIN` wrappers never run a planted
/// function, aggregate, operator, type or relation: on 13 and 18, with the
/// default `search_path` and with `public` before `pg_catalog`. A control
/// at the end proves the planted objects do run when SQL is unqualified.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn planted_objects_never_run() -> R {
    for (major, port) in servers() {
        drop_database(port).await?;
        let admin = connect(port, &admin_database()).await?;
        exec(admin.as_ref(), &format!("CREATE DATABASE {DATABASE}")).await?;
        admin.close().await?;
        let setup = connect(port, DATABASE).await?;
        exec(setup.as_ref(), &fixtures()).await?;
        exec(setup.as_ref(), &plant_hijacks()).await?;
        setup.close().await?;
        let planted = planted(port).await?;
        for (what, at_least) in [
            ("functions", 1500),
            ("aggregates", 40),
            ("operators", 500),
            ("better matches", 6),
            ("types", 40),
            ("relations", 60),
        ] {
            let n = planted.get(what).copied().unwrap_or(0);
            assert!(n >= at_least, "PostgreSQL {major}: only {n} {what} planted");
        }

        for (pass, search_path) in SEARCH_PATHS.iter().enumerate() {
            set_search_path(port, search_path).await?;
            let session = connect(port, DATABASE).await?;
            let shown = texts(session.as_ref(), "SHOW search_path").await?;
            assert_eq!(shown, vec![(*search_path).to_owned()]);
            let context = format!("PostgreSQL {major}, search_path {search_path}");
            data_edits(session.as_ref())
                .await
                .map_err(|e| format!("{context}: data edit: {e}"))?;
            explains(session.as_ref())
                .await
                .map_err(|e| format!("{context}: explain: {e}"))?;
            ddl(session.as_ref(), major, &format!("m2_pghijack_ddl{pass}"))
                .await
                .map_err(|e| format!("{context}: editor plans: {e}"))?;
            session.close().await?;
            assert!(
                counter_untouched(port).await?,
                "{context}: a planted object ran"
            );
        }

        // Control: the same session setup runs a planted operator as soon
        // as SQL leaves out `pg_catalog`.
        let session = connect(port, DATABASE).await?;
        let control = exec(session.as_ref(), "SELECT 1 = 1").await;
        session.close().await?;
        let message = control.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("HIJACKED"),
            "PostgreSQL {major}: the control did not hit a planted operator: {message:?}"
        );
        assert!(!counter_untouched(port).await?);

        drop_database(port).await?;
    }
    Ok(())
}
