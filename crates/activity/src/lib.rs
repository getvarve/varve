//! The local activity recorder and secret redactor (design doc sections 9.1
//! to 9.3, 9.5 and 9.6).
//!
//! - [`ActivityEvent`] is the event model, built with
//!   [`ActivityEvent::builder`] or one of the action constructors.
//! - [`prepare_statement_text`] decides what statement text is kept, after
//!   the secret redactor ([`redact_secrets`]) which always runs.
//!   [`prepare_error_message`] does the same for server error messages,
//!   which can echo statement text.
//! - [`Recorder`] writes events to the `core_activity` table of `app.db`,
//!   notifies subscribers, and serves the activity panel, the query history,
//!   retention, clearing and export.
//!
//! ```
//! use std::sync::Arc;
//! use varve_activity::{ActivityEvent, Recorder, RecorderSettings, RowCounts, SqlDialect};
//! use varve_store::{Store, core_migrations};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = Arc::new(Store::open_in_memory()?);
//! store.migrate(&[core_migrations()])?;
//! let recorder = Recorder::new(store, RecorderSettings::local("device-1"));
//!
//! recorder.record(
//!     ActivityEvent::query_executed(
//!         "SELECT * FROM users WHERE email = 'a@example.com'",
//!         SqlDialect::Postgres,
//!         "select",
//!         RowCounts { returned: Some(1), affected: None },
//!         4,
//!     )
//!     .connection("conn-1")
//!     .build()?,
//! )?;
//!
//! let history = recorder.query_history("conn-1", 50)?;
//! assert_eq!(history[0].details["statement"], "SELECT * FROM users WHERE email = ?");
//! # Ok(())
//! # }
//! ```

mod error;
mod event;
mod export;
mod query;
mod recorder;
mod redact;

pub use error::ActivityError;
pub use event::{
    ActivityEvent, ActivityEventBuilder, Category, ConnectionDetails, ErrorDetails, ExportDetails,
    LOCAL_ACTOR, Outcome, RowChanges, RowCounts, TunnelDetails, now_ms,
};
pub use query::{ActivityFilter, DEFAULT_PAGE_SIZE, DayCount, MAX_PAGE_SIZE, Page};
pub use recorder::{
    ACTION_CLEARED, ACTION_PRUNED, MAX_STATEMENT_BYTES, REDACTION_FAILED, Recorder,
    RecorderSettings, Retention, SubscriptionId,
};
pub use redact::{
    ERROR_MESSAGE_HIDDEN, ERROR_MESSAGE_OMITTED, ERROR_MESSAGE_WITHHELD, MAX_REDACTED_BYTES,
    REDACTED, SYNTAX_ERROR_HIDDEN, SqlDialect, TextLevel, prepare_error_message,
    prepare_statement_text, redact_secrets,
};

#[cfg(test)]
mod tests;
