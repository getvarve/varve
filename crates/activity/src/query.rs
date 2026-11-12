//! Reading the activity log: filters, orders, pages and day counts (design
//! doc 9.6).
//!
//! # Orders and cursors
//!
//! Pages use keyset paging (never `OFFSET`): a cursor holds the position of
//! the last event of a page, so the next page starts right after it, also
//! when that event was deleted meanwhile.
//!
//! - Time: by `occurred_at`, ties by id, both in the chosen direction. The
//!   default, newest first, keeps the cursor form `<occurred_at>:<id>`; a
//!   bare event id (the oldest form) is still accepted for it.
//! - Action, outcome, duration, connection: by the column's value in the
//!   chosen direction, and events with equal values newest first in both
//!   directions (as the panel's own sort did). Events without a value (no
//!   duration, no connection) come last in both directions. The cursor is
//!   `<column>.<asc|desc>:<occurred_at>:<id>:<value>`.
//!
//! Each value is read as one index range, newest first, and the next value
//! is found with one index seek, so a page costs the same at the start and
//! deep into a large log. A cursor made for one order is refused for
//! another ([`ActivityError::CursorOrderMismatch`]) instead of giving wrong
//! pages.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params_from_iter};
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

/// The names the connection order sorts by: connection profile id to name.
/// An event whose connection id has no name here belongs to a deleted
/// connection.
pub type ConnectionNames = HashMap<String, String>;

/// The column the activity log is sorted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SortColumn {
    /// When the event happened.
    Time,
    /// The action, in byte order. Actions are dotted lowercase identifiers,
    /// so that is also the case-insensitive order.
    Action,
    /// The outcome: failed, then cancelled, then successful (ascending).
    Outcome,
    /// How long it took. Events without a duration come last.
    Duration,
    /// The connection's name, compared without regard to case and with
    /// digit runs by their numeric value ([`compare_names`]). Events of a
    /// deleted connection come after every named one, events without a
    /// connection last.
    Connection,
}

impl SortColumn {
    const ALL: [SortColumn; 5] = [
        SortColumn::Time,
        SortColumn::Action,
        SortColumn::Outcome,
        SortColumn::Duration,
        SortColumn::Connection,
    ];

    /// The name used on the wire and in cursors.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SortColumn::Time => "time",
            SortColumn::Action => "action",
            SortColumn::Outcome => "outcome",
            SortColumn::Duration => "duration",
            SortColumn::Connection => "connection",
        }
    }
}

/// The direction of an [`ActivityOrder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SortDirection {
    /// Smallest first: oldest, A to Z, failed first, fastest.
    Ascending,
    /// Largest first: newest, Z to A, successful first, slowest.
    Descending,
}

impl SortDirection {
    /// `ordering` read in this direction.
    fn apply(self, ordering: Ordering) -> Ordering {
        match self {
            SortDirection::Ascending => ordering,
            SortDirection::Descending => ordering.reverse(),
        }
    }

    fn cursor_word(self) -> &'static str {
        match self {
            SortDirection::Ascending => "asc",
            SortDirection::Descending => "desc",
        }
    }
}

/// The order of the activity pages. The default is newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivityOrder {
    /// The column sorted on.
    pub column: SortColumn,
    /// Its direction.
    pub direction: SortDirection,
}

impl Default for ActivityOrder {
    fn default() -> Self {
        ActivityOrder {
            column: SortColumn::Time,
            direction: SortDirection::Descending,
        }
    }
}

impl fmt::Display for ActivityOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let direction = match self.direction {
            SortDirection::Ascending => "ascending",
            SortDirection::Descending => "descending",
        };
        write!(f, "{} {direction}", self.column.as_str())
    }
}

impl ActivityOrder {
    /// Newest first: the order of the old cursor forms.
    pub(crate) fn is_default(self) -> bool {
        self == ActivityOrder::default()
    }

    /// `<column>.<asc|desc>`, the head of a cursor.
    fn tag(self) -> String {
        format!("{}.{}", self.column.as_str(), self.direction.cursor_word())
    }

    fn from_tag(tag: &str) -> Option<Self> {
        let (column, direction) = tag.split_once('.')?;
        let column = SortColumn::ALL.into_iter().find(|c| c.as_str() == column)?;
        let direction = match direction {
            "asc" => SortDirection::Ascending,
            "desc" => SortDirection::Descending,
            _ => return None,
        };
        Some(ActivityOrder { column, direction })
    }
}

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
    /// Search in the action, the target and the text and number values of
    /// the details (not their key names). Case-insensitive for ASCII
    /// letters only: SQLite's `LIKE` does not fold other letters, so `é`
    /// does not find `É`.
    pub text: Option<String>,
    /// The order of the pages; newest first by default. Exports ignore it
    /// and always write newest first.
    pub order: ActivityOrder,
    /// Cursor: only events after this position in [`ActivityFilter::order`]
    /// (the `next_cursor` of the previous page, which must have been read
    /// in the same order). The cursor holds the position itself, so it
    /// stays valid when that event was deleted meanwhile. For newest first,
    /// the earlier forms (`<occurred_at>:<id>` and a bare event id) are
    /// still accepted.
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
            order: ActivityOrder::default(),
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

/// One page of events, in the order of the filter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    /// The events.
    pub events: Vec<ActivityEvent>,
    /// Pass as [`ActivityFilter::before_id`], with the same filter and
    /// order, to read the next page; `None` on the last page. Opaque (the
    /// module documentation lists the forms).
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

/// The sort value of a run of events in an order other than time: all
/// events with equal values, read newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Group {
    /// An action, an outcome (`failure`) or a connection name.
    Text(String),
    /// A duration in milliseconds.
    Int(i64),
    /// Events of connections that no longer have a profile.
    Deleted,
    /// Events without a value (no duration, no connection).
    Missing,
}

impl Group {
    /// The last part of a cursor: `=<text>`, `#<number>`, `d` or `n`.
    fn encode(&self) -> String {
        match self {
            Group::Text(text) => format!("={text}"),
            Group::Int(value) => format!("#{value}"),
            Group::Deleted => "d".into(),
            Group::Missing => "n".into(),
        }
    }

    /// Reads [`Group::encode`]; `None` when the group cannot occur in
    /// `column`.
    fn decode(text: &str, column: SortColumn) -> Option<Self> {
        let group = if let Some(rest) = text.strip_prefix('=') {
            Group::Text(rest.to_string())
        } else if let Some(rest) = text.strip_prefix('#') {
            Group::Int(rest.parse().ok()?)
        } else if text == "d" {
            Group::Deleted
        } else if text == "n" {
            Group::Missing
        } else {
            return None;
        };
        let fits = match (column, &group) {
            (SortColumn::Action, Group::Text(_))
            | (SortColumn::Duration, Group::Int(_) | Group::Missing)
            | (SortColumn::Connection, Group::Text(_) | Group::Deleted | Group::Missing) => true,
            (SortColumn::Outcome, Group::Text(outcome)) => Outcome::from_str(outcome).is_ok(),
            _ => false,
        };
        fits.then_some(group)
    }
}

/// A place in an order: right after the event at `occurred_at` with `id`,
/// whose sort value is `group` (`None` in the time order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) occurred_at: i64,
    pub(crate) id: String,
    pub(crate) group: Option<Group>,
}

/// What a `before_id` stands for in an order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// A position in that order.
    At(Position),
    /// Not a cursor, or a bare id of an event that is not stored.
    Unknown,
    /// A cursor of this other order.
    OtherOrder(ActivityOrder),
}

impl Resolved {
    /// The error for a cursor that is no position in `order`.
    pub(crate) fn into_error(self, token: &str, order: ActivityOrder) -> ActivityError {
        match self {
            Resolved::OtherOrder(cursor_order) => ActivityError::CursorOrderMismatch {
                cursor: token.to_string(),
                cursor_order,
                order,
            },
            Resolved::At(_) | Resolved::Unknown => ActivityError::UnknownCursor(token.to_string()),
        }
    }
}

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

/// True when every detail value that contains `text` also has it, as
/// typed, in the stored JSON of its event. Then a plain `LIKE` over the
/// stored JSON rules out most events before any of them is parsed, which
/// matters because no index helps a text search and the log can hold
/// hundreds of megabytes.
///
/// The details are written by `serde_json`, which changes only `"`, `\` and
/// control characters inside a string, so text without those is in the
/// stored JSON exactly as it is in the value. Numbers are different: the
/// search reads them as SQLite prints them (`1.0e+16`, fifteen significant
/// digits), not as they were written, so text that could be part of a
/// number (digits, `.`, `e`, `+`, `-` only) is searched in the parsed values
/// alone.
fn raw_details_hold(text: &str) -> bool {
    let encoded_as_typed = !text
        .chars()
        .any(|c| matches!(c, '"' | '\\') || c.is_control());
    let could_be_in_a_number = text
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    encoded_as_typed && !could_be_in_a_number
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// The conditions of `filter` and their parameters.
fn filter_conditions(filter: &ActivityFilter) -> (Vec<String>, Vec<SqlValue>) {
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
        // `details` is JSON: search its decoded values, so quotes, line
        // breaks and backslashes match as typed and key names never match.
        // A row whose details are not valid JSON is skipped (`json_tree`
        // would fail the whole search on it); `CASE` fixes the order of the
        // two tests.
        let values = "CASE WHEN json_valid(core_activity.details) THEN EXISTS \
             (SELECT 1 FROM json_tree(core_activity.details) AS j \
             WHERE j.type IN ('text', 'integer', 'real') AND j.value LIKE ? ESCAPE '\\') \
             ELSE 0 END";
        let prefilter = raw_details_hold(text);
        clauses.push(if prefilter {
            format!(
                "(action LIKE ? ESCAPE '\\' OR target LIKE ? ESCAPE '\\' \
                 OR (core_activity.details LIKE ? ESCAPE '\\' AND {values}))"
            )
        } else {
            format!("(action LIKE ? ESCAPE '\\' OR target LIKE ? ESCAPE '\\' OR {values})")
        });
        let count = if prefilter { 4 } else { 3 };
        params.extend(std::iter::repeat_n(SqlValue::Text(pattern), count));
    }
    (clauses, params)
}

fn join_where(clauses: &[String]) -> String {
    if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    }
}

/// The `WHERE` clause of `filter` and its parameters.
pub(crate) fn where_clause(filter: &ActivityFilter) -> (String, Vec<SqlValue>) {
    let (clauses, params) = filter_conditions(filter);
    (join_where(&clauses), params)
}

/// Adds "older than `position`" (newest first) or "newer than it" (oldest
/// first). The first conjunct is a range on the time indexes; the second
/// breaks ties within one millisecond by id.
fn push_time_bound(
    clauses: &mut Vec<String>,
    params: &mut Vec<SqlValue>,
    position: &Position,
    direction: SortDirection,
) {
    clauses.push(
        match direction {
            SortDirection::Descending => "occurred_at <= ? AND (occurred_at < ? OR id < ?)",
            SortDirection::Ascending => "occurred_at >= ? AND (occurred_at > ? OR id > ?)",
        }
        .into(),
    );
    params.push(SqlValue::Integer(position.occurred_at));
    params.push(SqlValue::Integer(position.occurred_at));
    params.push(SqlValue::Text(position.id.clone()));
}

/// SQL and parameters of one page in the time order, after `after`.
pub(crate) fn page_sql(
    filter: &ActivityFilter,
    direction: SortDirection,
    after: Option<&Position>,
    limit: u32,
) -> (String, Vec<SqlValue>) {
    let (mut clauses, mut params) = filter_conditions(filter);
    if let Some(position) = after {
        push_time_bound(&mut clauses, &mut params, position, direction);
    }
    params.push(SqlValue::Integer(i64::from(limit)));
    let order = match direction {
        SortDirection::Descending => "occurred_at DESC, id DESC",
        SortDirection::Ascending => "occurred_at ASC, id ASC",
    };
    (
        format!(
            "SELECT {COLUMNS} FROM core_activity{} ORDER BY {order} LIMIT ?",
            join_where(&clauses)
        ),
        params,
    )
}

/// The rows of one sort value that one index range holds: `action = ?`,
/// `connection_id IS NULL`. A group is one lane, or several when equal
/// connection names belong to several profiles (or deleted connections).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Lane {
    clause: &'static str,
    value: Option<SqlValue>,
}

impl Lane {
    pub(crate) fn equal(clause: &'static str, value: SqlValue) -> Self {
        Lane {
            clause,
            value: Some(value),
        }
    }

    pub(crate) fn null(clause: &'static str) -> Self {
        Lane {
            clause,
            value: None,
        }
    }
}

/// SQL and parameters of up to `limit` rows of `lane` that match `filter`,
/// newest first, older than `within` (a position in the same group).
pub(crate) fn lane_sql(
    filter: &ActivityFilter,
    lane: &Lane,
    within: Option<&Position>,
    limit: u32,
) -> (String, Vec<SqlValue>) {
    let (mut clauses, mut params) = filter_conditions(filter);
    clauses.push(lane.clause.into());
    params.extend(lane.value.clone());
    if let Some(position) = within {
        push_time_bound(
            &mut clauses,
            &mut params,
            position,
            SortDirection::Descending,
        );
    }
    params.push(SqlValue::Integer(i64::from(limit)));
    (
        format!(
            "SELECT {COLUMNS} FROM core_activity{} \
             ORDER BY occurred_at DESC, id DESC LIMIT ?",
            join_where(&clauses)
        ),
        params,
    )
}

/// SQL and parameters of the value of `column` that follows `after` in
/// `direction` (the first value without `after`), read from the column's
/// index alone, without the filter: one seek. The filter is applied when
/// the value's rows are read; a value without matching rows is passed
/// over. `None` for the columns whose values are not sought this way.
pub(crate) fn seek_sql(
    column: SortColumn,
    direction: SortDirection,
    after: Option<&Group>,
) -> Option<(String, Vec<SqlValue>)> {
    let name = match column {
        SortColumn::Action => "action",
        SortColumn::Duration => "duration_ms",
        SortColumn::Time | SortColumn::Outcome | SortColumn::Connection => return None,
    };
    let (compare, way) = match direction {
        SortDirection::Ascending => (">", "ASC"),
        SortDirection::Descending => ("<", "DESC"),
    };
    let (clause, params) = match after {
        None => (format!("{name} IS NOT NULL"), Vec::new()),
        Some(Group::Text(text)) => (
            format!("{name} {compare} ?"),
            vec![SqlValue::Text(text.clone())],
        ),
        Some(Group::Int(value)) => (
            format!("{name} {compare} ?"),
            vec![SqlValue::Integer(*value)],
        ),
        Some(Group::Deleted | Group::Missing) => return None,
    };
    Some((
        format!("SELECT {name} FROM core_activity WHERE {clause} ORDER BY {name} {way} LIMIT 1"),
        params,
    ))
}

/// The outcomes in ascending order: problems first.
const OUTCOME_ORDER: [Outcome; 3] = [Outcome::Failure, Outcome::Cancelled, Outcome::Success];

/// Compares connection names the way the connection order does: without
/// regard to case (Unicode lowercase), and runs of ASCII digits by their
/// numeric value, so `db2` comes before `db10` and leading zeros do not
/// count. Accents are not folded (`é` sorts after `z`), and other
/// characters compare by code point. Names that compare equal sort as one
/// value, newest first.
#[must_use]
pub fn compare_names(a: &str, b: &str) -> Ordering {
    let mut a = a.chars().flat_map(char::to_lowercase).peekable();
    let mut b = b.chars().flat_map(char::to_lowercase).peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let left = take_number(&mut a);
                let right = take_number(&mut b);
                let ordering = left.len().cmp(&right.len()).then_with(|| left.cmp(&right));
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                a.next();
                b.next();
            }
        }
    }
}

/// The run of ASCII digits at the front of `chars`, without leading zeros.
fn take_number(chars: &mut std::iter::Peekable<impl Iterator<Item = char>>) -> String {
    let mut digits = String::new();
    while let Some(c) = chars.next_if(char::is_ascii_digit) {
        if !(digits.is_empty() && c == '0') {
            digits.push(c);
        }
    }
    digits
}

/// Walks the values of an order other than time, in order.
struct Walk<'a> {
    conn: &'a Connection,
    filter: &'a ActivityFilter,
    names: &'a ConnectionNames,
    /// Named connections in ascending order: one name of each run of equal
    /// names, with the ids of every profile in the run.
    named: Option<Vec<(String, Vec<String>)>>,
    /// Connection ids in the log without a name.
    deleted: Option<Vec<String>>,
}

impl<'a> Walk<'a> {
    fn new(conn: &'a Connection, filter: &'a ActivityFilter, names: &'a ConnectionNames) -> Self {
        Walk {
            conn,
            filter,
            names,
            named: None,
            deleted: None,
        }
    }

    fn named(&mut self) -> &[(String, Vec<String>)] {
        let names = self.names;
        self.named.get_or_insert_with(|| {
            let mut entries: Vec<(&String, &String)> = names.iter().collect();
            entries.sort_by(|(a_id, a), (b_id, b)| {
                compare_names(a, b)
                    .then_with(|| a.cmp(b))
                    .then_with(|| a_id.cmp(b_id))
            });
            let mut runs: Vec<(String, Vec<String>)> = Vec::new();
            for (id, name) in entries {
                match runs.last_mut() {
                    Some((first, ids)) if compare_names(first, name) == Ordering::Equal => {
                        ids.push(id.clone());
                    }
                    _ => runs.push((name.clone(), vec![id.clone()])),
                }
            }
            runs
        })
    }

    /// Every connection id in the log that has no name, found with one
    /// index seek per distinct id (`core_activity_conn_time`).
    fn deleted(&mut self) -> Result<&[String], StoreError> {
        if self.deleted.is_none() {
            let mut found = Vec::new();
            let mut last: Option<String> = None;
            loop {
                let next: Option<String> = match &last {
                    None => self
                        .conn
                        .prepare_cached(
                            "SELECT connection_id FROM core_activity \
                             WHERE connection_id IS NOT NULL ORDER BY connection_id LIMIT 1",
                        )?
                        .query_row([], |row| row.get(0))
                        .optional()?,
                    Some(previous) => self
                        .conn
                        .prepare_cached(
                            "SELECT connection_id FROM core_activity \
                             WHERE connection_id > ? ORDER BY connection_id LIMIT 1",
                        )?
                        .query_row([previous], |row| row.get(0))
                        .optional()?,
                };
                let Some(id) = next else { break };
                if !self.names.contains_key(&id) {
                    found.push(id.clone());
                }
                last = Some(id);
            }
            self.deleted = Some(found);
        }
        Ok(self.deleted.as_deref().unwrap_or_default())
    }

    /// The value after `after` (the first without it); `None` past the end.
    fn next_group(&mut self, after: Option<&Group>) -> Result<Option<Group>, StoreError> {
        let ActivityOrder { column, direction } = self.filter.order;
        match column {
            SortColumn::Time => Ok(None),
            SortColumn::Outcome => {
                let mut ranked: Vec<&str> = OUTCOME_ORDER.iter().map(|o| o.as_str()).collect();
                if direction == SortDirection::Descending {
                    ranked.reverse();
                }
                let at = match after {
                    None => 0,
                    Some(Group::Text(outcome)) => ranked
                        .iter()
                        .position(|o| o == outcome)
                        .map_or(ranked.len(), |i| i + 1),
                    Some(_) => ranked.len(),
                };
                Ok(ranked.get(at).map(|o| Group::Text((*o).to_string())))
            }
            SortColumn::Action | SortColumn::Duration => {
                if matches!(after, Some(Group::Missing)) {
                    return Ok(None);
                }
                let Some((sql, params)) = seek_sql(column, direction, after) else {
                    return Ok(None);
                };
                let value: Option<SqlValue> = self
                    .conn
                    .prepare_cached(&sql)?
                    .query_row(params_from_iter(params), |row| row.get(0))
                    .optional()?;
                Ok(match value {
                    Some(SqlValue::Text(text)) => Some(Group::Text(text)),
                    Some(SqlValue::Integer(value)) => Some(Group::Int(value)),
                    // Past the last duration: the events without one.
                    _ if column == SortColumn::Duration => Some(Group::Missing),
                    _ => None,
                })
            }
            SortColumn::Connection => match after {
                None | Some(Group::Text(_)) => {
                    let current = match after {
                        Some(Group::Text(name)) => Some(name.clone()),
                        _ => None,
                    };
                    let named = self.named();
                    let follows = |name: &String| {
                        current.as_ref().is_none_or(|current| {
                            direction.apply(compare_names(name, current)) == Ordering::Greater
                        })
                    };
                    let next = match direction {
                        SortDirection::Ascending => named.iter().find(|(n, _)| follows(n)),
                        SortDirection::Descending => named.iter().rev().find(|(n, _)| follows(n)),
                    };
                    if let Some((name, _)) = next {
                        return Ok(Some(Group::Text(name.clone())));
                    }
                    Ok(Some(if self.deleted()?.is_empty() {
                        Group::Missing
                    } else {
                        Group::Deleted
                    }))
                }
                Some(Group::Deleted | Group::Int(_)) => Ok(Some(Group::Missing)),
                Some(Group::Missing) => Ok(None),
            },
        }
    }

    fn lanes(&mut self, group: &Group) -> Result<Vec<Lane>, StoreError> {
        let column = self.filter.order.column;
        Ok(match (column, group) {
            (SortColumn::Action, Group::Text(action)) => {
                vec![Lane::equal("action = ?", SqlValue::Text(action.clone()))]
            }
            (SortColumn::Outcome, Group::Text(outcome)) => {
                vec![Lane::equal("outcome = ?", SqlValue::Text(outcome.clone()))]
            }
            (SortColumn::Duration, Group::Int(ms)) => {
                vec![Lane::equal("duration_ms = ?", SqlValue::Integer(*ms))]
            }
            (SortColumn::Duration, Group::Missing) => vec![Lane::null("duration_ms IS NULL")],
            (SortColumn::Connection, Group::Text(name)) => self
                .named()
                .iter()
                .filter(|(first, _)| compare_names(first, name) == Ordering::Equal)
                .flat_map(|(_, ids)| ids)
                .map(|id| Lane::equal("connection_id = ?", SqlValue::Text(id.clone())))
                .collect(),
            (SortColumn::Connection, Group::Deleted) => self
                .deleted()?
                .iter()
                .map(|id| Lane::equal("connection_id = ?", SqlValue::Text(id.clone())))
                .collect(),
            (SortColumn::Connection, Group::Missing) => vec![Lane::null("connection_id IS NULL")],
            _ => Vec::new(),
        })
    }

    /// Up to `limit` rows of `group` that match the filter, newest first,
    /// older than `within`. The lanes are merged.
    fn rows(
        &mut self,
        group: &Group,
        within: Option<&Position>,
        limit: u32,
    ) -> Result<Vec<RawRow>, StoreError> {
        let lanes = self.lanes(group)?;
        let mut rows = Vec::new();
        for lane in &lanes {
            let (sql, params) = lane_sql(self.filter, lane, within, limit);
            let mut stmt = self.conn.prepare_cached(&sql)?;
            for row in stmt.query_map(params_from_iter(params), RawRow::read)? {
                rows.push(row?);
            }
        }
        if lanes.len() > 1 {
            rows.sort_by(|a, b| {
                b.occurred_at
                    .cmp(&a.occurred_at)
                    .then_with(|| b.id.cmp(&a.id))
            });
            rows.truncate(limit as usize);
        }
        Ok(rows)
    }
}

/// Up to `limit` rows matching `filter` in its order, after `after`, each
/// with its sort value (`None` in the time order). `names` are only read
/// in the connection order.
pub(crate) fn fetch_page(
    conn: &Connection,
    filter: &ActivityFilter,
    names: &ConnectionNames,
    after: Option<&Position>,
    limit: u32,
) -> Result<Vec<(RawRow, Option<Group>)>, StoreError> {
    let order = filter.order;
    if order.column == SortColumn::Time {
        let (sql, params) = page_sql(filter, order.direction, after, limit);
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(params), RawRow::read)?
            .map(|row| row.map(|row| (row, None)))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(rows);
    }
    let mut walk = Walk::new(conn, filter, names);
    let mut out: Vec<(RawRow, Option<Group>)> = Vec::new();
    let mut group: Option<Group> = None;
    if let Some(position) = after {
        // `resolve_cursor` gives every position in these orders a value.
        let Some(current) = &position.group else {
            return Ok(out);
        };
        for row in walk.rows(current, Some(position), limit)? {
            out.push((row, Some(current.clone())));
        }
        group = Some(current.clone());
    }
    while out.len() < limit as usize {
        let Some(next) = walk.next_group(group.as_ref())? else {
            break;
        };
        let need = limit - u32::try_from(out.len()).unwrap_or(limit);
        for row in walk.rows(&next, None, need)? {
            out.push((row, Some(next.clone())));
        }
        group = Some(next);
    }
    Ok(out)
}

/// The cursor of the position right after the event at `occurred_at` with
/// `id` and sort value `group`, in `order`.
pub(crate) fn format_cursor(
    order: ActivityOrder,
    occurred_at: i64,
    id: &str,
    group: Option<&Group>,
) -> String {
    if order.is_default() {
        // The form cursors had before there were other orders.
        return format!("{occurred_at}:{id}");
    }
    let mut cursor = format!("{}:{occurred_at}:{id}", order.tag());
    if let Some(group) = group {
        cursor.push(':');
        cursor.push_str(&group.encode());
    }
    cursor
}

fn parse_id(text: &str) -> Option<String> {
    Uuid::from_str(text).ok().map(|id| id.to_string())
}

/// The position a `before_id` stands for in `order`: a cursor as is, a
/// bare event id (newest first only) through the stored event.
pub(crate) fn resolve_cursor(
    conn: &Connection,
    token: &str,
    order: ActivityOrder,
) -> Result<Resolved, StoreError> {
    let Some((head, rest)) = token.split_once(':') else {
        // A bare event id, the oldest form: newest first only.
        if !order.is_default() {
            return Ok(Resolved::OtherOrder(ActivityOrder::default()));
        }
        return Ok(match cursor_of(conn, token)? {
            Some(position) => Resolved::At(position),
            None => Resolved::Unknown,
        });
    };
    if let Ok(occurred_at) = head.parse::<i64>() {
        // `<occurred_at>:<id>`: newest first.
        let Some(id) = parse_id(rest) else {
            return Ok(Resolved::Unknown);
        };
        if !order.is_default() {
            return Ok(Resolved::OtherOrder(ActivityOrder::default()));
        }
        return Ok(Resolved::At(Position {
            occurred_at,
            id,
            group: None,
        }));
    }
    let Some(cursor_order) = ActivityOrder::from_tag(head) else {
        return Ok(Resolved::Unknown);
    };
    if cursor_order != order {
        return Ok(Resolved::OtherOrder(cursor_order));
    }
    let mut parts = rest.splitn(3, ':');
    let occurred_at = parts.next().and_then(|t| t.parse::<i64>().ok());
    let id = parts.next().and_then(parse_id);
    let (Some(occurred_at), Some(id)) = (occurred_at, id) else {
        return Ok(Resolved::Unknown);
    };
    let group = match (order.column, parts.next()) {
        (SortColumn::Time, None) => None,
        (SortColumn::Time, Some(_)) | (_, None) => return Ok(Resolved::Unknown),
        (column, Some(text)) => match Group::decode(text, column) {
            Some(group) => Some(group),
            None => return Ok(Resolved::Unknown),
        },
    };
    Ok(Resolved::At(Position {
        occurred_at,
        id,
        group,
    }))
}

/// Looks up the position of the event `id` in the newest-first order.
fn cursor_of(conn: &Connection, id: &str) -> Result<Option<Position>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT occurred_at FROM core_activity WHERE id = ?")?;
    let mut rows = stmt.query([id])?;
    match rows.next()? {
        Some(row) => Ok(Some(Position {
            occurred_at: row.get(0)?,
            id: id.to_string(),
            group: None,
        })),
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
    let (where_sql, params) = where_clause(filter);
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
    fn the_stored_json_is_searched_first_only_when_it_holds_the_text_as_typed() {
        for text in [
            "orders",
            "it's 100%_done",
            "caf\u{e9} \u{1f600}",
            "v1.2",
            "e-mail",
        ] {
            assert!(raw_details_hold(text), "{text:?}");
        }
        // Encoded differently in JSON.
        for text in ["say \"hi\"", "a\\b", "tab\there", "line\nbreak", "\u{7f}"] {
            assert!(!raw_details_hold(text), "{text:?}");
        }
        // Could be part of a number, which SQLite prints its own way.
        for text in ["4242", "0.3", "e+16", "-1", "1E5"] {
            assert!(!raw_details_hold(text), "{text:?}");
        }
        let sql_for = |text: &str| {
            let filter = ActivityFilter {
                text: Some(text.into()),
                ..Default::default()
            };
            where_clause(&filter)
        };
        let (sql, params) = sql_for("orders_1");
        assert!(
            sql.contains("core_activity.details LIKE ? ESCAPE '\\' AND CASE WHEN json_valid("),
            "{sql}"
        );
        // Wildcards in the text are escaped in every pattern.
        let patterns = params
            .iter()
            .filter(|p| matches!(p, SqlValue::Text(t) if t == "%orders\\_1%"))
            .count();
        assert_eq!(patterns, 4);
        let (sql, params) = sql_for("a\\b");
        assert!(!sql.contains("core_activity.details LIKE"), "{sql}");
        assert!(sql.contains("CASE WHEN json_valid("), "{sql}");
        let patterns = params
            .iter()
            .filter(|p| matches!(p, SqlValue::Text(t) if t == "%a\\\\b%"))
            .count();
        assert_eq!(patterns, 3);
    }

    #[test]
    fn connection_names_compare_without_case_and_numbers_by_value() {
        use Ordering::{Equal, Greater, Less};
        for (a, b, expected) in [
            ("Orders", "orders", Equal),
            ("ORDERS", "orders", Equal),
            ("db2", "db10", Less),
            ("db10", "db2", Greater),
            ("db02", "db2", Equal),
            ("db0", "db", Greater),
            ("a", "B", Less),
            ("Zeta", "alpha", Greater),
            ("prod", "prod 2", Less),
            ("v1.10", "v1.9", Greater),
            ("", "a", Less),
            // Accents are not folded: code point order after lowercase.
            ("\u{e9}cole", "zoo", Greater),
            ("\u{c9}COLE", "\u{e9}cole", Equal),
        ] {
            assert_eq!(compare_names(a, b), expected, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn cursors_round_trip_in_every_order() {
        let id = "0190f0a0-0000-7000-8000-000000000000".to_string();
        let cases = [
            (SortColumn::Time, None),
            (
                SortColumn::Action,
                Some(Group::Text("query.executed".into())),
            ),
            (SortColumn::Outcome, Some(Group::Text("failure".into()))),
            (SortColumn::Duration, Some(Group::Int(250))),
            (SortColumn::Duration, Some(Group::Missing)),
            // A name may hold the separator.
            (
                SortColumn::Connection,
                Some(Group::Text("prod: eu=1 #2".into())),
            ),
            (SortColumn::Connection, Some(Group::Deleted)),
            (SortColumn::Connection, Some(Group::Missing)),
        ];
        let Ok(conn) = Connection::open_in_memory() else {
            panic!("no in-memory database");
        };
        for (column, group) in cases {
            for direction in [SortDirection::Ascending, SortDirection::Descending] {
                let order = ActivityOrder { column, direction };
                let cursor = format_cursor(order, -5, &id, group.as_ref());
                let expected = Position {
                    occurred_at: -5,
                    id: id.clone(),
                    group: group.clone(),
                };
                match resolve_cursor(&conn, &cursor, order) {
                    Ok(Resolved::At(position)) => assert_eq!(position, expected, "{cursor}"),
                    other => panic!("{cursor}: {other:?}"),
                }
            }
        }
        // Newest first keeps the form it always had.
        assert_eq!(
            format_cursor(ActivityOrder::default(), 12, &id, None),
            format!("12:{id}")
        );
        assert_eq!(
            format_cursor(
                ActivityOrder {
                    column: SortColumn::Duration,
                    direction: SortDirection::Descending
                },
                12,
                &id,
                Some(&Group::Int(250))
            ),
            format!("duration.desc:12:{id}:#250")
        );
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
