//! TLS integration tests. They need the plain PostgreSQL 18 server of
//! `dev/docker-compose.yml`, the TLS server of `tests/tls/compose.yml` and
//! the certificates it was started with, in `VARVE_PG_TLS_DIR`:
//!
//! ```sh
//! export VARVE_PG_TLS_DIR="$(mktemp -d)"
//! cargo run -p varve-driver-postgres --example tls-test-certs -- "$VARVE_PG_TLS_DIR"
//! docker compose -p varve-m1-pg -f dev/docker-compose.yml up -d --wait postgres-18
//! docker compose -p varve-m1-pg -f crates/driver-postgres/tests/tls/compose.yml up -d --wait
//! cargo test -p varve-driver-postgres --test tls -- --include-ignored
//! ```
//!
//! A missing `VARVE_PG_TLS_DIR` or certificate fails the tests: they never
//! pass by skipping.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{
    TestResult, connect, dead_sessions_fail_fast, dropped_discard_all_keeps_the_init_sql,
    latency_proxy, newest, params, run, scalar, tls_port,
};
use varve_driver::{ConnectParams, ErrorKind, Session, TlsMode, TlsParams, Value};

/// Role that must log in with a client certificate (see the compose file).
const CERT_USER: &str = "varve_cert";

/// Path of a generated certificate file. Panics (fails the test) when the
/// TLS setup is missing.
fn fixture(name: &str) -> String {
    let Some(dir) = std::env::var_os("VARVE_PG_TLS_DIR") else {
        panic!(
            "VARVE_PG_TLS_DIR is not set: generate certificates with the tls-test-certs \
             example and start tests/tls/compose.yml with the same directory"
        );
    };
    let path = std::path::Path::new(&dir).join(name);
    assert!(
        path.is_file(),
        "{} is missing: run the tls-test-certs example for VARVE_PG_TLS_DIR",
        path.display()
    );
    path.display().to_string()
}

fn tls_params(port: u16, tls: TlsParams) -> ConnectParams {
    let mut params = params(port);
    params.tls = tls;
    params
}

fn mode(mode: TlsMode) -> TlsParams {
    TlsParams {
        mode,
        ..TlsParams::default()
    }
}

fn with_ca(mode: TlsMode) -> TlsParams {
    TlsParams {
        mode,
        ca_path: Some(fixture("ca.crt")),
        ..TlsParams::default()
    }
}

async fn uses_tls(session: &dyn Session) -> Result<bool, Box<dyn std::error::Error>> {
    match scalar(
        session,
        "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
    )
    .await?
    {
        Value::Bool(ssl) => Ok(ssl),
        other => Err(format!("unexpected value {other:?}").into()),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn require_encrypts_without_checking_the_certificate() -> TestResult {
    let session = connect(&tls_params(tls_port(), mode(TlsMode::Require))).await?;
    assert!(uses_tls(session.as_ref()).await?);
    // Command tags and notices still come through the encrypted stream.
    let output = run(
        session.as_ref(),
        "CREATE TEMP TABLE t (a int); INSERT INTO t VALUES (1), (2); \
         DO $$ BEGIN RAISE NOTICE 'over tls'; END $$",
    )
    .await?;
    assert_eq!(output.results.len(), 3);
    assert_eq!(output.notices.len(), 1);
    assert!(matches!(
        &output.results[1],
        varve_driver::StatementResult::Command { tag, rows_affected: Some(2) } if tag == "INSERT 0 2"
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn disable_and_prefer() -> TestResult {
    let plain = connect(&tls_params(tls_port(), mode(TlsMode::Disable))).await?;
    assert!(!uses_tls(plain.as_ref()).await?);

    let preferred = connect(&tls_params(tls_port(), mode(TlsMode::Prefer))).await?;
    assert!(uses_tls(preferred.as_ref()).await?);

    // The compose servers have TLS off: Prefer falls back to plain text.
    let (_, plain_port) = newest();
    let fallback = connect(&tls_params(plain_port, mode(TlsMode::Prefer))).await?;
    assert!(!uses_tls(fallback.as_ref()).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn require_fails_when_the_server_has_no_tls() -> TestResult {
    let (_, plain_port) = newest();
    let error = connect(&tls_params(plain_port, mode(TlsMode::Require)))
        .await
        .err()
        .ok_or("connected without TLS")?;
    assert_eq!(error.kind, ErrorKind::Tls, "{error:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn verify_full_rejects_an_untrusted_certificate() -> TestResult {
    // The server certificate is signed by a private test CA that is not in
    // the OS trust store.
    let error = connect(&tls_params(tls_port(), mode(TlsMode::VerifyFull)))
        .await
        .err()
        .ok_or("an untrusted certificate was accepted")?;
    assert_eq!(error.kind, ErrorKind::Tls, "{error:?}");
    assert!(error.message.contains("certificate"), "{error:?}");

    let error = connect(&tls_params(tls_port(), mode(TlsMode::VerifyCa)))
        .await
        .err()
        .ok_or("an untrusted certificate was accepted")?;
    assert_eq!(error.kind, ErrorKind::Tls, "{error:?}");

    // A CA file that did not sign the server certificate.
    let wrong_ca = TlsParams {
        ca_path: Some(fixture("other-ca.crt")),
        ..mode(TlsMode::VerifyCa)
    };
    let error = connect(&tls_params(tls_port(), wrong_ca))
        .await
        .err()
        .ok_or("a certificate from another CA was accepted")?;
    assert_eq!(error.kind, ErrorKind::Tls, "{error:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn verify_with_the_test_ca() -> TestResult {
    // The certificate names localhost and 127.0.0.1.
    for host in ["localhost", "127.0.0.1"] {
        let mut params = tls_params(tls_port(), with_ca(TlsMode::VerifyFull));
        params.host = host.into();
        let session = connect(&params).await?;
        assert!(uses_tls(session.as_ref()).await?);
    }

    // A tunnel: the endpoint is local, the certificate name is checked
    // against server_name.
    let wrong_name = TlsParams {
        server_name: Some("db.example.com".into()),
        ..with_ca(TlsMode::VerifyFull)
    };
    let error = connect(&tls_params(tls_port(), wrong_name.clone()))
        .await
        .err()
        .ok_or("a certificate for another name was accepted")?;
    assert_eq!(error.kind, ErrorKind::Tls, "{error:?}");

    let right_name = TlsParams {
        server_name: Some("localhost".into()),
        ..with_ca(TlsMode::VerifyFull)
    };
    let mut params = tls_params(tls_port(), right_name);
    params.host = "127.0.0.1".into();
    connect(&params).await?;

    // VerifyCa checks the chain but not the name.
    let chain_only = TlsParams {
        mode: TlsMode::VerifyCa,
        ..wrong_name
    };
    let session = connect(&tls_params(tls_port(), chain_only)).await?;
    assert!(uses_tls(session.as_ref()).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn cancel_over_tls() -> TestResult {
    let session: Arc<dyn Session> =
        Arc::from(connect(&tls_params(tls_port(), with_ca(TlsMode::VerifyFull))).await?);
    let running = Arc::clone(&session);
    let task = tokio::spawn(async move { run(running.as_ref(), "SELECT pg_sleep(30)").await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.cancel_handle().cancel().await?;
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await??
        .err()
        .ok_or("pg_sleep finished")?;
    assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
    session.ping().await?;
    Ok(())
}

/// Parameters for [`CERT_USER`] over verified TLS, with or without the
/// client certificate.
fn cert_user_params(with_certificate: bool) -> ConnectParams {
    let mut tls = with_ca(TlsMode::VerifyFull);
    if with_certificate {
        tls.client_cert_path = Some(fixture("client.crt"));
        tls.client_key_path = Some(fixture("client.key"));
    }
    let mut params = tls_params(tls_port(), tls);
    params.username = Some(CERT_USER.into());
    params
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn client_certificate_logs_in() -> TestResult {
    let session = connect(&cert_user_params(true)).await?;
    assert_eq!(
        scalar(session.as_ref(), "SELECT current_user").await?,
        Value::Text(CERT_USER.into())
    );
    // The server saw and verified the certificate.
    assert_eq!(
        scalar(
            session.as_ref(),
            "SELECT client_dn FROM pg_stat_ssl WHERE pid = pg_backend_pid()"
        )
        .await?,
        Value::Text(format!("/CN={CERT_USER}"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn missing_client_certificate_is_refused() -> TestResult {
    let error = connect(&cert_user_params(false))
        .await
        .err()
        .ok_or("logged in without a client certificate")?;
    assert_eq!(error.kind, ErrorKind::Auth, "{error:?}");
    assert_eq!(error.code.as_deref(), Some("28000"), "{error:?}");
    assert!(error.message.contains("certificate"), "{error:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn read_only_procedures_cannot_commit_over_tls() -> TestResult {
    let writer = connect(&tls_params(tls_port(), with_ca(TlsMode::VerifyFull))).await?;
    run(
        writer.as_ref(),
        "DROP TABLE IF EXISTS varve_tls_pr_w; CREATE TABLE varve_tls_pr_w (a int);\n\
         CREATE OR REPLACE PROCEDURE public.varve_tls_pr() LANGUAGE plpgsql AS $$ BEGIN \
         PERFORM set_config('default_transaction_read_only', 'off', false); COMMIT; \
         INSERT INTO varve_tls_pr_w VALUES (1); END $$",
    )
    .await?;
    let mut params = tls_params(tls_port(), with_ca(TlsMode::VerifyFull));
    params.read_only = true;
    let session = connect(&params).await?;
    let error = run(session.as_ref(), "CALL public.varve_tls_pr()")
        .await
        .err()
        .ok_or("the procedure committed")?;
    assert_eq!(error.code.as_deref(), Some("2D000"), "{error:?}");
    assert_eq!(
        scalar(writer.as_ref(), "SELECT count(*) FROM varve_tls_pr_w").await?,
        Value::Int(0)
    );
    run(
        writer.as_ref(),
        "DROP TABLE varve_tls_pr_w; DROP PROCEDURE public.varve_tls_pr()",
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_dead_tls_session_fails_fast() -> TestResult {
    let session_params = tls_params(tls_port(), with_ca(TlsMode::VerifyFull));
    // The terminations come from a plain connection to the same server.
    let mut admin = params(tls_port());
    admin.tls = mode(TlsMode::Require);
    let session = connect(&session_params).await?;
    assert!(uses_tls(session.as_ref()).await?);
    dead_sessions_fail_fast(session_params, admin).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_dropped_tls_discard_all_runs_the_init_sql_again() -> TestResult {
    let far = latency_proxy(tls_port(), Duration::from_millis(5)).await?;
    let session_params = tls_params(far.port, mode(TlsMode::Require));
    let session = connect(&session_params).await?;
    assert!(uses_tls(session.as_ref()).await?);
    drop(session);
    let mut admin = params(tls_port());
    admin.tls = mode(TlsMode::Require);
    dropped_discard_all_keeps_the_init_sql(session_params, admin).await
}
