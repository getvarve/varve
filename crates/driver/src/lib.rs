//! The contract every Varve database driver implements (design doc 4.1).
//!
//! A [`Driver`] opens a [`Session`] from [`ConnectParams`]. A session runs
//! SQL (or a Redis command line), returning every result set the server
//! sends, and can be cancelled from another task through its
//! [`CancelHandle`]. Engine-specific features that do not fit the shared
//! contract are reached by downcasting with [`Session::as_any`].
//!
//! SSH tunnels are not a driver concern: the app opens the tunnel and passes
//! the local endpoint in [`ConnectParams::host`] and [`ConnectParams::port`],
//! keeping the real host name in [`TlsParams::server_name`] for certificate
//! checks.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use varve_core::Engine;
use varve_secrets::Secret;

mod classify;
mod partial;
pub mod tls_key;
mod value;

pub use classify::{
    AmbiguousSplit, Dialect, LexKind, Lexeme, MysqlMode, MysqlServer, StatementKind, StatementSpan,
    classify, is_destructive, lex, lex_mysql, mysql_defines_body, read_only_violation,
    split_statements, split_statements_mysql, split_statements_strict,
};
pub use partial::{
    Durability, PartialOutput, RanStatement, TransactionEffect, TransactionTracker,
    transaction_effect, transaction_effect_mysql,
};
pub use value::{JsonText, MAX_SAFE_INTEGER, Value};

/// How TLS is used for a connection (design doc 3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Never use TLS.
    Disable,
    /// Use TLS when the server offers it, without certificate checks.
    /// PostgreSQL and MySQL / MariaDB ask the server and use TLS when it
    /// agrees. Redis has no such request: `prefer` sends a plain `PING`
    /// first and uses plain TCP when the port answers it like Redis, TLS
    /// otherwise. A Redis port that accepts both (ElastiCache transit
    /// encryption "preferred", a proxy that detects TLS) therefore gets plain
    /// TCP, and the password goes unencrypted: use [`TlsMode::Require`]
    /// for such endpoints. [`Session::tls_in_use`] tells what was used.
    #[default]
    Prefer,
    /// Always use TLS, without certificate checks.
    Require,
    /// Always use TLS and check the certificate chain against the CA.
    VerifyCa,
    /// Like [`TlsMode::VerifyCa`] and also check the host name.
    VerifyFull,
}

/// TLS settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsParams {
    /// TLS mode.
    pub mode: TlsMode,
    /// PEM file with the CA certificate(s). The OS trust store is used when
    /// absent.
    pub ca_path: Option<String>,
    /// PEM file with the client certificate.
    pub client_cert_path: Option<String>,
    /// PEM file with the client private key. It may be encrypted (see
    /// [`tls_key`] for the formats); its passphrase is then
    /// [`Self::client_key_passphrase`].
    pub client_key_path: Option<String>,
    /// Passphrase of an encrypted client key, from the keychain or the
    /// prompt. Never printed, never serialized; ignored when the key file is
    /// not encrypted.
    #[serde(skip)]
    pub client_key_passphrase: Option<Secret>,
    /// Host name to check the certificate against. Set by the app when the
    /// connection runs through an SSH tunnel; defaults to the host.
    pub server_name: Option<String>,
}

/// Everything a driver needs to connect. Built by the app from a connection
/// profile, the user's overrides and the keychain.
#[derive(Debug, Clone)]
pub struct ConnectParams {
    /// Engine to connect to. MySQL and MariaDB are both served by the MySQL
    /// driver, which reports the real engine in [`ServerInfo::engine`].
    pub engine: Engine,
    /// Host or IP address (the local tunnel end when tunnelled).
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Database (PostgreSQL), default schema (MySQL, MariaDB). Unused for Redis.
    pub database: Option<String>,
    /// User name. Redis 6 ACL user; `None` uses the `default` user.
    pub username: Option<String>,
    /// Password from the keychain or the prompt. Never logged.
    pub password: Option<Secret>,
    /// TLS settings.
    pub tls: TlsParams,
    /// Redis logical database index.
    pub redis_db: u16,
    /// Give up connecting after this long.
    pub connect_timeout: Duration,
    /// Server-side statement timeout, when the engine supports one.
    pub statement_timeout: Option<Duration>,
    /// Open the session read-only (design doc 3.2).
    pub read_only: bool,
    /// Shown to the server as the client application name.
    pub application_name: String,
    /// SQL (or Redis commands, one per line) to run right after connecting.
    pub init_sql: Option<String>,
}

impl ConnectParams {
    /// Parameters with defaults for everything but the endpoint.
    #[must_use]
    pub fn new(engine: Engine, host: impl Into<String>, port: u16) -> Self {
        Self {
            engine,
            host: host.into(),
            port,
            database: None,
            username: None,
            password: None,
            tls: TlsParams::default(),
            redis_db: 0,
            connect_timeout: Duration::from_secs(10),
            statement_timeout: None,
            read_only: false,
            application_name: "Varve".into(),
            init_sql: None,
        }
    }
}

/// What the server told us about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// Detected engine (MariaDB is told apart from MySQL here).
    pub engine: Engine,
    /// Full version string as reported, for example `17.2` or `11.4.4-MariaDB`.
    pub version: String,
    /// Major version.
    pub major: u32,
    /// Minor version.
    pub minor: u32,
    /// Patch version, when reported.
    pub patch: Option<u32>,
}

impl ServerInfo {
    /// True when the server is at least `major.minor`.
    #[must_use]
    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        (self.major, self.minor) >= (major, minor)
    }
}

/// Parses the leading `major.minor[.patch]` out of a version string.
#[must_use]
pub fn parse_version(version: &str) -> Option<(u32, u32, Option<u32>)> {
    let numeric: String = version
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = numeric.split('.').filter(|p| !p.is_empty());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().and_then(|p| p.parse().ok());
    Some((major, minor, patch))
}

/// A column of a result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Column {
    /// Column name as returned by the server.
    pub name: String,
    /// Engine type name, for example `int8`, `varchar(255)` or `jsonb`.
    pub type_name: String,
}

/// One result of executing SQL. A script can return several.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StatementResult {
    /// A statement that returned rows.
    #[serde(rename_all = "camelCase")]
    Rows {
        /// Columns, in order.
        columns: Vec<Column>,
        /// Rows, each with one value per column.
        rows: Vec<Vec<Value>>,
        /// True when more rows existed than [`ExecOptions::max_rows`].
        truncated: bool,
    },
    /// A statement that returned no rows.
    #[serde(rename_all = "camelCase")]
    Command {
        /// Command tag or short description, for example `UPDATE 3` or `OK`.
        tag: String,
        /// Rows affected, when the engine reports it.
        rows_affected: Option<u64>,
    },
}

/// A server message that is not an error (PostgreSQL `NOTICE`, MySQL warnings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    /// Severity as reported, for example `NOTICE`, `WARNING`, `Note`.
    pub severity: String,
    /// Message text.
    pub message: String,
}

/// Options for one execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOptions {
    /// Keep at most this many rows per result set; the rest is read and
    /// dropped and the result is marked truncated.
    pub max_rows: usize,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self { max_rows: 10_000 }
    }
}

/// Everything one execution produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecOutput {
    /// Results in the order the server sent them.
    pub results: Vec<StatementResult>,
    /// Notices and warnings.
    pub notices: Vec<Notice>,
    /// Wall time on the client, in milliseconds.
    pub elapsed_ms: u64,
}

/// Category of a driver error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The server could not be reached.
    Connect,
    /// The server rejected the credentials.
    Auth,
    /// TLS negotiation or certificate validation failed.
    Tls,
    /// The operation took too long.
    Timeout,
    /// The user cancelled the operation.
    Cancelled,
    /// The server rejected a statement.
    Query,
    /// The session is read-only and the statement would write.
    ReadOnly,
    /// The feature is not available on this server or driver.
    Unsupported,
    /// The server sent something the driver did not expect.
    Protocol,
    /// The connection was lost.
    ConnectionLost,
}

/// An error from a driver, shaped for the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
#[serde(rename_all = "camelCase")]
pub struct DriverError {
    /// Category.
    pub kind: ErrorKind,
    /// Message for the user. Never contains a password.
    pub message: String,
    /// Engine error code: SQLSTATE, MySQL error number, Redis error prefix.
    pub code: Option<String>,
    /// Extra detail from the server.
    pub detail: Option<String>,
    /// Hint from the server.
    pub hint: Option<String>,
    /// 1-based character position in the SQL text, when reported.
    pub position: Option<u32>,
    /// What a script produced before it failed: the results and notices of
    /// the statements that completed, which statement failed, and what
    /// became of the completed statements' changes. Set by
    /// [`Session::execute`] whenever a statement completed (or a result or
    /// notice arrived) before the failure, also for cancels and timeouts;
    /// `None` otherwise and for errors outside a script run.
    pub partial: Option<Box<PartialOutput>>,
}

impl DriverError {
    /// Creates an error with only a kind and a message.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            code: None,
            detail: None,
            hint: None,
            position: None,
            partial: None,
        }
    }

    /// Sets the engine error code.
    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// Attaches what the script produced before it failed, when there is
    /// anything ([`PartialOutput::has_content`]); otherwise clears it.
    #[must_use]
    pub fn with_partial(mut self, partial: PartialOutput) -> Self {
        self.partial = partial.has_content().then(|| Box::new(partial));
        self
    }
}

/// Cancels the statement running on a session, from any task.
#[async_trait::async_trait]
pub trait CancelHandle: Send + Sync {
    /// Asks the server to cancel the running statement. Succeeds when nothing
    /// is running.
    async fn cancel(&self) -> Result<(), DriverError>;
}

/// An open connection, used by one editor tab or tool at a time.
#[async_trait::async_trait]
pub trait Session: Send + Sync {
    /// Server identity, read while connecting.
    fn server_info(&self) -> &ServerInfo;

    /// A handle that can cancel the running statement from another task.
    fn cancel_handle(&self) -> Arc<dyn CancelHandle>;

    /// Runs SQL (for Redis: commands, one per line) and returns every
    /// result. Calls on one session are serialized.
    ///
    /// When a statement fails, the error's [`DriverError::partial`] holds
    /// what the statements before it produced and which statement failed.
    async fn execute(&self, sql: &str, options: &ExecOptions) -> Result<ExecOutput, DriverError>;

    /// The statements the last [`Session::execute`] ran (or, when it
    /// failed, the ones that completed), when the driver knows them
    /// exactly: every statement was sent as a request of its own, so the
    /// server ran exactly that text, and the transaction status after each
    /// was reported. `None` when it does not know them exactly (the server
    /// split a multi-statement request itself, the session state is not
    /// tracked) or the engine does not report them. Activity logging of
    /// privilege changes relies on this; nothing is logged without it.
    fn last_statements(&self) -> Option<Vec<RanStatement>> {
        None
    }

    /// Whether the server says a transaction is open on this session, as of
    /// the last reply (PostgreSQL ReadyForQuery status, MySQL/MariaDB OK
    /// packet `SERVER_STATUS_IN_TRANS`), without a round trip and without
    /// any privilege. `None` when the engine has no such notion (Redis) or
    /// nothing was received yet.
    fn in_transaction(&self) -> Option<bool> {
        None
    }

    /// The server notices and warnings sent while connecting (the profile's
    /// init SQL), which no call returns. Empty when the engine does not
    /// keep them.
    fn connect_notices(&self) -> Vec<Notice> {
        Vec::new()
    }

    /// Whether the session talks to the server over TLS, as connected
    /// (with [`TlsMode::Prefer`] it may be either). `None` when the driver
    /// cannot tell.
    fn tls_in_use(&self) -> Option<bool> {
        None
    }

    /// Whether the password went over the network as is, without TLS:
    /// Redis `AUTH` on plain TCP, a PostgreSQL server that asked for it in
    /// clear text (pg_hba `password`, `ldap`, `radius`, `pam`) on a
    /// connection without TLS. False when the driver knows of no such case.
    fn password_in_clear(&self) -> bool {
        false
    }

    /// Checks that the connection is alive.
    async fn ping(&self) -> Result<(), DriverError>;

    /// Closes the connection.
    async fn close(self: Box<Self>) -> Result<(), DriverError>;

    /// For engine-specific features: downcast to the driver's session type.
    fn as_any(&self) -> &dyn Any;
}

/// Opens sessions for one engine family.
#[async_trait::async_trait]
pub trait Driver: Send + Sync {
    /// Engines this driver serves.
    fn engines(&self) -> &'static [Engine];

    /// Opens a session.
    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Session>, DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("17.2"), Some((17, 2, None)));
        assert_eq!(parse_version("8.4.3"), Some((8, 4, Some(3))));
        assert_eq!(
            parse_version("11.4.4-MariaDB-ubu2404"),
            Some((11, 4, Some(4)))
        );
        assert_eq!(parse_version("18"), Some((18, 0, None)));
        assert_eq!(parse_version("PostgreSQL"), None);
    }

    #[test]
    fn at_least_compares_major_then_minor() {
        let info = ServerInfo {
            engine: Engine::Postgres,
            version: "16.4".into(),
            major: 16,
            minor: 4,
            patch: None,
        };
        assert!(info.at_least(16, 0));
        assert!(info.at_least(16, 4));
        assert!(!info.at_least(16, 5));
        assert!(!info.at_least(17, 0));
    }

    #[test]
    fn results_serialize_for_the_ui() -> Result<(), serde_json::Error> {
        let result = StatementResult::Command {
            tag: "UPDATE 3".into(),
            rows_affected: Some(3),
        };
        assert_eq!(
            serde_json::to_value(&result)?,
            serde_json::json!({ "kind": "command", "tag": "UPDATE 3", "rowsAffected": 3 })
        );
        Ok(())
    }

    #[test]
    fn errors_serialize_their_partial_output() -> Result<(), serde_json::Error> {
        let plain = DriverError::new(ErrorKind::Query, "boom");
        assert_eq!(
            serde_json::to_value(&plain)?["partial"],
            serde_json::Value::Null
        );
        let empty = PartialOutput {
            results: Vec::new(),
            notices: Vec::new(),
            elapsed_ms: 0,
            failed_statement: Some(0),
            completed: Vec::new(),
        };
        assert!(plain.clone().with_partial(empty).partial.is_none());
        let partial = PartialOutput {
            results: Vec::new(),
            notices: Vec::new(),
            elapsed_ms: 1,
            failed_statement: Some(1),
            completed: vec![Durability::Committed],
        };
        let error = plain.with_partial(partial);
        let json = serde_json::to_value(&error)?;
        assert_eq!(json["partial"]["failedStatement"], 1);
        assert_eq!(json["partial"]["completed"][0], "committed");
        let back: DriverError = serde_json::from_value(json)?;
        assert_eq!(back, error);
        // Older payloads without the field still read.
        let old: DriverError = serde_json::from_value(serde_json::json!({
            "kind": "query", "message": "m", "code": null, "detail": null, "hint": null,
            "position": null
        }))?;
        assert!(old.partial.is_none());
        Ok(())
    }

    #[test]
    fn error_display_is_the_message() {
        let error =
            DriverError::new(ErrorKind::Auth, "password authentication failed").with_code("28P01");
        assert_eq!(error.to_string(), "password authentication failed");
        assert_eq!(error.code.as_deref(), Some("28P01"));
    }
}
