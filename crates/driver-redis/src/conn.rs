//! Opening a connection: TCP, optional TLS, AUTH, SELECT, CLIENT SETNAME.
//! Every connection reads through a [`ReadBudget`], so no single reply can
//! make the protocol library buffer more than [`MAX_REPLY_BYTES`].

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use redis::Value as Resp;
use redis::aio::MultiplexedConnection;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::task::AbortHandle;
use varve_core::Engine;
use varve_driver::{ConnectParams, DriverError, ErrorKind, ServerInfo, parse_version};
use varve_secrets::Secret;

use crate::error;
use crate::reply::{as_int, as_text};
use crate::session::MAX_REPLY_BYTES;
use crate::tls::{TlsPlan, TlsTarget, plan};

/// Everything needed to (re)open a connection. Holds the password for
/// transparent reconnects; it is only exposed into the connection info.
#[derive(Clone)]
pub struct Endpoint {
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<Secret>,
    tls: TlsPlan,
    connect_timeout: Duration,
    client_name: String,
    /// With TLS mode `prefer`: whether the first successful connection
    /// used TLS (`Some(true)`) or fell back to plain TCP (`Some(false)`).
    /// Later connections of the session (reconnects, the browser's side
    /// connections, cancel connections) use the same transport, so they
    /// neither wait for a TLS attempt that cannot work nor fall back to
    /// plain TCP once TLS worked. Shared by every clone.
    negotiated: Arc<std::sync::Mutex<Option<bool>>>,
    /// How messages name the endpoint instead of `host:port`, for example
    /// the real server behind an SSH tunnel (the driver connects to the
    /// tunnel's local end). Shared by every clone.
    label: Arc<std::sync::Mutex<Option<String>>>,
}

impl Endpoint {
    /// Builds the endpoint from connection parameters.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Tls`] when the TLS settings are invalid.
    pub async fn new(params: &ConnectParams) -> Result<Self, DriverError> {
        Ok(Self {
            host: params.host.clone(),
            port: params.port,
            username: params.username.clone().filter(|u| !u.is_empty()),
            password: params.password.clone(),
            tls: plan(&params.tls, &params.host, params.connect_timeout).await?,
            connect_timeout: params.connect_timeout,
            client_name: client_name(&params.application_name),
            negotiated: Arc::new(std::sync::Mutex::new(None)),
            label: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    /// Names the endpoint in later messages (see `label`).
    pub(crate) fn set_label(&self, label: String) {
        *self
            .label
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(label);
    }

    /// `host:port` (or the label) for messages.
    pub fn display(&self) -> String {
        if let Some(label) = self
            .label
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            return label.clone();
        }
        self.address()
    }

    /// The connect timeout.
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// The `CLIENT SETNAME` value.
    pub fn client_name(&self) -> &str {
        &self.client_name
    }

    /// Runs a connect (`work`) under the connect timeout, like
    /// [`Endpoint::bounded`], and says in the timeout error how far it got:
    /// a server that never answered "did not answer" (with `prefer`: not
    /// the plain probe and not the TLS handshake either), a server that
    /// answered and then stalled during the setup "timed out". The error
    /// keeps the address actually connected to in its detail when the
    /// endpoint has a label.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Timeout`] when the timeout passes first.
    pub async fn bounded_connect<T>(
        &self,
        work: impl Future<Output = T>,
    ) -> Result<T, DriverError> {
        let progress = Arc::new(ConnectProgress::default());
        let scoped = PROGRESS.scope(Arc::clone(&progress), work);
        match tokio::time::timeout(self.connect_timeout, scoped).await {
            Ok(value) => Ok(value),
            Err(_) => {
                let seconds = self.connect_timeout.as_secs_f32();
                let message = if progress.answered.load(Ordering::SeqCst) {
                    format!(
                        "Connecting to {} timed out after {seconds} s during the session setup \
                         (the server answered, then stopped answering)",
                        self.display()
                    )
                } else if progress.probe_silent.load(Ordering::SeqCst) {
                    format!(
                        "{} did not answer within {seconds} s: neither the plain Redis PING nor \
                         the TLS handshake got an answer",
                        self.display()
                    )
                } else {
                    format!("{} did not answer within {seconds} s", self.display())
                };
                Err(self.with_local(DriverError::new(ErrorKind::Timeout, message)))
            }
        }
    }

    /// Puts the address actually connected to (`host:port`) into the
    /// detail of an error when the endpoint has a label (the label replaces
    /// it in messages), so the local end of an SSH tunnel stays visible
    /// for troubleshooting.
    pub(crate) fn with_local(&self, mut error: DriverError) -> DriverError {
        if self.has_label() && error.detail.is_none() {
            error.detail = Some(format!("Connected to {}", self.address()));
        }
        error
    }

    fn has_label(&self) -> bool {
        self.label
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// `host:port` actually connected to, whatever the label.
    fn address(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Runs `work` under the connect timeout. Everything a connect does
    /// (TCP, TLS, AUTH, `CLIENT ID`, init commands, `INFO`) goes through
    /// here, so a server that accepts the connection and then stalls
    /// cannot hang it.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Timeout`] when the timeout passes first.
    pub async fn bounded<T>(&self, work: impl Future<Output = T>) -> Result<T, DriverError> {
        tokio::time::timeout(self.connect_timeout, work)
            .await
            .map_err(|_| {
                DriverError::new(
                    ErrorKind::Timeout,
                    format!(
                        "Connecting to {} timed out after {} s",
                        self.display(),
                        self.connect_timeout.as_secs_f32()
                    ),
                )
            })
    }

    /// Opens an authenticated connection on database `db`, named `name`,
    /// with TLS fallback for `prefer`. It has no deadline of its own:
    /// callers wrap it in [`Endpoint::bounded`] together with their setup
    /// commands.
    ///
    /// # Errors
    ///
    /// Connect, Tls or Auth errors.
    pub async fn open(&self, db: u16, name: &str) -> Result<Connection, DriverError> {
        let stream = self.open_stream().await?;
        self.open_on(stream, db, name)
            .await
            .map_err(|e| self.with_local(e))
    }

    /// A raw byte stream to the server, not yet authenticated: TCP, and TLS
    /// as planned. With `prefer` the first connection probes the server
    /// with a plain `PING` (see [`Endpoint::probe_plain`]): a server that
    /// answers like Redis gets plain TCP on that same connection, any other
    /// server gets a TLS handshake. The outcome is remembered and every
    /// later connection uses that transport (see `negotiated`). Callers
    /// bound it with [`Endpoint::bounded`].
    ///
    /// No credentials are sent before the transport is decided: the probe
    /// is a bare `PING`. When the server answers it in plain text, `prefer`
    /// sends AUTH over plain TCP, which is what `prefer` allows (use
    /// `require` or a verify mode to never send credentials unencrypted).
    ///
    /// # Errors
    ///
    /// Connect or Tls errors.
    pub(crate) async fn open_stream(&self) -> Result<Box<dyn Stream>, DriverError> {
        self.open_stream_inner()
            .await
            .map_err(|e| self.with_local(e))
    }

    async fn open_stream_inner(&self) -> Result<Box<dyn Stream>, DriverError> {
        let target = match &self.tls {
            TlsPlan::Plain => return self.stream_on(None).await,
            TlsPlan::Tls(target) => return self.stream_on(Some(target)).await,
            TlsPlan::TlsThenPlain(target) => target,
        };
        match self.transport() {
            Some(true) => return self.stream_on(Some(target)).await,
            Some(false) => return self.stream_on(None).await,
            None => {}
        }
        let refusal = match self.probe_plain().await? {
            Probe::Plain(tcp) => {
                self.remember(false);
                return Ok(Box::new(tcp));
            }
            Probe::PlainClosed => {
                // Redis answered with an error that may end the
                // connection (protected mode, loading): a fresh plain
                // connection lets the normal connect report it.
                self.remember(false);
                return self.stream_on(None).await;
            }
            Probe::NotPlain(refusal) => refusal,
        };
        tracing::debug!(
            endpoint = %self.display(),
            reason = %refusal.reason(),
            "no plain Redis reply, trying TLS"
        );
        let tcp = self.tcp().await?;
        match self.handshake(tcp, target).await {
            Ok(stream) => {
                self.remember(true);
                Ok(Box::new(stream))
            }
            Err(Handshake::Silent) if refusal == Refusal::Silent => Err(DriverError::new(
                ErrorKind::Timeout,
                format!(
                    "{} did not answer: neither the plain Redis PING nor the TLS handshake got \
                     an answer",
                    self.display()
                ),
            )),
            Err(Handshake::Closed(_)) if refusal == Refusal::Closed => Err(DriverError::new(
                ErrorKind::Connect,
                format!(
                    "{} closed the connection without answering, neither to a plain Redis PING \
                     nor to a TLS handshake",
                    self.display()
                ),
            )),
            Err(failure) => Err(error::tls(format!(
                "{} (the server did not answer a plain Redis PING either: {})",
                failure.message(&self.display()),
                refusal.reason()
            ))),
        }
    }

    /// Sends a bare `PING` over plain TCP and reads the first reply line,
    /// to tell a plain Redis from a TLS server without waiting for a TLS
    /// handshake that a plain Redis never answers (it takes a ClientHello
    /// for the start of an inline command and waits for its newline).
    ///
    /// The probe is a RESP array: its first byte (`*`) is no TLS record
    /// type, so a TLS server rejects it at once with an alert or by
    /// closing. A server that stays silent gets half of the connect
    /// timeout, which leaves time for the TLS attempt.
    ///
    /// # Errors
    ///
    /// Connect errors when the TCP connection cannot be opened.
    async fn probe_plain(&self) -> Result<Probe, DriverError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut tcp = self.tcp().await?;
        if tcp.write_all(PROBE).await.is_err() {
            return Ok(Probe::NotPlain(Refusal::Closed));
        }
        let read_line = async {
            let mut line = Vec::new();
            let mut chunk = [0; 128];
            loop {
                let n = tcp.read(&mut chunk).await?;
                if n == 0 {
                    return Ok::<_, io::Error>(line);
                }
                line.extend_from_slice(chunk.get(..n).unwrap_or_default());
                if line.windows(2).any(|w| w == b"\r\n") || line.len() >= PROBE_REPLY_MAX {
                    return Ok(line);
                }
            }
        };
        let line = match tokio::time::timeout(self.connect_timeout / 2, read_line).await {
            Err(_) => {
                let _ = PROGRESS.try_with(|p| p.probe_silent.store(true, Ordering::SeqCst));
                return Ok(Probe::NotPlain(Refusal::Silent));
            }
            Ok(Err(_)) => return Ok(Probe::NotPlain(Refusal::Closed)),
            Ok(Ok(line)) => line,
        };
        if !line.is_empty() {
            mark_answered();
        }
        Ok(match classify_probe_reply(&line) {
            ProbeReply::Reuse => Probe::Plain(tcp),
            ProbeReply::PlainClosed => Probe::PlainClosed,
            ProbeReply::NotPlain(refusal) => Probe::NotPlain(refusal),
        })
    }

    /// True when connections of this endpoint use TLS: always with
    /// `require` and the verify modes, never with `disable`, and with
    /// `prefer` when the first connection settled on TLS.
    pub(crate) fn uses_tls(&self) -> bool {
        match self.tls {
            TlsPlan::Plain => false,
            TlsPlan::Tls(_) => true,
            TlsPlan::TlsThenPlain(_) => self.transport() == Some(true),
        }
    }

    /// The transport `prefer` settled on, if any yet.
    pub(crate) fn transport(&self) -> Option<bool> {
        *self
            .negotiated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn remember(&self, tls: bool) {
        let mut negotiated = self
            .negotiated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The first outcome stands: a later connection never changes it.
        negotiated.get_or_insert(tls);
    }

    /// The user name, if any.
    pub(crate) fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    /// The password, if any.
    pub(crate) fn password(&self) -> Option<&Secret> {
        self.password.as_ref()
    }

    /// A TCP connection to the server with `TCP_NODELAY`.
    async fn tcp(&self) -> Result<TcpStream, DriverError> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|e| error::from_connect_io(&e, &self.display()))?;
        tcp.set_nodelay(true)
            .map_err(|e| error::from_connect_io(&e, &self.display()))?;
        Ok(tcp)
    }

    /// The TLS handshake on `tcp`, within half of the connect timeout: a
    /// plain server never answers a ClientHello, and `require` reports that
    /// as a TLS error rather than as a connect timeout.
    async fn handshake(
        &self,
        tcp: TcpStream,
        target: &TlsTarget,
    ) -> Result<tokio_rustls::client::TlsStream<TcpStream>, Handshake> {
        let connector = tokio_rustls::TlsConnector::from(target.config.clone());
        let handshake = connector.connect(target.server_name.clone(), tcp);
        match tokio::time::timeout(self.connect_timeout / 2, handshake).await {
            Err(_) => Err(Handshake::Silent),
            Ok(Ok(stream)) => {
                mark_answered();
                Ok(stream)
            }
            Ok(Err(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                Err(Handshake::Closed(e))
            }
            Ok(Err(e)) => Err(Handshake::Failed(e)),
        }
    }

    async fn stream_on(&self, tls: Option<&TlsTarget>) -> Result<Box<dyn Stream>, DriverError> {
        let tcp = self.tcp().await?;
        Ok(match tls {
            None => Box::new(tcp),
            Some(target) => Box::new(
                self.handshake(tcp, target)
                    .await
                    .map_err(|failure| error::tls(failure.message(&self.display())))?,
            ),
        })
    }

    async fn open_on(
        &self,
        plain: Box<dyn Stream>,
        db: u16,
        name: &str,
    ) -> Result<Connection, DriverError> {
        // SELECT is sent below rather than by the library, so its error
        // keeps the server's code (NOAUTH when no password was given).
        let mut info =
            redis::RedisConnectionInfo::default().set_lib_name("varve", env!("CARGO_PKG_VERSION"));
        if let Some(password) = &self.password {
            info = info.set_password(password.expose());
            if let Some(username) = &self.username {
                info = info.set_username(username);
            }
        } else if let Some(username) = &self.username {
            // AUTH needs a password; an ACL user without one ("nopass")
            // still authenticates with any password.
            info = info.set_username(username).set_password("");
        }
        let config = redis::AsyncConnectionConfig::new()
            .set_response_timeout(None)
            .set_connection_timeout(None);
        let budget = Arc::new(ReadBudget::new(MAX_REPLY_BYTES));
        let stream = Limited {
            inner: plain,
            budget: Arc::clone(&budget),
        };
        let (handle, driver) = MultiplexedConnection::new_with_config(&info, stream, config)
            .await
            .map_err(|e| connect_error(&e, &self.display()))?;
        mark_answered();
        let conn = Connection {
            handle,
            budget,
            task: tokio::spawn(driver).abort_handle(),
        };

        if !name.is_empty() {
            let reply = conn
                .send(redis::cmd("CLIENT").arg("SETNAME").arg(name))
                .await
                .map_err(|e| connect_error(&e, &self.display()))?;
            if let Resp::ServerError(e) = reply {
                return Err(error::from_server_error(&e));
            }
        }
        if db != 0 {
            let reply = conn
                .send(redis::cmd("SELECT").arg(db))
                .await
                .map_err(|e| connect_error(&e, &self.display()))?;
            if let Resp::ServerError(e) = reply {
                return Err(error::from_server_error(&e));
            }
        }
        Ok(conn)
    }
}

tokio::task_local! {
    /// How far the connect run by [`Endpoint::bounded_connect`] got.
    static PROGRESS: Arc<ConnectProgress>;
}

/// How far a connect got, for its timeout message.
#[derive(Default)]
struct ConnectProgress {
    /// Something the server sent was read (a probe reply, a TLS
    /// handshake, a reply to AUTH or HELLO).
    answered: AtomicBool,
    /// The plain probe of `prefer` got no answer in its time.
    probe_silent: AtomicBool,
}

/// Records that the server answered, for the connect running in
/// [`Endpoint::bounded_connect`] (outside one it does nothing).
pub(crate) fn mark_answered() {
    let _ = PROGRESS.try_with(|p| p.answered.store(true, Ordering::SeqCst));
}

/// The plain probe of TLS mode `prefer`: `PING` as a RESP array.
const PROBE: &[u8] = b"*1\r\n$4\r\nPING\r\n";

/// Most bytes of the probe reply that are read.
const PROBE_REPLY_MAX: usize = 512;

/// What the plain probe found.
enum Probe {
    /// A plain Redis answered; the reply line was read completely, so the
    /// connection is ready for the normal connect.
    Plain(TcpStream),
    /// A plain Redis answered with an error that may close the connection.
    PlainClosed,
    /// No Redis reply: a TLS server, or no server that speaks Redis.
    NotPlain(Refusal),
}

/// How the server did not answer the plain probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// It closed or reset the connection.
    Closed,
    /// It sent bytes that are not a Redis reply (a TLS alert).
    Foreign,
    /// It sent nothing within the probe's time.
    Silent,
}

impl Refusal {
    fn reason(self) -> &'static str {
        match self {
            Refusal::Closed => "it closed the connection",
            Refusal::Foreign => "it answered with bytes that are not a Redis reply",
            Refusal::Silent => "it did not answer",
        }
    }
}

/// How the probe reply is used.
#[derive(Debug, PartialEq, Eq)]
enum ProbeReply {
    /// Plain Redis; keep the connection.
    Reuse,
    /// Plain Redis; open a new connection.
    PlainClosed,
    /// Not a plain Redis.
    NotPlain(Refusal),
}

/// Classifies the bytes read after the probe. A plain Redis answers `PING`
/// with `+PONG`, or with `-NOAUTH` when it requires a password; both leave
/// the connection usable. Other errors (`-DENIED` in protected mode,
/// `-LOADING`) may close it. A TLS server answers with an alert (first
/// byte 0x15) or closes.
fn classify_probe_reply(line: &[u8]) -> ProbeReply {
    let Some(first) = line.first() else {
        return ProbeReply::NotPlain(Refusal::Closed);
    };
    if *first != b'+' && *first != b'-' {
        return ProbeReply::NotPlain(Refusal::Foreign);
    }
    // Exactly one complete line: nothing else waits to be read.
    let one_line =
        line.ends_with(b"\r\n") && line.windows(2).filter(|w| *w == b"\r\n").count() == 1;
    if one_line && (*first == b'+' || line.starts_with(b"-NOAUTH")) {
        ProbeReply::Reuse
    } else {
        ProbeReply::PlainClosed
    }
}

/// A failed TLS handshake.
enum Handshake {
    /// No answer within half of the connect timeout.
    Silent,
    /// The server closed the connection.
    Closed(io::Error),
    /// Any other failure (an alert, a certificate the checks refused).
    Failed(io::Error),
}

impl Handshake {
    fn message(&self, endpoint: &str) -> String {
        match self {
            Handshake::Silent => format!(
                "{endpoint} did not answer the TLS handshake; is TLS enabled on the server?"
            ),
            Handshake::Closed(e) | Handshake::Failed(e) => {
                format!("handshake with {endpoint} failed: {e}")
            }
        }
    }
}

/// An open connection. Dropping it stops the connection task, which closes
/// the socket at once, even while a reply is still outstanding; clones of
/// [`Connection::handle`] fail from then on.
pub struct Connection {
    handle: MultiplexedConnection,
    budget: Arc<ReadBudget>,
    task: AbortHandle,
}

impl Connection {
    /// A handle for sending commands.
    pub fn handle(&self) -> MultiplexedConnection {
        self.handle.clone()
    }

    /// The read budget of this connection.
    pub fn budget(&self) -> Arc<ReadBudget> {
        Arc::clone(&self.budget)
    }

    /// Sends one command with a fresh read budget and returns the raw reply.
    async fn send(&self, cmd: &redis::Cmd) -> redis::RedisResult<Resp> {
        self.budget.reset(MAX_REPLY_BYTES);
        self.handle.clone().send_packed_command(cmd).await
    }

    /// Runs one command and maps a server error.
    ///
    /// # Errors
    ///
    /// The mapped driver error; a reply above [`MAX_REPLY_BYTES`] gives
    /// [`error::reply_too_large`] and leaves the connection unusable.
    pub async fn query(&self, cmd: &redis::Cmd) -> Result<Resp, DriverError> {
        match self.send(cmd).await {
            Ok(Resp::ServerError(e)) => Err(error::from_server_error(&e)),
            Ok(reply) => Ok(reply),
            Err(_) if self.budget.exceeded() => Err(error::reply_too_large()),
            Err(e) => Err(error::from_redis(&e)),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How many more bytes the connection may read for the current request.
/// The session resets it before every request; a reply that goes over it
/// fails the read, so the protocol library never buffers more.
pub struct ReadBudget {
    remaining: AtomicU64,
    exceeded: AtomicBool,
}

impl ReadBudget {
    fn new(limit: u64) -> Self {
        Self {
            remaining: AtomicU64::new(limit),
            exceeded: AtomicBool::new(false),
        }
    }

    /// Allows `limit` more bytes for the next request.
    pub fn reset(&self, limit: u64) {
        self.remaining.store(limit, Ordering::SeqCst);
        self.exceeded.store(false, Ordering::SeqCst);
    }

    /// True when a read went over the budget since the last reset.
    pub fn exceeded(&self) -> bool {
        self.exceeded.load(Ordering::SeqCst)
    }

    /// Takes `bytes` from the budget; false (and marked exceeded) when they
    /// do not fit.
    fn take(&self, bytes: u64) -> bool {
        let fits = self
            .remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(bytes)
            })
            .is_ok();
        if !fits {
            self.exceeded.store(true, Ordering::SeqCst);
        }
        fits
    }
}

/// A byte stream to the server. `Sync` too, so a call that holds one (the
/// key browser's side connection) can be borrowed across an await in a
/// `Send` future.
pub(crate) trait Stream: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Stream for T {}

/// A stream whose reads are charged to a [`ReadBudget`].
struct Limited<S> {
    inner: S,
    budget: Arc<ReadBudget>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Limited<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &polled {
            let read = (buf.filled().len() - before) as u64;
            if read > 0 && !self.budget.take(read) {
                return Poll::Ready(Err(io::Error::other(format!(
                    "the reply is larger than {MAX_REPLY_BYTES} bytes"
                ))));
            }
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Limited<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

fn connect_error(e: &redis::RedisError, endpoint: &str) -> DriverError {
    let mut mapped = error::from_redis(e);
    if mapped.kind == ErrorKind::ConnectionLost {
        mapped.kind = ErrorKind::Connect;
        mapped.message = format!("Could not connect to {endpoint}: {e}");
    }
    mapped
}

/// A client name Redis accepts: printable ASCII without spaces. Other
/// characters become `-`.
pub fn client_name(application_name: &str) -> String {
    application_name
        .chars()
        .map(|c| if c.is_ascii_graphic() { c } else { '-' })
        .collect()
}

/// Which server family answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerFlavor {
    /// Redis.
    Redis,
    /// Valkey (reports a Redis compatible version too).
    Valkey,
}

/// Parsed `INFO server`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDetails {
    /// Identity for the session.
    pub info: ServerInfo,
    /// Redis or Valkey.
    pub flavor: ServerFlavor,
    /// The Redis version the server is compatible with (`redis_version`),
    /// used to gate features.
    pub compat: (u32, u32),
}

/// Parses `INFO` text into `key -> value` pairs, skipping section headers.
pub fn info_fields(text: &str) -> Vec<(&str, &str)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once(':'))
        .collect()
}

/// Reads server identity from `INFO server` text.
///
/// # Errors
///
/// [`ErrorKind::Protocol`] when no version is reported.
pub fn server_details(text: &str) -> Result<ServerDetails, DriverError> {
    let fields = info_fields(text);
    let field = |key: &str| fields.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
    let redis_version = field("redis_version")
        .ok_or_else(|| error::protocol("INFO server did not report redis_version"))?;
    let valkey_version = field("valkey_version").or_else(|| {
        field("server_name")
            .filter(|n| n.eq_ignore_ascii_case("valkey"))
            .map(|_| redis_version)
    });
    let compat = parse_version(redis_version)
        .map(|(major, minor, _)| (major, minor))
        .ok_or_else(|| error::protocol(format!("unreadable redis_version {redis_version}")))?;
    let (flavor, version, shown) = match valkey_version {
        Some(v) => (ServerFlavor::Valkey, v, format!("{v}-valkey")),
        None => (
            ServerFlavor::Redis,
            redis_version,
            redis_version.to_string(),
        ),
    };
    let (major, minor, patch) = parse_version(version)
        .ok_or_else(|| error::protocol(format!("unreadable server version {version}")))?;
    Ok(ServerDetails {
        info: ServerInfo {
            engine: Engine::Redis,
            version: shown,
            major,
            minor,
            patch,
        },
        flavor,
        compat,
    })
}

/// Reads `CLIENT ID`.
///
/// # Errors
///
/// Driver errors, or Protocol when the reply is not an integer.
pub async fn client_id(conn: &Connection) -> Result<i64, DriverError> {
    let reply = conn.query(redis::cmd("CLIENT").arg("ID")).await?;
    as_int(&reply).ok_or_else(|| error::protocol("CLIENT ID did not return an integer"))
}

/// Reads and parses `INFO server`.
///
/// # Errors
///
/// Driver errors, or Protocol for an unexpected reply.
pub async fn read_server(conn: &Connection) -> Result<ServerDetails, DriverError> {
    let reply = conn.query(redis::cmd("INFO").arg("server")).await?;
    let text = as_text(&reply).ok_or_else(|| error::protocol("INFO did not return text"))?;
    server_details(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_replies_tell_plain_redis_from_tls() {
        assert_eq!(classify_probe_reply(b"+PONG\r\n"), ProbeReply::Reuse);
        assert_eq!(
            classify_probe_reply(b"-NOAUTH Authentication required.\r\n"),
            ProbeReply::Reuse
        );
        // Protected mode closes the connection after its error.
        assert_eq!(
            classify_probe_reply(b"-DENIED Redis is running in protected mode\r\n"),
            ProbeReply::PlainClosed
        );
        // An incomplete line or more than one line is not reused.
        assert_eq!(classify_probe_reply(b"+PO"), ProbeReply::PlainClosed);
        assert_eq!(
            classify_probe_reply(b"+PONG\r\n+PONG\r\n"),
            ProbeReply::PlainClosed
        );
        // A TLS alert record, and a closed connection.
        assert_eq!(
            classify_probe_reply(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x46]),
            ProbeReply::NotPlain(Refusal::Foreign)
        );
        assert_eq!(
            classify_probe_reply(b""),
            ProbeReply::NotPlain(Refusal::Closed)
        );
    }

    #[test]
    fn the_probe_is_no_tls_record() {
        // TLS record content types are 20 to 24; the probe's first byte
        // must be none of them, so a TLS server rejects it at once.
        assert!(PROBE.first().is_some_and(|b| !(20..=24).contains(b)));
        // At least a whole TLS record header (5 bytes) arrives at once.
        assert!(PROBE.len() >= 5);
    }

    #[test]
    fn client_names_have_no_spaces() {
        assert_eq!(client_name("Varve"), "Varve");
        assert_eq!(client_name("Varve Query 1"), "Varve-Query-1");
        assert_eq!(client_name("tab\nnaïve"), "tab-na-ve");
    }

    #[test]
    fn reads_redis_versions() -> Result<(), DriverError> {
        let text = "# Server\r\nredis_version:8.10.0\r\nredis_mode:standalone\r\n";
        let details = server_details(text)?;
        assert_eq!(details.flavor, ServerFlavor::Redis);
        assert_eq!(details.info.version, "8.10.0");
        assert_eq!((details.info.major, details.info.minor), (8, 10));
        assert_eq!(details.info.patch, Some(0));
        assert_eq!(details.compat, (8, 10));
        Ok(())
    }

    #[test]
    fn detects_valkey() -> Result<(), DriverError> {
        let text =
            "# Server\r\nredis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:9.0.1\r\n";
        let details = server_details(text)?;
        assert_eq!(details.flavor, ServerFlavor::Valkey);
        assert_eq!(details.info.version, "9.0.1-valkey");
        assert_eq!(details.info.major, 9);
        assert_eq!(details.compat, (7, 2));

        let old = "redis_version:7.2.5\r\nserver_name:valkey\r\n";
        assert_eq!(server_details(old)?.flavor, ServerFlavor::Valkey);
        assert!(server_details("# Server\r\n").is_err());
        Ok(())
    }

    #[test]
    fn read_budget_stops_at_the_limit() {
        let budget = ReadBudget::new(10);
        assert!(budget.take(6));
        assert!(budget.take(4));
        assert!(!budget.exceeded());
        assert!(!budget.take(1));
        assert!(budget.exceeded());
        budget.reset(3);
        assert!(!budget.exceeded());
        assert!(budget.take(3));
    }

    #[tokio::test]
    async fn limited_streams_fail_reads_over_the_budget() -> Result<(), io::Error> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut server, client) = tokio::io::duplex(1024);
        let budget = Arc::new(ReadBudget::new(8));
        let mut limited = Limited {
            inner: client,
            budget: Arc::clone(&budget),
        };
        server.write_all(b"12345").await?;
        let mut buf = [0u8; 64];
        assert_eq!(limited.read(&mut buf).await?, 5);
        server.write_all(b"6789").await?;
        let error = limited.read(&mut buf).await.err();
        assert!(error.is_some(), "a read over the budget must fail");
        assert!(budget.exceeded());
        budget.reset(8);
        server.write_all(b"ok").await?;
        assert_eq!(limited.read(&mut buf).await?, 2);
        Ok(())
    }

    #[tokio::test]
    async fn prefer_remembers_the_transport_that_worked() -> Result<(), Box<dyn std::error::Error>>
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A plain server: answers the probe's PING, never answers a TLS
        // ClientHello (like Redis, it waits for a newline). Counts probes.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let probes = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&probes);
        let held = tokio::spawn(async move {
            let mut sockets = Vec::new();
            while let Ok((mut socket, _)) = listener.accept().await {
                let counted = Arc::clone(&counted);
                sockets.push(tokio::spawn(async move {
                    let mut buffer = [0; 64];
                    if let Ok(n) = socket.read(&mut buffer).await
                        && buffer.get(..n).is_some_and(|b| b == PROBE)
                    {
                        counted.fetch_add(1, Ordering::SeqCst);
                        let _ = socket.write_all(b"+PONG\r\n").await;
                    }
                    std::future::pending::<()>().await;
                }));
            }
        });
        let mut params = ConnectParams::new(Engine::Redis, "127.0.0.1", port);
        params.tls.mode = varve_driver::TlsMode::Prefer;
        params.connect_timeout = Duration::from_millis(800);
        let endpoint = Endpoint::new(&params).await?;
        let started = std::time::Instant::now();
        endpoint.open_stream().await?;
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "the probe waited: {:?}",
            started.elapsed()
        );
        assert_eq!(endpoint.transport(), Some(false));
        assert!(!endpoint.uses_tls());
        // Every later connection, also of a clone (side connections), goes
        // straight to plain TCP without another probe.
        let clone = endpoint.clone();
        for _ in 0..5 {
            clone.open_stream().await?;
        }
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        held.abort();
        Ok(())
    }

    #[tokio::test]
    async fn a_label_names_the_endpoint_for_every_clone() -> Result<(), DriverError> {
        let endpoint =
            Endpoint::new(&ConnectParams::new(Engine::Redis, "127.0.0.1", 50123)).await?;
        let clone = endpoint.clone();
        assert_eq!(clone.display(), "127.0.0.1:50123");
        endpoint.set_label("redis:6379 through SSH bastion bastion.example:22".into());
        assert_eq!(
            clone.display(),
            "redis:6379 through SSH bastion bastion.example:22"
        );
        Ok(())
    }

    #[test]
    fn info_fields_skip_headers() {
        let fields = info_fields("# Keyspace\r\ndb0:keys=3,expires=1\r\n\r\n");
        assert_eq!(fields, vec![("db0", "keys=3,expires=1")]);
    }
}
