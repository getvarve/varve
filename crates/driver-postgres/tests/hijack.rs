//! The driver's own SQL never runs objects a user put in front of
//! `pg_catalog` in the `search_path`. Against the PostgreSQL servers of
//! `dev/docker-compose.yml`:
//!
//! ```sh
//! docker compose -p varve-m1-pg -f dev/docker-compose.yml up -d --wait postgres-13 postgres-18
//! cargo test -p varve-driver-postgres --test hijack -- --include-ignored
//! ```
//!
//! Each run creates the database `varve_hijack`, plants operators,
//! functions, an aggregate and domains in its `public` schema that raise an
//! exception, and opens sessions whose `search_path` is
//! `public, pg_catalog`. The settings checks, statement timeout handling,
//! read-only checks, reconnects, multirange lookups and cancels must all
//! work without running any of them.
//!
//! That includes tokio-postgres's own type lookup queries, run whenever a
//! statement returns a type it does not know (an enum, a composite, a
//! domain, a range): the vendored copy (`vendor/tokio-postgres`, see its
//! `VARVE-PATCH.md`) qualifies them, so the planted `=(oid, oid)`,
//! `>(int2, int4)` and `>(int2, int2)` never run.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestResult, connect, params, run, servers};
use varve_driver::{ErrorKind, Session};

/// What is planted. Every planted function raises `planted object ran`.
const PLANT: &str = r#"
CREATE FUNCTION public.planted_bool(pg_catalog.text, pg_catalog.text) RETURNS pg_catalog.bool
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'planted object ran'; END $$;
CREATE FUNCTION public.planted_count(pg_catalog.int8, pg_catalog.int4) RETURNS pg_catalog.bool
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'planted object ran'; END $$;
CREATE FUNCTION public.planted_step(pg_catalog.int8) RETURNS pg_catalog.int8
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'planted object ran'; END $$;
CREATE FUNCTION public.planted_check(pg_catalog.text) RETURNS pg_catalog.bool
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'planted object ran'; END $$;
CREATE OPERATOR public.= (LEFTARG = pg_catalog.text, RIGHTARG = pg_catalog.text,
    FUNCTION = public.planted_bool);
CREATE OPERATOR public.<> (LEFTARG = pg_catalog.text, RIGHTARG = pg_catalog.text,
    FUNCTION = public.planted_bool);
CREATE OPERATOR public.= (LEFTARG = pg_catalog.int8, RIGHTARG = pg_catalog.int4,
    FUNCTION = public.planted_count);
CREATE AGGREGATE public.count(*) (SFUNC = public.planted_step, STYPE = pg_catalog.int8);
CREATE FUNCTION public.current_setting(pg_catalog.text) RETURNS pg_catalog.text
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'planted object ran'; END $$;
CREATE FUNCTION public.current_setting(pg_catalog.text, pg_catalog.bool) RETURNS pg_catalog.text
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'planted object ran'; END $$;
CREATE DOMAIN public.int8 AS pg_catalog.text CHECK (public.planted_check(VALUE));
CREATE DOMAIN public.text AS pg_catalog.varchar CHECK (public.planted_check(VALUE));
CREATE TYPE public.hijack_mood AS ENUM ('calm');
CREATE TYPE public.hijack_range AS RANGE (SUBTYPE = pg_catalog.float8);
CREATE TYPE public.hijack_pair AS (a pg_catalog.int4, b pg_catalog.varchar);
CREATE DOMAIN public.hijack_word AS pg_catalog.varchar;
CREATE TABLE public.hijack_typed (
    mood public.hijack_mood,
    pair public.hijack_pair,
    word public.hijack_word,
    span public.hijack_range
);
INSERT INTO public.hijack_typed VALUES ('calm', ROW(1, 'x'), 'w', '[1.5,2.5]');
CREATE FUNCTION public.planted_oid(pg_catalog.oid, pg_catalog.oid) RETURNS pg_catalog.bool
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'planted object ran'; END $$;
CREATE FUNCTION public.planted_gt4(pg_catalog.int2, pg_catalog.int4) RETURNS pg_catalog.bool
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'planted object ran'; END $$;
CREATE FUNCTION public.planted_gt2(pg_catalog.int2, pg_catalog.int2) RETURNS pg_catalog.bool
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'planted object ran'; END $$;
CREATE OPERATOR public.= (LEFTARG = pg_catalog.oid, RIGHTARG = pg_catalog.oid,
    FUNCTION = public.planted_oid);
CREATE OPERATOR public.> (LEFTARG = pg_catalog.int2, RIGHTARG = pg_catalog.int4,
    FUNCTION = public.planted_gt4);
CREATE OPERATOR public.> (LEFTARG = pg_catalog.int2, RIGHTARG = pg_catalog.int2,
    FUNCTION = public.planted_gt2);
"#;

const SEARCH_PATH: &str = "SET search_path = public, pg_catalog";

async fn prepare_database(port: u16) -> TestResult {
    let admin = connect(&params(port)).await?;
    run(admin.as_ref(), "DROP DATABASE IF EXISTS varve_hijack").await?;
    run(admin.as_ref(), "CREATE DATABASE varve_hijack").await?;
    let mut p = params(port);
    p.database = Some("varve_hijack".into());
    let planter = connect(&p).await?;
    run(planter.as_ref(), PLANT).await?;
    // The planted objects do run when SQL names them without a schema.
    for sql in [
        "SELECT 'a'::pg_catalog.text = 'b'::pg_catalog.text",
        "SELECT count(*) FROM pg_catalog.pg_class",
        "SELECT current_setting('work_mem')",
        "SELECT 1::pg_catalog.int8 = 1",
        // What the unpatched type lookups of tokio-postgres would run.
        "SELECT 1::pg_catalog.oid = 1::pg_catalog.oid",
        "SELECT 1::pg_catalog.int2 > 0",
        "SELECT 1::pg_catalog.int2 > 0::pg_catalog.int2",
        "SELECT t.typname FROM pg_catalog.pg_type t \
         JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid LIMIT 1",
    ] {
        let error = run(planter.as_ref(), &format!("{SEARCH_PATH}; {sql}"))
            .await
            .err()
            .ok_or_else(|| format!("`{sql}` ran without the planted object"))?;
        assert!(
            error.message.contains("planted object ran"),
            "{sql}: {error:?}"
        );
    }
    Ok(())
}

async fn drop_database(port: u16) -> TestResult {
    let admin = connect(&params(port)).await?;
    run(
        admin.as_ref(),
        "DROP DATABASE IF EXISTS varve_hijack WITH (FORCE)",
    )
    .await?;
    Ok(())
}

/// Runs `sql` and fails on an error or on any warning that the session
/// had to be closed or that a check failed.
async fn clean(session: &dyn Session, sql: &str, case: &str) -> TestResult {
    let output = run(session, sql)
        .await
        .map_err(|e| format!("{case}: `{sql}` failed: {e:?}"))?;
    for notice in &output.notices {
        assert!(
            !notice.message.contains("planted") && !notice.message.contains("closed"),
            "{case}: `{sql}`: {notice:?}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn planted_objects_never_run_in_the_drivers_own_sql() -> TestResult {
    for (major, port) in servers() {
        let case = format!("PG {major}");
        prepare_database(port).await?;
        let mut p = params(port);
        p.database = Some("varve_hijack".into());
        p.init_sql = Some(SEARCH_PATH.into());
        p.statement_timeout = Some(Duration::from_millis(1_500));

        // A normal session: settings checks after SET, RESET (the
        // statement_timeout check reads pg_settings), CALL-like statements
        // (the type lookup statements are counted), transaction blocks.
        let session: Arc<dyn Session> = Arc::from(connect(&p).await?);
        let s = session.as_ref();
        // Types tokio-postgres does not know: each is looked up in the
        // catalogs (base type, enum labels, composite fields, domain base,
        // range subtype), on a fresh connection.
        clean(s, "SELECT 'calm'::public.hijack_mood", &case).await?;
        clean(s, "SELECT * FROM public.hijack_typed", &case).await?;
        clean(
            s,
            "SELECT ROW(2, 'y')::public.hijack_pair, 'v'::public.hijack_word, \
             ARRAY['calm']::public.hijack_mood[]",
            &case,
        )
        .await?;
        clean(s, "BEGIN; SELECT * FROM public.hijack_typed; COMMIT", &case).await?;
        clean(s, "SET work_mem = '2MB'; SELECT 1", &case).await?;
        clean(s, "RESET statement_timeout; SELECT 2", &case).await?;
        clean(s, "SET statement_timeout TO DEFAULT", &case).await?;
        clean(s, "DO $$ BEGIN PERFORM 1; END $$", &case).await?;
        clean(
            s,
            "SELECT pg_catalog.set_config('bytea_output', 'escape', false)",
            &case,
        )
        .await?;
        clean(
            s,
            "BEGIN; SET LOCAL work_mem = '3MB'; SELECT 3; COMMIT",
            &case,
        )
        .await?;
        if major >= 14 {
            // A multirange of a user range type: its element type is looked
            // up in pg_range.
            clean(
                s,
                "SELECT 1; SELECT '{[1.5,2.5]}'::public.hijack_multirange",
                &case,
            )
            .await?;
        }
        // DISCARD ALL reconnects: setup, init SQL and checks run again, and
        // the new connection looks the types up again.
        clean(s, "DISCARD ALL", &case).await?;
        clean(s, "SELECT 4", &case).await?;
        clean(s, "SELECT * FROM public.hijack_typed", &case).await?;
        assert_eq!(session.in_transaction(), Some(false), "{case}");

        // A statement timeout and a cancel are told apart without
        // running anything planted.
        let error = run(s, "SELECT pg_catalog.pg_sleep(5)")
            .await
            .err()
            .ok_or("pg_sleep finished")?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{case}: {error:?}");
        let running = Arc::clone(&session);
        let task = tokio::spawn(async move {
            run(running.as_ref(), "SELECT 5; SELECT pg_catalog.pg_sleep(1)").await
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        session.cancel_handle().cancel().await?;
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await??
            .err()
            .ok_or("the script finished")?;
        assert_eq!(error.kind, ErrorKind::Cancelled, "{case}: {error:?}");
        clean(s, "SELECT 6", &case).await?;
        session.ping().await?;

        // A read-only session checks read-only mode after every statement.
        let mut ro = p.clone();
        ro.read_only = true;
        let reader = connect(&ro).await?;
        clean(reader.as_ref(), "SELECT * FROM public.hijack_typed", &case).await?;
        clean(reader.as_ref(), "SELECT 7; SET work_mem = '4MB'", &case).await?;
        clean(reader.as_ref(), "RESET work_mem; SELECT 8", &case).await?;
        reader.ping().await?;
        drop(reader);
        drop(session);
        drop_database(port).await?;
    }
    Ok(())
}
