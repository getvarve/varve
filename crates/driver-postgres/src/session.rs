//! Opening a connection, running scripts and cancelling.

use std::any::Any;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_postgres::config::SslMode;
use tokio_postgres::error::SqlState;
use tokio_postgres::{AsyncMessage, CancelToken, Client, SimpleQueryMessage};
use varve_core::Engine;
use varve_driver::{
    CancelHandle, Column, ConnectParams, Dialect, DriverError, Durability, ErrorKind, ExecOptions,
    ExecOutput, Notice, PartialOutput, ServerInfo, Session, StatementKind, StatementResult,
    TransactionEffect, TransactionTracker, Value, parse_version, read_only_violation,
    split_statements, transaction_effect,
};

use crate::convert::{
    ColumnType, Dependence, TypeCache, ValueKind, builtin_dependence, convert, dependence,
};
use crate::error::{
    ConnectContext, Interrupt, QUERY_CANCELED, cancel_cause_of, connect_error, decide_timeout,
    exec_error, is_server_interrupt, redact, socket_error, timeout_unknown,
};
use crate::guard::{self, Expected, Outcome, Scope};
use crate::tap::{NoticeLog, Shared, TapStream};
use crate::tls::{RawStream, TlsSetup};

/// How long [`Session::close`] waits for the server to see the connection end.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for the server's `ReadyForQuery` after an error.
const READY_WAIT: Duration = Duration::from_secs(5);

/// Type name shown for columns whose type is unknown (statements that could
/// not be prepared).
const UNKNOWN_TYPE: &str = "unknown";

/// Longest statement text quoted in an error message, in characters.
const MAX_QUOTED_STATEMENT: usize = 200;

/// An open PostgreSQL connection.
///
/// Calls are serialized: a second [`Session::execute`] waits until the first
/// one has finished.
pub struct PostgresSession {
    info: ServerInfo,
    inner: Mutex<Inner>,
    shared: Arc<Shared>,
    cancel: Arc<PostgresCancelHandle>,
    /// Opened with `read_only`: scripts that would switch it off are
    /// rejected and the mode is checked after every statement.
    read_only: bool,
    /// The state [`guard`] keeps the session in.
    expected: Expected,
    /// What the session was opened with, to open it again after
    /// `DISCARD ALL`.
    params: ConnectParams,
    /// The transaction status callers see ([`Session::in_transaction`]):
    /// the tap's last `ReadyForQuery` status, published when a call ends,
    /// so the driver's own BEGIN/COMMIT around a CALL or DO and its checks
    /// are never visible half way. 0 means unknown.
    visible_status: AtomicU8,
    /// The server notices the first connection's setup (init SQL) sent.
    connect_notices: Vec<Notice>,
}

/// A backend connection, set up and ready for statements.
pub(crate) struct Opened {
    pub(crate) client: Client,
    pub(crate) task: Option<JoinHandle<()>>,
    pub(crate) info: ServerInfo,
    tls: TlsSetup,
    /// Warnings from checking the session after the init SQL.
    warnings: Vec<String>,
    /// The check after the init SQL could not run (it left a transaction
    /// open): it is owed, with this scope.
    owed: Option<Scope>,
    /// The session's `statement_timeout` after the init SQL, in ms.
    timeout_ms: Option<u128>,
    /// The session's `lc_messages` after the init SQL.
    lc_messages: String,
    /// The connection's number ([`Shared::begin_connection`]), to put it in
    /// use ([`Shared::use_connection`]) once it replaces the previous one.
    pub(crate) number: u64,
}

/// Opens and sets up a connection (session settings, init SQL), all within
/// the connect timeout. Notices land in `shared`.
pub(crate) async fn open(
    params: &ConnectParams,
    shared: &Arc<Shared>,
) -> Result<Opened, DriverError> {
    if params.engine != Engine::Postgres {
        return Err(DriverError::new(
            ErrorKind::Unsupported,
            format!("the PostgreSQL driver cannot connect to {}", params.engine),
        ));
    }
    check_statement_timeout(params.statement_timeout)?;
    let tls = TlsSetup::new(&params.tls, &params.host, params.connect_timeout).await?;
    let password = params.password.as_ref().map(|p| p.expose());

    let mut config = tokio_postgres::Config::new();
    config
        .dbname(params.database.as_deref().unwrap_or("postgres"))
        .application_name(&params.application_name)
        .ssl_mode(tls.ssl_mode);
    if let Some(user) = &params.username {
        config.user(user);
    }
    if let Some(password) = password {
        config.password(password);
    }

    if params.read_only
        && let Some(init_sql) = params.init_sql.as_deref()
        && let Some(statement) = read_only_violation(init_sql, Dialect::Postgres)
    {
        return Err(DriverError::new(
            ErrorKind::ReadOnly,
            format!(
                "the connection is read-only and its init SQL statement \"{}\" would \
                 switch read-only mode off",
                quote_statement(statement)
            ),
        ));
    }
    let expected = expected_state(params);

    // The timeout covers everything up to a usable session: the socket,
    // TLS, authentication, the session settings and the init SQL.
    let setup = async {
        let connection_number = shared.begin_connection();
        let socket = open_socket(&params.host, params.port).await?;
        let stream = wrap_socket(socket, tls.ssl_mode, Arc::clone(shared));
        let connector = tls.connector(Arc::clone(shared));
        let (client, connection) = config.connect_raw(stream, connector).await.map_err(|e| {
            connect_error(
                &e,
                ConnectContext {
                    tls_required: matches!(tls.ssl_mode, SslMode::Require),
                    ssl_response: shared.ssl_response(),
                    password,
                },
            )
        })?;
        let task = AbortOnDrop(Some(spawn_connection(
            connection,
            Arc::clone(shared),
            connection_number,
        )));
        client
            .batch_execute(&session_setup_sql(params))
            .await
            .map_err(|e| exec_error(&e, 0, Interrupt::NONE))?;
        let version_num = show(&client, "server_version_num").await?;
        let version = show(&client, "server_version").await?;
        let info = server_info(&version_num, &version)?;
        let mut warnings = Vec::new();
        let mut owed = None;
        // The effective timeout: the profile's, or one from ALTER ROLE or
        // ALTER DATABASE when the profile sets none.
        let mut timeout_ms = parse_duration_ms(&show(&client, "statement_timeout").await?)
            .unwrap_or(expected.statement_timeout_ms);
        let mut lc_messages = show(&client, "lc_messages").await?;
        if let Some(init_sql) = params.init_sql.as_deref().filter(|s| !s.trim().is_empty()) {
            // A RESET in the init SQL brings the profile's timeout back; a
            // SET of its own is kept.
            let init_scope = split_statements(init_sql, Dialect::Postgres)
                .iter()
                .map(|span| Scope::for_statement(span.kind, &words_outside_literals(span.text)))
                .fold(Scope::SETTINGS, Scope::merge);
            let started = Instant::now();
            client.batch_execute(init_sql).await.map_err(|error| {
                let interrupt =
                    Interrupt::for_statement(false, started, expected.statement_timeout_ms);
                let mut mapped = exec_error(&error, 0, interrupt);
                mapped.message = format!("init SQL failed: {}", mapped.message);
                mapped
            })?;
            let restored = guard::ensure(&client, shared, expected, init_scope, &[])
                .await
                .map_err(|mut e| {
                    e.message = format!(
                        "the init SQL changed the session and Varve could not set it \
                         back: {}",
                        e.message
                    );
                    e
                })?;
            match restored {
                Outcome::Checked(restored) => {
                    warnings = restored.warnings("the init SQL");
                    if let Some(observed) = restored.statement_timeout_ms {
                        timeout_ms = observed;
                    }
                    if let Some(observed) = restored.lc_messages {
                        lc_messages = observed;
                    }
                }
                // The init SQL left a transaction open (or a cancel hit the
                // check): the check is owed until the backend is idle.
                Outcome::Skipped => owed = Some(init_scope),
            }
        }
        Ok((
            client,
            task.disarm(),
            info,
            warnings,
            owed,
            timeout_ms,
            lc_messages,
            connection_number,
        ))
    };
    let (client, task, info, warnings, owed, timeout_ms, lc_messages, number) =
        tokio::time::timeout(params.connect_timeout, setup)
            .await
            .map_err(|_| timeout_error("connecting", params.connect_timeout))?
            .map_err(|mut e: DriverError| {
                e.message = redact(e.message, password);
                e
            })?;
    Ok(Opened {
        client,
        task,
        info,
        tls,
        warnings,
        owed,
        timeout_ms,
        lc_messages,
        number,
    })
}

/// Largest `statement_timeout` PostgreSQL accepts, in ms (`INT_MAX`).
const MAX_STATEMENT_TIMEOUT_MS: u128 = i32::MAX as u128;

/// Refuses a profile `statement_timeout` the server cannot take: one under
/// a millisecond would be sent as 0, which switches the timeout off, and
/// one over `INT_MAX` ms (about 24.8 days) fails with "invalid value".
fn check_statement_timeout(timeout: Option<Duration>) -> Result<(), DriverError> {
    let Some(timeout) = timeout else {
        return Ok(());
    };
    let ms = timeout.as_millis();
    if (1..=MAX_STATEMENT_TIMEOUT_MS).contains(&ms) {
        return Ok(());
    }
    let set = if ms == 0 {
        format!("{} microseconds", timeout.as_micros())
    } else {
        format!("{ms} ms")
    };
    Err(DriverError::new(
        ErrorKind::Unsupported,
        format!(
            "the statement timeout must be between 1 ms and {MAX_STATEMENT_TIMEOUT_MS} ms \
             (about 24.8 days) for PostgreSQL; the connection sets {set}"
        ),
    ))
}

fn expected_state(params: &ConnectParams) -> Expected {
    Expected {
        read_only: params.read_only,
        statement_timeout_ms: params.statement_timeout.map(|t| t.as_millis()),
    }
}

/// Ends a replaced connection in the background: dropping the client makes
/// the task send Terminate; a task that does not finish is aborted.
pub(crate) fn close_in_background(client: Client, task: Option<JoinHandle<()>>) {
    drop(client);
    if let Some(task) = task {
        let abort = task.abort_handle();
        tokio::spawn(async move {
            if tokio::time::timeout(CLOSE_TIMEOUT, task).await.is_err() {
                abort.abort();
            }
        });
    }
}

struct Inner {
    client: Client,
    types: TypeCache,
    connection: Option<JoinHandle<()>>,
    /// Set when the session can no longer be used safely (read-only mode
    /// could not be restored); every later call fails with this message.
    lost: Option<String>,
    /// The session's `statement_timeout` in ms, as last read from the
    /// server (the profile's, or one the user set), to tell a timeout from
    /// a cancel when nothing better is known.
    timeout_ms: Option<u128>,
    /// Whether the server's messages are translated.
    messages: MessageLanguage,
    /// When the last message to the server for a user's statement (its
    /// prepare, or the statement itself) was sent: the server's statement
    /// timer starts with it.
    phase_started: Instant,
    /// A call is running, or was given up half way (its future dropped):
    /// the next call then finds this set and first brings the session back
    /// to a known state ([`PostgresSession::recover_abandoned`]).
    call_open: bool,
    /// The driver's own `BEGIN READ ONLY` around a CALL or DO was sent and
    /// its block not yet ended.
    read_only_block_open: bool,
    /// Driver warnings from connecting, shown with the first execution.
    pending_warnings: Vec<String>,
    /// What the guard still has to check once the backend is idle.
    guard: GuardState,
}

/// What happened since the guard last checked the session.
#[derive(Debug, Default)]
struct GuardState {
    /// Checks owed by statements that ran (inside a transaction block, or
    /// skipped because a cancel landed on the check).
    pending: Scope,
    /// Something since the unreported settings were last queried could have
    /// changed them (a risky statement, a function call, a reported change).
    maybe_changed: bool,
    /// Read-only session: a statement ran since read-only mode was last
    /// confirmed. No further statement runs until it is.
    ro_owed: bool,
    /// The session must reconnect as soon as the backend is idle, with this
    /// notice: tokio-postgres's type lookups were lost inside a
    /// transaction, a `DISCARD ALL` was sent (owed from before it is sent,
    /// so a call given up after the server ran it still reopens the session
    /// and runs the init SQL again), or a reconnect was given up half way.
    reconnect_owed: Option<&'static str>,
    /// A statement has executed since the unreported settings were last
    /// queried (this run or an earlier one). Any statement can run user
    /// code (a view, a trigger, a domain check, a cast) that changes them.
    executed_since_check: bool,
    /// Float or bytea values were read inside a transaction while
    /// `executed_since_check` was set, when no check could run.
    unchecked_reads: bool,
}

impl GuardState {
    /// The state after a connect whose init SQL check was `owed`.
    fn after_connect(owed: Option<Scope>, read_only: bool) -> Self {
        if let Some(scope) = owed {
            Self {
                pending: scope,
                maybe_changed: true,
                ro_owed: read_only,
                reconnect_owed: None,
                executed_since_check: true,
                unchecked_reads: false,
            }
        } else {
            Self::default()
        }
    }
}

/// Whether the server sends its messages untranslated, which decides how a
/// `57014` is told apart: by its message (untranslated), or by comparing the
/// time it ran with the `statement_timeout` in force (translated).
#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageLanguage {
    /// The session's `lc_messages`, as last read.
    lc_messages: String,
    /// Learned from a `57014` while `lc_messages` had this value: whether
    /// its message was the untranslated one. A server built without
    /// translations sends English whatever `lc_messages` says.
    untranslated: Option<bool>,
}

impl MessageLanguage {
    fn new(lc_messages: String) -> Self {
        Self {
            lc_messages,
            untranslated: None,
        }
    }

    /// True when messages may be translated: `lc_messages` names a
    /// language other than English (or is empty: the server's environment
    /// decides), unless a message showed otherwise.
    fn translated(&self) -> bool {
        match self.untranslated {
            Some(untranslated) => !untranslated,
            None => !is_english_locale(&self.lc_messages),
        }
    }

    /// Records the `lc_messages` read from the server.
    fn observe(&mut self, lc_messages: String) {
        if lc_messages != self.lc_messages {
            *self = Self::new(lc_messages);
        }
    }

    /// Learns from a cancel or timeout the server reported whether
    /// messages are translated. A `57014` that user code raised (with any
    /// message it likes) teaches nothing.
    fn learn(&mut self, error: &DriverError) {
        if is_server_interrupt(error) {
            self.untranslated = Some(cancel_cause_of(error).is_some());
        }
    }
}

/// True when the locale `name` gives untranslated (English) messages: `C`,
/// `POSIX` and English locales.
fn is_english_locale(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    let language = name.split(['_', '.', '@', '-']).next().unwrap_or_default();
    matches!(language, "c" | "posix" | "en" | "english")
}

/// Aborts the connection task when dropped, unless it was taken back: a
/// connect that fails or times out half way must not leave it running.
struct AbortOnDrop(Option<JoinHandle<()>>);

impl AbortOnDrop {
    fn disarm(mut self) -> Option<JoinHandle<()>> {
        self.0.take()
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl std::fmt::Debug for PostgresSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresSession")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

async fn open_socket(host: &str, port: u16) -> Result<TcpStream, DriverError> {
    if host.trim().is_empty() {
        return Err(DriverError::new(ErrorKind::Connect, "no host is set"));
    }
    let socket = TcpStream::connect((host, port))
        .await
        .map_err(|e| socket_error(host, port, &e))?;
    // Latency matters more than throughput for interactive queries.
    socket
        .set_nodelay(true)
        .map_err(|e| socket_error(host, port, &e))?;
    Ok(socket)
}

fn wrap_socket(socket: TcpStream, ssl_mode: SslMode, shared: Arc<Shared>) -> RawStream {
    if matches!(ssl_mode, SslMode::Disable) {
        TapStream::parsing(socket, shared)
    } else {
        TapStream::expecting_ssl_response(socket, shared)
    }
}

fn timeout_error(what: &str, after: Duration) -> DriverError {
    DriverError::new(
        ErrorKind::Timeout,
        format!("{what} timed out after {} ms", after.as_millis()),
    )
}

/// Reads the first column of the first row of a simple query.
async fn show(client: &Client, setting: &str) -> Result<String, DriverError> {
    let messages = client
        .simple_query(&format!("SHOW {setting}"))
        .await
        .map_err(|e| exec_error(&e, 0, Interrupt::NONE))?;
    messages
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
            _ => None,
        })
        .ok_or_else(|| {
            DriverError::new(
                ErrorKind::Protocol,
                format!("SHOW {setting} returned no row"),
            )
        })
}

fn server_info(version_num: &str, version: &str) -> Result<ServerInfo, DriverError> {
    let (major, minor, patch) = match version_num.trim().parse::<u32>() {
        Ok(num) if num >= 100_000 => (num / 10_000, num % 10_000, None),
        Ok(num) => (num / 10_000, num / 100 % 100, Some(num % 100)),
        Err(_) => parse_version(version).ok_or_else(|| {
            DriverError::new(
                ErrorKind::Protocol,
                format!("unrecognised server version \"{version}\""),
            )
        })?,
    };
    Ok(ServerInfo {
        engine: Engine::Postgres,
        version: version.trim().to_owned(),
        major,
        minor,
        patch,
    })
}

/// Session settings applied right after connecting.
fn session_setup_sql(params: &ConnectParams) -> String {
    let mut sql = guard::setup_sql();
    if params.read_only {
        sql.push_str("; SET default_transaction_read_only = on");
    }
    if let Some(timeout) = params.statement_timeout {
        sql.push_str(&format!(
            "; SET statement_timeout = {}",
            timeout.as_millis()
        ));
    }
    sql
}

/// Forwards notices from the connection to `shared` until it closes.
/// `number` is the connection's number from [`Shared::begin_connection`];
/// its end is recorded, so callers stop waiting for messages that cannot
/// come.
fn spawn_connection<S, T>(
    mut connection: tokio_postgres::Connection<S, T>,
    shared: Arc<Shared>,
    number: u64,
) -> JoinHandle<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut messages = futures::stream::poll_fn(move |cx| connection.poll_message(cx));
        while let Some(message) = messages.next().await {
            match message {
                Ok(AsyncMessage::Notice(notice)) => shared.push_notice(Notice {
                    severity: notice
                        .parsed_severity()
                        .map_or_else(|| notice.severity().to_owned(), |s| s.to_string()),
                    message: notice.message().to_owned(),
                }),
                // Only listener connections (crate::listen) have a sink.
                Ok(AsyncMessage::Notification(notification)) => {
                    shared.push_notification(notification);
                }
                Ok(_) => {}
                Err(error) => {
                    // Only the SQLSTATE: server messages can echo input.
                    tracing::debug!(
                        sqlstate = error.code().map(SqlState::code),
                        "PostgreSQL connection ended with an error"
                    );
                    break;
                }
            }
        }
        shared.end_connection(number);
    })
}

impl PostgresSession {
    pub(crate) async fn connect(params: &ConnectParams) -> Result<Self, DriverError> {
        let shared = Arc::new(Shared::default());
        let opened = open(params, &shared).await?;
        shared.use_connection(opened.number);
        let status = shared.transaction_status().unwrap_or(0);
        // The init SQL's notices are no call's, but privilege logging
        // reads them (a GRANT the server says granted nothing).
        let connect_notices = shared.take_server_notices();
        shared.clear_notices();
        let cancel = Arc::new(PostgresCancelHandle {
            token: StdMutex::new(opened.client.cancel_token()),
            host: params.host.clone(),
            port: params.port,
            connect_timeout: params.connect_timeout,
            tls: opened.tls,
            shared: Arc::clone(&shared),
        });
        Ok(Self {
            info: opened.info,
            inner: Mutex::new(Inner {
                client: opened.client,
                types: TypeCache::default(),
                connection: opened.task,
                lost: None,
                pending_warnings: opened.warnings,
                guard: GuardState::after_connect(opened.owed, params.read_only),
                timeout_ms: opened.timeout_ms,
                messages: MessageLanguage::new(opened.lc_messages),
                phase_started: Instant::now(),
                call_open: false,
                read_only_block_open: false,
            }),
            shared,
            cancel,
            read_only: params.read_only,
            expected: expected_state(params),
            params: params.clone(),
            visible_status: AtomicU8::new(status),
            connect_notices,
        })
    }

    /// Replaces the connection with a new one, set up like the first, after
    /// `DISCARD ALL` wiped what tokio-postgres keeps on the server (its type
    /// lookup statements). The cancel handle follows the new backend.
    /// When that fails the session is lost.
    async fn reconnect(&self, inner: &mut Inner, notice: &'static str) -> Result<(), DriverError> {
        // Owed until the new connection is in place: a call given up half
        // way leaves it owed for the next one.
        inner.guard.reconnect_owed = Some(notice);
        // The new connection's own notices (init SQL) are not this run's.
        let kept = self.shared.swap_notices(NoticeLog::default());
        // The old connection's statements go with it.
        self.shared.reset_statements();
        // Unknown until the new connection's first reply.
        self.shared.reset_transaction_status();
        let opened = open(&self.params, &self.shared).await;
        self.shared.swap_notices(kept);
        match opened {
            Ok(opened) => {
                self.shared.use_connection(opened.number);
                let old_client = std::mem::replace(&mut inner.client, opened.client);
                let old_task = std::mem::replace(&mut inner.connection, opened.task);
                inner.types = TypeCache::default();
                inner.guard = GuardState::after_connect(opened.owed, self.read_only);
                inner.timeout_ms = opened.timeout_ms;
                inner.messages.observe(opened.lc_messages);
                self.cancel.set_token(inner.client.cancel_token());
                close_in_background(old_client, old_task);
                self.shared.push_driver_notice("NOTICE", notice.to_owned());
                for warning in opened.warnings {
                    self.shared.push_driver_warning(warning);
                }
                Ok(())
            }
            Err(error) => {
                let message = format!(
                    "Varve could not open the session again, so the connection was closed: {}",
                    error.message
                );
                inner.lost = Some(message.clone());
                if let Some(task) = &inner.connection {
                    task.abort();
                }
                Err(DriverError::new(ErrorKind::ConnectionLost, message))
            }
        }
    }

    /// Ends the read-only transaction block a CALL or DO ran in: COMMIT
    /// after success, ROLLBACK after a failure (the statement's error stays
    /// the one returned). Either way the session is left idle, or closed.
    async fn end_read_only_block(
        &self,
        inner: &mut Inner,
        outcome: Result<(), DriverError>,
        offset_chars: usize,
    ) -> Result<(), DriverError> {
        let outcome = match outcome {
            Ok(()) => {
                let started = Instant::now();
                match inner.client.batch_execute("COMMIT").await {
                    Ok(()) => Ok(()),
                    Err(error) => Err(exec_error(
                        &error,
                        offset_chars,
                        self.interrupt(started, inner.timeout_ms),
                    )),
                }
            }
            Err(mut error) => {
                if error.code.as_deref() == Some("2D000") && error.hint.is_none() {
                    error.hint = Some(
                        "a read-only connection runs CALL and DO inside a read-only \
                         transaction, so they cannot COMMIT or ROLLBACK"
                            .into(),
                    );
                }
                Err(error)
            }
        };
        match outcome {
            Ok(()) if self.shared.transaction_status() == Some(b'I') => Ok(()),
            Ok(()) => Err(self
                .leave_block(
                    inner,
                    DriverError::new(ErrorKind::Protocol, "COMMIT left the transaction open"),
                )
                .await),
            Err(error) => Err(self.leave_block(inner, error).await),
        }
    }

    /// After `error` inside the read-only block of a CALL or DO, makes sure
    /// the backend is out of the transaction block: ROLLBACK, again after any
    /// cancel in flight (a cancel can hit the ROLLBACK itself), a few times.
    /// A session that stays in a transaction is closed instead of being left
    /// aborted, where every later statement would fail. Returns the error to
    /// report: `error`, or the closing.
    async fn leave_block(&self, inner: &mut Inner, error: DriverError) -> DriverError {
        if error.kind == ErrorKind::ConnectionLost {
            return error;
        }
        for _ in 0..ROLLBACK_ATTEMPTS {
            if !self.shared.wait_until_ready(READY_WAIT).await {
                break;
            }
            if self.shared.transaction_status() == Some(b'I') {
                return error;
            }
            self.shared.wait_for_cancels().await;
            if let Err(rollback) = inner.client.batch_execute("ROLLBACK").await
                && (rollback.is_closed() || rollback.code().is_none())
            {
                break;
            }
        }
        if self.shared.wait_until_ready(READY_WAIT).await
            && self.shared.transaction_status() == Some(b'I')
        {
            return error;
        }
        let message = format!(
            "Varve could not end the read-only transaction around a CALL or DO, so the \
             connection was closed: {}",
            error.message
        );
        inner.lost = Some(message.clone());
        if let Some(task) = &inner.connection {
            task.abort();
        }
        DriverError::new(ErrorKind::ConnectionLost, message)
    }

    /// True when values of `columns` depend on an unreported setting.
    /// Multirange types tokio-postgres cannot describe are looked up in
    /// `pg_range` when the backend is idle (once per type); otherwise, or
    /// when the lookup fails, they count as dependent.
    async fn reads_unreported(&self, inner: &mut Inner, columns: Option<&[ColumnType]>) -> bool {
        let Some(columns) = columns else {
            return false;
        };
        let major = self.info.major;
        let mut unknown = Vec::new();
        for column in columns {
            match dependence(&column.ty, &inner.types.multiranges, major) {
                Dependence::Yes => return true,
                Dependence::No => {}
                Dependence::Unknown(oids) => unknown.extend(oids),
            }
        }
        if unknown.is_empty() {
            return false;
        }
        if self.shared.transaction_status() != Some(b'I') {
            return true;
        }
        unknown.sort_unstable();
        unknown.dedup();
        let list: Vec<String> = unknown.iter().map(u32::to_string).collect();
        // Every name qualified: a user's search_path may put objects in
        // front of pg_catalog.
        let sql = format!(
            "SELECT rngmultitypid::pg_catalog.int8, rngsubtype::pg_catalog.int8 \
             FROM pg_catalog.pg_range \
             WHERE rngmultitypid OPERATOR(pg_catalog.=) ANY ('{{{}}}'::pg_catalog.oid[])",
            list.join(",")
        );
        self.shared.wait_for_cancels().await;
        let Ok(messages) = inner.client.simple_query(&sql).await else {
            return true;
        };
        let mut found = std::collections::HashMap::new();
        for message in &messages {
            if let SimpleQueryMessage::Row(row) = message
                && let (Some(multi), Some(sub)) = (
                    row.get(0).and_then(|v| v.parse::<u32>().ok()),
                    row.get(1).and_then(|v| v.parse::<u32>().ok()),
                )
            {
                // A user element type (a domain over float8, say) is not
                // looked into: it counts as dependent.
                found.insert(multi, builtin_dependence(sub).unwrap_or(true));
            }
        }
        for oid in unknown {
            // Not a multirange: a plain user type (citext, hstore).
            let depends = found.get(&oid).copied().unwrap_or(false);
            inner.types.multiranges.insert(oid, depends);
        }
        columns
            .iter()
            .any(|c| dependence(&c.ty, &inner.types.multiranges, major).likely())
    }

    /// True when the connection is gone: the server closed it (a FATAL
    /// error, `pg_terminate_backend`, a restart), the socket broke, or the
    /// session gave it up. Nothing more can run on it.
    fn connection_gone(&self, inner: &Inner) -> bool {
        inner.lost.is_some() || inner.client.is_closed() || self.shared.connection_ended()
    }

    /// Makes the transaction status of the last reply visible to callers.
    /// After an error it waits for the server's `ReadyForQuery` first, which
    /// tokio-postgres hands over after the error. A lost session, a closed
    /// connection (whose `ReadyForQuery` never comes: no waiting), or one
    /// whose status is not known, shows none.
    async fn publish_status(&self, inner: &Inner, failure: Option<&DriverError>) {
        let known = !failure.is_some_and(|e| e.kind == ErrorKind::ConnectionLost)
            && !self.connection_gone(inner)
            && (failure.is_none() || self.shared.wait_until_ready(READY_WAIT).await)
            && !self.connection_gone(inner);
        let status = if known {
            self.shared.transaction_status().unwrap_or(0)
        } else {
            0
        };
        self.visible_status.store(status, AtomicOrdering::Release);
    }

    /// What a `57014` for a statement started at `started` means.
    fn interrupt(&self, started: Instant, timeout_ms: Option<u128>) -> Interrupt {
        Interrupt::for_statement(self.shared.cancel_requested(), started, timeout_ms)
    }

    /// Prepares `text` to learn its column types (`None` when it cannot be
    /// prepared outside a transaction block: it is then run anyway).
    async fn prepare_statement(
        &self,
        inner: &mut Inner,
        text: &str,
        offset_chars: usize,
    ) -> Result<Prepared, DriverError> {
        let started = Instant::now();
        inner.phase_started = started;
        let timeout_ms = inner.timeout_ms;
        let map = |e: &tokio_postgres::Error| {
            exec_error(e, offset_chars, self.interrupt(started, timeout_ms))
        };
        // A cancel request that reaches an idle backend is ignored by the
        // server, so between round trips only this flag stops the script.
        if self.shared.cancel_requested() {
            return Err(cancelled_before(offset_chars));
        }
        // Prepare to learn the column types; the statement is closed again
        // when `prepared` is dropped at the end of this block.
        let typed: Option<Vec<ColumnType>> = match inner.client.prepare(text).await {
            Ok(prepared) => Some(
                prepared
                    .columns()
                    .iter()
                    .map(|c| inner.types.resolve(c.type_()))
                    .collect(),
            ),
            // A closed connection, or a cancel or timeout that hit the
            // prepare step, ends the statement: running it anyway would
            // ignore what the user or the server asked for.
            Err(error)
                if error.is_closed()
                    || error.code().map(SqlState::code) == Some(QUERY_CANCELED) =>
            {
                return Err(map(&error));
            }
            // Preparing a statement refers to no named statement except
            // tokio-postgres's own type lookups, unless the user's SQL does
            // (`EXECUTE nosuch`): the error names the statement.
            Err(error)
                if error.code() == Some(&SqlState::INVALID_SQL_STATEMENT_NAME)
                    && names_one_of(
                        error.as_db_error().map_or("", |db| db.message()),
                        &self.shared.typeinfo_statements(),
                    ) =>
            {
                self.shared.wait_until_ready(READY_WAIT).await;
                return Ok(Prepared::LookupsLost(map(&error)));
            }
            // A server error aborts an open transaction block: running the
            // statement again would only report "current transaction is
            // aborted" (25P02) instead of this, the real error.
            Err(error)
                if error.as_db_error().is_some()
                    && self.shared.wait_until_ready(READY_WAIT).await
                    && self.shared.transaction_status() == Some(b'E') =>
            {
                return Err(map(&error));
            }
            Err(error) => {
                // Outside a transaction block nothing is aborted: run it
                // anyway (a statement that cannot be prepared still runs, an
                // error is reported again), values come back as text. Only
                // the SQLSTATE is logged: server messages can echo input.
                tracing::debug!(
                    sqlstate = error.code().map(SqlState::code),
                    "statement could not be prepared"
                );
                None
            }
        };
        Ok(Prepared::Typed(typed))
    }

    /// Prepares `text`, recovering when tokio-postgres's type lookups are
    /// gone: outside a transaction by reconnecting and preparing again;
    /// inside one (which the failed lookup has aborted) with an error that
    /// says so, and a reconnect once the transaction has ended.
    async fn prepare_recovering(
        &self,
        inner: &mut Inner,
        text: &str,
        offset_chars: usize,
    ) -> Result<Option<Vec<ColumnType>>, DriverError> {
        match self.prepare_statement(inner, text, offset_chars).await? {
            Prepared::Typed(typed) => return Ok(typed),
            Prepared::LookupsLost(error) if self.shared.transaction_status() != Some(b'I') => {
                inner.guard.reconnect_owed = Some(TYPE_STATEMENTS_NOTICE);
                return Err(DriverError {
                    message: format!(
                        "{}: a statement in this transaction dropped the prepared statements \
                         Varve uses to look up column types (DEALLOCATE ALL), and the failed \
                         lookup aborted the transaction. Roll it back; Varve then reconnects \
                         the session",
                        error.message
                    ),
                    ..error
                });
            }
            Prepared::LookupsLost(_) => {}
        }
        self.reconnect(inner, TYPE_STATEMENTS_NOTICE).await?;
        match self.prepare_statement(inner, text, offset_chars).await? {
            Prepared::Typed(typed) => Ok(typed),
            Prepared::LookupsLost(error) => Err(error),
        }
    }

    /// Runs `text` (prepared as `typed`) and collects its results.
    async fn run_prepared(
        &self,
        inner: &mut Inner,
        text: &str,
        offset_chars: usize,
        typed: Option<Vec<ColumnType>>,
        options: &ExecOptions,
        results: &mut Vec<StatementResult>,
    ) -> Result<(), DriverError> {
        let started = Instant::now();
        inner.phase_started = started;
        let timeout_ms = inner.timeout_ms;
        let map = |e: &tokio_postgres::Error| {
            exec_error(e, offset_chars, self.interrupt(started, timeout_ms))
        };

        if self.shared.cancel_requested() {
            return Err(cancelled_before(offset_chars));
        }
        self.shared.clear_tags();
        let stream = inner
            .client
            .simple_query_raw(text)
            .await
            .map_err(|e| map(&e))?;
        let mut stream = std::pin::pin!(stream);
        let mut result_index = 0usize;
        let mut current: Option<ResultBuilder> = None;
        while let Some(message) = stream.next().await {
            match message.map_err(|e| map(&e))? {
                SimpleQueryMessage::RowDescription(columns) => {
                    let types = typed
                        .as_ref()
                        .filter(|t| result_index == 0 && t.len() == columns.len());
                    current = Some(ResultBuilder::new(columns.iter().enumerate().map(
                        |(i, column)| {
                            let ty = types.and_then(|t| t.get(i));
                            (
                                column.name().to_owned(),
                                ty.map_or_else(|| UNKNOWN_TYPE.to_owned(), |t| t.name.clone()),
                                ty.map_or(ValueKind::Text, |t| t.kind.clone()),
                            )
                        },
                    )));
                }
                SimpleQueryMessage::Row(row) => {
                    let builder = current.as_mut().ok_or_else(|| {
                        DriverError::new(ErrorKind::Protocol, "row received before its columns")
                    })?;
                    if builder.rows.len() < options.max_rows {
                        let values = (0..row.len())
                            .map(|i| match (row.get(i), builder.kinds.get(i)) {
                                (None, _) => Value::Null,
                                (Some(text), Some(kind)) => convert(kind, text),
                                (Some(text), None) => Value::Text(text.to_owned()),
                            })
                            .collect();
                        builder.rows.push(values);
                    } else {
                        builder.truncated = true;
                    }
                }
                SimpleQueryMessage::CommandComplete(_) => {
                    let tag = self.shared.pop_tag().ok_or_else(|| {
                        DriverError::new(
                            ErrorKind::Protocol,
                            "the server's command tag was not seen",
                        )
                    })?;
                    if let Some(builder) = current.take() {
                        results.push(builder.finish());
                        result_index += 1;
                    } else if let Some(tag) = tag {
                        results.push(StatementResult::Command {
                            rows_affected: rows_affected(&tag),
                            tag,
                        });
                        result_index += 1;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Runs `sql` statement by statement, one simple query per statement
    /// (so each statement outside a transaction block commits on its own),
    /// collecting results and what became of each statement in `progress`.
    async fn run_script(
        &self,
        inner: &mut Inner,
        sql: &str,
        options: &ExecOptions,
        progress: &mut Progress,
    ) -> Result<(), DriverError> {
        if self.read_only
            && let Some(statement) = read_only_violation(sql, Dialect::Postgres)
        {
            return Err(read_only_rejection(sql, statement));
        }
        let spans = split_statements(sql, Dialect::Postgres);
        let words: Vec<Vec<String>> = spans
            .iter()
            .map(|span| words_outside_literals(span.text))
            .collect();
        if let Some(span) = spans
            .iter()
            .zip(&words)
            .find_map(|(span, words)| is_deallocate_all(words).then_some(span))
        {
            return Err(DriverError {
                position: u32::try_from(sql[..span.offset].chars().count() + 1).ok(),
                ..DriverError::new(
                    ErrorKind::Unsupported,
                    "DEALLOCATE ALL is not run: it would also drop the prepared statements \
                     Varve uses to look up column types. Use DEALLOCATE <name> for your own \
                     statements, or DISCARD ALL to reset the whole session; nothing was run",
                )
            });
        }
        let results = &mut progress.results;
        let mut chars_before = 0usize;
        let mut counted_to = 0usize;
        for (index, (span, words)) in spans.iter().zip(&words).enumerate() {
            chars_before += sql[counted_to..span.offset].chars().count();
            counted_to = span.offset;
            if span.kind == StatementKind::Copy && uses_client_copy(span.text) {
                return Err(DriverError::new(
                    ErrorKind::Unsupported,
                    "COPY FROM STDIN and COPY TO STDOUT are not run in the editor; \
                     use Run SQL file, which handles COPY",
                ));
            }
            let number = index + 1;
            self.before_statement(inner, number).await?;
            let status_before = self.shared.transaction_status();
            let results_before = results.len();
            let changes_before = self.shared.parameter_changes();
            // A procedure or DO block can COMMIT inside itself, starting a
            // new transaction under whatever it set; in a read-only session
            // it runs inside a read-only transaction block, where it cannot.
            let wrapped = self.read_only
                && status_before == Some(b'I')
                && words.first().is_some_and(|w| {
                    w.eq_ignore_ascii_case("CALL") || w.eq_ignore_ascii_case("DO")
                });
            // Inside a transaction block with translated messages, the
            // timeout in force is read now: after a failure the block is
            // aborted and nothing can be asked (see `decide_interrupt`).
            let timeout_at_start = if status_before == Some(b'T') && inner.messages.translated() {
                self.timeout_in_transaction(inner, chars_before).await?
            } else {
                None
            };
            if wrapped {
                let started = Instant::now();
                inner.read_only_block_open = true;
                if let Err(error) = inner.client.batch_execute("BEGIN READ ONLY").await {
                    let mapped = exec_error(
                        &error,
                        chars_before,
                        self.interrupt(started, inner.timeout_ms),
                    );
                    let error = self.leave_block(inner, mapped).await;
                    inner.read_only_block_open = false;
                    return Err(error);
                }
            }
            let outcome = match self
                .prepare_recovering(inner, span.text, chars_before)
                .await
            {
                Ok(typed) => {
                    // Earlier statements may have changed a setting these
                    // values depend on (a function calling set_config):
                    // check while the backend is idle, before reading them.
                    if inner.guard.executed_since_check
                        && self.reads_unreported(inner, typed.as_deref()).await
                    {
                        if self.shared.transaction_status() == Some(b'I') {
                            let scope = inner.guard.pending.merge(Scope {
                                query_unreported: true,
                                ..Scope::default()
                            });
                            self.guard_now(
                                inner,
                                scope,
                                &format!("a statement before statement {number}"),
                                false,
                            )
                            .await?;
                        } else {
                            // No query inside a transaction block: the
                            // values may be off; a later check says so.
                            inner.guard.unchecked_reads = true;
                        }
                    }
                    // DISCARD ALL resets what the init SQL set: the session
                    // reopens after it. Owed from before it is sent, so a
                    // call given up after the server ran it still reopens.
                    let discard = is_discard_all(words);
                    let owed_before = inner.guard.reconnect_owed;
                    // Inside a transaction block the server refuses it.
                    if discard && self.shared.transaction_status() == Some(b'I') {
                        inner.guard.reconnect_owed = Some(DISCARD_ALL_NOTICE);
                    }
                    let outcome = self
                        .run_prepared(inner, span.text, chars_before, typed, options, results)
                        .await;
                    if discard && outcome.is_err() {
                        // DISCARD ALL either runs whole or not at all.
                        inner.guard.reconnect_owed = owed_before;
                    }
                    outcome
                }
                Err(error) => Err(error),
            };
            // How long the failed message ran, measured before anything else
            // is sent.
            let failed_after = outcome
                .as_ref()
                .err()
                .map(|_| inner.phase_started.elapsed());
            let outcome = if wrapped {
                let outcome = self.end_read_only_block(inner, outcome, chars_before).await;
                inner.read_only_block_open = false;
                outcome
            } else {
                outcome
            };
            let outcome = match outcome {
                Err(error) => Err(self
                    .decide_interrupt(inner, error, failed_after, timeout_at_start)
                    .await),
                ok => ok,
            };
            if outcome.is_ok() {
                // The statement completed on the server: record it before
                // anything of the driver's own (a check, a reconnect) can
                // fail, so a failure there names the next statement.
                let effect = statement_effect(
                    span.text,
                    words,
                    status_before,
                    results.get(results_before..).unwrap_or_default(),
                );
                progress
                    .tracker
                    .completed(effect, open_after(self.shared.transaction_status()));
            }
            if outcome.is_ok() && is_discard_all(words) {
                // A fresh connection is what DISCARD ALL asks for, and it
                // brings back the type lookups and the session settings.
                self.reconnect(inner, DISCARD_ALL_NOTICE).await?;
                continue;
            }
            let scope = Scope::for_statement(span.kind, words);
            let changed = scope.query_unreported
                || calls_function(span.text)
                || self.shared.parameter_changes() != changes_before;
            self.after_statement(
                inner,
                number,
                status_before,
                scope,
                changed,
                outcome.as_ref().err(),
            )
            .await?;
            outcome?;
        }
        Ok(())
    }

    /// Work owed from before that must be done, while the backend is idle,
    /// before statement `number` runs: a reconnect after lost type lookups,
    /// and in a read-only session confirming read-only mode (a check that a
    /// cancel interrupted, or one carried over from the previous run).
    async fn before_statement(&self, inner: &mut Inner, number: usize) -> Result<(), DriverError> {
        if self.shared.transaction_status() != Some(b'I') {
            return Ok(());
        }
        if let Some(notice) = inner.guard.reconnect_owed {
            self.reconnect(inner, notice).await?;
        }
        if self.read_only && inner.guard.ro_owed {
            let who = format!("a statement before statement {number}");
            for _ in 0..RO_CHECK_ATTEMPTS {
                let scope = inner.guard.pending;
                if self.guard_now(inner, scope, &who, false).await? {
                    return Ok(());
                }
            }
            return Err(DriverError::new(
                ErrorKind::ReadOnly,
                "Varve could not confirm that the session is still read-only (the check was \
                 cancelled again and again), so the script was stopped before this statement",
            ));
        }
        Ok(())
    }

    /// Records what statement `number` (1-based) may have changed
    /// (`changed`: something unreported could have changed) and, when the
    /// backend is idle afterwards, checks the session (see [`guard`]).
    /// `status_before` is the transaction status before the statement ran.
    async fn after_statement(
        &self,
        inner: &mut Inner,
        number: usize,
        status_before: Option<u8>,
        scope: Scope,
        changed: bool,
        failure: Option<&DriverError>,
    ) -> Result<(), DriverError> {
        if failure.is_some_and(|e| e.kind == ErrorKind::ConnectionLost) {
            return Ok(());
        }
        inner.guard.pending = inner.guard.pending.merge(scope);
        inner.guard.maybe_changed |= changed;
        inner.guard.executed_since_check = true;
        // Any statement in a read-only session can run user code (a view,
        // a domain check, an operator) that switches read-only mode off.
        inner.guard.ro_owed |= self.read_only;
        // After an error the transaction state is only known once the
        // server's ReadyForQuery is read.
        if failure.is_some() && !self.shared.wait_until_ready(READY_WAIT).await {
            return Ok(());
        }
        if self.shared.transaction_status() != Some(b'I') {
            return Ok(());
        }
        if let Some(notice) = inner.guard.reconnect_owed {
            return self.reconnect(inner, notice).await;
        }
        if !self.check_due(inner) {
            return Ok(());
        }
        let who = if status_before == Some(b'I') {
            format!("statement {number}")
        } else {
            format!("the transaction that ended with statement {number}")
        };
        let scope = inner.guard.pending.merge(Scope {
            query_unreported: inner.guard.unchecked_reads,
            ..Scope::default()
        });
        self.guard_now(inner, scope, &who, failure.is_some())
            .await
            .map(drop)
    }

    /// True when the guard has something to do now.
    fn check_due(&self, inner: &Inner) -> bool {
        inner.guard.ro_owed
            || inner.guard.unchecked_reads
            || !inner.guard.pending.is_empty()
            || guard::reported_drift_exists(&self.shared)
            || !self.shared.leaked_statements().is_empty()
            || !self.shared.superseded_typeinfo_statements().is_empty()
    }

    /// Runs the guard for `scope` now (the backend is idle). `who` names the
    /// culprit in warnings. Returns whether the check completed (a cancel
    /// can make it skip). When the session cannot be put back it is closed;
    /// with `warn_if_lost` (a statement already failed, or the run is over)
    /// that is added as a warning instead of returned as the error.
    async fn guard_now(
        &self,
        inner: &mut Inner,
        scope: Scope,
        who: &str,
        warn_if_lost: bool,
    ) -> Result<bool, DriverError> {
        // Only the lookups tokio-postgres uses: an older one being gone (a
        // DEALLOCATE of Varve's whose reply was never read) is no reason to
        // reconnect.
        let type_names = self.shared.current_typeinfo_statements();
        match guard::ensure(
            &inner.client,
            &self.shared,
            self.expected,
            scope,
            &type_names,
        )
        .await
        {
            Ok(Outcome::Skipped) => Ok(false),
            Ok(Outcome::Checked(restored)) => {
                inner.guard.pending = Scope::default();
                if let Some(observed) = restored.statement_timeout_ms {
                    inner.timeout_ms = observed;
                }
                if let Some(observed) = restored.lc_messages.clone() {
                    inner.messages.observe(observed);
                }
                // A read-only check also reads the unreported settings.
                let covered_unreported = scope.query_unreported || self.read_only;
                if covered_unreported {
                    inner.guard.maybe_changed = false;
                    inner.guard.executed_since_check = false;
                }
                inner.guard.ro_owed = false;
                for warning in restored.warnings(who) {
                    self.shared.push_driver_warning(warning);
                }
                if covered_unreported {
                    if inner.guard.unchecked_reads && restored.changed_unreported() {
                        self.shared
                            .push_driver_notice("NOTICE", UNCHECKED_READS_NOTICE.to_owned());
                    }
                    inner.guard.unchecked_reads = false;
                }
                if restored.type_statements_lost {
                    self.reconnect(inner, TYPE_STATEMENTS_NOTICE).await?;
                } else {
                    self.drop_leaked(inner).await;
                }
                Ok(true)
            }
            Err(error) => {
                let message = format!(
                    "{who} may have changed {what} and Varve could not set it back, so the \
                     connection was closed: {}",
                    error.message,
                    what = if self.read_only {
                        "read-only mode or session settings"
                    } else {
                        "session settings"
                    }
                );
                inner.lost = Some(message.clone());
                if let Some(task) = &inner.connection {
                    task.abort();
                }
                if warn_if_lost {
                    // The statement's own error (or the run's results) is
                    // what to report; the lost connection goes with it.
                    self.shared.push_driver_warning(message);
                    return Ok(false);
                }
                Err(DriverError::new(ErrorKind::ConnectionLost, message))
            }
        }
    }

    /// Deallocates statements a failed prepare left on the server (the
    /// backend is idle). One that is already gone is just forgotten; one a
    /// cancel interrupted is tried again later.
    /// Type lookup statements a call given up half way left behind (no
    /// longer used by tokio-postgres) go the same way.
    async fn drop_leaked(&self, inner: &mut Inner) {
        let mut names = self.shared.leaked_statements();
        names.extend(self.shared.superseded_typeinfo_statements());
        for name in names {
            // Forgotten once the server answered. A call given up before
            // that keeps the name: the next cleanup sends the DEALLOCATE
            // again (any error other than a cancel forgets it: the cleanup
            // only runs while the connection is idle, so in practice that
            // error says the statement is already gone), and the check
            // of tokio-postgres's lookups never counts a superseded one, so
            // the lost reply cannot make the session reconnect. Forgetting
            // before sending would instead leave the statement on the
            // server, untracked, when the call is given up before the
            // DEALLOCATE goes out.
            let sql = format!("DEALLOCATE \"{}\"", name.replace('"', "\"\""));
            match inner.client.batch_execute(&sql).await {
                // A cancel landed on it: try again later.
                Err(error) if error.code().map(SqlState::code) == Some(QUERY_CANCELED) => return,
                Ok(()) | Err(_) => self.shared.forget_statement(&name),
            }
        }
    }

    /// Reads the `statement_timeout` in force inside the open transaction
    /// block, before a statement runs in it (`SHOW` takes no snapshot, so
    /// it changes nothing about the transaction). `None` when the value
    /// cannot be read. A cancel already requested stops the script here.
    async fn timeout_in_transaction(
        &self,
        inner: &mut Inner,
        offset_chars: usize,
    ) -> Result<Option<Option<u128>>, DriverError> {
        if self.shared.cancel_requested() {
            return Err(cancelled_before(offset_chars));
        }
        let started = Instant::now();
        match show(&inner.client, "statement_timeout").await {
            Ok(value) => Ok(parse_duration_ms(&value)),
            // A cancel or a closed connection ends the script, as it would
            // have ended the statement.
            Err(error)
                if error.kind == ErrorKind::ConnectionLost
                    || error.code.as_deref() == Some(QUERY_CANCELED) =>
            {
                let requested = self.shared.cancel_requested();
                let error = DriverError {
                    position: u32::try_from(offset_chars + 1).ok(),
                    ..error
                };
                if requested {
                    return Err(DriverError {
                        kind: ErrorKind::Cancelled,
                        ..error
                    });
                }
                let timed_out = Interrupt::for_statement(false, started, inner.timeout_ms);
                Err(decide_timeout(error, timed_out.timed_out))
            }
            Err(_) => Ok(None),
        }
    }

    /// Tells a statement timeout from a cancel by the server or another
    /// session for `error`, a `57014` whose message the server translated
    /// (an untranslated one says which it is), by comparing how long the
    /// statement ran (`failed_after`) with the `statement_timeout` that was
    /// in force: read before the statement inside a transaction block
    /// (`at_start`), or from the server right after the failure when the
    /// session is idle again. An autocommitted statement's own changes are
    /// rolled back by its failure and a `SET LOCAL` ends with its
    /// transaction, so the session's value then is the one the statement
    /// ran under, however it was set (a function, `set_config`, a role
    /// default). Any other error is returned as it is.
    async fn decide_interrupt(
        &self,
        inner: &mut Inner,
        error: DriverError,
        failed_after: Option<Duration>,
        at_start: Option<Option<u128>>,
    ) -> DriverError {
        // Not from an interrupt Varve asked for: user code that catches it
        // can re-raise it with a message of its own, still labelled a cancel.
        if !self.shared.cancel_requested() {
            inner.messages.learn(&error);
        }
        if !timeout_unknown(&error, self.shared.cancel_requested()) {
            return error;
        }
        let Some(elapsed) = failed_after else {
            return error;
        };
        let in_force = match at_start {
            Some(value) => Some(value),
            None => self.timeout_after_failure(inner).await,
        };
        match in_force {
            Some(value) => decide_timeout(error, value.is_some_and(|ms| elapsed.as_millis() >= ms)),
            None => error,
        }
    }

    /// The session's `statement_timeout`, read once the backend is idle
    /// after a failure; `None` when it is not idle or cannot be asked.
    async fn timeout_after_failure(&self, inner: &mut Inner) -> Option<Option<u128>> {
        if !self.shared.wait_until_ready(READY_WAIT).await
            || self.shared.transaction_status() != Some(b'I')
        {
            return None;
        }
        self.shared.wait_for_cancels().await;
        let value = parse_duration_ms(&show(&inner.client, "statement_timeout").await.ok()?)?;
        inner.timeout_ms = value;
        Some(value)
    }

    /// Brings the session back to a known state after a call that was given
    /// up half way (its future dropped while a statement, a check or a
    /// reconnect was under way). What it sent may still be running and its
    /// checks never ran, so nothing it did is trusted:
    ///
    /// - an empty query is sent and its reply awaited, which comes after
    ///   every reply to what the abandoned call sent, so the transaction
    ///   status is current;
    /// - a read-only block around a CALL or DO that was left open is rolled
    ///   back;
    /// - an owed reconnect (a `DISCARD ALL`, lost type lookups, a reconnect
    ///   given up half way) is made;
    /// - every setting, read-only mode, the statement timeout and the type
    ///   lookups are checked and set back, with the usual warnings. Inside
    ///   a transaction the user left open that check is owed instead, as
    ///   for any statement in a transaction block.
    ///
    /// An error means the session could not be brought back and is closed.
    async fn recover_abandoned(&self, inner: &mut Inner) -> Result<(), DriverError> {
        let who = "a statement of an earlier call that was given up before it finished";
        // Owed until it is done: dropping this call too keeps it owed.
        inner.guard.pending = inner.guard.pending.merge(Scope::SETTINGS).merge(Scope {
            resets: true,
            ..Scope::default()
        });
        inner.guard.maybe_changed = true;
        inner.guard.executed_since_check = true;
        inner.guard.ro_owed |= self.read_only;
        if let Err(error) = inner.client.simple_query("").await
            && (error.is_closed() || error.code().is_none())
        {
            return Err(exec_error(&error, 0, Interrupt::NONE));
        }
        // Every reply of the abandoned call has been read: its server
        // notices are not this call's.
        drop(self.shared.take_server_notices());
        if inner.read_only_block_open {
            if self.shared.transaction_status() != Some(b'I') {
                let error = self
                    .leave_block(
                        inner,
                        DriverError::new(
                            ErrorKind::Protocol,
                            "a CALL or DO in a read-only transaction was given up half way",
                        ),
                    )
                    .await;
                if inner.lost.is_some() {
                    return Err(error);
                }
            }
            inner.read_only_block_open = false;
        }
        if self.shared.transaction_status() != Some(b'I') {
            // Inside the user's transaction: checked once it ends.
            return Ok(());
        }
        if let Some(notice) = inner.guard.reconnect_owed {
            self.reconnect(inner, notice).await?;
        }
        for _ in 0..RO_CHECK_ATTEMPTS {
            let scope = inner.guard.pending.merge(Scope::SETTINGS);
            if self.guard_now(inner, scope, who, false).await? {
                return Ok(());
            }
        }
        if self.read_only {
            return Err(DriverError::new(
                ErrorKind::ReadOnly,
                "Varve could not confirm that the session is still read-only (the check was \
                 cancelled again and again), so nothing was run",
            ));
        }
        Ok(())
    }

    /// The check at the end of a run, when the backend is idle and something
    /// in the run could have changed a setting, so no change survives into
    /// the next run unseen.
    async fn guard_at_end(&self, inner: &mut Inner, failure: Option<&DriverError>) {
        if inner.lost.is_some() || failure.is_some_and(|e| e.kind == ErrorKind::ConnectionLost) {
            return;
        }
        if failure.is_some() && !self.shared.wait_until_ready(READY_WAIT).await {
            return;
        }
        if self.shared.transaction_status() != Some(b'I') {
            return;
        }
        if let Some(notice) = inner.guard.reconnect_owed {
            if let Err(error) = self.reconnect(inner, notice).await {
                self.shared.push_driver_warning(error.message);
            }
            return;
        }
        if !inner.guard.maybe_changed && !self.check_due(inner) {
            return;
        }
        let scope = if inner.guard.maybe_changed {
            Scope::SETTINGS.merge(inner.guard.pending)
        } else {
            inner.guard.pending
        };
        // A lost session is reported as a warning with this run's results
        // (or error), and every later call fails. A skipped check stays
        // owed for the next run.
        if let Err(error) = self
            .guard_now(inner, scope, "a statement in this run", true)
            .await
        {
            self.shared.push_driver_warning(error.message);
        }
    }
}

/// What a script run has done so far: the results, and what became of
/// each completed statement.
#[derive(Default)]
struct Progress {
    results: Vec<StatementResult>,
    tracker: TransactionTracker,
}

/// Whether a transaction is open, from a `ReadyForQuery` status.
fn open_after(status: Option<u8>) -> Option<bool> {
    match status {
        Some(b'I') => Some(false),
        Some(b'T' | b'E') => Some(true),
        _ => None,
    }
}

/// What a completed statement did to the transaction. `words` are its words
/// outside literals, `status_before` the transaction status before it ran,
/// `results` what it returned.
fn statement_effect(
    text: &str,
    words: &[String],
    status_before: Option<u8>,
    results: &[StatementResult],
) -> TransactionEffect {
    match transaction_effect(text, Dialect::Postgres) {
        // Inside a transaction block a procedure or DO block cannot commit
        // or roll back: it is part of the block like any statement.
        TransactionEffect::Opaque
            if matches!(status_before, Some(b'T' | b'E'))
                && words.first().is_some_and(|w| {
                    w.eq_ignore_ascii_case("CALL") || w.eq_ignore_ascii_case("DO")
                }) =>
        {
            TransactionEffect::Statement
        }
        // COMMIT of an aborted transaction rolls it back, and says so in
        // its tag.
        TransactionEffect::Commit { chain }
            if results.iter().any(
                |r| matches!(r, StatementResult::Command { tag, .. } if tag == "ROLLBACK"),
            ) =>
        {
            TransactionEffect::Rollback { chain }
        }
        effect => effect,
    }
}

/// What preparing a statement gave.
enum Prepared {
    /// The column types, or `None` when it could not be prepared.
    Typed(Option<Vec<ColumnType>>),
    /// tokio-postgres's type lookup statements are gone (the error).
    LookupsLost(DriverError),
}

/// How often the read-only block around a CALL or DO is rolled back before
/// the session is given up.
const ROLLBACK_ATTEMPTS: usize = 3;

/// A `statement_timeout` value (`5s`, `'1500ms'`, `200`, `1min`) in ms;
/// `Some(None)` for 0 (no timeout), `None` when it cannot be read.
pub(crate) fn parse_duration_ms(value: &str) -> Option<Option<u128>> {
    let value = value.trim().trim_matches('\'').trim();
    let split = value
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: f64 = number.parse().ok()?;
    let factor = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "ms" => 1.0,
        "us" => 0.001,
        "s" => 1_000.0,
        "min" => 60_000.0,
        "h" => 3_600_000.0,
        "d" => 86_400_000.0,
        _ => return None,
    };
    let ms = (number * factor).round();
    if !(0.0..=1e15).contains(&ms) {
        return None;
    }
    // In range and rounded: the conversion is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = ms as u128;
    Some((ms > 0).then_some(ms))
}

/// Shown when a check after a transaction found float or bytea settings
/// changed while such values were read inside it.
const UNCHECKED_READS_NOTICE: &str = "float or bytea values read inside that transaction after \
     the change may have been rounded or shown in another format";

/// True when `message` contains one of `names` as a whole word (a name
/// such as `s2` does not match `s26`).
fn names_one_of(message: &str, names: &[String]) -> bool {
    names.iter().any(|name| {
        message.match_indices(name.as_str()).any(|(at, _)| {
            let before = message[..at].chars().next_back();
            let after = message[at + name.len()..].chars().next();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    })
}

/// How often a read-only session tries to confirm read-only mode before a
/// statement before it gives up.
const RO_CHECK_ATTEMPTS: usize = 3;

/// SQL words that are followed by `(` without being a function call.
const NOT_CALLS: [&str; 34] = [
    "IN",
    "VALUES",
    "EXISTS",
    "ANY",
    "ALL",
    "SOME",
    "ARRAY",
    "ROW",
    "AS",
    "USING",
    "ON",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "SELECT",
    "INTO",
    "OVER",
    "FILTER",
    "BY",
    "WHEN",
    "THEN",
    "ELSE",
    "IS",
    "LIKE",
    "BETWEEN",
    "DISTINCT",
    "RETURNING",
    "SET",
    "JOIN",
    "TABLE",
    "WITH",
    "LATERAL",
];

/// True when `sql` may call a function: a name directly followed by `(`
/// outside literals and comments (not a keyword such as `IN (` or a table
/// such as `INSERT INTO t (`). False positives only cost a check.
fn calls_function(sql: &str) -> bool {
    let chars: Vec<char> = sql.chars().collect();
    let mut previous: Option<String> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\''
            || c == '$'
            || (c == '-' && chars.get(i + 1) == Some(&'-'))
            || (c == '/' && chars.get(i + 1) == Some(&'*'))
        {
            // Literals and comments: skip them the same way the word scanner
            // does by scanning from here.
            let rest: String = chars[i..].iter().collect();
            let skipped = skip_literal_or_comment(&rest);
            if skipped > 0 {
                i += skipped;
                previous = None;
                continue;
            }
        }
        let start = i;
        let word = if c == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            i += 1;
            Some(chars[start..i.min(chars.len())].iter().collect::<String>())
        } else if c.is_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            Some(chars[start..i].iter().collect::<String>())
        } else {
            i += 1;
            None
        };
        if let Some(word) = word {
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let after_table = previous.as_deref().is_some_and(|p| {
                ["INTO", "TABLE", "ONLY"]
                    .iter()
                    .any(|k| p.eq_ignore_ascii_case(k))
            });
            if chars.get(j) == Some(&'(')
                && !after_table
                && !NOT_CALLS.iter().any(|k| word.eq_ignore_ascii_case(k))
            {
                return true;
            }
            previous = Some(word);
        } else if !c.is_whitespace() && c != '.' {
            previous = None;
        }
    }
    false
}

/// Length in chars of the string literal, dollar-quoted string or comment at
/// the start of `text`, or 0 when there is none.
fn skip_literal_or_comment(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    match chars.first() {
        Some('\'') => {
            let mut i = 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    return i + 1;
                }
                i += 1;
            }
            chars.len()
        }
        Some('-') if chars.get(1) == Some(&'-') => {
            chars.iter().position(|c| *c == '\n').unwrap_or(chars.len())
        }
        Some('/') if chars.get(1) == Some(&'*') => {
            let mut depth = 0usize;
            let mut i = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        return i;
                    }
                } else {
                    i += 1;
                }
            }
            chars.len()
        }
        Some('$') => {
            let end = chars[1..]
                .iter()
                .position(|c| !(c.is_alphanumeric() || *c == '_'))
                .map(|p| p + 1);
            match end {
                Some(end) if chars[end] == '$' => {
                    let tag = &chars[..=end];
                    let mut i = end + 1;
                    while i < chars.len() && !chars[i..].starts_with(tag) {
                        i += 1;
                    }
                    (i + tag.len()).min(chars.len())
                }
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// Adds the driver's warnings to an error's detail, after the server's own
/// detail: an error carries no notices, and a warning such as "read-only
/// mode was switched back on" must still reach the user.
fn with_warnings(mut error: DriverError, notices: Vec<Notice>) -> DriverError {
    if notices.is_empty() {
        return error;
    }
    let lines: Vec<String> = notices
        .iter()
        .map(|n| {
            let label = if n.severity == "WARNING" {
                "Warning"
            } else {
                "Notice"
            };
            format!("{label}: {}", n.message)
        })
        .collect();
    error.detail = Some(match error.detail.take() {
        Some(detail) => format!("{detail}\n{}", lines.join("\n")),
        None => lines.join("\n"),
    });
    error
}

/// Sets the transaction status callers see to unknown when dropped, unless
/// disarmed: a call whose future is dropped half way leaves it unknown.
struct UnknownStatusOnDrop<'a>(Option<&'a AtomicU8>);

impl UnknownStatusOnDrop<'_> {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for UnknownStatusOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(status) = self.0 {
            status.store(0, AtomicOrdering::Release);
        }
    }
}

/// The message for a call on a connection that has closed.
const CONNECTION_CLOSED: &str = "the connection to the server was closed";

/// The error for a script stopped by a cancel between two statements.
fn cancelled_before(offset_chars: usize) -> DriverError {
    DriverError {
        position: u32::try_from(offset_chars + 1).ok(),
        ..DriverError::new(
            ErrorKind::Cancelled,
            "cancelled before this statement ran; the statements before it have run",
        )
    }
}

/// `statement` shortened for an error message.
fn quote_statement(statement: &str) -> String {
    let flat: String = statement.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_QUOTED_STATEMENT {
        flat
    } else {
        let short: String = flat.chars().take(MAX_QUOTED_STATEMENT).collect();
        format!("{short}...")
    }
}

/// The error for a script with a statement that would switch read-only mode
/// off. `statement` is a slice of `sql`.
fn read_only_rejection(sql: &str, statement: &str) -> DriverError {
    let byte = (statement.as_ptr() as usize)
        .checked_sub(sql.as_ptr() as usize)
        .filter(|offset| sql.is_char_boundary(*offset));
    DriverError {
        position: byte.and_then(|b| u32::try_from(sql[..b].chars().count() + 1).ok()),
        ..DriverError::new(
            ErrorKind::ReadOnly,
            format!(
                "the connection is read-only and \"{}\" would switch read-only mode off; \
                 nothing was run",
                quote_statement(statement)
            ),
        )
    }
}

/// Collects one result set.
struct ResultBuilder {
    columns: Vec<Column>,
    kinds: Vec<ValueKind>,
    rows: Vec<Vec<Value>>,
    truncated: bool,
}

impl ResultBuilder {
    fn new(columns: impl Iterator<Item = (String, String, ValueKind)>) -> Self {
        let (columns, kinds) = columns
            .map(|(name, type_name, kind)| (Column { name, type_name }, kind))
            .unzip();
        Self {
            columns,
            kinds,
            rows: Vec::new(),
            truncated: false,
        }
    }

    fn finish(self) -> StatementResult {
        StatementResult::Rows {
            columns: self.columns,
            rows: self.rows,
            truncated: self.truncated,
        }
    }
}

/// The row count at the end of a tag such as `INSERT 0 3` or `UPDATE 3`.
fn rows_affected(tag: &str) -> Option<u64> {
    let mut words = tag.split_whitespace();
    let _command = words.next()?;
    words.last()?.parse().ok()
}

/// The notice shown after the guard found tokio-postgres's prepared
/// statements gone and reconnected.
const TYPE_STATEMENTS_NOTICE: &str = "Varve reconnected the session because a statement dropped \
     the prepared statements it uses to look up column types (DEALLOCATE ALL); temporary \
     tables and session settings were reset";

/// The notice shown after `DISCARD ALL` reconnected the session.
const DISCARD_ALL_NOTICE: &str =
    "DISCARD ALL: Varve reconnected the session so type information stays correct";

fn word_is(words: &[String], index: usize, word: &str) -> bool {
    words
        .get(index)
        .is_some_and(|w| w.eq_ignore_ascii_case(word))
}

/// `DISCARD ALL`, from a statement's bare words.
fn is_discard_all(words: &[String]) -> bool {
    words.len() == 2 && word_is(words, 0, "DISCARD") && word_is(words, 1, "ALL")
}

/// `DEALLOCATE [PREPARE] ALL`, from a statement's bare words.
fn is_deallocate_all(words: &[String]) -> bool {
    let rest = if word_is(words, 1, "PREPARE") { 2 } else { 1 };
    words.len() == rest + 1 && word_is(words, 0, "DEALLOCATE") && word_is(words, rest, "ALL")
}

/// True when a `COPY` statement reads from or writes to the client.
fn uses_client_copy(sql: &str) -> bool {
    words_outside_literals(sql)
        .iter()
        .any(|w| w.eq_ignore_ascii_case("STDIN") || w.eq_ignore_ascii_case("STDOUT"))
}

/// The bare words of `sql`, skipping string literals, quoted identifiers,
/// dollar-quoted strings and comments.
fn words_outside_literals(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut words = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == c {
                        if chars.get(i + 1) == Some(&c) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                let mut depth = 0usize;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            '$' => {
                let tag_end = chars[i + 1..]
                    .iter()
                    .position(|c| !(c.is_alphanumeric() || *c == '_'))
                    .map(|p| i + 1 + p);
                match tag_end {
                    Some(end) if chars[end] == '$' => {
                        let tag: Vec<char> = chars[i..=end].to_vec();
                        i = end + 1;
                        while i < chars.len() && !chars[i..].starts_with(&tag) {
                            i += 1;
                        }
                        i += tag.len();
                    }
                    _ => i += 1,
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                words.push(chars[start..i].iter().collect());
            }
            _ => i += 1,
        }
    }
    words
}

#[async_trait::async_trait]
impl Session for PostgresSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    /// The server's answer to the SSLRequest of the connection: `S` means
    /// it switched to TLS. Without TLS (`disable`) no request is sent.
    fn tls_in_use(&self) -> Option<bool> {
        Some(self.shared.ssl_response() == Some(b'S'))
    }

    /// Seen by the tap, which parses only connections without TLS.
    fn password_in_clear(&self) -> bool {
        self.shared.password_in_clear()
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::clone(&self.cancel) as Arc<dyn CancelHandle>
    }

    async fn execute(&self, sql: &str, options: &ExecOptions) -> Result<ExecOutput, DriverError> {
        let mut inner = self.inner.lock().await;
        if let Some(reason) = &inner.lost {
            return Err(DriverError::new(ErrorKind::ConnectionLost, reason.clone()));
        }
        if self.connection_gone(&inner) {
            // Nothing can run, and no reply will come: fail at once.
            self.visible_status.store(0, AtomicOrdering::Release);
            return Err(DriverError::new(
                ErrorKind::ConnectionLost,
                CONNECTION_CLOSED,
            ));
        }
        // Cancel requests from here on are this call's; one for the
        // previous call that is still being sent must not land on this one.
        self.shared.begin_call();
        self.shared.wait_for_cancels().await;
        let started = Instant::now();
        self.shared.clear_notices();
        for warning in std::mem::take(&mut inner.pending_warnings) {
            self.shared.push_driver_warning(warning);
        }
        // Dropped before it ends, this call leaves the transaction status
        // unknown, and the next call recovers.
        let unknown_if_dropped = UnknownStatusOnDrop(Some(&self.visible_status));
        let mut progress = Progress::default();
        let outcome = if inner.call_open {
            self.recover_abandoned(&mut inner).await
        } else {
            Ok(())
        };
        inner.call_open = true;
        let outcome = match outcome {
            Ok(()) => {
                self.run_script(&mut inner, sql, options, &mut progress)
                    .await
            }
            Err(error) => Err(error),
        };
        self.guard_at_end(&mut inner, outcome.as_ref().err()).await;
        self.publish_status(&inner, outcome.as_ref().err()).await;
        inner.call_open = false;
        unknown_if_dropped.disarm();
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Err(error) = outcome {
            let completed = progress.tracker.completed_count();
            // A transaction that ended during the failed statement (a
            // failing COMMIT) was rolled back.
            let partial = PartialOutput {
                failed_statement: u32::try_from(completed).ok(),
                completed: progress
                    .tracker
                    .finish(Session::in_transaction(self), Durability::RolledBack),
                results: progress.results,
                notices: self.shared.take_server_notices(),
                elapsed_ms,
            };
            return Err(
                with_warnings(error, self.shared.take_driver_notices()).with_partial(partial)
            );
        }
        Ok(ExecOutput {
            results: progress.results,
            notices: self.shared.take_notices(),
            elapsed_ms,
        })
    }

    async fn ping(&self) -> Result<(), DriverError> {
        let inner = self.inner.lock().await;
        if let Some(reason) = &inner.lost {
            return Err(DriverError::new(ErrorKind::ConnectionLost, reason.clone()));
        }
        if self.connection_gone(&inner) {
            return Err(DriverError::new(
                ErrorKind::ConnectionLost,
                CONNECTION_CLOSED,
            ));
        }
        inner
            .client
            .simple_query("SELECT 1")
            .await
            .map(drop)
            .map_err(|e| exec_error(&e, 0, Interrupt::NONE))
    }

    async fn close(self: Box<Self>) -> Result<(), DriverError> {
        let Inner {
            client, connection, ..
        } = self.inner.into_inner();
        // Dropping the client makes the connection task send Terminate and
        // finish.
        drop(client);
        if let Some(task) = connection {
            let abort = task.abort_handle();
            if tokio::time::timeout(CLOSE_TIMEOUT, task).await.is_err() {
                abort.abort();
                return Err(timeout_error("closing the connection", CLOSE_TIMEOUT));
            }
        }
        Ok(())
    }

    fn connect_notices(&self) -> Vec<Notice> {
        self.connect_notices.clone()
    }

    fn in_transaction(&self) -> Option<bool> {
        // A connection that has ended has no transaction status, even
        // before the next call notices.
        if self.shared.connection_ended() {
            return None;
        }
        match self.visible_status.load(AtomicOrdering::Acquire) {
            b'T' | b'E' => Some(true),
            b'I' => Some(false),
            _ => None,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Sends a PostgreSQL cancel request over a new connection, with the same
/// TLS setup as the session.
struct PostgresCancelHandle {
    /// The current backend's key; replaced when the session reconnects.
    token: StdMutex<CancelToken>,
    host: String,
    port: u16,
    connect_timeout: Duration,
    tls: TlsSetup,
    shared: Arc<Shared>,
}

impl PostgresCancelHandle {
    fn token(&self) -> std::sync::MutexGuard<'_, CancelToken> {
        self.token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_token(&self, token: CancelToken) {
        *self.token() = token;
    }
}

#[async_trait::async_trait]
impl CancelHandle for PostgresCancelHandle {
    async fn cancel(&self) -> Result<(), DriverError> {
        // Registered (and counted for the current call) before anything is
        // sent: an execution that sees it and returns must find the request
        // in flight and wait for it.
        let _in_flight = self.shared.cancel_started();
        let request = async {
            let socket = open_socket(&self.host, self.port).await?;
            // The cancel connection carries no results: its tap only forwards.
            let scratch = Arc::new(Shared::default());
            let stream = TapStream::cancel_request(socket, Arc::clone(&scratch));
            let token = self.token().clone();
            token
                .cancel_query_raw(stream, self.tls.connector(scratch))
                .await
                .map_err(|e| {
                    connect_error(
                        &e,
                        ConnectContext {
                            tls_required: matches!(self.tls.ssl_mode, SslMode::Require),
                            ssl_response: None,
                            password: None,
                        },
                    )
                })
        };
        tokio::time::timeout(self.connect_timeout, request)
            .await
            .map_err(|_| timeout_error("sending the cancel request", self.connect_timeout))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_statement_names_as_words() {
        let names = vec!["s2".to_owned(), "s7".to_owned()];
        assert!(names_one_of(
            "prepared statement \"s2\" does not exist",
            &names
        ));
        assert!(names_one_of(
            "la sentencia preparada «s7» no existe",
            &names
        ));
        assert!(!names_one_of(
            "prepared statement \"s26\" does not exist",
            &names
        ));
        assert!(!names_one_of(
            "prepared statement \"nosuch\" does not exist",
            &names
        ));
        assert!(!names_one_of(
            "prepared statement \"s2\" does not exist",
            &[]
        ));
    }

    #[test]
    fn reads_durations() {
        assert_eq!(parse_duration_ms("1500ms"), Some(Some(1500)));
        assert_eq!(parse_duration_ms("20s"), Some(Some(20_000)));
        assert_eq!(parse_duration_ms("'2s'"), Some(Some(2000)));
        assert_eq!(parse_duration_ms("1min"), Some(Some(60_000)));
        assert_eq!(parse_duration_ms("300"), Some(Some(300)));
        assert_eq!(parse_duration_ms("0"), Some(None));
        assert_eq!(parse_duration_ms("lots"), None);
    }

    #[test]
    fn knows_which_locales_translate() {
        for english in [
            "C",
            "C.UTF-8",
            "POSIX",
            "en_US.utf8",
            "en_GB",
            "en",
            "English_United States.1252",
        ] {
            assert!(is_english_locale(english), "{english}");
        }
        for translated in ["de_DE.UTF-8", "nl_NL", "fr", "", "ca_ES"] {
            assert!(!is_english_locale(translated), "{translated}");
        }
        let mut language = MessageLanguage::new("nl_NL.UTF-8".into());
        assert!(language.translated());
        // A server built without translations sends English anyway.
        let english = DriverError {
            code: Some(QUERY_CANCELED.into()),
            ..DriverError::new(
                ErrorKind::Timeout,
                "canceling statement due to statement timeout",
            )
        };
        language.learn(&english);
        assert!(!language.translated());
        // What was learned holds for that lc_messages only.
        language.observe("nl_NL.UTF-8".into());
        assert!(!language.translated());
        language.observe("de_DE.UTF-8".into());
        assert!(language.translated());
        let mut language = MessageLanguage::new("en_US.utf8".into());
        assert!(!language.translated());
        language.learn(&DriverError {
            code: Some(QUERY_CANCELED.into()),
            ..DriverError::new(ErrorKind::Cancelled, "Anweisung wird abgebrochen")
        });
        assert!(language.translated());
        // Other errors teach nothing.
        let mut language = MessageLanguage::new("de_DE".into());
        language.learn(&DriverError::new(ErrorKind::Query, "division by zero"));
        assert!(language.translated());
    }

    #[test]
    fn spots_function_calls() {
        assert!(calls_function("SELECT public.evil()"));
        assert!(calls_function("SELECT \"Evil\" (1)"));
        assert!(calls_function("SELECT count(*) FROM t"));
        assert!(!calls_function("SELECT 1"));
        assert!(!calls_function("SELECT 'f()' -- g()\n FROM t"));
        assert!(!calls_function("SELECT $$f()$$, /* g() */ 2"));
        assert!(!calls_function("INSERT INTO t (a, b) VALUES (1, 2)"));
        assert!(!calls_function(
            "SELECT * FROM t WHERE a IN (1, 2) AND (b OR c)"
        ));
        assert!(calls_function("INSERT INTO t VALUES (nextval('s'))"));
    }

    #[test]
    fn finds_discard_and_deallocate_all() {
        let words = |sql: &str| words_outside_literals(sql);
        assert!(is_discard_all(&words("discard all")));
        assert!(is_discard_all(&words("DISCARD /* c */ ALL")));
        assert!(!is_discard_all(&words("DISCARD PLANS")));
        assert!(!is_discard_all(&words("SELECT 'DISCARD ALL'")));
        assert!(is_deallocate_all(&words("DEALLOCATE ALL")));
        assert!(is_deallocate_all(&words("deallocate prepare all")));
        assert!(!is_deallocate_all(&words("DEALLOCATE my_statement")));
        assert!(!is_deallocate_all(&words("DEALLOCATE \"all\"")));
        assert!(!is_deallocate_all(&words("SELECT $$DEALLOCATE ALL$$")));
    }

    #[test]
    fn rows_affected_from_tags() {
        assert_eq!(rows_affected("UPDATE 3"), Some(3));
        assert_eq!(rows_affected("INSERT 0 12"), Some(12));
        assert_eq!(rows_affected("SELECT 0"), Some(0));
        assert_eq!(rows_affected("CREATE TABLE"), None);
        assert_eq!(rows_affected("BEGIN"), None);
    }

    #[test]
    fn detects_client_side_copy() {
        assert!(uses_client_copy("COPY t FROM STDIN"));
        assert!(uses_client_copy(
            "copy (select 1) to stdout with (format csv)"
        ));
        assert!(uses_client_copy("COPY t (a, b) FROM stdin WITH CSV"));
        assert!(!uses_client_copy("COPY t FROM '/tmp/stdin.csv'"));
        assert!(!uses_client_copy(
            "COPY (SELECT 'to stdout', $$stdin$$) TO '/tmp/x' -- stdout\n"
        ));
        assert!(!uses_client_copy(
            "COPY t TO PROGRAM 'gzip > /tmp/x' /* stdout */"
        ));
        assert!(!uses_client_copy("COPY \"stdin\" FROM '/tmp/x'"));
    }

    #[test]
    fn server_versions() -> Result<(), DriverError> {
        let v18 = server_info("180001", "18.1")?;
        assert_eq!((v18.major, v18.minor, v18.patch), (18, 1, None));
        let v13 = server_info("130022", "13.22 (Debian 13.22-1)")?;
        assert_eq!((v13.major, v13.minor), (13, 22));
        assert_eq!(v13.version, "13.22 (Debian 13.22-1)");
        let v96 = server_info("90624", "9.6.24")?;
        assert_eq!((v96.major, v96.minor, v96.patch), (9, 6, Some(24)));
        let fallback = server_info("x", "16.4")?;
        assert_eq!((fallback.major, fallback.minor), (16, 4));
        Ok(())
    }

    #[test]
    fn statement_timeouts_out_of_range_are_refused() {
        for ok in [None, Some(1), Some(1500), Some(2_147_483_647)] {
            assert!(
                check_statement_timeout(ok.map(Duration::from_millis)).is_ok(),
                "{ok:?}"
            );
        }
        for (bad, says) in [
            (Duration::from_micros(500), "500 microseconds"),
            (Duration::ZERO, "0 microseconds"),
            (Duration::from_millis(2_147_483_648), "2147483648 ms"),
            (Duration::from_secs(25 * 86_400), "2160000000 ms"),
        ] {
            let error = check_statement_timeout(Some(bad))
                .err()
                .unwrap_or_else(|| DriverError::new(ErrorKind::Query, "accepted"));
            assert_eq!(error.kind, ErrorKind::Unsupported, "{bad:?}");
            assert!(error.message.contains(says), "{bad:?}: {}", error.message);
        }
    }

    #[test]
    fn setup_sql_follows_the_params() {
        let mut params = ConnectParams::new(Engine::Postgres, "localhost", 5432);
        assert!(!session_setup_sql(&params).contains("read_only"));
        params.read_only = true;
        params.statement_timeout = Some(Duration::from_millis(1500));
        let sql = session_setup_sql(&params);
        assert!(sql.contains("SET default_transaction_read_only = on"));
        assert!(sql.contains("SET statement_timeout = 1500"));
        assert!(sql.starts_with("SET DateStyle = ISO, YMD"));
    }
}
