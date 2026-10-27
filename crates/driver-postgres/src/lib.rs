//! PostgreSQL driver for Varve (design doc sections 3 and 4).
//!
//! [`PostgresDriver`] opens a [`PostgresSession`] with tokio-postgres over
//! rustls. A session runs scripts statement by statement: each statement is
//! prepared to learn its column types, then run with the simple query
//! protocol so values arrive in PostgreSQL's lossless text format, which is
//! converted to [`varve_driver::Value`]s by type.
//!
//! Every session is set to `DateStyle = ISO, YMD`, `IntervalStyle =
//! iso_8601`, `TimeZone = 'UTC'` and `extra_float_digits = 3`, and to
//! `default_transaction_read_only = on` for read-only connections. A
//! read-only session refuses scripts with a statement that would switch that
//! off ([`varve_driver::read_only_violation`]) and checks after every
//! statement that it is still on, switching it back on (with a warning) when
//! a function turned it off. It runs every top-level `CALL` and `DO` inside
//! a read-only transaction block, so a procedure cannot commit its way out.
//!
//! `statement_timeout` from the connection profile is a default: a session
//! where the user sets another value keeps that value (Varve reads it, to
//! report a `57014` as a timeout only when the effective timeout passed);
//! after `RESET` or `DISCARD ALL` the profile's value is set again.
//!
//! Secrets: the stream tap never keeps statement text (it keeps statement
//! names, command tags and reported settings), and errors and logs carry no
//! server message text that could echo input. Below the tap, tokio-postgres
//! and the allocator do not zero their buffers, so the text of a statement
//! (a `PASSWORD '...'` in it, say) can stay in freed memory until it is
//! reused; nothing in Varve reads it from there.

mod convert;
mod error;
mod guard;
pub mod listen;
mod session;
#[cfg(test)]
mod sql_check;
mod tap;
#[cfg(test)]
mod test_certs;
mod tls;

use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, DriverError, Session};

pub use listen::{
    ListenConnector, ListenTarget, ListenerEvents, ListenerOptions, ListenerStatus, Notification,
    PgListener, TargetError,
};
pub use session::PostgresSession;

/// Opens PostgreSQL sessions (PostgreSQL 13 and newer).
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresDriver;

impl PostgresDriver {
    /// Creates the driver.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Driver for PostgresDriver {
    fn engines(&self) -> &'static [Engine] {
        &[Engine::Postgres]
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Session>, DriverError> {
        Ok(Box::new(PostgresSession::connect(params).await?))
    }
}
