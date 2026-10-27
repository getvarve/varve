//! Milestone M2 commands (design doc 4.2 and 12): the object browser,
//! object editors, `EXPLAIN`, grid editing and the live server panels.
//!
//! Every area is an `impl Services` block in its own module, so the Tauri
//! wrappers in `ipc.rs` stay thin and the logic is testable without a
//! window. Shared rules live here:
//!
//! - Calls that read run under the session lock ([`SessionManager::lock_session`]),
//!   so they never interleave with a running query of the same session.
//! - Calls that change something refuse read-only sessions
//!   ([`CmdError::ReadOnly`]), ask for confirmation when the change is
//!   destructive ([`SessionError::ConfirmationRequired`], recorded as
//!   `destructive.blocked`), and record an activity event (design doc 9.2).
//!
//! [`SessionManager::lock_session`]: crate::sessions::SessionManager::lock_session

pub mod catalog;
pub mod ddl;
pub mod editors;
pub mod explain;
pub mod grid;
// Drop impact for the object editors' drop flows.
pub mod impact;
pub mod live_redis;
pub mod live_sql;
pub mod metadata;
pub mod plans;
// B2: PostgreSQL LISTEN / NOTIFY.
pub mod notify;
// B1: Redis value editors, consumer groups and Pub/Sub.
pub mod pubsub;
pub mod redis_edit;
// Bulk TTL changes in the key browser.
pub mod redis_bulk;
// Redis modules (TimeSeries, probabilistic types, Search) and console help.
pub mod redis_modules;
// Sessions of a reloaded page.
pub mod reload;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use varve_activity::{ActivityEvent, ActivityEventBuilder, ErrorDetails};
use varve_catalog::Catalog;
use varve_catalog::explain::ExplainCancel;
use varve_core::Engine;
use varve_driver::{Dialect, DriverError, ErrorKind, Session};

use crate::commands::{CmdError, Services};
use crate::sessions::{SessionError, SessionInfo, SessionLock};

type CmdResult<T> = Result<T, CmdError>;

/// A database whose default collation was changed for a DDL run: the
/// server as it identifies itself, without the route (MySQL
/// `mysql:<server_uuid>`, MariaDB `mariadb:<server facts>`), and the
/// database name. Runs are serialized per key, whatever route reaches the
/// server, and there is at most one pending restore per key (it records
/// its route).
pub type CollationKey = (String, String);

/// A database collation that could not be restored after a DDL run. Kept
/// in `app.db` until it is restored, so it survives a restart.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRestore {
    /// The server as it identifies itself, without the route (the first
    /// part of [`CollationKey`]): `mysql:<server_uuid>`, or
    /// `mariadb:<server facts>`. Entries of versions before the route was
    /// kept apart hold `host:port` and never match a server: they can only
    /// be dismissed.
    pub server: String,
    /// The route the run reached the server through (host, port, SSH
    /// bastion). A restore through another route needs the user's
    /// confirmation.
    #[serde(default)]
    pub route: String,
    /// The database.
    pub database: String,
    /// Its own default collation, to restore.
    pub collation: String,
    /// Every collation runs left it at (each temporary collation they set).
    /// The restore applies only while the database has one of these;
    /// otherwise the entry is stale (another server with the same identity,
    /// a recreated database, a change by someone else).
    #[serde(default)]
    pub temporaries: Vec<String>,
    /// The single temporary collation of entries written by older
    /// versions; read into `temporaries`, never written.
    #[serde(default, skip_serializing)]
    pub temporary: Option<String>,
    /// True when the server reported a unique id (MySQL `server_uuid`).
    /// False for MariaDB, whose reported values can repeat across servers:
    /// such a restore never runs without the user's confirmation.
    #[serde(default)]
    pub verified: bool,
    /// The profile the failed run used.
    pub connection_id: String,
    /// When the entry was recorded (unix ms); 0 for older versions. The UI
    /// formats it in the user's time zone.
    #[serde(default)]
    pub recorded_at: i64,
}

impl PendingRestore {
    /// True when the database at `current` is where a run of this entry
    /// left it.
    #[must_use]
    pub fn left_at(&self, current: &str) -> bool {
        self.temporaries
            .iter()
            .any(|t| t.eq_ignore_ascii_case(current))
    }

    /// Brings an entry of an older version into the current shape: the
    /// single temporary collation into the list, and a key that held the
    /// route (`server@route`) split into server and route.
    fn upgraded(mut self) -> Self {
        if let Some(temporary) = self.temporary.take()
            && !self.left_at(&temporary)
        {
            self.temporaries.push(temporary);
        }
        if self.route.is_empty()
            && let Some((server, route)) = self.server.rsplit_once('@')
        {
            let (server, route) = (server.to_string(), route.to_string());
            self.server = server;
            self.route = route;
        }
        self
    }
}

/// Loads entries into one entry per server and database. Entries of
/// round 6 builds could hold one per route; two of them are merged:
///
/// - When one entry's own collation is among the other's temporary
///   collations, the other is the older run (the newer run took its
///   temporary collation for the database's own): the older entry's own
///   collation is the database's real one, and every collation either
///   left the database at counts as temporary.
/// - Otherwise it is ambiguous: the older entry's own collation is kept
///   (the newer one's becomes a temporary), and the merged entry is
///   unverified, so restoring it always asks the user.
///
/// Every merge is logged with both routes.
fn merge_pending(entries: Vec<PendingRestore>) -> HashMap<CollationKey, PendingRestore> {
    let mut map: HashMap<CollationKey, PendingRestore> = HashMap::new();
    for entry in entries.into_iter().map(PendingRestore::upgraded) {
        let key = (entry.server.clone(), entry.database.clone());
        let merged = match map.remove(&key) {
            None => entry,
            Some(existing) => {
                tracing::warn!(
                    database = %entry.database,
                    first_route = %existing.route,
                    second_route = %entry.route,
                    "merging two pending collation restores of one server and database"
                );
                merge_two(existing, entry)
            }
        };
        map.insert(key, merged);
    }
    map
}

fn merge_two(a: PendingRestore, b: PendingRestore) -> PendingRestore {
    // The newer run read the older run's temporary collation as the
    // database's own: the older entry's temporaries hold the newer one's
    // collation.
    let a_older = a.left_at(&b.collation) && !b.left_at(&a.collation);
    let b_older = b.left_at(&a.collation) && !a.left_at(&b.collation);
    let ambiguous = !a_older && !b_older;
    let (mut older, newer) =
        if b_older || (ambiguous && b.recorded_at > 0 && b.recorded_at < a.recorded_at) {
            (b, a)
        } else {
            (a, b)
        };
    for temporary in newer
        .temporaries
        .iter()
        .chain(std::iter::once(&newer.collation))
    {
        if !older.left_at(temporary) && !temporary.eq_ignore_ascii_case(&older.collation) {
            older.temporaries.push(temporary.clone());
        }
    }
    // Two routes recorded it: restoring always asks.
    older.verified = older.verified && newer.verified && older.route == newer.route && !ambiguous;
    older
}

/// `core_meta` key of the pending collation restores.
const PENDING_RESTORES_KEY: &str = "m2.pending_collation_restores";

/// M2 state shared by the commands: server-held plans and the DDL
/// collation bookkeeping.
#[derive(Debug)]
pub struct M2State {
    /// Plans waiting to be applied.
    pub plans: plans::PlanStore,
    /// PostgreSQL listeners of the tabs (LISTEN / NOTIFY panel).
    pub notify: notify::NotifyHub,
    /// B1: open Redis Pub/Sub subscriptions.
    pub pubsub: pubsub::PubSubHub,
    /// B1: background digest reads of Redis keys opened in the viewer.
    pub digests: redis_edit::DigestReads,
    /// Bulk TTL jobs of the key browsers.
    pub bulk_ttl: redis_bulk::BulkTtlJobs,
    /// Stop flags of the Redis memory analyses.
    pub memory_runs: live_redis::MemoryRuns,
    store: Arc<varve_store::Store>,
    /// Serializes DDL runs that change a database's collation.
    collation_locks: Mutex<HashMap<CollationKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Original collations of databases whose restore failed; the next run
    /// on the same database restores to this value, not to the temporary
    /// one.
    pending_restores: Mutex<HashMap<CollationKey, PendingRestore>>,
    /// Cancel requests per session, counted, so app sequences on a session
    /// (a DDL run and its collation restore) stop at their next step.
    cancels: Mutex<HashMap<String, u64>>,
    /// The cancel handles of the explains running (or waiting for their
    /// session) per session, by run id: a Cancel of the session cancels
    /// them as a whole, every step of them.
    explains: Mutex<HashMap<String, Vec<(u64, ExplainCancel)>>>,
    /// The id of the next explain run.
    next_explain: AtomicU64,
}

/// One explain run's cancel handle, registered with its session until the
/// run ends (dropped).
pub(crate) struct ExplainRun<'a> {
    state: &'a M2State,
    session_id: String,
    id: u64,
    /// The handle to pass to `explain_with_cancel`.
    pub(crate) cancel: ExplainCancel,
}

impl Drop for ExplainRun<'_> {
    fn drop(&mut self) {
        let mut map = self
            .state
            .explains
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(runs) = map.get_mut(&self.session_id) {
            runs.retain(|(id, _)| *id != self.id);
            if runs.is_empty() {
                map.remove(&self.session_id);
            }
        }
    }
}

impl M2State {
    /// The state, with the pending restores stored in `app.db`.
    #[must_use]
    pub fn new(store: Arc<varve_store::Store>) -> Self {
        let pending = Self::load_pending(&store).unwrap_or_else(|error| {
            tracing::warn!(%error, "could not read the pending collation restores");
            Vec::new()
        });
        Self {
            plans: plans::PlanStore::default(),
            notify: notify::NotifyHub::default(),
            pubsub: pubsub::PubSubHub::default(),
            digests: redis_edit::DigestReads::default(),
            bulk_ttl: redis_bulk::BulkTtlJobs::default(),
            memory_runs: live_redis::MemoryRuns::default(),
            store,
            collation_locks: Mutex::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
            explains: Mutex::new(HashMap::new()),
            next_explain: AtomicU64::new(0),
            pending_restores: Mutex::new(merge_pending(pending)),
        }
    }

    fn load_pending(store: &varve_store::Store) -> Result<Vec<PendingRestore>, String> {
        let text: Option<String> = store
            .with_conn(|c| {
                let mut stmt = c.prepare("SELECT value FROM core_meta WHERE key = ?1")?;
                let mut rows = stmt.query([PENDING_RESTORES_KEY])?;
                Ok(match rows.next()? {
                    Some(row) => Some(row.get(0)?),
                    None => None,
                })
            })
            .map_err(|e| e.to_string())?;
        match text {
            Some(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
            None => Ok(Vec::new()),
        }
    }

    fn save_pending(&self, list: &[PendingRestore]) {
        let saved = serde_json::to_string(list)
            .map_err(|e| e.to_string())
            .and_then(|json| {
                self.store
                    .with_conn(|c| {
                        c.execute(
                            "INSERT INTO core_meta (key, value) VALUES (?1, ?2) \
                             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                            [PENDING_RESTORES_KEY, json.as_str()],
                        )?;
                        Ok(())
                    })
                    .map_err(|e| e.to_string())
            });
        if let Err(error) = saved {
            tracing::warn!(%error, "could not store the pending collation restores");
        }
    }

    /// Notes a Cancel of a session.
    pub(crate) fn note_cancel(&self, session_id: &str) {
        if let Ok(mut map) = self.cancels.lock() {
            *map.entry(session_id.to_string()).or_default() += 1;
        }
        // Explains stop as a whole: the step that runs and every later one.
        let map = self
            .explains
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, cancel) in map.get(session_id).into_iter().flatten() {
            cancel.cancel();
        }
    }

    /// Registers an explain run on a session, before it waits for the
    /// session, so a Cancel of the session from now on stops it as a whole.
    pub(crate) fn start_explain(&self, session_id: &str) -> ExplainRun<'_> {
        let id = self.next_explain.fetch_add(1, Ordering::SeqCst);
        let cancel = ExplainCancel::new();
        self.explains
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session_id.to_string())
            .or_default()
            .push((id, cancel.clone()));
        ExplainRun {
            state: self,
            session_id: session_id.to_string(),
            id,
            cancel,
        }
    }

    /// How many explains of a session a Cancel would stop right now.
    #[must_use]
    pub fn running_explains(&self, session_id: &str) -> usize {
        self.explains
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .map_or(0, Vec::len)
    }

    /// Forgets the Cancel count of a closed session.
    pub(crate) fn forget_cancels(&self, session_id: &str) {
        if let Ok(mut map) = self.cancels.lock() {
            map.remove(session_id);
        }
    }

    /// How many Cancels a session has seen; compare before and after a
    /// step to know whether the user cancelled meanwhile.
    pub(crate) fn cancel_count(&self, session_id: &str) -> u64 {
        self.cancels
            .lock()
            .ok()
            .and_then(|map| map.get(session_id).copied())
            .unwrap_or_default()
    }

    pub(crate) fn collation_lock(&self, key: &CollationKey) -> Arc<tokio::sync::Mutex<()>> {
        match self.collation_locks.lock() {
            Ok(mut map) => {
                // Locks nobody holds or waits for are dropped on the way.
                map.retain(|_, lock| Arc::strong_count(lock) > 1);
                Arc::clone(map.entry(key.clone()).or_default())
            }
            Err(_) => Arc::default(),
        }
    }

    pub(crate) fn pending_restore(&self, key: &CollationKey) -> Option<PendingRestore> {
        self.pending_restores
            .lock()
            .ok()
            .and_then(|map| map.get(key).cloned())
    }

    /// Records (`Some`) or clears (`None`) the collation to restore, in
    /// memory and in `app.db`.
    pub(crate) fn set_pending_restore(&self, key: &CollationKey, pending: Option<PendingRestore>) {
        // Written while the map is locked, so saves land in the order the
        // changes were made and app.db never loses an entry.
        if let Ok(mut map) = self.pending_restores.lock() {
            let changed = match pending {
                Some(pending) => map.insert(key.clone(), pending.clone()) != Some(pending),
                None => map.remove(key).is_some(),
            };
            if changed {
                let list: Vec<PendingRestore> = map.values().cloned().collect();
                self.save_pending(&list);
            }
        }
    }

    /// Databases whose collation is still to be restored.
    #[must_use]
    pub fn pending_restores(&self) -> Vec<PendingRestore> {
        let mut list: Vec<PendingRestore> = self
            .pending_restores
            .lock()
            .map(|map| map.values().cloned().collect())
            .unwrap_or_default();
        list.sort_by(|a, b| (&a.server, &a.database).cmp(&(&b.server, &b.database)));
        list
    }
}

/// The catalog of each open session, opened on first use and dropped when
/// the session closes (the session event sink calls [`Self::forget`]).
#[derive(Default)]
pub struct CatalogCache {
    map: Mutex<HashMap<String, Arc<dyn Catalog>>>,
}

impl std::fmt::Debug for CatalogCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sessions: Vec<String> = self
            .map
            .lock()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        f.debug_struct("CatalogCache")
            .field("sessions", &sessions)
            .finish()
    }
}

impl CatalogCache {
    /// The cached catalog of a session.
    #[must_use]
    pub fn get(&self, session_id: &str) -> Option<Arc<dyn Catalog>> {
        self.map
            .lock()
            .ok()
            .and_then(|map| map.get(session_id).cloned())
    }

    /// Caches the catalog of a session.
    pub fn insert(&self, session_id: &str, catalog: Arc<dyn Catalog>) {
        if let Ok(mut map) = self.map.lock() {
            map.insert(session_id.to_string(), catalog);
        }
    }

    /// Drops the catalog of a closed session.
    pub fn forget(&self, session_id: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.remove(session_id);
        }
    }

    /// True when a catalog is cached for the session.
    #[must_use]
    pub fn contains(&self, session_id: &str) -> bool {
        self.map
            .lock()
            .is_ok_and(|map| map.contains_key(session_id))
    }
}

/// Milliseconds since `start`.
pub(crate) fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The SQL dialect of an engine; `None` for Redis.
pub(crate) fn sql_dialect(engine: Engine) -> Option<Dialect> {
    match engine {
        Engine::Postgres => Some(Dialect::Postgres),
        Engine::Mysql | Engine::Mariadb => Some(Dialect::Mysql),
        Engine::Redis => None,
    }
}

/// The environment as stored in safety events (`production`, ...).
pub(crate) fn environment_label(info: &SessionInfo) -> String {
    format!("{:?}", info.environment).to_ascii_lowercase()
}

/// Error details for the activity log.
pub(crate) fn error_details(error: &DriverError) -> ErrorDetails {
    ErrorDetails {
        code: error.code.clone(),
        message: error.message.clone(),
    }
}

/// The error for a call on a session of the wrong engine.
pub(crate) fn unsupported(message: impl Into<String>) -> CmdError {
    CmdError::Session {
        error: SessionError::Driver {
            error: DriverError::new(ErrorKind::Unsupported, message),
        },
    }
}

impl Services {
    /// The description of an open session.
    pub(crate) fn session_info(&self, session_id: &str) -> CmdResult<SessionInfo> {
        Ok(self.sessions.info(session_id)?)
    }

    /// Locks a session for a direct call.
    pub(crate) async fn lock(&self, session_id: &str) -> CmdResult<SessionLock> {
        Ok(self.sessions.lock_session(session_id).await?)
    }

    /// The driver session behind a lock. A session closed while the call
    /// waited for its lock is reported as not open.
    pub(crate) fn locked<'a>(
        lock: &'a SessionLock,
        session_id: &str,
    ) -> CmdResult<&'a dyn Session> {
        lock.session().ok_or_else(|| CmdError::Session {
            error: SessionError::NotFound {
                session_id: session_id.to_string(),
            },
        })
    }

    /// Refuses a change on a read-only session.
    pub(crate) fn require_writable(info: &SessionInfo, what: &str) -> CmdResult<()> {
        if info.read_only {
            Err(CmdError::ReadOnly {
                message: format!("This session is read-only, so {what} is not allowed."),
            })
        } else {
            Ok(())
        }
    }

    /// Refuses a call on a session of another engine.
    pub(crate) fn require_engine(
        info: &SessionInfo,
        engines: &[Engine],
        what: &str,
    ) -> CmdResult<()> {
        if engines.contains(&info.engine) {
            Ok(())
        } else {
            Err(unsupported(format!(
                "{what} is not available on a {:?} session",
                info.engine
            )))
        }
    }

    /// Asks for confirmation: when `needed` and not `confirmed`, records
    /// `destructive.blocked` and returns
    /// [`SessionError::ConfirmationRequired`] with `statements` as the
    /// labels the dialog shows.
    pub(crate) fn require_confirmation(
        &self,
        info: &SessionInfo,
        needed: bool,
        confirmed: bool,
        statements: Vec<String>,
    ) -> CmdResult<()> {
        if !needed || confirmed {
            return Ok(());
        }
        self.record(
            info,
            ActivityEvent::destructive_blocked(statements.join(","), &environment_label(info)),
        );
        Err(CmdError::Session {
            error: SessionError::ConfirmationRequired {
                environment: info.environment,
                statements,
            },
        })
    }

    /// Records an activity event for a session. Logging never fails the
    /// call: problems are traced, as the session event bridge does.
    pub(crate) fn record(&self, info: &SessionInfo, builder: ActivityEventBuilder) {
        let builder = if info.connection_id == "unsaved" {
            builder
        } else {
            builder.connection(info.connection_id.clone())
        };
        match builder.build() {
            Ok(event) => {
                if let Err(error) = self.activity.record(event) {
                    tracing::warn!(%error, "could not record an activity event");
                }
            }
            Err(error) => tracing::warn!(%error, "invalid activity event"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_for_new_cases_have_ui_shapes() -> Result<(), serde_json::Error> {
        assert_eq!(
            serde_json::to_value(CmdError::ReadOnly {
                message: "no".into()
            })?,
            serde_json::json!({ "kind": "readOnly", "message": "no" })
        );
        let catalog =
            serde_json::to_value(CmdError::from(varve_catalog::CatalogError::NotFound {
                message: "gone".into(),
            }))?;
        assert_eq!(
            catalog,
            serde_json::json!({ "kind": "catalog", "error": { "kind": "notFound", "message": "gone" } })
        );
        let live = serde_json::to_value(CmdError::from(
            varve_live_stats::sql::LiveError::OwnConnection { id: 7 },
        ))?;
        assert_eq!(
            live,
            serde_json::json!({ "kind": "live", "error": { "kind": "ownConnection", "id": 7 } })
        );
        let wrong = serde_json::to_value(unsupported("x"))?;
        assert_eq!(wrong["kind"], "session");
        assert_eq!(wrong["error"]["error"]["kind"], "unsupported");
        Ok(())
    }

    #[test]
    fn pending_restores_survive_a_restart() -> Result<(), Box<dyn std::error::Error>> {
        let store = Arc::new(varve_store::Store::open_in_memory()?);
        store.migrate(&[varve_store::core_migrations()])?;
        let state = M2State::new(Arc::clone(&store));
        let key = ("db:3306".to_string(), "shop".to_string());
        state.set_pending_restore(
            &key,
            Some(PendingRestore {
                server: key.0.clone(),
                database: key.1.clone(),
                collation: "utf8mb4_bin".into(),
                route: "db:3306".into(),
                temporaries: vec!["latin1_swedish_ci".into()],
                temporary: None,
                verified: true,
                connection_id: "p1".into(),
                recorded_at: 1,
            }),
        );
        let again = M2State::new(Arc::clone(&store));
        assert_eq!(
            again.pending_restore(&key).map(|p| p.collation).as_deref(),
            Some("utf8mb4_bin")
        );
        // Entries of older versions (no temporary collation) still load.
        let old: Vec<PendingRestore> = serde_json::from_str(
            r#"[{"server":"db:3306","database":"x","collation":"c","connectionId":"p"}]"#,
        )?;
        assert_eq!(old[0].temporary, None);
        assert!(!old[0].verified);
        let r5: Vec<PendingRestore> = serde_json::from_str(
            r#"[{"server":"mysql:u1@db:3306","database":"x","collation":"c","temporary":"t","verified":true,"connectionId":"p"}]"#,
        )?;
        // Two round 6 entries of one server and database (two routes): the
        // older run's collation is the database's own.
        let r6: Vec<PendingRestore> = serde_json::from_str(
            r#"[{"server":"mysql:u1@localhost:3306","database":"shop","collation":"utf8mb4_bin","temporary":"latin1_swedish_ci","verified":true,"connectionId":"p"},
                {"server":"mysql:u1@127.0.0.1:3306","database":"shop","collation":"utf8mb4_general_ci","temporary":"utf8mb4_bin","verified":true,"connectionId":"p"}]"#,
        )?;
        let merged = merge_pending(r6);
        let one = merged
            .get(&("mysql:u1".to_string(), "shop".to_string()))
            .ok_or("not merged")?;
        assert_eq!(one.collation, "utf8mb4_general_ci");
        assert!(one.left_at("utf8mb4_bin") && one.left_at("latin1_swedish_ci"));
        assert!(!one.verified, "two routes: restoring asks");
        // Ambiguous: neither entry's collation is the other's temporary.
        let odd: Vec<PendingRestore> = serde_json::from_str(
            r#"[{"server":"mysql:u1@a:1","database":"x","collation":"c1","temporary":"t1","verified":true,"connectionId":"p","recordedAt":20},
                {"server":"mysql:u1@b:1","database":"x","collation":"c2","temporary":"t2","verified":true,"connectionId":"p","recordedAt":10}]"#,
        )?;
        let odd = merge_pending(odd);
        let odd = odd
            .get(&("mysql:u1".to_string(), "x".to_string()))
            .ok_or("not merged")?;
        assert_eq!(odd.collation, "c2", "the older recording wins");
        assert!(odd.left_at("c1") && odd.left_at("t1") && odd.left_at("t2"));
        assert!(!odd.verified);
        let upgraded = r5[0].clone().upgraded();
        assert_eq!(upgraded.server, "mysql:u1");
        assert_eq!(upgraded.route, "db:3306");
        assert!(upgraded.left_at("T"));
        again.set_pending_restore(&key, None);
        assert!(M2State::new(store).pending_restores().is_empty());
        Ok(())
    }

    #[test]
    fn dialects_by_engine() {
        assert_eq!(sql_dialect(Engine::Postgres), Some(Dialect::Postgres));
        assert_eq!(sql_dialect(Engine::Mariadb), Some(Dialect::Mysql));
        assert_eq!(sql_dialect(Engine::Redis), None);
    }
}
