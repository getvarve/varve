//! Maps I/O, TLS and Redis errors to [`DriverError`]. Messages never contain
//! the password: they are built from the server's error text and the
//! endpoint only.

use std::io;

use varve_driver::{DriverError, ErrorKind};

use crate::reply::server_error_text;
use crate::session::MAX_REPLY_BYTES;

/// Error codes that mean the credentials were rejected.
const AUTH_CODES: [&str; 2] = ["WRONGPASS", "NOAUTH"];

/// Error for a server error reply. `code` is the error prefix
/// (`WRONGTYPE`, `ERR`, `NOPERM`, ...).
pub fn from_server_error(error: &redis::ServerError) -> DriverError {
    let code = error.code().to_string();
    let kind = if AUTH_CODES.contains(&code.as_str()) {
        ErrorKind::Auth
    } else {
        ErrorKind::Query
    };
    DriverError::new(kind, server_error_text(error)).with_code(code)
}

/// Error for a failure of the `redis` crate while talking to the server.
pub fn from_redis(error: &redis::RedisError) -> DriverError {
    if let Some(code) = error.code() {
        let code = code.to_string();
        let message = match error.detail() {
            Some(detail) => format!("{code} {detail}"),
            None => code.clone(),
        };
        let kind = if AUTH_CODES.contains(&code.as_str())
            || error.kind() == redis::ErrorKind::AuthenticationFailed
        {
            ErrorKind::Auth
        } else {
            ErrorKind::Query
        };
        return DriverError::new(kind, message).with_code(code);
    }
    if error.kind() == redis::ErrorKind::AuthenticationFailed {
        return DriverError::new(
            ErrorKind::Auth,
            "Authentication failed: the server rejected the user name or password",
        )
        .with_code("WRONGPASS");
    }
    if error.is_timeout() {
        return DriverError::new(ErrorKind::Timeout, "The server did not answer in time");
    }
    if error.is_connection_refusal() {
        return DriverError::new(ErrorKind::Connect, format!("Connection refused: {error}"));
    }
    if error.is_connection_dropped() || error.is_io_error() || error.is_unrecoverable_error() {
        return DriverError::new(
            ErrorKind::ConnectionLost,
            format!("The connection to the server was lost: {error}"),
        );
    }
    DriverError::new(ErrorKind::Protocol, error.to_string())
}

/// Error for a failed TCP connect.
pub fn from_connect_io(error: &io::Error, endpoint: &str) -> DriverError {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut => ErrorKind::Timeout,
        _ => ErrorKind::Connect,
    };
    DriverError::new(kind, format!("Could not connect to {endpoint}: {error}"))
}

/// Error for a failed TLS handshake or TLS setup.
pub fn tls(message: impl std::fmt::Display) -> DriverError {
    DriverError::new(ErrorKind::Tls, format!("TLS: {message}"))
}

/// Error returned by a cancelled call.
pub fn cancelled() -> DriverError {
    DriverError::new(ErrorKind::Cancelled, "The command was cancelled")
}

/// Error returned by a cancelled call when the server-side kill did not
/// succeed (for example `NOPERM` for an ACL user without `CLIENT KILL`).
/// `reason` says why and goes into the detail.
pub fn cancelled_client_side(reason: impl Into<String>) -> DriverError {
    let mut error = DriverError::new(
        ErrorKind::Cancelled,
        "The command was cancelled in Varve and its connection was closed, but the server \
         could not be asked to stop it: the command may keep running on the server",
    );
    error.detail = Some(reason.into());
    error
}

/// Error for a command that ran past the statement timeout. The connection
/// is closed, so a late reply can never be taken for the reply to a later
/// command, but the server may still finish the command.
pub fn statement_timeout(limit: std::time::Duration) -> DriverError {
    DriverError::new(
        ErrorKind::Timeout,
        format!(
            "The command did not finish within the statement timeout of {} s. Varve closed the \
             connection and reconnects on the next command; the server may still finish the \
             command, so a write can still take effect",
            limit.as_secs_f32()
        ),
    )
}

/// Error for a reply larger than [`MAX_REPLY_BYTES`]. The read stopped at
/// the limit, so the connection is closed and reopened on the next call.
pub fn reply_too_large() -> DriverError {
    DriverError::new(
        ErrorKind::Query,
        format!(
            "The reply is larger than {} MiB, the most Varve reads for one command. Read the \
             value in parts instead, for example with GETRANGE, LRANGE, HSCAN, SSCAN, ZRANGE or \
             XRANGE ... COUNT",
            MAX_REPLY_BYTES / (1024 * 1024)
        ),
    )
}

/// Error for a reply with an unexpected shape.
pub fn protocol(message: impl Into<String>) -> DriverError {
    DriverError::new(ErrorKind::Protocol, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_error(line: &str) -> redis::ServerError {
        match redis::parse_redis_value(format!("-{line}\r\n").as_bytes()) {
            Ok(redis::Value::ServerError(error)) => error,
            other => panic!("not an error reply: {other:?}"),
        }
    }

    #[test]
    fn server_errors_keep_their_prefix() {
        let error =
            server_error("WRONGTYPE Operation against a key holding the wrong kind of value");
        let direct = from_server_error(&error);
        assert_eq!(direct.kind, ErrorKind::Query);
        assert_eq!(direct.code.as_deref(), Some("WRONGTYPE"));
        let mapped = from_redis(&redis::RedisError::from(error));
        assert_eq!(mapped.kind, ErrorKind::Query);
        assert_eq!(mapped.code.as_deref(), Some("WRONGTYPE"));
        assert!(mapped.message.starts_with("WRONGTYPE Operation"));
    }

    #[test]
    fn auth_errors_map_to_auth() {
        for code in ["WRONGPASS", "NOAUTH"] {
            let error = server_error(&format!("{code} invalid username-password pair"));
            assert_eq!(from_server_error(&error).kind, ErrorKind::Auth);
            let error = redis::RedisError::from(error);
            assert_eq!(from_redis(&error).kind, ErrorKind::Auth);
        }
    }

    #[test]
    fn refused_and_timeouts() {
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert_eq!(
            from_connect_io(&refused, "localhost:1").kind,
            ErrorKind::Connect
        );
        let timed_out = io::Error::from(io::ErrorKind::TimedOut);
        assert_eq!(
            from_connect_io(&timed_out, "localhost:1").kind,
            ErrorKind::Timeout
        );
        let error = redis::RedisError::from(io::Error::from(io::ErrorKind::ConnectionRefused));
        assert_eq!(from_redis(&error).kind, ErrorKind::Connect);
    }
}
