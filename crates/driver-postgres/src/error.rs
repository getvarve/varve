//! Maps tokio-postgres and I/O errors to [`DriverError`].

use std::error::Error as StdError;
use std::io;

use tokio_postgres::error::{DbError, ErrorPosition, Severity};
use varve_driver::{DriverError, ErrorKind};

/// SQLSTATE of a statement cancelled by a cancel request or a timeout.
pub(crate) const QUERY_CANCELED: &str = "57014";

/// The text of an error and all its causes, joined by `": "`.
fn chain_text(error: &(dyn StdError + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let part = cause.to_string();
        if !text.ends_with(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = cause.source();
    }
    text
}

/// Every error in the chain, including errors wrapped in an [`io::Error`]
/// (whose `source` skips the wrapped error itself).
fn chain<'a>(error: &'a (dyn StdError + 'static)) -> Vec<&'a (dyn StdError + 'static)> {
    let mut out = Vec::new();
    let mut next = Some(error);
    while let Some(current) = next {
        out.push(current);
        // An `io::Error`'s `source` is the source of the error it wraps, so
        // continue with the wrapped error itself to see it too.
        next = match current
            .downcast_ref::<io::Error>()
            .and_then(io::Error::get_ref)
        {
            Some(inner) => Some(inner as &(dyn StdError + 'static)),
            None => current.source(),
        };
    }
    out
}

fn find_rustls_error<'a>(error: &'a (dyn StdError + 'static)) -> Option<&'a rustls::Error> {
    chain(error)
        .into_iter()
        .find_map(|e| e.downcast_ref::<rustls::Error>())
}

fn find_io_error<'a>(error: &'a (dyn StdError + 'static)) -> Option<&'a io::Error> {
    chain(error)
        .into_iter()
        .find_map(|e| e.downcast_ref::<io::Error>())
}

/// Removes the password from a message, should it ever appear in one.
/// Neither the server nor tokio-postgres echo it, so this is a last guard.
/// Very short passwords are not replaced: they would match ordinary words.
pub(crate) fn redact(mut message: String, password: Option<&str>) -> String {
    if let Some(password) = password.filter(|p| p.chars().count() >= 4)
        && message.contains(password)
    {
        message = message.replace(password, "********");
    }
    message
}

fn from_db_error(db: &DbError, kind: ErrorKind, position_offset: usize) -> DriverError {
    let position = match db.position() {
        Some(ErrorPosition::Original(position)) => u32::try_from(position_offset)
            .ok()
            .and_then(|offset| offset.checked_add(*position)),
        _ => None,
    };
    DriverError {
        kind,
        message: db.message().to_owned(),
        code: Some(db.code().code().to_owned()),
        detail: db.detail().map(str::to_owned),
        hint: db.hint().map(str::to_owned),
        position,
        partial: None,
    }
}

/// Maps an I/O error from opening the TCP connection.
pub(crate) fn socket_error(host: &str, port: u16, error: &io::Error) -> DriverError {
    let kind = if error.kind() == io::ErrorKind::TimedOut {
        ErrorKind::Timeout
    } else {
        ErrorKind::Connect
    };
    DriverError::new(kind, format!("could not connect to {host}:{port}: {error}"))
}

/// What went wrong while TLS was being negotiated, as far as the tap saw.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnectContext<'a> {
    /// The mode demands TLS.
    pub(crate) tls_required: bool,
    /// The byte the server answered the `SSLRequest` with.
    pub(crate) ssl_response: Option<u8>,
    /// The password, to keep it out of messages.
    pub(crate) password: Option<&'a str>,
}

/// Maps an error from the startup and authentication phase.
pub(crate) fn connect_error(
    error: &tokio_postgres::Error,
    context: ConnectContext<'_>,
) -> DriverError {
    let text = redact(chain_text(error), context.password);
    if let Some(db) = error.as_db_error() {
        let code = db.code().code();
        let kind = if code.starts_with("28") {
            ErrorKind::Auth
        } else {
            ErrorKind::Connect
        };
        let mut mapped = from_db_error(db, kind, 0);
        mapped.message = redact(mapped.message, context.password);
        return mapped;
    }
    if let Some(tls) = find_rustls_error(error) {
        return DriverError::new(
            ErrorKind::Tls,
            redact(format!("TLS handshake failed: {tls}"), context.password),
        );
    }
    if context.tls_required && context.ssl_response.is_some_and(|b| b != b'S') {
        return DriverError::new(ErrorKind::Tls, "the server does not support TLS");
    }
    // tokio-postgres does not expose its error kinds; its fixed messages
    // ("error performing TLS handshake", "timeout waiting for server",
    // "authentication error", "invalid configuration: password missing")
    // are the only way to tell them apart.
    let kind = if text.starts_with("error performing TLS handshake") {
        ErrorKind::Tls
    } else if text.starts_with("timeout waiting for server") {
        ErrorKind::Timeout
    } else if text.starts_with("authentication error") || text.contains("password missing") {
        ErrorKind::Auth
    } else if text.starts_with("unexpected message from server")
        || text.starts_with("error parsing response from server")
    {
        ErrorKind::Protocol
    } else if find_io_error(error).is_some_and(|e| e.kind() == io::ErrorKind::TimedOut) {
        ErrorKind::Timeout
    } else {
        ErrorKind::Connect
    };
    DriverError::new(kind, text)
}

/// Why a statement may have been interrupted (SQLSTATE `57014`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Interrupt {
    /// Varve sent a cancel request for this execution.
    pub(crate) requested: bool,
    /// The configured `statement_timeout` had passed when the error came.
    pub(crate) timed_out: bool,
}

impl Interrupt {
    /// Nothing known: an internal query.
    pub(crate) const NONE: Self = Self {
        requested: false,
        timed_out: false,
    };

    /// For a statement that started at `started`, with the configured
    /// statement timeout `timeout_ms` (if any).
    pub(crate) fn for_statement(
        requested: bool,
        started: std::time::Instant,
        timeout_ms: Option<u128>,
    ) -> Self {
        Self {
            requested,
            timed_out: timeout_ms.is_some_and(|ms| started.elapsed().as_millis() >= ms),
        }
    }
}

/// The server's untranslated message for a statement stopped by
/// `statement_timeout`.
const TIMEOUT_MESSAGE: &str = "canceling statement due to statement timeout";

/// The server's untranslated message for a statement stopped by a cancel
/// request (Varve's own, or `pg_cancel_backend` from another session).
const CANCEL_MESSAGE: &str = "canceling statement due to user request";

/// Prefix of the message of a `57014` that neither Varve's cancel request
/// nor a statement timeout explains.
const OUTSIDE_CANCEL_PREFIX: &str = "cancelled by the server or another session: ";

/// What stopped a statement, as far as the server's own message says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelCause {
    /// `statement_timeout`.
    Timeout,
    /// A cancel request.
    Request,
}

/// The cause of a `57014` read from its message. The server sends the
/// message in `lc_messages`: only the untranslated English text tells the
/// causes apart; `None` for any other text.
pub(crate) fn cancel_cause(message: &str) -> Option<CancelCause> {
    match message {
        TIMEOUT_MESSAGE => Some(CancelCause::Timeout),
        CANCEL_MESSAGE => Some(CancelCause::Request),
        _ => None,
    }
}

/// The function of the server that reports a cancel or a statement timeout
/// (the `R` field of its error, never translated).
const INTERRUPTS_ROUTINE: &str = "ProcessInterrupts";

/// What the server says about where an error came from, beyond its code.
/// User code can raise any SQLSTATE with any message (`RAISE ... USING
/// ERRCODE`), but not these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Origin {
    /// Severity `FATAL` or `PANIC`: the server ends the connection after
    /// it. User code cannot raise either.
    fatal: bool,
    /// Reported by a function other than the server's interrupt handling
    /// (for example PL/pgSQL's `exec_stmt_raise`): user code raised it.
    /// `false` when the server did not say (it always does).
    raised_elsewhere: bool,
}

impl Origin {
    fn of(db: &DbError) -> Self {
        let fatal = match db.parsed_severity() {
            Some(severity) => matches!(severity, Severity::Fatal | Severity::Panic),
            None => matches!(db.severity(), "FATAL" | "PANIC"),
        };
        Self {
            fatal,
            raised_elsewhere: db.routine().is_some_and(|r| r != INTERRUPTS_ROUTINE),
        }
    }
}

/// The error kind for a server error with SQLSTATE `code` and `message`.
///
/// Only a `FATAL` (or `PANIC`) error ends the connection: a `57P01` that a
/// function raised is an ordinary error of a session that goes on.
///
/// `57014` is a cancel or a `statement_timeout` when the server's interrupt
/// handling reports it, and then its untranslated message says which. A
/// translated one does not (`lc_messages` translates it), so Varve decides:
/// its own cancel request is a cancel; the timeout in force having passed
/// is a timeout; anything else was cancelled by the server or another
/// session (`pg_cancel_backend`). See [`timeout_unknown`].
///
/// The function that reported it (`ProcessInterrupts`) is how a `57014`
/// raised by user code (`RAISE ... USING ERRCODE`, whose message proves
/// nothing) is told apart: that is an ordinary error. It decides only
/// whether the message is trusted. When Varve itself sent a cancel request,
/// any `57014` is reported as cancelled, whoever reported it: a handler
/// that catches the cancel and raises it again (`RAISE ... USING ERRCODE =
/// SQLSTATE`), or a server that names the function differently, still
/// stopped because the user asked. Without a cancel request such a `57014`
/// is an ordinary error, also a statement timeout re-raised by a handler or
/// reported under another function name (the fallback: a timeout is then
/// shown as a query error, never the other way round).
fn kind_for_code(code: &str, message: &str, origin: Origin, interrupt: Interrupt) -> ErrorKind {
    if origin.fatal {
        return ErrorKind::ConnectionLost;
    }
    match code {
        "25006" => ErrorKind::ReadOnly,
        QUERY_CANCELED if origin.raised_elsewhere && interrupt.requested => ErrorKind::Cancelled,
        QUERY_CANCELED if origin.raised_elsewhere => ErrorKind::Query,
        QUERY_CANCELED => match cancel_cause(message) {
            Some(CancelCause::Timeout) => ErrorKind::Timeout,
            Some(CancelCause::Request) => ErrorKind::Cancelled,
            None if interrupt.requested => ErrorKind::Cancelled,
            None if interrupt.timed_out => ErrorKind::Timeout,
            None => ErrorKind::Cancelled,
        },
        _ if code.starts_with("28") => ErrorKind::Auth,
        _ => ErrorKind::Query,
    }
}

/// Maps an error from running a statement. `position_offset` is the
/// statement's offset in the script, in characters; `interrupt` tells a
/// user cancel, a statement timeout and an outside cancel apart when the
/// server's message does not.
pub(crate) fn exec_error(
    error: &tokio_postgres::Error,
    position_offset: usize,
    interrupt: Interrupt,
) -> DriverError {
    if let Some(db) = error.as_db_error() {
        let code = db.code().code();
        let kind = kind_for_code(code, db.message(), Origin::of(db), interrupt);
        let mut mapped = from_db_error(db, kind, position_offset);
        if code == QUERY_CANCELED && kind == ErrorKind::Cancelled && !interrupt.requested {
            mapped.message = format!("{OUTSIDE_CANCEL_PREFIX}{}", mapped.message);
        }
        return mapped;
    }
    let text = chain_text(error);
    let kind = if error.is_closed() || find_io_error(error).is_some() {
        ErrorKind::ConnectionLost
    } else {
        ErrorKind::Protocol
    };
    DriverError::new(kind, text)
}

/// The server's own message of a mapped error (without Varve's prefix).
fn server_message(error: &DriverError) -> &str {
    error
        .message
        .strip_prefix(OUTSIDE_CANCEL_PREFIX)
        .unwrap_or(&error.message)
}

/// True when `error` is a cancel or timeout the server reported (a `57014`
/// that user code raised is mapped as an ordinary query error).
pub(crate) fn is_server_interrupt(error: &DriverError) -> bool {
    error.code.as_deref() == Some(QUERY_CANCELED)
        && matches!(error.kind, ErrorKind::Cancelled | ErrorKind::Timeout)
}

/// For a cancel or timeout the server reported: the cause its message
/// gives, if the message is untranslated. `None` for any other error.
pub(crate) fn cancel_cause_of(error: &DriverError) -> Option<CancelCause> {
    if !is_server_interrupt(error) {
        return None;
    }
    cancel_cause(server_message(error))
}

/// True when `error` is a cancel or timeout the server reported, that
/// Varve did not request, and whose message is translated: whether it was
/// a timeout is then decided from the `statement_timeout` in force
/// ([`decide_timeout`]).
pub(crate) fn timeout_unknown(error: &DriverError, requested: bool) -> bool {
    !requested && is_server_interrupt(error) && cancel_cause_of(error).is_none()
}

/// For a `57014` with a translated message: the statement timeout in force
/// when it ran has passed (`timed_out`), or it was cancelled by the server
/// or another session.
pub(crate) fn decide_timeout(mut error: DriverError, timed_out: bool) -> DriverError {
    let message = server_message(&error).to_owned();
    if timed_out {
        error.kind = ErrorKind::Timeout;
        error.message = message;
    } else {
        error.kind = ErrorKind::Cancelled;
        error.message = format!("{OUTSIDE_CANCEL_PREFIX}{message}");
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_passwords() {
        assert_eq!(
            redact("bad secret123 here".into(), Some("secret123")),
            "bad ******** here"
        );
        assert_eq!(redact("a b c".into(), Some("a")), "a b c");
        assert_eq!(redact("no password".into(), None), "no password");
    }

    #[test]
    fn cancel_and_timeout_are_told_apart() {
        let requested = Interrupt {
            requested: true,
            timed_out: true,
        };
        let timed_out = Interrupt {
            requested: false,
            timed_out: true,
        };
        let translated = "Anweisung wird abgebrochen";
        assert_eq!(
            kind_for_code(QUERY_CANCELED, translated, Origin::default(), requested),
            ErrorKind::Cancelled
        );
        assert_eq!(
            kind_for_code(QUERY_CANCELED, translated, Origin::default(), timed_out),
            ErrorKind::Timeout
        );
        assert_eq!(
            kind_for_code(
                QUERY_CANCELED,
                translated,
                Origin::default(),
                Interrupt::NONE
            ),
            ErrorKind::Cancelled
        );
        // The untranslated message decides, whatever Varve guessed.
        assert_eq!(
            kind_for_code(
                QUERY_CANCELED,
                TIMEOUT_MESSAGE,
                Origin::default(),
                Interrupt::NONE
            ),
            ErrorKind::Timeout
        );
        assert_eq!(
            kind_for_code(
                QUERY_CANCELED,
                TIMEOUT_MESSAGE,
                Origin::default(),
                requested
            ),
            ErrorKind::Timeout
        );
        assert_eq!(
            kind_for_code(QUERY_CANCELED, CANCEL_MESSAGE, Origin::default(), timed_out),
            ErrorKind::Cancelled
        );
        assert_eq!(
            kind_for_code("25006", "", Origin::default(), Interrupt::NONE),
            ErrorKind::ReadOnly
        );
        // Only a FATAL error ends the connection; user code can raise the
        // SQLSTATE of a terminated backend, but only as an ERROR.
        let fatal = Origin {
            fatal: true,
            raised_elsewhere: false,
        };
        assert_eq!(
            kind_for_code("57P01", "", fatal, Interrupt::NONE),
            ErrorKind::ConnectionLost
        );
        assert_eq!(
            kind_for_code("25P03", "", fatal, Interrupt::NONE),
            ErrorKind::ConnectionLost
        );
        for code in ["57P01", "57P02", "57P03"] {
            assert_eq!(
                kind_for_code(code, "", Origin::default(), Interrupt::NONE),
                ErrorKind::Query
            );
        }
        // A 57014 user code raised is an ordinary error, whatever it says.
        let raised = Origin {
            fatal: false,
            raised_elsewhere: true,
        };
        for message in [TIMEOUT_MESSAGE, CANCEL_MESSAGE, "x"] {
            assert_eq!(
                kind_for_code(QUERY_CANCELED, message, raised, timed_out),
                ErrorKind::Query
            );
            // Unless Varve asked for a cancel: a handler re-raised it.
            assert_eq!(
                kind_for_code(QUERY_CANCELED, message, raised, requested),
                ErrorKind::Cancelled
            );
        }
        assert_eq!(
            kind_for_code("28P01", "", Origin::default(), Interrupt::NONE),
            ErrorKind::Auth
        );
        assert_eq!(
            kind_for_code("42601", "", Origin::default(), requested),
            ErrorKind::Query
        );
        let started = std::time::Instant::now() - std::time::Duration::from_millis(50);
        assert!(Interrupt::for_statement(false, started, Some(10)).timed_out);
        assert!(!Interrupt::for_statement(false, started, Some(60_000)).timed_out);
        assert!(!Interrupt::for_statement(false, started, None).timed_out);
    }

    #[test]
    fn translated_cancels_are_decided_later() {
        let outside = DriverError {
            code: Some(QUERY_CANCELED.into()),
            ..DriverError::new(
                ErrorKind::Cancelled,
                format!("{OUTSIDE_CANCEL_PREFIX}Anweisung wird abgebrochen"),
            )
        };
        assert!(timeout_unknown(&outside, false));
        assert!(!timeout_unknown(&outside, true));
        let timeout = decide_timeout(outside.clone(), true);
        assert_eq!(timeout.kind, ErrorKind::Timeout);
        assert_eq!(timeout.message, "Anweisung wird abgebrochen");
        // Deciding again gives the same answer.
        assert!(timeout_unknown(&timeout, false));
        let cancelled = decide_timeout(timeout, false);
        assert_eq!(cancelled, outside);
        let english = DriverError {
            code: Some(QUERY_CANCELED.into()),
            ..DriverError::new(ErrorKind::Timeout, TIMEOUT_MESSAGE)
        };
        assert!(!timeout_unknown(&english, false));
        let other = DriverError {
            code: Some("22012".into()),
            ..DriverError::new(ErrorKind::Query, "division by zero")
        };
        assert!(!timeout_unknown(&other, false));
        // Raised by user code: never decided, never a cause.
        let raised = DriverError {
            code: Some(QUERY_CANCELED.into()),
            ..DriverError::new(ErrorKind::Query, TIMEOUT_MESSAGE)
        };
        assert!(!timeout_unknown(&raised, false));
        assert_eq!(cancel_cause_of(&raised), None);
        assert_eq!(cancel_cause_of(&english), Some(CancelCause::Timeout));
    }

    #[test]
    fn io_errors_from_connecting() {
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        let error = socket_error("db", 5432, &refused);
        assert_eq!(error.kind, ErrorKind::Connect);
        assert!(error.message.starts_with("could not connect to db:5432"));
        let timed_out = io::Error::from(io::ErrorKind::TimedOut);
        assert_eq!(
            socket_error("db", 5432, &timed_out).kind,
            ErrorKind::Timeout
        );
    }

    #[test]
    fn finds_rustls_errors_inside_io_errors() {
        let inner = io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        );
        let outer = io::Error::other(inner);
        assert!(find_rustls_error(&outer).is_some());
    }
}
