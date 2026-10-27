//! Partial output of failed console scripts (`DriverError::partial`)
//! against the dev server matrix (`dev/docker-compose.yml`).
//!
//! Run with `cargo test -p varve-driver-redis --test partial -- --include-ignored`.
//! Servers come from `VARVE_REDIS_SERVERS` like in `tests/matrix.rs`
//! (`redis-7.2@127.0.0.1:63072,valkey-8@127.0.0.1:63180`), by default
//! Redis 6.2, Redis 8.10 and Valkey 9 on their dev ports; the password from
//! `VARVE_REDIS_PASSWORD`. Tests use logical database 15 and keys under
//! `partial:`, which they remove.

use std::sync::Arc;
use std::time::Duration;

use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, DriverError, Durability, ErrorKind, ExecOptions, PartialOutput, Session,
    StatementResult, TlsMode,
};
use varve_driver_redis::RedisDriver;
use varve_secrets::Secret;

use Durability::{Committed, InTransaction};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The tests share database 15, and some block the whole server for a
/// moment (a busy script, `CLIENT PAUSE`), so they run one at a time, like
/// `tests/matrix.rs`.
static SERVER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `(label, host, port)` of every server to test.
fn servers() -> Vec<(String, String, u16)> {
    let list = std::env::var("VARVE_REDIS_SERVERS").unwrap_or_else(|_| {
        "redis-6.2@127.0.0.1:63062,redis-8.10@127.0.0.1:63081,valkey-9@127.0.0.1:63190".into()
    });
    list.split(',')
        .map(|item| {
            let (label, endpoint) = item
                .trim()
                .split_once('@')
                .unwrap_or_else(|| panic!("`{item}` must look like redis-7.2@host:port"));
            let (host, port) = endpoint
                .rsplit_once(':')
                .unwrap_or_else(|| panic!("`{endpoint}` must be host:port"));
            (
                label.to_owned(),
                host.to_owned(),
                port.parse()
                    .unwrap_or_else(|_| panic!("`{endpoint}` has a bad port")),
            )
        })
        .collect()
}

fn params(host: &str, port: u16) -> ConnectParams {
    let mut p = ConnectParams::new(Engine::Redis, host, port);
    p.password = Some(Secret::new(
        std::env::var("VARVE_REDIS_PASSWORD").unwrap_or_else(|_| "varve_dev".into()),
    ));
    p.redis_db = 15;
    p.tls.mode = TlsMode::Disable;
    p.connect_timeout = Duration::from_secs(5);
    p.application_name = "Varve Test".into();
    p
}

async fn connect(p: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
    RedisDriver::new().connect(p).await
}

async fn run(session: &dyn Session, text: &str) -> Result<Vec<StatementResult>, DriverError> {
    Ok(session
        .execute(text, &ExecOptions::default())
        .await?
        .results)
}

async fn fails(session: &dyn Session, text: &str) -> Result<DriverError, String> {
    match run(session, text).await {
        Ok(results) => Err(format!("`{text}` succeeded: {results:?}")),
        Err(error) => Ok(error),
    }
}

fn partial(error: &DriverError) -> Result<&PartialOutput, String> {
    error
        .partial
        .as_deref()
        .ok_or_else(|| format!("no partial output: {error:?}"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn failed_scripts_keep_what_ran_before() -> TestResult {
    let _guard = SERVER_LOCK.lock().await;
    for (label, host, port) in servers() {
        let session = connect(&params(&host, port)).await?;
        let s = session.as_ref();
        run(s, "DEL partial:a partial:m").await?;

        // The first command fails: nothing to report.
        run(s, "SET partial:a text").await?;
        let error = fails(s, "INCR partial:a\nSET partial:a 2").await?;
        assert!(error.partial.is_none(), "{label}: {error:?}");

        // Comments and blank lines are not commands.
        let error = fails(
            s,
            "# set it\nSET partial:a 1\n\n  # read it\nGET partial:a\nHSET partial:a f v\nGET partial:a",
        )
        .await?;
        assert_eq!(error.code.as_deref(), Some("WRONGTYPE"), "{label}");
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2), "{label}: {p:?}");
        assert_eq!(p.results.len(), 2);
        assert_eq!(p.completed, [Committed, Committed]);

        // A failure after EXEC: the block ran.
        let error = fails(
            s,
            "MULTI\nSET partial:m 1\nINCR partial:m\nEXEC\nHSET partial:m f v",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(4), "{label}: {p:?}");
        assert_eq!(p.completed, [Committed; 4]);

        // A failure inside MULTI: the queued commands are still queued.
        let error = fails(s, "MULTI\nSET partial:m 5\nNOSUCHCOMMAND x\nEXEC").await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2), "{label}: {p:?}");
        assert_eq!(p.completed, [InTransaction, InTransaction]);
        // The server refuses the block now; DISCARD ends it.
        run(s, "DISCARD").await?;
        // The value from the EXEC block above; the discarded SET never ran.
        let value = format!("{:?}", run(s, "GET partial:m").await?);
        assert!(value.contains("\"2\""), "{label}: {value}");
        run(s, "DEL partial:a partial:m").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_cancel_mid_script_keeps_what_ran() -> TestResult {
    let _guard = SERVER_LOCK.lock().await;
    for (label, host, port) in servers() {
        let session: Arc<dyn Session> = Arc::from(connect(&params(&host, port)).await?);
        let running = Arc::clone(&session);
        let task = tokio::spawn(async move {
            run(
                running.as_ref(),
                "SET partial:c 1\nBLPOP partial:never 30\nSET partial:d 1",
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        session.cancel_handle().cancel().await?;
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await??
            .err()
            .ok_or("the script finished")?;
        assert_eq!(error.kind, ErrorKind::Cancelled, "{label}: {error:?}");
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(1), "{label}: {p:?}");
        assert_eq!(p.completed, [Committed]);
        run(session.as_ref(), "DEL partial:c partial:d").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_timeout_mid_script_keeps_what_ran() -> TestResult {
    let _guard = SERVER_LOCK.lock().await;
    for (label, host, port) in servers() {
        let mut p = params(&host, port);
        p.statement_timeout = Some(Duration::from_millis(300));
        let session = connect(&p).await?;
        let error = fails(
            session.as_ref(),
            "MULTI\nSET partial:t 1\nEXEC\nBLPOP partial:never 5",
        )
        .await?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{label}: {error:?}");
        let partial = partial(&error)?;
        assert_eq!(partial.failed_statement, Some(3), "{label}: {partial:?}");
        assert_eq!(partial.completed, [Committed; 3]);
        run(session.as_ref(), "DEL partial:t").await?;
    }
    Ok(())
}

/// Audit PC-3: an EXEC whose answer was not read may have run, so its
/// queued commands are not reported as rolled back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn an_exec_without_an_answer_is_not_reported_rolled_back() -> TestResult {
    let _guard = SERVER_LOCK.lock().await;
    for (label, host, port) in servers() {
        let mut p = params(&host, port);
        p.statement_timeout = Some(Duration::from_millis(300));
        let session = connect(&p).await?;
        run(session.as_ref(), "DEL partial:w").await?;
        let error = fails(
            session.as_ref(),
            "MULTI\nSET partial:w 1\n\
             EVAL \"local e = tonumber(redis.call('TIME')[1]) + 2 repeat until tonumber(redis.call('TIME')[1]) >= e return 1\" 0\n\
             EXEC",
        )
        .await?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{label}: {error:?}");
        let partial = partial(&error)?;
        assert_eq!(partial.failed_statement, Some(3), "{label}: {partial:?}");
        assert_eq!(partial.completed, [Durability::Unknown; 3], "{label}");
        // The server ran the block after all.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let checker = connect(&params(&host, port)).await?;
        let value = format!("{:?}", run(checker.as_ref(), "GET partial:w").await?);
        assert!(value.contains("\"1\""), "{label}: {value}");
        run(checker.as_ref(), "DEL partial:w").await?;
    }
    Ok(())
}

/// Audit PC-6: a queued command whose EXEC element is an error changed
/// nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn exec_element_errors_are_reported_as_failed() -> TestResult {
    let _guard = SERVER_LOCK.lock().await;
    for (label, host, port) in servers() {
        let session = connect(&params(&host, port)).await?;
        let s = session.as_ref();
        run(s, "DEL partial:e").await?;
        let error = fails(
            s,
            "MULTI\nSET partial:e x\nINCR partial:e\nAPPEND partial:e y\nEXEC\nHSET partial:e f v",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(5), "{label}: {p:?}");
        assert_eq!(
            p.completed,
            [
                Committed,
                Committed,
                Durability::Failed,
                Committed,
                Committed
            ],
            "{label}"
        );
        run(s, "DEL partial:e").await?;
    }
    Ok(())
}
