//! PostgreSQL `LISTEN` / `NOTIFY` panel (design doc 4.2, "Notifications
//! panel per session").
//!
//! Each PostgreSQL tab can have one listener: a dedicated connection
//! ([`PgListener`]), never the tab's own session, opened lazily by the first
//! `notify_listen` or `notify_send` with the tab's connection identity, its
//! database, the profile's saved or typed secrets and its SSH tunnel (like
//! the admin sessions, `metadata.rs`). The listener reconnects by itself with
//! backoff and listens to its channels again; every reconnect resolves the
//! target anew (current secrets, a new tunnel) and stops instead when the
//! tab closed or the profile now points at another identity.
//!
//! Notifications reach the UI as Tauri events ([`MESSAGES_EVENT`]) in
//! batches: at most [`MAX_BATCH`] messages every [`BATCH_INTERVAL`]. The
//! driver buffers at most [`BUFFER`] more; beyond that new notifications
//! are dropped (never stalling the connection, which would hold up the
//! server's notification queue for every client) and the next batch says
//! how many. Connection changes go out as [`STATUS_EVENT`].
//!
//! Sending (`notify_send`) runs `pg_catalog.pg_notify` on the listener's
//! connection, outside the tab's transaction, so it is delivered at once.
//! Read-only sessions refuse it, production connections (and profiles that
//! confirm destructive statements) ask for confirmation first, and it is logged as `query.executed` (kind
//! `notify`; the payload follows the statement text level). LISTEN and
//! UNLISTEN are logged the same way (kinds `listen`, `unlisten`); the
//! listener connection's `connection.opened`, `connection.lost` and
//! `connection.closed` carry `purpose: listen`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::task::JoinHandle;
use varve_activity::{ActivityEvent, ConnectionDetails, ErrorDetails, RowCounts, SqlDialect};
use varve_core::Engine;
use varve_driver::{DriverError, ErrorKind};
use varve_driver_postgres::listen::{
    QueueBudget, quote_channel, validate_channel, validate_payload,
};
use varve_driver_postgres::{
    ListenConnector, ListenTarget, ListenerEvents, ListenerOptions, ListenerStatus, Notification,
    PgListener, TargetError,
};
use varve_records::{ConnectionProfile, connect_params};

use super::{elapsed_ms, environment_label, error_details};
use crate::commands::{CmdError, Services};
use crate::sessions::{SessionError, SessionInfo, TunnelEnd, TunnelRoute, connection_identity};

type CmdResult<T> = Result<T, CmdError>;

/// Tauri event with a [`NotifyBatch`].
pub const MESSAGES_EVENT: &str = "notify-messages";

/// Tauri event with a [`NotifyState`].
pub const STATUS_EVENT: &str = "notify-status";

/// How often received notifications are sent to the UI.
pub const BATCH_INTERVAL: Duration = Duration::from_millis(100);

/// Most notifications in one batch.
pub const MAX_BATCH: usize = 500;

/// Notifications the driver buffers before it drops new ones.
pub const BUFFER: usize = 10_000;

/// Batches the UI may leave unacknowledged ([`Services::notify_ack`]);
/// further notifications wait in the buffer meanwhile.
pub const MAX_UNACKED: u64 = 4;

/// How often a listener checks that its tab is still open and its profile
/// still has the tab's identity (the session close hook stops it sooner).
pub const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// A received notification, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyMessage {
    /// The channel.
    pub channel: String,
    /// The payload (empty when none was sent).
    pub payload: String,
    /// Process id of the sending backend.
    pub pid: i32,
    /// Sent from this panel (the listener's own connection).
    pub own: bool,
    /// The channel or payload was not valid UTF-8 (a `SQL_ASCII`
    /// database) and is shown decoded lossily (U+FFFD for invalid bytes).
    pub lossy: bool,
    /// When it was received, unix ms.
    pub received_at: i64,
}

impl From<Notification> for NotifyMessage {
    fn from(note: Notification) -> Self {
        Self {
            channel: note.channel,
            payload: note.payload,
            pid: note.process_id,
            own: note.from_self,
            lossy: note.lossy,
            received_at: unix_ms(note.received_at),
        }
    }
}

/// Notifications of one tab, sent as [`MESSAGES_EVENT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyBatch {
    /// The tab's (user) session.
    pub session_id: String,
    /// The listener that sent it; acknowledge with it.
    pub listener_id: u64,
    /// 1, 2, 3, ... per listener; acknowledge with `notify_ack`.
    pub seq: u64,
    /// The notifications, oldest first.
    pub messages: Vec<NotifyMessage>,
    /// Notifications dropped since the previous batch because the UI fell
    /// behind (they came after these messages).
    pub dropped: u64,
}

/// The state of a tab's listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NotifyStatus {
    /// Connected and listening.
    Connected,
    /// Lost; trying again (`retryInMs`).
    Reconnecting,
    /// Not listening: never started, stopped, or gave up (`error`).
    Stopped,
}

/// A tab's listener, returned by the commands and sent as [`STATUS_EVENT`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyState {
    /// The tab's (user) session.
    pub session_id: String,
    /// The listener this state is of; `None` when the tab has none.
    pub listener_id: Option<u64>,
    /// Orders states: a state with a higher version is newer, whichever
    /// way it reached the UI (event or command reply). Unique in the app.
    pub version: u64,
    /// Connection state.
    pub status: NotifyStatus,
    /// Channels listened to (or to be listened to once reconnected),
    /// sorted.
    pub channels: Vec<String>,
    /// Process id of the listener's backend while connected.
    pub backend_pid: Option<i32>,
    /// Failed reconnect attempts since the connection was lost (0 when
    /// connected, and right after the loss).
    pub attempt: u32,
    /// Wait before the next attempt while reconnecting.
    pub retry_in_ms: Option<u64>,
    /// Why it is reconnecting or why it stopped.
    pub error: Option<DriverError>,
    /// Notifications dropped since the listener started.
    pub dropped: u64,
    /// True after a reconnect: notifications sent while it was down were
    /// lost. Cleared when the listener stops.
    pub gap: bool,
}

impl NotifyState {
    fn stopped(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            listener_id: None,
            version: 0,
            status: NotifyStatus::Stopped,
            channels: Vec::new(),
            backend_pid: None,
            attempt: 0,
            retry_in_ms: None,
            error: None,
            dropped: 0,
            gap: false,
        }
    }
}

/// The result of `notify_send`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifySent {
    /// The channel.
    pub channel: String,
    /// Payload size in bytes.
    pub payload_bytes: usize,
    /// How long the send took.
    pub duration_ms: u64,
}

/// Where a listener's events go (the window in the app, a collector in
/// tests).
pub trait NotifyEmitter: Send + Sync {
    /// Sends a batch of notifications.
    fn messages(&self, batch: NotifyBatch);
    /// Sends a state change.
    fn status(&self, state: NotifyState);
}

fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One tab's listener.
struct Entry {
    /// Tells this entry from a newer one of the same tab.
    id: u64,
    listener: Arc<PgListener>,
    state: Arc<Mutex<NotifyState>>,
    /// Why the app stopped it (tab closed, profile changed); reported
    /// instead of a plain stop.
    stop_reason: Arc<Mutex<Option<DriverError>>>,
    forwarder: Mutex<Option<JoinHandle<()>>>,
    /// The highest batch `seq` the UI acknowledged.
    acked: Arc<AtomicU64>,
    /// The last batch `seq` sent.
    sent: Arc<AtomicU64>,
}

impl Entry {
    fn stop(&self, reason: Option<DriverError>) {
        if let Some(reason) = reason {
            lock(&self.stop_reason).get_or_insert(reason);
        }
        self.listener.shutdown();
    }
}

type AsyncLock = Arc<tokio::sync::Mutex<()>>;

/// The listeners of all tabs.
pub struct NotifyHub {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    /// One per tab, so two calls do not start two listeners.
    opening: Mutex<HashMap<String, AsyncLock>>,
    next_id: AtomicU64,
    /// Hands out [`NotifyState::version`].
    versions: Arc<AtomicU64>,
    /// Bytes of notifications waiting for the UI, across all tabs.
    budget: Arc<QueueBudget>,
}

/// Bytes of notifications that may wait for the UI across all tabs
/// (each tab's listener also holds at most [`BUFFER`]).
pub const GLOBAL_BUDGET_BYTES: usize = 64 * 1024 * 1024;

impl Default for NotifyHub {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            opening: Mutex::default(),
            // Listener ids start at 1, so 0 never names one.
            next_id: AtomicU64::new(1),
            versions: Arc::new(AtomicU64::new(1)),
            budget: QueueBudget::new(GLOBAL_BUDGET_BYTES),
        }
    }
}

impl std::fmt::Debug for NotifyHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sessions: Vec<String> = lock(&self.entries).keys().cloned().collect();
        f.debug_struct("NotifyHub")
            .field("sessions", &sessions)
            .finish_non_exhaustive()
    }
}

impl NotifyHub {
    fn get(&self, session_id: &str) -> Option<Arc<Entry>> {
        lock(&self.entries).get(session_id).cloned()
    }

    fn opening_lock(&self, session_id: &str) -> AsyncLock {
        let mut map = lock(&self.opening);
        // Locks nobody holds or waits for are dropped on the way.
        map.retain(|_, l| Arc::strong_count(l) > 1);
        Arc::clone(map.entry(session_id.to_string()).or_default())
    }

    /// Removes the entry `id` of a tab (not a newer one).
    fn remove(&self, session_id: &str, id: u64) {
        let mut map = lock(&self.entries);
        if map.get(session_id).is_some_and(|e| e.id == id) {
            map.remove(session_id);
        }
    }

    /// Stops the listener of a closed tab. Called by the session event
    /// sink; it does not wait for the connection to close.
    pub fn forget_session(&self, session_id: &str) {
        let entry = lock(&self.entries).remove(session_id);
        if let Some(entry) = entry {
            entry.stop(Some(DriverError::new(
                ErrorKind::ConnectionLost,
                "The tab's session was closed.",
            )));
        }
    }

    /// The number of running listeners (for diagnostics and tests).
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.entries).len()
    }

    /// True when no listener runs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn invalid(message: String) -> CmdError {
    CmdError::Invalid { message }
}

fn driver(error: DriverError) -> CmdError {
    match error.kind {
        ErrorKind::ReadOnly => CmdError::ReadOnly {
            message: error.message,
        },
        _ => CmdError::Session {
            error: SessionError::Driver { error },
        },
    }
}

/// A command error as the driver error a stopped listener reports.
fn as_driver_error(error: &CmdError) -> DriverError {
    match error {
        CmdError::Session {
            error: SessionError::Driver { error },
        } => error.clone(),
        CmdError::Tunnel { .. } => DriverError::new(ErrorKind::Connect, error.to_string()),
        CmdError::SecretRequired { .. } => DriverError::new(
            ErrorKind::Auth,
            "The password is no longer known; listen again to enter it.",
        ),
        other => DriverError::new(ErrorKind::ConnectionLost, other.to_string()),
    }
}

/// The literal of a payload in the logged statement text (the recorder
/// redacts it at its text level).
fn sql_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Resolves the listener's target for a tab: refuses when the tab closed or
/// its profile now has another identity, then reads the secrets and opens
/// the tunnel like the admin sessions do. With an SSH tunnel, also the
/// tunnel's end, so errors name the server and the bastion rather than the
/// tunnel's local end.
async fn listen_target(
    services: &Services,
    info: &SessionInfo,
) -> CmdResult<(ListenTarget, Option<TunnelEnd>)> {
    if services.sessions.info(&info.id).is_err() {
        return Err(CmdError::Session {
            error: SessionError::NotFound {
                session_id: info.id.clone(),
            },
        });
    }
    let profile = current_profile(services, info)?;
    let mut connect = profile.clone();
    // The tab's own database, never the profile's current one.
    connect.database.clone_from(&info.database);
    let typed = services.metadata.typed_for(&profile);
    let lease = services
        .tunnels
        .lease_for(&connect, services.secrets.as_ref(), typed.ssh)
        .await?;
    let mut params = connect_params(
        &connect,
        services.secrets.as_ref(),
        typed.password,
        typed.tls,
        lease.as_ref().map(|l| l.endpoint()),
    )?;
    params.read_only = info.read_only;
    params.application_name = format!("{} (listener)", params.application_name);
    let end = TunnelRoute::of(&connect).map(|route| route.end(&params));
    Ok((
        ListenTarget {
            params,
            guard: lease.map(|l| Box::new(l) as Box<dyn std::any::Any + Send + Sync>),
        },
        end,
    ))
}

/// The tab's profile as stored now; `reconnectRequired` when it has
/// another identity than the tab.
fn current_profile(services: &Services, info: &SessionInfo) -> CmdResult<ConnectionProfile> {
    let profile = services.records.connection(&info.connection_id)?;
    if connection_identity(&profile) != info.identity {
        return Err(CmdError::ReconnectRequired {
            message: "The connection settings changed since this tab connected; reconnect this \
                      tab to listen."
                .into(),
        });
    }
    Ok(profile)
}

/// Reconnect targets of one tab's listener.
struct TabConnector {
    services: Weak<Services>,
    info: SessionInfo,
    /// The tunnel end of the latest target, shared with the forwarder.
    tunnel: Arc<Mutex<Option<TunnelEnd>>>,
}

#[async_trait::async_trait]
impl ListenConnector for TabConnector {
    async fn target(&self) -> Result<ListenTarget, TargetError> {
        let Some(services) = self.services.upgrade() else {
            return Err(TargetError::Stop(DriverError::new(
                ErrorKind::ConnectionLost,
                "The app is shutting down.",
            )));
        };
        let (target, end) =
            listen_target(&services, &self.info)
                .await
                .map_err(|error| match error {
                    // A tunnel that could not be opened may come back.
                    CmdError::Tunnel { .. } => TargetError::Retry(as_driver_error(&error)),
                    other => TargetError::Stop(as_driver_error(&other)),
                })?;
        *lock(&self.tunnel) = end;
        Ok(target)
    }
}

fn connection_details(services: &Services, info: &SessionInfo) -> ConnectionDetails {
    let profile = services.records.connection(&info.connection_id).ok();
    ConnectionDetails {
        engine: info.engine,
        server_version: Some(info.server.version.clone()),
        host: profile.as_ref().map(|p| p.host.clone()).unwrap_or_default(),
        port: profile.as_ref().map(|p| p.port),
        database: info.database.clone(),
        username: profile.as_ref().and_then(|p| p.username.clone()),
        via_ssh: profile.as_ref().is_some_and(|p| p.ssh.enabled),
        tls_mode: profile
            .as_ref()
            .map(|p| format!("{:?}", p.tls.mode).to_ascii_lowercase()),
    }
}

impl Services {
    /// The listener of a tab, started when there is none (or it stopped).
    async fn listener_entry(
        self: &Arc<Self>,
        info: &SessionInfo,
        emitter: &Arc<dyn NotifyEmitter>,
    ) -> CmdResult<Arc<Entry>> {
        let hub = &self.m2.notify;
        let opening = hub.opening_lock(&info.id);
        let _opening = opening.lock().await;
        if let Some(entry) = hub.get(&info.id)
            && !entry.listener.is_stopped()
        {
            return Ok(entry);
        }
        let (first, end) = listen_target(self, info).await?;
        let tunnel = Arc::new(Mutex::new(end.clone()));
        let connector: Arc<dyn ListenConnector> = Arc::new(TabConnector {
            services: Arc::downgrade(self),
            info: info.clone(),
            tunnel: Arc::clone(&tunnel),
        });
        let options = ListenerOptions {
            buffer: BUFFER,
            budget: Some(Arc::clone(&hub.budget)),
            ..ListenerOptions::default()
        };
        let started = PgListener::start(first, connector, options).await;
        let details = connection_details(self, info);
        let (listener, events) = match started {
            Ok(started) => started,
            Err(error) => {
                let error = match &end {
                    Some(end) => end.connect_error(error),
                    None => error,
                };
                self.record(
                    info,
                    ActivityEvent::connection_failed(&details, &error_details(&error))
                        .detail("purpose", "listen"),
                );
                return Err(driver(error));
            }
        };
        let id = hub.next_id.fetch_add(1, Ordering::SeqCst);
        let entry = Arc::new(Entry {
            id,
            listener: Arc::new(listener),
            state: Arc::new(Mutex::new(NotifyState::stopped(&info.id))),
            stop_reason: Arc::new(Mutex::new(None)),
            forwarder: Mutex::new(None),
            acked: Arc::new(AtomicU64::new(0)),
            sent: Arc::new(AtomicU64::new(0)),
        });
        lock(&hub.entries).insert(info.id.clone(), Arc::clone(&entry));
        let forwarder = Forwarder {
            services: Arc::downgrade(self),
            info: info.clone(),
            entry_id: id,
            listener: Arc::clone(&entry.listener),
            state: Arc::clone(&entry.state),
            stop_reason: Arc::clone(&entry.stop_reason),
            emitter: Arc::clone(emitter),
            details,
            opened: Instant::now(),
            acked: Arc::clone(&entry.acked),
            sent: Arc::clone(&entry.sent),
            versions: Arc::clone(&hub.versions),
            tunnel,
        };
        // Handle the start's Connected before the call returns, so the
        // state the command returns is current.
        let mut events = events;
        if let Some(status) = events.status.recv().await {
            forwarder.on_status(status);
        }
        *lock(&entry.forwarder) = Some(tokio::spawn(forwarder.run(events)));
        Ok(entry)
    }

    fn notify_info(&self, session_id: &str) -> CmdResult<SessionInfo> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Postgres], "LISTEN / NOTIFY")?;
        Ok(info)
    }

    fn snapshot(&self, entry: &Entry) -> NotifyState {
        let mut state = {
            let state = lock(&entry.state);
            let mut copy = state.clone();
            // Under the state lock, so it orders with the forwarder's states.
            copy.version = self.m2.notify.versions.fetch_add(1, Ordering::SeqCst);
            copy
        };
        state.listener_id = Some(entry.id);
        state.channels = entry.listener.channels();
        state
    }

    fn no_listener(&self, session_id: &str) -> NotifyState {
        NotifyState {
            version: self.m2.notify.versions.fetch_add(1, Ordering::SeqCst),
            ..NotifyState::stopped(session_id)
        }
    }

    /// The tab's listener state; `stopped` when it has none.
    ///
    /// # Errors
    /// Unknown session, not PostgreSQL.
    pub fn notify_state(&self, session_id: &str) -> CmdResult<NotifyState> {
        self.notify_info(session_id)?;
        Ok(self
            .m2
            .notify
            .get(session_id)
            .map_or_else(|| self.no_listener(session_id), |e| self.snapshot(&e)))
    }

    /// Listens to `channel` (exact name, case kept), starting the tab's
    /// listener when needed.
    ///
    /// # Errors
    /// Unknown session, not PostgreSQL (`session` / `unsupported`), an
    /// invalid name (`invalid`), `reconnectRequired`, everything opening a
    /// connection returns (`secretRequired`, `tunnel`, `driver`), and
    /// server errors.
    pub async fn notify_listen(
        self: &Arc<Self>,
        session_id: &str,
        channel: &str,
        emitter: Arc<dyn NotifyEmitter>,
    ) -> CmdResult<NotifyState> {
        let info = self.notify_info(session_id)?;
        validate_channel(channel).map_err(invalid)?;
        let entry = self.listener_entry(&info, &emitter).await?;
        let statement = format!("LISTEN {}", quote_channel(channel));
        let started = Instant::now();
        let result = entry.listener.listen(channel).await;
        self.record_statement(&info, &statement, "listen", started, result.as_ref().err());
        result.map_err(driver)?;
        let state = self.snapshot(&entry);
        emitter.status(state.clone());
        Ok(state)
    }

    /// Stops listening to `channel`. Without a listener nothing happens.
    ///
    /// # Errors
    /// Unknown session, not PostgreSQL, an invalid name, server errors.
    pub async fn notify_unlisten(
        &self,
        session_id: &str,
        channel: &str,
        emitter: Arc<dyn NotifyEmitter>,
    ) -> CmdResult<NotifyState> {
        let info = self.notify_info(session_id)?;
        validate_channel(channel).map_err(invalid)?;
        let Some(entry) = self.m2.notify.get(session_id) else {
            return Ok(self.no_listener(session_id));
        };
        let statement = format!("UNLISTEN {}", quote_channel(channel));
        let started = Instant::now();
        let result = entry.listener.unlisten(channel).await;
        self.record_statement(
            &info,
            &statement,
            "unlisten",
            started,
            result.as_ref().err(),
        );
        result.map_err(driver)?;
        let state = self.snapshot(&entry);
        emitter.status(state.clone());
        Ok(state)
    }

    /// Sends a notification on the tab's listener connection (started when
    /// needed), outside the tab's transaction.
    ///
    /// # Errors
    /// As [`Self::notify_listen`], plus `readOnly` on read-only sessions,
    /// `confirmationRequired` on production connections (and profiles that
    /// confirm destructive statements) until `confirmed`,
    /// an invalid payload, and `driver` / `connectionLost` while the
    /// listener reconnects (nothing was sent).
    pub async fn notify_send(
        self: &Arc<Self>,
        session_id: &str,
        channel: &str,
        payload: &str,
        confirmed: bool,
        emitter: Arc<dyn NotifyEmitter>,
    ) -> CmdResult<NotifySent> {
        let info = self.notify_info(session_id)?;
        Self::require_writable(&info, "sending a notification")?;
        validate_channel(channel).map_err(invalid)?;
        validate_payload(payload).map_err(invalid)?;
        // Production connections (and profiles that ask for it).
        let production = info.confirm_destructive;
        let label = format!("NOTIFY {}", quote_channel(channel));
        self.require_confirmation(&info, production, confirmed, vec![label.clone()])?;
        let entry = self.listener_entry(&info, &emitter).await?;
        let statement = format!(
            "NOTIFY {}, {}",
            quote_channel(channel),
            sql_literal(payload)
        );
        let started = Instant::now();
        let result = entry.listener.notify(channel, payload).await;
        self.record_statement(&info, &statement, "notify", started, result.as_ref().err());
        result.map_err(driver)?;
        if production {
            self.record(
                &info,
                ActivityEvent::destructive_confirmed(label, &environment_label(&info)),
            );
        }
        Ok(NotifySent {
            channel: channel.to_string(),
            payload_bytes: payload.len(),
            duration_ms: elapsed_ms(started),
        })
    }

    /// Acknowledges the batches of listener `listener_id` up to `seq` (the
    /// UI has shown them), so further batches are sent. An ack for another
    /// listener (one that stopped) changes nothing, and one beyond the last
    /// batch sent counts only up to it.
    ///
    /// # Errors
    /// Unknown session, not PostgreSQL.
    pub fn notify_ack(&self, session_id: &str, listener_id: u64, seq: u64) -> CmdResult<()> {
        self.notify_info(session_id)?;
        if let Some(entry) = self.m2.notify.get(session_id)
            && entry.id == listener_id
        {
            let sent = entry.sent.load(Ordering::Acquire);
            entry.acked.fetch_max(seq.min(sent), Ordering::AcqRel);
        }
        Ok(())
    }

    /// Stops the tab's listener and waits until its connection is closed.
    /// Without a listener nothing happens.
    ///
    /// # Errors
    /// Unknown session, not PostgreSQL.
    pub async fn notify_stop(&self, session_id: &str) -> CmdResult<()> {
        self.notify_info(session_id)?;
        let entry = lock(&self.m2.notify.entries).remove(session_id);
        if let Some(entry) = entry {
            entry.listener.close().await;
            let forwarder = lock(&entry.forwarder).take();
            if let Some(forwarder) = forwarder
                && let Err(error) = forwarder.await
            {
                tracing::debug!(%error, "the notification forwarder ended abnormally");
            }
        }
        Ok(())
    }

    fn record_statement(
        &self,
        info: &SessionInfo,
        statement: &str,
        kind: &str,
        started: Instant,
        error: Option<&DriverError>,
    ) {
        let duration = elapsed_ms(started);
        let builder = match error {
            None => ActivityEvent::query_executed(
                statement,
                SqlDialect::Postgres,
                kind,
                RowCounts::default(),
                duration,
            ),
            Some(error) => ActivityEvent::query_failed(
                statement,
                SqlDialect::Postgres,
                kind,
                &error_details(error),
                duration,
            ),
        };
        self.record(info, builder.detail("purpose", "listen"));
    }
}

/// Moves a listener's events to the UI and the activity log.
struct Forwarder {
    services: Weak<Services>,
    info: SessionInfo,
    entry_id: u64,
    listener: Arc<PgListener>,
    state: Arc<Mutex<NotifyState>>,
    stop_reason: Arc<Mutex<Option<DriverError>>>,
    emitter: Arc<dyn NotifyEmitter>,
    details: ConnectionDetails,
    opened: Instant,
    /// The highest batch `seq` the UI acknowledged.
    acked: Arc<AtomicU64>,
    /// The last batch `seq` sent.
    sent: Arc<AtomicU64>,
    /// Hands out [`NotifyState::version`].
    versions: Arc<AtomicU64>,
    /// The tunnel end of the latest target, from the connector.
    tunnel: Arc<Mutex<Option<TunnelEnd>>>,
}

/// A listener status whose error names the route of an SSH tunnel rather
/// than its local end: a failed reconnect attempt (`attempt` 1 and up) is
/// a connect error, a lost connection or a stop names the route wherever
/// the driver named the local end.
fn name_route(status: ListenerStatus, end: Option<&TunnelEnd>) -> ListenerStatus {
    let Some(end) = end else {
        return status;
    };
    match status {
        ListenerStatus::Reconnecting {
            error,
            attempt,
            retry_in,
        } => ListenerStatus::Reconnecting {
            error: if attempt > 0 {
                end.connect_error(error)
            } else {
                end.name_route(error)
            },
            attempt,
            retry_in,
        },
        ListenerStatus::Stopped { error } => ListenerStatus::Stopped {
            error: error.map(|error| end.name_route(error)),
        },
        connected @ ListenerStatus::Connected { .. } => connected,
    }
}

impl Forwarder {
    async fn run(self, mut events: ListenerEvents) {
        let mut batch = tokio::time::interval(BATCH_INTERVAL);
        batch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut watch = tokio::time::interval(WATCH_INTERVAL);
        watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                status = events.status.recv() => {
                    // Whatever arrived before the status goes first.
                    self.flush(&mut events, false);
                    match status {
                        Some(status) => {
                            let stopped = matches!(status, ListenerStatus::Stopped { .. });
                            self.on_status(status);
                            if stopped {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = batch.tick() => self.flush(&mut events, false),
                _ = watch.tick() => self.watch(),
            }
        }
        // The listener stopped: hand over what is left, whatever the
        // acknowledgements.
        while !events.is_empty() {
            self.flush(&mut events, true);
        }
        if let Some(services) = self.services.upgrade() {
            services.m2.notify.remove(&self.info.id, self.entry_id);
        }
    }

    /// Stops the listener when its tab closed or its profile changed
    /// identity (a backstop for the session close hook).
    fn watch(&self) {
        let Some(services) = self.services.upgrade() else {
            self.listener.shutdown();
            return;
        };
        let reason = if services.sessions.info(&self.info.id).is_err() {
            Some("The tab's session was closed.".to_string())
        } else {
            current_profile(&services, &self.info)
                .err()
                .map(|e| e.to_string())
        };
        if let Some(reason) = reason {
            lock(&self.stop_reason)
                .get_or_insert_with(|| DriverError::new(ErrorKind::ConnectionLost, reason));
            self.listener.shutdown();
        }
    }

    /// Sends up to [`MAX_BATCH`] buffered notifications (and the dropped
    /// count) to the UI, unless [`MAX_UNACKED`] batches wait for their
    /// acknowledgement (`force`: the listener stopped, send what is left).
    fn flush(&self, events: &mut ListenerEvents, force: bool) {
        let sent = self.sent.load(Ordering::Acquire);
        if !force && sent.saturating_sub(self.acked.load(Ordering::Acquire)) >= MAX_UNACKED {
            return;
        }
        let mut messages = Vec::new();
        while messages.len() < MAX_BATCH {
            match events.try_recv() {
                Some(note) => messages.push(NotifyMessage::from(note)),
                None => break,
            }
        }
        let dropped = self.listener.take_dropped();
        if messages.is_empty() && dropped == 0 {
            return;
        }
        if dropped > 0 {
            let state = {
                let mut state = lock(&self.state);
                state.dropped = state.dropped.saturating_add(dropped);
                self.versioned(&state)
            };
            self.emitter.status(state);
        }
        let seq = sent + 1;
        self.sent.store(seq, Ordering::Release);
        self.emitter.messages(NotifyBatch {
            session_id: self.info.id.clone(),
            listener_id: self.entry_id,
            seq,
            messages,
            dropped,
        });
    }

    /// A copy of `state` to send, with a new version (taken while the
    /// caller holds the state lock), this listener's id and its channels.
    fn versioned(&self, state: &NotifyState) -> NotifyState {
        let mut copy = state.clone();
        copy.version = self.versions.fetch_add(1, Ordering::SeqCst);
        copy.listener_id = Some(self.entry_id);
        copy.channels = if copy.status == NotifyStatus::Stopped {
            Vec::new()
        } else {
            self.listener.channels()
        };
        copy
    }

    fn record(&self, builder: varve_activity::ActivityEventBuilder) {
        if let Some(services) = self.services.upgrade() {
            services.record(&self.info, builder.detail("purpose", "listen"));
        }
    }

    fn on_status(&self, status: ListenerStatus) {
        let end = lock(&self.tunnel).clone();
        let status = name_route(status, end.as_ref());
        let state = {
            let mut state = lock(&self.state);
            match &status {
                ListenerStatus::Connected {
                    process_id,
                    reconnected,
                    ..
                } => {
                    state.status = NotifyStatus::Connected;
                    state.backend_pid = Some(*process_id);
                    state.attempt = 0;
                    state.retry_in_ms = None;
                    state.error = None;
                    state.gap |= *reconnected;
                }
                ListenerStatus::Reconnecting {
                    error,
                    attempt,
                    retry_in,
                } => {
                    state.status = NotifyStatus::Reconnecting;
                    state.backend_pid = None;
                    state.attempt = *attempt;
                    state.retry_in_ms =
                        Some(u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX));
                    state.error = Some(error.clone());
                }
                ListenerStatus::Stopped { error } => {
                    state.status = NotifyStatus::Stopped;
                    state.backend_pid = None;
                    state.attempt = 0;
                    state.retry_in_ms = None;
                    state.error = lock(&self.stop_reason).clone().or_else(|| error.clone());
                    state.gap = false;
                }
            }
            self.versioned(&state)
        };
        match &status {
            ListenerStatus::Connected { reconnected, .. } => self.record(
                ActivityEvent::connection_opened(&self.details).detail("reconnected", *reconnected),
            ),
            // The first report after the loss (no attempt failed yet).
            ListenerStatus::Reconnecting { error, attempt, .. } if *attempt == 0 => {
                self.record(ActivityEvent::connection_lost(
                    &self.details,
                    &ErrorDetails {
                        code: error.code.clone(),
                        message: error.message.clone(),
                    },
                    elapsed_ms(self.opened),
                ));
            }
            ListenerStatus::Reconnecting { .. } => {}
            ListenerStatus::Stopped { .. } => {
                let mut builder =
                    ActivityEvent::connection_closed(&self.details, elapsed_ms(self.opened));
                if let Some(error) = &state.error {
                    builder = builder.detail("reason", error.message.clone());
                }
                self.record(builder);
            }
        }
        self.emitter.status(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_errors_through_a_tunnel_name_the_route() {
        let end = TunnelEnd {
            route: TunnelRoute {
                target: "db.internal:5432".into(),
                bastion: "bastion.example:22".into(),
            },
            local: "127.0.0.1:50123".into(),
            connect_timeout: Duration::from_secs(10),
        };
        let route = "db.internal:5432 through SSH bastion bastion.example:22";
        let error = |kind, message: &str| DriverError::new(kind, message);

        // A failed reconnect attempt is a connect error.
        let status = name_route(
            ListenerStatus::Reconnecting {
                error: error(ErrorKind::Timeout, "connecting timed out after 10000 ms"),
                attempt: 2,
                retry_in: Duration::from_secs(1),
            },
            Some(&end),
        );
        let ListenerStatus::Reconnecting { error: named, .. } = status else {
            panic!("not reconnecting");
        };
        assert_eq!(
            named.message,
            format!("connecting timed out after 10000 ms ({route})")
        );

        // A lost connection names the route where it named the local end.
        let status = name_route(
            ListenerStatus::Reconnecting {
                error: error(
                    ErrorKind::ConnectionLost,
                    "the connection to 127.0.0.1:50123 was lost",
                ),
                attempt: 0,
                retry_in: Duration::from_secs(1),
            },
            Some(&end),
        );
        let ListenerStatus::Reconnecting { error: named, .. } = status else {
            panic!("not reconnecting");
        };
        assert_eq!(named.message, format!("the connection to {route} was lost"));

        let status = name_route(
            ListenerStatus::Stopped {
                error: Some(error(
                    ErrorKind::Connect,
                    "could not connect to 127.0.0.1:50123: refused",
                )),
            },
            Some(&end),
        );
        let ListenerStatus::Stopped { error: Some(named) } = status else {
            panic!("not stopped with an error");
        };
        assert_eq!(
            named.message,
            format!("could not connect to {route}: refused")
        );

        // Without a tunnel nothing changes.
        let status = name_route(
            ListenerStatus::Stopped {
                error: Some(error(ErrorKind::Connect, "127.0.0.1:50123 refused")),
            },
            None,
        );
        let ListenerStatus::Stopped { error: Some(kept) } = status else {
            panic!("not stopped with an error");
        };
        assert_eq!(kept.message, "127.0.0.1:50123 refused");
    }

    #[test]
    fn wire_shapes_are_camel_case() -> Result<(), serde_json::Error> {
        let state = NotifyState {
            retry_in_ms: Some(500),
            ..NotifyState::stopped("s1")
        };
        let json = serde_json::to_value(&state)?;
        assert_eq!(json["sessionId"], "s1");
        assert_eq!(json["status"], "stopped");
        assert_eq!(json["retryInMs"], 500);
        assert!(json["backendPid"].is_null());
        assert!(json["error"].is_null());
        let batch = serde_json::to_value(NotifyBatch {
            session_id: "s1".into(),
            listener_id: 1,
            seq: 3,
            messages: vec![NotifyMessage {
                channel: "c".into(),
                payload: "p".into(),
                pid: 7,
                own: true,
                lossy: false,
                received_at: 1,
            }],
            dropped: 2,
        })?;
        assert_eq!(batch["messages"][0]["receivedAt"], 1);
        assert_eq!(batch["messages"][0]["own"], true);
        assert_eq!(batch["dropped"], 2);
        Ok(())
    }

    #[test]
    fn logged_payloads_are_literals() {
        assert_eq!(sql_literal("it's"), "'it''s'");
        assert_eq!(
            format!("NOTIFY {}, {}", quote_channel("a\"b"), sql_literal("")),
            "NOTIFY \"a\"\"b\", ''"
        );
    }

    #[test]
    fn errors_map_to_ui_kinds() {
        assert!(matches!(
            driver(DriverError::new(ErrorKind::ReadOnly, "no")),
            CmdError::ReadOnly { .. }
        ));
        assert!(matches!(
            driver(DriverError::new(ErrorKind::Query, "bad")),
            CmdError::Session {
                error: SessionError::Driver { .. }
            }
        ));
        let secret = as_driver_error(&CmdError::SecretRequired {
            secret: varve_records::SecretKindName::DbPassword,
        });
        assert_eq!(secret.kind, ErrorKind::Auth);
    }

    #[test]
    fn received_time_is_unix_ms() {
        assert_eq!(unix_ms(UNIX_EPOCH + Duration::from_millis(1234)), 1234);
        assert_eq!(unix_ms(UNIX_EPOCH), 0);
    }
}
