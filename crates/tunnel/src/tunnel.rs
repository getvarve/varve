//! One SSH tunnel: a session plus a local listener that forwards every
//! accepted connection over a `direct-tcpip` channel.

use std::borrow::Cow;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::client::{self, DisconnectReason, Handle};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelOpenFailure, Disconnect, MethodKind, Preferred};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use varve_secrets::Secret;

use crate::error::{AuthMethod, TimeoutStage, TunnelError};
use crate::keys::load_private_key;
use crate::known_hosts::{KnownHosts, prefer_known};

/// How often the accept loop looks for a session that ended without the
/// library reporting it (for example when closing the socket fails).
const SESSION_POLL: Duration = Duration::from_secs(1);

/// First pause after a failed `accept` (usually: out of file descriptors).
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(50);

/// Longest pause between `accept` attempts while they keep failing.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(2);

/// At most one warning per this interval while `accept` keeps failing.
const ACCEPT_WARN_EVERY: Duration = Duration::from_secs(30);

/// Unanswered keepalives after which the session counts as dropped.
const KEEPALIVE_MAX: usize = 3;

/// How to authenticate to the SSH server (design doc 3.1). Secrets come
/// from the keychain (`ssh_password`, `ssh_key_passphrase`).
#[derive(Debug, Clone)]
pub enum SshAuth {
    /// Password authentication.
    Password(Secret),
    /// A private key file (OpenSSH or PEM; Ed25519, ECDSA or RSA).
    KeyFile {
        /// Path of the private key.
        path: PathBuf,
        /// Passphrase, when the key is encrypted.
        passphrase: Option<Secret>,
    },
    /// The running SSH agent: `SSH_AUTH_SOCK` on macOS and Linux, the
    /// OpenSSH agent named pipe on Windows.
    Agent,
}

impl SshAuth {
    /// The method for error reporting.
    #[must_use]
    pub fn method(&self) -> AuthMethod {
        match self {
            SshAuth::Password(_) => AuthMethod::Password,
            SshAuth::KeyFile { .. } => AuthMethod::KeyFile,
            SshAuth::Agent => AuthMethod::Agent,
        }
    }

    /// Whether both authenticate the same way: the same method and, for key
    /// files, the same path. Secrets are not compared.
    fn same_identity(&self, other: &SshAuth) -> bool {
        match (self, other) {
            (SshAuth::Password(_), SshAuth::Password(_)) | (SshAuth::Agent, SshAuth::Agent) => true,
            (SshAuth::KeyFile { path: a, .. }, SshAuth::KeyFile { path: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// Everything needed to open a tunnel.
#[derive(Debug, Clone)]
pub struct TunnelParams {
    /// SSH server host name or IP address.
    pub ssh_host: String,
    /// SSH server port.
    pub ssh_port: u16,
    /// SSH user name.
    pub username: String,
    /// How to authenticate.
    pub auth: SshAuth,
    /// Host to forward to, as seen from the SSH server.
    pub target_host: String,
    /// Port to forward to.
    pub target_port: u16,
    /// Host key checking.
    pub known_hosts: KnownHosts,
    /// Interval of SSH keepalives. After three unanswered keepalives the
    /// session counts as dropped. `Duration::ZERO` disables keepalives.
    pub keepalive: Duration,
    /// Limit for each of: TCP connect plus key exchange, authentication,
    /// opening a forwarding channel, and a health check.
    pub connect_timeout: Duration,
}

impl TunnelParams {
    /// Default keepalive interval.
    pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(15);
    /// Default connect timeout.
    pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

    /// Parameters with SSH port 22 and the default keepalive and timeout.
    #[must_use]
    pub fn new(
        ssh_host: impl Into<String>,
        username: impl Into<String>,
        auth: SshAuth,
        target_host: impl Into<String>,
        target_port: u16,
        known_hosts: KnownHosts,
    ) -> Self {
        Self {
            ssh_host: ssh_host.into(),
            ssh_port: 22,
            username: username.into(),
            auth,
            target_host: target_host.into(),
            target_port,
            known_hosts,
            keepalive: Self::DEFAULT_KEEPALIVE,
            connect_timeout: Self::DEFAULT_CONNECT_TIMEOUT,
        }
    }

    /// Whether an open tunnel made from `self` can serve `other`: same SSH
    /// server, account, authentication method and key file, same known
    /// hosts file, policy and entry format, same target, and the same
    /// keepalive and timeout settings. Host names are compared without
    /// regard to case. Secrets (password, passphrase) are not compared: the
    /// session is already authenticated, and a new secret for the same
    /// account and key does not change what the tunnel connects to.
    pub(crate) fn same_endpoint(&self, other: &TunnelParams) -> bool {
        self.ssh_host.eq_ignore_ascii_case(&other.ssh_host)
            && self.ssh_port == other.ssh_port
            && self.username == other.username
            && self.auth.same_identity(&other.auth)
            && self.known_hosts == other.known_hosts
            && self.target_host.eq_ignore_ascii_case(&other.target_host)
            && self.target_port == other.target_port
            && self.keepalive == other.keepalive
            && self.connect_timeout == other.connect_timeout
    }
}

/// The state of a tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelStatus {
    /// Accepting and forwarding connections.
    Open,
    /// The SSH session dropped. New local connections are reset; the port
    /// stays bound until the [`Tunnel`] is dropped.
    Broken {
        /// Why the session ended.
        reason: String,
    },
    /// Closed on request. New local connections are reset; the port stays
    /// bound until the [`Tunnel`] is dropped.
    Closed,
}

struct Shared {
    state: watch::Sender<TunnelStatus>,
    last_forward_error: Mutex<Option<String>>,
    /// Whether accepted local connections are forwarded. When false they
    /// are reset at once (see [`Tunnel::set_accepting`]).
    accepting: AtomicBool,
    /// Set once the accept loop stopped forwarding and shut down every
    /// forwarded connection (or ended in any other way).
    drained: watch::Sender<bool>,
}

impl Shared {
    fn new() -> Self {
        Self {
            state: watch::Sender::new(TunnelStatus::Open),
            last_forward_error: Mutex::new(None),
            accepting: AtomicBool::new(true),
            drained: watch::Sender::new(false),
        }
    }

    fn mark_broken(&self, reason: String) {
        self.state.send_if_modified(|s| {
            if *s == TunnelStatus::Open {
                tracing::warn!(%reason, "SSH tunnel broken");
                *s = TunnelStatus::Broken { reason };
                true
            } else {
                false
            }
        });
    }

    fn mark_closed(&self) {
        self.state.send_if_modified(|s| {
            if *s == TunnelStatus::Closed {
                false
            } else {
                *s = TunnelStatus::Closed;
                true
            }
        });
    }

    fn set_forward_error(&self, reason: Option<String>) {
        *self
            .last_forward_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = reason;
    }
}

#[derive(Debug)]
enum HandlerError {
    Ssh(russh::Error),
    Tunnel(TunnelError),
}

impl From<russh::Error> for HandlerError {
    fn from(e: russh::Error) -> Self {
        HandlerError::Ssh(e)
    }
}

impl fmt::Display for HandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandlerError::Ssh(e) => describe_ssh_error(e).fmt(f),
            HandlerError::Tunnel(e) => e.fmt(f),
        }
    }
}

struct ClientHandler {
    host: String,
    port: u16,
    known_hosts: KnownHosts,
    shared: Arc<Shared>,
    /// Set when the connect attempt timed out, so a host key check still
    /// running on the blocking pool does not record a key for it.
    abandoned: Arc<AtomicBool>,
}

impl client::Handler for ClientHandler {
    type Error = HandlerError;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            // Certificates are not requested, but a server could still send
            // one: check its embedded key like a plain key.
            PublicKeyOrCertificate::Certificate(cert) => {
                PublicKey::new(cert.public_key().clone(), "")
            }
        };
        self.known_hosts
            .verify_on_blocking_pool(self.host.clone(), self.port, key, self.abandoned.clone())
            .await
            .map_err(HandlerError::Tunnel)?;
        Ok(true)
    }

    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        match reason {
            DisconnectReason::ReceivedDisconnect(info) => {
                let message = if info.message.is_empty() {
                    format!("{:?}", info.reason_code)
                } else {
                    info.message
                };
                self.shared
                    .mark_broken(format!("the SSH server disconnected: {message}"));
                Ok(())
            }
            DisconnectReason::Error(e) => {
                self.shared.mark_broken(e.to_string());
                Err(e)
            }
        }
    }
}

/// An open SSH tunnel. Dropping it closes it.
///
/// # Who can use the local port
///
/// The listener binds `127.0.0.1` on a free port, exactly like `ssh -L`.
/// Loopback is not reachable from other machines, but **any process of any
/// user on this computer can connect to it** and is then forwarded to the
/// target as the SSH account; the database still asks for its own
/// credentials. There is no per-connection authentication, as with
/// `ssh -L`.
///
/// # How long it listens
///
/// A tunnel opened with [`Tunnel::open`] forwards new connections until
/// [`Tunnel::close`] is called, it is dropped, or the SSH session breaks.
/// After a close or a break the port stays bound and resets every new
/// connection, so that no other local process can take it over while a
/// driver may still connect to it; the port is released when the tunnel
/// is dropped.
///
/// A tunnel shared by [`TunnelManager`](crate::TunnelManager) forwards new
/// connections only while at least one
/// [`TunnelLease`](crate::TunnelLease) is held. After the last lease is
/// dropped the SSH session and the port stay open for the manager's grace
/// period so a quick reconnect is fast, but every new local connection is
/// reset right after it is accepted, without opening a forwarding channel.
/// The port is kept bound (rather than closed and bound again) so that no
/// other local process can take it over while the tunnel is idle. A new
/// lease within the grace period makes it forward again; after the grace
/// period the tunnel closes and, with no lease left, is dropped, which
/// releases the port. A shared tunnel that breaks or is closed while leases
/// still exist keeps its port bound (resetting connections) until the last
/// of those leases is dropped. Connections that were forwarded before the
/// last lease was dropped keep running until either side closes them.
pub struct Tunnel {
    local_addr: SocketAddr,
    handle: Arc<Handle<ClientHandler>>,
    shared: Arc<Shared>,
    accept_task: Mutex<Option<JoinHandle<()>>>,
    ssh_host: String,
    ssh_port: u16,
    target_host: String,
    target_port: u16,
    timeout: Duration,
}

impl fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tunnel")
            .field("local_addr", &self.local_addr)
            .field("ssh", &format_args!("{}:{}", self.ssh_host, self.ssh_port))
            .field(
                "target",
                &format_args!("{}:{}", self.target_host, self.target_port),
            )
            .field("status", &self.status())
            .finish()
    }
}

impl Tunnel {
    /// Connects, checks the host key, authenticates and starts listening
    /// on `127.0.0.1` with a free port.
    ///
    /// # Errors
    /// [`TunnelError::Connect`], [`TunnelError::Timeout`], one of the host
    /// key errors, [`TunnelError::Auth`], [`TunnelError::KeyFile`],
    /// [`TunnelError::Agent`], [`TunnelError::Unsupported`] (agent on an
    /// unsupported platform) or [`TunnelError::Io`] (local listener).
    pub async fn open(params: TunnelParams) -> Result<Tunnel, TunnelError> {
        let shared = Arc::new(Shared::new());
        let mut handle = connect(&params, shared.clone()).await?;

        match tokio::time::timeout(params.connect_timeout, authenticate(&mut handle, &params)).await
        {
            Err(_) => {
                return Err(TunnelError::Timeout {
                    stage: TimeoutStage::Authentication,
                    after: params.connect_timeout,
                });
            }
            Ok(Err(e)) => return Err(e),
            Ok(Ok(())) => {}
        }

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| TunnelError::io("cannot listen on 127.0.0.1", e))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| TunnelError::io("cannot read the local tunnel address", e))?;

        let handle = Arc::new(handle);
        let accept_task = tokio::spawn(accept_loop(
            listener,
            handle.clone(),
            shared.clone(),
            Target {
                host: params.target_host.clone(),
                port: params.target_port,
                timeout: params.connect_timeout,
            },
        ));
        tracing::info!(
            ssh = %format_args!("{}:{}", params.ssh_host, params.ssh_port),
            target = %format_args!("{}:{}", params.target_host, params.target_port),
            %local_addr,
            "SSH tunnel open"
        );
        Ok(Tunnel {
            local_addr,
            handle,
            shared,
            accept_task: Mutex::new(Some(accept_task)),
            ssh_host: params.ssh_host,
            ssh_port: params.ssh_port,
            target_host: params.target_host,
            target_port: params.target_port,
            timeout: params.connect_timeout,
        })
    }

    /// The local address to connect the database driver to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The current state.
    #[must_use]
    pub fn status(&self) -> TunnelStatus {
        let status = self.shared.state.borrow().clone();
        if status == TunnelStatus::Open && self.handle.is_closed() {
            self.shared.mark_broken("the SSH session ended".to_owned());
            return self.shared.state.borrow().clone();
        }
        status
    }

    /// Whether the tunnel accepts and forwards connections.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.status() == TunnelStatus::Open
    }

    /// Waits until the tunnel is no longer open (broken or closed) and
    /// returns the new status.
    pub async fn wait_closed(&self) -> TunnelStatus {
        let mut rx = self.shared.state.subscribe();
        loop {
            let status = rx.borrow_and_update().clone();
            if status != TunnelStatus::Open {
                return status;
            }
            // A closed handle without a disconnect callback is caught by the
            // accept loop's poll, which then updates the state.
            if rx.changed().await.is_err() {
                return self.status();
            }
        }
    }

    /// Sends a keepalive request and waits for the server's answer.
    ///
    /// # Errors
    /// [`TunnelError::Closed`] when the tunnel is broken or closed,
    /// [`TunnelError::Timeout`] when the server does not answer in time.
    pub async fn check_health(&self) -> Result<(), TunnelError> {
        self.ensure_open()?;
        match tokio::time::timeout(self.timeout, self.handle.send_ping()).await {
            Err(_) => {
                return Err(TunnelError::Timeout {
                    stage: TimeoutStage::HealthCheck,
                    after: self.timeout,
                });
            }
            Ok(Err(e)) => self.shared.mark_broken(describe_ssh_error(&e)),
            Ok(Ok(())) => {}
        }
        self.ensure_open()
    }

    /// Opens and immediately closes a forwarding channel to the target, to
    /// check that the SSH server can reach it. This makes a short TCP
    /// connection to the target, which the database may log.
    ///
    /// # Errors
    /// [`TunnelError::Forward`] when the server cannot reach the target,
    /// [`TunnelError::Timeout`] or [`TunnelError::Closed`].
    pub async fn probe_target(&self) -> Result<(), TunnelError> {
        self.ensure_open()?;
        let target = Target {
            host: self.target_host.clone(),
            port: self.target_port,
            timeout: self.timeout,
        };
        let channel = open_channel(&self.handle, &target, self.local_addr).await?;
        let _ = channel.close().await;
        Ok(())
    }

    /// The most recent failure to open a forwarding channel, cleared by the
    /// next successful one. Useful to explain why a driver connection
    /// through the tunnel was closed right away.
    #[must_use]
    pub fn last_forward_error(&self) -> Option<TunnelError> {
        self.shared
            .last_forward_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map(|reason| TunnelError::Forward {
                target_host: self.target_host.clone(),
                target_port: self.target_port,
                reason,
            })
    }

    /// Stops forwarding, closes all forwarded connections and the session.
    ///
    /// The local port stays bound and resets every new connection until the
    /// tunnel is dropped, so that no other local process can take the port
    /// over while a driver may still connect to [`Tunnel::local_addr`].
    pub async fn close(&self) {
        self.shared.mark_closed();
        let mut drained = self.shared.drained.subscribe();
        // The sender lives in `shared`, which this tunnel holds, so this
        // only ends when the accept loop reports it is drained.
        let _ = drained.wait_for(|drained| *drained).await;
        if !self.handle.is_closed() {
            let _ = self
                .handle
                .disconnect(Disconnect::ByApplication, "tunnel closed", "en")
                .await;
        }
        tracing::info!(local_addr = %self.local_addr, "SSH tunnel closed");
    }

    /// Whether new local connections are forwarded (true) or reset at once
    /// (false). The manager switches this off while no lease is held.
    pub(crate) fn set_accepting(&self, accepting: bool) {
        self.shared.accepting.store(accepting, Ordering::SeqCst);
    }

    fn ensure_open(&self) -> Result<(), TunnelError> {
        match self.status() {
            TunnelStatus::Open => Ok(()),
            TunnelStatus::Broken { reason } => Err(TunnelError::Closed { reason }),
            TunnelStatus::Closed => Err(TunnelError::Closed {
                reason: "closed on request".to_owned(),
            }),
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.shared.mark_closed();
        if let Some(task) = self
            .accept_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            // Aborting drops the listener (releasing the port) and every
            // forwarded connection; the session ends when the last handle
            // reference is gone.
            task.abort();
        }
    }
}

async fn connect(
    params: &TunnelParams,
    shared: Arc<Shared>,
) -> Result<Handle<ClientHandler>, TunnelError> {
    let known = {
        let known_hosts = params.known_hosts.clone();
        let (host, port) = (params.ssh_host.clone(), params.ssh_port);
        tokio::task::spawn_blocking(move || known_hosts.known_algorithms(&host, port))
            .await
            .unwrap_or_default()
    };
    let mut preferred = Preferred::default();
    preferred.key = Cow::Owned(prefer_known(&preferred.key, &known));
    let config = client::Config {
        keepalive_interval: (!params.keepalive.is_zero()).then_some(params.keepalive),
        keepalive_max: KEEPALIVE_MAX,
        nodelay: true,
        preferred,
        ..Default::default()
    };
    let abandoned = Arc::new(AtomicBool::new(false));
    let handler = ClientHandler {
        host: params.ssh_host.clone(),
        port: params.ssh_port,
        known_hosts: params.known_hosts.clone(),
        shared,
        abandoned: abandoned.clone(),
    };
    // Marks the connect as abandoned unless it finishes: on the timeout
    // below, and when the caller drops this future (an outer timeout, a
    // `select!`, an aborted task). The session runs in its own task, so its
    // host key check would otherwise still record a key.
    let mut abandon = AbandonUnlessFinished {
        abandoned,
        finished: false,
    };
    let addr = (params.ssh_host.clone(), params.ssh_port);
    let result = tokio::time::timeout(
        params.connect_timeout,
        client::connect(Arc::new(config), addr, handler),
    )
    .await;
    abandon.finished = result.is_ok();
    drop(abandon);
    match result {
        Err(_) => Err(TunnelError::Timeout {
            stage: TimeoutStage::Connect,
            after: params.connect_timeout,
        }),
        Ok(Err(HandlerError::Tunnel(e))) => Err(e),
        Ok(Err(HandlerError::Ssh(e))) => Err(TunnelError::Connect {
            host: params.ssh_host.clone(),
            port: params.ssh_port,
            reason: describe_ssh_error(&e),
        }),
        Ok(Ok(handle)) => Ok(handle),
    }
}

/// Sets the abandoned flag of a connect attempt when dropped before the
/// attempt finished.
struct AbandonUnlessFinished {
    abandoned: Arc<AtomicBool>,
    finished: bool,
}

impl Drop for AbandonUnlessFinished {
    fn drop(&mut self) {
        if !self.finished {
            self.abandoned.store(true, Ordering::SeqCst);
        }
    }
}

async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    params: &TunnelParams,
) -> Result<(), TunnelError> {
    let lost = |e: russh::Error| TunnelError::Connect {
        host: params.ssh_host.clone(),
        port: params.ssh_port,
        reason: format!(
            "the connection failed during authentication: {}",
            describe_ssh_error(&e)
        ),
    };
    let result = match &params.auth {
        SshAuth::Password(password) => handle
            .authenticate_password(params.username.clone(), password.expose())
            .await
            .map_err(lost)?,
        SshAuth::KeyFile { path, passphrase } => {
            let (path, passphrase) = (path.clone(), passphrase.clone());
            // Decrypting a key runs a deliberately slow KDF.
            let key =
                tokio::task::spawn_blocking(move || load_private_key(&path, passphrase.as_ref()))
                    .await
                    .map_err(|e| {
                        TunnelError::io(
                            "loading the key file failed",
                            std::io::Error::other(e.to_string()),
                        )
                    })??;
            let hash = if key.algorithm().is_rsa() {
                rsa_hash(handle).await
            } else {
                None
            };
            handle
                .authenticate_publickey(
                    params.username.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
                .map_err(lost)?
        }
        SshAuth::Agent => return authenticate_agent(handle, params).await,
    };
    auth_outcome(result, params)
}

fn auth_outcome(result: client::AuthResult, params: &TunnelParams) -> Result<(), TunnelError> {
    match result {
        client::AuthResult::Success => Ok(()),
        client::AuthResult::Failure {
            remaining_methods,
            partial_success,
        } => Err(TunnelError::Auth {
            username: params.username.clone(),
            method: params.auth.method(),
            accepted: remaining_methods.iter().map(String::from).collect(),
            partial_success,
        }),
    }
}

/// The RSA signature hash to use: the strongest one the server lists in
/// `server-sig-algs`; SHA-256 when the server sends no list.
async fn rsa_hash(handle: &Handle<ClientHandler>) -> Option<HashAlg> {
    match handle.best_supported_rsa_hash().await {
        Ok(Some(hash)) => hash,
        Ok(None) | Err(_) => Some(HashAlg::Sha256),
    }
}

type DynAgent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

#[cfg(unix)]
async fn connect_agent() -> Result<DynAgent, TunnelError> {
    let Some(path) = std::env::var_os("SSH_AUTH_SOCK").filter(|p| !p.is_empty()) else {
        return Err(TunnelError::Agent {
            reason: "SSH_AUTH_SOCK is not set; start an SSH agent or choose another method"
                .to_owned(),
        });
    };
    AgentClient::connect_uds(&path)
        .await
        .map(AgentClient::dynamic)
        .map_err(|e| TunnelError::Agent {
            reason: format!(
                "cannot connect to the agent at {}: {e}",
                PathBuf::from(&path).display()
            ),
        })
}

#[cfg(windows)]
async fn connect_agent() -> Result<DynAgent, TunnelError> {
    const PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    AgentClient::connect_named_pipe(PIPE)
        .await
        .map(AgentClient::dynamic)
        .map_err(|e| TunnelError::Agent {
            reason: format!("cannot connect to the OpenSSH agent ({PIPE}): {e}"),
        })
}

#[cfg(not(any(unix, windows)))]
async fn connect_agent() -> Result<DynAgent, TunnelError> {
    Err(TunnelError::Unsupported {
        feature: "SSH agent authentication".to_owned(),
    })
}

async fn authenticate_agent(
    handle: &mut Handle<ClientHandler>,
    params: &TunnelParams,
) -> Result<(), TunnelError> {
    let mut agent = connect_agent().await?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| TunnelError::Agent {
            reason: format!("cannot list the agent's keys: {e}"),
        })?;
    let keys: Vec<PublicKey> = identities
        .iter()
        .filter_map(|id| match id {
            russh::keys::agent::AgentIdentity::PublicKey { key, .. } => Some(key.clone()),
            russh::keys::agent::AgentIdentity::Certificate { .. } => None,
        })
        .collect();
    if keys.is_empty() {
        return Err(TunnelError::Agent {
            reason: "the SSH agent holds no keys".to_owned(),
        });
    }
    let mut last = None;
    for key in keys {
        let hash = if key.algorithm().is_rsa() {
            rsa_hash(handle).await
        } else {
            None
        };
        let result = handle
            .authenticate_publickey_with(params.username.clone(), key, hash, &mut agent)
            .await
            .map_err(|e| TunnelError::Agent {
                reason: format!("the agent could not sign: {e}"),
            })?;
        match result {
            client::AuthResult::Success => return Ok(()),
            client::AuthResult::Failure {
                ref remaining_methods,
                partial_success,
            } => {
                let more = !partial_success && remaining_methods.contains(&MethodKind::PublicKey);
                last = Some(result);
                if !more {
                    break;
                }
            }
        }
    }
    match last {
        Some(result) => auth_outcome(result, params),
        None => Err(TunnelError::Agent {
            reason: "the SSH agent holds no keys".to_owned(),
        }),
    }
}

struct Target {
    host: String,
    port: u16,
    timeout: Duration,
}

async fn open_channel(
    handle: &Handle<ClientHandler>,
    target: &Target,
    originator: SocketAddr,
) -> Result<russh::Channel<client::Msg>, TunnelError> {
    let open = handle.channel_open_direct_tcpip(
        target.host.clone(),
        u32::from(target.port),
        originator.ip().to_string(),
        u32::from(originator.port()),
    );
    match tokio::time::timeout(target.timeout, open).await {
        Err(_) => Err(TunnelError::Timeout {
            stage: TimeoutStage::Forward,
            after: target.timeout,
        }),
        Ok(Err(russh::Error::ChannelOpenFailure(failure))) => Err(TunnelError::Forward {
            target_host: target.host.clone(),
            target_port: target.port,
            reason: describe_open_failure(&failure),
        }),
        Ok(Err(e)) => Err(TunnelError::Closed {
            reason: describe_ssh_error(&e),
        }),
        Ok(Ok(channel)) => Ok(channel),
    }
}

/// The local side of the accept loop, so that the loop can be tested
/// without a real listener.
trait LocalListener: Send + Sync + 'static {
    fn accept(&self) -> impl Future<Output = std::io::Result<(TcpStream, SocketAddr)>> + Send;
}

impl LocalListener for TcpListener {
    fn accept(&self) -> impl Future<Output = std::io::Result<(TcpStream, SocketAddr)>> + Send {
        TcpListener::accept(self)
    }
}

/// The SSH side of the accept loop, so that the loop can be tested without
/// an SSH session.
trait Session: Send + Sync + 'static {
    fn is_closed(&self) -> bool;
    fn forward(
        self: &Arc<Self>,
        local: TcpStream,
        peer: SocketAddr,
        shared: Arc<Shared>,
        target: Arc<Target>,
    ) -> impl Future<Output = ()> + Send + 'static;
}

impl Session for Handle<ClientHandler> {
    fn is_closed(&self) -> bool {
        Handle::is_closed(self)
    }

    fn forward(
        self: &Arc<Self>,
        local: TcpStream,
        peer: SocketAddr,
        shared: Arc<Shared>,
        target: Arc<Target>,
    ) -> impl Future<Output = ()> + Send + 'static {
        forward(local, peer, self.clone(), shared, target)
    }
}

async fn accept_loop<L: LocalListener, S: Session>(
    listener: L,
    session: Arc<S>,
    shared: Arc<Shared>,
    target: Target,
) {
    let target = Arc::new(target);
    let drained = DrainedOnDrop(shared.clone());
    let mut state = shared.state.subscribe();
    let mut connections = JoinSet::new();
    let mut poll = tokio::time::interval(SESSION_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut backoff = AcceptBackoff::new();
    // While set, `accept` failed and is not tried again before this time.
    // The pause is a branch of the `select!` below, so a close or break is
    // still noticed at once.
    let mut resume_at: Option<tokio::time::Instant> = None;
    loop {
        if *state.borrow_and_update() != TunnelStatus::Open {
            break;
        }
        tokio::select! {
            // State changes first: a connection that is ready in the same
            // wakeup as a close or break is left for the reset loop below.
            biased;
            changed = state.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            _ = poll.tick() => {
                if session.is_closed() {
                    shared.mark_broken("the SSH session ended".to_owned());
                }
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            () = tokio::time::sleep_until(resume_at.unwrap_or_else(tokio::time::Instant::now)),
                if resume_at.is_some() => {
                resume_at = None;
            }
            accepted = listener.accept(), if resume_at.is_none() => match accepted {
                Ok((stream, peer)) => {
                    backoff.succeeded();
                    // The state can still change on another thread after
                    // the branches above were polled: decide now.
                    match admission(&shared, session.is_closed()) {
                        Admission::Forward => {
                            connections.spawn(session.forward(stream, peer, shared.clone(), target.clone()));
                        }
                        Admission::Refuse(why) => refuse(stream, peer, why),
                        Admission::RefuseAndStop(why) => {
                            if session.is_closed() {
                                shared.mark_broken("the SSH session ended".to_owned());
                            }
                            refuse(stream, peer, why);
                            break;
                        }
                    }
                }
                Err(e) => {
                    resume_at = Some(tokio::time::Instant::now() + backoff.failed(&e));
                }
            },
        }
    }
    connections.shutdown().await;
    drop(drained);
    // Keep the port bound while anyone may still connect to it: releasing it
    // would let another local process listen on it and receive the next
    // driver connection (and its credentials). Every connection is reset
    // until the tunnel is dropped, which aborts this task. Nothing else
    // happens here, so pausing inline after a failure is fine.
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                backoff.succeeded();
                refuse(stream, peer, "the tunnel is no longer open");
            }
            Err(e) => tokio::time::sleep(backoff.failed(&e)).await,
        }
    }
}

/// What to do with an accepted local connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// Forward it to the target.
    Forward,
    /// Reset it and keep forwarding later connections.
    Refuse(&'static str),
    /// Reset it and stop forwarding: the tunnel is no longer open.
    RefuseAndStop(&'static str),
}

/// Decides about a connection that was just accepted, from the tunnel state
/// at this moment (not when the accept loop last woke up).
fn admission(shared: &Shared, session_closed: bool) -> Admission {
    if *shared.state.borrow() != TunnelStatus::Open {
        Admission::RefuseAndStop("the tunnel is no longer open")
    } else if session_closed {
        Admission::RefuseAndStop("the SSH session ended")
    } else if !shared.accepting.load(Ordering::SeqCst) {
        Admission::Refuse("no lease is held")
    } else {
        Admission::Forward
    }
}

/// Backs off while `accept` keeps failing (usually: out of file
/// descriptors) and limits the warnings to one per [`ACCEPT_WARN_EVERY`].
struct AcceptBackoff {
    delay: Duration,
    last_warning: Option<Instant>,
    suppressed: u64,
}

impl AcceptBackoff {
    fn new() -> Self {
        Self {
            delay: ACCEPT_BACKOFF_MIN,
            last_warning: None,
            suppressed: 0,
        }
    }

    fn succeeded(&mut self) {
        self.delay = ACCEPT_BACKOFF_MIN;
    }

    /// Records a failure at `now`. Returns how long to pause and, when a
    /// warning is due, how many failures were not reported since the last
    /// one.
    fn record_failure(&mut self, now: Instant) -> (Duration, Option<u64>) {
        let pause = self.delay;
        self.delay = (self.delay * 2).min(ACCEPT_BACKOFF_MAX);
        let warn = self
            .last_warning
            .is_none_or(|last| now.duration_since(last) >= ACCEPT_WARN_EVERY);
        if warn {
            self.last_warning = Some(now);
            let suppressed = std::mem::take(&mut self.suppressed);
            (pause, Some(suppressed))
        } else {
            self.suppressed += 1;
            (pause, None)
        }
    }

    /// Records and reports a failure; returns how long to pause.
    fn failed(&mut self, error: &std::io::Error) -> Duration {
        let (pause, warn) = self.record_failure(Instant::now());
        match warn {
            Some(suppressed) => tracing::warn!(
                %error,
                suppressed,
                retry_in = ?pause,
                "accepting a tunnel connection failed"
            ),
            None => tracing::debug!(%error, "accepting a tunnel connection failed"),
        }
        pause
    }
}

/// Reports the accept loop as drained when dropped: after the forwarded
/// connections are shut down, or when the task is aborted or panics, so
/// that [`Tunnel::close`] never waits forever.
struct DrainedOnDrop(Arc<Shared>);

impl Drop for DrainedOnDrop {
    fn drop(&mut self) {
        self.0.drained.send_replace(true);
    }
}

/// Resets a local connection that must not be forwarded: with a zero linger
/// time, dropping the socket sends RST instead of FIN, so the client sees
/// "connection reset" and nothing is forwarded.
fn refuse(stream: TcpStream, peer: SocketAddr, why: &str) {
    if let Err(e) = stream.set_zero_linger() {
        tracing::debug!(error = %e, "cannot set zero linger on a refused connection");
    }
    tracing::debug!(%peer, why, "refused a tunnel connection");
    drop(stream);
}

async fn forward(
    mut local: TcpStream,
    peer: SocketAddr,
    handle: Arc<Handle<ClientHandler>>,
    shared: Arc<Shared>,
    target: Arc<Target>,
) {
    let _ = local.set_nodelay(true);
    let channel = match open_channel(&handle, &target, peer).await {
        Ok(channel) => channel,
        Err(TunnelError::Closed { reason }) => {
            shared.mark_broken(reason);
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot forward a tunnel connection");
            let reason = match e {
                TunnelError::Forward { reason, .. } => reason,
                other => other.to_string(),
            };
            shared.set_forward_error(Some(reason));
            return;
        }
    };
    shared.set_forward_error(None);
    let mut remote = channel.into_stream();
    match tokio::io::copy_bidirectional(&mut local, &mut remote).await {
        Ok((up, down)) => tracing::trace!(up, down, "tunnel connection finished"),
        Err(e) => tracing::debug!(error = %e, "tunnel connection ended with an error"),
    }
}

fn describe_open_failure(failure: &ChannelOpenFailure) -> String {
    match failure {
        ChannelOpenFailure::ConnectFailed => {
            "the target refused the connection or cannot be reached from the SSH server".to_owned()
        }
        ChannelOpenFailure::AdministrativelyProhibited => {
            "port forwarding is not allowed on the SSH server (AllowTcpForwarding, PermitOpen)"
                .to_owned()
        }
        ChannelOpenFailure::ResourceShortage => "the SSH server is out of resources".to_owned(),
        ChannelOpenFailure::UnknownChannelType => {
            "the SSH server does not support TCP forwarding".to_owned()
        }
        ChannelOpenFailure::Other { code, reason } => {
            if reason.is_empty() {
                format!("refused with code {code}")
            } else {
                reason.clone()
            }
        }
    }
}

fn describe_ssh_error(e: &russh::Error) -> String {
    use russh::Error as E;
    match e {
        E::IO(io) => io.to_string(),
        E::Disconnect | E::HUP | E::SendError | E::RecvError => {
            "the SSH server closed the connection".to_owned()
        }
        E::KeepaliveTimeout => "the SSH server stopped answering keepalives".to_owned(),
        E::ConnectionTimeout | E::InactivityTimeout => "the SSH connection timed out".to_owned(),
        E::Version => "the server does not speak SSH".to_owned(),
        E::NoCommonAlgo { kind, theirs, .. } => {
            format!(
                "no common {kind:?} algorithm with the server (it offers {})",
                theirs.join(", ")
            )
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::known_hosts::HostKeyPolicy;

    type Change = (&'static str, fn(&mut TunnelParams));

    fn base() -> TunnelParams {
        let mut p = TunnelParams::new(
            "Bastion.Example",
            "deploy",
            SshAuth::KeyFile {
                path: PathBuf::from("/keys/id_ed25519"),
                passphrase: Some(Secret::new("one".to_owned())),
            },
            "DB.internal",
            5432,
            KnownHosts::new("/app/known_hosts", HostKeyPolicy::AcceptNew),
        );
        p.ssh_port = 2222;
        p
    }

    #[test]
    fn same_endpoint_ignores_secrets_and_host_case() {
        let a = base();
        let mut b = base();
        b.ssh_host = "bastion.example".to_owned();
        b.target_host = "db.INTERNAL".to_owned();
        b.auth = SshAuth::KeyFile {
            path: PathBuf::from("/keys/id_ed25519"),
            passphrase: Some(Secret::new("two".to_owned())),
        };
        assert!(a.same_endpoint(&b));

        let mut pw_a = base();
        pw_a.auth = SshAuth::Password(Secret::new("old".to_owned()));
        let mut pw_b = base();
        pw_b.auth = SshAuth::Password(Secret::new("new".to_owned()));
        assert!(pw_a.same_endpoint(&pw_b));

        let mut agent_a = base();
        agent_a.auth = SshAuth::Agent;
        let mut agent_b = base();
        agent_b.auth = SshAuth::Agent;
        assert!(agent_a.same_endpoint(&agent_b));
    }

    #[test]
    fn same_endpoint_detects_every_tunnel_defining_change() {
        let changes: Vec<Change> = vec![
            ("ssh host", |p| p.ssh_host = "other.example".to_owned()),
            ("ssh port", |p| p.ssh_port = 22),
            ("user", |p| p.username = "root".to_owned()),
            ("auth method", |p| {
                p.auth = SshAuth::Password(Secret::new("pw".to_owned()));
            }),
            ("agent", |p| p.auth = SshAuth::Agent),
            ("key file path", |p| {
                p.auth = SshAuth::KeyFile {
                    path: PathBuf::from("/keys/id_rsa"),
                    passphrase: Some(Secret::new("one".to_owned())),
                };
            }),
            ("stricter policy", |p| {
                p.known_hosts.policy = HostKeyPolicy::Strict;
            }),
            ("known hosts path", |p| {
                p.known_hosts.path = PathBuf::from("/other/known_hosts");
            }),
            ("hashed entries", |p| p.known_hosts.hash_new_entries = true),
            ("target host", |p| {
                p.target_host = "replica.internal".to_owned()
            }),
            ("target port", |p| p.target_port = 5433),
            ("keepalive", |p| p.keepalive = Duration::ZERO),
            ("timeout", |p| p.connect_timeout = Duration::from_secs(3)),
        ];
        let a = base();
        for (what, change) in changes {
            let mut b = base();
            change(&mut b);
            assert!(!a.same_endpoint(&b), "{what} must open a new tunnel");
            assert!(!b.same_endpoint(&a), "{what} must open a new tunnel");
        }
    }

    /// A connection accepted after the tunnel was closed or broke (possibly
    /// on another thread, after the accept loop last looked) is reset.
    #[test]
    fn admission_follows_the_current_state() {
        let shared = Shared::new();
        assert_eq!(admission(&shared, false), Admission::Forward);
        assert!(matches!(
            admission(&shared, true),
            Admission::RefuseAndStop(_)
        ));
        shared.accepting.store(false, Ordering::SeqCst);
        assert!(matches!(admission(&shared, false), Admission::Refuse(_)));
        shared.accepting.store(true, Ordering::SeqCst);

        shared.mark_closed();
        assert!(matches!(
            admission(&shared, false),
            Admission::RefuseAndStop(_)
        ));
        let broken = Shared::new();
        broken.mark_broken("gone".to_owned());
        assert!(matches!(
            admission(&broken, false),
            Admission::RefuseAndStop(_)
        ));
    }

    #[test]
    fn accept_failures_back_off_and_warn_rarely() {
        let mut backoff = AcceptBackoff::new();
        let start = Instant::now();
        let mut now = start;
        let mut warnings = Vec::new();
        let mut pauses = Vec::new();
        // A minute of continuous failures (out of file descriptors).
        while now.duration_since(start) < Duration::from_secs(60) {
            let (pause, warn) = backoff.record_failure(now);
            if let Some(suppressed) = warn {
                warnings.push(suppressed);
            }
            pauses.push(pause);
            now += pause;
        }
        assert_eq!(pauses.first(), Some(&ACCEPT_BACKOFF_MIN));
        assert!(pauses.iter().all(|p| *p <= ACCEPT_BACKOFF_MAX));
        assert!(pauses.windows(2).all(|w| w[0] <= w[1]));
        // About 35 attempts instead of 1200, and 2 warnings instead of one
        // per attempt; the second one counts the failures it stood for.
        assert!(pauses.len() < 50, "{}", pauses.len());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert_eq!(warnings.first(), Some(&0));
        assert!(warnings.get(1).is_some_and(|n| *n > 10), "{warnings:?}");

        // A success resets the pause; failures right after a warning are
        // not reported again.
        backoff.succeeded();
        let (pause, _) = backoff.record_failure(now);
        assert_eq!(pause, ACCEPT_BACKOFF_MIN);
        let (pause, warn) = backoff.record_failure(now + pause);
        assert_eq!(pause, ACCEPT_BACKOFF_MIN * 2);
        assert_eq!(warn, None);
    }

    /// A listener whose `accept` always fails, like one out of file
    /// descriptors.
    struct FailingListener;

    impl LocalListener for FailingListener {
        fn accept(&self) -> impl Future<Output = std::io::Result<(TcpStream, SocketAddr)>> + Send {
            std::future::ready(Err(std::io::Error::other("too many open files")))
        }
    }

    struct IdleSession;

    impl Session for IdleSession {
        fn is_closed(&self) -> bool {
            false
        }

        fn forward(
            self: &Arc<Self>,
            _local: TcpStream,
            _peer: SocketAddr,
            _shared: Arc<Shared>,
            _target: Arc<Target>,
        ) -> impl Future<Output = ()> + Send + 'static {
            std::future::ready(())
        }
    }

    /// While `accept` keeps failing the loop pauses up to two seconds; a
    /// close during such a pause must still be handled at once (and
    /// `close()` must not wait for the pause).
    #[tokio::test]
    async fn close_is_noticed_during_an_accept_backoff() -> Result<(), Box<dyn std::error::Error>> {
        let shared = Arc::new(Shared::new());
        let task = tokio::spawn(accept_loop(
            FailingListener,
            Arc::new(IdleSession),
            shared.clone(),
            Target {
                host: "target.example".to_owned(),
                port: 5432,
                timeout: Duration::from_secs(1),
            },
        ));
        // Pauses of 50, 100, 200, 400 and 800 ms end after 1550 ms; the next
        // pause, of 1600 ms, is running at 1700 ms.
        tokio::time::sleep(Duration::from_millis(1700)).await;
        assert!(!task.is_finished());
        let closed_at = Instant::now();
        shared.mark_closed();
        let mut drained = shared.drained.subscribe();
        tokio::time::timeout(Duration::from_secs(5), drained.wait_for(|d| *d))
            .await?
            .map_err(|e| e.to_string())?;
        let took = closed_at.elapsed();
        task.abort();
        assert!(
            took < Duration::from_millis(300),
            "the close waited for the accept backoff: {took:?}"
        );
        Ok(())
    }
}
