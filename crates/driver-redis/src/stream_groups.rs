//! Stream consumer groups (design doc 4.2): `XINFO` and `XPENDING` reads,
//! and group changes planned like the value editors ([`crate::edit`]).
//!
//! Reads run on the key browser's side connection and read replies as a
//! stream: the first and last entries of `XINFO STREAM` are skipped
//! unread except for their ids, so an entry of any size is never held.

use redis::Value as Resp;
use serde::{Deserialize, Serialize};
use varve_driver::{DriverError, ErrorKind};

use crate::browser::{MAX_PAGE_COUNT, u64_text};
use crate::edit::{
    Check, ConflictReason, EditCounts, EditError, EditProgram, Expectation, RedisEditPlan,
    ScriptCheck, cmd, display_line, invalid, valid_stream_id,
};
use crate::measure::{self, Head, RawConn};
use crate::parse::{key_display, key_from_display};
use crate::risk::CommandRisk;
use crate::session::RedisSession;

/// Most bytes of one field value of an `XINFO` or `XPENDING` reply kept
/// (names, ids, numbers).
const FIELD_LIMIT: u64 = 1024 * 1024;

/// `XINFO STREAM`, without the entries' fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamInfo {
    /// Entries.
    #[serde(with = "u64_text")]
    pub length: u64,
    /// The id of the newest entry ever added.
    pub last_generated_id: String,
    /// Consumer groups.
    #[serde(with = "u64_text")]
    pub groups: u64,
    /// Id of the first entry.
    pub first_entry_id: Option<String>,
    /// Id of the last entry.
    pub last_entry_id: Option<String>,
    /// Highest deleted id (Redis 7.0 and newer).
    pub max_deleted_entry_id: Option<String>,
    /// Entries ever added (Redis 7.0 and newer), decimal text.
    pub entries_added: Option<String>,
    /// First id recorded (Redis 7.0 and newer).
    pub recorded_first_entry_id: Option<String>,
}

/// One consumer group (`XINFO GROUPS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamGroup {
    /// Name, display text.
    pub name: String,
    /// Consumers.
    #[serde(with = "u64_text")]
    pub consumers: u64,
    /// Pending entries.
    #[serde(with = "u64_text")]
    pub pending: u64,
    /// Last delivered id.
    pub last_delivered_id: String,
    /// Entries read (Redis 7.0 and newer), decimal text.
    pub entries_read: Option<String>,
    /// Entries not delivered yet (Redis 7.0 and newer, when known),
    /// decimal text.
    pub lag: Option<String>,
}

/// One consumer (`XINFO CONSUMERS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamConsumer {
    /// Name, display text.
    pub name: String,
    /// Pending entries.
    #[serde(with = "u64_text")]
    pub pending: u64,
    /// Milliseconds since its last attempted interaction.
    #[serde(with = "u64_text")]
    pub idle_ms: u64,
    /// Milliseconds since its last successful read (Redis 7.2 and newer;
    /// `None` before and for a consumer that never read), decimal text.
    pub inactive_ms: Option<String>,
}

/// One consumer's pending count in a [`PendingSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumerPending {
    /// Name, display text.
    pub name: String,
    /// Pending entries.
    #[serde(with = "u64_text")]
    pub pending: u64,
}

/// `XPENDING key group`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingSummary {
    /// Pending entries.
    #[serde(with = "u64_text")]
    pub count: u64,
    /// Lowest pending id.
    pub min_id: Option<String>,
    /// Highest pending id.
    pub max_id: Option<String>,
    /// Consumers with pending entries.
    pub consumers: Vec<ConsumerPending>,
}

/// Which pending entries to list.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingQuery {
    /// `None` for the beginning, else [`PendingPage::next_start`].
    #[serde(default)]
    pub start: Option<String>,
    /// Entries, clamped to 1..=[`MAX_PAGE_COUNT`].
    pub count: u32,
    /// Only this consumer's (display text).
    #[serde(default)]
    pub consumer: Option<String>,
    /// Only entries idle at least this long.
    #[serde(default, with = "crate::browser::opt_u64_text")]
    pub min_idle_ms: Option<u64>,
}

/// One pending entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingEntry {
    /// Entry id.
    pub id: String,
    /// Owner, display text.
    pub consumer: String,
    /// Milliseconds since it was last delivered.
    #[serde(with = "u64_text")]
    pub idle_ms: u64,
    /// Times delivered.
    #[serde(with = "u64_text")]
    pub deliveries: u64,
}

/// A page of pending entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPage {
    /// Entries in id order.
    pub entries: Vec<PendingEntry>,
    /// Start of the next page; `None` when this was the last.
    pub next_start: Option<String>,
}

/// A consumer group change, planned with
/// [`RedisSession::plan_stream_group`] and applied with
/// [`RedisSession::apply_edit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum StreamGroupChange {
    /// `XGROUP CREATE`.
    #[serde(rename_all = "camelCase")]
    GroupCreate {
        /// Group name, display text.
        group: String,
        /// `$`, `0` or an entry id.
        id: String,
        /// Create the stream when it does not exist.
        mk_stream: bool,
        /// `ENTRIESREAD` (Redis 7.0 and newer).
        #[serde(default, with = "crate::browser::opt_u64_text")]
        entries_read: Option<u64>,
    },
    /// `XGROUP DESTROY`.
    #[serde(rename_all = "camelCase")]
    GroupDestroy {
        /// Group name.
        group: String,
        /// Pending entries when it was read.
        #[serde(with = "u64_text")]
        expected_pending: u64,
    },
    /// `XGROUP SETID`.
    #[serde(rename_all = "camelCase")]
    GroupSetId {
        /// Group name.
        group: String,
        /// `$`, `0` or an entry id.
        id: String,
        /// `ENTRIESREAD` (Redis 7.0 and newer).
        #[serde(default, with = "crate::browser::opt_u64_text")]
        entries_read: Option<u64>,
    },
    /// `XGROUP DELCONSUMER`.
    #[serde(rename_all = "camelCase")]
    GroupDelConsumer {
        /// Group name.
        group: String,
        /// Consumer name.
        consumer: String,
        /// Its pending entries when it was read.
        #[serde(with = "u64_text")]
        expected_pending: u64,
    },
    /// `XACK`.
    Ack {
        /// Group name.
        group: String,
        /// Entry ids.
        ids: Vec<String>,
    },
    /// `XCLAIM`.
    #[serde(rename_all = "camelCase")]
    Claim {
        /// Group name.
        group: String,
        /// New owner.
        consumer: String,
        /// Only entries idle at least this long.
        #[serde(with = "u64_text")]
        min_idle_ms: u64,
        /// Entry ids.
        ids: Vec<String>,
        /// `JUSTID`: return ids only and leave the delivery counter alone.
        just_id: bool,
    },
    /// `XAUTOCLAIM`.
    #[serde(rename_all = "camelCase")]
    AutoClaim {
        /// Group name.
        group: String,
        /// New owner.
        consumer: String,
        /// Only entries idle at least this long.
        #[serde(with = "u64_text")]
        min_idle_ms: u64,
        /// Where to start (`0-0` for the beginning).
        start: String,
        /// Entries to claim at most.
        count: u32,
        /// `JUSTID`.
        just_id: bool,
    },
}

impl StreamGroupChange {
    /// The `op` name, as serialized.
    #[must_use]
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::GroupCreate { .. } => "groupCreate",
            Self::GroupDestroy { .. } => "groupDestroy",
            Self::GroupSetId { .. } => "groupSetId",
            Self::GroupDelConsumer { .. } => "groupDelConsumer",
            Self::Ack { .. } => "ack",
            Self::Claim { .. } => "claim",
            Self::AutoClaim { .. } => "autoClaim",
        }
    }
}

/// Text of a small reply value (a bulk string, a status or an integer).
fn resp_text(value: &Resp) -> Option<String> {
    match value {
        Resp::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Resp::SimpleString(text) => Some(text.clone()),
        Resp::Int(n) => Some(n.to_string()),
        _ => None,
    }
}

fn resp_name(value: &Resp) -> Option<String> {
    match value {
        Resp::BulkString(bytes) => Some(key_display(bytes)),
        other => resp_text(other),
    }
}

fn resp_u64(value: &Resp) -> Option<u64> {
    match value {
        Resp::Int(n) => u64::try_from(*n).ok(),
        other => resp_text(other)?.parse().ok(),
    }
}

/// Reads a flat `name value ...` reply of `n` parts whose values are small;
/// `special` handles a value itself (returns false to read it normally).
async fn read_pairs<S: tokio::io::AsyncRead + Unpin>(
    raw: &mut RawConn<S>,
    n: u64,
) -> Result<Vec<(String, Resp)>, DriverError> {
    if !n.is_multiple_of(2) {
        return Err(measure::unexpected(
            "a map reply with an odd number of parts",
        ));
    }
    let mut pairs = Vec::new();
    for _ in 0..n / 2 {
        let name = raw.short().await?;
        pairs.push((name, raw.read_resp(FIELD_LIMIT).await?));
    }
    Ok(pairs)
}

fn field<'a>(pairs: &'a [(String, Resp)], name: &str) -> Option<&'a Resp> {
    pairs.iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

fn missing(what: &str) -> DriverError {
    measure::unexpected(&format!("the reply has no {what}"))
}

/// Reads `XINFO STREAM`; `None` when the key does not exist.
async fn stream_info(raw: &mut RawConn, key: &[u8]) -> Result<Option<StreamInfo>, DriverError> {
    raw.send(&[b"XINFO", b"STREAM", key]).await?;
    let n = match raw.head().await? {
        Head::Array(Some(n)) => n,
        Head::Error(line) if line.starts_with("ERR no such key") => return Ok(None),
        Head::Error(line) => return Err(measure::server_error(&line)),
        other => return Err(measure::unexpected(&format!("{other:?}"))),
    };
    if !n.is_multiple_of(2) {
        return Err(measure::unexpected(
            "XINFO STREAM with an odd number of parts",
        ));
    }
    let mut pairs = Vec::new();
    let mut first = None;
    let mut last = None;
    for _ in 0..n / 2 {
        let name = raw.short().await?;
        if name == "first-entry" || name == "last-entry" {
            // [id, [field, value, ...]]: the id only.
            let id = match raw.next().await? {
                Head::Array(Some(2)) => {
                    let id = raw.short().await?;
                    raw.skip_value().await?;
                    Some(id)
                }
                Head::Array(None) | Head::Bulk(None) => None,
                other => return Err(measure::unexpected(&format!("{other:?}"))),
            };
            if name == "first-entry" {
                first = id;
            } else {
                last = id;
            }
        } else {
            pairs.push((name, raw.read_resp(FIELD_LIMIT).await?));
        }
    }
    let text = |name: &str| field(&pairs, name).and_then(resp_text);
    Ok(Some(StreamInfo {
        length: field(&pairs, "length")
            .and_then(resp_u64)
            .ok_or_else(|| missing("length"))?,
        last_generated_id: text("last-generated-id").ok_or_else(|| missing("last id"))?,
        groups: field(&pairs, "groups").and_then(resp_u64).unwrap_or(0),
        first_entry_id: first,
        last_entry_id: last,
        max_deleted_entry_id: text("max-deleted-entry-id"),
        entries_added: text("entries-added"),
        recorded_first_entry_id: text("recorded-first-entry-id"),
    }))
}

/// Reads a list of maps (`XINFO GROUPS`, `XINFO CONSUMERS`); an empty
/// list when the key does not exist.
async fn info_list(
    raw: &mut RawConn,
    args: &[&[u8]],
) -> Result<Vec<Vec<(String, Resp)>>, DriverError> {
    raw.send(args).await?;
    let n = match raw.head().await? {
        Head::Array(Some(n)) => n,
        Head::Array(None) => 0,
        Head::Error(line) if line.starts_with("ERR no such key") => return Ok(Vec::new()),
        Head::Error(line) => return Err(measure::server_error(&line)),
        other => return Err(measure::unexpected(&format!("{other:?}"))),
    };
    let mut items = Vec::new();
    for _ in 0..n {
        let parts = raw.array().await?;
        items.push(read_pairs(raw, parts).await?);
    }
    Ok(items)
}

/// Parsed `XPENDING key group` summary body (after its array header of
/// `n` parts). Consumer names are raw bytes.
pub(crate) struct Summary {
    pub(crate) count: u64,
    pub(crate) min_id: Option<String>,
    pub(crate) max_id: Option<String>,
    pub(crate) consumers: Vec<(Vec<u8>, u64)>,
}

/// Reads the body of an `XPENDING key group` summary.
///
/// # Errors
///
/// Connection and protocol errors.
pub(crate) async fn read_pending_summary_body<S: tokio::io::AsyncRead + Unpin>(
    raw: &mut RawConn<S>,
    n: u64,
) -> Result<Summary, DriverError> {
    if n != 4 {
        return Err(measure::unexpected(
            "an XPENDING summary without four parts",
        ));
    }
    let count = raw.int_reply().await?;
    let min_id = resp_text(&raw.read_resp(FIELD_LIMIT).await?);
    let max_id = resp_text(&raw.read_resp(FIELD_LIMIT).await?);
    let mut consumers = Vec::new();
    if let Resp::Array(items) = raw.read_resp(FIELD_LIMIT * 16).await? {
        for item in items {
            if let Resp::Array(pair) = item
                && let [Resp::BulkString(name), count] = pair.as_slice()
            {
                consumers.push((name.clone(), resp_u64(count).unwrap_or(0)));
            }
        }
    }
    Ok(Summary {
        count: u64::try_from(count).unwrap_or(0),
        min_id,
        max_id,
        consumers,
    })
}

fn group_bytes(text: &str, what: &str) -> Result<Vec<u8>, EditError> {
    let bytes = key_from_display(text).map_err(|e| invalid(e.message))?;
    if bytes.is_empty() {
        return Err(invalid(format!("The {what} name is empty")));
    }
    Ok(bytes)
}

fn driver_group(text: &str, what: &str) -> Result<Vec<u8>, DriverError> {
    group_bytes(text, what).map_err(|e| DriverError::new(ErrorKind::Query, e.to_string()))
}

/// `$`, `0` or an entry id.
fn group_start_id(id: &str) -> Result<String, EditError> {
    let id = id.trim();
    if id == "$" || valid_stream_id(id, true) {
        Ok(id.to_string())
    } else {
        Err(invalid("The id is $, 0 or <milliseconds>-<sequence>"))
    }
}

fn entry_ids(ids: &[String]) -> Result<Vec<String>, EditError> {
    if ids.is_empty() || ids.len() > MAX_PAGE_COUNT as usize {
        return Err(invalid(format!("Give 1 to {MAX_PAGE_COUNT} entry ids")));
    }
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let id = id.trim();
        if !valid_stream_id(id, false) {
            return Err(invalid("An entry id is <milliseconds>-<sequence>"));
        }
        if out.iter().any(|seen: &String| seen == id) {
            return Err(invalid(format!("Entry {id} is listed twice")));
        }
        out.push(id.to_string());
    }
    Ok(out)
}

/// Checks the group's pending count, then destroys it, at once (`XREADGROUP`
/// and `XACK` do not abort a `WATCH`, so a separate check could miss
/// entries read in between). Returns `{0, now}` when the count differs,
/// `{0, 'gone'}` without the group (`NOGROUP`), `{1, 'OK'}` after the
/// change; any other error of `XPENDING` (an ACL refusal) is returned as
/// the script's error.
const DESTROY_SCRIPT: &str = "\
local ok, p = pcall(redis.call, 'XPENDING', KEYS[1], ARGV[1])
if not ok then
  local e = type(p) == 'table' and tostring(p.err) or tostring(p)
  if string.find(e, 'NOGROUP', 1, true) then return {0, 'gone'} end
  return redis.error_reply(e)
end
if p[1] ~= tonumber(ARGV[2]) then return {0, tostring(p[1])} end
redis.call('XGROUP', 'DESTROY', KEYS[1], ARGV[1])
return {1, 'OK'}";

/// As [`DESTROY_SCRIPT`] for one consumer's pending count and
/// `XGROUP DELCONSUMER`.
const DELCONSUMER_SCRIPT: &str = "\
local ok, p = pcall(redis.call, 'XPENDING', KEYS[1], ARGV[1])
if not ok then
  local e = type(p) == 'table' and tostring(p.err) or tostring(p)
  if string.find(e, 'NOGROUP', 1, true) then return {0, 'gone'} end
  return redis.error_reply(e)
end
local have = 0
if p[4] then
  for _, c in ipairs(p[4]) do
    if c[1] == ARGV[2] then have = tonumber(c[2]) end
  end
end
if have ~= tonumber(ARGV[3]) then return {0, tostring(have)} end
redis.call('XGROUP', 'DELCONSUMER', KEYS[1], ARGV[1], ARGV[2])
return {1, 'OK'}";

/// Builds the program of a group change (no server involved).
fn build_group(
    key: &[u8],
    db: u16,
    change: &StreamGroupChange,
    compat: (u32, u32),
) -> Result<EditProgram, EditError> {
    let mut checks = Vec::new();
    let mut lines = vec!["the key still is a stream".to_string()];
    let mut counts = EditCounts::default();
    let mut destructive = false;
    let mut type_names = vec!["stream".to_string()];
    let mut warnings = Vec::new();
    let mut script = None;
    let mut assess = Vec::new();
    let mut shown = None;
    let pending_of = |group: &[u8]| cmd(&[b"XPENDING", key, group]);
    let name = |bytes: &[u8]| display_line(&[bytes.to_vec()]);
    let entries_read = |n: Option<u64>, command: &mut Vec<Vec<u8>>| -> Result<(), EditError> {
        if let Some(n) = n {
            if compat < (7, 0) {
                return Err(invalid("ENTRIESREAD needs Redis 7.0 or newer"));
            }
            command.push(b"ENTRIESREAD".to_vec());
            command.push(n.to_string().into_bytes());
        }
        Ok(())
    };
    // `holds` describes the check for the plan, `fails` the conflict.
    let mut check = |command: Vec<Vec<u8>>, expect: Expectation, holds: String, fails: String| {
        lines.push(holds);
        checks.push(Check {
            edit: 0,
            command,
            expect,
            reason: ConflictReason::ValueChanged,
            what: fails,
        });
    };
    let command = match change {
        StreamGroupChange::GroupCreate {
            group,
            id,
            mk_stream,
            entries_read: read,
        } => {
            let group = group_bytes(group, "group")?;
            let id = group_start_id(id)?;
            check(
                pending_of(&group),
                Expectation::NoGroup,
                format!("no group named {} exists yet", name(&group)),
                format!("a group named {} exists now", name(&group)),
            );
            let mut command = cmd(&[b"XGROUP", b"CREATE", key, &group, id.as_bytes()]);
            if *mk_stream {
                command.push(b"MKSTREAM".to_vec());
                type_names.push("none".into());
            }
            entries_read(*read, &mut command)?;
            counts.added = 1;
            command
        }
        StreamGroupChange::GroupDestroy {
            group,
            expected_pending,
        } => {
            let group = group_bytes(group, "group")?;
            lines.push(format!(
                "group {} still has {expected_pending} pending entries, checked by the same Lua \
                 script that destroys it (so an entry read in between is never lost)",
                name(&group)
            ));
            script = Some(ScriptCheck {
                gone: format!("Group {} no longer exists", name(&group)),
                changed: format!(
                    "Group {} now has {{now}} pending entries, not {expected_pending}",
                    name(&group)
                ),
                commands: "reads the group's pending entries (XPENDING) and then destroys the \
                           group (XGROUP DESTROY)"
                    .into(),
            });
            assess.push("XGROUP DESTROY".to_string());
            shown = Some(format!(
                "{} (in a Lua script, after checking the pending count)",
                display_line(&cmd(&[b"XGROUP", b"DESTROY", key, &group]))
            ));
            destructive = true;
            counts.removed = 1;
            cmd(&[
                b"EVAL",
                DESTROY_SCRIPT.as_bytes(),
                b"1",
                key,
                &group,
                expected_pending.to_string().as_bytes(),
            ])
        }
        StreamGroupChange::GroupSetId {
            group,
            id,
            entries_read: read,
        } => {
            let group = group_bytes(group, "group")?;
            let id = group_start_id(id)?;
            check(
                pending_of(&group),
                Expectation::Pending {
                    total: None,
                    consumer: None,
                },
                format!("group {} still exists", name(&group)),
                format!("group {} no longer exists", name(&group)),
            );
            warnings.push(
                "Moving the last delivered id delivers entries again or skips them".to_string(),
            );
            destructive = true;
            counts.changed = 1;
            let mut command = cmd(&[b"XGROUP", b"SETID", key, &group, id.as_bytes()]);
            entries_read(*read, &mut command)?;
            command
        }
        StreamGroupChange::GroupDelConsumer {
            group,
            consumer,
            expected_pending,
        } => {
            let group = group_bytes(group, "group")?;
            let consumer = group_bytes(consumer, "consumer")?;
            lines.push(format!(
                "consumer {} still has {expected_pending} pending entries, checked by the same \
                 Lua script that deletes it",
                name(&consumer)
            ));
            if *expected_pending > 0 {
                warnings.push(format!(
                    "Its {expected_pending} pending entries are no longer tracked"
                ));
            }
            script = Some(ScriptCheck {
                gone: format!("Group {} no longer exists", name(&group)),
                changed: format!(
                    "Consumer {} now has {{now}} pending entries, not {expected_pending}",
                    name(&consumer)
                ),
                commands: "reads the group's pending entries (XPENDING) and then deletes the \
                           consumer (XGROUP DELCONSUMER)"
                    .into(),
            });
            assess.push("XGROUP DELCONSUMER".to_string());
            shown = Some(format!(
                "{} (in a Lua script, after checking its pending count)",
                display_line(&cmd(&[b"XGROUP", b"DELCONSUMER", key, &group, &consumer]))
            ));
            destructive = true;
            counts.removed = 1;
            cmd(&[
                b"EVAL",
                DELCONSUMER_SCRIPT.as_bytes(),
                b"1",
                key,
                &group,
                &consumer,
                expected_pending.to_string().as_bytes(),
            ])
        }
        StreamGroupChange::Ack { group, ids } | StreamGroupChange::Claim { group, ids, .. } => {
            let group = group_bytes(group, "group")?;
            let ids = entry_ids(ids)?;
            for id in &ids {
                check(
                    cmd(&[b"XPENDING", key, &group, id.as_bytes(), id.as_bytes(), b"1"]),
                    Expectation::NonEmpty,
                    format!("entry {id} is still pending"),
                    format!("entry {id} is no longer pending"),
                );
            }
            counts.changed = ids.len() as u64;
            match change {
                StreamGroupChange::Claim {
                    consumer,
                    min_idle_ms,
                    just_id,
                    ..
                } => {
                    let consumer = group_bytes(consumer, "consumer")?;
                    let mut command = cmd(&[
                        b"XCLAIM",
                        key,
                        &group,
                        &consumer,
                        min_idle_ms.to_string().as_bytes(),
                    ]);
                    command.extend(ids.into_iter().map(String::into_bytes));
                    if *just_id {
                        command.push(b"JUSTID".to_vec());
                    }
                    command
                }
                _ => {
                    let mut command = cmd(&[b"XACK", key, &group]);
                    command.extend(ids.into_iter().map(String::into_bytes));
                    command
                }
            }
        }
        StreamGroupChange::AutoClaim {
            group,
            consumer,
            min_idle_ms,
            start,
            count,
            just_id,
        } => {
            let group = group_bytes(group, "group")?;
            let consumer = group_bytes(consumer, "consumer")?;
            let start = start.trim();
            if !valid_stream_id(start, true) {
                return Err(invalid(
                    "The start is <milliseconds>-<sequence> (0-0: the beginning)",
                ));
            }
            if *count == 0 || *count > MAX_PAGE_COUNT {
                return Err(invalid(format!("Claim 1 to {MAX_PAGE_COUNT} entries")));
            }
            check(
                pending_of(&group),
                Expectation::Pending {
                    total: None,
                    consumer: None,
                },
                format!("group {} still exists", name(&group)),
                format!("group {} no longer exists", name(&group)),
            );
            counts.changed = 1;
            let mut command = cmd(&[
                b"XAUTOCLAIM",
                key,
                &group,
                &consumer,
                min_idle_ms.to_string().as_bytes(),
                start.as_bytes(),
                b"COUNT",
                count.to_string().as_bytes(),
            ]);
            if *just_id {
                command.push(b"JUSTID".to_vec());
            }
            command
        }
    };
    lines.push(
        "nobody changes the stream's entries or groups between these checks and the transaction \
         (WATCH; reads by consumers and acknowledgements do not count as changes there, which is \
         why destroying a group or deleting a consumer is checked inside its script)"
            .into(),
    );
    let view = RedisEditPlan {
        key: key_display(key),
        db,
        commands: vec![shown.unwrap_or_else(|| display_line(&command))],
        checks: lines,
        risk: CommandRisk::Write,
        destructive,
        unchecked: false,
        warnings,
    };
    Ok(EditProgram {
        key: key.to_vec(),
        db,
        watch: vec![key.to_vec()],
        type_names,
        version: None,
        check_content: false,
        check_expiry: false,
        exact_expiry: compat >= (7, 0),
        script,
        assess,
        checks,
        commands: vec![command],
        operations: vec![change.op_name()],
        counts,
        view,
        json: false,
    })
}

/// A reply value as a `u64`, for the extended `XPENDING` entries.
fn entry_u64(value: &Resp) -> u64 {
    resp_u64(value).unwrap_or(0)
}

impl RedisSession {
    /// `XINFO STREAM` without the entries' fields; `None` when the key does
    /// not exist.
    ///
    /// # Errors
    ///
    /// `WRONGTYPE` for another type, connection errors, Cancelled.
    pub async fn stream_info(&self, key: &[u8]) -> Result<Option<StreamInfo>, DriverError> {
        let mut call = self.begin().await?;
        let key = key.to_vec();
        measure::with_side(&mut call, move |raw| {
            Box::pin(async move { stream_info(raw, &key).await })
        })
        .await
    }

    /// `XINFO GROUPS`; empty when the key does not exist.
    ///
    /// # Errors
    ///
    /// As [`RedisSession::stream_info`].
    pub async fn stream_groups(&self, key: &[u8]) -> Result<Vec<StreamGroup>, DriverError> {
        let mut call = self.begin().await?;
        let key = key.to_vec();
        let items = measure::with_side(&mut call, move |raw| {
            Box::pin(async move { info_list(raw, &[b"XINFO", b"GROUPS", &key]).await })
        })
        .await?;
        items
            .iter()
            .map(|pairs| {
                let text = |name: &str| field(pairs, name).and_then(resp_text);
                Ok(StreamGroup {
                    name: field(pairs, "name")
                        .and_then(resp_name)
                        .ok_or_else(|| missing("name"))?,
                    consumers: field(pairs, "consumers").and_then(resp_u64).unwrap_or(0),
                    pending: field(pairs, "pending").and_then(resp_u64).unwrap_or(0),
                    last_delivered_id: text("last-delivered-id").unwrap_or_default(),
                    entries_read: text("entries-read"),
                    lag: text("lag"),
                })
            })
            .collect()
    }

    /// `XINFO CONSUMERS` of a group.
    ///
    /// # Errors
    ///
    /// `NOGROUP` for an unknown group, as [`RedisSession::stream_info`].
    pub async fn stream_consumers(
        &self,
        key: &[u8],
        group: &str,
    ) -> Result<Vec<StreamConsumer>, DriverError> {
        let group = driver_group(group, "group")?;
        let mut call = self.begin().await?;
        let key = key.to_vec();
        let items = measure::with_side(&mut call, move |raw| {
            Box::pin(async move { info_list(raw, &[b"XINFO", b"CONSUMERS", &key, &group]).await })
        })
        .await?;
        items
            .iter()
            .map(|pairs| {
                Ok(StreamConsumer {
                    name: field(pairs, "name")
                        .and_then(resp_name)
                        .ok_or_else(|| missing("name"))?,
                    pending: field(pairs, "pending").and_then(resp_u64).unwrap_or(0),
                    idle_ms: field(pairs, "idle").and_then(resp_u64).unwrap_or(0),
                    inactive_ms: field(pairs, "inactive").and_then(|v| match v {
                        Resp::Int(n) if *n >= 0 => Some(n.to_string()),
                        _ => None,
                    }),
                })
            })
            .collect()
    }

    /// `XPENDING key group`.
    ///
    /// # Errors
    ///
    /// `NOGROUP` for an unknown group or a missing key, as
    /// [`RedisSession::stream_info`].
    pub async fn stream_pending_summary(
        &self,
        key: &[u8],
        group: &str,
    ) -> Result<PendingSummary, DriverError> {
        let group = driver_group(group, "group")?;
        let mut call = self.begin().await?;
        let key = key.to_vec();
        let summary = measure::with_side(&mut call, move |raw| {
            Box::pin(async move {
                raw.send(&[b"XPENDING", &key, &group]).await?;
                let n = raw.array().await?;
                read_pending_summary_body(raw, n).await
            })
        })
        .await?;
        Ok(PendingSummary {
            count: summary.count,
            min_id: summary.min_id,
            max_id: summary.max_id,
            consumers: summary
                .consumers
                .into_iter()
                .map(|(name, pending)| ConsumerPending {
                    name: key_display(&name),
                    pending,
                })
                .collect(),
        })
    }

    /// A page of pending entries (`XPENDING key group [IDLE ms] start + count
    /// [consumer]`).
    ///
    /// # Errors
    ///
    /// Query errors for a malformed start, as
    /// [`RedisSession::stream_pending_summary`].
    pub async fn stream_pending(
        &self,
        key: &[u8],
        group: &str,
        query: &PendingQuery,
    ) -> Result<PendingPage, DriverError> {
        let group = driver_group(group, "group")?;
        let consumer = query
            .consumer
            .as_deref()
            .map(|c| driver_group(c, "consumer"))
            .transpose()?;
        let start = match query.start.as_deref().map(str::trim) {
            None | Some("" | "-") => "-".to_string(),
            Some(start) => {
                let id = start.strip_prefix('(').unwrap_or(start);
                if !valid_stream_id(id, true) {
                    return Err(DriverError::new(
                        ErrorKind::Query,
                        "The start of the page is not an entry id",
                    ));
                }
                start.to_string()
            }
        };
        let count = query.count.clamp(1, MAX_PAGE_COUNT);
        let min_idle = query.min_idle_ms;
        let mut call = self.begin().await?;
        let key = key.to_vec();
        let rows = measure::with_side(&mut call, move |raw| {
            Box::pin(async move {
                let ask = (count + 1).to_string();
                let idle = min_idle.map(|m| m.to_string());
                let mut args: Vec<&[u8]> = vec![b"XPENDING", &key, &group];
                if let Some(idle) = &idle {
                    args.push(b"IDLE");
                    args.push(idle.as_bytes());
                }
                args.push(start.as_bytes());
                args.push(b"+");
                args.push(ask.as_bytes());
                if let Some(consumer) = &consumer {
                    args.push(consumer);
                }
                raw.send(&args).await?;
                raw.read_resp(crate::session::MAX_REPLY_BYTES).await
            })
        })
        .await?;
        let Resp::Array(rows) = rows else {
            return Ok(PendingPage {
                entries: Vec::new(),
                next_start: None,
            });
        };
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let Resp::Array(parts) = row else {
                return Err(measure::unexpected(
                    "an XPENDING entry that is not an array",
                ));
            };
            let [id, owner, idle, deliveries] = parts.as_slice() else {
                return Err(measure::unexpected("an XPENDING entry without four parts"));
            };
            entries.push(PendingEntry {
                id: resp_text(id).ok_or_else(|| missing("id"))?,
                consumer: resp_name(owner).ok_or_else(|| missing("consumer"))?,
                idle_ms: entry_u64(idle),
                deliveries: entry_u64(deliveries),
            });
        }
        let more = entries.len() > count as usize;
        entries.truncate(count as usize);
        let next_start = if more {
            entries.last().map(|e| format!("({}", e.id))
        } else {
            None
        };
        Ok(PendingPage {
            entries,
            next_start,
        })
    }

    /// Plans a consumer group change on a stream key, applied with
    /// [`RedisSession::apply_edit`] (same checks and outcome as the value
    /// editors).
    ///
    /// # Errors
    ///
    /// [`EditError::Invalid`] for a malformed change, read-only sessions,
    /// connection errors while classifying the commands.
    pub async fn plan_stream_group(
        &self,
        key: &[u8],
        change: &StreamGroupChange,
    ) -> Result<EditProgram, EditError> {
        if self.is_read_only() {
            return Err(EditError::Driver(DriverError::new(
                ErrorKind::ReadOnly,
                "This connection is read-only, so consumer groups cannot be changed",
            )));
        }
        if key.is_empty() {
            return Err(invalid("The key name is empty"));
        }
        let db = self.current_db().await;
        let mut program = build_group(key, db, change, self.compat_version())?;
        self.finish_plan(&mut program).await?;
        Ok(program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_changes_deserialize_and_build() -> Result<(), Box<dyn std::error::Error>> {
        let change: StreamGroupChange = serde_json::from_value(serde_json::json!({
            "op": "claim", "group": "g", "consumer": "c", "minIdleMs": "1000",
            "ids": ["1-1", "2-0"], "justId": true
        }))?;
        let program = build_group(b"s", 0, &change, (8, 10))?;
        assert_eq!(program.view.commands, ["XCLAIM s g c 1000 1-1 2-0 JUSTID"]);
        assert_eq!(program.checks.len(), 2);
        assert!(!program.view.destructive);

        let create = StreamGroupChange::GroupCreate {
            group: "g".into(),
            id: "$".into(),
            mk_stream: true,
            entries_read: Some(3),
        };
        assert!(
            build_group(b"s", 0, &create, (6, 2)).is_err(),
            "ENTRIESREAD needs 7.0"
        );
        let program = build_group(b"s", 0, &create, (7, 2))?;
        assert_eq!(
            program.view.commands,
            ["XGROUP CREATE s g $ MKSTREAM ENTRIESREAD 3"]
        );
        assert_eq!(program.type_names, ["stream", "none"]);

        let destroy = StreamGroupChange::GroupDestroy {
            group: "g".into(),
            expected_pending: 2,
        };
        let program = build_group(b"s", 0, &destroy, (6, 2))?;
        assert!(program.view.destructive);
        assert_eq!(program.command_names(), ["XGROUP"]);

        let bad = StreamGroupChange::Ack {
            group: "g".into(),
            ids: vec!["1-1".into(), "1-1".into()],
        };
        assert!(build_group(b"s", 0, &bad, (8, 10)).is_err());
        let bad = StreamGroupChange::Ack {
            group: String::new(),
            ids: vec!["1-1".into()],
        };
        assert!(build_group(b"s", 0, &bad, (8, 10)).is_err());
        Ok(())
    }
}
