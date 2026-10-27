//! The connect timeout covers the whole connect (TCP, TLS, AUTH,
//! `CLIENT ID`, the init commands and `INFO`) and transparent reconnects.
//! These tests use small fake servers that accept the connection and then
//! stall, so they need no dev database.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, DriverError, ErrorKind, Session, TlsMode};
use varve_driver_redis::RedisDriver;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// Longest a connect may take in these tests before it counts as a hang.
const HANG: Duration = Duration::from_secs(10);

/// When the fake server stops answering.
#[derive(Clone, Copy)]
struct Stall {
    /// Upper-cased command line prefix that is never answered.
    command: &'static str,
    /// 0-based index of the first connection that stalls.
    from_connection: usize,
}

/// A fake server that answers like Redis 7.2 until it sees the stall
/// command, and then never answers again on that connection.
async fn fake_server(stall: Stall) -> Result<u16, std::io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let connections = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let index = connections.fetch_add(1, Ordering::SeqCst);
            let stalls = index >= stall.from_connection;
            tokio::spawn(async move {
                let _ = serve(socket, stalls.then_some(stall.command)).await;
            });
        }
    });
    Ok(port)
}

async fn serve(socket: TcpStream, stall_on: Option<&str>) -> Result<(), std::io::Error> {
    let (read, mut write) = socket.into_split();
    let mut read = BufReader::new(read);
    loop {
        let Some(words) = read_command(&mut read).await? else {
            return Ok(());
        };
        let line = words.join(" ").to_ascii_uppercase();
        if stall_on.is_some_and(|prefix| line.starts_with(prefix)) {
            // Keep the connection open and never answer.
            std::future::pending::<()>().await;
        }
        let reply: Vec<u8> = if line.starts_with("CLIENT ID") {
            b":42\r\n".to_vec()
        } else if line.starts_with("INFO") {
            let text = "# Server\r\nredis_version:7.2.0\r\n";
            format!("${}\r\n{text}\r\n", text.len()).into_bytes()
        } else if line.starts_with("COMMAND INFO") {
            // A minimal entry with the command's own name (no flags).
            let name = words
                .get(2)
                .map(|w| w.to_ascii_lowercase())
                .unwrap_or_default();
            format!(
                "*1\r\n*7\r\n${}\r\n{name}\r\n:-1\r\n*0\r\n:0\r\n:0\r\n:0\r\n*0\r\n",
                name.len()
            )
            .into_bytes()
        } else if line.starts_with("PING") {
            b"+PONG\r\n".to_vec()
        } else if line.starts_with("GET") {
            b"$-1\r\n".to_vec()
        } else if line.starts_with("QUIT") {
            write.write_all(b"+OK\r\n").await?;
            return Ok(());
        } else {
            b"+OK\r\n".to_vec()
        };
        write.write_all(&reply).await?;
    }
}

/// Reads one RESP array command; `None` at the end of the stream.
async fn read_command<R: AsyncBufReadExt + AsyncReadExt + Unpin>(
    read: &mut R,
) -> Result<Option<Vec<String>>, std::io::Error> {
    let mut header = String::new();
    if read.read_line(&mut header).await? == 0 {
        return Ok(None);
    }
    let count: usize = header
        .trim()
        .strip_prefix('*')
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("not a command: {header:?}")))?;
    let mut words = Vec::with_capacity(count);
    for _ in 0..count {
        let mut length = String::new();
        read.read_line(&mut length).await?;
        let length: usize = length
            .trim()
            .strip_prefix('$')
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| std::io::Error::other(format!("not a bulk string: {length:?}")))?;
        let mut data = vec![0; length + 2];
        read.read_exact(&mut data).await?;
        data.truncate(length);
        words.push(String::from_utf8_lossy(&data).into_owned());
    }
    Ok(Some(words))
}

fn params(port: u16) -> ConnectParams {
    let mut p = ConnectParams::new(Engine::Redis, "127.0.0.1", port);
    p.tls.mode = TlsMode::Disable;
    p.connect_timeout = CONNECT_TIMEOUT;
    p
}

async fn connect(p: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
    RedisDriver::new().connect(p).await
}

/// Connects and expects a Timeout error well before [`HANG`] whose message
/// contains `says`.
async fn expect_connect_timeout(p: &ConnectParams, says: &str) -> TestResult {
    let started = Instant::now();
    let outcome = tokio::time::timeout(HANG, connect(p))
        .await
        .map_err(|_| "the connect hung past the connect timeout")?;
    let error = outcome
        .err()
        .ok_or("the connect succeeded against a stalled server")?;
    assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
    assert!(error.message.contains(says), "{}", error.message);
    assert!(
        started.elapsed() < CONNECT_TIMEOUT * 4,
        "took {:?}",
        started.elapsed()
    );
    Ok(())
}

#[tokio::test]
async fn a_silent_listener_times_out() -> TestResult {
    // Accepts TCP connections and never reads or writes.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let held = tokio::spawn(async move {
        let mut sockets = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            sockets.push(socket);
        }
    });
    // Nothing was ever read: the server did not answer.
    expect_connect_timeout(
        &params(port),
        &format!("127.0.0.1:{port} did not answer within 0.5 s"),
    )
    .await?;
    held.abort();
    Ok(())
}

#[tokio::test]
async fn a_server_that_stalls_on_client_id_times_out() -> TestResult {
    let port = fake_server(Stall {
        command: "CLIENT ID",
        from_connection: 0,
    })
    .await?;
    // The server answered AUTH or HELLO and then stalled: a setup stall,
    // not a server that did not answer.
    expect_connect_timeout(
        &params(port),
        "timed out after 0.5 s during the session setup",
    )
    .await
}

#[tokio::test]
async fn a_server_that_stalls_on_info_times_out() -> TestResult {
    let port = fake_server(Stall {
        command: "INFO",
        from_connection: 0,
    })
    .await?;
    // The server answered AUTH or HELLO and then stalled: a setup stall,
    // not a server that did not answer.
    expect_connect_timeout(
        &params(port),
        "timed out after 0.5 s during the session setup",
    )
    .await
}

#[tokio::test]
async fn a_stalled_init_command_times_out() -> TestResult {
    let port = fake_server(Stall {
        command: "CLIENT NO-TOUCH",
        from_connection: 0,
    })
    .await?;
    let mut p = params(port);
    p.init_sql = Some("SELECT 0\nCLIENT NO-TOUCH ON".into());
    expect_connect_timeout(&p, "timed out after 0.5 s during the session setup").await
}

#[tokio::test]
async fn the_fake_server_answers_a_normal_connect() -> TestResult {
    // Guards the tests above: without a stall the fake server connects.
    let port = fake_server(Stall {
        command: "NEVER-SENT",
        from_connection: 0,
    })
    .await?;
    let mut p = params(port);
    p.init_sql = Some("SELECT 0\nCLIENT NO-TOUCH ON".into());
    let session = tokio::time::timeout(HANG, connect(&p)).await??;
    assert_eq!(session.server_info().version, "7.2.0");
    session.ping().await?;
    Ok(())
}

#[tokio::test]
async fn a_stalled_reconnect_times_out() -> TestResult {
    // The first connection works; every later one stalls on CLIENT ID.
    let port = fake_server(Stall {
        command: "CLIENT ID",
        from_connection: 1,
    })
    .await?;
    let session = tokio::time::timeout(HANG, connect(&params(port))).await??;
    // QUIT closes the connection; the next call reconnects.
    session
        .execute("QUIT", &varve_driver::ExecOptions::default())
        .await?;
    let started = Instant::now();
    let error = tokio::time::timeout(HANG, session.ping())
        .await
        .map_err(|_| "the reconnect hung past the connect timeout")?
        .err()
        .ok_or("the reconnect succeeded against a stalled server")?;
    assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
    assert!(started.elapsed() < CONNECT_TIMEOUT * 4);
    Ok(())
}

#[tokio::test]
async fn a_cancel_stops_a_stalled_reconnect() -> TestResult {
    // The first connection works; every later one stalls on CLIENT ID, and
    // the connect timeout is long, so only the cancel can end the wait.
    let port = fake_server(Stall {
        command: "CLIENT ID",
        from_connection: 1,
    })
    .await?;
    let mut p = params(port);
    p.connect_timeout = Duration::from_secs(8);
    let session: Arc<dyn Session> = tokio::time::timeout(HANG, connect(&p)).await??.into();
    session
        .execute("QUIT", &varve_driver::ExecOptions::default())
        .await?;
    let running = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.ping().await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    session.cancel_handle().cancel().await?;
    let error = tokio::time::timeout(HANG, running)
        .await
        .map_err(|_| "the cancelled reconnect hung")??
        .err()
        .ok_or("the reconnect succeeded against a stalled server")?;
    assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
    Ok(())
}
