//! `LISTEN` / `NOTIFY` on a dedicated connection (design doc 4.2,
//! "Notifications panel per session").
//!
//! A [`PgListener`] owns one connection of its own, never a user's session:
//! notifications are only delivered to a backend between transactions, so a
//! transaction the user leaves open would hold them back, and the panel must
//! keep receiving while the user's query runs. The connection is opened like
//! a session (TLS, the session settings, read-only mode, the statement
//! timeout) except for the profile's init SQL, which does not run: the
//! listener only runs its own fixed statements, which depend on neither the
//! search path nor the role, and an init SQL that leaves a transaction open
//! would hold back every notification (and one with side effects would run
//! again on every reconnect).
//!
//! - Channels are quoted identifiers ([`quote_channel`]), so a name is used
//!   exactly as given (case and spaces kept). Names the server would
//!   truncate (over [`MAX_CHANNEL_BYTES`] bytes) are refused.
//! - Notifications arrive with their channel, payload and the sending
//!   backend's process id, and are marked when this connection sent them.
//!   They go into a bounded buffer ([`ListenerOptions::buffer`]); when the
//!   reader falls behind, new ones are dropped and counted
//!   ([`PgListener::take_dropped`]). The connection is never stalled: a
//!   listener that stops reading would keep the server's notification
//!   queue from being cleaned up for every client.
//! - A lost connection (also one that stops answering the periodic ping)
//!   is opened again with exponential backoff, from a fresh
//!   [`ListenTarget`] (new tunnel, current secrets), and every channel is
//!   listened to again. Notifications sent while it was down are lost: the
//!   server keeps none for a listener that is gone. Authentication, TLS and
//!   missing database errors stop the listener instead of retrying.
//! - [`PgListener::notify`] sends with `pg_catalog.pg_notify($1, $2)`
//!   (qualified, values as parameters) on the same connection, outside any
//!   user transaction, so it is delivered at once. A read-only listener
//!   refuses it: the server allows `NOTIFY` in read-only transactions.

use std::any::Any;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, SystemTime};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, SimpleQueryMessage};
use varve_driver::{ConnectParams, DriverError, ErrorKind, ServerInfo};

use crate::error::{Interrupt, exec_error};
use crate::session::open;
use crate::tap::Shared;

/// Longest channel name in bytes: PostgreSQL truncates identifiers to
/// `NAMEDATALEN - 1` bytes, which would listen to another name.
pub const MAX_CHANNEL_BYTES: usize = 63;

/// Longest payload in bytes the server accepts (it refuses 8000 and more).
pub const MAX_PAYLOAD_BYTES: usize = 7999;

/// Most channels one listener listens to.
pub const MAX_CHANNELS: usize = 256;

/// How long closing waits for the server to see the connection end.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// A notification received on a listened channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The channel, exactly as the sender named it.
    pub channel: String,
    /// The payload (empty when none was given).
    pub payload: String,
    /// Process id of the backend that sent it.
    pub process_id: i32,
    /// Sent by this listener's own connection ([`PgListener::notify`]).
    pub from_self: bool,
    /// The channel or payload was not valid UTF-8 (possible on a
    /// `SQL_ASCII` database) and was decoded lossily: invalid bytes became
    /// U+FFFD.
    pub lossy: bool,
    /// When the listener received it.
    pub received_at: SystemTime,
}

/// What a queued notification counts against a [`QueueBudget`].
fn queued_cost(note: &Notification) -> usize {
    note.channel.len() + note.payload.len() + 64
}

/// A limit on the bytes of received notifications waiting for their
/// readers, shared by any number of listeners (one per tab in the app).
/// A notification that does not fit is dropped and counted like one that
/// does not fit the listener's own buffer.
#[derive(Debug)]
pub struct QueueBudget {
    max_bytes: usize,
    used: AtomicUsize,
}

impl QueueBudget {
    /// A budget of `max_bytes`.
    #[must_use]
    pub fn new(max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            max_bytes,
            used: AtomicUsize::new(0),
        })
    }

    /// Bytes in use right now.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    fn reserve(&self, bytes: usize) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let Some(total) = used.checked_add(bytes).filter(|t| *t <= self.max_bytes) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(used, total, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(now) => used = now,
            }
        }
    }

    fn release(&self, bytes: usize) {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            match self.used.compare_exchange_weak(
                used,
                used.saturating_sub(bytes),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(now) => used = now,
            }
        }
    }
}

/// What a listener connects with: the parameters, and whatever must stay
/// alive while the connection is open (an SSH tunnel lease).
pub struct ListenTarget {
    /// Connection parameters. The init SQL is ignored (see the module
    /// documentation).
    pub params: ConnectParams,
    /// Kept until the connection made with `params` has closed.
    pub guard: Option<Box<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for ListenTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenTarget")
            .field("host", &self.params.host)
            .field("port", &self.params.port)
            .field("database", &self.params.database)
            .field("guard", &self.guard.is_some())
            .finish_non_exhaustive()
    }
}

/// Why a [`ListenConnector`] could not give a target.
#[derive(Debug, Clone, PartialEq)]
pub enum TargetError {
    /// Worth another try later (the tunnel could not be opened, say).
    Retry(DriverError),
    /// The listener must stop (the profile changed, its tab closed, the
    /// password is no longer known).
    Stop(DriverError),
}

/// Gives the listener what to connect with, for every reconnect.
#[async_trait::async_trait]
pub trait ListenConnector: Send + Sync {
    /// The parameters (and guard) for the next connection.
    async fn target(&self) -> Result<ListenTarget, TargetError>;
}

/// Tuning of a listener.
#[derive(Debug, Clone)]
pub struct ListenerOptions {
    /// Notifications buffered for the reader; more are dropped and counted.
    pub buffer: usize,
    /// A byte limit shared with other listeners on what waits for readers.
    pub budget: Option<Arc<QueueBudget>>,
    /// Wait before the first reconnect attempt; doubled per failed attempt.
    pub backoff_initial: Duration,
    /// Longest wait between reconnect attempts.
    pub backoff_max: Duration,
    /// How often an idle connection is checked (a broken network does not
    /// close a socket that sends nothing).
    pub ping_interval: Duration,
    /// Longest wait for one statement of the listener (LISTEN, UNLISTEN,
    /// the ping, a NOTIFY); a connection that does not answer in time is
    /// treated as lost.
    pub call_timeout: Duration,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            buffer: 10_000,
            budget: None,
            backoff_initial: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            ping_interval: Duration::from_secs(30),
            call_timeout: Duration::from_secs(10),
        }
    }
}

impl ListenerOptions {
    /// The wait before reconnect attempt `attempt` (1 based).
    #[must_use]
    pub fn backoff(&self, attempt: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempt.saturating_sub(1).min(31));
        self.backoff_initial
            .saturating_mul(factor)
            .min(self.backoff_max)
    }
}

/// A change of the listener's connection.
#[derive(Debug, Clone, PartialEq)]
pub enum ListenerStatus {
    /// Connected, and listening to every channel.
    Connected {
        /// Process id of the listener's backend.
        process_id: i32,
        /// The server.
        server: ServerInfo,
        /// True after a reconnect (notifications sent while it was down
        /// were lost).
        reconnected: bool,
    },
    /// The connection was lost (or an attempt to reopen it failed); the
    /// next attempt follows after `retry_in`.
    Reconnecting {
        /// Why.
        error: DriverError,
        /// Failed reconnect attempts since the connection was lost: 0 when
        /// it was just lost, then 1, 2, ...
        attempt: u32,
        /// Wait before the next attempt.
        retry_in: Duration,
    },
    /// The listener ended: closed (`error` is `None`), or it gave up.
    Stopped {
        /// Why it gave up.
        error: Option<DriverError>,
    },
}

/// What a listener hands its reader.
#[derive(Debug)]
pub struct ListenerEvents {
    /// Received notifications, oldest first.
    notifications: mpsc::Receiver<Notification>,
    budget: Option<Arc<QueueBudget>>,
    /// Connection changes. The first is the `Connected` of the start.
    pub status: mpsc::UnboundedReceiver<ListenerStatus>,
}

impl ListenerEvents {
    fn taken(&self, note: Option<Notification>) -> Option<Notification> {
        if let (Some(note), Some(budget)) = (&note, &self.budget) {
            budget.release(queued_cost(note));
        }
        note
    }

    /// The next notification; `None` once the listener has stopped and
    /// every notification was read.
    pub async fn recv(&mut self) -> Option<Notification> {
        let note = self.notifications.recv().await;
        self.taken(note)
    }

    /// The next notification when one is waiting.
    pub fn try_recv(&mut self) -> Option<Notification> {
        let note = self.notifications.try_recv().ok();
        self.taken(note)
    }

    /// True when no notification is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.notifications.is_empty()
    }
}

impl Drop for ListenerEvents {
    fn drop(&mut self) {
        // Give the shared budget back for what nobody will read.
        self.notifications.close();
        while self.try_recv().is_some() {}
    }
}

/// Checks a channel name: not empty, no NUL, at most
/// [`MAX_CHANNEL_BYTES`] bytes.
///
/// # Errors
/// A message saying what is wrong.
pub fn validate_channel(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("The channel name is empty.".into());
    }
    if name.contains('\0') {
        return Err("The channel name contains a NUL character.".into());
    }
    if name.len() > MAX_CHANNEL_BYTES {
        return Err(format!(
            "The channel name is {} bytes long; PostgreSQL allows at most {MAX_CHANNEL_BYTES}.",
            name.len()
        ));
    }
    Ok(())
}

/// Checks a payload: no NUL, at most [`MAX_PAYLOAD_BYTES`] bytes.
///
/// # Errors
/// A message saying what is wrong.
pub fn validate_payload(payload: &str) -> Result<(), String> {
    if payload.contains('\0') {
        return Err("The payload contains a NUL character.".into());
    }
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(format!(
            "The payload is {} bytes long; PostgreSQL allows at most {MAX_PAYLOAD_BYTES}.",
            payload.len()
        ));
    }
    Ok(())
}

/// The channel name as a quoted identifier (`"name"`, with `"` doubled).
#[must_use]
pub fn quote_channel(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn invalid(message: String) -> DriverError {
    DriverError::new(ErrorKind::Query, message)
}

fn stopped_error() -> DriverError {
    DriverError::new(ErrorKind::ConnectionLost, "The listener has stopped.")
}

/// True when another connection attempt may help: everything but rejected
/// credentials and a missing database. TLS and network errors are retried
/// (a handshake cut off by a server restart, a server that comes back with
/// TLS); a closed tab or a moved profile is the connector's
/// [`TargetError::Stop`].
fn transient(error: &DriverError) -> bool {
    if let Some(code) = error.code.as_deref()
        && (code == "3D000" || code.starts_with("28"))
    {
        return false;
    }
    error.kind != ErrorKind::Auth
}

enum Command {
    Listen(String, oneshot::Sender<Result<(), DriverError>>),
    Unlisten(String, oneshot::Sender<Result<(), DriverError>>),
    Notify(String, String, oneshot::Sender<Result<(), DriverError>>),
}

/// What the listener's handle reads without asking the supervisor.
#[derive(Debug, Default)]
struct Snapshot {
    channels: BTreeSet<String>,
    stopped: bool,
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running listener. Dropping it closes the connection.
#[derive(Debug)]
pub struct PgListener {
    commands: mpsc::Sender<Command>,
    closed: watch::Sender<bool>,
    dropped: Arc<AtomicU64>,
    snapshot: Arc<StdMutex<Snapshot>>,
    supervisor: StdMutex<Option<JoinHandle<()>>>,
}

impl PgListener {
    /// Connects with `first` and starts listening (to no channel yet).
    /// Later reconnects ask `connector` for their target.
    ///
    /// # Errors
    /// The first connection failed (nothing keeps running then).
    pub async fn start(
        first: ListenTarget,
        connector: Arc<dyn ListenConnector>,
        options: ListenerOptions,
    ) -> Result<(Self, ListenerEvents), DriverError> {
        let (notify_tx, notify_rx) = mpsc::channel(options.buffer.max(1));
        let budget = options.budget.clone();
        let (status_tx, status_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::channel(64);
        let (closed_tx, closed_rx) = watch::channel(false);
        let dropped = Arc::new(AtomicU64::new(0));
        let snapshot = Arc::new(StdMutex::new(Snapshot::default()));
        let supervisor = Supervisor {
            connector,
            options,
            commands: command_rx,
            closed: closed_rx,
            status: status_tx,
            notifications: notify_tx,
            dropped: Arc::clone(&dropped),
            desired: BTreeSet::new(),
            snapshot: Arc::clone(&snapshot),
        };
        let conn = supervisor.connect(first).await?;
        supervisor.send_status(ListenerStatus::Connected {
            process_id: conn.process_id,
            server: conn.info.clone(),
            reconnected: false,
        });
        let task = tokio::spawn(supervisor.run(conn));
        Ok((
            Self {
                commands: command_tx,
                closed: closed_tx,
                dropped,
                snapshot,
                supervisor: StdMutex::new(Some(task)),
            },
            ListenerEvents {
                notifications: notify_rx,
                budget,
                status: status_rx,
            },
        ))
    }

    async fn call(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), DriverError>>) -> Command,
    ) -> Result<(), DriverError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| stopped_error())?;
        answer.await.map_err(|_| stopped_error())?
    }

    /// Listens to `channel` (a no-op when it already does). While the
    /// listener reconnects the channel is recorded and listened to once
    /// connected.
    ///
    /// # Errors
    /// An invalid name, more than [`MAX_CHANNELS`] channels, a server
    /// error, or a stopped listener.
    pub async fn listen(&self, channel: &str) -> Result<(), DriverError> {
        validate_channel(channel).map_err(invalid)?;
        let channel = channel.to_owned();
        self.call(|reply| Command::Listen(channel, reply)).await
    }

    /// Stops listening to `channel` (a no-op when it does not).
    ///
    /// # Errors
    /// An invalid name, a server error, or a stopped listener.
    pub async fn unlisten(&self, channel: &str) -> Result<(), DriverError> {
        validate_channel(channel).map_err(invalid)?;
        let channel = channel.to_owned();
        self.call(|reply| Command::Unlisten(channel, reply)).await
    }

    /// Sends a notification with `pg_catalog.pg_notify` on the listener's
    /// connection (in a transaction of its own, so it is delivered at once).
    ///
    /// # Errors
    /// An invalid name or payload, a read-only listener
    /// ([`ErrorKind::ReadOnly`]), a reconnecting listener
    /// ([`ErrorKind::ConnectionLost`], nothing was sent), a server error.
    pub async fn notify(&self, channel: &str, payload: &str) -> Result<(), DriverError> {
        validate_channel(channel).map_err(invalid)?;
        validate_payload(payload).map_err(invalid)?;
        let (channel, payload) = (channel.to_owned(), payload.to_owned());
        self.call(|reply| Command::Notify(channel, payload, reply))
            .await
    }

    /// The channels listened to (or to be listened to once reconnected),
    /// sorted.
    #[must_use]
    pub fn channels(&self) -> Vec<String> {
        lock(&self.snapshot).channels.iter().cloned().collect()
    }

    /// True once the listener has stopped (closed or given up).
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        lock(&self.snapshot).stopped
    }

    /// Notifications dropped because the buffer was full, since the last
    /// call; the count starts again at zero.
    #[must_use]
    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::AcqRel)
    }

    /// Asks the listener to stop without waiting for it (it then reports
    /// [`ListenerStatus::Stopped`]).
    pub fn shutdown(&self) {
        self.closed.send_replace(true);
    }

    /// Closes the connection and waits until the listener has stopped.
    pub async fn close(&self) {
        self.closed.send_replace(true);
        let task = lock(&self.supervisor).take();
        if let Some(task) = task
            && let Err(error) = task.await
        {
            tracing::debug!(%error, "the listener task ended abnormally");
        }
    }
}

impl Drop for PgListener {
    fn drop(&mut self) {
        self.closed.send_replace(true);
    }
}

/// One open listener connection.
struct Conn {
    client: Client,
    task: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    info: ServerInfo,
    process_id: i32,
    read_only: bool,
    guard: Option<Box<dyn Any + Send + Sync>>,
}

impl Conn {
    /// Ends the connection and waits (at most [`CLOSE_TIMEOUT`]) until the
    /// server has closed it, which it does once the backend has ended. The
    /// guard (tunnel) is kept until then.
    async fn close_and_wait(self) {
        let Self {
            client,
            task,
            guard,
            ..
        } = self;
        // Dropping the client makes the connection task send Terminate and
        // shut the stream down; a listener's tap waits for the server's end.
        drop(client);
        if let Some(task) = task {
            let abort = task.abort_handle();
            if tokio::time::timeout(CLOSE_TIMEOUT, task).await.is_err() {
                abort.abort();
            }
        }
        drop(guard);
    }

    /// Ends a lost connection in the background.
    fn close(self) {
        tokio::spawn(self.close_and_wait());
    }

    /// True when nothing more can be sent on the connection.
    fn ended(&self) -> bool {
        self.client.is_closed() || self.shared.connection_ended()
    }

    /// Drops what the stream tap keeps per call: the listener never reads
    /// command tags or notices, and must not grow them over days.
    fn tidy(&self) {
        self.shared.clear_tags();
        self.shared.clear_notices();
    }
}

enum Served {
    Closed,
    Lost(DriverError),
}

struct Supervisor {
    connector: Arc<dyn ListenConnector>,
    options: ListenerOptions,
    commands: mpsc::Receiver<Command>,
    closed: watch::Receiver<bool>,
    status: mpsc::UnboundedSender<ListenerStatus>,
    notifications: mpsc::Sender<Notification>,
    dropped: Arc<AtomicU64>,
    desired: BTreeSet<String>,
    snapshot: Arc<StdMutex<Snapshot>>,
}

fn lost_error() -> DriverError {
    DriverError::new(
        ErrorKind::ConnectionLost,
        "The listening connection to the server was lost.",
    )
}

fn timed_out(what: &str, after: Duration) -> DriverError {
    DriverError::new(
        ErrorKind::Timeout,
        format!(
            "The server did not answer the listener's {what} within {} ms.",
            after.as_millis()
        ),
    )
}

impl Supervisor {
    fn send_status(&self, status: ListenerStatus) {
        // A reader that went away is fine: the status is only information.
        let _ = self.status.send(status);
    }

    fn publish_channels(&self) {
        lock(&self.snapshot).channels.clone_from(&self.desired);
    }

    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Opens a connection and listens to every desired channel.
    async fn connect(&self, target: ListenTarget) -> Result<Conn, DriverError> {
        let ListenTarget { mut params, guard } = target;
        params.init_sql = None;
        let shared = Arc::new(Shared::default());
        let own_pid = Arc::new(AtomicI32::new(0));
        {
            let sender = self.notifications.clone();
            let dropped = Arc::clone(&self.dropped);
            let own_pid = Arc::clone(&own_pid);
            let budget = self.options.budget.clone();
            shared.set_notification_sink(Box::new(
                move |n: tokio_postgres::Notification, lossy: bool| {
                    let note = Notification {
                        channel: n.channel().to_owned(),
                        payload: n.payload().to_owned(),
                        process_id: n.process_id(),
                        from_self: n.process_id() == own_pid.load(Ordering::Acquire),
                        lossy,
                        received_at: SystemTime::now(),
                    };
                    // Never wait: a full buffer (or shared budget) drops the
                    // newest and counts it.
                    let cost = queued_cost(&note);
                    if budget.as_ref().is_some_and(|b| !b.reserve(cost)) {
                        dropped.fetch_add(1, Ordering::AcqRel);
                        return;
                    }
                    if sender.try_send(note).is_err() {
                        if let Some(budget) = &budget {
                            budget.release(cost);
                        }
                        dropped.fetch_add(1, Ordering::AcqRel);
                    }
                },
            ));
        }
        let opened = open(&params, &shared).await?;
        shared.use_connection(opened.number);
        let conn = Conn {
            client: opened.client,
            task: opened.task,
            shared,
            info: opened.info,
            process_id: 0,
            read_only: params.read_only,
            guard,
        };
        let setup = async {
            let pid = backend_pid(&conn.client).await?;
            own_pid.store(pid, Ordering::Release);
            let sql_ascii = conn
                .shared
                .parameter("server_encoding")
                .is_some_and(|e| e.eq_ignore_ascii_case("SQL_ASCII"));
            let sql = setup_sql(&conn.info, &self.desired, sql_ascii);
            conn.client
                .batch_execute(&sql)
                .await
                .map_err(|e| exec_error(&e, 0, Interrupt::NONE))?;
            Ok::<i32, DriverError>(pid)
        };
        match tokio::time::timeout(self.options.call_timeout, setup).await {
            Ok(Ok(pid)) => {
                conn.tidy();
                Ok(Conn {
                    process_id: pid,
                    ..conn
                })
            }
            Ok(Err(error)) => {
                conn.close();
                Err(error)
            }
            Err(_) => {
                conn.close();
                Err(timed_out("setup", self.options.call_timeout))
            }
        }
    }

    async fn run(mut self, first: Conn) {
        let mut conn = Some(first);
        let mut error = lost_error();
        // Failed reconnect attempts since the connection was lost.
        let mut failed = 0u32;
        loop {
            if let Some(current) = conn.take() {
                match self.serve(&current).await {
                    Served::Closed => {
                        current.close_and_wait().await;
                        return self.stop(None);
                    }
                    Served::Lost(lost) => {
                        current.close();
                        error = lost;
                        failed = 0;
                    }
                }
            }
            let retry_in = self.options.backoff(failed.saturating_add(1));
            self.send_status(ListenerStatus::Reconnecting {
                error: error.clone(),
                attempt: failed,
                retry_in,
            });
            if !self.wait_disconnected(retry_in).await {
                return self.stop(None);
            }
            let attempted = {
                let mut closed = self.closed.clone();
                let try_connect = async {
                    match self.connector.target().await {
                        Ok(target) => self.connect(target).await.map_err(|e| {
                            if transient(&e) {
                                TargetError::Retry(e)
                            } else {
                                TargetError::Stop(e)
                            }
                        }),
                        Err(e) => Err(e),
                    }
                };
                tokio::select! {
                    result = try_connect => Some(result),
                    () = wait_closed(&mut closed) => None,
                }
            };
            match attempted {
                None => return self.stop(None),
                Some(Ok(opened)) => {
                    self.send_status(ListenerStatus::Connected {
                        process_id: opened.process_id,
                        server: opened.info.clone(),
                        reconnected: true,
                    });
                    conn = Some(opened);
                }
                Some(Err(TargetError::Retry(e))) => {
                    error = e;
                    failed = failed.saturating_add(1);
                }
                Some(Err(TargetError::Stop(e))) => return self.stop(Some(e)),
            }
        }
    }

    fn stop(mut self, error: Option<DriverError>) {
        lock(&self.snapshot).stopped = true;
        // Callers waiting on a reply learn that it stopped.
        self.commands.close();
        while let Ok(command) = self.commands.try_recv() {
            let reply = match command {
                Command::Listen(_, reply)
                | Command::Unlisten(_, reply)
                | Command::Notify(_, _, reply) => reply,
            };
            let _ = reply.send(Err(stopped_error()));
        }
        self.send_status(ListenerStatus::Stopped { error });
    }

    /// Serves commands until the listener is closed or the connection is
    /// lost.
    async fn serve(&mut self, conn: &Conn) -> Served {
        let period = self.options.ping_interval.max(Duration::from_millis(10));
        let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut closed = self.closed.clone();
        loop {
            if self.is_closed() {
                return Served::Closed;
            }
            tokio::select! {
                () = wait_closed(&mut closed) => return Served::Closed,
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        return Served::Closed;
                    };
                    if let Err(lost) = self.handle(conn, command).await {
                        return Served::Lost(lost);
                    }
                }
                () = conn.shared.wait_connection_end() => return Served::Lost(lost_error()),
                _ = ping.tick() => {
                    let result = self.timed(conn, "ping", async {
                        conn.client.simple_query("").await.map(|_| ())
                    }).await;
                    if let Err(error) = result {
                        return Served::Lost(error);
                    }
                }
            }
        }
    }

    /// Runs one call with the call timeout; a call that does not answer in
    /// time fails with [`ErrorKind::Timeout`] (the connection is then
    /// treated as lost).
    async fn timed<F>(&self, conn: &Conn, what: &str, call: F) -> Result<(), DriverError>
    where
        F: std::future::Future<Output = Result<(), tokio_postgres::Error>>,
    {
        let result = tokio::time::timeout(self.options.call_timeout, call).await;
        conn.tidy();
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(exec_error(&error, 0, Interrupt::NONE)),
            Err(_) => Err(timed_out(what, self.options.call_timeout)),
        }
    }

    /// Handles a command on a live connection. `Err` is the error that
    /// lost the connection (the reply was sent already).
    async fn handle(&mut self, conn: &Conn, command: Command) -> Result<(), DriverError> {
        match command {
            Command::Listen(channel, reply) => {
                if self.desired.contains(&channel) {
                    let _ = reply.send(Ok(()));
                    return Ok(());
                }
                if self.desired.len() >= MAX_CHANNELS {
                    let _ = reply.send(Err(invalid(format!(
                        "A listener listens to at most {MAX_CHANNELS} channels."
                    ))));
                    return Ok(());
                }
                let sql = format!("LISTEN {}", quote_channel(&channel));
                let result = self
                    .timed(conn, "LISTEN", conn.client.batch_execute(&sql))
                    .await;
                if result.is_ok() {
                    self.desired.insert(channel);
                    self.publish_channels();
                }
                self.answer(conn, reply, result)
            }
            Command::Unlisten(channel, reply) => {
                if !self.desired.contains(&channel) {
                    let _ = reply.send(Ok(()));
                    return Ok(());
                }
                let sql = format!("UNLISTEN {}", quote_channel(&channel));
                let result = self
                    .timed(conn, "UNLISTEN", conn.client.batch_execute(&sql))
                    .await;
                if result.is_ok() || conn.ended() {
                    // A lost connection listens to nothing; the reconnect
                    // must not listen to it again.
                    self.desired.remove(&channel);
                    self.publish_channels();
                }
                if conn.ended() {
                    let _ = reply.send(Ok(()));
                    return Err(result.err().unwrap_or_else(lost_error));
                }
                self.answer(conn, reply, result)
            }
            Command::Notify(channel, payload, reply) => {
                if conn.read_only {
                    let _ = reply.send(Err(DriverError::new(
                        ErrorKind::ReadOnly,
                        "The connection is read-only, so NOTIFY is not allowed.",
                    )));
                    return Ok(());
                }
                let result = self
                    .timed(conn, "NOTIFY", async {
                        conn.client
                            .execute("SELECT pg_catalog.pg_notify($1, $2)", &[&channel, &payload])
                            .await
                            .map(|_| ())
                    })
                    .await;
                self.answer(conn, reply, result)
            }
        }
    }

    /// Replies with `result`; when the call lost the connection, returns
    /// the error so the connection is replaced.
    fn answer(
        &self,
        conn: &Conn,
        reply: oneshot::Sender<Result<(), DriverError>>,
        result: Result<(), DriverError>,
    ) -> Result<(), DriverError> {
        let lost = match &result {
            Err(error) if error.kind == ErrorKind::Timeout || conn.ended() => Some(error.clone()),
            _ => None,
        };
        let _ = reply.send(result);
        lost.map_or(Ok(()), Err)
    }

    /// Waits `delay` without a connection, still answering commands.
    /// Returns false when the listener was closed meanwhile.
    async fn wait_disconnected(&mut self, delay: Duration) -> bool {
        let deadline = tokio::time::sleep(delay);
        tokio::pin!(deadline);
        let mut closed = self.closed.clone();
        loop {
            if self.is_closed() {
                return false;
            }
            tokio::select! {
                () = &mut deadline => return true,
                () = wait_closed(&mut closed) => return false,
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        return false;
                    };
                    self.handle_disconnected(command);
                }
            }
        }
    }

    /// Handles a command while there is no connection: channel changes are
    /// recorded for the reconnect, NOTIFY is refused.
    fn handle_disconnected(&mut self, command: Command) {
        match command {
            Command::Listen(channel, reply) => {
                let result = if self.desired.contains(&channel) || self.desired.len() < MAX_CHANNELS
                {
                    self.desired.insert(channel);
                    self.publish_channels();
                    Ok(())
                } else {
                    Err(invalid(format!(
                        "A listener listens to at most {MAX_CHANNELS} channels."
                    )))
                };
                let _ = reply.send(result);
            }
            Command::Unlisten(channel, reply) => {
                self.desired.remove(&channel);
                self.publish_channels();
                let _ = reply.send(Ok(()));
            }
            Command::Notify(_, _, reply) => {
                let _ = reply.send(Err(DriverError::new(
                    ErrorKind::ConnectionLost,
                    "The listener is reconnecting; nothing was sent.",
                )));
            }
        }
    }
}

/// Waits until the listener is closed (or its handle is gone).
async fn wait_closed(closed: &mut watch::Receiver<bool>) {
    // The guard of the value is dropped here, so the caller's future stays
    // `Send`.
    let _ = closed.wait_for(|closed| *closed).await.map(|_| ());
}

/// The process id of the connection's backend.
async fn backend_pid(client: &Client) -> Result<i32, DriverError> {
    let messages = client
        .simple_query("SELECT pg_catalog.pg_backend_pid()")
        .await
        .map_err(|e| exec_error(&e, 0, Interrupt::NONE))?;
    messages
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(row) => row.get(0).and_then(|v| v.parse::<i32>().ok()),
            _ => None,
        })
        .ok_or_else(|| DriverError::new(ErrorKind::Protocol, "pg_backend_pid() returned no row"))
}

/// What a new connection runs: no idle session timeout (PostgreSQL 14 and
/// newer would close an idle listener), on a `SQL_ASCII` database the
/// client encoding `SQL_ASCII`, then a LISTEN per channel.
///
/// A `SQL_ASCII` database stores any bytes. With client encoding UTF8 the
/// server checks every notification it delivers and ends the listener's
/// connection ("protocol synchronization was lost") at the first one that
/// is not valid UTF-8; with `SQL_ASCII` it passes the bytes on unchecked,
/// and the stream tap decodes such notifications lossily
/// (`Notification::lossy`). Valid UTF-8 arrives unchanged either way.
fn setup_sql(info: &ServerInfo, channels: &BTreeSet<String>, sql_ascii: bool) -> String {
    let mut statements = Vec::with_capacity(channels.len() + 2);
    if sql_ascii {
        statements.push("SET client_encoding = 'SQL_ASCII'".to_owned());
    }
    if info.at_least(14, 0) {
        statements.push("SET idle_session_timeout = 0".to_owned());
    }
    statements.extend(
        channels
            .iter()
            .map(|channel| format!("LISTEN {}", quote_channel(channel))),
    );
    statements.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_core::Engine;

    fn server(major: u32) -> ServerInfo {
        ServerInfo {
            engine: Engine::Postgres,
            version: format!("{major}.0"),
            major,
            minor: 0,
            patch: None,
        }
    }

    #[test]
    fn channels_are_quoted_identifiers() {
        assert_eq!(quote_channel("orders"), "\"orders\"");
        assert_eq!(quote_channel("Mixed Case"), "\"Mixed Case\"");
        assert_eq!(
            quote_channel("a\"; NOTIFY x; --"),
            "\"a\"\"; NOTIFY x; --\""
        );
    }

    #[test]
    fn channel_and_payload_limits() {
        assert!(validate_channel("").is_err());
        assert!(validate_channel("a\0b").is_err());
        assert!(validate_channel(&"x".repeat(MAX_CHANNEL_BYTES)).is_ok());
        assert!(validate_channel(&"x".repeat(MAX_CHANNEL_BYTES + 1)).is_err());
        // Bytes, not characters: 32 two-byte characters are 64 bytes.
        assert!(validate_channel(&"é".repeat(32)).is_err());
        assert!(validate_payload("").is_ok());
        assert!(validate_payload(&"p".repeat(MAX_PAYLOAD_BYTES)).is_ok());
        assert!(validate_payload(&"p".repeat(MAX_PAYLOAD_BYTES + 1)).is_err());
        assert!(validate_payload("a\0").is_err());
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let options = ListenerOptions::default();
        assert_eq!(options.backoff(1), Duration::from_millis(500));
        assert_eq!(options.backoff(2), Duration::from_secs(1));
        assert_eq!(options.backoff(4), Duration::from_secs(4));
        assert_eq!(options.backoff(7), Duration::from_secs(30));
        assert_eq!(options.backoff(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn only_some_errors_are_retried() {
        let mut error = DriverError::new(ErrorKind::Connect, "refused");
        assert!(transient(&error));
        error.code = Some("3D000".into());
        assert!(!transient(&error), "missing database");
        assert!(!transient(&DriverError::new(ErrorKind::Auth, "no")));
        assert!(
            transient(&DriverError::new(
                ErrorKind::Tls,
                "error performing TLS handshake: connection reset"
            )),
            "a handshake cut off by a restart is retried"
        );
        assert!(transient(&DriverError::new(ErrorKind::Unsupported, "x")));
        let mut auth = DriverError::new(ErrorKind::Query, "password");
        auth.code = Some("28P01".into());
        assert!(!transient(&auth));
        assert!(transient(&DriverError::new(ErrorKind::Timeout, "slow")));
    }

    #[test]
    fn setup_listens_to_every_channel_and_keeps_the_session_open() {
        let channels: BTreeSet<String> = ["b".to_owned(), "A x".to_owned()].into();
        assert_eq!(
            setup_sql(&server(13), &channels, false),
            "LISTEN \"A x\"; LISTEN \"b\""
        );
        assert_eq!(
            setup_sql(&server(18), &BTreeSet::new(), false),
            "SET idle_session_timeout = 0"
        );
        assert_eq!(
            setup_sql(&server(13), &BTreeSet::new(), true),
            "SET client_encoding = 'SQL_ASCII'"
        );
    }

    #[test]
    fn target_debug_hides_the_password() {
        let mut params = ConnectParams::new(Engine::Postgres, "db", 5432);
        params.password = Some(varve_secrets::Secret::new("hunter2-secret".to_string()));
        let text = format!(
            "{:?}",
            ListenTarget {
                params,
                guard: None
            }
        );
        assert!(!text.contains("hunter2"));
    }
}
