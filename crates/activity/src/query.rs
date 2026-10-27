//! Reading the activity log: filters, pages and day counts (design doc 9.6).

use std::str::FromStr;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, Row, params_from_iter};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use varve_store::StoreError;

use crate::error::ActivityError;
use crate::event::{ActivityEvent, Category, Outcome};

/// Default page size of [`ActivityFilter::limit`].
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Largest page [`crate::Recorder::list`] returns.
pub const MAX_PAGE_SIZE: u32 = 500;

const DAY_MS: i64 = 86_400_000;

/// Which events to read. Empty lists and `None` fields do not filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ActivityFilter {
    /// Only events at or after this time (unix ms).
    pub from: Option<i64>,
    /// Only events before this time (unix ms, exclusive).
    pub to: Option<i64>,
    /// Only these categories.
    pub categories: Vec<Category>,
    /// Only these actions. An entry ending in `.*` matches a prefix
    /// (`transfer.*`).
    pub actions: Vec<String>,
    /// Only these outcomes.
    pub outcomes: Vec<Outcome>,
    /// Only this connection profile.
    pub connection_id: Option<String>,
    /// Only this workspace.
    pub workspace_id: Option<String>,
    /// Only this actor (member).
    pub actor_id: Option<String>,
    /// Only this device.
    pub device_id: Option<String>,
    /// Case-insensitive search in the action, the target and the details.
    pub text: Option<String>,
    /// Cursor: only events older than this event (the `next_cursor` of the
    /// previous page).
    pub before_id: Option<String>,
    /// Page size, clamped to 1 ..= [`MAX_PAGE_SIZE`]. Exports ignore it.
    pub limit: u32,
}

impl Default for ActivityFilter {
    fn default() -> Self {
        ActivityFilter {
            from: None,
            to: None,
            categories: Vec::new(),
            actions: Vec::new(),
            outcomes: Vec::new(),
            connection_id: None,
            workspace_id: None,
            actor_id: None,
            device_id: None,
            text: None,
            before_id: None,
            limit: DEFAULT_PAGE_SIZE,
        }
    }
}

impl ActivityFilter {
    /// The effective page size.
    #[must_use]
    pub fn page_size(&self) -> u32 {
        self.limit.clamp(1, MAX_PAGE_SIZE)
    }
}

/// One page of events, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    /// The events.
    pub events: Vec<ActivityEvent>,
    /// Pass as [`ActivityFilter::before_id`] to read the next page; `None`
    /// on the last page.
    pub next_cursor: Option<String>,
}

/// Number of events on one day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DayCount {
    /// The day, `YYYY-MM-DD`, in the requested UTC offset.
    pub day: String,
    /// Start of that day as unix milliseconds.
    pub day_start_ms: i64,
    /// Number of events.
    pub count: u64,
}

/// Position in the newest-first order.
pub(crate) type Cursor = (i64, String);

pub(crate) const COLUMNS: &str = "id, occurred_at, actor_id, device_id, workspace_id, \
    connection_id, server_key, category, action, outcome, duration_ms, target, details";

/// A stored row before validation, so decoding errors are reported outside
/// the connection lock.
pub(crate) struct RawRow {
    id: String,
    occurred_at: i64,
    actor_id: String,
    device_id: Option<String>,
    workspace_id: Option<String>,
    connection_id: Option<String>,
    server_key: Option<String>,
    category: String,
    action: String,
    outcome: String,
    duration_ms: Option<i64>,
    target: Option<String>,
    details: String,
}

impl RawRow {
    pub(crate) fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(RawRow {
            id: row.get(0)?,
            occurred_at: row.get(1)?,
            actor_id: row.get(2)?,
            device_id: row.get(3)?,
            workspace_id: row.get(4)?,
            connection_id: row.get(5)?,
            server_key: row.get(6)?,
            category: row.get(7)?,
            action: row.get(8)?,
            outcome: row.get(9)?,
            duration_ms: row.get(10)?,
            target: row.get(11)?,
            details: row.get(12)?,
        })
    }

    pub(crate) fn into_event(self) -> Result<ActivityEvent, ActivityError> {
        let corrupt = |reason: String| ActivityError::Corrupt {
            id: self.id.clone(),
            reason,
        };
        let id = Uuid::from_str(&self.id).map_err(|e| corrupt(format!("id: {e}")))?;
        let category = Category::from_str(&self.category).map_err(|e| corrupt(e.to_string()))?;
        let outcome = Outcome::from_str(&self.outcome).map_err(|e| corrupt(e.to_string()))?;
        let duration_ms = self
            .duration_ms
            .map(u64::try_from)
            .transpose()
            .map_err(|_| corrupt("negative duration".into()))?;
        let details =
            serde_json::from_str(&self.details).map_err(|e| corrupt(format!("details: {e}")))?;
        Ok(ActivityEvent {
            id,
            occurred_at: self.occurred_at,
            actor_id: self.actor_id,
            device_id: self.device_id,
            workspace_id: self.workspace_id,
            connection_id: self.connection_id,
            server_key: self.server_key,
            category,
            action: self.action,
            outcome,
            duration_ms,
            target: self.target,
            details,
            statement: None,
            error: None,
        })
    }
}

/// Escapes `%`, `_` and `\` for `LIKE ... ESCAPE '\'`.
fn like_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// Builds the `WHERE` clause and its parameters.
pub(crate) fn where_clause(
    filter: &ActivityFilter,
    cursor: Option<&Cursor>,
) -> (String, Vec<SqlValue>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();
    if let Some(from) = filter.from {
        clauses.push("occurred_at >= ?".into());
        params.push(SqlValue::Integer(from));
    }
    if let Some(to) = filter.to {
        clauses.push("occurred_at < ?".into());
        params.push(SqlValue::Integer(to));
    }
    if !filter.categories.is_empty() {
        clauses.push(format!(
            "category IN ({})",
            placeholders(filter.categories.len())
        ));
        params.extend(
            filter
                .categories
                .iter()
                .map(|c| SqlValue::Text(c.as_str().into())),
        );
    }
    if !filter.actions.is_empty() {
        let mut alternatives = Vec::new();
        for action in &filter.actions {
            if let Some(prefix) = action.strip_suffix(".*") {
                alternatives.push("action LIKE ? ESCAPE '\\'");
                params.push(SqlValue::Text(format!("{}.%", like_escape(prefix))));
            } else {
                alternatives.push("action = ?");
                params.push(SqlValue::Text(action.clone()));
            }
        }
        clauses.push(format!("({})", alternatives.join(" OR ")));
    }
    if !filter.outcomes.is_empty() {
        clauses.push(format!(
            "outcome IN ({})",
            placeholders(filter.outcomes.len())
        ));
        params.extend(
            filter
                .outcomes
                .iter()
                .map(|o| SqlValue::Text(o.as_str().into())),
        );
    }
    for (column, value) in [
        ("connection_id", &filter.connection_id),
        ("workspace_id", &filter.workspace_id),
        ("actor_id", &filter.actor_id),
        ("device_id", &filter.device_id),
    ] {
        if let Some(value) = value {
            clauses.push(format!("{column} = ?"));
            params.push(SqlValue::Text(value.clone()));
        }
    }
    if let Some(text) = filter
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let pattern = format!("%{}%", like_escape(text));
        clauses.push(
            "(action LIKE ? ESCAPE '\\' OR target LIKE ? ESCAPE '\\' OR details LIKE ? ESCAPE '\\')"
                .into(),
        );
        params.extend(std::iter::repeat_n(SqlValue::Text(pattern), 3));
    }
    if let Some((occurred_at, id)) = cursor {
        // The first conjunct is a range on the time indexes; the second
        // breaks ties within one millisecond by id.
        clauses.push("occurred_at <= ? AND (occurred_at < ? OR id < ?)".into());
        params.push(SqlValue::Integer(*occurred_at));
        params.push(SqlValue::Integer(*occurred_at));
        params.push(SqlValue::Text(id.clone()));
    }
    if clauses.is_empty() {
        (String::new(), params)
    } else {
        (format!(" WHERE {}", clauses.join(" AND ")), params)
    }
}

/// SQL and parameters of one newest-first page.
pub(crate) fn page_sql(
    filter: &ActivityFilter,
    cursor: Option<&Cursor>,
    limit: u32,
) -> (String, Vec<SqlValue>) {
    let (where_sql, mut params) = where_clause(filter, cursor);
    params.push(SqlValue::Integer(i64::from(limit)));
    (
        format!(
            "SELECT {COLUMNS} FROM core_activity{where_sql} \
             ORDER BY occurred_at DESC, id DESC LIMIT ?"
        ),
        params,
    )
}

/// Reads one page of raw rows.
pub(crate) fn fetch_rows(
    conn: &Connection,
    filter: &ActivityFilter,
    cursor: Option<&Cursor>,
    limit: u32,
) -> Result<Vec<RawRow>, StoreError> {
    let (sql, params) = page_sql(filter, cursor, limit);
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(params), RawRow::read)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Looks up the position of the event `id`.
pub(crate) fn cursor_of(conn: &Connection, id: &str) -> Result<Option<Cursor>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT occurred_at FROM core_activity WHERE id = ?")?;
    let mut rows = stmt.query([id])?;
    match rows.next()? {
        Some(row) => Ok(Some((row.get(0)?, id.to_string()))),
        None => Ok(None),
    }
}

/// Counts events per day, with days starting at midnight in
/// `utc_offset_minutes`.
pub(crate) fn count_rows_by_day(
    conn: &Connection,
    filter: &ActivityFilter,
    utc_offset_minutes: i32,
) -> Result<Vec<(i64, u64)>, StoreError> {
    let (where_sql, params) = where_clause(filter, None);
    let offset_ms = i64::from(utc_offset_minutes) * 60_000;
    // Floor division, also correct before 1970.
    let sql = format!(
        "SELECT day, COUNT(*) FROM (\
            SELECT ((occurred_at + {offset_ms}) - \
                    ((((occurred_at + {offset_ms}) % {DAY_MS}) + {DAY_MS}) % {DAY_MS})) / {DAY_MS} AS day \
            FROM core_activity{where_sql}\
         ) GROUP BY day ORDER BY day"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(params), |row| {
            let day: i64 = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((day, u64::try_from(count).unwrap_or(0)))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Converts days since 1970-01-01 to a civil date (proleptic Gregorian).
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// `YYYY-MM-DD` for a day number.
pub(crate) fn format_day(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for unix milliseconds.
pub(crate) fn format_utc(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(DAY_MS);
    let ms_of_day = unix_ms.rem_euclid(DAY_MS);
    let (h, rest) = (ms_of_day / 3_600_000, ms_of_day % 3_600_000);
    let (min, rest) = (rest / 60_000, rest % 60_000);
    let (s, ms) = (rest / 1000, rest % 1000);
    format!("{}T{h:02}:{min:02}:{s:02}.{ms:03}Z", format_day(days))
}

pub(crate) fn day_count(day: i64, count: u64, utc_offset_minutes: i32) -> DayCount {
    DayCount {
        day: format_day(day),
        day_start_ms: day * DAY_MS - i64::from(utc_offset_minutes) * 60_000,
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(format_day(0), "1970-01-01");
        assert_eq!(format_day(-1), "1969-12-31");
        assert_eq!(format_day(19_782), "2024-02-29");
        assert_eq!(format_day(20_745), "2026-10-19");
        assert_eq!(format_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_utc(1_760_875_200_123), "2025-10-19T12:00:00.123Z");
        assert_eq!(format_utc(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn like_patterns_are_escaped() {
        assert_eq!(like_escape("50%_a\\b"), "50\\%\\_a\\\\b");
    }

    #[test]
    fn page_size_is_clamped() {
        let mut filter = ActivityFilter::default();
        assert_eq!(filter.page_size(), DEFAULT_PAGE_SIZE);
        filter.limit = 0;
        assert_eq!(filter.page_size(), 1);
        filter.limit = 10_000;
        assert_eq!(filter.page_size(), MAX_PAGE_SIZE);
    }
}
