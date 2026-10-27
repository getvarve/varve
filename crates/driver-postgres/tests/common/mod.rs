//! Shared helpers for the integration tests. Endpoints come from environment
//! variables with the defaults of `dev/docker-compose.yml`.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, DriverError, ErrorKind, ExecOptions, ExecOutput, Session,
    StatementResult, Value,
};
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

pub fn host() -> String {
    env_or("VARVE_PG_HOST", "127.0.0.1")
}

/// Servers to test as `(major version, port)`, oldest first.
///
/// `VARVE_PG_SERVERS=13:54313,15:54315,16:54316,17:54317,18:54318` runs the
/// whole matrix; without it the oldest and newest supported versions run.
pub fn servers() -> Vec<(u32, u16)> {
    if let Ok(list) = std::env::var("VARVE_PG_SERVERS") {
        let parsed: Option<Vec<(u32, u16)>> = list
            .split(',')
            .map(|item| {
                let (major, port) = item.trim().split_once(':')?;
                Some((major.parse().ok()?, port.parse().ok()?))
            })
            .collect();
        match parsed {
            Some(servers) if !servers.is_empty() => return servers,
            _ => panic!("VARVE_PG_SERVERS must look like `13:54313,18:54318`, got `{list}`"),
        }
    }
    vec![
        (13, port("VARVE_PG13_PORT", 54313)),
        (18, port("VARVE_PG18_PORT", 54318)),
    ]
}

/// The oldest server to test (the only one when a single server is set).
pub fn oldest() -> (u32, u16) {
    let servers = servers();
    servers[0]
}

/// The newest server to test (the only one when a single server is set).
pub fn newest() -> (u32, u16) {
    let servers = servers();
    servers[servers.len() - 1]
}

/// Port of the PostgreSQL 18 server with TLS from `tests/tls/compose.yml`.
pub fn tls_port() -> u16 {
    port("VARVE_PG_TLS_PORT", 54390)
}

pub fn password() -> String {
    env_or("VARVE_PG_PASSWORD", "varve_dev")
}

pub fn params(port: u16) -> ConnectParams {
    let mut params = ConnectParams::new(Engine::Postgres, host(), port);
    params.database = Some(env_or("VARVE_PG_DATABASE", "varve"));
    params.username = Some(env_or("VARVE_PG_USER", "varve"));
    params.password = Some(Secret::new(password()));
    params.connect_timeout = Duration::from_secs(10);
    params.application_name = "varve-driver-postgres tests".into();
    params
}

pub async fn connect(params: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
    PostgresDriver::new().connect(params).await
}

pub async fn run(session: &dyn Session, sql: &str) -> Result<ExecOutput, DriverError> {
    session.execute(sql, &ExecOptions::default()).await
}

/// The rows of the result at `index`.
pub fn rows(output: &ExecOutput, index: usize) -> Result<&Vec<Vec<Value>>, String> {
    match output.results.get(index) {
        Some(StatementResult::Rows { rows, .. }) => Ok(rows),
        other => Err(format!("result {index} has no rows: {other:?}")),
    }
}

/// The single value of a one-row, one-column result.
pub async fn scalar(session: &dyn Session, sql: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let output = run(session, sql).await?;
    let rows = rows(&output, 0)?;
    Ok(rows
        .first()
        .and_then(|row| row.first())
        .cloned()
        .ok_or("no value")?)
}

/// A TCP proxy in front of a server. When the server closes a connection,
/// the proxy waits `hold` before closing the client side, so a client that
/// waits for the server to close (a cancel request) is seen to wait.
pub struct Proxy {
    /// The port the proxy listens on, on 127.0.0.1.
    pub port: u16,
    task: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    delay: Duration,
    hold: Duration,
) {
    tokio::time::sleep(delay).await;
    let _ = tokio::io::copy(&mut from, &mut to).await;
    tokio::time::sleep(hold).await;
    let _ = to.shutdown().await;
}

/// Starts a [`Proxy`] to `host():target_port`.
pub async fn proxy(target_port: u16, hold: Duration) -> std::io::Result<Proxy> {
    delaying_proxy(target_port, Duration::ZERO, hold).await
}

/// A [`Proxy`] that also holds back what a new client sends for `delay`
/// (so a cancel request reaches the server late).
pub async fn delaying_proxy(
    target_port: u16,
    delay: Duration,
    hold: Duration,
) -> std::io::Result<Proxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = TcpStream::connect((host(), target_port)).await else {
                continue;
            };
            let (client_read, client_write) = client.into_split();
            let (server_read, server_write) = server.into_split();
            tokio::spawn(pipe(client_read, server_write, delay, Duration::ZERO));
            tokio::spawn(pipe(server_read, client_write, Duration::ZERO, hold));
        }
    });
    Ok(Proxy { port, task })
}

/// Reads one frontend message: `typed` messages have a type byte, the
/// first ones (SSLRequest, startup) do not. Returns the raw bytes.
async fn read_frame(
    from: &mut tokio::net::tcp::OwnedReadHalf,
    typed: bool,
) -> std::io::Result<Vec<u8>> {
    let mut head = vec![0u8; if typed { 5 } else { 4 }];
    from.read_exact(&mut head).await?;
    let l = &head[head.len() - 4..];
    let length = u32::from_be_bytes([l[0], l[1], l[2], l[3]]) as usize;
    let mut body = vec![0u8; length.saturating_sub(4)];
    from.read_exact(&mut body).await?;
    head.extend(body);
    Ok(head)
}

/// A simple query message for `sql`.
fn query_message(sql: &str) -> Vec<u8> {
    let mut out = vec![b'Q'];
    out.extend_from_slice(
        &u32::try_from(sql.len() + 5)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(sql.as_bytes());
    out.push(0);
    out
}

/// A [`Proxy`] that replaces the first simple query equal to `from` with
/// `to`, to inject a fault into a query the driver sends itself.
pub async fn rewriting_proxy(target_port: u16, from: &str, to: &str) -> std::io::Result<Proxy> {
    rewriting_proxy_rules(target_port, &[(from, to, 1)]).await
}

/// A [`Proxy`] that replaces simple queries equal to a rule's `from` with
/// its `to`, the first `times` times each.
pub async fn rewriting_proxy_rules(
    target_port: u16,
    rules: &[(&str, &str, usize)],
) -> std::io::Result<Proxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let rules: std::sync::Arc<Vec<(String, String, std::sync::atomic::AtomicUsize)>> =
        std::sync::Arc::new(
            rules
                .iter()
                .map(|(f, t, n)| ((*f).to_owned(), (*t).to_owned(), (*n).into()))
                .collect(),
        );
    let task = tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = TcpStream::connect((host(), target_port)).await else {
                continue;
            };
            let (mut client_read, client_write) = client.into_split();
            let (server_read, mut server_write) = server.into_split();
            tokio::spawn(pipe(
                server_read,
                client_write,
                Duration::ZERO,
                Duration::ZERO,
            ));
            let rules = std::sync::Arc::clone(&rules);
            tokio::spawn(async move {
                let mut untyped = true;
                loop {
                    let Ok(mut frame) = read_frame(&mut client_read, !untyped).await else {
                        break;
                    };
                    if untyped {
                        // An SSLRequest (code 80877103) is followed by the startup.
                        untyped = frame.get(4..8) == Some(&80_877_103u32.to_be_bytes()[..]);
                    } else if frame[0] == b'Q' {
                        let query = frame.get(5..frame.len() - 1).unwrap_or_default().to_vec();
                        for (from, to, left) in rules.iter() {
                            if query == from.as_bytes() && take_one(left) {
                                frame = query_message(to);
                                break;
                            }
                        }
                    }
                    if server_write.write_all(&frame).await.is_err() {
                        break;
                    }
                }
                let _ = server_write.shutdown().await;
            });
        }
    });
    Ok(Proxy { port, task })
}

/// The backend `pid` of `session`.
pub async fn backend_pid(session: &dyn Session) -> Result<i64, Box<dyn std::error::Error>> {
    match scalar(session, "SELECT pg_backend_pid()").await? {
        Value::Int(pid) => Ok(pid),
        other => Err(format!("pid is not an integer: {other:?}").into()),
    }
}

/// A [`Proxy`] whose connections can all be cut at once, without any
/// message from the server (a firewall or NAT dropping the connection).
pub struct CuttingProxy {
    /// The proxy itself.
    pub proxy: Proxy,
    cut: tokio::sync::watch::Sender<bool>,
}

impl CuttingProxy {
    /// Closes every connection through the proxy, both sides.
    pub fn cut(&self) {
        let _ = self.cut.send(true);
    }
}

/// Starts a [`CuttingProxy`] to `host():target_port`.
pub async fn cutting_proxy(target_port: u16) -> std::io::Result<CuttingProxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let (cut, cut_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let mut cut_rx = cut_rx.clone();
            tokio::spawn(async move {
                let Ok(server) = TcpStream::connect((host(), target_port)).await else {
                    return;
                };
                let (mut client_read, mut client_write) = client.into_split();
                let (mut server_read, mut server_write) = server.into_split();
                // Dropping both sockets when cut closes them.
                tokio::select! {
                    _ = tokio::io::copy(&mut server_read, &mut client_write) => {}
                    _ = tokio::io::copy(&mut client_read, &mut server_write) => {}
                    _ = cut_rx.wait_for(|cut| *cut) => {}
                }
            });
        }
    });
    Ok(CuttingProxy {
        proxy: Proxy { port, task },
        cut,
    })
}

/// Reads one backend message (type byte, length, body).
async fn read_backend_frame(from: &mut tokio::net::tcp::OwnedReadHalf) -> std::io::Result<Vec<u8>> {
    read_frame(from, true).await
}

/// `frame` (an `ErrorResponse`) with its message field (`M`) replaced by
/// `translate(message)` when that gives a new one, for errors the server
/// reports itself (routine `ProcessInterrupts`): a message user code raises
/// is its own text, which a server never translates.
fn translate_error(frame: &[u8], translate: &dyn Fn(&str) -> Option<String>) -> Vec<u8> {
    let mut fields: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut rest = &frame[5..];
    while let Some((&code, tail)) = rest.split_first() {
        if code == 0 {
            break;
        }
        let end = tail.iter().position(|b| *b == 0).unwrap_or(tail.len());
        fields.push((code, tail[..end].to_vec()));
        rest = tail.get(end + 1..).unwrap_or_default();
    }
    let from_server = fields
        .iter()
        .any(|(code, value)| *code == b'R' && value == b"ProcessInterrupts");
    let mut body = Vec::new();
    for (code, value) in fields {
        let value = if code == b'M' && from_server {
            translate(&String::from_utf8_lossy(&value)).map_or(value, String::into_bytes)
        } else {
            value
        };
        body.push(code);
        body.extend(value);
        body.push(0);
    }
    body.push(0);
    let mut out = vec![frame[0]];
    out.extend_from_slice(
        &u32::try_from(body.len() + 4)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend(body);
    out
}

/// What [`translating_proxy`] makes of the server's untranslated
/// statement timeout message (as a German server would send it).
pub const TRANSLATED_TIMEOUT: &str = "storniere Anfrage wegen Zeitüberschreitung der Anweisung";

/// What [`translating_proxy`] makes of the server's untranslated cancel
/// request message.
pub const TRANSLATED_CANCEL: &str = "storniere Anfrage wegen Benutzeraufforderung";

/// A [`Proxy`] that translates the server's `57014` messages, as a server
/// with `lc_messages` set to German and translations built in does. The
/// client must not ask for TLS (`TlsMode::Disable`): the proxy reads the
/// backend messages from the first byte.
pub async fn translating_proxy(target_port: u16) -> std::io::Result<Proxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = TcpStream::connect((host(), target_port)).await else {
                continue;
            };
            let (client_read, mut client_write) = client.into_split();
            let (mut server_read, server_write) = server.into_split();
            tokio::spawn(pipe(
                client_read,
                server_write,
                Duration::ZERO,
                Duration::ZERO,
            ));
            tokio::spawn(async move {
                let translate = |message: &str| match message {
                    "canceling statement due to statement timeout" => {
                        Some(TRANSLATED_TIMEOUT.to_owned())
                    }
                    "canceling statement due to user request" => Some(TRANSLATED_CANCEL.to_owned()),
                    _ => None,
                };
                while let Ok(frame) = read_backend_frame(&mut server_read).await {
                    let frame = if frame[0] == b'E' {
                        translate_error(&frame, &translate)
                    } else {
                        frame
                    };
                    if client_write.write_all(&frame).await.is_err() {
                        break;
                    }
                }
                let _ = client_write.shutdown().await;
            });
        }
    });
    Ok(Proxy { port, task })
}

/// How long a call on a dead session may take: it must not wait for a
/// reply that cannot come (the bound is loose for slow CI machines; the
/// calls take well under a millisecond).
pub const FAIL_FAST: Duration = Duration::from_millis(150);

/// Asserts that calls on `session`, whose connection is gone, fail at once
/// with `ConnectionLost`, and that it reports no transaction status.
pub async fn assert_dead(session: &dyn Session, case: &str) -> TestResult {
    assert_eq!(session.in_transaction(), None, "{case}: before a call");
    for call in 1..=3 {
        let started = Instant::now();
        let error = run(session, "SELECT 1")
            .await
            .err()
            .ok_or(format!("{case}: call {call} succeeded"))?;
        let took = started.elapsed();
        assert_eq!(error.kind, ErrorKind::ConnectionLost, "{case}: {error:?}");
        assert!(took < FAIL_FAST, "{case}: call {call} took {took:?}");
        assert_eq!(session.in_transaction(), None, "{case}: after call {call}");
    }
    let started = Instant::now();
    let error = session.ping().await.err().ok_or(format!("{case}: ping"))?;
    assert_eq!(error.kind, ErrorKind::ConnectionLost, "{case}: {error:?}");
    assert!(started.elapsed() < FAIL_FAST, "{case}: ping");
    Ok(())
}

/// Waits until `session` notices that its connection has ended (the
/// server's FATAL message and the closed socket arrive in the background).
pub async fn wait_until_dead(session: &dyn Session, case: &str) -> TestResult {
    let started = Instant::now();
    while session.in_transaction().is_some() {
        if started.elapsed() > Duration::from_secs(5) {
            return Err(format!("{case}: the session never noticed the end").into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

/// Calls on a session whose backend was terminated or timed out fail at
/// once, also while the session lock is held by a statement that the
/// termination interrupts, and the transaction status turns unknown.
pub async fn dead_sessions_fail_fast(
    base: ConnectParams,
    admin_params: ConnectParams,
) -> TestResult {
    let admin = connect(&admin_params).await?;
    for read_only in [false, true] {
        let mut params = base.clone();
        params.read_only = read_only;
        for (setup, in_tx) in [
            ("", Some(false)),
            ("BEGIN", Some(true)),
            ("BEGIN; SELECT 1/0", Some(true)),
        ] {
            let case = format!("read_only={read_only} terminated after {setup:?}");
            let session = connect(&params).await?;
            let pid = backend_pid(session.as_ref()).await?;
            if !setup.is_empty() {
                let _ = run(session.as_ref(), setup).await;
            }
            assert_eq!(session.in_transaction(), in_tx, "{case}");
            run(
                admin.as_ref(),
                &format!("SELECT pg_terminate_backend({pid})"),
            )
            .await?;
            wait_until_dead(session.as_ref(), &case).await?;
            assert_dead(session.as_ref(), &case).await?;
        }
        for sql in ["SELECT pg_sleep(10)", "BEGIN; SELECT pg_sleep(10)"] {
            let case = format!("read_only={read_only} terminated while running {sql:?}");
            let session: Arc<dyn Session> = Arc::from(connect(&params).await?);
            let pid = backend_pid(session.as_ref()).await?;
            let running = Arc::clone(&session);
            let task = tokio::spawn(async move { run(running.as_ref(), sql).await });
            tokio::time::sleep(Duration::from_millis(300)).await;
            let terminated = Instant::now();
            run(
                admin.as_ref(),
                &format!("SELECT pg_terminate_backend({pid})"),
            )
            .await?;
            let error = tokio::time::timeout(Duration::from_secs(5), task)
                .await??
                .err()
                .ok_or(format!("{case}: finished"))?;
            let took = terminated.elapsed();
            assert_eq!(error.kind, ErrorKind::ConnectionLost, "{case}: {error:?}");
            assert!(took < Duration::from_secs(1), "{case}: took {took:?}");
            assert_dead(session.as_ref(), &case).await?;
        }
        let case = format!("read_only={read_only} idle_in_transaction_session_timeout");
        let session = connect(&params).await?;
        run(
            session.as_ref(),
            "SET idle_in_transaction_session_timeout = 200; BEGIN",
        )
        .await?;
        assert_eq!(session.in_transaction(), Some(true), "{case}");
        wait_until_dead(session.as_ref(), &case).await?;
        assert_dead(session.as_ref(), &case).await?;
    }
    Ok(())
}

/// A [`Proxy`] that holds back every chunk the server sends for `latency`,
/// like a server further away, so a call can be given up between a reply
/// and what the driver does next.
pub async fn latency_proxy(target_port: u16, latency: Duration) -> std::io::Result<Proxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = TcpStream::connect((host(), target_port)).await else {
                continue;
            };
            let (client_read, mut client_write) = client.into_split();
            let (mut server_read, server_write) = server.into_split();
            tokio::spawn(pipe(
                client_read,
                server_write,
                Duration::ZERO,
                Duration::ZERO,
            ));
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    let read = match server_read.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => read,
                    };
                    tokio::time::sleep(latency).await;
                    if client_write.write_all(&buffer[..read]).await.is_err() {
                        break;
                    }
                }
                let _ = client_write.shutdown().await;
            });
        }
    });
    Ok(Proxy { port, task })
}

/// A `DISCARD ALL` whose call is given up at any point (also after the
/// server ran it, before the session reopened) still ends with the session
/// reopened: what the init SQL set (a setting, a role) is back. `base` must
/// reach the server through a [`latency_proxy`]; `admin` connects directly
/// and creates the role.
pub async fn dropped_discard_all_keeps_the_init_sql(
    base: ConnectParams,
    admin_params: ConnectParams,
) -> TestResult {
    let admin = connect(&admin_params).await?;
    let role = "varve_init_role";
    run(
        admin.as_ref(),
        &format!(
            "DO $$ BEGIN CREATE ROLE {role}; EXCEPTION WHEN duplicate_object THEN NULL; END $$"
        ),
    )
    .await?;
    let mut params = base;
    params.statement_timeout = Some(Duration::from_secs(20));
    params.init_sql = Some(format!("SET work_mem = '7MB'; SET ROLE {role}"));
    let session = connect(&params).await?;
    let check = |case: String| {
        let session = session.as_ref();
        async move {
            for (sql, want) in [
                ("SHOW work_mem", "7MB"),
                ("SELECT current_user::text", role),
                ("SHOW statement_timeout", "20s"),
            ] {
                assert_eq!(
                    scalar(session, sql).await?,
                    Value::Text(want.into()),
                    "{case}: {sql}"
                );
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        }
    };
    check("before".into()).await?;
    let options = ExecOptions::default();
    let mut dropped = 0;
    for script in ["DISCARD ALL", "SELECT pg_sleep(0.001); DISCARD ALL"] {
        for after in (0..=60).map(Duration::from_millis) {
            let call = session.execute(script, &options);
            if tokio::time::timeout(after, call).await.is_ok() {
                check(format!("`{script}` finished")).await?;
                continue;
            }
            dropped += 1;
            check(format!("`{script}` given up after {after:?}")).await?;
        }
    }
    assert!(dropped > 20, "only {dropped} calls were given up");
    Ok(())
}

/// Takes one use from `left`; false when none are left.
fn take_one(left: &std::sync::atomic::AtomicUsize) -> bool {
    use std::sync::atomic::Ordering;
    let mut n = left.load(Ordering::SeqCst);
    loop {
        let Some(after) = n.checked_sub(1) else {
            return false;
        };
        match left.compare_exchange_weak(n, after, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => n = now,
        }
    }
}
