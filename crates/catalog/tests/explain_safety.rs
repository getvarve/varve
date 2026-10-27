//! The fail-closed `EXPLAIN ANALYZE` safety check on PostgreSQL 13 and 18:
//! every way the catalog audits (rounds 5 to 13) found to reach user code
//! or a side-effecting built-in without confirmation must ask for it, and
//! nothing planted may run; statements that read only built-ins still run
//! without confirmation (read-only); a proven statement that fails reports
//! its own error; dropping a call at any point leaves nothing open. The
//! check resolves every name on the server (a temporary view's
//! dependencies), so no spelling of a name can hide an object.
//!
//! ```sh
//! cargo test -p varve-catalog --test explain_safety -- --include-ignored
//! ```
//!
//! Works in the database `m2_pgsafety` and the role `m2_pgsafety_planter`,
//! dropped before and after.

use std::time::{Duration, Instant};

use varve_catalog::CatalogError;
use varve_catalog::explain::{
    AnalyzeSafety, ExplainCancel, ExplainOptions, analyze_safety_checked, explain,
    explain_with_cancel,
};
use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, ErrorKind, ExecOptions, ExecOutput, Session, StatementResult, Value,
};
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type R<T = ()> = Result<T, Box<dyn std::error::Error>>;

const DATABASE: &str = "m2_pgsafety";
const PLANTER: &str = "m2_pgsafety_planter";
const PLANTER_PASSWORD: &str = "m2 safety pw";

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

async fn connect_as(port: u16, database: &str, user: &str, password: &str) -> R<Box<dyn Session>> {
    connect_with(port, database, user, password, false).await
}

async fn connect_with(
    port: u16,
    database: &str,
    user: &str,
    password: &str,
    read_only: bool,
) -> R<Box<dyn Session>> {
    let mut params =
        ConnectParams::new(Engine::Postgres, env_or("VARVE_PG_HOST", "127.0.0.1"), port);
    params.database = Some(database.to_owned());
    params.username = Some(user.to_owned());
    params.password = Some(Secret::new(password.to_owned()));
    params.connect_timeout = Duration::from_secs(10);
    params.read_only = read_only;
    params.application_name = "varve-catalog explain_safety tests".into();
    Ok(PostgresDriver::new()
        .connect(&params)
        .await
        .map_err(|e| format!("connect as {user}: {e}"))?)
}

async fn connect(port: u16, database: &str) -> R<Box<dyn Session>> {
    connect_as(
        port,
        database,
        &env_or("VARVE_PG_USER", "varve"),
        &env_or("VARVE_PG_PASSWORD", "varve_dev"),
    )
    .await
}

async fn exec(session: &dyn Session, sql: &str) -> R<ExecOutput> {
    Ok(session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}\n=> {e}"))?)
}

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

async fn cleanup(admin: &dyn Session) -> R {
    exec(
        admin,
        &format!("DROP DATABASE IF EXISTS {DATABASE} WITH (FORCE)"),
    )
    .await?;
    exec(admin, &format!("DROP ROLE IF EXISTS {PLANTER}")).await?;
    Ok(())
}

/// Everything the audit planted, in `public`. Every planted function
/// advances `public.hits`; the views and the trigger also take advisory
/// lock 4243.
const FIXTURES: &str = "
CREATE SEQUENCE public.hits;
CREATE TABLE public.t (a int, b text, c varchar);
INSERT INTO public.t VALUES (1, 'x', 'y');
CREATE FUNCTION public.bump() RETURNS int LANGUAGE sql AS
  $$ SELECT pg_catalog.nextval('public.hits')::int $$;
CREATE FUNCTION public.evilrec(int) RETURNS SETOF record LANGUAGE sql AS
  $$ SELECT pg_catalog.nextval('public.hits') $$;
CREATE FUNCTION public.text(int) RETURNS text LANGUAGE sql VOLATILE AS
  $$ SELECT pg_catalog.nextval('public.hits')::text $$;
CREATE FUNCTION public.first(int) RETURNS int LANGUAGE sql VOLATILE AS
  $$ SELECT pg_catalog.nextval('public.hits')::int $$;
CREATE FUNCTION public.\"delete\"(int) RETURNS int LANGUAGE sql VOLATILE AS
  $$ SELECT pg_catalog.nextval('public.hits')::int $$;
CREATE FUNCTION public.mode(int) RETURNS int LANGUAGE sql VOLATILE AS
  $$ SELECT pg_catalog.nextval('public.hits')::int $$;
CREATE FUNCTION public.key(int) RETURNS int LANGUAGE sql VOLATILE AS
  $$ SELECT pg_catalog.nextval('public.hits')::int $$;
CREATE FUNCTION public.evil(public.t) RETURNS int LANGUAGE plpgsql AS
  $$ BEGIN RETURN pg_catalog.nextval('public.hits'); END $$;
CREATE FUNCTION public.chk(int) RETURNS boolean LANGUAGE plpgsql AS
  $$ BEGIN PERFORM pg_catalog.nextval('public.hits'); RETURN true; END $$;
CREATE DOMAIN public.dom AS int CHECK (public.chk(VALUE));
CREATE VIEW public.vlock AS SELECT pg_catalog.pg_advisory_lock(4243)::text AS l;
CREATE VIEW public.vseq AS SELECT pg_catalog.nextval('public.hits') AS n;
CREATE TYPE public.ct AS (k int);
CREATE FUNCTION public.ct_text(public.ct) RETURNS text LANGUAGE plpgsql AS
  $$ BEGIN PERFORM pg_catalog.nextval('public.hits'); RETURN 'ct'; END $$;
CREATE TABLE public.tc (k public.ct);
INSERT INTO public.tc VALUES (ROW(1));
CREATE FUNCTION public.trg() RETURNS trigger LANGUAGE plpgsql AS
  $$ BEGIN PERFORM pg_catalog.pg_advisory_lock(4243);
     PERFORM pg_catalog.nextval('public.hits'); RETURN NEW; END $$;
CREATE TABLE public.trgt (a int);
CREATE TRIGGER trgt_before BEFORE INSERT ON public.trgt FOR EACH ROW EXECUTE FUNCTION public.trg();
CREATE TABLE public.rls (a int);
INSERT INTO public.rls VALUES (1);
ALTER TABLE public.rls ENABLE ROW LEVEL SECURITY;
CREATE POLICY rls_all ON public.rls USING (public.bump() OPERATOR(pg_catalog.>) 0);
CREATE EXTENSION dblink;
CREATE EXTENSION citext;
CREATE VIEW public.w_lock AS SELECT pg_catalog.pg_try_advisory_lock(424201) AS l;
CREATE VIEW public.v_xml AS
  SELECT pg_catalog.table_to_xml('public.w_lock'::pg_catalog.regclass, true, false, '') AS x;
CREATE SCHEMA lockschema;
CREATE VIEW lockschema.w AS SELECT pg_catalog.pg_try_advisory_lock(424202) AS l;
CREATE VIEW public.v_schema AS SELECT pg_catalog.schema_to_xml('lockschema', true, false, '') AS x;
CREATE VIEW public.v_query AS
  SELECT pg_catalog.query_to_xml('SELECT pg_catalog.pg_try_advisory_lock(424203)', true, false, '') AS x;
CREATE FUNCTION public.imm(int) RETURNS int LANGUAGE plpgsql IMMUTABLE AS
  $$ BEGIN RETURN pg_catalog.nextval('public.hits'); END $$;
CREATE TYPE public.cdom AS (k public.dom);
CREATE TYPE public.rdom AS RANGE (subtype = public.dom);
CREATE VIEW public.v_din AS
  SELECT pg_catalog.domain_in('1', 'public.dom'::pg_catalog.regtype, -1) IS NULL AS x;
CREATE VIEW public.v_rin AS
  SELECT pg_catalog.record_in('(1)', 'public.cdom'::pg_catalog.regtype, -1) IS NULL AS x;
CREATE VIEW public.v_ain AS
  SELECT pg_catalog.array_in('{1}', 'public.dom'::pg_catalog.regtype, -1) IS NULL AS x;
CREATE VIEW public.v_rgin AS
  SELECT pg_catalog.range_in('[1,2]', 'public.rdom'::pg_catalog.regtype, -1) IS NULL AS x;
CREATE VIEW public.v_xt AS
  SELECT * FROM XMLTABLE('/r' PASSING ('<r><a>1</a></r>'::xml) COLUMNS a public.dom PATH 'a');
CREATE VIEW public.v_outer AS SELECT x.a FROM public.v_xt x;
CREATE VIEW public.v_cast AS SELECT CAST(t.a AS public.dom) AS d FROM public.t;
CREATE TABLE public.t_dom (d public.dom);
CREATE TABLE public.s (v text);
INSERT INTO public.s VALUES ('(5)');
CREATE VIEW public.v_plain AS SELECT x.d FROM public.t_dom x;
CREATE MATERIALIZED VIEW public.mv_plain AS SELECT x.d FROM public.t_dom x;
CREATE FUNCTION public.noop(int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN $1; END $$;
CREATE MATERIALIZED VIEW public.mv_noop AS SELECT public.noop(1) AS n;
CREATE TABLE public.ruled (a int);
CREATE RULE ruled_ins AS ON INSERT TO public.ruled DO ALSO SELECT public.noop(1);
CREATE DOMAIN public.posint AS int CHECK (VALUE > 0);
CREATE TABLE public.tt (a int);
INSERT INTO public.tt VALUES (1);
CREATE FUNCTION public.to_ct(bigint) RETURNS public.ct LANGUAGE plpgsql AS
  $$ BEGIN PERFORM pg_catalog.nextval('public.hits'); RETURN ROW(1)::public.ct; END $$;
CREATE CAST (bigint AS public.ct) WITH FUNCTION public.to_ct(bigint);
CREATE EXTENSION hstore;
CREATE EXTENSION pg_trgm;
CREATE FUNCTION public.chk13(text) RETURNS boolean LANGUAGE plpgsql AS
  $$ BEGIN IF $1 = 'bad' THEN PERFORM pg_catalog.nextval('public.hits'); END IF; RETURN true; END $$;
CREATE DOMAIN public.dom13 AS text CHECK (public.chk13(VALUE));
CREATE TABLE public.td (v public.dom13);
INSERT INTO public.td VALUES ('good');
CREATE FUNCTION public.myplus(int, int) RETURNS int LANGUAGE plpgsql AS
  $$ BEGIN PERFORM pg_catalog.nextval('public.hits'); RETURN $1 + $2; END $$;
CREATE OPERATOR public.=== (LEFTARG = int, RIGHTARG = int, FUNCTION = public.myplus);
CREATE AGGREGATE public.mysum(int) (SFUNC = public.myplus, STYPE = int, INITCOND = '0');
CREATE FUNCTION public.ct_int(public.ct) RETURNS int LANGUAGE plpgsql AS
  $$ BEGIN PERFORM pg_catalog.nextval('public.hits'); RETURN 1; END $$;
CREATE TABLE public.twin (a int);
INSERT INTO public.twin VALUES (1);
CREATE SCHEMA twinschema;
CREATE DOMAIN twinschema.twin AS int CHECK (public.chk(VALUE));
CREATE SCHEMA r12a;
CREATE SCHEMA r12b;
CREATE FUNCTION r12a.chk(text) RETURNS boolean LANGUAGE plpgsql AS
  $$ BEGIN PERFORM pg_catalog.nextval('public.hits'); RETURN true; END $$;
CREATE DOMAIN r12a.t AS text CHECK (r12a.chk(VALUE));
CREATE TABLE r12b.t (a text);
INSERT INTO r12b.t VALUES ('x');
CREATE TYPE r12a.mood AS ENUM ('x');
CREATE TABLE r12b.mood (a text);
CREATE DOMAIN r12a.dm AS r12a.mood CHECK (r12a.chk(VALUE::text));
CREATE TABLE r12b.dm (a text);
CREATE VIEW public.v_safe_xt AS
  SELECT * FROM XMLTABLE('/r' PASSING ('<r><a>1</a></r>'::xml) COLUMNS a public.posint PATH 'a');
CREATE DOMAIN public.dom2 AS public.dom;
CREATE TABLE public.rls_ok (a int);
INSERT INTO public.rls_ok VALUES (1);
ALTER TABLE public.rls_ok ENABLE ROW LEVEL SECURITY;
CREATE POLICY rls_ok_all ON public.rls_ok USING (a OPERATOR(pg_catalog.>) 0);
CREATE EXTENSION postgres_fdw;
CREATE SERVER nowhere FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'x');
CREATE FOREIGN TABLE public.ft (a int) SERVER nowhere;
CREATE TABLE public.part (a int) PARTITION BY LIST (a);
CREATE TABLE public.part1 PARTITION OF public.part FOR VALUES IN (1);
INSERT INTO public.part VALUES (1);
CREATE TABLE public.partf (a int) PARTITION BY LIST (a);
CREATE TABLE public.partf1 PARTITION OF public.partf FOR VALUES IN (1);
CREATE FOREIGN TABLE public.partf2 PARTITION OF public.partf FOR VALUES IN (2) SERVER nowhere;
CREATE COLLATION public.myicu (provider = icu, locale = 'und');
CREATE EXTENSION tsm_system_rows;
CREATE VIEW public.v_sample AS SELECT t.a FROM public.t TABLESAMPLE system_rows(1);
CREATE FUNCTION public.uccmp(a int, b int) RETURNS int LANGUAGE plpgsql IMMUTABLE AS
  $$ BEGIN RETURN pg_catalog.btint4cmp(a, b); END $$;
CREATE OPERATOR CLASS public.int4_uc_ops FOR TYPE int4 USING btree AS
  OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
  FUNCTION 1 public.uccmp(int4, int4);
CREATE TYPE public.rngo AS RANGE (subtype = int4, subtype_opclass = public.int4_uc_ops);
CREATE TABLE public.trng (id int, r public.rngo);
INSERT INTO public.trng SELECT g, public.rngo(g, g + 5) FROM generate_series(1, 5) g;
CREATE TABLE public.tix (a int);
INSERT INTO public.tix SELECT g FROM generate_series(1, 100) g;
CREATE INDEX tix_a ON public.tix (a public.int4_uc_ops);
CREATE TABLE public.tixd (a int);
INSERT INTO public.tixd SELECT g FROM generate_series(1, 100) g;
CREATE INDEX tixd_a ON public.tixd (a);
CREATE TABLE public.pto (a int) PARTITION BY RANGE (a public.int4_uc_ops);
CREATE TABLE public.pto1 PARTITION OF public.pto FOR VALUES FROM (0) TO (100);
INSERT INTO public.pto SELECT g FROM generate_series(1, 10) g;
CREATE FUNCTION public.rdiff(a int, b int) RETURNS float8 LANGUAGE plpgsql IMMUTABLE AS
  $$ BEGIN RETURN a - b; END $$;
CREATE TYPE public.rngd AS RANGE (subtype = int4, subtype_diff = public.rdiff);
CREATE TABLE public.ih (id int);
INSERT INTO public.ih VALUES (1);
CREATE FOREIGN TABLE public.ihc () INHERITS (public.ih) SERVER nowhere;
CREATE FUNCTION public.etf() RETURNS event_trigger LANGUAGE plpgsql AS
  $$ BEGIN PERFORM pg_catalog.nextval('public.hits'); END $$;
";

/// PostgreSQL 17 and newer: SQL/JSON functions that coerce to a domain.
const FIXTURES_17: &str = "
CREATE DOMAIN public.jdom AS jsonb CHECK (public.chk(1));
CREATE VIEW public.v_jv AS SELECT JSON_VALUE('{\"a\": 1}'::jsonb, '$.a' RETURNING public.dom) AS v;
CREATE VIEW public.v_jq AS SELECT JSON_QUERY('{\"a\": [1]}'::jsonb, '$.a' RETURNING public.jdom) AS v;
CREATE VIEW public.v_jt AS
  SELECT * FROM JSON_TABLE('{\"a\": 1}'::jsonb, '$' COLUMNS (a public.dom PATH '$.a')) j;
";

/// PostgreSQL 14 and newer: a view that reads a multirange of a domain by
/// type oid.
const FIXTURES_14: &str = "CREATE VIEW public.v_mrin AS \
     SELECT pg_catalog.multirange_in('{[1,2]}', 'public.rdom_multirange'::pg_catalog.regtype, -1) \
     IS NULL AS x";

/// PostgreSQL 16 and newer: a view that checks a value against a domain by
/// name, which runs the domain's checks.
const FIXTURES_16: &str = "CREATE VIEW public.v_valid AS \
     SELECT pg_catalog.pg_input_is_valid('1', 'public.dom') AS v";

/// The statements that must need confirmation, with a word the reasons
/// must contain.
const CONFIRM: &[(&str, &str)] = &[
    // H1: column definition lists, dblink.
    ("SELECT * FROM evilrec(1) AS (a bigint)", "evilrec"),
    (
        "SELECT * FROM ROWS FROM (evilrec(1) AS (a bigint))",
        "evilrec",
    ),
    (
        "SELECT * FROM dblink('dbname=nowhere', 'INSERT INTO x VALUES (1) RETURNING 1') AS (x int)",
        "dblink",
    ),
    // H2: functions named like unreserved keywords.
    ("SELECT text(t.a) FROM public.t", "text"),
    ("SELECT first(t.a) FROM public.t", "first"),
    ("SELECT \"delete\"(t.a) FROM public.t", "delete"),
    ("SELECT mode(t.a) FROM public.t", "mode"),
    ("SELECT key(t.a) FROM public.t", "key"),
    // H3: attribute notation, qualified typed literal.
    ("SELECT t.evil FROM public.t", "evil"),
    ("SELECT (t).evil FROM public.t", "evil"),
    ("SELECT public.dom '5'", "chk"),
    ("SELECT '5'::public.dom", "chk"),
    // Domains over domains, composites and ranges of a domain, arrays (a
    // range literal is not used: parsing one runs the subtype's input
    // function, domain check included, before any check can answer).
    ("SELECT '5'::public.dom2", "chk"),
    ("SELECT ROW(1)::public.cdom", "chk"),
    ("SELECT NULL::public.rdom", "chk"),
    ("SELECT ARRAY['1']::public.dom[]", "chk"),
    // A column definition list names a domain (its input runs the check).
    (
        "SELECT * FROM json_to_record('{\"a\": 1}') AS x(a public.dom)",
        "chk",
    ),
    // M1: views, casts, row level security.
    ("SELECT * FROM public.vlock", "advisory"),
    ("SELECT * FROM public.vseq", "nextval"),
    ("SELECT * FROM public.rls", "bump"),
    // A foreign table, also as a partition of a table the statement reads.
    ("SELECT * FROM public.ft", "foreign table"),
    ("SELECT * FROM public.partf", "foreign table"),
    // A sampling method that is not a built-in, written and in a view.
    (
        "SELECT * FROM public.t TABLESAMPLE system_rows(1)",
        "sampling",
    ),
    ("SELECT * FROM public.v_sample", "sampling"),
    // A user operator class (its compare function) reached through a range
    // type's subtype opclass (comparisons, ordering, containment, GREATEST,
    // JOIN USING, a literal), an index on a read table and a partition key.
    (
        "SELECT count(*) FROM public.trng a, public.trng b WHERE a.r && b.r",
        "uccmp",
    ),
    ("SELECT r FROM public.trng ORDER BY r", "uccmp"),
    ("SELECT r FROM public.trng WHERE r @> 7", "uccmp"),
    (
        "SELECT GREATEST(a.r, b.r) FROM public.trng a, public.trng b LIMIT 1",
        "uccmp",
    ),
    (
        "SELECT a.r = b.r FROM public.trng a, public.trng b LIMIT 3",
        "uccmp",
    ),
    (
        "SELECT * FROM public.trng a JOIN public.trng b USING (r)",
        "uccmp",
    ),
    ("SELECT '[1,5)'::public.rngo", "uccmp"),
    ("SELECT NULL::public.rngo", "uccmp"),
    ("SELECT * FROM public.tix WHERE a = 77", "uccmp"),
    (
        "SELECT * FROM public.tix WHERE a BETWEEN 10 AND 12",
        "uccmp",
    ),
    ("SELECT count(*) FROM public.pto WHERE a = 5", "uccmp"),
    (
        "SELECT count(*) FROM public.pto WHERE a < $$5$$::int",
        "uccmp",
    ),
    // Inheritance: the foreign child is read unless ONLY.
    ("SELECT * FROM public.ih", "foreign table"),
    // H1: built-ins that run other relations or a domain's checks, in a
    // view or written.
    ("SELECT * FROM public.v_xml", "table_to_xml"),
    ("SELECT * FROM public.v_schema", "schema_to_xml"),
    ("SELECT * FROM public.v_query", "query_to_xml"),
    (
        "SELECT pg_catalog.database_to_xml(true, false, '')",
        "database_to_xml",
    ),
    // Type input functions take the type as an oid and run its domain
    // checks, a type no name in the statement shows.
    ("SELECT * FROM public.v_din", "domain_in"),
    ("SELECT * FROM public.v_rin", "record_in"),
    ("SELECT * FROM public.v_ain", "array_in"),
    ("SELECT * FROM public.v_rgin", "range_in"),
    // Domain checks reached through syntax that names a type anywhere,
    // in a view (also a view over it) and in a cast inside a view.
    ("SELECT * FROM public.v_xt", "chk"),
    ("SELECT * FROM public.v_outer", "chk"),
    ("SELECT * FROM public.v_cast", "chk"),
    (
        "SELECT * FROM XMLTABLE('/r' PASSING ('<r><a>1</a></r>'::xml) COLUMNS a public.dom PATH 'a')",
        "",
    ),
    // A relation's row type used as a type runs its column domains' checks.
    ("SELECT s.v::public.t_dom FROM public.s", "chk"),
    ("SELECT s.v::t_dom FROM public.s", "chk"),
    ("SELECT CAST(s.v AS t_dom) FROM public.s", "chk"),
    ("SELECT ARRAY[s.v]::public.t_dom[] FROM public.s", "chk"),
    ("SELECT s.v::public.v_plain FROM public.s", "chk"),
    ("SELECT s.v::mv_plain FROM public.s", "chk"),
    // A user cast function, whatever the source type's spelling or origin
    // (an aggregate's result, a subquery column, a qualified target).
    ("SELECT count(*)::ct FROM public.tt", "to_ct"),
    ("SELECT CAST(count(*) AS ct) FROM public.tt", "to_ct"),
    (
        "SELECT s.x::ct FROM (SELECT count(*) AS x FROM public.tt) s",
        "to_ct",
    ),
    ("SELECT count(*)::public.ct FROM public.tt", "to_ct"),
    ("SELECT 1::bigint::ct", "to_ct"),
    // An extension operator (and its function) that fills the table's row
    // type through its columns' input functions (a column domain check),
    // and the built-in that does the same.
    (
        "SELECT x #= 'v=>bad'::hstore FROM public.td x",
        "populate_record",
    ),
    (
        "SELECT populate_record(x, 'v=>bad'::hstore) FROM public.td x",
        "populate_record",
    ),
    (
        "SELECT jsonb_populate_record(x, '{}'::jsonb) FROM public.td x",
        "jsonb_populate_record",
    ),
    // A user operator over built-in types, a user aggregate (also as a
    // window function).
    ("SELECT 1 === 2", "==="),
    ("SELECT mysum(t.a) FROM public.t", "mysum"),
    ("SELECT mysum(t.a) OVER () FROM public.t", "mysum"),
    // The text check already asks: nothing is planned (planning would fold
    // the immutable imm(1), which advances the sequence).
    ("SELECT pg_sleep(0), public.imm(1)", "pg_sleep"),
    // Row locks cannot be taken in a read-only run.
    ("SELECT * FROM public.t FOR UPDATE", "locks rows"),
    // Built-ins with effects outside the transaction.
    ("SELECT pg_catalog.pg_advisory_lock(1)", "advisory"),
    ("SELECT pg_sleep(0)", "pg_sleep"),
    ("SELECT nextval('public.hits')", "nextval"),
    // Changes data: never read-only.
    ("INSERT INTO public.trgt VALUES (1)", "changes data"),
    (
        "WITH x AS (DELETE FROM public.t RETURNING a) SELECT * FROM x",
        "changes data",
    ),
];

/// Statements proven safe: they run without confirmation.
const SAFE: &[&str] = &[
    "SELECT t.a, t.b FROM public.t WHERE t.a > 0 ORDER BY t.b",
    "SELECT pg_catalog.count(*), upper(t.b) FROM public.t GROUP BY 2",
    "SELECT * FROM generate_series(1, 3) g, public.t WHERE t.a IN (1, 2)",
    "WITH x (n) AS (SELECT 1) SELECT n, now(), random() FROM x",
    "SELECT max(c.relname), pg_catalog.count(*) FROM pg_catalog.pg_class c WHERE c.relname = 'pg_class'",
    // Ordered-set aggregates, alias column lists, built-in sampling,
    // operators and functions with SQL-language overloads, AT TIME ZONE.
    "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY t.a) FROM public.t",
    "SELECT * FROM generate_series(1, 3) g(i) JOIN public.t ON t.a = g.i",
    "SELECT * FROM public.t TABLESAMPLE BERNOULLI (50)",
    // A view over XMLTABLE with a domain whose check is a built-in.
    "SELECT * FROM public.v_safe_xt",
    // Operators and functions of trusted extensions, a built-in window
    // function, an ICU collation.
    "SELECT 'A'::citext = 'a'::citext",
    "SELECT similarity(t.b, 'x') FROM public.t",
    "SELECT 'a=>1'::hstore -> 'a'",
    "SELECT row_number() OVER (ORDER BY t.a) FROM public.t",
    "SELECT t.b FROM public.t ORDER BY t.b COLLATE \"und-x-icu\"",
    // Reading a table with a column of such a domain, a materialized view
    // (also one over a user function: only a refresh runs that) and a
    // table whose rules are not ON SELECT runs no user code.
    "SELECT * FROM public.t_dom",
    "SELECT * FROM public.mv_plain",
    "SELECT * FROM public.mv_noop",
    "SELECT * FROM public.ruled",
    "SELECT t.a + 1, round(avg(t.a), 2), extract(year FROM now()), now() AT TIME ZONE 'UTC' \
     FROM public.t GROUP BY t.a",
    // Row level security whose policy is built-in only, partitions, a
    // composite column, a user ICU collation, extension operators, a
    // record-returning extension function, a column definition list of
    // built-in types, catalog views over volatile built-ins that only read
    // server state, a view over a domain with a built-in check.
    "SELECT * FROM public.rls_ok",
    "SELECT * FROM public.part",
    "SELECT * FROM public.tc",
    "SELECT t.b FROM public.t ORDER BY t.b COLLATE public.myicu",
    "SELECT t.b % 'x', 'a'::citext = 'A', 'a=>1'::hstore ? 'a' FROM public.t",
    "SELECT * FROM each('a=>1'::hstore)",
    "SELECT * FROM json_to_record('{\"a\": 1}') AS x(a int)",
    "SELECT count(*) FROM pg_catalog.pg_locks",
    "SELECT count(*) FROM pg_catalog.pg_stat_activity",
    "SELECT '5'::public.posint, ARRAY[1]::public.posint[]",
    // A built-in index, a range whose only user function is its
    // subtype_diff (GiST only), FROM ONLY above a foreign child, and a
    // pg_catalog SQL function whose body holds words that are also names
    // of user functions (`first`, `mode`, ...: whole identifiers count).
    "SELECT * FROM public.tixd WHERE a = 77",
    "SELECT '[1,5)'::public.rngd",
    "SELECT public.rngd(1, 5)",
    "SELECT * FROM ONLY public.ih",
    "SELECT obj_description('public.t'::regclass, 'pg_class')",
];

fn analyze() -> ExplainOptions {
    ExplainOptions {
        analyze: true,
        ..ExplainOptions::default()
    }
}

/// The statement needs confirmation, names `word`, and `explain` refuses
/// it without confirmation.
async fn must_confirm(session: &dyn Session, statement: &str, word: &str, context: &str) -> R {
    let server = session.server_info().clone();
    match analyze_safety_checked(session, &server, statement).await {
        AnalyzeSafety::NeedsConfirmation { warnings } => {
            let all = warnings.join(" ");
            assert!(
                all.to_lowercase().contains(&word.to_lowercase()),
                "{context}: {statement}: the reasons do not mention {word}: {all}"
            );
        }
        other => return Err(format!("{context}: {statement}: {other:?}").into()),
    }
    let refused = explain(session, &server, statement, &analyze()).await;
    assert!(
        matches!(refused, Err(CatalogError::NeedsConfirmation { .. })),
        "{context}: {statement}: {refused:?}"
    );
    Ok(())
}

/// Nothing planted ran and no advisory lock is held.
async fn untouched(session: &dyn Session, context: &str) -> R {
    assert_eq!(
        texts(
            session,
            "SELECT is_called::pg_catalog.text FROM public.hits"
        )
        .await?,
        ["false"],
        "{context}: a planted function ran"
    );
    assert_eq!(
        texts(
            session,
            "SELECT pg_catalog.count(*)::pg_catalog.text FROM pg_catalog.pg_locks \
             WHERE locktype OPERATOR(pg_catalog.=) 'advisory' \
               AND pid OPERATOR(pg_catalog.=) pg_catalog.pg_backend_pid()"
        )
        .await?,
        ["0"],
        "{context}: an advisory lock is held"
    );
    Ok(())
}

async fn checks(port: u16, major: u32, search_path: &str) -> R {
    let session = connect(port, DATABASE).await?;
    let s = session.as_ref();
    exec(s, &format!("SET search_path = {search_path}")).await?;
    let context = format!("PostgreSQL {major}, search_path {search_path}");
    let server = s.server_info().clone();
    for (statement, word) in CONFIRM {
        must_confirm(s, statement, word, &context).await?;
    }
    if major >= 14 {
        must_confirm(s, "SELECT * FROM public.v_mrin", "multirange_in", &context).await?;
    }
    if major >= 18 {
        let server = s.server_info().clone();
        assert_eq!(
            analyze_safety_checked(s, &server, "SELECT * FROM public.vg").await,
            AnalyzeSafety::ReadOnly,
            "{context}: virtual generated column"
        );
        // (PostgreSQL 18 refuses user-defined functions and types in a
        // virtual generation expression, so only built-ins can be computed
        // on read; the check still walks every such expression.)
    }
    if major >= 17 {
        for view in ["v_jv", "v_jq", "v_jt"] {
            must_confirm(s, &format!("SELECT * FROM public.{view}"), "chk", &context).await?;
        }
        must_confirm(
            s,
            "SELECT JSON_VALUE('{\"a\": 1}'::jsonb, '$.a' RETURNING public.dom) AS v",
            "chk",
            &context,
        )
        .await?;
        must_confirm(
            s,
            "SELECT JSON_QUERY('{\"a\": [1]}'::jsonb, '$.a' RETURNING public.jdom) AS v",
            "chk",
            &context,
        )
        .await?;
        must_confirm(
            s,
            "SELECT JSON_QUERY('{\"a\": [1]}'::jsonb, '$.a' RETURNING public.dom[]) AS v",
            "chk",
            &context,
        )
        .await?;
        // Built-in and harmless types after RETURNING.
        for statement in [
            "SELECT JSON_VALUE('{\"a\": 1}'::jsonb, '$.a' RETURNING int) AS v",
            "SELECT JSON_VALUE('{\"a\": 1}'::jsonb, '$.a' RETURNING public.posint) AS v",
            "SELECT JSON_QUERY('{\"a\": [1]}'::jsonb, '$.a' RETURNING character varying) AS v",
            "SELECT JSON_OBJECT('a': 1 RETURNING jsonb) AS v",
        ] {
            assert_eq!(
                analyze_safety_checked(s, &server, statement).await,
                AnalyzeSafety::ReadOnly,
                "{context}: {statement}"
            );
        }
    }
    if major >= 16 {
        must_confirm(
            s,
            "SELECT * FROM public.v_valid",
            "pg_input_is_valid",
            &context,
        )
        .await?;
    }
    // A statement that cannot be planned: the check says so, and the
    // unconfirmed run returns the planning error.
    let server = s.server_info().clone();
    let unplanned = "SELECT nosuchcol FROM public.t";
    let verdict = analyze_safety_checked(s, &server, unplanned).await;
    assert!(
        matches!(&verdict, AnalyzeSafety::NeedsConfirmation { warnings }
            if warnings.iter().any(|w| w.contains("could not be planned")
                && w.contains("nosuchcol"))),
        "{context}: {verdict:?}"
    );
    let failed = explain(s, &server, unplanned, &analyze()).await;
    assert!(
        matches!(&failed, Err(CatalogError::Driver { error }) if error.message.contains("nosuchcol")),
        "{context}: {failed:?}"
    );

    // M2: an extension member added by a role that merely owns the
    // extension is user code.
    let planter = connect_as(port, DATABASE, PLANTER, PLANTER_PASSWORD).await?;
    let p = planter.as_ref();
    exec(p, "CREATE EXTENSION fuzzystrmatch").await?;
    exec(
        p,
        "CREATE FUNCTION public.lower(varchar) RETURNS text LANGUAGE sql VOLATILE AS \
         $$ SELECT pg_catalog.nextval('public.hits')::text $$",
    )
    .await?;
    exec(
        p,
        "ALTER EXTENSION fuzzystrmatch ADD FUNCTION public.lower(varchar)",
    )
    .await?;
    must_confirm(s, "SELECT lower(t.c) FROM public.t", "lower", &context).await?;
    exec(p, "DROP EXTENSION fuzzystrmatch CASCADE").await?;
    planter.close().await?;

    // Statements that read only built-ins run without confirmation, in a
    // read-only transaction.
    let server = s.server_info().clone();
    for statement in SAFE {
        assert_eq!(
            analyze_safety_checked(s, &server, statement).await,
            AnalyzeSafety::ReadOnly,
            "{context}: {statement}"
        );
        let plan = explain(s, &server, statement, &analyze())
            .await
            .map_err(|e| format!("{context}: {statement}: {e}"))?;
        assert!(plan.execution_ms.is_some(), "{context}: {statement}");
    }

    // A read-only connection cannot create the temporary view the check
    // resolves names with (its driver keeps every statement read-only), so
    // every statement asks there, with that reason; the confirmed run
    // still works and is read-only.
    let read_only = connect_with(
        port,
        DATABASE,
        &env_or("VARVE_PG_USER", "varve"),
        &env_or("VARVE_PG_PASSWORD", "varve_dev"),
        true,
    )
    .await?;
    for statement in SAFE.iter().take(3) {
        must_confirm(read_only.as_ref(), statement, "read-only", &context).await?;
        let plan = explain(
            read_only.as_ref(),
            &server,
            statement,
            &ExplainOptions {
                confirmed: true,
                ..analyze()
            },
        )
        .await
        .map_err(|e| format!("{context}: read-only connection: {statement}: {e}"))?;
        assert!(plan.execution_ms.is_some(), "{context}: {statement}");
    }
    must_confirm(
        read_only.as_ref(),
        "SELECT * FROM public.vseq",
        "read-only",
        &context,
    )
    .await?;
    read_only.close().await?;

    // A temporary table (PostgreSQL 18 plans name its schema pg_temp) and a
    // table that shares its name with a domain elsewhere in the
    // search_path read without confirmation.
    exec(
        s,
        "CREATE TEMP TABLE tmp_read (a int); INSERT INTO tmp_read VALUES (1)",
    )
    .await?;
    exec(s, &format!("SET search_path = {search_path}, twinschema")).await?;
    for statement in [
        "SELECT * FROM tmp_read",
        "SELECT t.a FROM pg_temp.tmp_read t",
        "SELECT * FROM twin",
        "SELECT twin.a FROM public.twin",
    ] {
        assert_eq!(
            analyze_safety_checked(s, &server, statement).await,
            AnalyzeSafety::ReadOnly,
            "{context}: {statement}"
        );
    }
    // The domain itself still asks.
    must_confirm(s, "SELECT '5'::twinschema.twin", "chk", &context).await?;
    // A user cast to a common type: every statement naming the type asks
    // while it exists; an implicit or assignment one, every statement.
    exec(
        s,
        "CREATE CAST (public.ct AS text) WITH FUNCTION public.ct_text(public.ct)",
    )
    .await?;
    let casted = async {
        must_confirm(s, "SELECT k::text FROM public.tc", "ct_text", &context).await?;
        must_confirm(
            s,
            "SELECT CAST(k AS text) FROM public.tc",
            "ct_text",
            &context,
        )
        .await?;
        // The column itself reads without the cast.
        assert_eq!(
            analyze_safety_checked(s, &server, "SELECT k FROM public.tc").await,
            AnalyzeSafety::ReadOnly,
            "{context}"
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    exec(s, "DROP CAST (public.ct AS text)").await?;
    casted?;
    // An implicit or assignment cast: the statements that use it ask, the
    // ones that do not still run.
    for context_kind in ["IMPLICIT", "ASSIGNMENT"] {
        exec(
            s,
            &format!(
                "CREATE CAST (public.ct AS int) WITH FUNCTION public.ct_int(public.ct) \
                 AS {context_kind}"
            ),
        )
        .await?;
        let casted = async {
            must_confirm(
                s,
                "SELECT CAST(k AS int) FROM public.tc",
                "ct_int",
                &context,
            )
            .await?;
            if context_kind == "IMPLICIT" {
                must_confirm(s, "SELECT k + 1 FROM public.tc", "ct_int", &context).await?;
            }
            assert_eq!(
                analyze_safety_checked(s, &server, "SELECT t.a FROM public.t").await,
                AnalyzeSafety::ReadOnly,
                "{context}: {context_kind} cast"
            );
            Ok::<(), Box<dyn std::error::Error>>(())
        }
        .await;
        exec(s, "DROP CAST (public.ct AS int)").await?;
        casted?;
    }
    // A word that names the table read is still resolved on its own when
    // used as a type: here it finds a domain earlier in the search_path
    // (also an array of it, and a domain over an enum); an enum that
    // shadows a table is harmless.
    exec(s, "SET search_path = r12a, r12b").await?;
    for statement in [
        "SELECT a FROM t WHERE 'x'::t IS NOT NULL",
        "SELECT CAST('x' AS t) FROM t",
        "SELECT a FROM r12b.t WHERE 'x'::t IS NOT NULL",
        "SELECT a::text::t FROM t",
        "SELECT a FROM t WHERE ARRAY[a]::t[] IS NOT NULL",
        "SELECT a FROM dm WHERE 'x'::dm IS NOT NULL",
        "SELECT a::r12a.mood::dm FROM dm",
    ] {
        must_confirm(s, statement, "chk", &context).await?;
    }
    // Reading the table that shares the domain's name is a read of the
    // table, whatever the search_path holds.
    for statement in [
        "SELECT a FROM t",
        "SELECT a FROM dm",
        "SELECT a FROM mood WHERE 'x'::mood IS NOT NULL",
        "SELECT a FROM mood WHERE ARRAY[a]::mood[] IS NOT NULL",
    ] {
        assert_eq!(
            analyze_safety_checked(s, &server, statement).await,
            AnalyzeSafety::ReadOnly,
            "{context}: {statement}"
        );
    }
    exec(s, &format!("SET search_path = {search_path}")).await?;
    exec(s, "DROP TABLE tmp_read").await?;

    // An event trigger that fires for CREATE VIEW would run during the
    // check (its temporary view): every statement asks, and the trigger
    // never ran (`untouched` below); one whose tags do not include CREATE
    // VIEW does not matter.
    exec(
        s,
        "CREATE EVENT TRIGGER m2_pgsafety_et ON ddl_command_end EXECUTE FUNCTION public.etf()",
    )
    .await?;
    let gated = async {
        must_confirm(s, "SELECT 1", "event trigger", &context).await?;
        must_confirm(s, SAFE[0], "event trigger", &context).await
    }
    .await;
    exec(s, "DROP EVENT TRIGGER m2_pgsafety_et").await?;
    gated?;
    exec(
        s,
        "CREATE EVENT TRIGGER m2_pgsafety_et ON ddl_command_end WHEN TAG IN ('CREATE TABLE') \
         EXECUTE FUNCTION public.etf()",
    )
    .await?;
    let verdict = analyze_safety_checked(s, &server, SAFE[0]).await;
    exec(s, "DROP EVENT TRIGGER m2_pgsafety_et").await?;
    assert_eq!(
        verdict,
        AnalyzeSafety::ReadOnly,
        "{context}: event trigger on other tags"
    );

    // Inside the user's transaction nothing runs without confirmation.
    exec(s, "BEGIN").await?;
    let inside = analyze_safety_checked(s, &server, SAFE[0]).await;
    let still = texts(s, "SELECT 'usable'").await;
    exec(s, "ROLLBACK").await?;
    assert!(
        matches!(&inside, AnalyzeSafety::NeedsConfirmation { warnings }
            if warnings.iter().any(|w| w.contains("inside a transaction"))),
        "{context}: {inside:?}"
    );
    assert_eq!(still?, ["usable"], "{context}");

    failures_are_reported(s, &context).await?;
    confirmed_runs_keep_the_transaction(s, &context).await?;
    one_cancel_stops_the_explain(s, &context).await?;
    large_plans_arrive(s, &context).await?;
    dropped_calls_leave_nothing_open(s, &context).await?;
    untouched(s, &context).await?;
    session.close().await?;
    Ok(())
}

/// A proven statement whose run fails returns that failure (a run-time
/// error, a statement timeout, a cancel), never a confirmation request,
/// and leaves the session as it was.
async fn failures_are_reported(s: &dyn Session, context: &str) -> R {
    let server = s.server_info().clone();
    let failing = explain(
        s,
        &server,
        "SELECT 1 / (t.a - t.a) FROM public.t",
        &analyze(),
    )
    .await;
    assert!(
        matches!(&failing, Err(CatalogError::Driver { error }) if error.message.contains("division by zero")),
        "{context}: {failing:?}"
    );
    let slow = "SELECT pg_catalog.count(*) FROM pg_catalog.generate_series(1, 400000000)";
    exec(s, "SET statement_timeout = '300ms'").await?;
    let timed_out = explain(s, &server, slow, &analyze()).await;
    exec(s, "RESET statement_timeout").await?;
    assert!(
        matches!(&timed_out, Err(CatalogError::Driver { error })
            if error.kind == ErrorKind::Timeout || error.message.contains("statement timeout")),
        "{context}: {timed_out:?}"
    );
    let handle = s.cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        handle.cancel().await
    });
    let cancelled = explain(s, &server, slow, &analyze()).await;
    let _ = canceller.await;
    assert!(
        matches!(&cancelled, Err(CatalogError::Driver { error })
            if error.kind == ErrorKind::Cancelled || error.message.contains("user request")),
        "{context}: {cancelled:?}"
    );
    assert_eq!(s.in_transaction(), Some(false), "{context}");
    exec(
        s,
        "INSERT INTO public.t VALUES (77, 'w', 'w'); DELETE FROM public.t WHERE a = 77",
    )
    .await?;
    Ok(())
}

/// Inside the user's transaction a confirmed run that fails (an error, a
/// cancel) leaves the transaction usable with its earlier changes, and a
/// run leaves the session's settings as they were.
async fn confirmed_runs_keep_the_transaction(s: &dyn Session, context: &str) -> R {
    let server = s.server_info().clone();
    let confirmed = ExplainOptions {
        confirmed: true,
        ..analyze()
    };
    exec(s, "BEGIN").await?;
    let checked = async {
        exec(s, "INSERT INTO public.t VALUES (55, 'tx', 'tx')").await?;
        exec(s, "SET LOCAL client_min_messages = warning").await?;
        let failing = explain(
            s,
            &server,
            "SELECT 1 / (t.a - t.a) FROM public.t",
            &confirmed,
        )
        .await;
        assert!(
            matches!(&failing, Err(CatalogError::Driver { error })
                if error.message.contains("division by zero")),
            "{context}: {failing:?}"
        );
        let slow = "SELECT pg_catalog.count(*) FROM pg_catalog.generate_series(1, 400000000)";
        let handle = s.cancel_handle();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            handle.cancel().await
        });
        let cancelled = explain(s, &server, slow, &confirmed).await;
        let _ = canceller.await;
        assert!(
            matches!(&cancelled, Err(CatalogError::Driver { error })
                if error.kind == ErrorKind::Cancelled || error.message.contains("user request")),
            "{context}: {cancelled:?}"
        );
        let plan = explain(s, &server, SAFE[0], &confirmed)
            .await
            .map_err(|e| format!("{context}: confirmed run in a transaction: {e}"))?;
        assert!(plan.execution_ms.is_some(), "{context}");
        assert_eq!(
            texts(
                s,
                "SELECT pg_catalog.count(*)::pg_catalog.text FROM public.t WHERE a = 55"
            )
            .await?,
            ["1"],
            "{context}: the transaction lost its row"
        );
        assert_eq!(
            texts(s, "SHOW client_min_messages").await?,
            ["warning"],
            "{context}: a run changed client_min_messages"
        );
        assert_eq!(s.in_transaction(), Some(true), "{context}");
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    exec(s, "ROLLBACK").await?;
    checked
}

/// One cancel stops the whole explain wherever it lands among its driver
/// calls (the check, the run, a savepoint): the explain returns a cancel
/// quickly and leaves the session (and the user's transaction) as it was.
async fn one_cancel_stops_the_explain(s: &dyn Session, context: &str) -> R {
    let server = s.server_info().clone();
    let slow = "SELECT pg_catalog.count(*) FROM pg_catalog.generate_series(1, 400000000)";
    let confirmed = ExplainOptions {
        confirmed: true,
        ..analyze()
    };
    for in_transaction in [false, true] {
        for delay in [0u64, 1, 2, 3, 4, 5, 6, 8, 10, 15, 25, 50, 100] {
            let how = format!("{context}: in transaction {in_transaction}, cancel at {delay} ms");
            if in_transaction {
                exec(s, "BEGIN").await?;
                exec(s, "INSERT INTO public.t VALUES (56, 'c', 'c')").await?;
            }
            let options = if in_transaction {
                &confirmed
            } else {
                &analyze()
            };
            let cancel = ExplainCancel::new();
            let trigger = cancel.clone();
            let canceller = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                trigger.cancel();
            });
            let started = Instant::now();
            let outcome = tokio::time::timeout(
                Duration::from_secs(10),
                explain_with_cancel(s, &server, slow, options, &cancel),
            )
            .await;
            let _ = canceller.await;
            let Ok(outcome) = outcome else {
                s.cancel_handle().cancel().await?;
                return Err(format!("{how}: the explain ran on").into());
            };
            assert!(
                matches!(&outcome, Err(CatalogError::Driver { error })
                    if error.kind == ErrorKind::Cancelled),
                "{how}: {outcome:?}"
            );
            assert!(started.elapsed() < Duration::from_secs(5), "{how}");
            if in_transaction {
                let kept = texts(
                    s,
                    "SELECT pg_catalog.count(*)::pg_catalog.text FROM public.t WHERE a = 56",
                )
                .await;
                exec(s, "ROLLBACK").await?;
                assert_eq!(kept?, ["1"], "{how}: the transaction lost its row");
            }
            assert_eq!(s.in_transaction(), Some(false), "{how}");
        }
    }
    Ok(())
}

/// A plan larger than the driver keeps of a call's notices (1 MiB) still
/// arrives: proven and confirmed, outside and inside a transaction.
async fn large_plans_arrive(s: &dyn Session, context: &str) -> R {
    let server = s.server_info().clone();
    let literal = "x".repeat(1_200_000);
    let statement = format!("SELECT t.a FROM public.t WHERE t.b <> '{literal}'");
    let confirmed = ExplainOptions {
        confirmed: true,
        ..analyze()
    };
    let check = |plan: varve_catalog::CatalogResult<varve_catalog::explain::Plan>, how: &str| {
        let plan = plan.map_err(|e| format!("{context}: {how}: {e}"))?;
        assert!(plan.execution_ms.is_some(), "{context}: {how}");
        Ok::<(), Box<dyn std::error::Error>>(())
    };
    check(explain(s, &server, &statement, &analyze()).await, "proven")?;
    check(
        explain(s, &server, &statement, &confirmed).await,
        "confirmed",
    )?;
    exec(s, "BEGIN").await?;
    let inside = explain(s, &server, &statement, &confirmed).await;
    exec(s, "ROLLBACK").await?;
    check(inside, "confirmed in a transaction")?;
    // The setting that carried the plan is reset.
    assert_eq!(
        texts(
            s,
            "SELECT COALESCE(pg_catalog.current_setting('varve.explain_output', true), '')"
        )
        .await?,
        [""],
        "{context}"
    );
    Ok(())
}

/// Dropping `explain()` at any point leaves no transaction of Varve's
/// open: the next statement runs outside a transaction and may write.
async fn dropped_calls_leave_nothing_open(s: &dyn Session, context: &str) -> R {
    let server = s.server_info().clone();
    for step in 0..40u64 {
        let _ = tokio::time::timeout(
            Duration::from_micros(250 * step),
            explain(s, &server, SAFE[0], &analyze()),
        )
        .await;
        exec(s, "INSERT INTO public.t VALUES (99, 'drop', 'drop')")
            .await
            .map_err(|e| format!("{context}: after a drop at step {step}: {e}"))?;
        assert_eq!(s.in_transaction(), Some(false), "{context}: step {step}");
    }
    exec(s, "DELETE FROM public.t WHERE a = 99").await?;
    Ok(())
}

/// Every audit probe needs confirmation and runs nothing; safe statements
/// still run; on 13 and 18, with the default `search_path` and with the
/// planted schema first.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn analyze_safety_fails_closed() -> R {
    for (major, port) in servers() {
        let admin = connect(port, &env_or("VARVE_PG_DATABASE", "varve")).await?;
        cleanup(admin.as_ref()).await?;
        exec(admin.as_ref(), &format!("CREATE DATABASE {DATABASE}")).await?;
        exec(
            admin.as_ref(),
            &format!("CREATE ROLE {PLANTER} LOGIN PASSWORD '{PLANTER_PASSWORD}'"),
        )
        .await?;
        exec(
            admin.as_ref(),
            &format!("GRANT CREATE ON DATABASE {DATABASE} TO {PLANTER}"),
        )
        .await?;
        let checked = async {
            let setup = connect(port, DATABASE).await?;
            exec(setup.as_ref(), FIXTURES).await?;
            if major >= 14 {
                exec(setup.as_ref(), FIXTURES_14).await?;
            }
            if major >= 16 {
                exec(setup.as_ref(), FIXTURES_16).await?;
            }
            if major >= 17 {
                exec(setup.as_ref(), FIXTURES_17).await?;
            }
            if major >= 18 {
                exec(
                    setup.as_ref(),
                    "CREATE TABLE public.vg (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL); \
                     INSERT INTO public.vg (a) VALUES (1)",
                )
                .await?;
            }
            exec(
                setup.as_ref(),
                &format!(
                    "GRANT CREATE, USAGE ON SCHEMA public TO {PLANTER}; \
                     GRANT USAGE, UPDATE ON SEQUENCE public.hits TO {PLANTER}"
                ),
            )
            .await?;
            setup.close().await?;
            for search_path in ["\"$user\", public", "public, pg_catalog"] {
                checks(port, major, search_path).await?;
            }
            Ok::<(), Box<dyn std::error::Error>>(())
        }
        .await;
        cleanup(admin.as_ref()).await?;
        admin.close().await?;
        checked?;
    }
    Ok(())
}

/// What the check costs per `EXPLAIN ANALYZE` (PostgreSQL 18, a safe
/// statement): the bound guards against a check that grows into many round
/// trips or a slow catalog query.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn the_check_is_cheap() -> R {
    for (major, port) in servers() {
        let session = connect(port, &env_or("VARVE_PG_DATABASE", "varve")).await?;
        let s = session.as_ref();
        let server = s.server_info().clone();
        // A catalog read, and a pg_catalog SQL function (on 13 its body is
        // text that the lookup splits into words once, so a search_path
        // holding thousands of functions costs a hash join, not a regular
        // expression per function).
        for statement in [
            "SELECT c.relname, upper(c.relname) FROM pg_catalog.pg_class c \
             WHERE c.relkind = 'r' ORDER BY 1 LIMIT 5",
            "SELECT obj_description('pg_catalog.pg_class'::regclass, 'pg_class')",
        ] {
            // Warm up the caches.
            assert_eq!(
                analyze_safety_checked(s, &server, statement).await,
                AnalyzeSafety::ReadOnly,
                "PostgreSQL {major}: {statement}"
            );
            let runs = 20u32;
            let started = Instant::now();
            for _ in 0..runs {
                analyze_safety_checked(s, &server, statement).await;
            }
            let per_check = started.elapsed() / runs;
            eprintln!("PostgreSQL {major}: {per_check:?} per check: {statement}");
            assert!(
                per_check < Duration::from_millis(250),
                "PostgreSQL {major}: the check took {per_check:?}: {statement}"
            );
        }
        session.close().await?;
    }
    Ok(())
}
