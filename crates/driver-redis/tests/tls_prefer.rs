//! TLS mode `prefer` decides between TLS and plain TCP without waiting for a
//! TLS handshake that a plain server never answers, and `require` never
//! falls back to plain TCP.
//!
//! Three groups of tests:
//!
//! - Offline tests against small fake servers (no Docker).
//! - `#[ignore]` tests against the plain dev servers: the oldest Redis, the
//!   newest Redis and the newest Valkey, with the endpoint overrides
//!   `VARVE_REDIS_62` (`127.0.0.1:63062`), `VARVE_REDIS_810`
//!   (`127.0.0.1:63081`) and `VARVE_VALKEY_9` (`127.0.0.1:63190`), password
//!   `VARVE_REDIS_PASSWORD` (`varve_dev`).
//! - One `#[ignore]` test that generates a CA and a server certificate with
//!   rcgen and starts TLS-only Redis 6.2 and 8.10 and Valkey 9 from
//!   `tests/tls-servers/compose.yml` under its own compose project
//!   (`VARVE_REDIS_TLS_PROJECT`, default `varve-redis-tls-<process id>`;
//!   Docker picks the host ports),
//!   and tears it down again. It needs Docker.
//!
//! Run all with `cargo test -p varve-driver-redis --test tls_prefer -- --include-ignored`.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use varve_core::Engine;
use varve_driver::Value;
use varve_driver::{ConnectParams, Driver, DriverError, ErrorKind, Session, TlsMode};
use varve_driver_redis::{
    CommandRisk, DigestMissing, FullDigest, NameKind, OPEN_DIGEST_ELEMENTS, PubSubItem,
    RedisDriver, RedisSession, ValueData, ValuePage,
};
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn Error>>;

/// Longest a `prefer` connect to a plain server may take. Before the plain
/// probe, `prefer` waited half the connect timeout (5 s by default) for a
/// TLS handshake the plain server never answered.
const PLAIN_PREFER_BOUND: Duration = Duration::from_millis(1000);

/// Longest a TLS connect to a local server may take.
const TLS_BOUND: Duration = Duration::from_millis(2000);

/// Longest any connect in these tests may take before it counts as a hang.
const HANG: Duration = Duration::from_secs(15);

async fn connect(p: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
    RedisDriver::new().connect(p).await
}

fn redis(session: &dyn Session) -> Result<&RedisSession, Box<dyn Error>> {
    session
        .as_any()
        .downcast_ref::<RedisSession>()
        .ok_or_else(|| "not a RedisSession".into())
}

/// Connects with a hang guard and returns the outcome and the time taken.
async fn timed_connect(
    p: &ConnectParams,
) -> Result<(Result<Box<dyn Session>, DriverError>, Duration), Box<dyn Error>> {
    let started = Instant::now();
    let outcome = tokio::time::timeout(HANG, connect(p))
        .await
        .map_err(|_| "the connect hung")?;
    Ok((outcome, started.elapsed()))
}

// Offline tests against fake servers.

/// A fake plain Redis 7.2 that answers every command.
async fn fake_plain_server() -> Result<u16, std::io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = serve(socket).await;
            });
        }
    });
    Ok(port)
}

async fn serve(socket: TcpStream) -> Result<(), std::io::Error> {
    let (read, mut write) = socket.into_split();
    let mut read = BufReader::new(read);
    loop {
        // Like Redis, bytes that do not start a RESP array (a TLS
        // ClientHello) are an inline command that is not complete until a
        // newline arrives: keep the connection open and never answer.
        if read.fill_buf().await?.first().is_some_and(|b| *b != b'*') {
            std::future::pending::<()>().await;
        }
        let Some(words) = read_command(&mut read).await? else {
            return Ok(());
        };
        let line = words.join(" ").to_ascii_uppercase();
        let reply: Vec<u8> = if line.starts_with("CLIENT ID") {
            b":42\r\n".to_vec()
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
        } else if line.starts_with("INFO") {
            let text = "# Server\r\nredis_version:7.2.0\r\n";
            format!("${}\r\n{text}\r\n", text.len()).into_bytes()
        } else if line.starts_with("PING") {
            b"+PONG\r\n".to_vec()
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
    let mut header = Vec::new();
    if read.read_until(b'\n', &mut header).await? == 0 {
        return Ok(None);
    }
    let header = String::from_utf8_lossy(&header);
    let count: usize = header
        .trim()
        .strip_prefix('*')
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| std::io::Error::other("not a command"))?;
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

/// A server that reads what the client sends, writes `answer` (if any) and
/// closes the connection, like a TLS server that gets plain bytes.
async fn closing_server(answer: Option<&'static [u8]>) -> Result<u16, std::io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0; 64];
            let _ = socket.read(&mut buffer).await;
            if let Some(answer) = answer {
                let _ = socket.write_all(answer).await;
            }
            drop(socket);
        }
    });
    Ok(port)
}

fn local(port: u16, mode: TlsMode) -> ConnectParams {
    let mut p = ConnectParams::new(Engine::Redis, "127.0.0.1", port);
    p.tls.mode = mode;
    p
}

#[tokio::test]
async fn prefer_connects_to_a_plain_server_without_a_tls_wait() -> TestResult {
    let port = fake_plain_server().await?;
    // The default connect timeout (10 s), as the app uses it.
    let p = local(port, TlsMode::Prefer);
    let (session, elapsed) = timed_connect(&p).await?;
    let session = session?;
    eprintln!("prefer to the fake plain server: {elapsed:?}");
    assert!(elapsed < PLAIN_PREFER_BOUND, "took {elapsed:?}");
    assert!(!redis(session.as_ref())?.uses_tls());
    session.ping().await?;
    // A reconnect stays on plain TCP without a new probe.
    session
        .execute("QUIT", &varve_driver::ExecOptions::default())
        .await?;
    let started = Instant::now();
    session.ping().await?;
    assert!(started.elapsed() < PLAIN_PREFER_BOUND);
    Ok(())
}

#[tokio::test]
async fn require_never_falls_back_to_a_plain_server() -> TestResult {
    let port = fake_plain_server().await?;
    let mut p = local(port, TlsMode::Require);
    p.connect_timeout = Duration::from_secs(2);
    let (outcome, elapsed) = timed_connect(&p).await?;
    let error = outcome.err().ok_or("require connected to a plain server")?;
    eprintln!(
        "require to the fake plain server: {elapsed:?}: {}",
        error.message
    );
    assert_eq!(error.kind, ErrorKind::Tls, "{error:?}");
    Ok(())
}

#[tokio::test]
async fn prefer_tries_tls_when_the_plain_probe_is_refused() -> TestResult {
    // The server closes on the plain probe, so `prefer` takes it for a TLS
    // server; it closes on the handshake too: a connection problem, and
    // the error says that it answered neither.
    let port = closing_server(None).await?;
    let p = local(port, TlsMode::Prefer);
    let (outcome, elapsed) = timed_connect(&p).await?;
    let error = outcome.err().ok_or("connected to a closing server")?;
    eprintln!("prefer to a closing server: {elapsed:?}: {}", error.message);
    assert_eq!(error.kind, ErrorKind::Connect, "{error:?}");
    assert!(error.message.contains("plain Redis"), "{}", error.message);
    assert!(elapsed < PLAIN_PREFER_BOUND, "took {elapsed:?}");
    Ok(())
}

#[tokio::test]
async fn prefer_tries_tls_when_the_plain_probe_gets_a_tls_alert() -> TestResult {
    // Answers every connection with a TLS alert record (fatal,
    // protocol_version) and closes, like a TLS server that gets plain bytes
    // or a ClientHello it does not accept. `prefer` takes the probe answer
    // for a TLS server, and the failed handshake is a TLS error.
    let port = closing_server(Some(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x46])).await?;
    let p = local(port, TlsMode::Prefer);
    let (outcome, elapsed) = timed_connect(&p).await?;
    let error = outcome.err().ok_or("connected to an alerting server")?;
    eprintln!(
        "prefer to an alerting server: {elapsed:?}: {}",
        error.message
    );
    assert_eq!(error.kind, ErrorKind::Tls, "{error:?}");
    assert!(error.message.contains("plain Redis"), "{}", error.message);
    assert!(elapsed < PLAIN_PREFER_BOUND, "took {elapsed:?}");
    Ok(())
}

#[tokio::test]
async fn prefer_against_a_silent_server_stays_within_the_connect_timeout() -> TestResult {
    // Accepts TCP connections and never reads or writes.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let held = tokio::spawn(async move {
        let mut sockets = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            sockets.push(socket);
        }
    });
    let mut p = local(port, TlsMode::Prefer);
    p.connect_timeout = Duration::from_millis(800);
    let (outcome, elapsed) = timed_connect(&p).await?;
    let error = outcome.err().ok_or("connected to a silent server")?;
    // Whether the TLS attempt or the connect timeout ends it first, the
    // error says that neither attempt got an answer.
    assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
    assert!(
        error
            .message
            .contains("neither the plain Redis PING nor the TLS handshake got an answer"),
        "{}",
        error.message
    );
    assert!(elapsed < p.connect_timeout * 3, "took {elapsed:?}");
    held.abort();
    Ok(())
}

// The plain dev servers.

struct Server {
    label: &'static str,
    host: String,
    port: u16,
}

fn plain_servers() -> Result<Vec<Server>, Box<dyn Error>> {
    [
        ("redis-6.2", "VARVE_REDIS_62", "127.0.0.1:63062"),
        ("redis-8.10", "VARVE_REDIS_810", "127.0.0.1:63081"),
        ("valkey-9", "VARVE_VALKEY_9", "127.0.0.1:63190"),
    ]
    .into_iter()
    .map(|(label, var, default)| {
        let endpoint = std::env::var(var).unwrap_or_else(|_| default.to_string());
        let (host, port) = endpoint
            .rsplit_once(':')
            .ok_or_else(|| format!("{var}: `{endpoint}` must be host:port"))?;
        Ok(Server {
            label,
            host: host.to_string(),
            port: port.parse()?,
        })
    })
    .collect()
}

/// Fails with a clear message when the dev server does not accept TCP
/// connections, rather than with a confusing connect or TLS error.
async fn reachable(server: &Server) -> TestResult {
    match tokio::time::timeout(
        Duration::from_secs(3),
        TcpStream::connect((server.host.as_str(), server.port)),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!(
            "{}: nothing accepts connections on {}:{} ({error}); start the dev Redis servers or \
             point VARVE_REDIS_62, VARVE_REDIS_810 and VARVE_VALKEY_9 at running ones",
            server.label, server.host, server.port
        )
        .into()),
        Err(_) => Err(format!(
            "{}: {}:{} did not accept a TCP connection within 3 s; start the dev Redis servers \
             or point VARVE_REDIS_62, VARVE_REDIS_810 and VARVE_VALKEY_9 at running ones",
            server.label, server.host, server.port
        )
        .into()),
    }
}

fn password() -> Secret {
    Secret::new(std::env::var("VARVE_REDIS_PASSWORD").unwrap_or_else(|_| "varve_dev".into()))
}

fn plain_params(server: &Server, mode: TlsMode) -> ConnectParams {
    let mut p = ConnectParams::new(Engine::Redis, server.host.clone(), server.port);
    p.password = Some(password());
    p.tls.mode = mode;
    p.application_name = "Varve Test".into();
    p
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn prefer_connects_to_plain_servers_without_a_tls_wait() -> TestResult {
    for server in plain_servers()? {
        let label = server.label;
        // The default connect timeout (10 s), as the app uses it.
        let p = plain_params(&server, TlsMode::Prefer);
        reachable(&server).await?;
        let (session, elapsed) = timed_connect(&p).await?;
        let session = session.map_err(|e| format!("{label}: {e:?}"))?;
        eprintln!("{label}: prefer connect took {elapsed:?}");
        assert!(elapsed < PLAIN_PREFER_BOUND, "{label}: took {elapsed:?}");
        assert!(!redis(session.as_ref())?.uses_tls(), "{label}");
        session.ping().await?;
        session.close().await?;

        // A wrong password is still an Auth error after the probe.
        let mut wrong = plain_params(&server, TlsMode::Prefer);
        wrong.password = Some(Secret::new("definitely-wrong".into()));
        let error = connect(&wrong).await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Auth), "{label}");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn require_fails_against_plain_servers() -> TestResult {
    for server in plain_servers()? {
        let label = server.label;
        reachable(&server).await?;
        for mode in [TlsMode::Require, TlsMode::VerifyFull] {
            let mut p = plain_params(&server, mode);
            p.connect_timeout = Duration::from_secs(4);
            let (outcome, elapsed) = timed_connect(&p).await?;
            let error = outcome
                .err()
                .ok_or_else(|| format!("{label}: {mode:?} connected to a plain server"))?;
            eprintln!(
                "{label}: {mode:?} failed after {elapsed:?}: {}",
                error.message
            );
            assert_eq!(error.kind, ErrorKind::Tls, "{label}: {mode:?}: {error:?}");
        }
    }
    Ok(())
}

// TLS-only servers from tests/tls-servers/compose.yml.

/// The only name in the server certificate.
const SERVER_NAME: &str = "db.varve.invalid";

/// Certificates written to a fresh temporary directory.
struct Certificates {
    dir: PathBuf,
}

impl Certificates {
    fn ca(&self) -> String {
        self.dir.join("ca.pem").display().to_string()
    }
}

impl Drop for Certificates {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write(dir: &Path, name: &str, contents: &str) -> TestResult {
    let path = dir.join(name);
    std::fs::write(&path, contents)?;
    // The server in the container runs as another user; these are
    // throwaway test keys.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

/// A CA and a server certificate for [`SERVER_NAME`] signed by it.
fn generate() -> Result<Certificates, Box<dyn Error>> {
    // Canonical, so Docker sees the real path (`/private/var/...` on macOS,
    // where `/var` inside the Docker VM is the VM's own directory).
    let dir = std::env::temp_dir()
        .canonicalize()?
        .join(format!("varve-redis-tls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    }
    let certificates = Certificates { dir };

    let ca_key = KeyPair::generate()?;
    let mut ca = CertificateParams::new(Vec::<String>::new())?;
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "Varve Redis Test CA");
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca.self_signed(&ca_key)?;
    let issuer = Issuer::new(ca, ca_key);

    let server_key = KeyPair::generate()?;
    let mut server = CertificateParams::new(vec![SERVER_NAME.to_owned()])?;
    server
        .distinguished_name
        .push(DnType::CommonName, SERVER_NAME);
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server.signed_by(&server_key, &issuer)?;

    let dir = &certificates.dir;
    write(dir, "ca.pem", &ca_cert.pem())?;
    write(dir, "server-cert.pem", &server_cert.pem())?;
    write(dir, "server-key.pem", &server_key.serialize_pem())?;
    Ok(certificates)
}

/// The TLS servers; stopped and removed when dropped.
struct TlsServers {
    project: String,
    compose: PathBuf,
    dir: PathBuf,
}

impl TlsServers {
    fn compose(&self, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let output = Command::new("docker")
            .arg("compose")
            .args(["-p", &self.project, "-f"])
            .arg(&self.compose)
            .args(args)
            .env("VARVE_REDIS_TLS_DIR", &self.dir)
            .output()
            .map_err(|error| {
                format!("cannot run docker compose (this test needs Docker): {error}")
            })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "docker compose {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )
            .into())
        }
    }

    fn start(certificates: &Certificates) -> Result<Self, Box<dyn Error>> {
        let compose = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tls-servers/compose.yml");
        if !compose.is_file() {
            return Err(format!("{} is missing", compose.display()).into());
        }
        let servers = Self {
            project: std::env::var("VARVE_REDIS_TLS_PROJECT")
                .unwrap_or_else(|_| format!("varve-redis-tls-{}", std::process::id())),
            compose,
            dir: certificates.dir.clone(),
        };
        // Leftovers of an aborted run would keep old certificates.
        servers.compose(&["down", "-v"])?;
        if let Err(error) = servers.compose(&["up", "-d", "--wait"]) {
            let logs = servers
                .compose(&["logs", "--tail", "40"])
                .unwrap_or_else(|logs_error| logs_error.to_string());
            return Err(format!(
                "could not start the TLS Redis fixture (compose project {}, needs a running \
                 Docker): {error}\n{logs}",
                servers.project
            )
            .into());
        }
        Ok(servers)
    }
}

impl TlsServers {
    /// The host port Docker picked for `service`.
    fn port(&self, service: &str) -> Result<u16, Box<dyn Error>> {
        let output = self.compose(&["port", service, "6379"])?;
        let port = output
            .trim()
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse().ok())
            .ok_or_else(|| format!("no host port for {service}: {output:?}"))?;
        Ok(port)
    }
}

impl Drop for TlsServers {
    fn drop(&mut self) {
        if let Err(error) = self.compose(&["down", "-v"]) {
            eprintln!("{error}");
        }
    }
}

/// The side connections of a `prefer` session that settled on TLS use TLS
/// too (the servers listen on TLS only, so a plain side connection fails):
/// a command lookup while `MULTI` is open, a Pub/Sub connection, the cancel
/// connection that sends `CLIENT KILL`, the key browser's measuring
/// connection and the full digest read.
async fn side_connections_use_tls(label: &str, session: &dyn Session) -> TestResult {
    let rs = redis(session)?;
    let options = varve_driver::ExecOptions::default();

    // Lookups of commands not seen before run on a side connection while
    // a MULTI block is open.
    session.execute("MULTI", &options).await?;
    let risk = tokio::time::timeout(HANG, rs.command_risk("FLUSHALL")).await??;
    assert_eq!(risk, CommandRisk::Destructive, "{label}");
    session.execute("DISCARD", &options).await?;

    // Pub/Sub on its own connection.
    let opener = rs.pubsub_opener().await;
    let (mut reader, mut writer) = tokio::time::timeout(HANG, opener.open()).await??;
    writer
        .subscribe(NameKind::Channel, &[b"tls-prefer".to_vec()])
        .await?;
    match tokio::time::timeout(HANG, reader.next(1024)).await?? {
        PubSubItem::Subscribed { .. } => {}
        other => return Err(format!("{label}: expected a confirmation, got {other:?}").into()),
    }
    assert_eq!(rs.publish(b"tls-prefer", b"over tls").await?, 1, "{label}");
    match tokio::time::timeout(HANG, reader.next(1024)).await?? {
        PubSubItem::Message { payload, .. } => assert_eq!(payload, b"over tls", "{label}"),
        other => return Err(format!("{label}: expected the message, got {other:?}").into()),
    }
    drop((reader, writer));

    // Cancel: `CLIENT KILL` goes over a new connection.
    let cancel = session.cancel_handle();
    let running = session.execute("BLPOP tls-prefer:never 20", &options);
    let cancelling = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await
    };
    let (outcome, cancelled) =
        tokio::time::timeout(HANG, async { tokio::join!(running, cancelling) }).await?;
    cancelled?;
    let error = outcome
        .err()
        .ok_or_else(|| format!("{label}: BLPOP was not cancelled"))?;
    assert_eq!(error.kind, ErrorKind::Cancelled, "{label}: {error:?}");
    // The server-side kill worked (it reports when it could not ask).
    assert!(
        !error.message.contains("could not be asked"),
        "{label}: {}",
        error.message
    );
    session.ping().await?;

    // The key browser measures entries too large to read on a side
    // connection (measure.rs).
    let rep = format!("string.rep('x', {HUGE_ENTRY})");
    session
        .execute(
            &format!(
                "DEL tls:measure\nRPUSH tls:measure a\nEVAL \"redis.call('RPUSH', KEYS[1], {rep})\" 1 tls:measure"
            ),
            &options,
        )
        .await?;
    let before = rs.measuring_connections();
    let page = tokio::time::timeout(HANG, rs.read_value(b"tls:measure", &ValuePage::first(10)))
        .await??
        .ok_or_else(|| format!("{label}: no list"))?;
    let ValueData::List { items } = &page.data else {
        return Err(format!("{label}: not a list").into());
    };
    assert_eq!(items.len(), 2, "{label}");
    assert_eq!(
        items.get(1).map(|i| &i.value),
        Some(&Value::Other(format!(
            "(too large to show: {HUGE_ENTRY} bytes)"
        ))),
        "{label}"
    );
    assert_eq!(rs.measuring_connections() - before, 1, "{label}");

    // The full digest of a key too large to digest when opened is read on
    // a side connection too (digest.rs).
    let many = OPEN_DIGEST_ELEMENTS + 1;
    session
        .execute(
            &format!(
                "DEL tls:digest\nEVAL \"for i = 1, {many} do redis.call('RPUSH', KEYS[1], i) end\" 1 tls:digest"
            ),
            &options,
        )
        .await?;
    let version = rs
        .key_version(b"tls:digest")
        .await?
        .ok_or_else(|| format!("{label}: no key"))?;
    assert_eq!(
        version.digest_missing,
        Some(DigestMissing::Deferred),
        "{label}"
    );
    let reader = rs.version_reader().await;
    let full = tokio::time::timeout(
        HANG,
        reader.read_full(b"tls:digest", &version, reader.full_read_limit()),
    )
    .await??;
    assert!(matches!(full, FullDigest::Done(_)), "{label}: {full:?}");
    session
        .execute("DEL tls:measure tls:digest", &options)
        .await?;
    Ok(())
}

/// An entry larger than the most bytes one reply may take, so the key
/// browser measures it instead of reading it.
const HUGE_ENTRY: u64 = varve_driver_redis::MAX_REPLY_BYTES + 1024 * 1024;

#[tokio::test]
#[ignore = "needs Docker for tests/tls-servers/compose.yml"]
async fn prefer_uses_tls_on_tls_servers() -> TestResult {
    let certificates = generate()?;
    let servers = TlsServers::start(&certificates)?;
    for label in ["redis-6.2", "redis-8.10", "valkey-9"] {
        let port = servers.port(label)?;
        let mut p = ConnectParams::new(Engine::Redis, "127.0.0.1", port);
        p.password = Some(password());
        p.application_name = "Varve Test".into();

        // `prefer` (the default connect timeout) settles on TLS, quickly.
        p.tls.mode = TlsMode::Prefer;
        let (session, elapsed) = timed_connect(&p).await?;
        let session = session.map_err(|e| format!("{label}: prefer: {e:?}"))?;
        eprintln!("{label}: prefer to the TLS server took {elapsed:?}");
        assert!(elapsed < TLS_BOUND, "{label}: took {elapsed:?}");
        assert!(redis(session.as_ref())?.uses_tls(), "{label}");
        session.ping().await?;
        // A reconnect stays on TLS.
        session
            .execute("QUIT", &varve_driver::ExecOptions::default())
            .await?;
        session.ping().await?;
        assert!(redis(session.as_ref())?.uses_tls(), "{label}");
        side_connections_use_tls(label, session.as_ref()).await?;
        session.close().await?;

        // `require` and `verify-full` (with the CA and the certificate's
        // name) connect over TLS.
        p.tls.mode = TlsMode::Require;
        let session = connect(&p)
            .await
            .map_err(|e| format!("{label}: require: {e:?}"))?;
        assert!(redis(session.as_ref())?.uses_tls(), "{label}");
        session.close().await?;
        let mut verify = p.clone();
        verify.tls.mode = TlsMode::VerifyFull;
        verify.tls.ca_path = Some(certificates.ca());
        verify.tls.server_name = Some(SERVER_NAME.into());
        let session = connect(&verify)
            .await
            .map_err(|e| format!("{label}: verify-full: {e:?}"))?;
        assert!(redis(session.as_ref())?.uses_tls(), "{label}");
        session.close().await?;

        // `verify-full` against the IP address fails the name check.
        let mut wrong_name = verify.clone();
        wrong_name.tls.server_name = None;
        let error = connect(&wrong_name).await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Tls), "{label}");

        // Plain TCP to a TLS server fails rather than hanging.
        p.tls.mode = TlsMode::Disable;
        p.connect_timeout = Duration::from_secs(4);
        let (outcome, elapsed) = timed_connect(&p).await?;
        assert!(
            outcome.is_err(),
            "{label}: plain TCP connected to a TLS port"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "{label}: took {elapsed:?}"
        );
    }
    Ok(())
}
