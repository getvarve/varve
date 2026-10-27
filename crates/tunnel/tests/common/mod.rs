//! Shared helpers for the integration tests against the OpenSSH container
//! from `tests/docker-compose.yml` (compose project `varve-m1-ssh`).
//!
//! Endpoints come from environment variables so CI can point them elsewhere:
//! `VARVE_TUNNEL_SSH_HOST` (127.0.0.1), `VARVE_TUNNEL_SSH_PORT` (22022),
//! `VARVE_TUNNEL_SSH_USER` (varve), `VARVE_TUNNEL_SSH_PASSWORD` (varve_dev),
//! `VARVE_TUNNEL_TARGET_HOST` (redis) and `VARVE_TUNNEL_TARGET_PORT` (6379).
//! The tests that pause and stop the SSH container use
//! `VARVE_TUNNEL_COMPOSE_PROJECT` (varve-m1-ssh) and
//! `VARVE_TUNNEL_COMPOSE_FILE` (this directory's `docker-compose.yml`).

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKeyOrCertificate};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use varve_secrets::Secret;
use varve_tunnel::{HostKeyPolicy, KnownHosts, SshAuth, TunnelParams};

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

pub fn ssh_host() -> String {
    env_or("VARVE_TUNNEL_SSH_HOST", "127.0.0.1")
}

pub fn ssh_port() -> u16 {
    env_or("VARVE_TUNNEL_SSH_PORT", "22022")
        .parse()
        .unwrap_or(22022)
}

pub fn ssh_user() -> String {
    env_or("VARVE_TUNNEL_SSH_USER", "varve")
}

pub fn ssh_password() -> Secret {
    Secret::new(env_or("VARVE_TUNNEL_SSH_PASSWORD", "varve_dev"))
}

pub fn target_host() -> String {
    env_or("VARVE_TUNNEL_TARGET_HOST", "redis")
}

pub fn target_port() -> u16 {
    env_or("VARVE_TUNNEL_TARGET_PORT", "6379")
        .parse()
        .unwrap_or(6379)
}

pub fn compose_file() -> PathBuf {
    match std::env::var_os("VARVE_TUNNEL_COMPOSE_FILE").filter(|f| !f.is_empty()) {
        Some(file) => PathBuf::from(file),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("docker-compose.yml"),
    }
}

pub fn compose_project() -> String {
    env_or("VARVE_TUNNEL_COMPOSE_PROJECT", "varve-m1-ssh")
}

/// Parameters for the test server with a fresh known hosts file.
pub fn params(dir: &Path, auth: SshAuth, policy: HostKeyPolicy) -> TunnelParams {
    let mut p = TunnelParams::new(
        ssh_host(),
        ssh_user(),
        auth,
        target_host(),
        target_port(),
        KnownHosts::new(dir.join("known_hosts"), policy),
    );
    p.ssh_port = ssh_port();
    p.connect_timeout = Duration::from_secs(10);
    p
}

pub fn password_params(dir: &Path) -> TunnelParams {
    params(
        dir,
        SshAuth::Password(ssh_password()),
        HostKeyPolicy::AcceptNew,
    )
}

/// Generates an Ed25519 key in OpenSSH format, optionally encrypted.
pub fn write_ed25519_key(
    dir: &Path,
    name: &str,
    passphrase: Option<&str>,
) -> TestResult<(PathBuf, String)> {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let public = key.public_key().to_openssh()?;
    let stored = match passphrase {
        Some(p) => key.encrypt(&mut rand::rng(), p)?,
        None => key,
    };
    let path = dir.join(name);
    std::fs::write(&path, stored.to_openssh(LineEnding::LF)?.as_bytes())?;
    Ok((path, public))
}

/// Generates a key with `ssh-keygen` (for RSA and PEM formats).
pub fn ssh_keygen(dir: &Path, name: &str, args: &[&str]) -> TestResult<(PathBuf, String)> {
    let path = dir.join(name);
    let out = std::process::Command::new("ssh-keygen")
        .args(args)
        .args(["-q", "-C", "varve-test", "-f"])
        .arg(&path)
        .output()
        .map_err(|e| format!("ssh-keygen is needed for this test: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    let public = std::fs::read_to_string(path.with_extension("pub"))?;
    Ok((path, public.trim().to_owned()))
}

struct AcceptAny;

impl russh::client::Handler for AcceptAny {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Runs a shell command on the test server as the test user (password
/// login, host key not checked: test setup only).
pub async fn remote_exec(command: &str) -> TestResult<u32> {
    let config = Arc::new(russh::client::Config::default());
    let mut handle = russh::client::connect(config, (ssh_host(), ssh_port()), AcceptAny).await?;
    let auth = handle
        .authenticate_password(ssh_user(), ssh_password().expose())
        .await?;
    if !auth.success() {
        return Err("setup login with the test password failed".into());
    }
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command).await?;
    let mut status = None;
    while let Some(msg) = channel.wait().await {
        if let ChannelMsg::ExitStatus { exit_status } = msg {
            status = Some(exit_status);
        }
    }
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    status.ok_or_else(|| "no exit status".into())
}

/// Adds a public key to the test user's `authorized_keys`.
pub async fn authorize(public_key: &str) -> TestResult {
    let key = public_key.trim();
    if key.contains('\'') {
        return Err("unexpected quote in public key".into());
    }
    let cmd = format!(
        "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && \
         chmod 600 ~/.ssh/authorized_keys && \
         (grep -qxF '{key}' ~/.ssh/authorized_keys || echo '{key}' >> ~/.ssh/authorized_keys)"
    );
    match remote_exec(&cmd).await? {
        0 => Ok(()),
        code => Err(format!("installing the key failed with exit code {code}").into()),
    }
}

/// Sends `PING` to Redis through `addr`.
pub async fn redis_ping(addr: SocketAddr) -> TestResult {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(b"PING\r\n").await?;
    let mut buf = [0u8; 7];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut buf)).await??;
    if &buf != b"+PONG\r\n" {
        return Err(format!("unexpected reply {:?}", String::from_utf8_lossy(&buf)).into());
    }
    Ok(())
}

/// Sends `PING` on an already connected stream.
pub async fn redis_ping_on(stream: &mut TcpStream) -> TestResult {
    stream.write_all(b"PING\r\n").await?;
    let mut buf = [0u8; 7];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut buf)).await??;
    if &buf != b"+PONG\r\n" {
        return Err(format!("unexpected reply {:?}", String::from_utf8_lossy(&buf)).into());
    }
    Ok(())
}

/// Sends `ECHO <payload>` to Redis through `addr` and checks the reply.
pub async fn redis_echo(addr: SocketAddr, payload: &[u8]) -> TestResult {
    let mut stream = TcpStream::connect(addr).await?;
    let mut request = format!("*2\r\n$4\r\nECHO\r\n${}\r\n", payload.len()).into_bytes();
    request.extend_from_slice(payload);
    request.extend_from_slice(b"\r\n");
    let expected_head = format!("${}\r\n", payload.len()).into_bytes();
    let (mut reader, mut writer) = stream.split();
    let write = async {
        writer.write_all(&request).await?;
        Ok::<_, std::io::Error>(())
    };
    let read = async {
        let mut reply = vec![0u8; expected_head.len() + payload.len() + 2];
        reader.read_exact(&mut reply).await?;
        Ok::<_, std::io::Error>(reply)
    };
    let (w, r) =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(write, read) }).await?;
    w?;
    let reply = r?;
    if reply.get(..expected_head.len()) != Some(expected_head.as_slice())
        || reply.get(expected_head.len()..expected_head.len() + payload.len()) != Some(payload)
    {
        return Err("ECHO reply does not match".into());
    }
    Ok(())
}

/// Deterministic test payload.
pub fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Waits until the SSH server accepts password logins again.
pub async fn wait_for_ssh(limit: Duration) -> TestResult {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        match remote_exec("true").await {
            Ok(0) => return Ok(()),
            result if tokio::time::Instant::now() >= deadline => {
                return Err(format!("SSH server not ready: {result:?}").into());
            }
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Runs `docker compose` for the test project.
pub fn compose(args: &[&str]) -> TestResult {
    let out = std::process::Command::new("docker")
        .args(["compose", "-p"])
        .arg(compose_project())
        .arg("-f")
        .arg(compose_file())
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "docker compose {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

/// Checks that `addr` does not forward: the connection is refused, or it is
/// accepted and then reset or closed without an answer to `PING`.
pub async fn assert_not_forwarding(addr: SocketAddr) -> TestResult {
    let mut stream = match TcpStream::connect(addr).await {
        Ok(stream) => stream,
        Err(_) => return Ok(()),
    };
    // Writing may already fail when the reset arrived first.
    let _ = stream.write_all(b"PING\r\n").await;
    let mut buf = [0u8; 7];
    match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await? {
        Ok(0) | Err(_) => Ok(()),
        Ok(n) => Err(format!(
            "the port forwarded while it should not: {:?}",
            String::from_utf8_lossy(buf.get(..n).unwrap_or_default())
        )
        .into()),
    }
}

/// Checks that `addr` is still bound (a connect succeeds, so no other
/// process can have taken the port) but does not forward: the connection
/// is reset or closed without an answer.
pub async fn assert_bound_but_refusing(addr: SocketAddr) -> TestResult {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("the port {addr} was released: {e}"))?;
    let _ = stream.write_all(b"PING\r\n").await;
    let mut buf = [0u8; 7];
    match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await? {
        Ok(0) | Err(_) => Ok(()),
        Ok(n) => Err(format!(
            "the port forwarded while it should not: {:?}",
            String::from_utf8_lossy(buf.get(..n).unwrap_or_default())
        )
        .into()),
    }
}

/// Waits until nothing listens on `addr` any more (the tunnel was dropped).
pub async fn assert_released(addr: SocketAddr) -> TestResult {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(addr).await.is_err() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("the port {addr} is still bound").into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
