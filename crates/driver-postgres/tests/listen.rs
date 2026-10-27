//! Integration tests of `LISTEN` / `NOTIFY` on a dedicated connection
//! ([`varve_driver_postgres::listen`]), against the oldest and newest
//! supported servers.
//!
//! Endpoints: `VARVE_LISTEN_PG13_PORT` and `VARVE_LISTEN_PG18_PORT` (defaults
//! 56713 and 56718), `VARVE_PG_HOST`, user `varve`, password `varve_dev`,
//! database `varve`. Run with `--include-ignored` against a compose project
//! that publishes those ports. `VARVE_PG_SERVERS` (as in
//! `tests/common/mod.rs`) replaces both ports when it is set.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, ErrorKind, ExecOptions, Session, StatementResult, Value,
};
use varve_driver_postgres::listen::{MAX_CHANNEL_BYTES, MAX_PAYLOAD_BYTES, QueueBudget};
use varve_driver_postgres::{
    ListenConnector, ListenTarget, ListenerEvents, ListenerOptions, ListenerStatus, Notification,
    PgListener, PostgresDriver, TargetError,
};
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const WAIT: Duration = Duration::from_secs(10);

fn host() -> String {
    std::env::var("VARVE_PG_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

/// `(major, port)` of the servers under test, oldest first.
///
/// `VARVE_PG_SERVERS` (for example `18:54318`, as CI sets it for every
/// integration test of this crate) replaces the listen ports; without it
/// the listen test servers run.
fn servers() -> Vec<(u32, u16)> {
    if std::env::var_os("VARVE_PG_SERVERS").is_some() {
        return common::servers();
    }
    vec![
        (13, port("VARVE_LISTEN_PG13_PORT", 56713)),
        (18, port("VARVE_LISTEN_PG18_PORT", 56718)),
    ]
}

fn params_at(host: &str, port: u16) -> ConnectParams {
    let mut params = ConnectParams::new(Engine::Postgres, host, port);
    params.database = Some("varve".into());
    params.username = Some("varve".into());
    params.password = Some(Secret::new("varve_dev".to_string()));
    params.connect_timeout = Duration::from_secs(5);
    params.application_name = "varve listen tests".into();
    params
}

fn params(port: u16) -> ConnectParams {
    params_at(&host(), port)
}

/// Hands out whatever parameters the test sets, counting the calls.
struct Fixed {
    params: Mutex<ConnectParams>,
    calls: Mutex<u32>,
}

impl Fixed {
    fn new(params: ConnectParams) -> Arc<Self> {
        Arc::new(Self {
            params: Mutex::new(params),
            calls: Mutex::new(0),
        })
    }

    fn set(&self, params: ConnectParams) {
        if let Ok(mut current) = self.params.lock() {
            *current = params;
        }
    }

    fn target(&self) -> ListenTarget {
        ListenTarget {
            params: self
                .params
                .lock()
                .map(|p| p.clone())
                .unwrap_or_else(|p| p.into_inner().clone()),
            guard: None,
        }
    }
}

#[async_trait::async_trait]
impl ListenConnector for Fixed {
    async fn target(&self) -> Result<ListenTarget, TargetError> {
        if let Ok(mut calls) = self.calls.lock() {
            *calls += 1;
        }
        Ok(Fixed::target(self))
    }
}

fn fast() -> ListenerOptions {
    ListenerOptions {
        backoff_initial: Duration::from_millis(100),
        backoff_max: Duration::from_millis(400),
        ..ListenerOptions::default()
    }
}

async fn start(
    connector: &Arc<Fixed>,
    options: ListenerOptions,
) -> Result<(PgListener, ListenerEvents), Box<dyn std::error::Error>> {
    let first = connector.target();
    let dynamic: Arc<dyn ListenConnector> = Arc::clone(connector) as Arc<dyn ListenConnector>;
    Ok(PgListener::start(first, dynamic, options).await?)
}

async fn session(port: u16) -> Result<Box<dyn Session>, Box<dyn std::error::Error>> {
    Ok(PostgresDriver::new().connect(&params(port)).await?)
}

async fn run(session: &dyn Session, sql: &str) -> Result<(), Box<dyn std::error::Error>> {
    session.execute(sql, &ExecOptions::default()).await?;
    Ok(())
}

async fn scalar_int(session: &dyn Session, sql: &str) -> Result<i64, Box<dyn std::error::Error>> {
    let output = session.execute(sql, &ExecOptions::default()).await?;
    match output.results.first() {
        Some(StatementResult::Rows { rows, .. }) => match rows.first().and_then(|r| r.first()) {
            Some(Value::Int(value)) => Ok(*value),
            other => Err(format!("not an integer: {other:?}").into()),
        },
        other => Err(format!("no rows: {other:?}").into()),
    }
}

async fn next_note(
    events: &mut ListenerEvents,
) -> Result<Notification, Box<dyn std::error::Error>> {
    Ok(tokio::time::timeout(WAIT, events.recv())
        .await?
        .ok_or("the notification stream ended")?)
}

async fn next_status(
    events: &mut ListenerEvents,
) -> Result<ListenerStatus, Box<dyn std::error::Error>> {
    Ok(tokio::time::timeout(WAIT, events.status.recv())
        .await?
        .ok_or("the status stream ended")?)
}

/// The process id of the first `Connected` status.
async fn connected_pid(events: &mut ListenerEvents) -> Result<i32, Box<dyn std::error::Error>> {
    match next_status(events).await? {
        ListenerStatus::Connected { process_id, .. } => Ok(process_id),
        other => Err(format!("expected Connected, got {other:?}").into()),
    }
}

/// Waits for the next `Connected`, skipping `Reconnecting`; returns it.
async fn reconnected(
    events: &mut ListenerEvents,
) -> Result<(i32, Vec<ListenerStatus>), Box<dyn std::error::Error>> {
    let mut seen = Vec::new();
    loop {
        let status = next_status(events).await?;
        match &status {
            ListenerStatus::Connected {
                process_id,
                reconnected,
                ..
            } => {
                assert!(*reconnected, "a reconnect says so");
                return Ok((*process_id, seen));
            }
            ListenerStatus::Reconnecting { .. } => seen.push(status),
            ListenerStatus::Stopped { error } => {
                return Err(format!("stopped instead of reconnecting: {error:?}").into());
            }
        }
    }
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn receives_notifications_with_payload_and_sender() -> TestResult {
    for (major, port) in servers() {
        let connector = Fixed::new(params(port));
        let (listener, mut events) = start(&connector, ListenerOptions::default()).await?;
        let own = connected_pid(&mut events).await?;
        // A name that needs quoting, with an embedded quote and mixed case.
        let channel = "Orders \"eu\"; x";
        listener.listen(channel).await?;
        listener.listen(channel).await?; // twice is fine
        assert_eq!(listener.channels(), [channel]);

        let sender = session(port).await?;
        let sender_pid = scalar_int(sender.as_ref(), "SELECT pg_backend_pid()").await?;
        // The lower-case spelling is another channel.
        run(sender.as_ref(), "NOTIFY orders").await?;
        run(
            sender.as_ref(),
            "SELECT pg_notify('Orders \"eu\"; x', 'héllo, wörld')",
        )
        .await?;
        let note = next_note(&mut events).await?;
        assert_eq!(note.channel, channel, "PostgreSQL {major}");
        assert_eq!(note.payload, "héllo, wörld");
        assert_eq!(i64::from(note.process_id), sender_pid);
        assert!(!note.from_self);
        assert_ne!(note.process_id, own);

        // Sent by the listener itself: delivered at once, marked as own.
        listener.notify(channel, "").await?;
        let own_note = next_note(&mut events).await?;
        assert_eq!(own_note.process_id, own);
        assert!(own_note.from_self);
        assert_eq!(own_note.payload, "");

        // Notifications inside a transaction arrive at its commit, in order.
        run(
            sender.as_ref(),
            "BEGIN; NOTIFY \"Orders \"\"eu\"\"; x\", '1'; NOTIFY \"Orders \"\"eu\"\"; x\", '2'; COMMIT",
        )
        .await?;
        assert_eq!(next_note(&mut events).await?.payload, "1");
        assert_eq!(next_note(&mut events).await?.payload, "2");
        assert!(events.try_recv().is_none(), "nothing for `orders`");

        sender.close().await?;
        listener.close().await;
        assert!(matches!(
            next_status(&mut events).await?,
            ListenerStatus::Stopped { error: None }
        ));
        assert!(listener.is_stopped());
        assert_eq!(
            listener.listen("x").await.err().map(|e| e.kind),
            Some(ErrorKind::ConnectionLost)
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn unlisten_stops_delivery() -> TestResult {
    for (_, port) in servers() {
        let connector = Fixed::new(params(port));
        let (listener, mut events) = start(&connector, ListenerOptions::default()).await?;
        connected_pid(&mut events).await?;
        listener.listen("first").await?;
        listener.listen("second").await?;
        listener.unlisten("first").await?;
        listener.unlisten("never").await?; // not listened: a no-op
        assert_eq!(listener.channels(), ["second"]);
        let sender = session(port).await?;
        run(
            sender.as_ref(),
            "NOTIFY first, 'gone'; NOTIFY second, 'here'",
        )
        .await?;
        let note = next_note(&mut events).await?;
        assert_eq!(
            (note.channel.as_str(), note.payload.as_str()),
            ("second", "here")
        );
        sender.close().await?;
        listener.close().await;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn refuses_bad_names_payloads_and_read_only_sends() -> TestResult {
    for (_, port) in servers() {
        let connector = Fixed::new(params(port));
        let (listener, mut events) = start(&connector, ListenerOptions::default()).await?;
        connected_pid(&mut events).await?;
        let long = "c".repeat(MAX_CHANNEL_BYTES + 1);
        assert!(listener.listen(&long).await.is_err());
        assert!(listener.listen("").await.is_err());
        assert!(
            listener
                .notify("c", &"p".repeat(MAX_PAYLOAD_BYTES + 1))
                .await
                .is_err()
        );
        // The longest name and payload the server takes go through.
        let longest = "c".repeat(MAX_CHANNEL_BYTES);
        listener.listen(&longest).await?;
        listener
            .notify(&longest, &"p".repeat(MAX_PAYLOAD_BYTES))
            .await?;
        let note = next_note(&mut events).await?;
        assert_eq!(note.channel, longest);
        assert_eq!(note.payload.len(), MAX_PAYLOAD_BYTES);
        listener.close().await;

        let mut read_only = params(port);
        read_only.read_only = true;
        let connector = Fixed::new(read_only);
        let (listener, mut events) = start(&connector, ListenerOptions::default()).await?;
        connected_pid(&mut events).await?;
        listener.listen("ro").await?; // listening is fine on a read-only connection
        let refused = listener.notify("ro", "x").await.err().map(|e| e.kind);
        assert_eq!(refused, Some(ErrorKind::ReadOnly));
        assert!(
            tokio::time::timeout(Duration::from_millis(300), events.recv())
                .await
                .is_err(),
            "nothing was sent"
        );
        listener.close().await;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn the_first_connect_fails_fast() -> TestResult {
    // A port nothing listens on.
    let probe = TcpListener::bind("127.0.0.1:0").await?;
    let closed_port = probe.local_addr()?.port();
    drop(probe);
    let connector = Fixed::new(params_at("127.0.0.1", closed_port));
    let error = start(&connector, ListenerOptions::default()).await.err();
    assert!(error.is_some());

    let (_, port) = servers()[0];
    let mut wrong = params(port);
    wrong.password = Some(Secret::new("not-the-password".to_string()));
    let connector = Fixed::new(wrong);
    let error = start(&connector, ListenerOptions::default())
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(!error.contains("not-the-password"), "{error}");
    assert!(!error.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn reconnects_and_listens_again_after_the_backend_is_terminated() -> TestResult {
    for (major, port) in servers() {
        let connector = Fixed::new(params(port));
        let (listener, mut events) = start(&connector, fast()).await?;
        let first = connected_pid(&mut events).await?;
        listener.listen("Jobs").await?;
        listener.listen("other").await?;
        let admin = session(port).await?;
        run(
            admin.as_ref(),
            &format!("SELECT pg_terminate_backend({first})"),
        )
        .await?;
        let (second, seen) = reconnected(&mut events).await?;
        assert_ne!(first, second, "PostgreSQL {major}: a new backend");
        assert!(
            matches!(
                seen.first(),
                Some(ListenerStatus::Reconnecting { attempt: 0, .. })
            ),
            "{seen:?}"
        );
        assert_eq!(listener.channels(), ["Jobs", "other"]);
        // The new backend listens to both channels again.
        let listening = scalar_int(
            admin.as_ref(),
            &format!(
                "SELECT count(*) FROM pg_stat_activity WHERE pid = {second} AND state = 'idle'"
            ),
        )
        .await?;
        assert_eq!(listening, 1);
        run(admin.as_ref(), "NOTIFY \"Jobs\", 'after'").await?;
        let note = next_note(&mut events).await?;
        assert_eq!(
            (note.channel.as_str(), note.payload.as_str()),
            ("Jobs", "after")
        );
        // The idle session timeout cannot close the listener (14 and newer).
        if major >= 14 {
            run(
                admin.as_ref(),
                "ALTER ROLE varve SET idle_session_timeout = '200ms'",
            )
            .await?;
            run(
                admin.as_ref(),
                &format!("SELECT pg_terminate_backend({second})"),
            )
            .await?;
            let (third, _) = reconnected(&mut events).await?;
            tokio::time::sleep(Duration::from_millis(800)).await;
            run(
                admin.as_ref(),
                "ALTER ROLE varve RESET idle_session_timeout",
            )
            .await?;
            assert!(
                events.status.try_recv().is_err(),
                "the listener on backend {third} stayed connected"
            );
            run(admin.as_ref(), "NOTIFY other").await?;
            assert_eq!(next_note(&mut events).await?.channel, "other");
        }
        admin.close().await?;
        listener.close().await;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn channel_changes_while_reconnecting_are_applied() -> TestResult {
    let (_, port) = servers()[servers().len() - 1];
    let connector = Fixed::new(params(port));
    let options = ListenerOptions {
        backoff_initial: Duration::from_millis(600),
        backoff_max: Duration::from_millis(600),
        ..ListenerOptions::default()
    };
    let (listener, mut events) = start(&connector, options).await?;
    let first = connected_pid(&mut events).await?;
    listener.listen("old").await?;
    let admin = session(port).await?;
    run(
        admin.as_ref(),
        &format!("SELECT pg_terminate_backend({first})"),
    )
    .await?;
    match next_status(&mut events).await? {
        ListenerStatus::Reconnecting { .. } => {}
        other => return Err(format!("expected Reconnecting, got {other:?}").into()),
    }
    // While down: channel changes are recorded, sends are refused.
    listener.listen("new").await?;
    listener.unlisten("old").await?;
    assert_eq!(
        listener.notify("new", "x").await.err().map(|e| e.kind),
        Some(ErrorKind::ConnectionLost)
    );
    reconnected(&mut events).await?;
    assert_eq!(listener.channels(), ["new"]);
    run(admin.as_ref(), "NOTIFY old; NOTIFY new, 'n'").await?;
    let note = next_note(&mut events).await?;
    assert_eq!(note.channel, "new");
    admin.close().await?;
    listener.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn gives_up_when_the_password_no_longer_works() -> TestResult {
    let (_, port) = servers()[0];
    let connector = Fixed::new(params(port));
    let (listener, mut events) = start(&connector, fast()).await?;
    let pid = connected_pid(&mut events).await?;
    let mut wrong = params(port);
    wrong.password = Some(Secret::new("changed-password".to_string()));
    connector.set(wrong);
    let admin = session(port).await?;
    run(
        admin.as_ref(),
        &format!("SELECT pg_terminate_backend({pid})"),
    )
    .await?;
    loop {
        match next_status(&mut events).await? {
            ListenerStatus::Reconnecting { .. } => {}
            ListenerStatus::Stopped { error } => {
                let error = error.ok_or("stopped without a reason")?;
                assert_eq!(error.kind, ErrorKind::Auth, "{error:?}");
                assert!(!error.message.contains("changed-password"));
                break;
            }
            other @ ListenerStatus::Connected { .. } => {
                return Err(format!("connected with a wrong password: {other:?}").into());
            }
        }
    }
    assert!(listener.is_stopped());
    let calls = connector.calls.lock().map(|c| *c).unwrap_or_default();
    assert_eq!(calls, 1, "no retry after an authentication failure");
    admin.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn a_full_buffer_drops_and_counts_without_stalling() -> TestResult {
    for (_, port) in servers() {
        let connector = Fixed::new(params(port));
        let options = ListenerOptions {
            buffer: 2,
            ..ListenerOptions::default()
        };
        let (listener, mut events) = start(&connector, options).await?;
        connected_pid(&mut events).await?;
        listener.listen("burst").await?;
        let sender = session(port).await?;
        run(
            sender.as_ref(),
            "SELECT pg_notify('burst', g::text) FROM generate_series(1, 10) AS g",
        )
        .await?;
        let mut dropped = 0;
        for _ in 0..100 {
            dropped += listener.take_dropped();
            if dropped >= 8 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(dropped, 8);
        assert_eq!(next_note(&mut events).await?.payload, "1");
        assert_eq!(next_note(&mut events).await?.payload, "2");
        // The connection kept reading: a later notification arrives.
        run(sender.as_ref(), "NOTIFY burst, 'later'").await?;
        assert_eq!(next_note(&mut events).await?.payload, "later");
        assert_eq!(listener.take_dropped(), 0);
        sender.close().await?;
        listener.close().await;
    }
    Ok(())
}

/// A TCP proxy that can be frozen: while frozen it forwards nothing and
/// closes nothing (a network that silently drops packets).
struct FreezingProxy {
    port: u16,
    frozen: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Drop for FreezingProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn forward(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    frozen: Arc<AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let Ok(read) = from.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            let _ = to.shutdown().await;
            return;
        }
        while frozen.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if to.write_all(&buffer[..read]).await.is_err() {
            return;
        }
    }
}

async fn freezing_proxy(target: u16) -> std::io::Result<FreezingProxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let frozen = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&frozen);
    let (parked_tx, mut parked_rx) = mpsc::unbounded_channel::<JoinHandle<()>>();
    let task = tokio::spawn(async move {
        let mut parked = Vec::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((client, _)) = accepted else { return };
                    let Ok(server) = TcpStream::connect((host(), target)).await else {
                        continue;
                    };
                    let (client_read, client_write) = client.into_split();
                    let (server_read, server_write) = server.into_split();
                    let _ = parked_tx.send(tokio::spawn(forward(client_read, server_write, Arc::clone(&flag))));
                    let _ = parked_tx.send(tokio::spawn(forward(server_read, client_write, Arc::clone(&flag))));
                }
                Some(handle) = parked_rx.recv() => parked.push(handle),
            }
        }
    });
    Ok(FreezingProxy { port, frozen, task })
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn a_silent_network_is_found_by_the_ping() -> TestResult {
    let (_, port) = servers()[servers().len() - 1];
    let proxy = freezing_proxy(port).await?;
    let connector = Fixed::new(params_at("127.0.0.1", proxy.port));
    let options = ListenerOptions {
        ping_interval: Duration::from_millis(200),
        call_timeout: Duration::from_millis(500),
        backoff_initial: Duration::from_millis(100),
        backoff_max: Duration::from_millis(200),
        ..ListenerOptions::default()
    };
    let (listener, mut events) = start(&connector, options).await?;
    let first = connected_pid(&mut events).await?;
    listener.listen("ping").await?;
    proxy.frozen.store(true, Ordering::Release);
    match next_status(&mut events).await? {
        ListenerStatus::Reconnecting { error, .. } => {
            assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
        }
        other => return Err(format!("expected Reconnecting, got {other:?}").into()),
    }
    proxy.frozen.store(false, Ordering::Release);
    let (second, _) = reconnected(&mut events).await?;
    assert_ne!(first, second);
    let admin = session(port).await?;
    run(admin.as_ref(), "NOTIFY ping, 'back'").await?;
    assert_eq!(next_note(&mut events).await?.payload, "back");
    admin.close().await?;
    listener.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn dropping_the_handle_closes_the_connection() -> TestResult {
    let (_, port) = servers()[0];
    let mut named = params(port);
    named.application_name = "varve listen drop test".into();
    let connector = Fixed::new(named);
    let (listener, mut events) = start(&connector, ListenerOptions::default()).await?;
    let pid = connected_pid(&mut events).await?;
    drop(listener);
    assert!(matches!(
        next_status(&mut events).await?,
        ListenerStatus::Stopped { error: None }
    ));
    let admin = session(port).await?;
    let mut gone = false;
    for _ in 0..50 {
        let left = scalar_int(
            admin.as_ref(),
            &format!("SELECT count(*) FROM pg_stat_activity WHERE pid = {pid}"),
        )
        .await?;
        if left == 0 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(gone, "the backend ended");
    admin.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn notifications_that_are_not_utf8_do_not_drop_the_connection() -> TestResult {
    for (major, port) in servers() {
        let admin = session(port).await?;
        run(admin.as_ref(), "DROP DATABASE IF EXISTS b2_listen_ascii").await?;
        run(
            admin.as_ref(),
            "CREATE DATABASE b2_listen_ascii ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C' \
             TEMPLATE template0",
        )
        .await?;
        let mut ascii = params(port);
        ascii.database = Some("b2_listen_ascii".into());
        let connector = Fixed::new(ascii.clone());
        let (listener, mut events) = start(&connector, ListenerOptions::default()).await?;
        connected_pid(&mut events).await?;
        listener.listen("c").await?;
        let sender = PostgresDriver::new().connect(&ascii).await?;
        // Byte 0xE9 alone is not UTF-8; SQL_ASCII passes it on unchecked.
        run(sender.as_ref(), "SELECT pg_notify('c', E'caf\\xe9')").await?;
        run(sender.as_ref(), "NOTIFY c, 'fine'").await?;
        let first = match tokio::time::timeout(WAIT, events.recv()).await {
            Ok(Some(note)) => note,
            other => {
                let status = events.status.try_recv();
                return Err(format!("no notification ({other:?}), status {status:?}").into());
            }
        };
        assert_eq!(first.payload, "caf\u{fffd}", "PostgreSQL {major}");
        assert!(first.lossy);
        let second = next_note(&mut events).await?;
        assert_eq!(second.payload, "fine");
        assert!(!second.lossy);
        assert!(
            events.status.try_recv().is_err(),
            "the connection was never lost"
        );
        sender.close().await?;
        listener.close().await;
        drop(events);
        run(admin.as_ref(), "DROP DATABASE IF EXISTS b2_listen_ascii").await?;
        admin.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn a_shared_budget_caps_what_waits_across_listeners() -> TestResult {
    let (_, port) = servers()[0];
    // Room for about two 1000 byte notifications in all.
    let budget = QueueBudget::new(2 * (1000 + 1 + 64) + 10);
    let options = ListenerOptions {
        budget: Some(Arc::clone(&budget)),
        ..ListenerOptions::default()
    };
    let connector = Fixed::new(params(port));
    let (one, mut one_events) = start(&connector, options.clone()).await?;
    let (two, mut two_events) = start(&connector, options).await?;
    connected_pid(&mut one_events).await?;
    connected_pid(&mut two_events).await?;
    one.listen("b").await?;
    two.listen("b").await?;
    let sender = session(port).await?;
    run(
        sender.as_ref(),
        // Distinct payloads: equal notifications in one transaction are sent once.
        "SELECT pg_notify('b', repeat('x', 999) || g) FROM generate_series(1, 3) AS g",
    )
    .await?;
    let mut dropped = 0;
    for _ in 0..100 {
        dropped += one.take_dropped() + two.take_dropped();
        if dropped >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(dropped, 4, "6 sent, room for 2");
    assert!(budget.used() > 0);
    let mut read = 0;
    while one_events.try_recv().is_some() || two_events.try_recv().is_some() {
        read += 1;
    }
    assert_eq!(read, 2);
    assert_eq!(budget.used(), 0, "reading gives the room back");
    one.close().await;
    two.close().await;
    sender.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the listen test servers"]
async fn close_returns_once_the_backend_has_ended() -> TestResult {
    for (major, port) in servers() {
        let connector = Fixed::new(params(port));
        let (listener, mut events) = start(&connector, ListenerOptions::default()).await?;
        let pid = connected_pid(&mut events).await?;
        listener.listen("x").await?;
        let admin = session(port).await?;
        listener.close().await;
        let left = scalar_int(
            admin.as_ref(),
            &format!("SELECT count(*) FROM pg_stat_activity WHERE pid = {pid}"),
        )
        .await?;
        assert_eq!(left, 0, "PostgreSQL {major}: no backend left after close");
        admin.close().await?;
    }
    Ok(())
}
