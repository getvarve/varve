//! Name hijacking tests for every PostgreSQL statement of the live server
//! panels: the process list, the own connection id, cancel and terminate,
//! the status summary (including both replication queries, on the
//! PostgreSQL 18 streaming pair), server variables and their edit plans
//! (`SET`, `RESET`, `ALTER SYSTEM RESET` with the configuration reload):
//!
//! ```sh
//! cargo test -p varve-live-stats --test pg_hijack -- --include-ignored
//! ```
//!
//! Every function, aggregate and operator of `pg_catalog` gets an
//! identical-signature copy in `public`, base types get domains of the same
//! name and catalog relations views of the same name, each advancing a
//! sequence and raising `HIJACKED`. Everything then runs with
//! `search_path` set to `"$user", public` (better matches win) and to
//! `public, pg_catalog` (identical signatures win). Nothing planted may
//! run. The test database `m2_pghijack_live` is dropped before and after.
//!
//! Endpoints: `VARVE_M2_HOST` (`127.0.0.1`), `VARVE_M2_PG13_PORT` (55513),
//! `VARVE_M2_PG18_PORT` (55518), `VARVE_M2_PG_PRIMARY_PORT` (55611) and
//! `VARVE_M2_PG_REPLICA_PORT` (55612), all as the superuser `varve` with
//! password `VARVE_M2_PASSWORD` (`varve_dev`).

use std::collections::BTreeMap;
use std::error::Error;
use std::time::{Duration, Instant};

use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, ExecOptions, ExecOutput, Session, StatementResult, TlsMode, Value,
};
use varve_driver_postgres::PostgresDriver;
use varve_live_stats::sql::{
    EditScope, KillTarget, ReplicationRole, VariableChange, kill, own_connection_id,
    plan_variable_change, process_list, run_edit_plan, server_status, server_variable,
    server_variables,
};
use varve_secrets::Secret;

type R<T = ()> = Result<T, Box<dyn Error>>;

const DATABASE: &str = "m2_pghijack_live";
/// The sequence every planted object advances before it raises; sequence
/// advances survive the error and the rollback that follows it.
const HIJACK_COUNTER: &str = "public.m2_pghijack_hijacked";
/// The two `search_path` settings every check runs under.
const SEARCH_PATHS: [&str; 2] = ["\"$user\", public", "public, pg_catalog"];
/// A `user` context setting nothing else in the matrix sets, reset server
/// wide (an `ALTER SYSTEM RESET` of a setting that is not in
/// `postgresql.auto.conf` changes nothing).
const GLOBAL_SETTING: &str = "vacuum_cost_page_miss";

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

/// A server and the superuser to connect as.
#[derive(Debug, Clone, Copy)]
struct Server {
    label: &'static str,
    port: u16,
    user: &'static str,
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

/// PostgreSQL 13 and 18 (see [`selected`]), never none.
fn standalone() -> Vec<Server> {
    let servers: Vec<Server> = vec![
        Server {
            label: "PostgreSQL 13",
            port: port("VARVE_M2_PG13_PORT", 55513),
            user: "varve",
        },
        Server {
            label: "PostgreSQL 18",
            port: port("VARVE_M2_PG18_PORT", 55518),
            user: "varve",
        },
    ]
    .into_iter()
    .filter(|server| selected(&server.label.replace("PostgreSQL ", "postgres-")))
    .collect();
    assert!(
        !servers.is_empty(),
        "VARVE_ONLY_SERVERS selects no PostgreSQL server"
    );
    servers
}

fn primary() -> Server {
    Server {
        label: "PostgreSQL 18 primary",
        port: port("VARVE_M2_PG_PRIMARY_PORT", 55611),
        user: "varve",
    }
}

fn replica() -> Server {
    Server {
        label: "PostgreSQL 18 standby",
        port: port("VARVE_M2_PG_REPLICA_PORT", 55612),
        user: "varve",
    }
}

async fn connect(server: Server, database: &str) -> R<Box<dyn Session>> {
    let host = std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let password = std::env::var("VARVE_M2_PASSWORD").unwrap_or_else(|_| "varve_dev".into());
    let mut params = ConnectParams::new(Engine::Postgres, host, server.port);
    params.username = Some(server.user.into());
    params.password = Some(Secret::new(password));
    params.database = Some(database.into());
    params.tls.mode = TlsMode::Disable;
    params.connect_timeout = Duration::from_secs(10);
    params.application_name = "varve-live-stats pg_hijack tests".into();
    PostgresDriver::new()
        .connect(&params)
        .await
        .map_err(|e| format!("{}: connect to {database}: {e}", server.label).into())
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

/// Drops the test database from the `varve` database (the helpers' own
/// SQL never runs inside the planted database's `search_path`).
async fn drop_database(server: Server) -> R {
    let admin = connect(server, "varve").await?;
    exec(
        admin.as_ref(),
        &format!("DROP DATABASE IF EXISTS {DATABASE} WITH (FORCE)"),
    )
    .await?;
    admin.close().await?;
    Ok(())
}

/// Creates the test database and plants the hijacks; checks the counts.
async fn create_planted(server: Server) -> R {
    drop_database(server).await?;
    let admin = connect(server, "varve").await?;
    exec(admin.as_ref(), &format!("CREATE DATABASE {DATABASE}")).await?;
    admin.close().await?;
    let setup = connect(server, DATABASE).await?;
    exec(setup.as_ref(), &plant_hijacks()).await?;
    let output = exec(
        setup.as_ref(),
        "SELECT what, n FROM public.m2_pghijack_planted",
    )
    .await?;
    setup.close().await?;
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
    for (what, at_least) in [
        ("functions", 1500),
        ("aggregates", 40),
        ("operators", 500),
        ("better matches", 6),
        ("types", 40),
        ("relations", 60),
    ] {
        let n = planted.get(what).copied().unwrap_or(0);
        assert!(n >= at_least, "{}: only {n} {what} planted", server.label);
    }
    Ok(())
}

async fn set_search_path(server: Server, search_path: &str) -> R {
    let admin = connect(server, "varve").await?;
    exec(
        admin.as_ref(),
        &format!("ALTER DATABASE {DATABASE} SET search_path = {search_path}"),
    )
    .await?;
    admin.close().await?;
    Ok(())
}

/// Connects to the test database and checks that `search_path` is set.
async fn planted_session(server: Server, search_path: &str) -> R<Box<dyn Session>> {
    let session = connect(server, DATABASE).await?;
    let shown = texts(session.as_ref(), "SHOW search_path").await?;
    if shown != [search_path] {
        return Err(format!("{}: search_path is {shown:?}", server.label).into());
    }
    Ok(session)
}

/// True while no planted object has run.
async fn counter_untouched(server: Server) -> R<bool> {
    let session = connect(server, DATABASE).await?;
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
        ('FUNCTION public.lower(pg_catalog.name) RETURNS pg_catalog.text'),
        ('FUNCTION public.host(pg_catalog.text) RETURNS pg_catalog.text'),
        ('FUNCTION public.current_setting(pg_catalog.name) RETURNS pg_catalog.text'),
        ('FUNCTION public.has_database_privilege(pg_catalog.oid, pg_catalog.name) RETURNS pg_catalog.bool'),
        ('FUNCTION public.date_part(pg_catalog.name, pg_catalog.interval) RETURNS pg_catalog.float8')
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

/// Every panel call on a standalone server: process list, own id, cancel
/// and terminate of a second connection, status, variables and the three
/// kinds of variable edit plans.
async fn panels(server: Server, session: &dyn Session) -> R {
    let info = session.server_info().clone();
    let list = process_list(session, &info).await?;
    let own = own_connection_id(session, &info).await?;
    assert_eq!(list.own_id, Some(own), "{list:?}");

    let victim = connect(server, DATABASE).await?;
    let pid: i64 = texts(
        victim.as_ref(),
        "SELECT pg_catalog.pg_backend_pid()::pg_catalog.text",
    )
    .await?
    .first()
    .ok_or("no backend pid")?
    .parse()?;
    let sleeper = tokio::spawn(async move {
        let result = victim
            .execute("SELECT pg_catalog.pg_sleep(30)", &ExecOptions::default())
            .await;
        (victim, result)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = process_list(session, &info).await?;
        let active = list
            .processes
            .iter()
            .any(|p| p.id == pid && p.state.as_deref() == Some("active"));
        if active {
            break;
        }
        if Instant::now() > deadline {
            return Err(format!("backend {pid} never ran its sleep: {list:?}").into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let cancelled = kill(session, &info, pid, KillTarget::Query).await?;
    assert!(cancelled.signalled, "{cancelled:?}");
    let (victim, result) = sleeper.await?;
    assert!(result.is_err(), "the sleep was not cancelled");
    let terminated = kill(session, &info, pid, KillTarget::Connection).await?;
    assert!(terminated.signalled, "{terminated:?}");
    let _ = victim.close().await;

    let status = server_status(session, &info).await?;
    assert!(status.version_detail.is_some(), "{status:?}");
    assert!(status.uptime_seconds.is_some(), "{status:?}");
    assert!(status.max_connections.is_some(), "{status:?}");
    assert!(status.connections.is_some(), "{status:?}");
    assert!(
        status
            .databases
            .iter()
            .any(|d| d.name == DATABASE && d.size_bytes.is_some()),
        "{status:?}"
    );

    let all = server_variables(session, &info).await?;
    assert!(all.len() > 200, "only {} variables", all.len());
    let work_mem = server_variable(session, &info, "WORK_MEM").await?;
    assert_eq!(work_mem.name, "work_mem");
    for change in [
        VariableChange::Set {
            scope: EditScope::Session,
            value: "8MB".into(),
        },
        VariableChange::Reset {
            scope: EditScope::Session,
        },
    ] {
        let plan = plan_variable_change(session, &info, "work_mem", &change).await?;
        run_edit_plan(session, &plan).await?;
    }
    let reset = plan_variable_change(
        session,
        &info,
        GLOBAL_SETTING,
        &VariableChange::Reset {
            scope: EditScope::Global,
        },
    )
    .await?;
    assert!(
        reset
            .statements
            .iter()
            .any(|s| s == "SELECT pg_catalog.pg_reload_conf()"),
        "{reset:?}"
    );
    run_edit_plan(session, &reset).await?;
    Ok(())
}

/// The process list, kill, status and variable panels never run a planted
/// function, aggregate, operator, type or relation: on 13 and 18, with the
/// default `search_path` and with `public` before `pg_catalog`. A control
/// at the end proves the planted objects do run when SQL is unqualified.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn panels_never_run_planted_objects() -> R {
    for server in standalone() {
        create_planted(server).await?;
        for search_path in SEARCH_PATHS {
            set_search_path(server, search_path).await?;
            let session = planted_session(server, search_path).await?;
            let checked = panels(server, session.as_ref()).await;
            session.close().await?;
            checked.map_err(|e| format!("{}, search_path {search_path}: {e}", server.label))?;
            assert!(
                counter_untouched(server).await?,
                "{}, search_path {search_path}: a planted object ran",
                server.label
            );
        }

        // Control: unqualified SQL in the same setup runs a planted operator.
        let session = connect(server, DATABASE).await?;
        let control = exec(session.as_ref(), "SELECT 1 = 1").await;
        session.close().await?;
        let message = control.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("HIJACKED"),
            "{}: the control did not hit a planted operator: {message:?}",
            server.label
        );
        assert!(!counter_untouched(server).await?);

        drop_database(server).await?;
    }
    Ok(())
}

/// Both replication queries (the primary's replicas, the standby's
/// upstream) on the PostgreSQL 18 streaming pair. The planted database is
/// created on the primary and replayed by the standby; there a planted
/// object fails on its `nextval` (read-only), so it shows as an error.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn replication_status_never_runs_planted_objects() -> R {
    let primary = primary();
    let replica = replica();
    create_planted(primary).await?;
    let checked = async {
        for search_path in SEARCH_PATHS {
            set_search_path(primary, search_path).await?;
            // The standby replays the database, the plant and the setting.
            let deadline = Instant::now() + Duration::from_secs(30);
            let standby = loop {
                match planted_session(replica, search_path).await {
                    Ok(session) => break session,
                    Err(_) if Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    Err(error) => return Err(error),
                }
            };
            let session = planted_session(primary, search_path).await?;
            let status = server_status(session.as_ref(), session.server_info()).await;
            session.close().await?;
            let status = status.map_err(|e| format!("primary, {search_path}: {e}"))?;
            assert_eq!(status.replication.role, ReplicationRole::Primary);
            assert!(
                !status.replication.replicas.is_empty(),
                "{:?}",
                status.replication
            );
            let info = standby.server_info().clone();
            let status = server_status(standby.as_ref(), &info).await;
            standby.close().await?;
            let status = status.map_err(|e| format!("standby, {search_path}: {e}"))?;
            assert_eq!(status.replication.role, ReplicationRole::Replica);
            assert!(
                !status.replication.upstream.is_empty(),
                "{:?}",
                status.replication
            );
            assert!(
                counter_untouched(primary).await?,
                "{search_path}: a planted object ran"
            );
        }
        Ok::<(), Box<dyn Error>>(())
    }
    .await;
    drop_database(primary).await?;
    checked
}
