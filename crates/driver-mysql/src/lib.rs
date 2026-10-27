//! MySQL and MariaDB driver for Varve (design doc sections 2.2, 3 and 4).
//!
//! One crate serves both engines because they share the wire protocol;
//! above the wire they are separate engines. The engine reported in
//! [`varve_driver::ServerInfo::engine`] is detected from `VERSION()` and
//! `@@version_comment`, whatever the connection profile said.
//!
//! # Session setup
//!
//! Right after logging in, every session reads `VERSION()`,
//! `@@version_comment` and `CONNECTION_ID()` (the id that cancel kills;
//! the handshake id is not used because proxies may rewrite it), then runs:
//!
//! 1. `SET SESSION TRANSACTION READ ONLY` for read-only profiles. An
//!    `init_sql` that would switch read-only mode off, or that holds
//!    dynamic SQL the session cannot check, is refused (see
//!    [`MysqlSession`]).
//! 2. The statement timeout: on MySQL `SET SESSION max_execution_time`
//!    (milliseconds), which MySQL applies to read-only `SELECT` statements
//!    only; on MariaDB `SET SESSION max_statement_time` (seconds), which
//!    applies to every statement.
//! 3. Session state tracking: `time_zone`, the tracking variables and (in
//!    read-only sessions) the access mode variable are appended to
//!    `session_track_system_variables` (the server's own entries stay), and
//!    read-only sessions set `session_track_transaction_info =
//!    CHARACTERISTICS`. This needs the Varve patch to `mysql_async` in
//!    `vendor/mysql_async`, which asks for `CLIENT_SESSION_TRACK`.
//! 4. The profile's `init_sql`, which may hold several statements.
//! 5. `SET time_zone = '+00:00'`, last, so `TIMESTAMP` values are UTC even
//!    when `init_sql` set another zone. `sql_mode` is left as the server
//!    configured it. The server must report this `SET`; if it does not,
//!    tracking does not work: a read-only connection is then refused, a
//!    normal one reads the time zone with a query when a script may have
//!    changed it.
//! 6. `SET NAMES utf8mb4`, last of all, whatever the server's
//!    `init_connect` or the init SQL set, and a check that the session is
//!    utf8mb4.
//! 7. For read-only profiles, a check that read-only mode is still on.
//!
//! The connect timeout covers all of it, including `init_sql`.
//!
//! See [`MysqlSession`] for how read-only mode and the time zone are kept
//! track of while the session runs.
//!
//! # TLS
//!
//! TLS uses rustls with the `ring` provider. `Prefer` falls back to a plain
//! connection only when the server offers no TLS. `Require` encrypts without
//! checking the certificate. `VerifyCa` checks the chain against the CA file
//! (or the OS trust store) but not the host name. `VerifyFull` also checks
//! the host name against [`varve_driver::TlsParams::server_name`] or the
//! host. Like every rustls client, host name checks use the certificate's
//! subjectAltName only; certificates with just a common name (such as the
//! ones MySQL generates on first start) cannot pass `VerifyFull`.
//!
//! `VerifyCa` hands the TLS layer the made-up name
//! `NotValidForName.verify-ca.invalid` instead of the host, so that
//! `mysql_async` can tell a name mismatch from every other certificate
//! error. That name is also sent as SNI: plain MySQL and MariaDB servers
//! ignore SNI, but proxies and gateways that route connections by SNI cannot
//! route `VerifyCa` connections. Use `VerifyFull` (with
//! [`varve_driver::TlsParams::server_name`] when needed) behind such a proxy.
//!
//! # Cancel
//!
//! [`varve_driver::CancelHandle::cancel`] opens a short-lived second
//! connection with the same parameters and runs `KILL QUERY <id>`, repeated
//! every 200 ms until the script ends. The next call on the session waits
//! until every kill has been answered, so a late kill cannot hit a later
//! statement. A call returns [`varve_driver::ErrorKind::Cancelled`] only
//! when a statement was interrupted (it failed with error 1317, or a kill
//! was confirmed while the script still ran); a script that finished before
//! the kill reached the server returns its results with a note that the
//! cancel came too late.

/// Fails to compile when one of the types implements `Debug`. Used for the
/// types that hold `mysql_async::Opts`: its `Debug` prints the client
/// identity, which holds the decrypted TLS client key as PEM (see
/// `connect::client_identity`), so none of them may ever derive `Debug` or
/// be logged.
macro_rules! assert_not_debug {
    ($($t:ty),+ $(,)?) => {
        $(
            const _: fn() = || {
                trait AmbiguousIfDebug<A> {
                    fn some_item() {}
                }
                impl<T: ?Sized> AmbiguousIfDebug<()> for T {}
                struct IsDebug;
                impl<T: ?Sized + std::fmt::Debug> AmbiguousIfDebug<IsDebug> for T {}
                let _ = <$t as AmbiguousIfDebug<_>>::some_item;
            };
        )+
    };
}

mod connect;
mod dump;
mod error;
mod guard;
mod overrides;
mod session;
mod tracking;
mod types;

use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, DriverError, ErrorKind, Session};

pub use dump::normalize_dump_boilerplate;
pub use overrides::SessionOverrides;
pub use session::MysqlSession;

/// The MySQL and MariaDB driver.
#[derive(Debug, Clone, Copy, Default)]
pub struct MysqlDriver;

impl MysqlDriver {
    /// Creates the driver.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Opens a session and returns the concrete type, for callers that need
    /// MySQL-specific methods.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Connect`] when the server cannot be reached,
    /// [`ErrorKind::Auth`] for rejected credentials, [`ErrorKind::Tls`] for
    /// TLS failures, [`ErrorKind::Timeout`] when connecting takes longer than
    /// the connect timeout, and the statement error when the session setup
    /// or `init_sql` fails.
    pub async fn open(&self, params: &ConnectParams) -> Result<MysqlSession, DriverError> {
        if !matches!(params.engine, Engine::Mysql | Engine::Mariadb) {
            return Err(DriverError::new(
                ErrorKind::Unsupported,
                format!(
                    "the MySQL driver cannot connect to {:?} servers",
                    params.engine
                ),
            ));
        }
        let opened = connect::open(params).await?;
        Ok(MysqlSession::new(opened, params.connect_timeout))
    }
}

#[async_trait::async_trait]
impl Driver for MysqlDriver {
    fn engines(&self) -> &'static [Engine] {
        &[Engine::Mysql, Engine::Mariadb]
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
        Ok(Box::new(self.open(params).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_both_engines() {
        assert_eq!(
            MysqlDriver::new().engines(),
            [Engine::Mysql, Engine::Mariadb]
        );
    }

    #[tokio::test]
    async fn rejects_other_engines() {
        let params = ConnectParams::new(Engine::Postgres, "localhost", 5432);
        let error = MysqlDriver::new().open(&params).await.err().map(|e| e.kind);
        assert_eq!(error, Some(ErrorKind::Unsupported));
    }

    #[tokio::test]
    async fn refused_connections_are_connect_errors() {
        // Port 1 on localhost is closed on every test machine.
        let params = ConnectParams::new(Engine::Mysql, "127.0.0.1", 1);
        let error = MysqlDriver::new().open(&params).await.err();
        assert_eq!(error.map(|e| e.kind), Some(ErrorKind::Connect));
    }
}
