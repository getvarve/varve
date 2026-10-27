//! Maps `mysql_async` errors to [`DriverError`].
//!
//! Messages come from the server or the I/O layer and never contain the
//! password: the protocol library never echoes it, and the messages built
//! here only name the host and port.

use mysql_async::{DriverError as MyDriverError, Error as MyError, IoError, ServerError};
use varve_driver::{DriverError, ErrorKind};

/// `ER_DBACCESS_DENIED_ERROR`: no access to the default schema.
const ER_DBACCESS_DENIED: u16 = 1044;
/// `ER_ACCESS_DENIED_ERROR`: wrong user name or password.
const ER_ACCESS_DENIED: u16 = 1045;
/// `ER_HOST_NOT_PRIVILEGED`: the user may not connect from this host.
const ER_HOST_NOT_PRIVILEGED: u16 = 1130;
/// `ER_NOT_SUPPORTED_AUTH_MODE`: the auth plugin is not supported.
const ER_NOT_SUPPORTED_AUTH_MODE: u16 = 1251;
/// `ER_ACCESS_DENIED_NO_PASSWORD_ERROR`.
const ER_ACCESS_DENIED_NO_PASSWORD: u16 = 1698;
/// `ER_MUST_CHANGE_PASSWORD_LOGIN`: the password expired.
const ER_MUST_CHANGE_PASSWORD_LOGIN: u16 = 1862;
/// `ER_ACCOUNT_HAS_BEEN_LOCKED` (MySQL).
const ER_ACCOUNT_HAS_BEEN_LOCKED: u16 = 3118;
/// `ER_QUERY_INTERRUPTED`: the statement was killed (`KILL QUERY`).
pub(crate) const ER_QUERY_INTERRUPTED: u16 = 1317;
/// `ER_QUERY_TIMEOUT` (MySQL): `max_execution_time` was exceeded.
const ER_QUERY_TIMEOUT: u16 = 3024;
/// `ER_STATEMENT_TIMEOUT` (MariaDB): `max_statement_time` was exceeded.
const ER_STATEMENT_TIMEOUT: u16 = 1969;
/// `ER_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION`.
const ER_READ_ONLY_TRANSACTION: u16 = 1792;
/// `ER_READ_ONLY_MODE` (MySQL `super_read_only`).
const ER_READ_ONLY_MODE: u16 = 1836;
/// `ER_NO_SUCH_THREAD`: the connection to kill is gone.
pub(crate) const ER_NO_SUCH_THREAD: u16 = 1094;

/// The error code shown to the user: error number and SQLSTATE, for example
/// `1064 (42000)`.
pub(crate) fn server_code(error: &ServerError) -> String {
    if error.state.is_empty() {
        error.code.to_string()
    } else {
        format!("{} ({})", error.code, error.state)
    }
}

/// Category of a server error raised by a statement.
pub(crate) fn statement_kind(code: u16) -> ErrorKind {
    match code {
        ER_QUERY_INTERRUPTED => ErrorKind::Cancelled,
        ER_QUERY_TIMEOUT | ER_STATEMENT_TIMEOUT => ErrorKind::Timeout,
        ER_READ_ONLY_TRANSACTION | ER_READ_ONLY_MODE => ErrorKind::ReadOnly,
        _ => ErrorKind::Query,
    }
}

/// Category of a server error raised while logging in.
fn connect_kind(code: u16) -> ErrorKind {
    match code {
        ER_DBACCESS_DENIED
        | ER_ACCESS_DENIED
        | ER_HOST_NOT_PRIVILEGED
        | ER_NOT_SUPPORTED_AUTH_MODE
        | ER_ACCESS_DENIED_NO_PASSWORD
        | ER_MUST_CHANGE_PASSWORD_LOGIN
        | ER_ACCOUNT_HAS_BEEN_LOCKED => ErrorKind::Auth,
        _ => ErrorKind::Connect,
    }
}

fn from_server(kind: ErrorKind, error: &ServerError) -> DriverError {
    DriverError::new(kind, error.message.clone()).with_code(server_code(error))
}

/// True when an I/O error was raised by rustls during the handshake.
fn is_tls_io(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some())
}

/// Maps an error raised while connecting to `endpoint` (`host:port`).
pub(crate) fn connect_error(error: &MyError, endpoint: &str) -> DriverError {
    match error {
        MyError::Server(server) => from_server(connect_kind(server.code), server),
        MyError::Io(IoError::Tls(tls)) => {
            DriverError::new(ErrorKind::Tls, format!("TLS with {endpoint} failed: {tls}"))
        }
        MyError::Io(IoError::Io(io)) if is_tls_io(io) => {
            DriverError::new(ErrorKind::Tls, format!("TLS with {endpoint} failed: {io}"))
        }
        MyError::Io(IoError::Io(io)) if io.kind() == std::io::ErrorKind::TimedOut => {
            DriverError::new(
                ErrorKind::Timeout,
                format!("connecting to {endpoint} timed out: {io}"),
            )
        }
        MyError::Io(IoError::Io(io)) => DriverError::new(
            ErrorKind::Connect,
            format!("could not connect to {endpoint}: {io}"),
        ),
        MyError::Driver(MyDriverError::NoClientSslFlagFromServer) => DriverError::new(
            ErrorKind::Tls,
            format!("{endpoint} does not support TLS, but the connection requires it"),
        ),
        MyError::Driver(MyDriverError::NoKeyFound) => DriverError::new(
            ErrorKind::Tls,
            "the client key file contains no private key",
        ),
        MyError::Driver(MyDriverError::ConnectionClosed) => DriverError::new(
            ErrorKind::Connect,
            format!("{endpoint} closed the connection while logging in"),
        ),
        MyError::Driver(driver) => DriverError::new(
            ErrorKind::Protocol,
            format!("connecting to {endpoint} failed: {driver}"),
        ),
        MyError::Other(other) => DriverError::new(
            ErrorKind::Protocol,
            format!("connecting to {endpoint} failed: {other}"),
        ),
        MyError::Url(url) => DriverError::new(
            ErrorKind::Protocol,
            format!("invalid connection options: {url}"),
        ),
    }
}

/// Maps an error raised by a statement or a ping on an open session.
pub(crate) fn session_error(error: &MyError) -> DriverError {
    match error {
        MyError::Server(server) => from_server(statement_kind(server.code), server),
        MyError::Io(io) => DriverError::new(
            ErrorKind::ConnectionLost,
            format!("the connection to the server was lost: {io}"),
        ),
        MyError::Driver(MyDriverError::ConnectionClosed) => DriverError::new(
            ErrorKind::ConnectionLost,
            "the connection to the server was closed",
        ),
        MyError::Driver(driver) => DriverError::new(ErrorKind::Protocol, driver.to_string()),
        MyError::Other(other) => DriverError::new(ErrorKind::Protocol, other.to_string()),
        MyError::Url(url) => DriverError::new(ErrorKind::Protocol, url.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(code: u16, state: &str, message: &str) -> MyError {
        MyError::Server(ServerError {
            code,
            message: message.into(),
            state: state.into(),
        })
    }

    #[test]
    fn statement_errors_carry_number_and_sqlstate() {
        let error = session_error(&server(
            1064,
            "42000",
            "You have an error in your SQL syntax",
        ));
        assert_eq!(error.kind, ErrorKind::Query);
        assert_eq!(error.code.as_deref(), Some("1064 (42000)"));
        assert_eq!(error.message, "You have an error in your SQL syntax");
    }

    #[test]
    fn statement_error_kinds() {
        assert_eq!(statement_kind(1317), ErrorKind::Cancelled);
        assert_eq!(statement_kind(3024), ErrorKind::Timeout);
        assert_eq!(statement_kind(1969), ErrorKind::Timeout);
        assert_eq!(statement_kind(1792), ErrorKind::ReadOnly);
        assert_eq!(statement_kind(1146), ErrorKind::Query);
    }

    #[test]
    fn login_errors() {
        let error = connect_error(
            &server(
                1045,
                "28000",
                "Access denied for user 'varve'@'%' (using password: YES)",
            ),
            "db:3306",
        );
        assert_eq!(error.kind, ErrorKind::Auth);
        assert_eq!(error.code.as_deref(), Some("1045 (28000)"));
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let error = connect_error(&MyError::Io(IoError::Io(refused)), "db:3306");
        assert_eq!(error.kind, ErrorKind::Connect);
        assert!(error.message.contains("db:3306"));
        let timed_out = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        let error = connect_error(&MyError::Io(IoError::Io(timed_out)), "db:3306");
        assert_eq!(error.kind, ErrorKind::Timeout);
        let tls = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        );
        let error = connect_error(&MyError::Io(IoError::Io(tls)), "db:3306");
        assert_eq!(error.kind, ErrorKind::Tls);
        let error = connect_error(
            &MyError::Driver(MyDriverError::NoClientSslFlagFromServer),
            "db:3306",
        );
        assert_eq!(error.kind, ErrorKind::Tls);
    }

    #[test]
    fn lost_connections() {
        let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let error = session_error(&MyError::Io(IoError::Io(reset)));
        assert_eq!(error.kind, ErrorKind::ConnectionLost);
    }
}
