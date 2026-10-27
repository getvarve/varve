//! PostgreSQL, MySQL and MariaDB live server panels (design doc 4.2 and the
//! live server status part of section 5): current values only, never
//! history.
//!
//! - [`process`]: the process list, with cancel query and terminate
//!   connection.
//! - [`variables`]: server variables, browsed and edited through
//!   [`EditPlan`]s the UI shows before they run.
//! - [`status`]: the server status summary (version, uptime, connections,
//!   cache hit ratio, database sizes, replication) and the MariaDB Galera
//!   panel.
//!
//! Every call takes the [`Session`] to run on and the [`ServerInfo`] read
//! while connecting, and runs through [`Session::execute`]. Server errors
//! that mean "you lack the privilege" become [`LiveError::PermissionDenied`]
//! so the UI can show them as such.

use serde::{Deserialize, Serialize};
use varve_catalog::rows::{self, Row};
use varve_catalog::{CatalogError, EditPlan};
use varve_core::Engine;
use varve_driver::{Dialect, DriverError, ServerInfo, Session};

mod mysql_readonly;
pub mod process;
pub mod status;
pub mod variables;

pub use process::{
    KillOutcome, KillTarget, Process, ProcessList, kill, kill_statement, own_connection_id,
    process_list,
};
pub use status::{
    DatabaseSize, GaleraStatus, ReplicaInfo, Replication, ReplicationRole, ServerStatus,
    StatusValue, UpstreamInfo, galera_status, server_status,
};
pub use variables::{
    EditScope, HIDDEN, ServerVariable, VariableChange, VariableType, plan_variable_change,
    server_variable, server_variables, variable_edit_plan,
};

/// A failure of a live server panel call.
#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LiveError {
    /// The server refused because the user lacks a privilege (PostgreSQL
    /// SQLSTATE `42501`, MySQL and MariaDB access denied errors).
    #[error("{message}")]
    PermissionDenied {
        /// The server's message.
        message: String,
        /// The engine error code (SQLSTATE or MySQL error number).
        code: Option<String>,
    },
    /// The request would kill the connection the panel itself runs on.
    #[error("connection {id} is this session's own connection and cannot be killed from here")]
    OwnConnection {
        /// The own connection or backend id.
        id: i64,
    },
    /// The variable, process or object does not exist.
    #[error("{message}")]
    NotFound {
        /// What was not found.
        message: String,
    },
    /// The engine or server version does not support this.
    #[error("{message}")]
    Unsupported {
        /// What is not supported.
        message: String,
    },
    /// The request is not valid, for example a value of the wrong type for
    /// a variable or a scope the variable does not have.
    #[error("{message}")]
    Invalid {
        /// What is wrong.
        message: String,
    },
    /// Any other server error.
    #[error("{error}")]
    Driver {
        /// The driver error.
        error: DriverError,
    },
    /// The server answered with something the panel did not expect.
    #[error("unexpected server result: {message}")]
    Unexpected {
        /// What was unexpected.
        message: String,
    },
}

/// Result of live server panel calls.
pub type LiveResult<T> = Result<T, LiveError>;

/// SQLSTATE `insufficient_privilege`.
const PG_INSUFFICIENT_PRIVILEGE: &str = "42501";

/// MySQL and MariaDB error numbers that mean a missing privilege:
/// `ER_DBACCESS_DENIED_ERROR` (1044), `ER_KILL_DENIED_ERROR` (1095),
/// `ER_TABLEACCESS_DENIED_ERROR` (1142), `ER_COLUMNACCESS_DENIED_ERROR`
/// (1143), `ER_SPECIFIC_ACCESS_DENIED_ERROR` (1227),
/// `ER_PERSIST_ONLY_ACCESS_DENIED_ERROR` (3630, MySQL).
const MY_PERMISSION_ERRORS: [u32; 6] = [1044, 1095, 1142, 1143, 1227, 3630];

/// The leading error number of a MySQL error code such as `1227 (42000)`.
pub(crate) fn mysql_error_number(code: &str) -> Option<u32> {
    code.split_whitespace().next()?.parse().ok()
}

/// True when a driver error means a missing privilege.
fn is_permission_error(error: &DriverError) -> bool {
    let Some(code) = error.code.as_deref() else {
        return false;
    };
    code == PG_INSUFFICIENT_PRIVILEGE
        || mysql_error_number(code).is_some_and(|n| MY_PERMISSION_ERRORS.contains(&n))
}

impl From<DriverError> for LiveError {
    fn from(error: DriverError) -> Self {
        if is_permission_error(&error) {
            Self::PermissionDenied {
                message: error.message,
                code: error.code,
            }
        } else {
            Self::Driver { error }
        }
    }
}

impl From<CatalogError> for LiveError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::Driver { error } => error.into(),
            CatalogError::NotFound { message } => Self::NotFound { message },
            CatalogError::Unsupported { message } => Self::Unsupported { message },
            CatalogError::Invalid { message } => Self::Invalid { message },
            CatalogError::Unexpected { message } => Self::Unexpected { message },
            CatalogError::NeedsConfirmation { message, .. } => Self::Invalid { message },
        }
    }
}

/// The SQL engine family a panel call runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Postgres,
    Mysql,
    Mariadb,
}

impl Family {
    /// The family of a server.
    ///
    /// # Errors
    ///
    /// [`LiveError::Unsupported`] for Redis, whose panels live in
    /// [`crate::redis`].
    pub(crate) fn of(server: &ServerInfo) -> LiveResult<Self> {
        match server.engine {
            Engine::Postgres => Ok(Self::Postgres),
            Engine::Mysql => Ok(Self::Mysql),
            Engine::Mariadb => Ok(Self::Mariadb),
            Engine::Redis => Err(LiveError::Unsupported {
                message: "Redis server panels are in the Redis live stats module".into(),
            }),
        }
    }

    /// The SQL dialect for quoting.
    pub(crate) fn dialect(self) -> Dialect {
        match self {
            Self::Postgres => Dialect::Postgres,
            Self::Mysql | Self::Mariadb => Dialect::Mysql,
        }
    }
}

/// True when the server is at least `major.minor.patch` (a missing patch
/// counts as 0).
pub(crate) fn at_least_patch(server: &ServerInfo, major: u32, minor: u32, patch: u32) -> bool {
    (server.major, server.minor, server.patch.unwrap_or(0)) >= (major, minor, patch)
}

/// A column as a float; `None` for `NULL`. Accepts floats, integers and
/// numeric text (MySQL `DECIMAL`, status values).
pub(crate) fn float(row: &Row<'_>, column: &str) -> LiveResult<Option<f64>> {
    match row.text(column)? {
        None => Ok(None),
        Some(text) => text
            .trim()
            .parse::<f64>()
            .map(Some)
            .map_err(|_| LiveError::Unexpected {
                message: format!("column {column} is not a number"),
            }),
    }
}

/// A column as text, with empty text read as `None`.
pub(crate) fn non_empty(row: &Row<'_>, column: &str) -> LiveResult<Option<String>> {
    Ok(row.text(column)?.filter(|text| !text.is_empty()))
}

/// Runs the statements of an [`EditPlan`] one at a time, in order.
///
/// Statements run separately because PostgreSQL rejects `ALTER SYSTEM` in
/// a multi-statement script (which runs as one implicit transaction). The
/// plans of this module are never transactional; a plan marked
/// transactional is refused here because this runner does not open a
/// transaction.
///
/// # Errors
///
/// [`LiveError::Invalid`] for a transactional plan, and the error of the
/// first statement that fails ([`LiveError::PermissionDenied`] for a
/// missing privilege). Statements before it have run.
pub async fn run_edit_plan(session: &dyn Session, plan: &EditPlan) -> LiveResult<()> {
    if plan.transactional {
        return Err(LiveError::Invalid {
            message: "transactional plans are run by the editor's transaction runner".into(),
        });
    }
    for statement in &plan.statements {
        rows::run(session, statement).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_driver::ErrorKind;

    #[test]
    fn permission_errors_are_typed() {
        let pg = DriverError::new(ErrorKind::Query, "permission denied").with_code("42501");
        assert!(matches!(
            LiveError::from(pg),
            LiveError::PermissionDenied { code: Some(code), .. } if code == "42501"
        ));
        let kill = DriverError::new(ErrorKind::Query, "You are not owner of thread 7")
            .with_code("1095 (HY000)");
        assert!(matches!(
            LiveError::from(kill),
            LiveError::PermissionDenied { .. }
        ));
        let super_needed = DriverError::new(ErrorKind::Query, "Access denied; you need SUPER")
            .with_code("1227 (42000)");
        assert!(matches!(
            LiveError::from(super_needed),
            LiveError::PermissionDenied { .. }
        ));
        let syntax = DriverError::new(ErrorKind::Query, "syntax").with_code("1064 (42000)");
        assert!(matches!(LiveError::from(syntax), LiveError::Driver { .. }));
        let none = DriverError::new(ErrorKind::ConnectionLost, "gone");
        assert!(matches!(LiveError::from(none), LiveError::Driver { .. }));
    }

    #[test]
    fn catalog_errors_keep_their_meaning() {
        let error = CatalogError::Driver {
            error: DriverError::new(ErrorKind::Query, "denied").with_code("42501"),
        };
        assert!(matches!(
            LiveError::from(error),
            LiveError::PermissionDenied { .. }
        ));
        assert!(matches!(
            LiveError::from(CatalogError::Invalid {
                message: "x".into()
            }),
            LiveError::Invalid { .. }
        ));
    }

    #[test]
    fn errors_serialize_for_the_ui() -> Result<(), serde_json::Error> {
        let error = LiveError::OwnConnection { id: 42 };
        assert_eq!(
            serde_json::to_value(&error)?,
            serde_json::json!({ "kind": "ownConnection", "id": 42 })
        );
        assert!(error.to_string().contains("42"));
        Ok(())
    }

    #[test]
    fn mysql_codes_parse() {
        assert_eq!(mysql_error_number("1094 (HY000)"), Some(1094));
        assert_eq!(mysql_error_number("1227"), Some(1227));
        assert_eq!(mysql_error_number("42501"), Some(42501));
        assert_eq!(mysql_error_number("57P01"), None);
    }

    #[test]
    fn patch_versions_compare() {
        let server = ServerInfo {
            engine: Engine::Mysql,
            version: "8.0.21".into(),
            major: 8,
            minor: 0,
            patch: Some(21),
        };
        assert!(!at_least_patch(&server, 8, 0, 22));
        assert!(at_least_patch(&server, 8, 0, 21));
        let newer = ServerInfo {
            version: "9.7".into(),
            major: 9,
            minor: 7,
            patch: None,
            ..server
        };
        assert!(at_least_patch(&newer, 8, 0, 22));
    }
}
