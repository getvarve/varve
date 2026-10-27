//! Getting the key browser past entries too large to read.
//!
//! An entry whose reply alone is over [`MAX_REPLY_BYTES`] cannot be read.
//! Instead of stopping the page there, the browser shows a placeholder
//! ([`placeholder`]) with the entry's size when it can be measured, and
//! moves on:
//!
//! - lists and sorted sets are paged by rank, so the next page simply
//!   starts one further; the element's size (and a sorted set member's
//!   score) is measured on a side connection that reads the reply as a
//!   stream and drops the element ([`crate::measure`]);
//! - a hash `HSCAN` step too large for one reply is read once without its
//!   values: `HSCAN ... NOVALUES` (Redis 7.4, Valkey 8), otherwise as a
//!   stream that keeps the field names and drops the values. The names
//!   are kept in the session ([`crate::pagestate`]) and the fields are read
//!   by name, with `HSTRLEN` first and `HGET` when the value fits, while
//!   the page budget lasts; deleted fields are skipped;
//! - a set `SSCAN` step too large for one reply is read once as a stream
//!   that keeps members up to [`MAX_REPLY_BYTES`] in total and drops the
//!   rest, keeping their sizes (a set member cannot be read on its own, so
//!   those are shown with the size they had at the time of the scan);
//! - a stream entry is measured as a stream too, keeping its id;
//! - a `SCAN` step whose key names are too large is read as a stream too:
//!   names that fit are kept, the others become placeholder keys of
//!   unknown type.
//!
//! No script runs for any of this, so it works the same in read-only
//! sessions and for users without script permissions. All measurements of
//! one page share one side connection. `HSCAN ... NOVALUES` refused for
//! lack of permission (`NOPERM`) or not known falls back to the stream; a
//! busy server (`BUSY`) and any other error are reported as errors.

use redis::Value as Resp;
use varve_driver::{DriverError, ErrorKind, Value};

use crate::browser::{
    FieldValue, KeyEntry, KeyType, ListItem, PageBudget, ScoredMember, StreamEntry, query_error,
};
use crate::conn::ServerFlavor;
use crate::measure;
use crate::pagestate::Pending;
use crate::parse::key_display;
use crate::reply::{ValueBudget, as_int, as_text, bytes_value, double_value, to_value};
use crate::session::{Call, MAX_REPLY_BYTES};

/// Text every placeholder starts with. In a value page a [`Value::Other`]
/// in a member, field or value position is always a placeholder: real
/// data comes as text or bytes. A key too large to show has an empty
/// [`KeyEntry::name`] and a placeholder as [`KeyEntry::display`].
pub const OVERSIZED_MARK: &str = "(too large to show";

/// Room left in a reply for the protocol around the values.
const REPLY_SLACK: u64 = 64 * 1024;

/// Most bytes of values one probe reply may carry.
const HARD_LIMIT: u64 = MAX_REPLY_BYTES - REPLY_SLACK;

/// The placeholder text for something of `bytes` bytes (`None`: only
/// known to be over [`MAX_REPLY_BYTES`]).
fn placeholder_text(bytes: Option<u64>) -> String {
    match bytes {
        Some(bytes) => format!("{OVERSIZED_MARK}: {bytes} bytes)"),
        None => format!(
            "{OVERSIZED_MARK}: over {} MiB)",
            MAX_REPLY_BYTES / (1024 * 1024)
        ),
    }
}

/// The placeholder value for an entry part of `bytes` bytes.
pub fn placeholder(bytes: Option<u64>) -> Value {
    Value::Other(placeholder_text(bytes))
}

/// True when a single value of `bytes` bytes can be read in one reply.
fn fits(bytes: u64) -> bool {
    bytes <= HARD_LIMIT
}

/// `HSCAN ... NOVALUES` exists from Redis 7.4 and Valkey 8.
fn has_novalues(call: &Call<'_>) -> bool {
    let details = call.details();
    match details.flavor {
        ServerFlavor::Redis => details.compat >= (7, 4),
        ServerFlavor::Valkey => details.info.major >= 8,
    }
}

/// The error for a probe the server could not run because it is busy.
fn busy(e: &redis::ServerError) -> DriverError {
    let mut error = DriverError::new(
        ErrorKind::Query,
        "The server is busy running a script or function, so Varve could not read past an \
         entry that is too large to show. Try again when it has finished.",
    )
    .with_code(e.code());
    error.detail = e.details().map(str::to_string);
    error
}

/// What a refused probe means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// Not permitted or not supported here: fall back or report what is
    /// missing.
    Unavailable,
    /// The server runs a long script or function: a transient error.
    Busy,
    /// Anything else (a killed or failing script, out of memory, ...):
    /// reported as the server's error.
    Failed,
}

fn classify(e: &redis::ServerError) -> Refusal {
    match e.code() {
        "NOPERM" => Refusal::Unavailable,
        "BUSY" => Refusal::Busy,
        "ERR"
            if e.details().is_some_and(|d| {
                let d = d.to_ascii_lowercase();
                d.starts_with("unknown command") || d.starts_with("unknown subcommand")
            }) =>
        {
            Refusal::Unavailable
        }
        _ => Refusal::Failed,
    }
}

/// The reply of a probe, or `None` when it is unavailable here (refused
/// for lack of permission, not supported, or its reply is itself too large).
async fn probe(call: &mut Call<'_>, cmd: &redis::Cmd) -> Result<Option<Resp>, DriverError> {
    match call.send(cmd).await {
        Ok(Resp::ServerError(e)) => match classify(&e) {
            Refusal::Unavailable => {
                tracing::debug!(code = e.code(), "oversized-entry probe not available");
                Ok(None)
            }
            Refusal::Busy => Err(busy(&e)),
            Refusal::Failed => Err(query_error(&e)),
        },
        Ok(reply) => Ok(Some(reply)),
        Err(_) if call.reply_was_too_large() => {
            call.reconnect().await?;
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Reads one value that is known to fit; a placeholder when it turns out
/// too large after all (it grew meanwhile). Returns the bytes read.
async fn read_one(
    call: &mut Call<'_>,
    cmd: &redis::Cmd,
    bytes: u64,
    values: &mut ValueBudget,
) -> Result<(Value, u64), DriverError> {
    match call.send(cmd).await {
        Ok(Resp::ServerError(e)) => Err(query_error(&e)),
        Ok(reply) => {
            let read = crate::browser::reply_bytes(&reply);
            Ok((to_value(reply, values)?, read))
        }
        Err(_) if call.reply_was_too_large() => {
            call.reconnect().await?;
            Ok((placeholder(Some(bytes)), 0))
        }
        Err(e) => Err(e),
    }
}

fn integer(reply: Option<&Resp>) -> Option<u64> {
    reply.and_then(as_int).and_then(|n| u64::try_from(n).ok())
}

fn bytes_of(reply: Option<Resp>) -> Option<Vec<u8>> {
    match reply? {
        Resp::BulkString(bytes) => Some(bytes),
        Resp::SimpleString(text) => Some(text.into_bytes()),
        _ => None,
    }
}

fn bad_probe(what: &str) -> DriverError {
    crate::error::protocol(format!("unexpected {what} probe reply"))
}

/// `[cursor, [entry, ...]]` of a step probe.
fn step_reply(reply: Resp) -> Option<(u64, Vec<Resp>)> {
    let Resp::Array(mut parts) = reply else {
        return None;
    };
    if parts.len() != 2 {
        return None;
    }
    let entries = parts.pop()?;
    let cursor = as_text(&parts.pop()?)?.parse().ok()?;
    match entries {
        Resp::Array(items) | Resp::Set(items) => Some((cursor, items)),
        _ => None,
    }
}

/// The field names of the `HSCAN` step at `cursor` from
/// `HSCAN ... NOVALUES`; `None` when that is not available or its reply is
/// too large.
async fn hash_names_novalues(
    call: &mut Call<'_>,
    key: &[u8],
    cursor: u64,
) -> Result<Option<(u64, Vec<Pending>)>, DriverError> {
    let mut scan = redis::cmd("HSCAN");
    scan.arg(key)
        .arg(cursor)
        .arg("COUNT")
        .arg(1)
        .arg("NOVALUES");
    let Some(reply) = probe(call, &scan).await? else {
        return Ok(None);
    };
    let (next, fields) = step_reply(reply).ok_or_else(|| bad_probe("HSCAN NOVALUES"))?;
    Ok(Some((
        next,
        fields
            .into_iter()
            .filter_map(|f| bytes_of(Some(f)))
            .map(Pending::Field)
            .collect(),
    )))
}

/// Reads the field names of the `HSCAN` step at `cursor`, which is too
/// large to read with its values: `HSCAN ... NOVALUES` where it exists,
/// otherwise the step is read as a stream on a side connection that keeps
/// the names and drops the values ([`measure::hash_step`]). The step is
/// read once; its fields are then read by name (see [`serve_pending`]), so
/// a changed step on the server cannot lose or repeat fields. Returns the
/// real next cursor.
///
/// # Errors
///
/// Server and connection errors.
pub(crate) async fn capture_hash_step(
    call: &mut Call<'_>,
    key: &[u8],
    cursor: u64,
) -> Result<(u64, Vec<Pending>), DriverError> {
    if has_novalues(call)
        && let Some(step) = hash_names_novalues(call, key, cursor).await?
    {
        return Ok(step);
    }
    measure::hash_step(call, key, cursor).await
}

/// Reads the `SSCAN` step at `cursor`, which is too large for one reply, as
/// a stream on a side connection ([`measure::set_step`]): members up to
/// [`MAX_REPLY_BYTES`] in total are kept, the others are dropped while
/// reading and only their sizes kept (shown as
/// [`placeholder_at_scan`]: a set member cannot be read on its own).
/// Returns the real next cursor.
///
/// # Errors
///
/// Server and connection errors.
pub(crate) async fn capture_set_step(
    call: &mut Call<'_>,
    key: &[u8],
    cursor: u64,
) -> Result<(u64, Vec<Pending>), DriverError> {
    measure::set_step(call, key, cursor).await
}

/// The placeholder for a set member of `bytes` bytes, measured when its
/// scan step was read.
pub fn placeholder_at_scan(bytes: u64) -> Value {
    Value::Other(format!(
        "{OVERSIZED_MARK}: {bytes} bytes at the time of the scan)"
    ))
}

/// One shown entry of a hash or set page.
pub(crate) enum Served {
    /// A hash field and value.
    Field(FieldValue),
    /// A set member.
    Member(Value),
}

/// Shows entries of a kept scan step while the page has room (at least
/// one entry when any is left). Hash fields kept by name are read now and
/// skipped when they were deleted meanwhile; when every remaining entry
/// was deleted nothing is shown. Returns how many entries of `pending`
/// were used and the shown entries.
///
/// # Errors
///
/// Value budget, connection errors.
pub(crate) async fn serve_pending(
    call: &mut Call<'_>,
    key: &[u8],
    pending: &[Pending],
    page: &mut PageBudget,
    values: &mut ValueBudget,
) -> Result<(usize, Vec<Served>), DriverError> {
    let mut out = Vec::new();
    for (used, entry) in pending.iter().enumerate() {
        if !out.is_empty() && page.is_full() {
            return Ok((used, out));
        }
        match entry {
            Pending::Pair(field, value) => {
                page.record(1, (field.len() + value.len()) as u64);
                out.push(Served::Field(FieldValue {
                    field: to_value(Resp::BulkString(field.clone()), values)?,
                    value: to_value(Resp::BulkString(value.clone()), values)?,
                }));
            }
            Pending::Field(field) => {
                let mut strlen = redis::cmd("HSTRLEN");
                strlen.arg(key).arg(field.as_slice());
                let size = match call.send(&strlen).await? {
                    Resp::ServerError(e) => return Err(query_error(&e)),
                    reply => integer(Some(&reply)).unwrap_or_default(),
                };
                let value = if fits(size) {
                    let mut get = redis::cmd("HGET");
                    get.arg(key).arg(field.as_slice());
                    let (value, read) = read_one(call, &get, size, values).await?;
                    if value == Value::Null {
                        // Deleted since the step was read.
                        continue;
                    }
                    page.record(1, read + field.len() as u64);
                    value
                } else {
                    let mut exists = redis::cmd("HEXISTS");
                    exists.arg(key).arg(field.as_slice());
                    if integer(Some(&call.query(&exists).await?)) == Some(0) {
                        continue;
                    }
                    page.record(1, field.len() as u64);
                    placeholder(Some(size))
                };
                out.push(Served::Field(FieldValue {
                    field: bytes_value(field.clone()),
                    value,
                }));
            }
            Pending::OversizedField {
                field_bytes,
                value_bytes,
            } => {
                page.record(1, 0);
                out.push(Served::Field(FieldValue {
                    field: placeholder(Some(*field_bytes)),
                    value: placeholder(*value_bytes),
                }));
            }
            Pending::Member(member) => {
                page.record(1, member.len() as u64);
                out.push(Served::Member(to_value(
                    Resp::BulkString(member.clone()),
                    values,
                )?));
            }
            Pending::OversizedMember(size) => {
                page.record(1, 0);
                out.push(Served::Member(placeholder_at_scan(*size)));
            }
        }
    }
    Ok((pending.len(), out))
}

/// A placeholder for the list element at `index` with its size, measured
/// without reading it ([`measure::list_element`]); `None` when the element
/// is gone.
///
/// # Errors
///
/// Server and connection errors.
pub(crate) async fn list_item(
    call: &mut Call<'_>,
    key: &[u8],
    index: u64,
) -> Result<Option<ListItem>, DriverError> {
    Ok(measure::list_element(call, key, index)
        .await?
        .map(|size| ListItem {
            index,
            value: placeholder(Some(size)),
        }))
}

/// A placeholder for the sorted set member at `rank`, with its size and
/// score, measured without reading it ([`measure::zset_member`]); `None`
/// when the member is gone.
///
/// # Errors
///
/// Server and connection errors.
pub(crate) async fn zset_member(
    call: &mut Call<'_>,
    key: &[u8],
    rank: u64,
) -> Result<Option<ScoredMember>, DriverError> {
    Ok(measure::zset_member(call, key, rank)
        .await?
        .map(|(size, score)| ScoredMember {
            member: placeholder(Some(size)),
            score: score.parse().map_or(Value::Null, double_value),
        }))
}

/// A placeholder for the first stream entry at or after `start`, with its
/// id (needed to page past it) and size, measured without reading it
/// ([`measure::stream_entry`]). `None` when there is no entry any more.
///
/// # Errors
///
/// Server and connection errors.
pub(crate) async fn stream_entry(
    call: &mut Call<'_>,
    key: &[u8],
    start: &str,
) -> Result<Option<StreamEntry>, DriverError> {
    Ok(measure::stream_entry(call, key, start)
        .await?
        .map(|(id, size)| StreamEntry {
            id,
            fields: vec![FieldValue {
                field: placeholder(Some(size)),
                value: Value::Null,
            }],
        }))
}

/// Reads the `SCAN` step at `cursor` with `COUNT count`, whose key names
/// are too large for one reply, as a stream on the page's side connection
/// ([`measure::scan_step`]): names that fit are kept and get their type and
/// time to live with `TYPE`/`PTTL`; the others become placeholder entries
/// with their size, the type filter of the scan (applied by the server) or
/// [`KeyType::Unknown`], and no time to live. Returns the next cursor and
/// the keys.
///
/// # Errors
///
/// Server and connection errors.
pub(crate) async fn scan_step(
    call: &mut Call<'_>,
    cursor: u64,
    pattern: &str,
    type_filter: Option<&KeyType>,
    count: u32,
) -> Result<(u64, Vec<KeyEntry>), DriverError> {
    // The whole step at the asked count, halving the count (on the same
    // side connection, nothing to reconnect) while the names do not fit.
    let mut count = count.max(1);
    let (next, names) = loop {
        let type_name = type_filter.map(KeyType::type_name);
        if let Some(step) = measure::scan_step(call, cursor, pattern, type_name, count).await? {
            break step;
        }
        count /= 2;
    };
    let mut pipe = redis::pipe();
    for name in names.iter().filter_map(|n| n.as_ref().ok()) {
        pipe.cmd("TYPE").arg(name.as_slice());
        pipe.cmd("PTTL").arg(name.as_slice());
    }
    let replies = call.send_pipeline(&pipe).await?;
    let mut replies = replies.chunks(2);
    let mut keys = Vec::with_capacity(names.len());
    for name in names {
        match name {
            Ok(name) => {
                let Some([type_reply, ttl_reply]) = replies.next() else {
                    return Err(crate::error::protocol("short pipeline reply"));
                };
                let Some(key_type) = as_text(type_reply).and_then(|t| KeyType::from_type_name(&t))
                else {
                    continue;
                };
                if type_filter.is_some_and(|f| *f != key_type) {
                    continue;
                }
                let ttl = as_int(ttl_reply);
                if ttl == Some(-2) {
                    continue;
                }
                keys.push(KeyEntry {
                    display: key_display(&name),
                    name,
                    key_type,
                    ttl_ms: ttl.filter(|t| *t >= 0),
                    placeholder: false,
                });
            }
            Err(size) => keys.push(KeyEntry {
                name: Vec::new(),
                display: placeholder_text(Some(size)),
                key_type: type_filter.cloned().unwrap_or(KeyType::Unknown),
                ttl_ms: None,
                placeholder: true,
            }),
        }
    }
    Ok((next, keys))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_error(line: &str) -> redis::ServerError {
        match redis::parse_redis_value(format!("-{line}\r\n").as_bytes()) {
            Ok(Resp::ServerError(error)) => error,
            other => panic!("not an error reply: {other:?}"),
        }
    }

    #[test]
    fn placeholders_name_the_size() {
        assert_eq!(
            placeholder(Some(17)),
            Value::Other("(too large to show: 17 bytes)".into())
        );
        assert_eq!(
            placeholder(None),
            Value::Other("(too large to show: over 16 MiB)".into())
        );
        assert!(fits(1024));
        assert!(!fits(MAX_REPLY_BYTES));
    }

    #[test]
    fn step_replies_parse() {
        let reply = Resp::Array(vec![
            Resp::BulkString(b"42".to_vec()),
            Resp::Array(vec![Resp::Int(1)]),
        ]);
        assert_eq!(step_reply(reply).map(|(c, e)| (c, e.len())), Some((42, 1)));
        assert!(step_reply(Resp::Nil).is_none());
    }

    #[test]
    fn probe_refusals_are_told_apart() {
        for (line, expected) in [
            (
                "NOPERM User u has no permissions to run the 'eval_ro' command",
                Refusal::Unavailable,
            ),
            (
                "ERR unknown command 'EVAL_RO', with args beginning with: ",
                Refusal::Unavailable,
            ),
            (
                "BUSY Redis is busy running a script. You can only call SCRIPT KILL or SHUTDOWN NOSAVE.",
                Refusal::Busy,
            ),
            (
                "ERR Error running script (call to f_x): @user_script:1: Script killed by user with SCRIPT KILL...",
                Refusal::Failed,
            ),
            (
                "OOM command not allowed when used memory > 'maxmemory'.",
                Refusal::Failed,
            ),
            ("ERR syntax error", Refusal::Failed),
        ] {
            assert_eq!(classify(&server_error(line)), expected, "{line}");
        }
        let error = busy(&server_error("BUSY Redis is busy running a script."));
        assert_eq!(error.code.as_deref(), Some("BUSY"));
        assert!(error.message.contains("Try again"), "{}", error.message);
        assert!(!error.message.contains("permission"));
    }
}
