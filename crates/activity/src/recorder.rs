//! The local activity recorder (design doc 9.4 and 9.5).

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use varve_store::{Store, StoreError};

use crate::error::ActivityError;
use crate::event::{ActivityEvent, Category, LOCAL_ACTOR, now_ms, validate_action};
use crate::query::{
    self, ActivityFilter, COLUMNS, Cursor, DayCount, Page, RawRow, cursor_of, fetch_rows,
};
use crate::redact::{
    ERROR_MESSAGE_WITHHELD, MAX_REDACTED_BYTES, PreparedStatement, SqlDialect, TextLevel,
    prepare_error_message_with, prepare_statement, redact_credentials, statement_literals,
    statement_raises,
};

/// Action recorded when the user clears the log (category [`Category::App`]).
pub const ACTION_CLEARED: &str = "activity.cleared";
/// Action recorded when retention removed old events (category
/// [`Category::App`]).
pub const ACTION_PRUNED: &str = "activity.pruned";

/// Longest statement text stored, in bytes. Longer text is cut at a
/// character boundary and marked with `statement_truncated`.
pub const MAX_STATEMENT_BYTES: usize = 64 * 1024;

/// Detail keys the recorder owns; values set by callers are replaced.
const RESERVED_DETAILS: [&str; 4] = ["statement", "statement_truncated", "text_level", "error"];

const EXPORT_PAGE: u32 = 500;
const DAY_MS: i64 = 86_400_000;

/// Who records and how much statement text is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecorderSettings {
    /// User id, or [`LOCAL_ACTOR`] when signed out.
    pub actor_id: String,
    /// This device.
    pub device_id: String,
    /// Statement text level (design doc 9.3).
    pub text_level: TextLevel,
}

impl RecorderSettings {
    /// Settings for a signed-out user on `device_id` at the default level.
    #[must_use]
    pub fn local(device_id: impl Into<String>) -> Self {
        RecorderSettings {
            actor_id: LOCAL_ACTOR.to_string(),
            device_id: device_id.into(),
            text_level: TextLevel::default(),
        }
    }
}

/// How long the local log keeps events (design doc 9.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    /// Keep everything (the default).
    #[default]
    Forever,
    /// Keep events of the last this many days.
    Days(u32),
}

/// Handle returned by [`Recorder::on_recorded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

type Subscriber = Arc<dyn Fn(&ActivityEvent) + Send + Sync>;

/// Records activity events in `core_activity` and reads them back.
pub struct Recorder {
    store: Arc<Store>,
    settings: RwLock<RecorderSettings>,
    subscribers: RwLock<Vec<(SubscriptionId, Subscriber)>>,
    next_subscription: AtomicU64,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("settings", &self.settings())
            .field(
                "subscribers",
                &self
                    .subscribers
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .len(),
            )
            .finish_non_exhaustive()
    }
}

/// A validated event with its encoded details.
struct Prepared {
    event: ActivityEvent,
    details: String,
}

impl Recorder {
    /// A recorder writing to `store`, which must have the core migrations
    /// applied.
    #[must_use]
    pub fn new(store: Arc<Store>, settings: RecorderSettings) -> Self {
        Recorder {
            store,
            settings: RwLock::new(settings),
            subscribers: RwLock::new(Vec::new()),
            next_subscription: AtomicU64::new(1),
        }
    }

    /// Current settings.
    #[must_use]
    pub fn settings(&self) -> RecorderSettings {
        self.settings
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the settings (sign-in, sign-out, text level change).
    pub fn set_settings(&self, settings: RecorderSettings) {
        *self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = settings;
    }

    /// Changes only the statement text level.
    pub fn set_text_level(&self, level: TextLevel) {
        self.settings
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .text_level = level;
    }

    /// Calls `callback` after every recorded event, including the events
    /// that record clearing and pruning. The app forwards these to extension
    /// hooks. A panicking callback is caught and logged; the event stays
    /// recorded.
    pub fn on_recorded(
        &self,
        callback: impl Fn(&ActivityEvent) + Send + Sync + 'static,
    ) -> SubscriptionId {
        let id = SubscriptionId(self.next_subscription.fetch_add(1, Ordering::Relaxed));
        self.subscribers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, Arc::new(callback)));
        id
    }

    /// Removes a callback. Returns false when it was not registered.
    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut subscribers = self
            .subscribers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let before = subscribers.len();
        subscribers.retain(|(sid, _)| *sid != id);
        subscribers.len() != before
    }

    /// Records one event.
    ///
    /// Fills in the actor and device from the settings when the event has
    /// none, stores statement text only through
    /// [`crate::prepare_statement_text`] and error messages only through
    /// [`crate::prepare_error_message`] at the current level (an error
    /// message also loses every secret removed from the event's
    /// statement), and redacts credentials in the target and in every
    /// string detail.
    ///
    /// # Errors
    /// [`ActivityError::InvalidAction`] or [`ActivityError::InvalidEvent`]
    /// for an invalid event, [`ActivityError::Store`] when the write fails.
    pub fn record(&self, event: ActivityEvent) -> Result<(), ActivityError> {
        let prepared = self.prepare(event)?;
        self.store
            .with_conn(|conn| insert(conn, &prepared).map_err(StoreError::from))?;
        self.notify(&prepared.event);
        Ok(())
    }

    /// Records several events in one transaction: all or none are stored.
    ///
    /// # Errors
    /// As [`Recorder::record`]; nothing is written when any event is invalid.
    pub fn record_batch(&self, events: Vec<ActivityEvent>) -> Result<(), ActivityError> {
        let prepared = events
            .into_iter()
            .map(|e| self.prepare(e))
            .collect::<Result<Vec<_>, _>>()?;
        self.store.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            for p in &prepared {
                insert(&tx, p)?;
            }
            tx.commit()?;
            Ok(())
        })?;
        for p in &prepared {
            self.notify(&p.event);
        }
        Ok(())
    }

    fn prepare(&self, mut event: ActivityEvent) -> Result<Prepared, ActivityError> {
        validate_action(&event.action)?;
        let settings = self.settings();
        if event.actor_id.trim().is_empty() {
            event.actor_id = settings.actor_id.clone();
        }
        if event.actor_id.trim().is_empty() {
            return Err(ActivityError::InvalidEvent("the actor id is empty".into()));
        }
        if event.device_id.is_none() {
            event.device_id = Some(settings.device_id.clone());
        }
        if event.duration_ms.is_some_and(|d| i64::try_from(d).is_err()) {
            return Err(ActivityError::InvalidEvent("duration out of range".into()));
        }

        let mut details = match std::mem::take(&mut event.details) {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            other => {
                return Err(ActivityError::InvalidEvent(format!(
                    "details must be a JSON object, got {other}"
                )));
            }
        };
        for key in RESERVED_DETAILS {
            details.remove(key);
        }
        // Details never hold secrets: strip credentials from every string.
        // The redactors must never take the app down: should one panic on
        // some input, the event is still recorded, fail closed (no text
        // that could not be checked), and flagged `redaction_failed`.
        let mut details = Value::Object(details);
        if catch_unwind(AssertUnwindSafe(|| {
            injected_failure("details");
            scrub(&mut details);
        }))
        .is_err()
        {
            redaction_failed("details");
            details = json!({ "details_omitted": REDACTION_FAILED });
        }
        if let Some(target) = event.target.as_mut() {
            match catch_unwind(AssertUnwindSafe(|| {
                injected_failure("target");
                redact_credentials(target).into_owned()
            })) {
                Ok(redacted) => *target = redacted,
                Err(_) => {
                    redaction_failed("target");
                    event.target = None;
                }
            }
        }

        let level = settings.text_level;
        // No statement: nothing of it can be echoed. `None` once the
        // statement could not be redacted.
        let mut statement_secrets: Option<Vec<String>> = Some(Vec::new());
        // The statement as given, read again for its literal values only
        // when an error message is stored at text level normalized.
        let mut statement_source: Option<(String, SqlDialect)> = None;
        if let Some(statement) = event.statement.take()
            && let Value::Object(map) = &mut details
        {
            map.insert("text_level".into(), level.as_str().into());
            if statement.text.len() > MAX_REDACTED_BYTES {
                // Too long to redact: only its length is kept.
                map.insert("statement_bytes".into(), statement.text.len().into());
                map.insert("statement_omitted".into(), "too_large".into());
            }
            let prepared = match catch_unwind(AssertUnwindSafe(|| {
                injected_failure("statement");
                prepare_statement(&statement.text, statement.dialect, level)
            })) {
                Ok(prepared) => prepared,
                Err(_) => {
                    redaction_failed("statement");
                    map.insert("statement_omitted".into(), REDACTION_FAILED.into());
                    PreparedStatement::default()
                }
            };
            statement_secrets = prepared.secrets;
            if let Some(mut text) = prepared.text {
                if text.len() > MAX_STATEMENT_BYTES {
                    let mut cut = MAX_STATEMENT_BYTES;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.truncate(cut);
                    map.insert("statement_truncated".into(), true.into());
                }
                map.insert("statement".into(), text.into());
            }
            statement_source = Some((statement.text, statement.dialect));
        }
        if let Some(pending) = event.error.take()
            && let Value::Object(map) = &mut details
        {
            let prepared = catch_unwind(AssertUnwindSafe(|| {
                injected_failure("error message");
                let mut secrets = statement_secrets.clone();
                let mut literals = Vec::new();
                let raises = statement_source
                    .as_ref()
                    .is_some_and(|(text, dialect)| statement_raises(text, *dialect));
                if level == TextLevel::Normalized
                    && let Some((text, dialect)) = &statement_source
                {
                    match statement_literals(text, *dialect) {
                        Some(found) => literals = found,
                        // Too costly to collect: the message is withheld.
                        None => secrets = None,
                    }
                }
                let message = prepare_error_message_with(
                    &pending.error.message,
                    pending.error.code.as_deref(),
                    pending.dialect,
                    level,
                    secrets.as_deref(),
                    &literals,
                    raises,
                );
                let code = pending
                    .error
                    .code
                    .as_deref()
                    .map(|code| redact_credentials(code).into_owned());
                (message, code)
            }));
            let (message, code) = prepared.unwrap_or_else(|_| {
                redaction_failed("error message");
                (ERROR_MESSAGE_WITHHELD.to_string(), None)
            });
            map.insert("error".into(), json!({ "code": code, "message": message }));
            map.insert("text_level".into(), level.as_str().into());
        }

        let encoded = serde_json::to_string(&details)?;
        event.details = details;
        Ok(Prepared {
            event,
            details: encoded,
        })
    }

    fn notify(&self, event: &ActivityEvent) {
        let subscribers: Vec<Subscriber> = self
            .subscribers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, s)| Arc::clone(s))
            .collect();
        for subscriber in subscribers {
            if catch_unwind(AssertUnwindSafe(|| subscriber(event))).is_err() {
                tracing::warn!(
                    action = %event.action,
                    id = %event.id,
                    "an activity subscriber panicked; the event is recorded"
                );
            }
        }
    }

    /// One page of events matching `filter`, newest first.
    ///
    /// # Errors
    /// [`ActivityError::UnknownCursor`] when `before_id` names no stored
    /// event, [`ActivityError::Store`] or [`ActivityError::Corrupt`] on read
    /// failures.
    pub fn list(&self, filter: &ActivityFilter) -> Result<Page, ActivityError> {
        let limit = filter.page_size();
        let rows = self.store.with_conn(|conn| {
            let cursor = match &filter.before_id {
                Some(id) => match cursor_of(conn, id)? {
                    Some(cursor) => Some(cursor),
                    None => return Ok(None),
                },
                None => None,
            };
            fetch_rows(conn, filter, cursor.as_ref(), limit + 1).map(Some)
        })?;
        let Some(mut rows) = rows else {
            return Err(ActivityError::UnknownCursor(
                filter.before_id.clone().unwrap_or_default(),
            ));
        };
        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let events = rows
            .into_iter()
            .map(RawRow::into_event)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = if has_more {
            events.last().map(|e| e.id.to_string())
        } else {
            None
        };
        Ok(Page {
            events,
            next_cursor,
        })
    }

    /// The latest `query.*` events of a connection, newest first (query
    /// history, design doc 9.5). `limit` is clamped like a page size.
    ///
    /// # Errors
    /// As [`Recorder::list`].
    pub fn query_history(
        &self,
        connection_id: &str,
        limit: u32,
    ) -> Result<Vec<ActivityEvent>, ActivityError> {
        let filter = ActivityFilter {
            categories: vec![Category::Query],
            connection_id: Some(connection_id.to_string()),
            limit,
            ..ActivityFilter::default()
        };
        Ok(self.list(&filter)?.events)
    }

    /// Events per UTC day matching `filter` (for example connects per day:
    /// actions `["connection.opened"]`). `before_id` and `limit` are
    /// ignored. Days without events are left out.
    ///
    /// # Errors
    /// [`ActivityError::Store`] when the read fails.
    pub fn count_by_day(&self, filter: &ActivityFilter) -> Result<Vec<DayCount>, ActivityError> {
        self.count_by_day_with_offset(filter, 0)
    }

    /// Like [`Recorder::count_by_day`], with days starting at midnight in
    /// the given UTC offset (the user's time zone).
    ///
    /// # Errors
    /// [`ActivityError::Store`] when the read fails.
    pub fn count_by_day_with_offset(
        &self,
        filter: &ActivityFilter,
        utc_offset_minutes: i32,
    ) -> Result<Vec<DayCount>, ActivityError> {
        let filter = ActivityFilter {
            before_id: None,
            ..filter.clone()
        };
        let rows = self
            .store
            .with_conn(|conn| query::count_rows_by_day(conn, &filter, utc_offset_minutes))?;
        Ok(rows
            .into_iter()
            .map(|(day, count)| query::day_count(day, count, utc_offset_minutes))
            .collect())
    }

    /// Deletes events older than the retention period and records
    /// `activity.pruned` when anything was removed. Returns the number of
    /// removed events.
    ///
    /// # Errors
    /// [`ActivityError::InvalidRetention`] for `Days(0)`,
    /// [`ActivityError::Store`] when the delete fails.
    pub fn apply_retention(&self, retention: Retention) -> Result<u64, ActivityError> {
        self.apply_retention_at(retention, now_ms())
    }

    pub(crate) fn apply_retention_at(
        &self,
        retention: Retention,
        now: i64,
    ) -> Result<u64, ActivityError> {
        let days = match retention {
            Retention::Forever => return Ok(0),
            Retention::Days(0) => {
                return Err(ActivityError::InvalidRetention(
                    "keep at least one day, or clear the log instead".into(),
                ));
            }
            Retention::Days(days) => days,
        };
        let cutoff = now.saturating_sub(i64::from(days).saturating_mul(DAY_MS));
        let event = ActivityEvent::builder(Category::App, ACTION_PRUNED)
            .occurred_at(now)
            .detail("retention_days", days)
            .detail("cutoff", cutoff)
            .build()?;
        self.delete_and_record(
            event,
            |conn| conn.execute("DELETE FROM core_activity WHERE occurred_at < ?", [cutoff]),
            true,
        )
    }

    /// Deletes all events, or the events of one connection, and records
    /// `activity.cleared` (clearing is itself logged). Returns the number of
    /// removed events.
    ///
    /// # Errors
    /// [`ActivityError::Store`] when the delete fails.
    pub fn clear(&self, connection_id: Option<&str>) -> Result<u64, ActivityError> {
        let mut builder = ActivityEvent::builder(Category::App, ACTION_CLEARED).detail(
            "scope",
            if connection_id.is_some() {
                "connection"
            } else {
                "all"
            },
        );
        if let Some(id) = connection_id {
            builder = builder.connection(id);
        }
        let event = builder.build()?;
        let connection_id = connection_id.map(str::to_string);
        self.delete_and_record(
            event,
            move |conn| match &connection_id {
                Some(id) => conn.execute("DELETE FROM core_activity WHERE connection_id = ?", [id]),
                None => conn.execute("DELETE FROM core_activity", []),
            },
            false,
        )
    }

    /// Runs `delete` and records `event` (with the removed count) in one
    /// transaction. With `only_when_removed`, nothing is recorded when the
    /// delete removed no rows.
    fn delete_and_record(
        &self,
        event: ActivityEvent,
        delete: impl FnOnce(&Connection) -> rusqlite::Result<usize>,
        only_when_removed: bool,
    ) -> Result<u64, ActivityError> {
        let mut prepared = self.prepare(event)?;
        let (removed, recorded) = self.store.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let removed = u64::try_from(delete(&tx)?).unwrap_or(u64::MAX);
            let recorded = !(only_when_removed && removed == 0);
            if recorded {
                insert_with_removed(&tx, &prepared, removed)?;
            }
            tx.commit()?;
            Ok((removed, recorded))
        })?;
        if recorded {
            if let Value::Object(map) = &mut prepared.event.details {
                map.insert("removed".into(), json!(removed));
            }
            self.notify(&prepared.event);
        }
        Ok(removed)
    }

    /// Streams every event matching `filter` (newest first, starting below
    /// `before_id` when set; `limit` is ignored) as CSV with a header row.
    /// Returns the number of events written.
    ///
    /// # Errors
    /// [`ActivityError::Io`] when writing fails, otherwise as
    /// [`Recorder::list`].
    pub fn export_csv<W: Write>(
        &self,
        filter: &ActivityFilter,
        mut writer: W,
    ) -> Result<u64, ActivityError> {
        writer.write_all(crate::export::CSV_HEADER.as_bytes())?;
        let count = self.for_each_export_page(filter, |events| {
            for event in events {
                crate::export::write_csv_row(&mut writer, event)?;
            }
            Ok(())
        })?;
        writer.flush()?;
        Ok(count)
    }

    /// Streams every event matching `filter` as JSON Lines, one event per
    /// line. Returns the number of events written.
    ///
    /// # Errors
    /// As [`Recorder::export_csv`].
    pub fn export_jsonl<W: Write>(
        &self,
        filter: &ActivityFilter,
        mut writer: W,
    ) -> Result<u64, ActivityError> {
        let count = self.for_each_export_page(filter, |events| {
            for event in events {
                serde_json::to_writer(&mut writer, event)?;
                writer.write_all(b"\n")?;
            }
            Ok(())
        })?;
        writer.flush()?;
        Ok(count)
    }

    /// Reads `filter` in pages of [`EXPORT_PAGE`] and hands each page to
    /// `sink`. The connection is not held while `sink` runs.
    fn for_each_export_page(
        &self,
        filter: &ActivityFilter,
        mut sink: impl FnMut(&[ActivityEvent]) -> Result<(), ActivityError>,
    ) -> Result<u64, ActivityError> {
        let mut cursor: Option<Cursor> = match &filter.before_id {
            Some(id) => Some(
                self.store
                    .with_conn(|conn| cursor_of(conn, id))?
                    .ok_or_else(|| ActivityError::UnknownCursor(id.clone()))?,
            ),
            None => None,
        };
        let mut total = 0u64;
        loop {
            let rows = self
                .store
                .with_conn(|conn| fetch_rows(conn, filter, cursor.as_ref(), EXPORT_PAGE))?;
            let fetched = rows.len();
            let events = rows
                .into_iter()
                .map(RawRow::into_event)
                .collect::<Result<Vec<_>, _>>()?;
            sink(&events)?;
            total += events.len() as u64;
            match events.last() {
                Some(last) if fetched == EXPORT_PAGE as usize => {
                    cursor = Some((last.occurred_at, last.id.to_string()));
                }
                _ => return Ok(total),
            }
        }
    }
}

/// The value of `statement_omitted` / `details_omitted` when a redactor
/// failed on the text: it was not stored.
pub const REDACTION_FAILED: &str = "redaction_failed";

/// Logs that a redactor failed on part of an event, without any of its
/// text.
fn redaction_failed(part: &str) {
    tracing::error!(
        part,
        "a redactor failed; that part of the event is not stored"
    );
}

#[cfg(test)]
thread_local! {
    /// The part of an event (`"details"`, `"target"`, `"statement"`,
    /// `"error message"`) whose redactor fails on this thread, so tests
    /// can check that [`Recorder::record`] fails closed.
    pub(crate) static FAILING_REDACTOR: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Fails like a redactor that panics, when a test asked for it on `part`.
#[cfg(test)]
pub(crate) fn injected_failure(part: &str) {
    if FAILING_REDACTOR.with(std::cell::Cell::get) == Some(part) {
        std::panic::resume_unwind(Box::new(format!("injected failure: {part}")));
    }
}

#[cfg(not(test))]
pub(crate) fn injected_failure(_part: &str) {}

/// Replaces credentials in every string of a JSON value.
fn scrub(value: &mut Value) {
    match value {
        Value::String(s) => {
            if let std::borrow::Cow::Owned(clean) = redact_credentials(s) {
                *s = clean;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(scrub),
        Value::Object(map) => map.values_mut().for_each(scrub),
        _ => {}
    }
}

fn insert(conn: &Connection, prepared: &Prepared) -> rusqlite::Result<()> {
    insert_row(conn, prepared, "?", None)
}

/// Inserts the event with `removed` added to its details.
fn insert_with_removed(
    conn: &Connection,
    prepared: &Prepared,
    removed: u64,
) -> rusqlite::Result<()> {
    let removed = i64::try_from(removed).unwrap_or(i64::MAX);
    insert_row(conn, prepared, "json_set(?, '$.removed', ?)", Some(removed))
}

fn insert_row(
    conn: &Connection,
    prepared: &Prepared,
    details_sql: &str,
    removed: Option<i64>,
) -> rusqlite::Result<()> {
    let e = &prepared.event;
    let id = e.id.to_string();
    let duration = e.duration_ms.and_then(|d| i64::try_from(d).ok());
    let mut stmt = conn.prepare_cached(&format!(
        "INSERT INTO core_activity ({COLUMNS}, upload_state) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {details_sql}, 'local_only')"
    ))?;
    let base = params![
        id,
        e.occurred_at,
        e.actor_id,
        e.device_id,
        e.workspace_id,
        e.connection_id,
        e.server_key,
        e.category.as_str(),
        e.action,
        e.outcome.as_str(),
        duration,
        e.target,
        prepared.details,
    ];
    match removed {
        None => stmt.execute(base)?,
        Some(removed) => {
            let mut all: Vec<&dyn rusqlite::ToSql> = base.to_vec();
            all.push(&removed);
            stmt.execute(all.as_slice())?
        }
    };
    Ok(())
}
