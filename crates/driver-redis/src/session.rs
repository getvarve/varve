//! The Redis session: console execution, safety checks, cancel and
//! transparent reconnects.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use redis::Value as Resp;
use redis::aio::MultiplexedConnection;
use tokio::sync::Notify;
use tokio::time::Instant as Deadline;
use varve_driver::{
    CancelHandle, ConnectParams, DriverError, Durability, ErrorKind, ExecOptions, ExecOutput,
    Notice, PartialOutput, ServerInfo, Session, StatementResult, TransactionEffect,
    TransactionTracker,
};

use crate::conn::{self, Connection, Endpoint, ReadBudget, ServerDetails, ServerFlavor};
use crate::error;
use crate::measure::RawConn;
use crate::pagestate::{PageStates, ScanState};
use crate::parse::{
    ParsedCommand, check_init_script, init_command_problem, init_error, parse_script,
};
use crate::reply::{as_int, server_error_text, to_result};
use crate::risk::{Assessment, CommandRisk, CommandSpec, assess};

/// Most bytes one reply may take on the wire (16 MiB), for console
/// commands and browser reads alike. The connection stops reading when a
/// reply goes over it, so the protocol library never buffers more: the
/// command fails with a Query error and the session reconnects on its next
/// call. A decoded reply needs more memory than its wire size, about
/// 100 bytes per element on top of the element data.
pub const MAX_REPLY_BYTES: u64 = 16 * 1024 * 1024;

/// How long a cancelled call waits for the server-side kill to report
/// before it returns, so the result can say whether the server stopped.
const KILL_REPORT_WAIT: Duration = Duration::from_secs(2);

/// Console commands that would leave the shared request/response connection
/// in a state the session cannot use. The app offers dedicated panels
/// (Pub/Sub, monitor) instead.
const CONSOLE_UNSUPPORTED: [&str; 7] = [
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "SSUBSCRIBE",
    "MONITOR",
    "SYNC",
    "PSYNC",
    "RESET",
];

/// State shared between a session and its cancel handle.
struct Shared {
    endpoint: Endpoint,
    /// Wakes the running call when it is cancelled.
    notify: Notify,
    /// Wakes a cancelled call when the server-side kill has finished.
    kill_done: Notify,
    /// Side connections opened for measuring oversized entries.
    side_opened: std::sync::atomic::AtomicU64,
    state: Mutex<CallState>,
}

#[derive(Default)]
struct CallState {
    /// A call is running.
    busy: bool,
    /// The running call was cancelled.
    cancelled: bool,
    /// `CLIENT ID` of the live connection.
    client_id: Option<i64>,
    /// Counts calls, so a late kill report never lands on a later call.
    generation: u64,
    /// Server-side kill of the running call.
    kill: Kill,
}

/// Progress of `CLIENT KILL` for a cancelled call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Kill {
    /// Not asked for: nothing was cancelled, or nothing ran on the server.
    #[default]
    None,
    /// The kill is on its way.
    Pending,
    /// The server closed the connection.
    Done,
    /// The server could not be asked; the reason.
    Failed(String),
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, CallState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_cancelled(&self) -> bool {
        self.state().cancelled
    }
}

/// Connection state guarded by the session's async mutex.
struct Inner {
    conn: Option<Connection>,
    /// Database to SELECT on reconnect (follows console `SELECT`).
    db: u16,
    /// `COMMAND INFO` cache by upper-case name; `None` for unknown commands.
    commands: HashMap<String, Option<CommandSpec>>,
    /// Rests of hash and set scan steps for the next page.
    pages: PageStates,
    /// A console `MULTI` block is open on the connection: everything sent
    /// on it is queued until `EXEC`, so internal lookups use a side
    /// connection and the key browser waits.
    in_multi: bool,
    /// One entry per command queued in the open `MULTI` block: the
    /// database of a queued `SELECT`, `None` for any other command. `EXEC`
    /// applies a `SELECT` only when its element of the reply is `OK`; the
    /// tracked database is always one the connection is really on.
    queued: Vec<Option<u16>>,
    /// A console `WATCH` is active on the connection.
    watching: bool,
    /// This session's token in [`crate::watches`], where its watched keys
    /// are recorded for the other sessions.
    watch_token: u64,
    /// The connection was replaced while a `WATCH` was active: an `EXEC`
    /// must not commit without it. Cleared only by a `WATCH` or `UNWATCH`
    /// that the server confirmed.
    lost_watch: bool,
    /// The connection was replaced while a `MULTI` block was open: its
    /// queued commands are gone, and commands meant for it would run at
    /// once. Cleared by `MULTI` or `DISCARD`.
    lost_multi: bool,
}

impl Inner {
    /// The connection is being replaced: remembers a `WATCH` and a `MULTI`
    /// block that go with it and clears the transaction state.
    fn connection_replaced(&mut self) {
        self.lost_multi |= self.in_multi;
        self.lost_watch |= self.watching;
        self.in_multi = false;
        self.end_watch();
        self.queued.clear();
    }

    /// The console `WATCH` ended (or its connection went away).
    fn end_watch(&mut self) {
        self.watching = false;
        crate::watches::clear(self.watch_token);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        crate::watches::clear(self.watch_token);
    }
}

/// An open Redis connection. Calls are serialized; the connection is
/// re-opened transparently (AUTH, SELECT, CLIENT SETNAME and the init
/// commands again) after a cancel, a statement timeout, a reply above
/// [`MAX_REPLY_BYTES`] or a lost connection.
pub struct RedisSession {
    shared: Arc<Shared>,
    inner: tokio::sync::Mutex<Inner>,
    details: ServerDetails,
    read_only: bool,
    statement_timeout: Option<Duration>,
    init: Vec<ParsedCommand>,
}

impl RedisSession {
    /// Connects and reads the server identity.
    ///
    /// # Errors
    ///
    /// Connect, Auth, Tls, Timeout, or Query when an init command fails.
    pub async fn connect(params: &ConnectParams) -> Result<Self, DriverError> {
        let endpoint = Endpoint::new(params).await?;
        let init = match params.init_sql.as_deref() {
            Some(text) => check_init_script(text)?,
            None => Vec::new(),
        };
        let mut inner = Inner {
            conn: None,
            db: params.redis_db,
            commands: HashMap::new(),
            pages: PageStates::default(),
            in_multi: false,
            queued: Vec::new(),
            watching: false,
            watch_token: crate::watches::token(),
            lost_watch: false,
            lost_multi: false,
        };
        let shared = Arc::new(Shared {
            endpoint,
            notify: Notify::new(),
            kill_done: Notify::new(),
            side_opened: std::sync::atomic::AtomicU64::new(0),
            state: Mutex::new(CallState::default()),
        });
        // One deadline for the whole connect: TCP, TLS, AUTH, CLIENT ID,
        // the init commands and INFO.
        let (conn, details) = shared
            .endpoint
            .bounded_connect(async {
                let conn = open_session_conn(&shared, &mut inner, &init, true).await?;
                let details = conn::read_server(&conn).await?;
                Ok::<_, DriverError>((conn, details))
            })
            .await??;
        inner.conn = Some(conn);
        Ok(Self {
            shared,
            inner: tokio::sync::Mutex::new(inner),
            details,
            read_only: params.read_only,
            statement_timeout: params.statement_timeout,
            init,
        })
    }

    /// How many side connections the key browser has opened to measure
    /// entries too large to read (one per page that needed any), for
    /// diagnostics.
    #[must_use]
    pub fn measuring_connections(&self) -> u64 {
        self.shared
            .side_opened
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The endpoint, for side connections opened without a call.
    pub(crate) fn endpoint(&self) -> &Endpoint {
        &self.shared.endpoint
    }

    /// The init commands, for side connections opened without a call.
    pub(crate) fn init_commands(&self) -> &[ParsedCommand] {
        &self.init
    }

    /// True when the session talks to the server over TLS (with TLS mode
    /// `prefer`: when the server answered as a TLS server).
    #[must_use]
    pub fn uses_tls(&self) -> bool {
        self.shared.endpoint.uses_tls()
    }

    /// Names the server in the messages of this session from now on
    /// (reconnects, side connections, cancel connections) instead of the
    /// host and port it connects to. The app sets it for a session through
    /// an SSH tunnel, whose host and port are the tunnel's local end, for
    /// example `redis:6379 through SSH bastion bastion.example:22`.
    pub fn set_endpoint_label(&self, label: impl Into<String>) {
        self.shared.endpoint.set_label(label.into());
    }

    /// Redis or Valkey.
    #[must_use]
    pub fn flavor(&self) -> ServerFlavor {
        self.details.flavor
    }

    /// The Redis version the server is compatible with (`redis_version`;
    /// Valkey reports 7.2). Use it to gate features.
    #[must_use]
    pub fn compat_version(&self) -> (u32, u32) {
        self.details.compat
    }

    /// True when the session refuses writes.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Risk of a command, for the production confirmation. `name` is the
    /// command name, optionally followed by its subcommand (`CONFIG SET`,
    /// `config|set`) or a whole command line. `COMMAND INFO` is loaded once
    /// per command name and cached.
    ///
    /// # Errors
    ///
    /// Connection errors while loading `COMMAND INFO`.
    pub async fn command_risk(&self, name: &str) -> Result<CommandRisk, DriverError> {
        Ok(self.assess_name(name).await?.risk)
    }

    /// True when a read-only session runs the command: it is known to be
    /// read-only (see [`crate::READ_ONLY_EXTRAS`] and the classification
    /// rules there). `name` is as for [`RedisSession::command_risk`]. A
    /// command name the server has renamed is classified by its real name.
    ///
    /// # Errors
    ///
    /// Connection errors while loading `COMMAND INFO`.
    pub async fn read_only_allows(&self, name: &str) -> Result<bool, DriverError> {
        Ok(!self.assess_name(name).await?.writes)
    }

    async fn assess_name(&self, name: &str) -> Result<Assessment, DriverError> {
        let mut words = name
            .split(|c: char| c.is_whitespace() || c == '|')
            .filter(|w| !w.is_empty());
        let Some(command) = words.next() else {
            return Ok(Assessment {
                risk: CommandRisk::Read,
                writes: false,
            });
        };
        let sub = words.next();
        let mut call = self.begin().await?;
        call.assess(command, sub).await
    }

    /// Highest [`CommandRisk`] of all commands in a console script.
    ///
    /// # Errors
    ///
    /// Query errors for unparsable text, connection errors while loading
    /// `COMMAND INFO`.
    pub async fn script_risk(&self, text: &str) -> Result<CommandRisk, DriverError> {
        let commands = parse_script(text)?;
        let mut call = self.begin().await?;
        let mut highest = CommandRisk::Read;
        for command in &commands {
            let sub = command.sub();
            highest = highest.max(call.assess(&command.name(), sub.as_deref()).await?.risk);
        }
        Ok(highest)
    }

    /// The database the session is on.
    pub async fn current_db(&self) -> u16 {
        self.inner.lock().await.db
    }

    /// Switches the session to another logical database.
    ///
    /// # Errors
    ///
    /// Query errors from `SELECT` (for example an index out of range).
    pub async fn select_db(&self, db: u16) -> Result<(), DriverError> {
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        call.query(redis::cmd("SELECT").arg(db)).await?;
        call.inner.db = db;
        Ok(())
    }

    /// Starts a serialized call: takes the session lock, marks the session
    /// busy for cancel, and reconnects when needed.
    pub(crate) async fn begin(&self) -> Result<Call<'_>, DriverError> {
        let inner = self.inner.lock().await;
        {
            let mut state = self.shared.state();
            state.busy = true;
            state.cancelled = false;
            state.generation = state.generation.wrapping_add(1);
            state.kill = Kill::None;
        }
        let mut call = Call {
            session: self,
            inner,
            conn: None,
            budget: None,
            broken: false,
            too_large: false,
            in_flight: false,
            side: None,
        };
        if call.inner.conn.is_none() {
            // The connection was lost or closed: what was scanned on it may
            // be gone (a restart, a failover), so open scans start over.
            call.inner.pages.clear();
            call.reconnect().await?;
        } else {
            call.attach();
        }
        if self.shared.is_cancelled() {
            return Err(error::cancelled());
        }
        Ok(call)
    }
}

/// Opens a connection for the session on its current database, records its
/// client id and runs the init commands. Callers bound it with
/// [`Endpoint::bounded`].
/// On the first connect (`first`), an init `SELECT` decides the session's
/// database. On a reconnect the database the session is on (the user may
/// have switched it) is selected again after the init commands.
async fn open_session_conn(
    shared: &Shared,
    inner: &mut Inner,
    init: &[ParsedCommand],
    first: bool,
) -> Result<Connection, DriverError> {
    let endpoint = &shared.endpoint;
    let conn = endpoint.open(inner.db, endpoint.client_name()).await?;
    let id = conn::client_id(&conn).await?;
    for command in init {
        let typed = command.name();
        let spec = load_spec(&conn, &mut inner.commands, &typed).await?;
        // Fail closed: without a COMMAND INFO entry the real command
        // behind the name cannot be known (it may be renamed).
        let Some(spec) = spec else {
            return Err(init_error(
                command,
                &typed,
                "cannot be checked: the server does not describe it in COMMAND INFO (renamed \
                 away, not permitted, or unknown), so Varve cannot tell what it really runs",
            ));
        };
        let spec = Some(spec);
        let name = real_name(command, spec.as_ref());
        if let Some(problem) = init_command_problem(&name, &command.args) {
            return Err(init_error(command, &name, problem));
        }
        // Every init command is a connection setup command (the allowlist
        // above, by real name): it changes the connection, never data, so
        // read-only sessions accept all of them.
        conn.query(&to_cmd(&command.args)).await.map_err(|mut e| {
            e.message = format!("Init command on line {}: {}", command.line, e.message);
            e.kind = ErrorKind::Query;
            e
        })?;
        if first
            && name == "SELECT"
            && let Some(db) = command.args.get(1).and_then(|a| parse_u16(a))
        {
            inner.db = db;
        }
    }
    if !first && !init.is_empty() {
        conn.query(redis::cmd("SELECT").arg(inner.db)).await?;
    }
    shared.state().client_id = Some(id);
    Ok(conn)
}

async fn load_spec(
    conn: &Connection,
    cache: &mut HashMap<String, Option<CommandSpec>>,
    name: &str,
) -> Result<Option<CommandSpec>, DriverError> {
    let key = name.to_ascii_uppercase();
    if let Some(spec) = cache.get(&key) {
        return Ok(spec.clone());
    }
    let spec = match conn
        .query(redis::cmd("COMMAND").arg("INFO").arg(&key))
        .await
    {
        Ok(reply @ Resp::Array(_)) => parse_command_info(&reply),
        Ok(other) => {
            return Err(error::protocol(format!("COMMAND INFO returned {other:?}")));
        }
        // Not cached: an ACL change can make it available.
        Err(e) if command_info_unavailable(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    cache.insert(key, spec.clone());
    Ok(spec)
}

/// `COMMAND INFO` refused by an ACL or renamed away: classification then
/// falls back to the typed name and the generated tables.
fn command_info_unavailable(error: &DriverError) -> bool {
    match error.code.as_deref() {
        Some("NOPERM") => true,
        Some("ERR") => error
            .message
            .to_ascii_lowercase()
            .contains("unknown command"),
        _ => false,
    }
}

/// The real name of a command: from its `COMMAND INFO` entry, so a renamed
/// command is known as what it is; the typed name when there is none.
fn real_name(command: &ParsedCommand, spec: Option<&CommandSpec>) -> String {
    spec.map_or_else(|| command.name(), CommandSpec::command_name)
}

fn parse_command_info(reply: &Resp) -> Option<CommandSpec> {
    match reply {
        Resp::Array(entries) => entries.first().and_then(CommandSpec::parse),
        _ => None,
    }
}

fn parse_u16(bytes: &[u8]) -> Option<u16> {
    std::str::from_utf8(bytes).ok()?.trim().parse().ok()
}

/// Builds a command from raw arguments.
pub(crate) fn to_cmd(args: &[Vec<u8>]) -> redis::Cmd {
    let mut cmd = redis::Cmd::new();
    for arg in args {
        cmd.arg(arg.as_slice());
    }
    cmd
}

fn read_only_error(command: &ParsedCommand) -> DriverError {
    let mut error = DriverError::new(
        ErrorKind::ReadOnly,
        format!(
            "Line {}: {} changes data and this connection is read-only",
            command.line,
            command.name()
        ),
    );
    error.position = u32::try_from(command.position).ok();
    error
}

fn unsupported_in_console(command: &ParsedCommand) -> DriverError {
    let mut error = DriverError::new(
        ErrorKind::Unsupported,
        format!(
            "Line {}: {} is not available in the console",
            command.line,
            command.name()
        ),
    );
    error.position = u32::try_from(command.position).ok();
    error
}

/// One serialized call on the session. Dropping it ends the call: it clears
/// the busy flag and drops the connection when it was cancelled or broken,
/// so the next call reconnects.
pub(crate) struct Call<'s> {
    session: &'s RedisSession,
    inner: tokio::sync::MutexGuard<'s, Inner>,
    conn: Option<MultiplexedConnection>,
    budget: Option<Arc<ReadBudget>>,
    broken: bool,
    /// The last request failed because its reply was over
    /// [`MAX_REPLY_BYTES`].
    too_large: bool,
    /// A request was sent and its reply not read yet. When the call is
    /// dropped in that state (its future given up), what the server did is
    /// unknown (a `SELECT` or `MULTI` may have run), so the connection is
    /// dropped and the next call reconnects on the confirmed database.
    in_flight: bool,
    /// The side connection for measuring oversized entries, opened on
    /// first use and closed when the call (the page) ends.
    pub(crate) side: Option<RawConn>,
}

impl Call<'_> {
    /// Uses the session's live connection for this call.
    fn attach(&mut self) {
        self.conn = self.inner.conn.as_ref().map(Connection::handle);
        self.budget = self.inner.conn.as_ref().map(Connection::budget);
    }

    /// Replaces the connection with a new one (closing the old one), within
    /// the connect timeout. A cancel stops it at once.
    ///
    /// # Errors
    ///
    /// Connect errors, or Cancelled when the call was cancelled meanwhile.
    pub(crate) async fn reconnect(&mut self) -> Result<(), DriverError> {
        let session = self.session;
        self.conn = None;
        self.budget = None;
        self.inner.conn = None;
        self.inner.connection_replaced();
        session.shared.state().client_id = None;
        // A cancel ends a stalled reconnect at once instead of after the
        // connect timeout; nothing runs on the server yet.
        let notified = session.shared.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if session.shared.is_cancelled() {
            return Err(error::cancelled());
        }
        let opening = session.shared.endpoint.bounded_connect(open_session_conn(
            &session.shared,
            &mut self.inner,
            &session.init,
            false,
        ));
        let conn = tokio::select! {
            biased;
            () = &mut notified => return Err(error::cancelled()),
            opened = opening => opened??,
        };
        self.inner.conn = Some(conn);
        self.attach();
        self.broken = false;
        self.too_large = false;
        if session.shared.is_cancelled() {
            return Err(error::cancelled());
        }
        Ok(())
    }

    /// True when the last request failed because its reply was larger than
    /// [`MAX_REPLY_BYTES`]; the call must [`Call::reconnect`] before it
    /// sends anything else.
    pub(crate) fn reply_was_too_large(&self) -> bool {
        self.too_large
    }

    fn connection(&self) -> Result<MultiplexedConnection, DriverError> {
        self.conn
            .clone()
            .ok_or_else(|| DriverError::new(ErrorKind::ConnectionLost, "Not connected"))
    }

    /// Server identity details of the session.
    pub(crate) fn details(&self) -> &ServerDetails {
        &self.session.details
    }

    /// Counts a side connection opened for this call.
    pub(crate) fn count_side_connection(&self) {
        self.session
            .shared
            .side_opened
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// A console `MULTI` block is open.
    pub(crate) fn in_multi(&self) -> bool {
        self.inner.in_multi
    }

    /// A console `WATCH` of any session of this process holds `key` of the
    /// call's database on this server (see [`crate::watches`]).
    pub(crate) fn key_is_watched(&self, key: &[u8]) -> bool {
        crate::watches::is_watched(&self.endpoint().display(), self.db(), key)
    }

    /// Refuses key browser calls while a console `MULTI` block is open:
    /// their commands would be queued into the user's transaction.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Query`] with code `MULTI_OPEN`.
    pub(crate) fn no_open_transaction(&self) -> Result<(), DriverError> {
        if self.inner.in_multi {
            return Err(DriverError::new(
                ErrorKind::Query,
                "A MULTI transaction is open in the console; run EXEC or DISCARD there before \
                 browsing keys or switching databases",
            )
            .with_code("MULTI_OPEN"));
        }
        Ok(())
    }

    /// The session's init commands, for side connections.
    pub(crate) fn init(&self) -> &[ParsedCommand] {
        &self.session.init
    }

    /// The session's endpoint, for side connections.
    pub(crate) fn endpoint(&self) -> &Endpoint {
        &self.session.shared.endpoint
    }

    /// Runs `work`, which does not use the session connection (a side
    /// connection), under the call's cancel and statement timeout. Dropping
    /// `work` closes whatever it opened.
    ///
    /// # Errors
    ///
    /// The error of `work`, Cancelled or Timeout.
    pub(crate) async fn watch<T>(
        &self,
        work: impl Future<Output = Result<T, DriverError>>,
    ) -> Result<T, DriverError> {
        let shared = &self.session.shared;
        let notified = shared.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if shared.is_cancelled() {
            return Err(error::cancelled());
        }
        let timeout = self.session.statement_timeout;
        let timed = async move {
            match timeout {
                Some(limit) => tokio::time::timeout(limit, work)
                    .await
                    .map_err(|_| error::statement_timeout(limit))?,
                None => work.await,
            }
        };
        tokio::select! {
            biased;
            () = &mut notified => Err(error::cancelled()),
            outcome = timed => outcome,
        }
    }

    /// The logical database the call runs on.
    pub(crate) fn db(&self) -> u16 {
        self.inner.db
    }

    /// The session's page states.
    pub(crate) fn pages(&mut self) -> &mut PageStates {
        &mut self.inner.pages
    }

    /// Stores the rest of a scan step and returns its page cursor.
    pub(crate) fn keep_page_state(&mut self, state: ScanState) -> String {
        self.inner.pages.insert(state)
    }

    /// Sends one command and returns the raw reply; error replies come back
    /// as [`Resp::ServerError`]. Honors cancel and the statement timeout.
    pub(crate) async fn send(&mut self, cmd: &redis::Cmd) -> Result<Resp, DriverError> {
        let mut conn = self.connection()?;
        self.guard(async move { conn.send_packed_command(cmd).await })
            .await
    }

    /// Sends a pipeline and returns one reply per command, error replies
    /// included as [`Resp::ServerError`].
    pub(crate) async fn send_pipeline(
        &mut self,
        pipe: &redis::Pipeline,
    ) -> Result<Vec<Resp>, DriverError> {
        if pipe.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.connection()?;
        self.guard(async move { conn.send_packed_commands(pipe, 0, pipe.len()).await })
            .await
    }

    /// Sends one command and turns an error reply into a Query error.
    pub(crate) async fn query(&mut self, cmd: &redis::Cmd) -> Result<Resp, DriverError> {
        match self.send(cmd).await? {
            Resp::ServerError(e) => Err(query_error(&e)),
            reply => Ok(reply),
        }
    }

    async fn guard<T>(
        &mut self,
        request: impl Future<Output = redis::RedisResult<T>>,
    ) -> Result<T, DriverError> {
        let session = self.session;
        let shared = &session.shared;
        let notified = shared.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if shared.is_cancelled() {
            // Nothing was sent yet, so nothing runs on the server.
            return Err(error::cancelled());
        }
        if self.broken {
            return Err(DriverError::new(
                ErrorKind::ConnectionLost,
                "The connection was closed after the previous command; run it again",
            ));
        }
        self.too_large = false;
        let budget = self.budget.clone();
        if let Some(budget) = &budget {
            budget.reset(MAX_REPLY_BYTES);
        }
        let timeout = session.statement_timeout;
        let timed = async move {
            match timeout {
                Some(limit) => tokio::time::timeout(limit, request).await.ok(),
                None => Some(request.await),
            }
        };
        self.in_flight = true;
        let outcome = tokio::select! {
            biased;
            () = &mut notified => return Err(cancel_outcome(shared).await),
            outcome = timed => outcome,
        };
        self.in_flight = false;
        match outcome {
            None => {
                // Dropping the connection (when the call ends) closes the
                // socket, so a late reply can never be read as the reply to
                // a later command.
                self.broken = true;
                Err(error::statement_timeout(timeout.unwrap_or_default()))
            }
            Some(Ok(value)) => Ok(value),
            Some(Err(e)) => {
                if shared.is_cancelled() {
                    return Err(cancel_outcome(shared).await);
                }
                if budget.as_ref().is_some_and(|b| b.exceeded()) {
                    self.broken = true;
                    self.too_large = true;
                    return Err(error::reply_too_large());
                }
                let mapped = error::from_redis(&e);
                if matches!(
                    mapped.kind,
                    ErrorKind::ConnectionLost
                        | ErrorKind::Connect
                        | ErrorKind::Timeout
                        | ErrorKind::Protocol
                ) {
                    self.broken = true;
                }
                Err(mapped)
            }
        }
    }

    /// `COMMAND INFO` for `name`, cached.
    async fn spec(&mut self, name: &str) -> Result<Option<CommandSpec>, DriverError> {
        let key = name.to_ascii_uppercase();
        if let Some(spec) = self.inner.commands.get(&key) {
            return Ok(spec.clone());
        }
        // Inside a console MULTI block the lookup would be queued (and run
        // in the user's EXEC): it goes to a side connection instead.
        let reply = if self.inner.in_multi {
            crate::measure::command_info(self, &key).await
        } else {
            self.query(redis::cmd("COMMAND").arg("INFO").arg(&key))
                .await
        };
        let spec = match reply {
            Ok(reply @ Resp::Array(_)) => parse_command_info(&reply),
            // Never cached: a reply of another shape (such as QUEUED) or a
            // refusal says nothing lasting about the command.
            Ok(other) => {
                return Err(error::protocol(format!("COMMAND INFO returned {other:?}")));
            }
            Err(e) if command_info_unavailable(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        self.inner.commands.insert(key, spec.clone());
        Ok(spec)
    }

    async fn assess(&mut self, name: &str, sub: Option<&str>) -> Result<Assessment, DriverError> {
        let spec = self.spec(name).await?;
        Ok(assess(name, sub, spec.as_ref()))
    }

    /// Sets the database to re-select after a reconnect.
    pub(crate) fn set_db(&mut self, db: u16) {
        self.inner.db = db;
    }
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        let cancelled = {
            let mut state = self.session.shared.state();
            let cancelled = state.cancelled;
            state.busy = false;
            state.cancelled = false;
            if cancelled || self.broken || self.in_flight {
                state.client_id = None;
            }
            cancelled
        };
        if cancelled || self.broken || self.in_flight {
            // A new connection has no open transaction.
            self.inner.conn = None;
            self.inner.connection_replaced();
        }
    }
}

/// A console error reply: always [`ErrorKind::Query`] with the error prefix
/// as code.
fn query_error(e: &redis::ServerError) -> DriverError {
    DriverError::new(ErrorKind::Query, server_error_text(e)).with_code(e.code())
}

#[async_trait::async_trait]
impl Session for RedisSession {
    fn server_info(&self) -> &ServerInfo {
        &self.details.info
    }

    fn tls_in_use(&self) -> Option<bool> {
        Some(self.uses_tls())
    }

    /// `AUTH` sends the password as is.
    fn password_in_clear(&self) -> bool {
        !self.uses_tls() && self.shared.endpoint.password().is_some()
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(RedisCancel {
            shared: Arc::clone(&self.shared),
        })
    }

    async fn execute(&self, sql: &str, options: &ExecOptions) -> Result<ExecOutput, DriverError> {
        let started = Instant::now();
        let commands = parse_script(sql)?;
        let mut call = self.begin().await?;
        // Checks use the real command names, so renamed commands cannot
        // slip past them.
        let mut real_names = Vec::with_capacity(commands.len());
        for command in &commands {
            let typed = command.name();
            let sub = command.sub();
            let spec = call.spec(&typed).await?;
            let real = real_name(command, spec.as_ref());
            if CONSOLE_UNSUPPORTED.contains(&real.as_str())
                || (real == "CLIENT" && sub.as_deref() == Some("REPLY"))
            {
                return Err(unsupported_in_console(command));
            }
            if real == "HELLO"
                && let Some(problem) = varve_redis_syntax::hello_problem(&command.args)
            {
                // The arguments are never echoed: they may hold a password.
                let mut error = DriverError::new(
                    ErrorKind::Unsupported,
                    format!(
                        "Line {}: HELLO with these arguments is not available in the console: \
                         it {problem}; use HELLO 2 or HELLO without arguments",
                        command.line
                    ),
                );
                error.position = u32::try_from(command.position).ok();
                return Err(error);
            }
            if real == "SELECT" && untrackable_db(command) {
                let mut error = DriverError::new(
                    ErrorKind::Unsupported,
                    format!(
                        "Line {}: Varve follows database numbers up to {} only",
                        command.line,
                        u16::MAX
                    ),
                );
                error.position = u32::try_from(command.position).ok();
                return Err(error);
            }
            if self.read_only && assess(&typed, sub.as_deref(), spec.as_ref()).writes {
                return Err(read_only_error(command));
            }
            real_names.push(real);
        }
        check_lost_transaction(&mut call, &commands, &real_names).await?;
        let mut results = Vec::with_capacity(commands.len());
        let mut notices = Vec::new();
        let mut script = ScriptState {
            in_multi: call.inner.in_multi,
            ..ScriptState::default()
        };
        for (command, real) in commands.iter().zip(&real_names) {
            let located = |mut error: DriverError| {
                if commands.len() > 1 {
                    error.detail = Some(format!("Line {}", command.line));
                }
                error.position = u32::try_from(command.position).ok();
                error
            };
            // `Ok(Err(..))`: the server answered with an error;
            // `Err(..)`: no answer was read (cancel, timeout, a reply too
            // large, a lost connection).
            // EXEC and DISCARD end the block (and a WATCH) whatever their
            // answer, also when the call is given up before it arrives:
            // there is nothing left to protect after them.
            // (The queued SELECTs stay: the EXEC reply applies them.)
            if script.in_multi && matches!(real.as_str(), "EXEC" | "DISCARD") {
                call.inner.in_multi = false;
                call.inner.end_watch();
            }
            // DISCARD after a lost block: the server already dropped it, so
            // it is answered here without being sent.
            if real == "DISCARD" && !script.in_multi && call.inner.lost_multi {
                call.inner.lost_multi = false;
                let index = script.tracker.completed_count();
                let effect = script.effect(real, &Resp::Okay, index);
                script.completed(effect);
                let mut message = String::from(
                    "DISCARD was not sent: the connection was reset, which already dropped the \
                     MULTI block and its queued commands",
                );
                if call.inner.lost_watch {
                    message.push_str(
                        ". The WATCH was lost with it: run WATCH again (or UNWATCH) before the \
                         next transaction",
                    );
                }
                notices.push(Notice {
                    severity: "NOTICE".into(),
                    message,
                });
                results.push(StatementResult::Command {
                    tag: "OK".into(),
                    rows_affected: None,
                });
                continue;
            }
            let reply = match call.send(&to_cmd(&command.args)).await {
                Ok(Resp::ServerError(e)) => Ok(Err(located(query_error(&e)))),
                Ok(reply) => Ok(Ok(reply)),
                Err(error) => Err(error),
            };
            let reply = match reply {
                Ok(Ok(reply)) => reply,
                Ok(Err(error)) => {
                    // An error reply to EXEC (EXECABORT) or DISCARD ends the
                    // block without running it; any other error leaves the
                    // queue as it was.
                    let end = if matches!(real.as_str(), "EXEC" | "DISCARD") {
                        MultiEnd::Discarded
                    } else {
                        MultiEnd::Open
                    };
                    // A broken connection is replaced when the call ends,
                    // which records the transaction it loses.
                    call.inner.in_multi = script.in_multi && end == MultiEnd::Open;
                    if !call.inner.in_multi {
                        call.inner.queued.clear();
                    }
                    return Err(script.fail(error, results, end, started));
                }
                Err(error) => {
                    // The call drops the connection, and the server drops a
                    // queue that was never executed with it. An EXEC whose
                    // answer was not read may have run.
                    let end = if real == "EXEC" {
                        MultiEnd::Unknown
                    } else {
                        MultiEnd::Discarded
                    };
                    // An EXEC or DISCARD without an answer ended the block
                    // (whether it ran is unknown); any other command leaves
                    // it as it was, and the replaced connection records it.
                    // (Outside a block, EXEC and DISCARD only fail and leave a
                    // WATCH in place: the replaced connection records it.)
                    if script.in_multi && matches!(real.as_str(), "EXEC" | "DISCARD") {
                        call.inner.in_multi = false;
                        call.inner.end_watch();
                        call.inner.queued.clear();
                    } else {
                        call.inner.in_multi = script.in_multi;
                    }
                    return Err(script.fail(error, results, end, started));
                }
            };
            let was_in_multi = script.in_multi;
            track_database(&mut call, real, command, &reply, was_in_multi);
            let index = script.tracker.completed_count();
            let effect = script.effect(real, &reply, index);
            // The server closes the connection after QUIT (unless a MULTI
            // block only queued it).
            if real == "QUIT" && !matches!(&reply, Resp::SimpleString(t) if t == "QUEUED") {
                call.broken = true;
            }
            let result = match to_result(reply, options.max_rows) {
                Ok(result) => result,
                // The command ran; only its reply could not be shown.
                Err(error) => {
                    script.completed(effect);
                    call.inner.in_multi = script.in_multi;
                    let end = if call.broken {
                        MultiEnd::Discarded
                    } else {
                        MultiEnd::Open
                    };
                    return Err(script.fail(located(error), results, end, started));
                }
            };
            script.completed(effect);
            results.push(result);
        }
        call.inner.in_multi = script.in_multi;
        Ok(ExecOutput {
            results,
            notices,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }

    async fn ping(&self) -> Result<(), DriverError> {
        let mut call = self.begin().await?;
        if call.in_multi() {
            // PING on the connection would be queued into the user's
            // transaction: ask on a side connection instead.
            return crate::measure::ping(&mut call).await;
        }
        match call.query(&redis::cmd("PING")).await? {
            Resp::SimpleString(s) if s == "PONG" => Ok(()),
            other => Err(error::protocol(format!("PING returned {other:?}"))),
        }
    }

    async fn close(self: Box<Self>) -> Result<(), DriverError> {
        let mut inner = self.inner.lock().await;
        // Dropping the connection stops its task and closes the socket.
        inner.conn = None;
        inner.in_multi = false;
        inner.end_watch();
        inner.lost_watch = false;
        inner.lost_multi = false;
        inner.queued.clear();
        inner.pages.clear();
        self.shared.state().client_id = None;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Follows the database the connection is on after `command` (real name
/// `real`) got `reply`. A `SELECT` outside a `MULTI` block counts when the
/// server answered `OK`; one queued inside a block only when its element
/// of the `EXEC` reply is `OK` (`DISCARD`, an aborted or failed `EXEC`
/// leave the database as it was).
fn track_database(
    call: &mut Call<'_>,
    real: &str,
    command: &ParsedCommand,
    reply: &Resp,
    in_multi: bool,
) {
    let ok = |reply: &Resp| match reply {
        Resp::Okay => true,
        Resp::SimpleString(text) => text == "OK",
        _ => false,
    };
    let selected = || command.args.get(1).and_then(|a| parse_u16(a));
    match real {
        // Only a WATCH or UNWATCH the server confirmed (outside a block,
        // where it is not just queued) changes the WATCH state.
        "WATCH" if !in_multi && ok(reply) => {
            call.inner.watching = true;
            call.inner.lost_watch = false;
            let server = call.endpoint().display();
            let db = call.db();
            crate::watches::add(
                call.inner.watch_token,
                &server,
                db,
                command.args.get(1..).unwrap_or_default(),
            );
        }
        "UNWATCH" if !in_multi && ok(reply) => {
            call.inner.end_watch();
            call.inner.lost_watch = false;
        }
        // EXEC and DISCARD of a block end its WATCH whatever their outcome.
        "EXEC" | "DISCARD" if in_multi => call.inner.end_watch(),
        "MULTI" if !in_multi && ok(reply) => call.inner.lost_multi = false,
        _ => {}
    }
    if !in_multi {
        if real == "SELECT"
            && ok(reply)
            && let Some(db) = selected()
        {
            call.set_db(db);
        }
        if real == "MULTI" && ok(reply) {
            call.inner.queued.clear();
        }
        return;
    }
    match real {
        "EXEC" => {
            let queued = std::mem::take(&mut call.inner.queued);
            if let Resp::Array(elements) = reply {
                for (entry, element) in queued.iter().zip(elements) {
                    if let Some(db) = entry
                        && ok(element)
                    {
                        call.set_db(*db);
                    }
                }
            }
        }
        "DISCARD" => call.inner.queued.clear(),
        _ if matches!(reply, Resp::SimpleString(text) if text == "QUEUED") => {
            let entry = if real == "SELECT" { selected() } else { None };
            call.inner.queued.push(entry);
        }
        _ => {}
    }
}

/// True for `SELECT n` with `n` above what the session can follow.
fn untrackable_db(command: &ParsedCommand) -> bool {
    command
        .args
        .get(1)
        .and_then(|a| std::str::from_utf8(a).ok())
        .and_then(|t| t.trim().parse::<u64>().ok())
        .is_some_and(|n| n > u64::from(u16::MAX))
}

/// After a connection was replaced while a `WATCH` or a `MULTI` block was
/// active, refuses (before anything is sent) an `EXEC` that would commit
/// without them, and, after a lost block, commands that were meant to be
/// queued in it. Follows the script as if every command succeeds: a
/// `WATCH` or `UNWATCH` outside a block lifts a lost WATCH (the server
/// confirms it before the `EXEC` runs; an error stops the script first),
/// `MULTI` lifts a lost block, and a `DISCARD` after a lost block is
/// answered locally. An `EXEC` refused this way also discards a block the
/// user opened on the new connection.
async fn check_lost_transaction(
    call: &mut Call<'_>,
    commands: &[ParsedCommand],
    real_names: &[String],
) -> Result<(), DriverError> {
    let mut lost_watch = call.inner.lost_watch;
    let mut lost_multi = call.inner.lost_multi;
    let mut block = call.inner.in_multi;
    for (command, real) in commands.iter().zip(real_names) {
        if !lost_watch && !lost_multi {
            break;
        }
        let refuse = |message: String| {
            let mut error = DriverError::new(
                ErrorKind::Query,
                format!("Line {}: {message}", command.line),
            )
            .with_code("TRANSACTION_RESET");
            error.position = u32::try_from(command.position).ok();
            error
        };
        match real.as_str() {
            "WATCH" | "UNWATCH" if !block => lost_watch = false,
            "MULTI" if !block => {
                block = true;
                lost_multi = false;
            }
            "EXEC" => {
                let (what, todo) = match (lost_watch, lost_multi) {
                    (true, true) => (
                        "the WATCH and the MULTI block with its queued commands were lost",
                        "Run WATCH again (or UNWATCH), then MULTI and the transaction.",
                    ),
                    (true, false) => (
                        "the WATCH was lost",
                        "Run WATCH again (or UNWATCH) and then the transaction.",
                    ),
                    _ => (
                        "the MULTI block and its queued commands were lost (nothing ran)",
                        "Start the transaction again with MULTI.",
                    ),
                };
                // A block lost with the connection is now reported; a lost
                // WATCH stays until WATCH or UNWATCH.
                call.inner.lost_multi = false;
                let mut discarded = "";
                if call.inner.in_multi {
                    call.query(&redis::cmd("DISCARD")).await?;
                    call.inner.in_multi = false;
                    call.inner.queued.clear();
                    discarded = " The commands queued since were discarded.";
                }
                return Err(refuse(format!(
                    "EXEC was not sent: the connection was reset, so {what}. {todo}{discarded}"
                )));
            }
            "DISCARD" if block => block = false,
            "DISCARD" => lost_multi = false,
            _ if lost_multi && !block => {
                let watch = if lost_watch {
                    " The WATCH was lost too: run WATCH again (or UNWATCH) before the next \
                     transaction."
                } else {
                    ""
                };
                return Err(refuse(format!(
                    "{real} was not sent: the connection was reset while a MULTI block was \
                     open, so it would run at once instead of being queued. Start the \
                     transaction again with MULTI, or run DISCARD to leave it (Varve answers \
                     it, the server already dropped the block).{watch}"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// What the commands of one console script did to a `MULTI` block.
#[derive(Default)]
struct ScriptState {
    tracker: TransactionTracker,
    /// A `MULTI` block is open: commands are queued until `EXEC`.
    in_multi: bool,
    /// Indices of this script's commands queued in the open block.
    queued: Vec<usize>,
    /// Indices of queued commands whose element of the `EXEC` reply was an
    /// error: they ran and changed nothing.
    failed: Vec<usize>,
}

/// What a failure did to an open `MULTI` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultiEnd {
    /// The block is still open on the connection.
    Open,
    /// The queued commands were dropped without running: an `EXECABORT`,
    /// `DISCARD`, or a connection closed before `EXEC` was sent.
    Discarded,
    /// `EXEC` was sent and its answer not read: the server may have run
    /// the queued commands.
    Unknown,
}

impl ScriptState {
    /// What a command that got `reply` did to the transaction, and the
    /// `MULTI` state after it.
    /// `index` is the command's index in the script.
    fn effect(&mut self, real: &str, reply: &Resp, index: usize) -> TransactionEffect {
        match real {
            "MULTI" => {
                self.in_multi = true;
                self.queued.clear();
                TransactionEffect::Begin {
                    commits_open: false,
                }
            }
            "EXEC" => {
                self.in_multi = false;
                let queued = std::mem::take(&mut self.queued);
                match reply {
                    // A nil reply: a watched key changed and nothing ran.
                    Resp::Nil => TransactionEffect::Rollback { chain: false },
                    // One element per queued command, those of an earlier
                    // call first: an error element changed nothing.
                    Resp::Array(elements) if elements.len() >= queued.len() => {
                        let ours = &elements[elements.len() - queued.len()..];
                        self.failed.extend(
                            queued
                                .iter()
                                .zip(ours)
                                .filter(|(_, element)| matches!(element, Resp::ServerError(_)))
                                .map(|(index, _)| *index),
                        );
                        TransactionEffect::Commit { chain: false }
                    }
                    _ => TransactionEffect::Commit { chain: false },
                }
            }
            "DISCARD" => {
                self.in_multi = false;
                self.queued.clear();
                TransactionEffect::Rollback { chain: false }
            }
            _ => {
                if self.in_multi {
                    self.queued.push(index);
                }
                TransactionEffect::Statement
            }
        }
    }

    /// Records the next command as completed.
    fn completed(&mut self, effect: TransactionEffect) {
        self.tracker.completed(effect, Some(self.in_multi));
    }

    /// `error` with what the script produced before it failed; `end` says
    /// what the failure did to an open `MULTI` block.
    fn fail(
        self,
        error: DriverError,
        results: Vec<StatementResult>,
        end: MultiEnd,
        started: Instant,
    ) -> DriverError {
        let (open_after, ended) = match end {
            MultiEnd::Open => (self.in_multi, Durability::RolledBack),
            MultiEnd::Discarded => (false, Durability::RolledBack),
            MultiEnd::Unknown => (false, Durability::Unknown),
        };
        let failed_statement = u32::try_from(self.tracker.completed_count()).ok();
        let mut completed = self.tracker.finish(Some(open_after), ended);
        for index in self.failed {
            if let Some(durability) = completed.get_mut(index) {
                *durability = Durability::Failed;
            }
        }
        let partial = PartialOutput {
            failed_statement,
            completed,
            results,
            notices: Vec::new(),
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        error.with_partial(partial)
    }
}

/// The error for a cancelled call that was running on the server. Waits up
/// to [`KILL_REPORT_WAIT`] for the server-side kill, so the error can say
/// when the server may still be running the command.
async fn cancel_outcome(shared: &Shared) -> DriverError {
    let deadline = Deadline::now() + KILL_REPORT_WAIT;
    loop {
        let reported = shared.kill_done.notified();
        tokio::pin!(reported);
        reported.as_mut().enable();
        let kill = shared.state().kill.clone();
        match kill {
            Kill::Pending => {}
            Kill::Failed(reason) => return error::cancelled_client_side(reason),
            Kill::None | Kill::Done => return error::cancelled(),
        }
        if tokio::time::timeout_at(deadline, reported).await.is_err() {
            return error::cancelled_client_side(
                "The request to stop the command on the server did not finish in time",
            );
        }
    }
}

/// Cancels the running call. The call stops waiting for its reply at once
/// and returns when the kill below has reported, or after at most
/// [`KILL_REPORT_WAIT`]. Its connection is killed from a short-lived second
/// connection
/// (`CLIENT KILL ID`), which also releases blocking commands such as
/// `BLPOP`. When the server refuses the kill (an ACL user without
/// `CLIENT KILL`) the call is still cancelled and its connection closed;
/// its error then says that the server may keep running the command.
struct RedisCancel {
    shared: Arc<Shared>,
}

#[async_trait::async_trait]
impl CancelHandle for RedisCancel {
    async fn cancel(&self) -> Result<(), DriverError> {
        let (client_id, generation) = {
            let mut state = self.shared.state();
            if !state.busy || state.cancelled {
                return Ok(());
            }
            state.cancelled = true;
            state.kill = if state.client_id.is_some() {
                Kill::Pending
            } else {
                Kill::None
            };
            (state.client_id, state.generation)
        };
        self.shared.notify.notify_waiters();
        let Some(id) = client_id else {
            return Ok(());
        };
        let outcome = kill_client(&self.shared.endpoint, id).await;
        {
            let mut state = self.shared.state();
            if state.generation == generation {
                state.kill = match &outcome {
                    Ok(()) => Kill::Done,
                    Err(e) => Kill::Failed(e.message.clone()),
                };
            }
        }
        self.shared.kill_done.notify_waiters();
        if let Err(e) = outcome {
            tracing::debug!(
                client_id = id,
                error = %e.message,
                "server-side cancel failed; the call was cancelled client side"
            );
        }
        Ok(())
    }
}

/// `CLIENT KILL ID <id>` from a new connection, within the connect timeout.
async fn kill_client(endpoint: &Endpoint, id: i64) -> Result<(), DriverError> {
    let name = format!("{}-cancel", endpoint.client_name());
    endpoint
        .bounded_connect(async {
            let side = endpoint.open(0, &name).await?;
            let reply = side
                .query(redis::cmd("CLIENT").arg("KILL").arg("ID").arg(id))
                .await?;
            tracing::debug!(client_id = id, killed = ?as_int(&reply), "cancelled Redis call");
            Ok(())
        })
        .await?
}
