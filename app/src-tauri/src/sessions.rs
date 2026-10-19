//! Open database sessions, one per editor tab or tool (design doc 3.3), with
//! the safety rules of design doc 3.2.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use varve_core::Engine;
use varve_driver::{
    CancelHandle, ConnectParams, Dialect, Driver, DriverError, ErrorKind, ExecOptions, ExecOutput,
    ServerInfo, Session, StatementKind, StatementResult, classify,
};
use varve_records::{ConnectionProfile, Environment};

/// Something kept alive as long as a session is open, such as an SSH tunnel lease.
pub type SessionGuard = Box<dyn Any + Send + Sync>;

/// Public description of an open session.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Session id.
    pub id: String,
    /// Connection profile id.
    pub connection_id: String,
    /// Engine reported by the server.
    pub engine: Engine,
    /// Server identity.
    pub server: ServerInfo,
    /// Environment tag of the profile when the session opened.
    pub environment: Environment,
    /// Read-only session.
    pub read_only: bool,
    /// Opened, unix ms.
    pub opened_at: i64,
}

/// Why a session operation failed. Sent to the UI as JSON.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SessionError {
    /// The driver failed.
    #[error("{0}")]
    Driver(DriverError),
    /// No session with that id is open.
    #[error("the session is not open")]
    #[serde(rename_all = "camelCase")]
    NotFound {
        /// Requested id.
        session_id: String,
    },
    /// The script contains destructive statements on a connection that
    /// requires confirmation; repeat the call with `confirmed = true`.
    #[error("this statement needs confirmation")]
    #[serde(rename_all = "camelCase")]
    ConfirmationRequired {
        /// Environment of the connection.
        environment: Environment,
        /// Destructive statement kinds found, in order.
        statements: Vec<String>,
    },
    /// No driver is registered for the engine.
    #[error("no driver for {0}")]
    NoDriver(Engine),
}

impl From<DriverError> for SessionError {
    fn from(error: DriverError) -> Self {
        SessionError::Driver(error)
    }
}

/// Outcome of a statement run, for the activity recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Finished.
    Success,
    /// Failed.
    Failure,
    /// Cancelled by the user.
    Cancelled,
}

/// Something the activity recorder should know about (design doc 9.2).
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// A session opened.
    Opened {
        /// Session.
        info: SessionInfo,
        /// Opened through an SSH tunnel.
        via_ssh: bool,
        /// Time to connect.
        duration_ms: u64,
    },
    /// Opening a session failed.
    OpenFailed {
        /// Connection profile.
        connection_id: String,
        /// Engine of the profile.
        engine: Engine,
        /// What went wrong.
        error: DriverError,
        /// Time until it failed.
        duration_ms: u64,
    },
    /// A script ran (or failed, or was cancelled).
    Executed {
        /// Session.
        info: SessionInfo,
        /// The script as typed. The recorder redacts it before storing.
        text: String,
        /// Statement kinds in the script.
        kinds: Vec<StatementKind>,
        /// Outcome.
        outcome: Outcome,
        /// Rows returned by the last row set, if any.
        rows: Option<u64>,
        /// Rows affected by the last command, if any.
        rows_affected: Option<u64>,
        /// Wall time.
        duration_ms: u64,
        /// Error, when it failed.
        error: Option<DriverError>,
        /// True when the user confirmed destructive statements.
        confirmed_destructive: bool,
    },
    /// A destructive script was stopped because it was not confirmed.
    DestructiveBlocked {
        /// Session.
        info: SessionInfo,
        /// Destructive statement kinds.
        kinds: Vec<StatementKind>,
    },
    /// A session closed.
    Closed {
        /// Session.
        info: SessionInfo,
        /// How long it was open.
        duration_ms: u64,
    },
}

/// Receives session events.
pub type EventSink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

struct OpenSession {
    info: SessionInfo,
    session: tokio::sync::Mutex<Option<Box<dyn Session>>>,
    cancel: Arc<dyn CancelHandle>,
    confirm_destructive: bool,
    opened: Instant,
    _guard: Option<SessionGuard>,
}

/// Manages open sessions.
pub struct SessionManager {
    drivers: HashMap<Engine, Arc<dyn Driver>>,
    sessions: Mutex<HashMap<String, Arc<OpenSession>>>,
    events: EventSink,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("drivers", &self.drivers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Redis commands that need confirmation on protected connections.
const REDIS_DESTRUCTIVE: &[&str] = &["FLUSHALL", "FLUSHDB", "SWAPDB", "SHUTDOWN", "DEBUG", "KEYS"];

/// Destructive statements in a script, by engine.
fn destructive_statements(engine: Engine, text: &str) -> Vec<String> {
    match engine {
        Engine::Postgres | Engine::Mysql | Engine::Mariadb => {
            let dialect = if engine == Engine::Postgres {
                Dialect::Postgres
            } else {
                Dialect::Mysql
            };
            classify(text, dialect)
                .into_iter()
                .filter(|k| k.is_destructive())
                .map(|k| format!("{k:?}"))
                .collect()
        }
        Engine::Redis => text
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_ascii_uppercase)
            .filter(|cmd| REDIS_DESTRUCTIVE.contains(&cmd.as_str()))
            .collect(),
    }
}

fn kinds_of(engine: Engine, text: &str) -> Vec<StatementKind> {
    match engine {
        Engine::Postgres => classify(text, Dialect::Postgres),
        Engine::Mysql | Engine::Mariadb => classify(text, Dialect::Mysql),
        Engine::Redis => Vec::new(),
    }
}

impl SessionManager {
    /// Creates a manager with the given drivers.
    #[must_use]
    pub fn new(drivers: Vec<Arc<dyn Driver>>, events: EventSink) -> Self {
        let mut map = HashMap::new();
        for driver in drivers {
            for engine in driver.engines() {
                map.insert(*engine, Arc::clone(&driver));
            }
        }
        Self {
            drivers: map,
            sessions: Mutex::new(HashMap::new()),
            events,
        }
    }

    fn driver(&self, engine: Engine) -> Result<Arc<dyn Driver>, SessionError> {
        self.drivers
            .get(&engine)
            .cloned()
            .ok_or(SessionError::NoDriver(engine))
    }

    fn get(&self, id: &str) -> Result<Arc<OpenSession>, SessionError> {
        self.sessions
            .lock()
            .ok()
            .and_then(|map| map.get(id).cloned())
            .ok_or_else(|| SessionError::NotFound {
                session_id: id.to_string(),
            })
    }

    /// Opens a session for a profile with resolved parameters. `guard` (for
    /// example a tunnel lease) is dropped when the session closes.
    ///
    /// # Errors
    /// Driver errors, or no driver for the engine.
    pub async fn open(
        &self,
        profile: &ConnectionProfile,
        params: ConnectParams,
        guard: Option<SessionGuard>,
    ) -> Result<SessionInfo, SessionError> {
        let started = Instant::now();
        let driver = self.driver(profile.engine)?;
        let via_ssh = profile.ssh.enabled;
        let session = match driver.connect(&params).await {
            Ok(session) => session,
            Err(error) => {
                (self.events)(SessionEvent::OpenFailed {
                    connection_id: profile.id.clone(),
                    engine: profile.engine,
                    error: error.clone(),
                    duration_ms: elapsed_ms(started),
                });
                return Err(error.into());
            }
        };
        let info = SessionInfo {
            id: uuid::Uuid::now_v7().to_string(),
            connection_id: profile.id.clone(),
            engine: session.server_info().engine,
            server: session.server_info().clone(),
            environment: profile.environment,
            read_only: params.read_only,
            opened_at: now_ms(),
        };
        let open = Arc::new(OpenSession {
            info: info.clone(),
            cancel: session.cancel_handle(),
            session: tokio::sync::Mutex::new(Some(session)),
            confirm_destructive: profile.options.confirm_destructive
                || profile.environment == Environment::Production,
            opened: Instant::now(),
            _guard: guard,
        });
        if let Ok(mut map) = self.sessions.lock() {
            map.insert(info.id.clone(), open);
        }
        (self.events)(SessionEvent::Opened {
            info: info.clone(),
            via_ssh,
            duration_ms: elapsed_ms(started),
        });
        Ok(info)
    }

    /// Connects, reads the server identity and disconnects.
    ///
    /// # Errors
    /// Driver errors.
    pub async fn test(
        &self,
        engine: Engine,
        params: &ConnectParams,
    ) -> Result<(ServerInfo, u64), SessionError> {
        let started = Instant::now();
        let session = self.driver(engine)?.connect(params).await?;
        let server = session.server_info().clone();
        session.ping().await?;
        let latency = elapsed_ms(started);
        session.close().await?;
        Ok((server, latency))
    }

    /// Runs a script on a session.
    ///
    /// # Errors
    /// [`SessionError::ConfirmationRequired`] when the script is destructive on
    /// a protected connection and `confirmed` is false; driver errors.
    pub async fn execute(
        &self,
        session_id: &str,
        text: &str,
        confirmed: bool,
        options: &ExecOptions,
    ) -> Result<ExecOutput, SessionError> {
        let open = self.get(session_id)?;
        let engine = open.info.engine;
        let kinds = kinds_of(engine, text);
        let destructive = destructive_statements(engine, text);
        if open.confirm_destructive && !destructive.is_empty() && !confirmed {
            (self.events)(SessionEvent::DestructiveBlocked {
                info: open.info.clone(),
                kinds: kinds
                    .iter()
                    .copied()
                    .filter(|k| k.is_destructive())
                    .collect(),
            });
            return Err(SessionError::ConfirmationRequired {
                environment: open.info.environment,
                statements: destructive,
            });
        }

        let started = Instant::now();
        let guard = open.session.lock().await;
        let Some(session) = guard.as_ref() else {
            return Err(SessionError::NotFound {
                session_id: session_id.to_string(),
            });
        };
        let result = session.execute(text, options).await;
        drop(guard);

        let (outcome, rows, rows_affected, error) = match &result {
            Ok(output) => {
                let rows = output.results.iter().rev().find_map(|r| match r {
                    StatementResult::Rows { rows, .. } => Some(rows.len() as u64),
                    StatementResult::Command { .. } => None,
                });
                let affected = output.results.iter().rev().find_map(|r| match r {
                    StatementResult::Command { rows_affected, .. } => *rows_affected,
                    StatementResult::Rows { .. } => None,
                });
                (Outcome::Success, rows, affected, None)
            }
            Err(e) if e.kind == ErrorKind::Cancelled => {
                (Outcome::Cancelled, None, None, Some(e.clone()))
            }
            Err(e) => (Outcome::Failure, None, None, Some(e.clone())),
        };
        (self.events)(SessionEvent::Executed {
            info: open.info.clone(),
            text: text.to_string(),
            kinds,
            outcome,
            rows,
            rows_affected,
            duration_ms: elapsed_ms(started),
            error,
            confirmed_destructive: confirmed && !destructive.is_empty(),
        });
        Ok(result?)
    }

    /// Cancels the running statement of a session. Succeeds when idle.
    ///
    /// # Errors
    /// Unknown session or driver errors.
    pub async fn cancel(&self, session_id: &str) -> Result<(), SessionError> {
        let open = self.get(session_id)?;
        Ok(open.cancel.cancel().await?)
    }

    /// Runs `f` with the driver session, for engine-specific features.
    ///
    /// # Errors
    /// Unknown session.
    pub async fn with_session<T>(
        &self,
        session_id: &str,
        f: impl AsyncFnOnce(&dyn Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let open = self.get(session_id)?;
        let guard = open.session.lock().await;
        let Some(session) = guard.as_ref() else {
            return Err(SessionError::NotFound {
                session_id: session_id.to_string(),
            });
        };
        f(session.as_ref()).await
    }

    /// Closes a session.
    ///
    /// # Errors
    /// Unknown session or driver errors while closing.
    pub async fn close(&self, session_id: &str) -> Result<(), SessionError> {
        let open = self
            .sessions
            .lock()
            .ok()
            .and_then(|mut map| map.remove(session_id))
            .ok_or_else(|| SessionError::NotFound {
                session_id: session_id.to_string(),
            })?;
        let session = open.session.lock().await.take();
        (self.events)(SessionEvent::Closed {
            info: open.info.clone(),
            duration_ms: elapsed_ms(open.opened),
        });
        if let Some(session) = session {
            session.close().await?;
        }
        Ok(())
    }

    /// Open sessions.
    #[must_use]
    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .lock()
            .map(|map| map.values().map(|s| s.info.clone()).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use varve_driver::Column;
    use varve_records::{ConnectionInput, PERSONAL_WORKSPACE, Records};
    use varve_store::{Store, core_migrations};

    use super::*;

    struct FakeCancel(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl CancelHandle for FakeCancel {
        async fn cancel(&self) -> Result<(), DriverError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeSession {
        info: ServerInfo,
        cancels: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Session for FakeSession {
        fn server_info(&self) -> &ServerInfo {
            &self.info
        }
        fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
            Arc::new(FakeCancel(Arc::clone(&self.cancels)))
        }
        async fn execute(&self, sql: &str, _: &ExecOptions) -> Result<ExecOutput, DriverError> {
            if sql.contains("fail") {
                return Err(DriverError::new(ErrorKind::Query, "syntax error").with_code("42601"));
            }
            Ok(ExecOutput {
                results: vec![StatementResult::Rows {
                    columns: vec![Column {
                        name: "x".into(),
                        type_name: "int4".into(),
                    }],
                    rows: vec![vec![varve_driver::Value::Int(1)]],
                    truncated: false,
                }],
                notices: vec![],
                elapsed_ms: 1,
            })
        }
        async fn ping(&self) -> Result<(), DriverError> {
            Ok(())
        }
        async fn close(self: Box<Self>) -> Result<(), DriverError> {
            Ok(())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct FakeDriver(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Driver for FakeDriver {
        fn engines(&self) -> &'static [Engine] {
            &[Engine::Postgres]
        }
        async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
            if params.host == "unreachable" {
                return Err(DriverError::new(ErrorKind::Connect, "connection refused"));
            }
            Ok(Box::new(FakeSession {
                info: ServerInfo {
                    engine: Engine::Postgres,
                    version: "17.2".into(),
                    major: 17,
                    minor: 2,
                    patch: None,
                },
                cancels: Arc::clone(&self.0),
            }))
        }
    }

    type Events = Arc<Mutex<Vec<SessionEvent>>>;
    type Setup = (SessionManager, ConnectionProfile, Events, Arc<AtomicUsize>);

    fn setup(environment: Environment) -> Result<Setup, Box<dyn std::error::Error>> {
        let store = Store::open_in_memory()?;
        store.migrate(&[core_migrations()])?;
        let records = Records::new(Arc::new(store));
        let profile = records.create_connection(
            PERSONAL_WORKSPACE,
            &ConnectionInput {
                folder_id: None,
                name: "Orders".into(),
                color: None,
                engine: Engine::Postgres,
                host: "db".into(),
                port: None,
                database: None,
                username: None,
                environment,
                tls: Default::default(),
                ssh: Default::default(),
                options: varve_records::ConnectionOptions {
                    confirm_destructive: false,
                    ..Default::default()
                },
            },
        )?;
        let events: Events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&events);
        let cancels = Arc::new(AtomicUsize::new(0));
        let manager = SessionManager::new(
            vec![Arc::new(FakeDriver(Arc::clone(&cancels)))],
            Arc::new(move |e| {
                if let Ok(mut list) = sink_events.lock() {
                    list.push(e);
                }
            }),
        );
        Ok((manager, profile, events, cancels))
    }

    fn params() -> ConnectParams {
        ConnectParams::new(Engine::Postgres, "db", 5432)
    }

    #[tokio::test]
    async fn production_requires_confirmation_for_destructive_scripts()
    -> Result<(), Box<dyn std::error::Error>> {
        let (manager, profile, events, _) = setup(Environment::Production)?;
        let session = manager.open(&profile, params(), None).await?;
        let blocked = manager
            .execute(
                &session.id,
                "select 1; drop table t",
                false,
                &ExecOptions::default(),
            )
            .await;
        assert!(
            matches!(blocked, Err(SessionError::ConfirmationRequired { environment: Environment::Production, ref statements }) if statements == &vec!["Drop".to_string()])
        );
        manager
            .execute(
                &session.id,
                "select 1; drop table t",
                true,
                &ExecOptions::default(),
            )
            .await?;
        manager
            .execute(&session.id, "select 1", false, &ExecOptions::default())
            .await?;
        let list = events.lock().map_err(|_| "poisoned")?;
        assert!(
            list.iter()
                .any(|e| matches!(e, SessionEvent::DestructiveBlocked { .. }))
        );
        assert!(list.iter().any(|e| matches!(
            e,
            SessionEvent::Executed {
                confirmed_destructive: true,
                ..
            }
        )));
        Ok(())
    }

    #[tokio::test]
    async fn development_without_the_option_runs_destructive_statements()
    -> Result<(), Box<dyn std::error::Error>> {
        let (manager, profile, _, _) = setup(Environment::Development)?;
        let session = manager.open(&profile, params(), None).await?;
        manager
            .execute(&session.id, "drop table t", false, &ExecOptions::default())
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn failures_cancel_and_close_are_reported() -> Result<(), Box<dyn std::error::Error>> {
        let (manager, profile, events, cancels) = setup(Environment::Development)?;
        let session = manager.open(&profile, params(), None).await?;
        let failed = manager
            .execute(&session.id, "select fail", false, &ExecOptions::default())
            .await;
        assert!(
            matches!(failed, Err(SessionError::Driver(ref e)) if e.code.as_deref() == Some("42601"))
        );
        manager.cancel(&session.id).await?;
        assert_eq!(cancels.load(Ordering::SeqCst), 1);
        assert_eq!(manager.list().len(), 1);
        manager.close(&session.id).await?;
        assert!(manager.list().is_empty());
        assert!(matches!(
            manager.close(&session.id).await,
            Err(SessionError::NotFound { .. })
        ));
        let list = events.lock().map_err(|_| "poisoned")?;
        assert!(list.iter().any(|e| matches!(
            e,
            SessionEvent::Executed {
                outcome: Outcome::Failure,
                ..
            }
        )));
        assert!(
            list.iter()
                .any(|e| matches!(e, SessionEvent::Closed { .. }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn open_failure_is_reported_and_test_measures_latency()
    -> Result<(), Box<dyn std::error::Error>> {
        let (manager, profile, events, _) = setup(Environment::Development)?;
        let bad = ConnectParams::new(Engine::Postgres, "unreachable", 5432);
        assert!(manager.open(&profile, bad, None).await.is_err());
        assert!(
            events
                .lock()
                .map_err(|_| "poisoned")?
                .iter()
                .any(|e| matches!(e, SessionEvent::OpenFailed { .. }))
        );
        let (server, _latency) = manager.test(Engine::Postgres, &params()).await?;
        assert_eq!(server.version, "17.2");
        Ok(())
    }

    #[test]
    fn redis_destructive_commands() {
        assert_eq!(
            destructive_statements(Engine::Redis, "get a\nflushdb\n keys *"),
            vec!["FLUSHDB", "KEYS"]
        );
        assert!(destructive_statements(Engine::Redis, "set a 1").is_empty());
    }
}
