//! The activity event model (design doc 9.1 and 9.2).

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;
use varve_core::{DottedId, Engine};

use crate::error::ActivityError;
use crate::redact::SqlDialect;

/// Actor id used while the user is signed out.
pub const LOCAL_ACTOR: &str = "local";

/// Current time as unix milliseconds.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Category of an activity event (design doc 9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Connecting, disconnecting and tunnels.
    Connection,
    /// Query execution.
    Query,
    /// Manual transactions.
    Transaction,
    /// Grid edits, imports, exports, dumps and SQL file runs.
    Data,
    /// Schema changes through object editors.
    Schema,
    /// Transfer, structure sync and data sync jobs.
    Tool,
    /// Users, roles, privileges and Redis ACLs.
    Security,
    /// Destructive statement confirmations and policies.
    Safety,
    /// Keychain secrets.
    Secrets,
    /// Statistics snapshots.
    Stats,
    /// The app itself, including the activity log.
    App,
    /// Cloud account and sign-in.
    Account,
    /// Cloud sync.
    Sync,
    /// Workspace management.
    Workspace,
    /// Workspace members and invites.
    Membership,
    /// Folder restrictions and access.
    Access,
    /// Synced records.
    Record,
}

impl Category {
    /// Every category, in the order of design doc 9.2.
    pub const ALL: [Category; 17] = [
        Category::Connection,
        Category::Query,
        Category::Transaction,
        Category::Data,
        Category::Schema,
        Category::Tool,
        Category::Security,
        Category::Safety,
        Category::Secrets,
        Category::Stats,
        Category::App,
        Category::Account,
        Category::Sync,
        Category::Workspace,
        Category::Membership,
        Category::Access,
        Category::Record,
    ];

    /// Snake case name, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Connection => "connection",
            Category::Query => "query",
            Category::Transaction => "transaction",
            Category::Data => "data",
            Category::Schema => "schema",
            Category::Tool => "tool",
            Category::Security => "security",
            Category::Safety => "safety",
            Category::Secrets => "secrets",
            Category::Stats => "stats",
            Category::App => "app",
            Category::Account => "account",
            Category::Sync => "sync",
            Category::Workspace => "workspace",
            Category::Membership => "membership",
            Category::Access => "access",
            Category::Record => "record",
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Category {
    type Err = ActivityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Category::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| ActivityError::InvalidEvent(format!("unknown category `{s}`")))
    }
}

/// How an action ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// It worked.
    #[default]
    Success,
    /// It failed.
    Failure,
    /// The user or a policy stopped it.
    Cancelled,
}

impl Outcome {
    /// Every outcome.
    pub const ALL: [Outcome; 3] = [Outcome::Success, Outcome::Failure, Outcome::Cancelled];

    /// Snake case name, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
            Outcome::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Outcome {
    type Err = ActivityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Outcome::ALL
            .into_iter()
            .find(|o| o.as_str() == s)
            .ok_or_else(|| ActivityError::InvalidEvent(format!("unknown outcome `{s}`")))
    }
}

/// Statement text waiting for the recorder, which stores it only through
/// [`crate::prepare_statement_text`] at the configured level.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PendingStatement {
    pub(crate) text: String,
    pub(crate) dialect: SqlDialect,
}

impl fmt::Debug for PendingStatement {
    // Never print raw statement text: it may hold secrets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingStatement")
            .field("dialect", &self.dialect)
            .field("len", &self.text.len())
            .finish()
    }
}

/// An error waiting for the recorder, which stores it in the `error` detail
/// only through [`crate::prepare_error_message`] at the configured level.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PendingError {
    pub(crate) error: ErrorDetails,
    /// The engine whose server produced the message; `None` when it cannot
    /// echo statement text (SSH, client-side errors).
    pub(crate) dialect: Option<SqlDialect>,
}

impl fmt::Debug for PendingError {
    // Never print the raw message: it may echo secrets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingError")
            .field("code", &self.error.code)
            .field("dialect", &self.dialect)
            .field("len", &self.error.message.len())
            .finish()
    }
}

/// One activity event (design doc 9.1).
///
/// Build events with [`ActivityEvent::builder`] or one of the action
/// constructors such as [`ActivityEvent::query_executed`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEvent {
    /// Time-ordered UUID v7, generated where the event happens.
    pub id: Uuid,
    /// When it happened, unix milliseconds (UTC).
    pub occurred_at: i64,
    /// User id, or [`LOCAL_ACTOR`] when signed out. Filled in by the
    /// recorder when empty.
    pub actor_id: String,
    /// Device id. Filled in by the recorder when unset.
    pub device_id: Option<String>,
    /// Workspace of the record the event concerns.
    pub workspace_id: Option<String>,
    /// Connection profile id, for database events.
    pub connection_id: Option<String>,
    /// Server identity, for database events.
    pub server_key: Option<String>,
    /// Category.
    pub category: Category,
    /// Dotted action such as `query.executed`.
    pub action: String,
    /// How it ended.
    pub outcome: Outcome,
    /// How long it took.
    pub duration_ms: Option<u64>,
    /// Object, record, member or job reference.
    pub target: Option<String>,
    /// Action-specific JSON object, never secrets.
    pub details: Value,
    #[serde(skip)]
    pub(crate) statement: Option<PendingStatement>,
    #[serde(skip)]
    pub(crate) error: Option<PendingError>,
}

impl ActivityEvent {
    /// Starts an event with the given category and action.
    pub fn builder(category: Category, action: impl Into<String>) -> ActivityEventBuilder {
        ActivityEventBuilder {
            category,
            action: action.into(),
            outcome: Outcome::Success,
            occurred_at: None,
            actor_id: None,
            device_id: None,
            workspace_id: None,
            connection_id: None,
            server_key: None,
            duration_ms: None,
            target: None,
            details: Map::new(),
            statement: None,
            error: None,
        }
    }
}

/// Builder for [`ActivityEvent`].
#[derive(Debug, Clone)]
#[must_use]
pub struct ActivityEventBuilder {
    category: Category,
    action: String,
    outcome: Outcome,
    occurred_at: Option<i64>,
    actor_id: Option<String>,
    device_id: Option<String>,
    workspace_id: Option<String>,
    connection_id: Option<String>,
    server_key: Option<String>,
    duration_ms: Option<u64>,
    target: Option<String>,
    details: Map<String, Value>,
    statement: Option<PendingStatement>,
    error: Option<PendingError>,
}

impl ActivityEventBuilder {
    /// Sets the outcome (default [`Outcome::Success`]).
    pub fn outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// Sets when it happened (default: when [`Self::build`] runs).
    pub fn occurred_at(mut self, unix_ms: i64) -> Self {
        self.occurred_at = Some(unix_ms);
        self
    }

    /// Sets the actor (default: the recorder's actor).
    pub fn actor(mut self, actor_id: impl Into<String>) -> Self {
        self.actor_id = Some(actor_id.into());
        self
    }

    /// Sets the device (default: the recorder's device).
    pub fn device(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = Some(device_id.into());
        self
    }

    /// Sets the workspace.
    pub fn workspace(mut self, workspace_id: impl Into<String>) -> Self {
        self.workspace_id = Some(workspace_id.into());
        self
    }

    /// Sets the connection profile.
    pub fn connection(mut self, connection_id: impl Into<String>) -> Self {
        self.connection_id = Some(connection_id.into());
        self
    }

    /// Sets the server identity.
    pub fn server_key(mut self, server_key: impl Into<String>) -> Self {
        self.server_key = Some(server_key.into());
        self
    }

    /// Sets the duration in milliseconds.
    pub fn duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    /// Sets the duration.
    pub fn duration(self, duration: Duration) -> Self {
        let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        self.duration_ms(ms)
    }

    /// Sets the target reference.
    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// Adds one detail. `statement`, `statement_truncated`, `text_level`
    /// and `error` are reserved: the recorder replaces them. Attach errors
    /// with [`Self::error`].
    pub fn detail(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }

    /// Adds an optional detail; `None` leaves it out.
    pub fn detail_opt<V: Into<Value>>(self, key: impl Into<String>, value: Option<V>) -> Self {
        match value {
            Some(value) => self.detail(key, value),
            None => self,
        }
    }

    /// Attaches statement text. The recorder stores it in the `statement`
    /// detail only after the secret redactor, at its text level.
    pub fn statement(mut self, text: impl Into<String>, dialect: SqlDialect) -> Self {
        self.statement = Some(PendingStatement {
            text: text.into(),
            dialect,
        });
        self
    }

    /// Attaches an error. The recorder stores it in the `error` detail
    /// (`{"code", "message"}`) after [`crate::prepare_error_message`] at its
    /// text level: server messages can echo statement text, so they follow
    /// the same rules as statements.
    ///
    /// `dialect` is the engine whose server produced the message, or `None`
    /// for messages that cannot echo statement text (SSH tunnel errors),
    /// which only have their credentials redacted. The action constructors
    /// (`query_failed`, `connection_failed`, ...) call this for you.
    pub fn error(mut self, error: &ErrorDetails, dialect: Option<SqlDialect>) -> Self {
        self.error = Some(PendingError {
            error: error.clone(),
            dialect,
        });
        self
    }

    /// Validates and builds the event, with a new UUID v7.
    ///
    /// # Errors
    /// [`ActivityError::InvalidAction`] when the action is not a dotted
    /// lowercase identifier.
    pub fn build(self) -> Result<ActivityEvent, ActivityError> {
        validate_action(&self.action)?;
        Ok(ActivityEvent {
            id: Uuid::now_v7(),
            occurred_at: self.occurred_at.unwrap_or_else(now_ms),
            actor_id: self.actor_id.unwrap_or_default(),
            device_id: self.device_id,
            workspace_id: self.workspace_id,
            connection_id: self.connection_id,
            server_key: self.server_key,
            category: self.category,
            action: self.action,
            outcome: self.outcome,
            duration_ms: self.duration_ms,
            target: self.target,
            details: Value::Object(self.details),
            statement: self.statement,
            error: self.error,
        })
    }
}

/// Checks that `action` is a dotted lowercase identifier with at least two
/// segments (`query.executed`).
pub(crate) fn validate_action(action: &str) -> Result<(), ActivityError> {
    match DottedId::new(action) {
        Ok(_) if action.contains('.') => Ok(()),
        _ => Err(ActivityError::InvalidAction(action.to_string())),
    }
}

/// A server or client error, as stored in `details.error`. Attach it with
/// [`ActivityEventBuilder::error`] or an action constructor.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ErrorDetails {
    /// Engine error code (SQLSTATE, MySQL error number, Redis error prefix).
    pub code: Option<String>,
    /// Error message. The recorder stores it according to its text level
    /// and redacts secrets in it (see [`crate::prepare_error_message`]).
    pub message: String,
}

/// What a connection event records about the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionDetails {
    /// Engine.
    pub engine: Engine,
    /// Server version, when known.
    pub server_version: Option<String>,
    /// Host name as configured.
    pub host: String,
    /// Port.
    pub port: Option<u16>,
    /// Database, when one is selected.
    pub database: Option<String>,
    /// Effective user name.
    pub username: Option<String>,
    /// Whether the connection runs through an SSH tunnel.
    pub via_ssh: bool,
    /// TLS mode, for example `verify-full` or `disabled`.
    pub tls_mode: Option<String>,
}

fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "postgres",
        Engine::Mysql => "mysql",
        Engine::Mariadb => "mariadb",
        Engine::Redis => "redis",
    }
}

impl ConnectionDetails {
    fn apply(&self, builder: ActivityEventBuilder) -> ActivityEventBuilder {
        builder
            .detail("engine", engine_name(self.engine))
            .detail_opt("server_version", self.server_version.clone())
            .detail("host", self.host.clone())
            .detail_opt("port", self.port)
            .detail_opt("database", self.database.clone())
            .detail_opt("username", self.username.clone())
            .detail("via_ssh", self.via_ssh)
            .detail_opt("tls_mode", self.tls_mode.clone())
    }
}

/// What a tunnel event records about the SSH tunnel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelDetails {
    /// SSH host.
    pub ssh_host: String,
    /// SSH port.
    pub ssh_port: u16,
    /// SSH user name.
    pub ssh_username: Option<String>,
    /// Authentication method, for example `password`, `key` or `agent`.
    pub auth_method: Option<String>,
}

impl TunnelDetails {
    fn apply(&self, builder: ActivityEventBuilder) -> ActivityEventBuilder {
        builder
            .detail("ssh_host", self.ssh_host.clone())
            .detail("ssh_port", self.ssh_port)
            .detail_opt("ssh_username", self.ssh_username.clone())
            .detail_opt("auth_method", self.auth_method.clone())
    }
}

/// Row counts of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RowCounts {
    /// Rows returned by a result set.
    pub returned: Option<u64>,
    /// Rows inserted, updated or deleted.
    pub affected: Option<u64>,
}

/// Rows changed by a grid commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RowChanges {
    /// Rows inserted.
    pub inserted: u64,
    /// Rows updated.
    pub updated: u64,
    /// Rows deleted.
    pub deleted: u64,
}

/// What an export records. Never the file contents.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExportDetails {
    /// Tables, views or queries included.
    pub objects: Vec<String>,
    /// File name (not the full path).
    pub file_name: String,
    /// Format, for example `csv`, `json`, `xlsx` or `parquet`.
    pub format: String,
    /// File size in bytes.
    pub size_bytes: Option<u64>,
    /// Rows written.
    pub rows: Option<u64>,
}

/// Constructors for the M1 actions of design doc 9.2. Each returns a
/// builder, so callers add the connection, server key and duration.
impl ActivityEvent {
    /// `connection.opened`.
    pub fn connection_opened(connection: &ConnectionDetails) -> ActivityEventBuilder {
        connection.apply(Self::builder(Category::Connection, "connection.opened"))
    }

    /// `connection.closed`, with the session duration.
    pub fn connection_closed(
        connection: &ConnectionDetails,
        session_ms: u64,
    ) -> ActivityEventBuilder {
        connection
            .apply(Self::builder(Category::Connection, "connection.closed"))
            .detail("session_duration_ms", session_ms)
            .duration_ms(session_ms)
    }

    /// `connection.failed`. The error message is treated as text of the
    /// connection's engine (an `init_sql` error can echo it).
    pub fn connection_failed(
        connection: &ConnectionDetails,
        error: &ErrorDetails,
    ) -> ActivityEventBuilder {
        connection
            .apply(Self::builder(Category::Connection, "connection.failed"))
            .outcome(Outcome::Failure)
            .error(error, Some(SqlDialect::for_engine(connection.engine)))
    }

    /// `connection.lost`, with the error and the session duration.
    pub fn connection_lost(
        connection: &ConnectionDetails,
        error: &ErrorDetails,
        session_ms: u64,
    ) -> ActivityEventBuilder {
        connection
            .apply(Self::builder(Category::Connection, "connection.lost"))
            .outcome(Outcome::Failure)
            .error(error, Some(SqlDialect::for_engine(connection.engine)))
            .detail("session_duration_ms", session_ms)
            .duration_ms(session_ms)
    }

    /// `tls.downgraded`: a connection whose TLS mode is `prefer` opened
    /// without TLS (the outcome is success: the connection works). `password_unencrypted` says whether the password went
    /// to the server as is (Redis `AUTH` on plain TCP, a PostgreSQL server
    /// asking for a clear-text password).
    pub fn tls_downgraded(
        connection: &ConnectionDetails,
        password_unencrypted: bool,
    ) -> ActivityEventBuilder {
        connection
            .apply(Self::builder(Category::Connection, "tls.downgraded"))
            .detail("tls", false)
            .detail("password_unencrypted", password_unencrypted)
    }

    /// `tunnel.opened`.
    pub fn tunnel_opened(tunnel: &TunnelDetails) -> ActivityEventBuilder {
        tunnel.apply(Self::builder(Category::Connection, "tunnel.opened"))
    }

    /// `tunnel.failed`. SSH messages do not echo statement text: only
    /// credentials are redacted in them.
    pub fn tunnel_failed(tunnel: &TunnelDetails, error: &ErrorDetails) -> ActivityEventBuilder {
        tunnel
            .apply(Self::builder(Category::Connection, "tunnel.failed"))
            .outcome(Outcome::Failure)
            .error(error, None)
    }

    /// `query.executed`. `kind` is the statement kind (`select`, `ddl`, ...)
    /// or the Redis command name. The statement text is stored according
    /// to the recorder's text level.
    pub fn query_executed(
        statement: &str,
        dialect: SqlDialect,
        kind: impl Into<String>,
        rows: RowCounts,
        duration_ms: u64,
    ) -> ActivityEventBuilder {
        Self::builder(Category::Query, "query.executed")
            .detail("kind", kind.into())
            .detail_opt("rows_returned", rows.returned)
            .detail_opt("rows_affected", rows.affected)
            .duration_ms(duration_ms)
            .statement(statement, dialect)
    }

    /// `query.failed`. The error message follows the text level, like the
    /// statement (see [`crate::prepare_error_message`]).
    pub fn query_failed(
        statement: &str,
        dialect: SqlDialect,
        kind: impl Into<String>,
        error: &ErrorDetails,
        duration_ms: u64,
    ) -> ActivityEventBuilder {
        Self::builder(Category::Query, "query.failed")
            .outcome(Outcome::Failure)
            .detail("kind", kind.into())
            .error(error, Some(dialect))
            .duration_ms(duration_ms)
            .statement(statement, dialect)
    }

    /// `query.cancelled`.
    pub fn query_cancelled(
        statement: &str,
        dialect: SqlDialect,
        kind: impl Into<String>,
        duration_ms: u64,
    ) -> ActivityEventBuilder {
        Self::builder(Category::Query, "query.cancelled")
            .outcome(Outcome::Cancelled)
            .detail("kind", kind.into())
            .duration_ms(duration_ms)
            .statement(statement, dialect)
    }

    /// `transaction.committed` (manual transaction mode).
    pub fn transaction_committed(statement_count: u64, duration_ms: u64) -> ActivityEventBuilder {
        Self::builder(Category::Transaction, "transaction.committed")
            .detail("statement_count", statement_count)
            .duration_ms(duration_ms)
    }

    /// `transaction.rolled_back` (manual transaction mode).
    pub fn transaction_rolled_back(statement_count: u64, duration_ms: u64) -> ActivityEventBuilder {
        Self::builder(Category::Transaction, "transaction.rolled_back")
            .detail("statement_count", statement_count)
            .duration_ms(duration_ms)
    }

    /// `data.edited`: a grid commit on `table`.
    pub fn data_edited(table: &str, changes: RowChanges) -> ActivityEventBuilder {
        Self::builder(Category::Data, "data.edited")
            .target(table)
            .detail("table", table)
            .detail("rows_inserted", changes.inserted)
            .detail("rows_updated", changes.updated)
            .detail("rows_deleted", changes.deleted)
    }

    /// `data.exported`.
    pub fn data_exported(export: &ExportDetails) -> ActivityEventBuilder {
        Self::builder(Category::Data, "data.exported")
            .target(export.file_name.clone())
            .detail("objects", export.objects.clone())
            .detail("file_name", export.file_name.clone())
            .detail("format", export.format.clone())
            .detail_opt("size_bytes", export.size_bytes)
            .detail_opt("rows", export.rows)
    }

    /// `destructive.confirmed`: the user confirmed a destructive statement.
    pub fn destructive_confirmed(kind: impl Into<String>, policy: &str) -> ActivityEventBuilder {
        Self::builder(Category::Safety, "destructive.confirmed")
            .detail("kind", kind.into())
            .detail("policy", policy)
    }

    /// `destructive.blocked`: a destructive statement did not run.
    pub fn destructive_blocked(kind: impl Into<String>, policy: &str) -> ActivityEventBuilder {
        Self::builder(Category::Safety, "destructive.blocked")
            .outcome(Outcome::Cancelled)
            .detail("kind", kind.into())
            .detail("policy", policy)
    }

    /// `secret.saved`: a secret of `secret_kind` (`password`,
    /// `ssh_passphrase`, ...) was stored in the keychain. Never the value.
    pub fn secret_saved(secret_kind: &str) -> ActivityEventBuilder {
        Self::builder(Category::Secrets, "secret.saved").detail("secret_kind", secret_kind)
    }

    /// `secret.removed`.
    pub fn secret_removed(secret_kind: &str) -> ActivityEventBuilder {
        Self::builder(Category::Secrets, "secret.removed").detail("secret_kind", secret_kind)
    }

    /// `app.started`.
    pub fn app_started(version: &str) -> ActivityEventBuilder {
        Self::builder(Category::App, "app.started").detail("version", version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> ConnectionDetails {
        ConnectionDetails {
            engine: Engine::Postgres,
            server_version: Some("18.0".into()),
            host: "db.local".into(),
            port: Some(5432),
            database: Some("app".into()),
            username: Some("alice".into()),
            via_ssh: true,
            tls_mode: Some("verify-full".into()),
        }
    }

    #[test]
    fn categories_serialize_snake_case_and_round_trip() -> Result<(), ActivityError> {
        for category in Category::ALL {
            let json = serde_json::to_string(&category)?;
            assert_eq!(json, format!("\"{}\"", category.as_str()));
            assert_eq!(category.as_str().parse::<Category>()?, category);
        }
        assert!("nope".parse::<Category>().is_err());
        for outcome in Outcome::ALL {
            assert_eq!(outcome.as_str().parse::<Outcome>()?, outcome);
        }
        Ok(())
    }

    #[test]
    fn actions_are_validated() {
        for bad in [
            "",
            "query",
            "Query.Executed",
            "query..executed",
            "query.exe cuted",
        ] {
            assert!(
                ActivityEvent::builder(Category::Query, bad)
                    .build()
                    .is_err(),
                "{bad}"
            );
        }
        assert!(
            ActivityEvent::builder(Category::Tool, "acme.hello.greeted")
                .build()
                .is_ok()
        );
    }

    #[test]
    fn ids_are_time_ordered_v7() -> Result<(), ActivityError> {
        let a = ActivityEvent::app_started("1.0").build()?;
        let b = ActivityEvent::app_started("1.0").build()?;
        assert_eq!(a.id.get_version_num(), 7);
        assert!(b.id > a.id);
        Ok(())
    }

    #[test]
    fn constructors_fill_details() -> Result<(), ActivityError> {
        let e = ActivityEvent::connection_lost(
            &conn(),
            &ErrorDetails {
                code: Some("57P01".into()),
                message: "terminating connection".into(),
            },
            1500,
        )
        .connection("c1")
        .build()?;
        assert_eq!(e.action, "connection.lost");
        assert_eq!(e.category, Category::Connection);
        assert_eq!(e.outcome, Outcome::Failure);
        assert_eq!(e.duration_ms, Some(1500));
        assert_eq!(e.details["engine"], "postgres");
        assert_eq!(e.details["via_ssh"], true);
        assert!(e.details.get("error").is_none());
        assert_eq!(
            e.error
                .as_ref()
                .map(|p| (p.error.code.as_deref(), p.dialect)),
            Some((Some("57P01"), Some(SqlDialect::Postgres)))
        );
        assert_eq!(e.details["session_duration_ms"], 1500);

        let q = ActivityEvent::query_executed(
            "SELECT 1",
            SqlDialect::Postgres,
            "select",
            RowCounts {
                returned: Some(1),
                affected: None,
            },
            3,
        )
        .build()?;
        assert_eq!(q.details["kind"], "select");
        assert_eq!(q.details["rows_returned"], 1);
        assert!(q.details.get("rows_affected").is_none());
        assert!(q.details.get("statement").is_none());
        assert!(q.statement.is_some());

        let cases = [
            (
                ActivityEvent::connection_opened(&conn()),
                "connection.opened",
                Outcome::Success,
            ),
            (
                ActivityEvent::connection_closed(&conn(), 9),
                "connection.closed",
                Outcome::Success,
            ),
            (
                ActivityEvent::connection_failed(&conn(), &ErrorDetails::default()),
                "connection.failed",
                Outcome::Failure,
            ),
            (
                ActivityEvent::tls_downgraded(&conn(), true),
                "tls.downgraded",
                Outcome::Success,
            ),
            (
                ActivityEvent::tunnel_opened(&TunnelDetails {
                    ssh_host: "bastion".into(),
                    ssh_port: 22,
                    ssh_username: None,
                    auth_method: Some("agent".into()),
                }),
                "tunnel.opened",
                Outcome::Success,
            ),
            (
                ActivityEvent::query_failed(
                    "x",
                    SqlDialect::Mysql,
                    "other",
                    &ErrorDetails::default(),
                    1,
                ),
                "query.failed",
                Outcome::Failure,
            ),
            (
                ActivityEvent::query_cancelled("x", SqlDialect::Redis, "GET", 1),
                "query.cancelled",
                Outcome::Cancelled,
            ),
            (
                ActivityEvent::transaction_committed(3, 10),
                "transaction.committed",
                Outcome::Success,
            ),
            (
                ActivityEvent::transaction_rolled_back(3, 10),
                "transaction.rolled_back",
                Outcome::Success,
            ),
            (
                ActivityEvent::data_edited("public.users", RowChanges::default()),
                "data.edited",
                Outcome::Success,
            ),
            (
                ActivityEvent::data_exported(&ExportDetails::default()),
                "data.exported",
                Outcome::Success,
            ),
            (
                ActivityEvent::destructive_confirmed("drop", "production"),
                "destructive.confirmed",
                Outcome::Success,
            ),
            (
                ActivityEvent::destructive_blocked("truncate", "read_only"),
                "destructive.blocked",
                Outcome::Cancelled,
            ),
            (
                ActivityEvent::secret_saved("password"),
                "secret.saved",
                Outcome::Success,
            ),
            (
                ActivityEvent::secret_removed("password"),
                "secret.removed",
                Outcome::Success,
            ),
        ];
        for (builder, action, outcome) in cases {
            let event = builder.build()?;
            assert_eq!(event.action, action);
            assert_eq!(event.outcome, outcome);
            assert!(event.details.is_object());
        }
        Ok(())
    }

    #[test]
    fn debug_never_prints_statement_text() -> Result<(), ActivityError> {
        let e = ActivityEvent::query_executed(
            "ALTER ROLE a PASSWORD 'Tr0ub4dor'",
            SqlDialect::Postgres,
            "ddl",
            RowCounts::default(),
            1,
        )
        .build()?;
        assert!(!format!("{e:?}").contains("Tr0ub4dor"));
        assert!(!serde_json::to_string(&e)?.contains("Tr0ub4dor"));

        let e = ActivityEvent::query_failed(
            "x",
            SqlDialect::Postgres,
            "ddl",
            &ErrorDetails {
                code: Some("42601".into()),
                message: "syntax error at or near \"Tr0ub4dor\"".into(),
            },
            1,
        )
        .build()?;
        assert!(!format!("{e:?}").contains("Tr0ub4dor"));
        assert!(!serde_json::to_string(&e)?.contains("Tr0ub4dor"));
        Ok(())
    }
}
