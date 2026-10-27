//! Open database sessions, one per editor tab or tool (design doc 3.3), with
//! the safety rules of design doc 3.2.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use varve_core::Engine;
use varve_driver::{
    CancelHandle, ConnectParams, Dialect, Driver, DriverError, Durability, ErrorKind, ExecOptions,
    ExecOutput, Notice, PartialOutput, RanStatement, ServerInfo, Session, StatementKind,
    StatementResult, TlsMode, classify,
};
use varve_driver_mysql::{MysqlSession, SessionOverrides};
use varve_driver_redis::{CommandRisk, RedisSession, parse_script};
use varve_records::{ConnectionProfile, Environment};

/// Something kept alive as long as a session is open, such as an SSH tunnel lease.
pub type SessionGuard = Box<dyn Any + Send + Sync>;

/// What a session is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionPurpose {
    /// Opened by the user: an editor tab or tool.
    #[default]
    User,
    /// Opened by the app for the object browser (read-only, shared per
    /// profile and database, hidden from [`SessionManager::list`]).
    Metadata,
    /// Opened by the app for the live server panels (process list, kill,
    /// variables, Redis panels), one per profile, hidden from
    /// [`SessionManager::list`].
    Admin,
}

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
    /// Database the session is connected to (PostgreSQL) or its default
    /// schema (MySQL, MariaDB); `None` for Redis and when the profile names
    /// none. Differs from the profile's database for sessions opened with
    /// `open_database_session`.
    pub database: Option<String>,
    /// Read-only session.
    pub read_only: bool,
    /// Destructive statements need confirmation on this session: the
    /// profile asks for it or the connection is tagged production.
    pub confirm_destructive: bool,
    /// What the session is for.
    pub purpose: SessionPurpose,
    /// Opened, unix ms.
    pub opened_at: i64,
    /// Whether the connection is encrypted, as connected.
    pub encryption: Encryption,
    /// The connection identity the session was opened with
    /// ([`connection_identity`]); not sent to the UI.
    #[serde(skip)]
    pub identity: String,
    /// Which server the session reached, read when it connected
    /// ([`server_identity`]): PostgreSQL `system_identifier`, timeline and
    /// role, MySQL `@@server_uuid`, MariaDB `@@gtid_domain_id`,
    /// `@@server_id`, `@@hostname`, `@@port` and `@@datadir`.
    /// `None` when it could not be read (no privilege, Redis). Two sessions
    /// with the same value are on the same server, whatever their profiles
    /// say now (tunnels, edited connections). Not sent to the UI.
    #[serde(skip)]
    pub server_identity: Option<String>,
}

/// How a session's connection is protected, as connected (TLS mode
/// `prefer` may end either way).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Encryption {
    /// The connection uses TLS; `None` when the driver cannot tell.
    pub tls: Option<bool>,
    /// The profile's TLS mode.
    pub mode: TlsMode,
    /// The password went to the server as is over a connection without TLS
    /// ([`Session::password_in_clear`]): Redis `AUTH` on plain TCP, or a
    /// PostgreSQL server that asked for the password in clear text (pg_hba
    /// `password`, `ldap`, `radius`, `pam`). MySQL / MariaDB never send it
    /// as is (Varve does not enable the cleartext plugin), and PostgreSQL
    /// SCRAM and MD5 do not either.
    pub password_unencrypted: bool,
}

impl Encryption {
    /// The encryption of `session`, opened with `params`.
    #[must_use]
    pub fn of(session: &dyn Session, params: &ConnectParams) -> Self {
        Self {
            tls: session.tls_in_use(),
            mode: params.tls.mode,
            password_unencrypted: session.password_in_clear(),
        }
    }
}

/// True when two connection identities ([`connection_identity`]) name the
/// same endpoint: engine, host, port and SSH settings. Used only where the
/// server itself could not be identified.
#[must_use]
pub fn same_endpoint(a: &str, b: &str) -> bool {
    let parse = |text: &str| serde_json::from_str::<serde_json::Value>(text).ok();
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => ["engine", "host", "port", "ssh"]
            .iter()
            .all(|key| a.get(key) == b.get(key)),
        _ => false,
    }
}

/// What identifies the server, account and session setup of a profile:
/// engine, host, port, user, environment, TLS, SSH and options. The
/// database is left out (a PostgreSQL session per database of the same
/// server is the same identity). Sessions of a tab only talk to the
/// identity the tab was opened with.
#[must_use]
pub fn connection_identity(profile: &ConnectionProfile) -> String {
    serde_json::json!({
        "engine": profile.engine,
        "host": profile.host.to_ascii_lowercase(),
        "port": profile.port,
        "username": profile.username,
        "environment": profile.environment,
        "tls": profile.tls,
        "ssh": profile.ssh,
        "options": profile.options,
    })
    .to_string()
}

/// Why a session operation failed. Sent to the UI as JSON.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SessionError {
    /// The driver failed. A struct variant, because `DriverError` has its
    /// own `kind` field that would collide with the tag.
    #[error("{error}")]
    Driver {
        /// What the driver reported.
        error: DriverError,
    },
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
    /// The call must not run inside the transaction the session has open
    /// (MySQL would commit it implicitly; PostgreSQL would make the change
    /// part of it).
    #[error("{message}")]
    OpenTransaction {
        /// User-facing message naming the effect.
        message: String,
    },
    /// No driver is registered for the engine.
    #[error("no driver for {engine:?}")]
    #[serde(rename_all = "camelCase")]
    NoDriver {
        /// The engine.
        engine: Engine,
    },
}

impl From<DriverError> for SessionError {
    fn from(error: DriverError) -> Self {
        SessionError::Driver { error }
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
        /// The profile's init SQL the driver ran while connecting.
        init_sql: Option<String>,
        /// Whether a transaction was open right after connecting (driver
        /// status flag, so after the init SQL), `None` when unknown.
        init_in_transaction: Option<bool>,
        /// PostgreSQL: whether the session user is a superuser
        /// (`is_superuser`), `None` when unknown or another engine.
        superuser: Option<bool>,
        /// The server notices the init SQL sent.
        init_notices: Vec<Notice>,
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
        /// The profile's init SQL, which may have run in part.
        init_sql: Option<String>,
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
        /// Error, when it failed (boxed: it can carry a partial output).
        error: Option<Box<DriverError>>,
        /// True when the user confirmed destructive statements.
        confirmed_destructive: bool,
        /// Labels of what was destructive (statement kinds, Redis command
        /// names, or the caller's own labels).
        destructive: Vec<String>,
        /// Whether a transaction was open before the run, from the
        /// driver's status flag (`None`: unknown).
        in_transaction_before: Option<bool>,
        /// The statements the run executed, when the driver knows them
        /// exactly ([`Session::last_statements`]).
        ran: Option<Vec<RanStatement>>,
        /// Whether a transaction was open after the run, from the driver's
        /// status flag (`None`: unknown).
        in_transaction_after: Option<bool>,
        /// The notices and warnings the server sent during the run (also
        /// those of a failed run), for privilege logging.
        notices: Vec<Notice>,
        /// PostgreSQL, after a script that may switch the current role:
        /// whether the current role is a superuser now (`None`: unknown or
        /// not read).
        superuser_after: Option<bool>,
    },
    /// A connection test connected (or failed to). The driver ran the
    /// profile's init SQL as for a session.
    Tested {
        /// Connection profile (or the unsaved preview's id).
        connection_id: String,
        /// Engine of the profile.
        engine: Engine,
        /// The server, when the test connected.
        server: Option<ServerInfo>,
        /// The profile's init SQL.
        init_sql: Option<String>,
        /// Whether a transaction was open right after connecting.
        init_in_transaction: Option<bool>,
        /// PostgreSQL: whether the user is a superuser.
        superuser: Option<bool>,
        /// The server notices the init SQL sent.
        init_notices: Vec<Notice>,
        /// The error, when the test failed.
        error: Option<DriverError>,
        /// How the test connection was encrypted, when it connected.
        encryption: Option<Encryption>,
    },
    /// A script was refused by a session rule other than confirmation (an
    /// open transaction).
    PolicyBlocked {
        /// Session.
        info: SessionInfo,
        /// The rule, for example `open_transaction`.
        policy: String,
        /// Statement kinds of the refused script.
        kinds: Vec<StatementKind>,
    },
    /// A destructive script was stopped because it was not confirmed.
    DestructiveBlocked {
        /// Session.
        info: SessionInfo,
        /// Destructive statement kinds.
        kinds: Vec<StatementKind>,
        /// Labels of what was destructive, as shown to the user.
        statements: Vec<String>,
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
    /// The page that opened it (user sessions).
    page: u64,
    session: Arc<tokio::sync::Mutex<Option<Box<dyn Session>>>>,
    cancel: Arc<dyn CancelHandle>,
    confirm_destructive: bool,
    opened: Instant,
    _guard: Option<SessionGuard>,
}

/// Exclusive access to an open session, from [`SessionManager::lock_session`].
pub struct SessionLock {
    guard: tokio::sync::OwnedMutexGuard<Option<Box<dyn Session>>>,
}

impl SessionLock {
    /// The driver session.
    #[must_use]
    pub fn session(&self) -> Option<&dyn Session> {
        self.guard.as_deref()
    }
}

/// Manages open sessions.
pub struct SessionManager {
    drivers: HashMap<Engine, Arc<dyn Driver>>,
    sessions: Mutex<HashMap<String, Arc<OpenSession>>>,
    events: EventSink,
    /// The page of the main webview that user sessions belong to: bumped
    /// when a page starts loading (see `m2/reload.rs`).
    page: std::sync::atomic::AtomicU64,
    /// What closing the previous page's sessions did, for the new page.
    reload: Mutex<ReloadReport>,
    /// Held while the previous page's sessions are being closed, so the
    /// new page reads a complete report.
    reload_lock: Arc<tokio::sync::Mutex<()>>,
}

/// A user session closed because the page that opened it was reloaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadedSession {
    /// The profile it was opened from.
    pub connection_id: String,
    /// A transaction was open and was rolled back by the close; `None` when
    /// a statement was still running (it was cancelled) so the state was
    /// not readable.
    pub in_transaction: Option<bool>,
}

/// What a page reload closed (`reload_report`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadReport {
    /// Sessions of the previous page that were open.
    pub sessions: Vec<ReloadedSession>,
    /// Sessions the previous page was still opening; closed as soon as
    /// they were open (by profile).
    pub opening: Vec<String>,
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

/// Destructive statements in a script, by engine. Redis asks the server
/// (through `COMMAND INFO`, cached per session) instead of a fixed list.
async fn destructive_statements(
    engine: Engine,
    text: &str,
    session: &dyn Session,
) -> Result<Vec<String>, DriverError> {
    match engine {
        Engine::Postgres | Engine::Mysql | Engine::Mariadb => {
            let dialect = if engine == Engine::Postgres {
                Dialect::Postgres
            } else {
                Dialect::Mysql
            };
            Ok(classify(text, dialect)
                .into_iter()
                .filter(|k| k.is_destructive())
                .map(|k| format!("{k:?}"))
                .collect())
        }
        Engine::Redis => {
            let Some(redis) = session.as_any().downcast_ref::<RedisSession>() else {
                return Err(DriverError::new(
                    ErrorKind::Protocol,
                    "the Redis session has an unexpected type",
                ));
            };
            redis_destructive_commands(redis, text).await
        }
    }
}

/// The destructive commands of a Redis console script, by name. Every
/// command of every line is parsed the way the driver runs it (quotes
/// removed, so `"FLUSHALL"` is `FLUSHALL`) and assessed by its name and
/// subcommand with the server's `COMMAND INFO`.
///
/// # Errors
/// Unparsable text and connection errors.
pub async fn redis_destructive_commands(
    redis: &RedisSession,
    text: &str,
) -> Result<Vec<String>, DriverError> {
    let mut found = Vec::new();
    for command in parse_script(text)? {
        let head: Vec<&[u8]> = command.args.iter().take(2).map(Vec::as_slice).collect();
        let line = varve_redis_syntax::join_args(&head);
        if redis.script_risk(&line).await? == CommandRisk::Destructive {
            found.push(command.name());
        }
    }
    Ok(found)
}

/// The database a PostgreSQL session is connected to, or `None` when it
/// cannot be read.
async fn current_database(session: &dyn Session) -> Option<String> {
    let output = session
        .execute(
            "SELECT pg_catalog.current_database()",
            &ExecOptions { max_rows: 1 },
        )
        .await
        .ok()?;
    output.results.iter().find_map(|r| match r {
        StatementResult::Rows { rows, .. } => match rows.first().and_then(|row| row.first()) {
            Some(varve_driver::Value::Text(name)) => Some(name.clone()),
            _ => None,
        },
        StatementResult::Command { .. } => None,
    })
}

/// Which server a session reached, as one text: PostgreSQL
/// `system_identifier`, timeline and role (only when the session may read
/// `pg_control_system` and `pg_control_checkpoint`,
/// so the query never fails and never disturbs a transaction the init SQL
/// opened), MySQL `@@server_uuid`, MariaDB `@@gtid_domain_id`,
/// `@@server_id`, `@@hostname`, `@@port` and `@@datadir`. `None` when it
/// cannot be read.
async fn server_identity(engine: Engine, session: &dyn Session) -> Option<String> {
    let sql = match engine {
        Engine::Postgres => return pg_server_identity(session).await,
        Engine::Mysql => "SELECT CONCAT('mysql:', @@server_uuid)",
        // MariaDB has no server UUID: a composite that can still collide
        // (server_id defaults to 1), so matches on it are only "possible".
        Engine::Mariadb => {
            "SELECT CONCAT('mariadb:', @@gtid_domain_id, ':', @@server_id, ':', @@hostname, ':', \
             @@port, ':', @@datadir)"
        }
        Engine::Redis => return None,
    };
    let output = session
        .execute(sql, &ExecOptions { max_rows: 1 })
        .await
        .ok()?;
    output.results.iter().find_map(|r| match r {
        StatementResult::Rows { rows, .. } => match rows.first().and_then(|row| row.first()) {
            Some(varve_driver::Value::Text(id)) if !id.is_empty() => Some(id.clone()),
            _ => None,
        },
        StatementResult::Command { .. } => None,
    })
}

/// The pieces of a PostgreSQL server identity, each read only where the
/// session may read it (so the query never fails): the system identifier,
/// the role, and the current timeline from the current WAL file name on a
/// primary or the WAL receiver on a standby, with the last checkpoint's
/// timeline (which lags right after a promotion) as the fallback.
const PG_IDENTITY_SQL: &str = "SELECT \
     CASE WHEN pg_catalog.has_function_privilege('pg_catalog.pg_control_system()', 'EXECUTE') \
       THEN (SELECT c.system_identifier::pg_catalog.text FROM pg_catalog.pg_control_system() c) END \
       AS system_id, \
     pg_catalog.pg_is_in_recovery() AS standby, \
     CASE WHEN NOT pg_catalog.pg_is_in_recovery() \
       AND pg_catalog.has_function_privilege('pg_catalog.pg_current_wal_lsn()', 'EXECUTE') \
       AND pg_catalog.has_function_privilege('pg_catalog.pg_walfile_name(pg_catalog.pg_lsn)', 'EXECUTE') \
       THEN pg_catalog.pg_walfile_name(pg_catalog.pg_current_wal_lsn()) END AS walfile, \
     (SELECT w.received_tli::pg_catalog.text FROM pg_catalog.pg_stat_wal_receiver w LIMIT 1) \
       AS received_tli, \
     CASE WHEN pg_catalog.has_function_privilege('pg_catalog.pg_control_checkpoint()', 'EXECUTE') \
       THEN (SELECT k.timeline_id::pg_catalog.text FROM pg_catalog.pg_control_checkpoint() k) END \
       AS checkpoint_tli";

async fn pg_server_identity(session: &dyn Session) -> Option<String> {
    let rows = varve_catalog::rows::query(session, PG_IDENTITY_SQL)
        .await
        .ok()?;
    let row = rows.single("the server identity").ok()?;
    pg_identity_text(
        row.text("system_id").ok()?.as_deref(),
        row.bool("standby").ok()?.unwrap_or(false),
        row.text("walfile").ok()?.as_deref(),
        row.text("received_tli").ok()?.as_deref(),
        row.text("checkpoint_tli").ok()?.as_deref(),
    )
}

/// `pg:<system identifier>:tl<timeline>:<primary|standby>`: a streaming
/// standby shares the system identifier with its primary (the role tells
/// them apart), a promoted standby keeps it but moves to a new timeline.
/// The timeline is the current one where readable (the WAL file name on a
/// primary, the WAL receiver on a standby), else the last checkpoint's.
/// `None` without the system identifier or any timeline.
fn pg_identity_text(
    system_id: Option<&str>,
    standby: bool,
    walfile: Option<&str>,
    received_tli: Option<&str>,
    checkpoint_tli: Option<&str>,
) -> Option<String> {
    let system_id = system_id.filter(|s| !s.is_empty())?;
    let current = if standby {
        received_tli.and_then(|t| t.trim().parse::<u32>().ok())
    } else {
        walfile.and_then(walfile_timeline)
    };
    let timeline = current.or_else(|| checkpoint_tli.and_then(|t| t.trim().parse::<u32>().ok()))?;
    let role = if standby { "standby" } else { "primary" };
    Some(format!("pg:{system_id}:tl{timeline}:{role}"))
}

/// The timeline of a WAL file name: its first 8 hex digits
/// (`00000002000000000000000A` is timeline 2).
fn walfile_timeline(name: &str) -> Option<u32> {
    let head = name.get(..8)?;
    if name.len() != 24 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(head, 16).ok()
}

/// Whether the PostgreSQL session user is a superuser (`is_superuser`, a
/// setting every session can read), `None` when it cannot be read.
async fn is_superuser(session: &dyn Session) -> Option<bool> {
    let output = session
        .execute(
            "SELECT pg_catalog.current_setting('is_superuser')",
            &ExecOptions { max_rows: 1 },
        )
        .await
        .ok()?;
    output.results.iter().find_map(|r| match r {
        StatementResult::Rows { rows, .. } => match rows.first().and_then(|row| row.first()) {
            Some(varve_driver::Value::Text(value)) => Some(value == "on"),
            _ => None,
        },
        StatementResult::Command { .. } => None,
    })
}

/// Whether the session has a transaction open: the driver's status flag
/// from the server's last reply ([`Session::in_transaction`]) when it has
/// one, else a query. `None` when neither can tell (Redis; MySQL drivers
/// without the flag and without access to the Performance Schema or
/// `INNODB_TRX`).
///
/// # Errors
/// Connection errors.
pub async fn open_transaction(
    engine: Engine,
    session: &dyn Session,
) -> Result<Option<bool>, DriverError> {
    // The server's own status flag from the last reply: no round trip and
    // no privilege. The queries below are only the fallback.
    if let Some(open) = session.in_transaction() {
        return Ok(Some(open));
    }
    let queries: &[&str] = match engine {
        Engine::Postgres => {
            &["SELECT pg_catalog.transaction_timestamp() <> pg_catalog.statement_timestamp()"]
        }
        Engine::Mariadb => &["SELECT @@in_transaction"],
        // MySQL has no session flag. The Performance Schema shows the
        // thread's explicit transaction at once; INNODB_TRX (needs
        // PROCESS) is refreshed only every 0.1 s, so it is the fallback.
        Engine::Mysql => &[
            "SELECT COUNT(*) FROM performance_schema.events_transactions_current t \
             JOIN performance_schema.threads th ON th.THREAD_ID = t.THREAD_ID \
             WHERE th.PROCESSLIST_ID = CONNECTION_ID() AND t.STATE = 'ACTIVE' \
             AND t.AUTOCOMMIT = 'NO'",
            "SELECT COUNT(*) FROM information_schema.INNODB_TRX \
             WHERE trx_mysql_thread_id = CONNECTION_ID()",
        ],
        Engine::Redis => return Ok(None),
    };
    let mut answer = None;
    for sql in queries {
        match session.execute(sql, &ExecOptions { max_rows: 1 }).await {
            Ok(output) => {
                answer = Some(output);
                break;
            }
            Err(error) if error.kind == ErrorKind::ConnectionLost => return Err(error),
            // PostgreSQL refuses every statement in an aborted transaction.
            Err(_) if engine == Engine::Postgres => return Ok(Some(true)),
            Err(_) => {}
        }
    }
    let Some(output) = answer else {
        return Ok(None);
    };
    let value = output.results.iter().find_map(|r| match r {
        StatementResult::Rows { rows, .. } => rows.first().and_then(|row| row.first()).cloned(),
        StatementResult::Command { .. } => None,
    });
    Ok(match value {
        Some(varve_driver::Value::Bool(b)) => Some(b),
        Some(varve_driver::Value::Int(i)) => Some(i != 0),
        Some(varve_driver::Value::UInt(u)) => Some(u != 0),
        Some(varve_driver::Value::Text(t) | varve_driver::Value::Decimal(t)) => {
            Some(!matches!(t.as_str(), "0" | "f" | "false"))
        }
        _ => None,
    })
}

/// Extra rules for one run through [`SessionManager::execute_with`].
#[derive(Debug, Clone, Default)]
pub struct ExecSpec {
    /// The caller judged the script destructive (an editor plan that loses
    /// data): confirmation is needed whatever the environment, and these
    /// labels are shown when the classifier finds nothing destructive.
    pub destructive: Option<Vec<String>>,
    /// Refuse when the session has a transaction open; `effect` says what
    /// running anyway would do.
    pub outside_transaction: Option<String>,
    /// The script opens a transaction: roll it back, still under the
    /// session lock, when the script fails.
    pub rollback_on_error: bool,
    /// MySQL and MariaDB session variables for this call only.
    pub overrides: Option<SessionOverrides>,
}

/// The body of [`SessionManager::execute_with`], on a session whose lock
/// the caller holds: classification, confirmation, the open transaction
/// check, the run (with per-call MySQL variables), the rollback on failure
/// and the activity events.
async fn run_locked(
    events: &EventSink,
    open: &OpenSession,
    session: &dyn Session,
    text: &str,
    confirmed: bool,
    options: &ExecOptions,
    spec: &ExecSpec,
) -> Result<ExecOutput, SessionError> {
    let engine = open.info.engine;
    let kinds = kinds_of(engine, text);
    let mut destructive = destructive_statements(engine, text, session).await?;
    let needs_confirmation =
        spec.destructive.is_some() || (open.confirm_destructive && !destructive.is_empty());
    if destructive.is_empty()
        && let Some(labels) = &spec.destructive
    {
        destructive.clone_from(labels);
    }
    if needs_confirmation && !confirmed {
        (events)(SessionEvent::DestructiveBlocked {
            info: open.info.clone(),
            kinds: kinds
                .iter()
                .copied()
                .filter(|k| k.is_destructive())
                .collect(),
            statements: destructive.clone(),
        });
        return Err(SessionError::ConfirmationRequired {
            environment: open.info.environment,
            statements: destructive,
        });
    }
    if let Some(effect) = &spec.outside_transaction
        && open_transaction(engine, session).await? == Some(true)
    {
        (events)(SessionEvent::PolicyBlocked {
            info: open.info.clone(),
            policy: "open_transaction".into(),
            kinds,
        });
        return Err(SessionError::OpenTransaction {
            message: format!(
                "The session has an open transaction; {effect} Commit or roll it back first."
            ),
        });
    }

    let in_transaction_before = session.in_transaction();
    let started = Instant::now();
    let mut result = match &spec.overrides {
        Some(overrides) => match session.as_any().downcast_ref::<MysqlSession>() {
            Some(mysql) => mysql.execute_with_session(overrides, text, options).await,
            None => Err(DriverError::new(
                ErrorKind::Unsupported,
                "per-call session variables need a MySQL or MariaDB session",
            )),
        },
        None => session.execute(text, options).await,
    };
    // Read before anything else runs on the session.
    let mut ran = session.last_statements();
    if spec.rollback_on_error
        && let Err(error) = &mut result
    {
        let note = match session.execute("ROLLBACK", options).await {
            Ok(_) => {
                // What was still in the transaction is undone now.
                let undone = |durability: &mut Durability| {
                    if *durability == Durability::InTransaction {
                        *durability = Durability::RolledBack;
                    }
                };
                if let Some(partial) = error.partial.as_deref_mut() {
                    partial.completed.iter_mut().for_each(undone);
                }
                if let Some(ran) = ran.as_mut() {
                    ran.iter_mut().for_each(|r| undone(&mut r.durability));
                }
                "The transaction was rolled back; nothing changed.".to_string()
            }
            Err(rollback) => format!("Rolling back also failed: {rollback}"),
        };
        error.detail = Some(match error.detail.take() {
            Some(detail) => format!("{detail}\n{note}"),
            None => note,
        });
    }

    let in_transaction_after = session.in_transaction();
    // A script that may switch the current role (SET ROLE, SET SESSION
    // AUTHORIZATION, ...) may change whether it is a superuser: read it
    // again for privilege logging, after the run's own state was read.
    let superuser_after =
        if engine == Engine::Postgres && crate::activity_bridge::may_switch_role(text) {
            is_superuser(session).await
        } else {
            None
        };
    let notices = match &result {
        Ok(output) => output.notices.clone(),
        Err(error) => error
            .partial
            .as_deref()
            .map(|partial| partial.notices.clone())
            .unwrap_or_default(),
    };
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
        Err(e) if e.kind == ErrorKind::Cancelled => (
            Outcome::Cancelled,
            None,
            None,
            Some(Box::new(event_error(e))),
        ),
        Err(e) => (Outcome::Failure, None, None, Some(Box::new(event_error(e)))),
    };
    (events)(SessionEvent::Executed {
        info: open.info.clone(),
        text: text.to_string(),
        kinds,
        outcome,
        rows,
        rows_affected,
        duration_ms: elapsed_ms(started),
        error,
        confirmed_destructive: confirmed && !destructive.is_empty(),
        destructive,
        in_transaction_before,
        ran,
        in_transaction_after,
        notices,
        superuser_after,
    });
    Ok(result?)
}

/// Where a session through an SSH tunnel really goes. The driver connects
/// to the tunnel's local end (`127.0.0.1` and a random port), so its
/// connect errors name that end; [`TunnelRoute::explain`] makes them name
/// the server and the bastion instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelRoute {
    /// The database server as the bastion reaches it, `host:port`.
    pub target: String,
    /// The SSH bastion, `host:port`.
    pub bastion: String,
}

/// `host:port`, with brackets around an IPv6 address.
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

impl TunnelRoute {
    /// The route of a profile; `None` without an SSH tunnel.
    #[must_use]
    pub fn of(profile: &ConnectionProfile) -> Option<Self> {
        profile.ssh.enabled.then(|| Self {
            target: host_port(&profile.host, profile.port),
            bastion: host_port(&profile.ssh.host, profile.ssh.port),
        })
    }

    /// How messages name the route: `target through SSH bastion host:port`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} through SSH bastion {}", self.target, self.bastion)
    }

    /// The route with the tunnel's local end that `params` point at.
    #[must_use]
    pub fn end(&self, params: &ConnectParams) -> TunnelEnd {
        TunnelEnd {
            route: self.clone(),
            local: host_port(&params.host, params.port),
            connect_timeout: params.connect_timeout,
        }
    }

    /// [`TunnelEnd::connect_error`] for a connect with `params`.
    #[must_use]
    pub fn explain(&self, error: DriverError, params: &ConnectParams) -> DriverError {
        self.end(params).connect_error(error)
    }
}

/// A [`TunnelRoute`] and the tunnel's local end a driver connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelEnd {
    /// The route.
    pub route: TunnelRoute,
    /// The tunnel's local end, `host:port`.
    pub local: String,
    /// The connect timeout of the connect.
    pub connect_timeout: std::time::Duration,
}

impl TunnelEnd {
    /// Rewrites a connect error, for every engine. A message that names
    /// the local end names the route instead ([`Self::name_route`]), so the
    /// driver's own wording stays: "did not answer" only where the driver
    /// knows the server never answered, "timed out during the session
    /// setup" where it answered and then stalled. A timeout that names no
    /// address (PostgreSQL) gets the route added. The driver's own message
    /// stays in the detail (when the error has none) for troubleshooting.
    #[must_use]
    pub fn connect_error(&self, error: DriverError) -> DriverError {
        if error.message.contains(&self.local) || error.kind != ErrorKind::Timeout {
            return self.name_route(error);
        }
        let original = error.message.clone();
        let mut error = error;
        error.message = format!("{original} ({})", self.route.label());
        self.keep_original(error, &original)
    }

    /// Replaces the local end in the message of any error (a lost
    /// connection, a failed reconnect) with the route; other errors are
    /// returned as they are.
    #[must_use]
    pub fn name_route(&self, mut error: DriverError) -> DriverError {
        if !error.message.contains(&self.local) {
            return error;
        }
        let original = error.message.clone();
        error.message = error.message.replace(&self.local, &self.route.label());
        self.keep_original(error, &original)
    }

    fn keep_original(&self, mut error: DriverError, original: &str) -> DriverError {
        if error.detail.is_none() {
            error.detail = Some(format!(
                "Connected to the tunnel's local end {}: {original}",
                self.local
            ));
        }
        error
    }
}

/// A driver error for an activity event: everything but the partial
/// output's results and notices, which the recorder never needs (and which
/// can be large).
fn event_error(error: &DriverError) -> DriverError {
    DriverError {
        kind: error.kind,
        message: error.message.clone(),
        code: error.code.clone(),
        detail: error.detail.clone(),
        hint: error.hint.clone(),
        position: error.position,
        partial: error.partial.as_deref().map(|partial| {
            Box::new(PartialOutput {
                results: Vec::new(),
                notices: Vec::new(),
                elapsed_ms: partial.elapsed_ms,
                failed_statement: partial.failed_statement,
                completed: partial.completed.clone(),
            })
        }),
    }
}

/// A session locked for a sequence of scripts, from
/// [`SessionManager::lock_for_scripts`]. Dropping it releases the lock.
pub struct LockedSession<'m> {
    events: &'m EventSink,
    open: Arc<OpenSession>,
    guard: tokio::sync::OwnedMutexGuard<Option<Box<dyn Session>>>,
}

impl LockedSession<'_> {
    /// The driver session, for reads that are not user scripts (catalog
    /// queries, the transaction state).
    #[must_use]
    pub fn session(&self) -> Option<&dyn Session> {
        self.guard.as_deref()
    }

    /// The session's description.
    #[must_use]
    pub fn info(&self) -> &SessionInfo {
        &self.open.info
    }

    /// Runs a script as [`SessionManager::execute_with`] does, without
    /// releasing the lock.
    ///
    /// # Errors
    /// As [`SessionManager::execute_with`].
    pub async fn execute_with(
        &self,
        text: &str,
        confirmed: bool,
        options: &ExecOptions,
        spec: &ExecSpec,
    ) -> Result<ExecOutput, SessionError> {
        let Some(session) = self.guard.as_deref() else {
            return Err(SessionError::NotFound {
                session_id: self.open.info.id.clone(),
            });
        };
        run_locked(
            self.events,
            &self.open,
            session,
            text,
            confirmed,
            options,
            spec,
        )
        .await
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
            page: std::sync::atomic::AtomicU64::new(0),
            reload: Mutex::new(ReloadReport::default()),
            reload_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// The current page of the main webview.
    #[must_use]
    pub fn page(&self) -> u64 {
        self.page.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A page starts loading: user sessions opened from now on belong to
    /// it. Returns the new page and a guard to hold while the previous
    /// page's sessions are closed (`None` when a close is still running).
    pub fn begin_page(&self) -> (u64, Option<tokio::sync::OwnedMutexGuard<()>>) {
        // Under the map lock: see `open_for`.
        let _map = self.sessions.lock();
        let page = self.page.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        (page, Arc::clone(&self.reload_lock).try_lock_owned().ok())
    }

    /// User sessions opened before `page`.
    #[must_use]
    pub fn sessions_before_page(&self, page: u64) -> Vec<SessionInfo> {
        self.sessions
            .lock()
            .map(|map| {
                map.values()
                    .filter(|s| s.info.purpose == SessionPurpose::User && s.page < page)
                    .map(|s| s.info.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the session has a transaction open now, without waiting:
    /// `None` while a statement runs on it or when it cannot tell.
    pub async fn in_transaction_now(&self, session_id: &str) -> Option<bool> {
        let open = self.get(session_id).ok()?;
        let guard = Arc::clone(&open.session).try_lock_owned().ok()?;
        let session = guard.as_ref()?;
        open_transaction(open.info.engine, session.as_ref())
            .await
            .ok()
            .flatten()
    }

    /// Adds closed sessions to the report the next page reads.
    pub fn record_reload(&self, sessions: Vec<ReloadedSession>) {
        if let Ok(mut report) = self.reload.lock() {
            report.sessions.extend(sessions);
        }
    }

    /// The report of the last reload, emptied; waits while the previous
    /// page's sessions are still being closed.
    pub async fn take_reload_report(&self) -> ReloadReport {
        let _done = self.reload_lock.lock().await;
        self.reload
            .lock()
            .map(|mut report| std::mem::take(&mut *report))
            .unwrap_or_default()
    }

    fn driver(&self, engine: Engine) -> Result<Arc<dyn Driver>, SessionError> {
        self.drivers
            .get(&engine)
            .cloned()
            .ok_or(SessionError::NoDriver { engine })
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
        self.open_for(profile, params, guard, SessionPurpose::User)
            .await
    }

    /// Opens a session for a purpose; [`Self::open`] opens user sessions.
    ///
    /// # Errors
    /// Driver errors, or no driver for the engine.
    pub async fn open_for(
        &self,
        profile: &ConnectionProfile,
        params: ConnectParams,
        guard: Option<SessionGuard>,
        purpose: SessionPurpose,
    ) -> Result<SessionInfo, SessionError> {
        let started = Instant::now();
        // The page this open belongs to: if it is reloaded meanwhile, no
        // tab will ever use the session.
        let page = self.page();
        let driver = self.driver(profile.engine)?;
        let via_ssh = profile.ssh.enabled;
        let session = match driver.connect(&params).await {
            Ok(session) => session,
            Err(error) => {
                let error = match TunnelRoute::of(profile) {
                    Some(route) => route.explain(error, &params),
                    None => error,
                };
                // A missing TLS key passphrase is a prompt, not a failed
                // connect (like a missing SSH password): nothing is logged.
                if error.code.as_deref() != Some(varve_driver::tls_key::PASSPHRASE_REQUIRED) {
                    (self.events)(SessionEvent::OpenFailed {
                        connection_id: profile.id.clone(),
                        engine: profile.engine,
                        error: error.clone(),
                        duration_ms: elapsed_ms(started),
                        init_sql: params.init_sql.clone(),
                    });
                }
                return Err(error.into());
            }
        };
        // A Redis session reconnects by itself: its later messages name the
        // route, not the tunnel's local end.
        if let Some(route) = TunnelRoute::of(profile)
            && let Some(redis) = session.as_any().downcast_ref::<RedisSession>()
        {
            redis.set_endpoint_label(route.label());
        }
        // Read before any query of Varve's own: the state the init SQL
        // left.
        let init_in_transaction = session.in_transaction();
        let init_notices = session.connect_notices();
        let superuser = match session.server_info().engine {
            Engine::Postgres => is_superuser(session.as_ref()).await,
            _ => None,
        };
        let confirm_destructive =
            profile.options.confirm_destructive || profile.environment == Environment::Production;
        // A PostgreSQL session without a named database is on the server's
        // default one; record which, so the tab's catalog and editors
        // always work on the same database.
        let database = match (&params.database, session.server_info().engine) {
            (None, Engine::Postgres) => current_database(session.as_ref()).await,
            (named, _) => named.clone(),
        };
        let server_identity = server_identity(session.server_info().engine, session.as_ref()).await;
        let info = SessionInfo {
            id: uuid::Uuid::now_v7().to_string(),
            connection_id: profile.id.clone(),
            engine: session.server_info().engine,
            server: session.server_info().clone(),
            environment: profile.environment,
            database,
            read_only: params.read_only,
            confirm_destructive,
            purpose,
            opened_at: now_ms(),
            encryption: Encryption::of(session.as_ref(), &params),
            identity: connection_identity(profile),
            server_identity,
        };
        let cancel = session.cancel_handle();
        let slot = Arc::new(tokio::sync::Mutex::new(Some(session)));
        // Checked and inserted under the map lock, which `begin_page` also
        // takes: either the reload sees this session, or this open sees the
        // reload.
        let stale = match self.sessions.lock() {
            Ok(mut map) => {
                let stale = purpose == SessionPurpose::User && self.page() != page;
                if !stale {
                    map.insert(
                        info.id.clone(),
                        Arc::new(OpenSession {
                            info: info.clone(),
                            page,
                            cancel,
                            session: Arc::clone(&slot),
                            confirm_destructive,
                            opened: Instant::now(),
                            _guard: guard,
                        }),
                    );
                }
                stale
            }
            Err(_) => false,
        };
        if stale {
            // Opened for a page that is gone: no tab will use it.
            if let Some(session) = slot.lock().await.take() {
                let _ = session.close().await;
            }
            if let Ok(mut report) = self.reload.lock() {
                report.opening.push(profile.id.clone());
            }
            return Err(SessionError::Driver {
                error: DriverError::new(
                    ErrorKind::Cancelled,
                    "The window was reloaded while the session was opening; it was closed.",
                ),
            });
        }
        (self.events)(SessionEvent::Opened {
            info: info.clone(),
            via_ssh,
            duration_ms: elapsed_ms(started),
            init_sql: params.init_sql.clone(),
            init_in_transaction,
            superuser,
            init_notices,
        });
        Ok(info)
    }

    /// Connects, reads the server identity and disconnects. `route` is the
    /// SSH tunnel `params` point into, if any ([`TunnelRoute::of`]).
    ///
    /// # Errors
    /// Driver errors.
    pub async fn test(
        &self,
        connection_id: &str,
        engine: Engine,
        params: &ConnectParams,
        route: Option<&TunnelRoute>,
    ) -> Result<(ServerInfo, u64), SessionError> {
        let started = Instant::now();
        let session = match self.driver(engine)?.connect(params).await {
            Ok(session) => session,
            Err(error) => {
                let error = match route {
                    Some(route) => route.explain(error, params),
                    None => error,
                };
                (self.events)(SessionEvent::Tested {
                    connection_id: connection_id.to_string(),
                    engine,
                    server: None,
                    init_sql: params.init_sql.clone(),
                    init_in_transaction: None,
                    superuser: None,
                    init_notices: Vec::new(),
                    error: Some(error.clone()),
                    encryption: None,
                });
                return Err(error.into());
            }
        };
        let server = session.server_info().clone();
        // The driver ran the profile's init SQL: record it like an open.
        let init_in_transaction = session.in_transaction();
        let init_notices = session.connect_notices();
        let superuser = match server.engine {
            Engine::Postgres => is_superuser(session.as_ref()).await,
            _ => None,
        };
        (self.events)(SessionEvent::Tested {
            connection_id: connection_id.to_string(),
            engine,
            server: Some(server.clone()),
            init_sql: params.init_sql.clone(),
            init_in_transaction,
            superuser,
            init_notices,
            error: None,
            encryption: Some(Encryption::of(session.as_ref(), params)),
        });
        // A Redis session may reconnect for the ping: its messages name the
        // route, like those of an open session.
        let name_route = |error: DriverError| match route {
            Some(route) => route.end(params).name_route(error),
            None => error,
        };
        if let Some(route) = route
            && let Some(redis) = session.as_any().downcast_ref::<RedisSession>()
        {
            redis.set_endpoint_label(route.label());
        }
        session.ping().await.map_err(name_route)?;
        let latency = elapsed_ms(started);
        session.close().await.map_err(name_route)?;
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
        self.execute_with(session_id, text, confirmed, options, &ExecSpec::default())
            .await
    }

    /// [`Self::execute`] with extra rules: caller-judged destructive
    /// scripts, refusing open transactions, rolling back on failure and
    /// per-call MySQL session variables. Classification, confirmation and
    /// the activity events are the same as for [`Self::execute`]; the
    /// transaction check, the script and the rollback all run under one
    /// session lock.
    ///
    /// # Errors
    /// As [`Self::execute`], and [`SessionError::OpenTransaction`].
    pub async fn execute_with(
        &self,
        session_id: &str,
        text: &str,
        confirmed: bool,
        options: &ExecOptions,
        spec: &ExecSpec,
    ) -> Result<ExecOutput, SessionError> {
        let open = self.get(session_id)?;
        let guard = Arc::clone(&open.session).lock_owned().await;
        let Some(session) = guard.as_deref() else {
            return Err(SessionError::NotFound {
                session_id: session_id.to_string(),
            });
        };
        run_locked(&self.events, &open, session, text, confirmed, options, spec).await
    }

    /// Locks a session for a sequence of scripts that must not interleave
    /// with other calls on it (for example ALTER, DDL and restore). Every
    /// script still runs with the rules and events of
    /// [`Self::execute_with`].
    ///
    /// # Errors
    /// Unknown or closed session.
    pub async fn lock_for_scripts(
        &self,
        session_id: &str,
    ) -> Result<LockedSession<'_>, SessionError> {
        let open = self.get(session_id)?;
        let guard = Arc::clone(&open.session).lock_owned().await;
        if guard.is_none() {
            return Err(SessionError::NotFound {
                session_id: session_id.to_string(),
            });
        }
        Ok(LockedSession {
            events: &self.events,
            open,
            guard,
        })
    }

    /// Cancels the running statement of a session. Succeeds when idle.
    ///
    /// # Errors
    /// Unknown session or driver errors.
    pub async fn cancel(&self, session_id: &str) -> Result<(), SessionError> {
        let open = self.get(session_id)?;
        Ok(open.cancel.cancel().await?)
    }

    /// The transaction state of a session without waiting for it: `None`
    /// while a statement runs on it (its lock is held); otherwise the
    /// driver's flag from the server's last reply, itself `None` when the
    /// driver cannot tell. Never sends anything to the server.
    #[must_use]
    pub fn transaction_state_now(&self, session_id: &str) -> Option<Option<bool>> {
        let open = self.get(session_id).ok()?;
        let guard = open.session.try_lock().ok()?;
        Some(guard.as_ref().and_then(|s| s.in_transaction()))
    }

    /// A whole number read on a session without waiting for it (for example
    /// a session variable): `None` while a statement runs on it, or when
    /// the query fails or returns no number. Not logged; use only for
    /// side-effect-free reads of the session's own settings.
    pub async fn int_now(&self, session_id: &str, sql: &str) -> Option<i64> {
        let open = self.get(session_id).ok()?;
        let guard = Arc::clone(&open.session).try_lock_owned().ok()?;
        let session = guard.as_ref()?;
        let rows = varve_catalog::rows::query(session.as_ref(), sql)
            .await
            .ok()?;
        let row = rows.iter().next()?;
        let column = rows.columns().first()?.clone();
        row.int(&column).ok().flatten()
    }

    /// Locks a session for engine-specific calls. Statements on the same
    /// session wait until the lock is dropped.
    ///
    /// # Errors
    /// Unknown or closed session.
    pub async fn lock_session(&self, session_id: &str) -> Result<SessionLock, SessionError> {
        let open = self.get(session_id)?;
        let guard = Arc::clone(&open.session).lock_owned().await;
        if guard.is_none() {
            return Err(SessionError::NotFound {
                session_id: session_id.to_string(),
            });
        }
        Ok(SessionLock { guard })
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
        // The session is out of the map, so `cancel` cannot reach it any
        // more: while something still runs on it, cancel that here (again
        // every half second, for sequences of statements) before taking the
        // lock, instead of waiting for it to finish on its own.
        let mut guard = loop {
            if let Ok(guard) = Arc::clone(&open.session).try_lock_owned() {
                break guard;
            }
            if let Err(error) = open.cancel.cancel().await {
                tracing::debug!(%error, "cancelling the running statement of a closing session");
            }
            if let Ok(guard) = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                Arc::clone(&open.session).lock_owned(),
            )
            .await
            {
                break guard;
            }
        };
        let session = guard.take();
        drop(guard);
        (self.events)(SessionEvent::Closed {
            info: open.info.clone(),
            duration_ms: elapsed_ms(open.opened),
        });
        if let Some(session) = session {
            session.close().await?;
        }
        Ok(())
    }

    /// The description of one open session.
    ///
    /// # Errors
    /// [`SessionError::NotFound`] when no session with that id is open.
    pub fn info(&self, session_id: &str) -> Result<SessionInfo, SessionError> {
        Ok(self.get(session_id)?.info.clone())
    }

    /// Open user sessions (metadata sessions of the object browser are
    /// left out).
    #[must_use]
    pub fn list(&self) -> Vec<SessionInfo> {
        self.list_all()
            .into_iter()
            .filter(|s| s.purpose == SessionPurpose::User)
            .collect()
    }

    /// Every open session, including metadata sessions.
    #[must_use]
    pub fn list_all(&self) -> Vec<SessionInfo> {
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

    #[test]
    fn pg_identity_uses_the_current_timeline() {
        assert_eq!(walfile_timeline("00000002000000000000000A"), Some(2));
        assert_eq!(walfile_timeline("0000001F00000003000000FF"), Some(31));
        assert_eq!(walfile_timeline("0000000200000000"), None);
        assert_eq!(walfile_timeline("XYZ00002000000000000000A"), None);
        // A primary: the WAL file name wins over a checkpoint that lags after a promotion.
        assert_eq!(
            pg_identity_text(
                Some("7"),
                false,
                Some("00000003000000000000000A"),
                None,
                Some("2")
            ),
            Some("pg:7:tl3:primary".into())
        );
        // A standby: the WAL receiver's timeline.
        assert_eq!(
            pg_identity_text(Some("7"), true, None, Some("3"), Some("2")),
            Some("pg:7:tl3:standby".into())
        );
        // Not permitted to read the current timeline: the checkpoint's.
        assert_eq!(
            pg_identity_text(Some("7"), true, None, None, Some("2")),
            Some("pg:7:tl2:standby".into())
        );
        assert_eq!(
            pg_identity_text(None, false, Some("00000001000000000000000A"), None, None),
            None
        );
        assert_eq!(pg_identity_text(Some("7"), false, None, None, None), None);
    }

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
            if sql.contains("sleep") {
                // Runs until the cancel handle is used.
                let before = self.cancels.load(Ordering::SeqCst);
                while self.cancels.load(Ordering::SeqCst) == before {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                return Err(DriverError::new(ErrorKind::Cancelled, "cancelled"));
            }
            if sql.contains("partly") {
                // Statement 0 completed inside the open transaction.
                return Err(DriverError::new(ErrorKind::Query, "boom").with_partial(
                    PartialOutput {
                        results: vec![StatementResult::Command {
                            tag: "INSERT 0 1".into(),
                            rows_affected: Some(1),
                        }],
                        notices: Vec::new(),
                        elapsed_ms: 1,
                        failed_statement: Some(1),
                        completed: vec![Durability::InTransaction],
                    },
                ));
            }
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

    /// A local port the fake driver times out on.
    const STALLED_PORT: u16 = 40001;

    #[async_trait::async_trait]
    impl Driver for FakeDriver {
        fn engines(&self) -> &'static [Engine] {
            &[Engine::Postgres]
        }
        async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
            if params.host == "unreachable" {
                return Err(DriverError::new(ErrorKind::Connect, "connection refused"));
            }
            if params.port == STALLED_PORT {
                // Like the Redis driver behind a tunnel whose server never
                // answers.
                return Err(DriverError::new(
                    ErrorKind::Timeout,
                    format!("{}:{} did not answer within 10 s", params.host, params.port),
                ));
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
            matches!(failed, Err(SessionError::Driver { ref error }) if error.code.as_deref() == Some("42601"))
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
    async fn partial_output_reaches_the_caller_and_follows_the_rollback()
    -> Result<(), Box<dyn std::error::Error>> {
        let (manager, profile, events, _) = setup(Environment::Development)?;
        let session = manager.open(&profile, params(), None).await?;
        let failed = manager
            .execute(&session.id, "insert partly", false, &ExecOptions::default())
            .await;
        let Err(SessionError::Driver { error }) = failed else {
            return Err(format!("{failed:?}").into());
        };
        let partial = error.partial.ok_or("no partial output")?;
        assert_eq!(partial.results.len(), 1);
        assert_eq!(partial.completed, [Durability::InTransaction]);

        // The editors roll back on failure: what was open is undone.
        let failed = manager
            .execute_with(
                &session.id,
                "insert partly",
                false,
                &ExecOptions::default(),
                &ExecSpec {
                    rollback_on_error: true,
                    ..ExecSpec::default()
                },
            )
            .await;
        let Err(SessionError::Driver { error }) = failed else {
            return Err(format!("{failed:?}").into());
        };
        let partial = error.partial.ok_or("no partial output")?;
        assert_eq!(partial.completed, [Durability::RolledBack]);

        // The activity event keeps the report but not the rows.
        let list = events.lock().map_err(|_| "poisoned")?;
        let reported = list
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Executed {
                    error: Some(error), ..
                } => error.partial.as_deref(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reported.len(), 2);
        assert!(reported.iter().all(|p| p.results.is_empty()
            && p.failed_statement == Some(1)
            && p.completed.len() == 1));
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
        let (server, _latency) = manager
            .test("unsaved", Engine::Postgres, &params(), None)
            .await?;
        assert_eq!(server.version, "17.2");
        Ok(())
    }

    fn tunnelled(profile: &ConnectionProfile) -> ConnectionProfile {
        let mut profile = profile.clone();
        profile.host = "redis".into();
        profile.port = 6379;
        profile.ssh.enabled = true;
        profile.ssh.host = "bastion.example".into();
        profile.ssh.port = 2222;
        profile
    }

    #[test]
    fn tunnel_errors_name_the_target_and_the_bastion() -> Result<(), Box<dyn std::error::Error>> {
        let (_, profile, _, _) = setup(Environment::Development)?;
        assert_eq!(TunnelRoute::of(&profile), None);
        let route = TunnelRoute::of(&tunnelled(&profile)).ok_or("no route")?;
        assert_eq!(route.target, "redis:6379");
        assert_eq!(route.bastion, "bastion.example:2222");
        let params = ConnectParams::new(Engine::Redis, "127.0.0.1", 50123);

        // Timeouts keep the driver's wording with the route in place of
        // the local end: "did not answer" only where the driver says the
        // server never answered, a setup stall stays a setup stall. A
        // timeout naming no address (PostgreSQL) gets the route added.
        let route_text = "redis:6379 through SSH bastion bastion.example:2222";
        for (driver_message, expected) in [
            (
                "127.0.0.1:50123 did not answer within 10 s",
                format!("{route_text} did not answer within 10 s"),
            ),
            (
                "Connecting to 127.0.0.1:50123 timed out after 10 s during the session setup \
                 (the server answered, then stopped answering)",
                format!(
                    "Connecting to {route_text} timed out after 10 s during the session setup \
                     (the server answered, then stopped answering)"
                ),
            ),
            (
                "connecting to 127.0.0.1:50123 timed out during the session setup",
                format!("connecting to {route_text} timed out during the session setup"),
            ),
            (
                "connecting timed out after 10000 ms",
                format!("connecting timed out after 10000 ms ({route_text})"),
            ),
        ] {
            let error = route.explain(
                DriverError::new(ErrorKind::Timeout, driver_message),
                &params,
            );
            assert_eq!(error.message, expected);
            assert_eq!(error.kind, ErrorKind::Timeout);
            assert!(
                error
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("127.0.0.1:50123") && d.contains(driver_message)),
                "{error:?}"
            );
        }

        // Another error that names the local end names the route instead.
        let lost = route.explain(
            DriverError::new(
                ErrorKind::Connect,
                "Could not connect to 127.0.0.1:50123: connection reset",
            ),
            &params,
        );
        assert_eq!(
            lost.message,
            "Could not connect to redis:6379 through SSH bastion bastion.example:2222: \
             connection reset"
        );
        // Errors that do not name it are left alone.
        let auth =
            DriverError::new(ErrorKind::Auth, "password authentication failed").with_code("28P01");
        let unchanged = route.explain(auth.clone(), &params);
        assert_eq!(unchanged.message, auth.message);
        assert_eq!(unchanged.detail, None);
        Ok(())
    }

    #[tokio::test]
    async fn tunnelled_open_and_test_timeouts_name_the_route()
    -> Result<(), Box<dyn std::error::Error>> {
        let (manager, profile, events, _) = setup(Environment::Development)?;
        let profile = tunnelled(&profile);
        let stalled = ConnectParams::new(Engine::Postgres, "127.0.0.1", STALLED_PORT);
        let expected =
            "redis:6379 through SSH bastion bastion.example:2222 did not answer within 10 s";

        let Err(SessionError::Driver { error }) =
            manager.open(&profile, stalled.clone(), None).await
        else {
            return Err("the open did not fail with a driver error".into());
        };
        assert_eq!(error.message, expected);
        // The activity event records the same message.
        assert!(events.lock().map_err(|_| "poisoned")?.iter().any(
            |e| matches!(e, SessionEvent::OpenFailed { error, .. } if error.message == expected)
        ));

        let route = TunnelRoute::of(&profile);
        let Err(SessionError::Driver { error }) = manager
            .test(&profile.id, Engine::Postgres, &stalled, route.as_ref())
            .await
        else {
            return Err("the test did not fail with a driver error".into());
        };
        assert_eq!(error.message, expected);

        // Without a tunnel the driver's message stays.
        let direct = ConnectionProfile {
            ssh: Default::default(),
            ..profile
        };
        let Err(SessionError::Driver { error }) =
            manager.open(&direct, stalled.clone(), None).await
        else {
            return Err("the direct open did not fail with a driver error".into());
        };
        assert_eq!(
            error.message,
            format!("127.0.0.1:{STALLED_PORT} did not answer within 10 s")
        );
        Ok(())
    }

    #[tokio::test]
    async fn closing_cancels_a_running_statement_first() -> Result<(), Box<dyn std::error::Error>> {
        let (manager, profile, events, cancels) = setup(Environment::Development)?;
        let manager = Arc::new(manager);
        let session = manager.open(&profile, params(), None).await?;
        let running = {
            let manager = Arc::clone(&manager);
            let id = session.id.clone();
            tokio::spawn(async move {
                manager
                    .execute(&id, "select sleep", false, &ExecOptions::default())
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            manager.close(&session.id),
        )
        .await??;
        assert!(cancels.load(Ordering::SeqCst) >= 1, "close did not cancel");
        let outcome = running.await?;
        assert!(
            matches!(outcome, Err(SessionError::Driver { ref error }) if error.kind == ErrorKind::Cancelled),
            "{outcome:?}"
        );
        assert!(manager.list().is_empty());
        let list = events.lock().map_err(|_| "poisoned")?;
        assert!(
            list.iter()
                .any(|e| matches!(e, SessionEvent::Closed { .. }))
        );
        Ok(())
    }

    #[test]
    fn errors_serialize_without_tag_collisions() -> Result<(), serde_json::Error> {
        let error = SessionError::Driver {
            error: DriverError::new(ErrorKind::Auth, "password authentication failed")
                .with_code("28P01"),
        };
        let json = serde_json::to_value(&error)?;
        assert_eq!(json["kind"], "driver");
        assert_eq!(json["error"]["kind"], "auth");
        assert_eq!(json["error"]["code"], "28P01");
        let text = serde_json::to_string(&error)?;
        assert_eq!(text.matches("\"kind\"").count(), 2, "{text}");

        let confirm = SessionError::ConfirmationRequired {
            environment: Environment::Production,
            statements: vec!["Drop".into()],
        };
        assert_eq!(
            serde_json::to_value(&confirm)?,
            serde_json::json!({ "kind": "confirmationRequired", "environment": "production", "statements": ["Drop"] })
        );
        assert_eq!(
            serde_json::to_value(SessionError::NoDriver {
                engine: Engine::Redis
            })?,
            serde_json::json!({ "kind": "noDriver", "engine": "redis" })
        );
        Ok(())
    }
}
