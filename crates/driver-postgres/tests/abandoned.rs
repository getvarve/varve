//! Calls given up half way (their future dropped), resets the driver
//! cannot see, and errors user code raises with the SQLSTATE of a server
//! event. Against the PostgreSQL servers of `dev/docker-compose.yml`:
//!
//! ```sh
//! docker compose -p varve-m1-pg -f dev/docker-compose.yml up -d --wait postgres-13 postgres-18
//! cargo test -p varve-driver-postgres --test abandoned -- --include-ignored
//! ```

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    TRANSLATED_TIMEOUT, TestResult, backend_pid, connect, dropped_discard_all_keeps_the_init_sql,
    latency_proxy, newest, params, run, scalar, servers, translating_proxy, wait_until_dead,
};
use varve_driver::{ConnectParams, DriverError, ErrorKind, ExecOptions, Session, Value};

/// Starts `sql` on `session` and drops the call after `after`. Returns
/// whether the call finished before that.
async fn drop_after(session: &dyn Session, sql: &str, after: Duration) -> bool {
    tokio::time::timeout(after, session.execute(sql, &ExecOptions::default()))
        .await
        .is_ok()
}

/// The drop points tried: every millisecond up to `last`.
fn drop_points(last: u64) -> impl Iterator<Item = Duration> {
    (0..=last).map(Duration::from_millis)
}

/// The error of `sql`, which must fail.
async fn fails(session: &dyn Session, sql: &str) -> Result<DriverError, String> {
    match run(session, sql).await {
        Ok(output) => Err(format!("`{sql}` succeeded: {output:?}")),
        Err(error) => Ok(error),
    }
}

/// Creates what the read-only drop test needs, as a writer: a table and a
/// function that switches read-only mode off for the session.
async fn read_only_fixtures(port: u16) -> TestResult {
    let writer = connect(&params(port)).await?;
    run(
        writer.as_ref(),
        "CREATE TABLE IF NOT EXISTS public.varve_drop_w (v int);\n\
         TRUNCATE public.varve_drop_w;\n\
         CREATE OR REPLACE FUNCTION public.varve_drop_ro_off() RETURNS int LANGUAGE plpgsql AS \
         $$ BEGIN PERFORM pg_catalog.set_config('default_transaction_read_only', 'off', false); \
         RETURN 1; END $$",
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_dropped_call_never_leaves_a_read_only_session_writable() -> TestResult {
    for (major, port) in servers() {
        read_only_fixtures(port).await?;
        let mut ro = params(port);
        ro.read_only = true;
        let session = connect(&ro).await?;
        let scripts = [
            "SELECT public.varve_drop_ro_off(); SELECT pg_sleep(0.01); \
             SELECT public.varve_drop_ro_off(); SELECT pg_sleep(0.01)",
            "DO $$ BEGIN PERFORM public.varve_drop_ro_off(); PERFORM pg_sleep(0.01); END $$",
            "SELECT pg_sleep(0.005); SELECT public.varve_drop_ro_off()",
        ];
        let mut dropped = 0;
        for script in scripts {
            for after in drop_points(30) {
                if drop_after(session.as_ref(), script, after).await {
                    continue;
                }
                dropped += 1;
                let error = fails(
                    session.as_ref(),
                    "INSERT INTO public.varve_drop_w VALUES (42)",
                )
                .await
                .map_err(|e| format!("PG {major} `{script}` dropped after {after:?}: {e}"))?;
                assert_eq!(
                    error.kind,
                    ErrorKind::ReadOnly,
                    "PG {major} `{script}` dropped after {after:?}: {error:?}"
                );
                assert_eq!(session.in_transaction(), Some(false), "PG {major}");
            }
        }
        assert!(
            dropped > 20,
            "PG {major}: only {dropped} calls were dropped"
        );
        let writer = connect(&params(port)).await?;
        assert_eq!(
            scalar(writer.as_ref(), "SELECT count(*) FROM public.varve_drop_w").await?,
            Value::Int(0),
            "PG {major}"
        );
    }
    Ok(())
}

fn with_profile_timeout(port: u16) -> ConnectParams {
    let mut profile = params(port);
    profile.statement_timeout = Some(Duration::from_secs(20));
    profile
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_dropped_discard_all_keeps_the_profile_timeout() -> TestResult {
    for (major, port) in servers() {
        let session = connect(&with_profile_timeout(port)).await?;
        let mut dropped = 0;
        for script in [
            "DISCARD ALL",
            "SELECT pg_sleep(0.005); DISCARD ALL; SELECT pg_sleep(0.005)",
            "RESET ALL; SELECT pg_sleep(0.01)",
            "SET TimeZone = 'Europe/Amsterdam'; SET extra_float_digits = 0; \
             SELECT pg_sleep(0.01)",
        ] {
            for after in drop_points(40) {
                if drop_after(session.as_ref(), script, after).await {
                    continue;
                }
                dropped += 1;
                let case = format!("PG {major} `{script}` dropped after {after:?}");
                assert_eq!(
                    scalar(session.as_ref(), "SHOW statement_timeout").await?,
                    Value::Text("20s".into()),
                    "{case}"
                );
                assert_eq!(
                    scalar(session.as_ref(), "SHOW TimeZone").await?,
                    Value::Text("UTC".into()),
                    "{case}"
                );
                assert_eq!(
                    scalar(session.as_ref(), "SHOW extra_float_digits").await?,
                    Value::Text("3".into()),
                    "{case}"
                );
                assert_eq!(session.in_transaction(), Some(false), "{case}");
            }
        }
        assert!(
            dropped > 20,
            "PG {major}: only {dropped} calls were dropped"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_dropped_call_in_a_transaction_keeps_it() -> TestResult {
    for (major, port) in servers() {
        let session = connect(&params(port)).await?;
        run(session.as_ref(), "CREATE TEMP TABLE varve_drop_tx (v int)").await?;
        run(
            session.as_ref(),
            "BEGIN; INSERT INTO varve_drop_tx VALUES (1)",
        )
        .await?;
        assert!(
            !drop_after(
                session.as_ref(),
                "INSERT INTO varve_drop_tx VALUES (2); SELECT pg_sleep(0.2)",
                Duration::from_millis(50)
            )
            .await
        );
        assert_eq!(session.in_transaction(), None, "PG {major}");
        // The next call waits for the abandoned statement, then goes on in
        // the same transaction.
        assert_eq!(
            scalar(session.as_ref(), "SELECT count(*) FROM varve_drop_tx").await?,
            Value::Int(2),
            "PG {major}"
        );
        assert_eq!(session.in_transaction(), Some(true), "PG {major}");
        run(session.as_ref(), "ROLLBACK").await?;
        assert_eq!(
            scalar(session.as_ref(), "SELECT count(*) FROM varve_drop_tx").await?,
            Value::Int(0),
            "PG {major}"
        );
        assert_eq!(session.in_transaction(), Some(false), "PG {major}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn the_end_of_the_connection_is_seen_after_a_dropped_reconnect() -> TestResult {
    for (major, port) in servers() {
        let admin = connect(&params(port)).await?;
        let mut dropped = 0;
        for after in (0..=40).step_by(2).map(Duration::from_millis) {
            let mut p = params(port);
            p.application_name = format!("varve drop reconnect {major} {}", after.as_millis());
            let session = connect(&p).await?;
            if drop_after(session.as_ref(), "DISCARD ALL", after).await {
                continue;
            }
            dropped += 1;
            let case = format!("PG {major} DISCARD ALL dropped after {after:?}");
            // The next call finishes what was given up.
            run(session.as_ref(), "SELECT 1").await?;
            assert_eq!(session.in_transaction(), Some(false), "{case}");
            // Whichever connection is in use now, its end must be seen,
            // without a call.
            run(
                admin.as_ref(),
                &format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE application_name = '{}'",
                    p.application_name
                ),
            )
            .await?;
            let started = Instant::now();
            wait_until_dead(session.as_ref(), &case).await?;
            assert!(started.elapsed() < Duration::from_secs(2), "{case}");
            let error = fails(session.as_ref(), "SELECT 1").await?;
            assert_eq!(error.kind, ErrorKind::ConnectionLost, "{case}: {error:?}");
        }
        assert!(dropped > 0, "PG {major}: no DISCARD ALL was dropped");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn resets_the_driver_cannot_see_keep_the_profile_timeout() -> TestResult {
    for (major, port) in servers() {
        let session = connect(&with_profile_timeout(port)).await?;
        run(
            session.as_ref(),
            "CREATE FUNCTION pg_temp.varve_reset() RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN RESET statement_timeout; RETURN 1; END $$",
        )
        .await?;
        for sql in [
            "RESET \"statement_timeout\"",
            "DO $$ BEGIN RESET statement_timeout; END $$",
            "SELECT pg_temp.varve_reset()",
            "SELECT set_config('statement_timeout', NULL, false)",
            "BEGIN; SELECT pg_temp.varve_reset(); COMMIT",
            "SET \"statement_timeout\" TO DEFAULT",
        ] {
            run(session.as_ref(), "SET statement_timeout = 300").await?;
            run(session.as_ref(), sql).await?;
            assert_eq!(
                scalar(session.as_ref(), "SHOW statement_timeout").await?,
                Value::Text("20s".into()),
                "PG {major}: {sql}"
            );
        }
        // The user's own value still stays, even through other checks.
        run(session.as_ref(), "SET statement_timeout = 0").await?;
        run(
            session.as_ref(),
            "DO $$ BEGIN PERFORM 1; END $$; SELECT 1::float8",
        )
        .await?;
        assert_eq!(
            scalar(session.as_ref(), "SHOW statement_timeout").await?,
            Value::Text("0".into()),
            "PG {major}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_raised_connection_sqlstate_does_not_lose_the_session() -> TestResult {
    for (major, port) in servers() {
        for read_only in [false, true] {
            let mut p = params(port);
            p.read_only = read_only;
            let session = connect(&p).await?;
            for code in ["57P01", "57P02", "57P03"] {
                let case = format!("PG {major} read_only={read_only} {code}");
                let raise = format!(
                    "DO $$ BEGIN RAISE EXCEPTION 'raised by user code' USING ERRCODE = '{code}'; \
                     END $$"
                );
                let error = fails(session.as_ref(), &raise).await?;
                assert_eq!(error.kind, ErrorKind::Query, "{case}: {error:?}");
                assert_eq!(error.code.as_deref(), Some(code), "{case}");
                assert_eq!(session.in_transaction(), Some(false), "{case}");
                if !read_only {
                    let error = fails(session.as_ref(), &format!("BEGIN; {raise}")).await?;
                    assert_eq!(error.kind, ErrorKind::Query, "{case}: {error:?}");
                    assert_eq!(
                        session.in_transaction(),
                        Some(true),
                        "{case} in a transaction"
                    );
                    run(session.as_ref(), "ROLLBACK").await?;
                }
                assert_eq!(
                    scalar(session.as_ref(), "SELECT 1").await?,
                    Value::Int(1),
                    "{case}"
                );
                assert_eq!(session.in_transaction(), Some(false), "{case}");
            }
        }
    }
    Ok(())
}

/// A `57014` with the server's own message, raised by user code.
fn raise_cancel(message: &str) -> String {
    format!(
        "DO $$ BEGIN RAISE EXCEPTION USING ERRCODE = 'query_canceled', MESSAGE = '{message}'; \
         END $$"
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn raised_cancel_messages_are_not_trusted() -> TestResult {
    for (major, port) in servers() {
        // An untranslated server: a raised "statement timeout" is no
        // timeout, a raised "user request" no cancel.
        let session = connect(&params(port)).await?;
        for message in [
            "canceling statement due to statement timeout",
            "canceling statement due to user request",
        ] {
            let error = fails(session.as_ref(), &raise_cancel(message)).await?;
            assert_eq!(error.kind, ErrorKind::Query, "PG {major}: {error:?}");
            assert_eq!(error.message, message, "PG {major}");
        }

        // A translated server: a raised English message does not teach the
        // session that messages are untranslated.
        let proxy = translating_proxy(port).await?;
        let mut german = params(proxy.port);
        german.tls.mode = varve_driver::TlsMode::Disable;
        german.statement_timeout = Some(Duration::from_secs(5));
        german.init_sql = Some("SET lc_messages = 'de_DE.UTF-8'".into());
        let session = connect(&german).await?;
        for message in [
            "canceling statement due to user request",
            "canceling statement due to statement timeout",
        ] {
            let error = fails(session.as_ref(), &raise_cancel(message)).await?;
            assert_eq!(error.kind, ErrorKind::Query, "PG {major}: {error:?}");
            // In a transaction, the timeout in force is still read first.
            let error = fails(
                session.as_ref(),
                "BEGIN; SET LOCAL statement_timeout = 200; SELECT pg_sleep(1)",
            )
            .await?;
            assert_eq!(error.kind, ErrorKind::Timeout, "PG {major}: {error:?}");
            assert_eq!(error.message, TRANSLATED_TIMEOUT, "PG {major}");
            run(session.as_ref(), "ROLLBACK").await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_timeout_inside_a_function_is_a_timeout() -> TestResult {
    // The server reports it with the function as context: still the
    // server's own interrupt, not user code.
    for (major, port) in servers() {
        let session: Arc<dyn Session> = Arc::from(connect(&params(port)).await?);
        run(
            session.as_ref(),
            "CREATE FUNCTION pg_temp.varve_sleep() RETURNS void LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM pg_sleep(2); END $$",
        )
        .await?;
        let error = fails(
            session.as_ref(),
            "SET statement_timeout = 200; SELECT pg_temp.varve_sleep()",
        )
        .await?;
        assert_eq!(error.kind, ErrorKind::Timeout, "PG {major}: {error:?}");
        run(session.as_ref(), "RESET statement_timeout").await?;
        let running = Arc::clone(&session);
        let task =
            tokio::spawn(
                async move { run(running.as_ref(), "SELECT pg_temp.varve_sleep()").await },
            );
        tokio::time::sleep(Duration::from_millis(300)).await;
        session.cancel_handle().cancel().await?;
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await??
            .err()
            .ok_or("finished")?;
        assert_eq!(error.kind, ErrorKind::Cancelled, "PG {major}: {error:?}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_dropped_discard_all_runs_the_init_sql_again() -> TestResult {
    for (_, port) in servers() {
        let far = latency_proxy(port, Duration::from_millis(5)).await?;
        dropped_discard_all_keeps_the_init_sql(params(far.port), params(port)).await?;
    }
    Ok(())
}

/// Prepared statements of this session on the server.
async fn prepared_statements(session: &dyn Session) -> Result<i64, Box<dyn std::error::Error>> {
    match scalar(
        session,
        "SELECT pg_catalog.count(*) FROM pg_catalog.pg_prepared_statements",
    )
    .await?
    {
        Value::Int(count) => Ok(count),
        other => Err(format!("not a count: {other:?}").into()),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn dropped_type_lookups_do_not_pile_up() -> TestResult {
    for (major, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DO $$ BEGIN CREATE TYPE public.varve_drop_mood AS ENUM ('calm'); \
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$;\n\
             DO $$ BEGIN CREATE TYPE public.varve_drop_pair AS (a int, b text); \
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$",
        )
        .await?;
        let far = latency_proxy(port, Duration::from_millis(3)).await?;
        let options = ExecOptions::default();
        let mut dropped = 0;
        for after in (0..=40).map(Duration::from_millis) {
            // A fresh session each time: its type lookups are prepared
            // during the first statement that returns these types.
            let session = connect(&params(far.port)).await?;
            let call = session.execute(
                "SELECT 'calm'::public.varve_drop_mood, ROW(1, 'x')::public.varve_drop_pair",
                &options,
            );
            if tokio::time::timeout(after, call).await.is_ok() {
                continue;
            }
            dropped += 1;
            // The next call recovers; the same types again need no new
            // lookups, so what is left is at most one per kind of lookup.
            run(
                session.as_ref(),
                "SELECT 'calm'::public.varve_drop_mood, ROW(1, 'x')::public.varve_drop_pair",
            )
            .await?;
            let count = prepared_statements(session.as_ref()).await?;
            assert!(
                count <= 3,
                "PG {major}: {count} prepared statements after a drop at {after:?}"
            );
        }
        assert!(
            dropped > 10,
            "PG {major}: only {dropped} calls were given up"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn notices_of_a_dropped_call_are_not_the_next_ones() -> TestResult {
    let options = ExecOptions::default();
    for (major, port) in servers() {
        let session = connect(&params(port)).await?;
        let call = session.execute(
            "DO $$ BEGIN PERFORM pg_sleep(0.1); RAISE NOTICE 'from the given up call'; END $$",
            &options,
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), call)
                .await
                .is_err()
        );
        let output = run(session.as_ref(), "SELECT 1").await?;
        assert!(
            output
                .notices
                .iter()
                .all(|n| !n.message.contains("given up call")),
            "PG {major}: {:?}",
            output.notices
        );
        // Also not in the partial output of a failing call.
        let call = session.execute(
            "DO $$ BEGIN PERFORM pg_sleep(0.1); RAISE NOTICE 'from the given up call'; END $$",
            &options,
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), call)
                .await
                .is_err()
        );
        let error = fails(session.as_ref(), "SELECT 1; SELECT 1/0").await?;
        let partial = error.partial.as_deref().ok_or("no partial output")?;
        assert!(
            partial
                .notices
                .iter()
                .all(|n| !n.message.contains("given up call")),
            "PG {major}: {:?}",
            partial.notices
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_requested_cancel_is_a_cancel_even_when_re_raised() -> TestResult {
    for (major, port) in servers() {
        let session: Arc<dyn Session> = Arc::from(connect(&params(port)).await?);
        let running = Arc::clone(&session);
        let task = tokio::spawn(async move {
            run(
                running.as_ref(),
                "DO $$ BEGIN PERFORM pg_sleep(5); \
                 EXCEPTION WHEN query_canceled THEN \
                 RAISE EXCEPTION USING ERRCODE = SQLSTATE, MESSAGE = SQLERRM; END $$",
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        session.cancel_handle().cancel().await?;
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await??
            .err()
            .ok_or("finished")?;
        assert_eq!(error.kind, ErrorKind::Cancelled, "PG {major}: {error:?}");
        assert_eq!(session.in_transaction(), Some(false), "PG {major}");
    }
    Ok(())
}

/// Asserts that `session` is still on backend `pid` with what the user set
/// (`work_mem` 5MB and the temporary table `varve_kept`): it did not
/// reconnect.
async fn assert_not_reopened(session: &dyn Session, pid: i64, case: &str) -> TestResult {
    assert_eq!(backend_pid(session).await?, pid, "{case}: reconnected");
    assert_eq!(
        scalar(session, "SHOW work_mem").await?,
        Value::Text("5MB".into()),
        "{case}"
    );
    assert_eq!(
        scalar(session, "SELECT pg_catalog.count(*) FROM varve_kept").await?,
        Value::Int(0),
        "{case}"
    );
    let output = run(session, "SELECT 1").await?;
    assert!(
        output
            .notices
            .iter()
            .all(|n| !n.message.contains("reconnected")),
        "{case}: {:?}",
        output.notices
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_call_dropped_while_old_lookups_are_cleaned_up_keeps_the_session() -> TestResult {
    for (major, port) in servers() {
        let writer = connect(&params(port)).await?;
        run(
            writer.as_ref(),
            "DO $$ BEGIN CREATE TYPE public.varve_drop_mood AS ENUM ('calm'); \
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$;\n\
             DO $$ BEGIN CREATE TYPE public.varve_drop_pair AS (a int, b text); \
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$",
        )
        .await?;
        let far = latency_proxy(port, Duration::from_millis(3)).await?;
        let options = ExecOptions::default();
        let typed = "SELECT 'calm'::public.varve_drop_mood, ROW(1, 'x')::public.varve_drop_pair";
        let mut cleanups_dropped = 0;
        // Measured at 3 ms latency: the first call leaves a lookup behind
        // when given up after about 5 to 9 ms, and the second call's
        // DEALLOCATE of it has run on the server, its reply not yet read,
        // after about 63 to 70 ms.
        for first in 4u64..=10 {
            for second in 56..=84 {
                let case = format!("PG {major} drops after {first} and {second} ms");
                let session = connect(&params(far.port)).await?;
                run(
                    session.as_ref(),
                    "SET work_mem = '5MB'; CREATE TEMP TABLE varve_kept (v int)",
                )
                .await?;
                let pid = backend_pid(session.as_ref()).await?;
                // Given up while the type lookups are prepared: one may stay.
                let call = session.execute(typed, &options);
                if tokio::time::timeout(Duration::from_millis(first), call)
                    .await
                    .is_ok()
                {
                    continue;
                }
                // Recovers, prepares the lookups again and deallocates the
                // old one at its end: given up at any point of that.
                let call = session.execute(typed, &options);
                if tokio::time::timeout(Duration::from_millis(second), call)
                    .await
                    .is_err()
                {
                    cleanups_dropped += 1;
                }
                assert_not_reopened(session.as_ref(), pid, &case).await?;
            }
        }
        assert!(
            cleanups_dropped > 50,
            "PG {major}: only {cleanups_dropped} drops"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_refused_discard_all_dropped_in_a_transaction_keeps_the_session() -> TestResult {
    for (major, port) in servers() {
        let far = latency_proxy(port, Duration::from_millis(5)).await?;
        let mut init = params(far.port);
        init.init_sql = Some("SET work_mem = '7MB'".into());
        let options = ExecOptions::default();
        let mut dropped = 0;
        for after in (1..=25).map(Duration::from_millis) {
            let case = format!("PG {major} DISCARD ALL in a transaction dropped after {after:?}");
            let session = connect(&init).await?;
            run(
                session.as_ref(),
                "SET work_mem = '5MB'; CREATE TEMP TABLE varve_kept (v int)",
            )
            .await?;
            let pid = backend_pid(session.as_ref()).await?;
            run(session.as_ref(), "BEGIN").await?;
            let call = session.execute("DISCARD ALL", &options);
            if tokio::time::timeout(after, call).await.is_ok() {
                run(session.as_ref(), "ROLLBACK").await?;
                continue;
            }
            dropped += 1;
            // Refused by the server (25001), so the block is aborted.
            let _ = run(session.as_ref(), "SELECT 1").await;
            run(session.as_ref(), "ROLLBACK").await?;
            assert_not_reopened(session.as_ref(), pid, &case).await?;
        }
        assert!(dropped > 5, "PG {major}: only {dropped} drops");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_cancel_as_a_call_starts_is_the_users() -> TestResult {
    let (major, port) = newest();
    let session: Arc<dyn Session> = Arc::from(connect(&params(port)).await?);
    let mut cancelled = 0;
    for round in 0..150 {
        let handle = session.cancel_handle();
        let canceller = tokio::spawn(async move { handle.cancel().await });
        let running = Arc::clone(&session);
        let call = tokio::spawn(async move { run(running.as_ref(), "SELECT pg_sleep(0.2)").await });
        canceller.await??;
        if let Err(error) = call.await? {
            cancelled += 1;
            assert_eq!(
                error.kind,
                ErrorKind::Cancelled,
                "PG {major} round {round}: {error:?}"
            );
            assert!(
                !error.message.contains("another session"),
                "PG {major} round {round}: {error:?}"
            );
        }
    }
    eprintln!("PG {major}: {cancelled} of 150 calls were cancelled");
    Ok(())
}
