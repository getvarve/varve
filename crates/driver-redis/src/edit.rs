//! Value editors (design doc 4.2): planned writes to one key, applied with
//! optimistic checks.
//!
//! [`RedisSession::plan_edit`] turns what the UI wants ([`KeyEdit`]s on one
//! key, with what it read: [`KeyExpect`] and each edit's `original`) into an
//! [`EditProgram`]: the exact commands, the checks that run first, and a
//! view for the preview ([`RedisEditPlan`]). [`RedisSession::apply_edit`]
//! runs a program on its own short-lived side connection (client name
//! `<name>-edit`, set up like the session's), never on the session
//! connection, so a console `WATCH` or `MULTI` is never touched:
//!
//! 1. `WATCH` the key (and the new name of a rename);
//! 2. the key's type must be what was read; for whole-key changes
//!    (delete, rename, trim) also its length and content digest, and for
//!    time to live changes its expiry ([`KeyVersion`]);
//! 3. every edit's `original` must still hold (compared while it is read,
//!    so a current value of any size is never held);
//! 4. `MULTI`, the commands, `EXEC`: a change by anyone after step 1 makes
//!    `EXEC` return nil, which is a conflict and nothing ran.
//!
//! The content digest is read in pieces ([`crate::digest`]) and again
//! inside the `WATCH`, so a same-length change is a conflict too. Opening
//! a key ([`RedisSession::key_version`], [`VersionReader::read_open`])
//! reads it only for a small key (bounded in bytes, elements and time;
//! `DUMP` only of a small JSON document); a larger key is [`DigestMissing::Deferred`] and its
//! digest is read right after, in the background, on its own connection
//! ([`VersionReader::read_full`]), by the caller (the app). A whole-key
//! change whose version has no digest (still deferred, too large, not
//! permitted, or the read stalled) is [`RedisEditPlan::unchecked`]: it needs
//! the user's explicit confirmation, and the plan says why. Originals are kept
//! in the plan only as their length and digest.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use varve_driver::{DriverError, ErrorKind, Value};

use crate::browser::{FieldValue, KeyType};
use crate::measure::{self, Head, RawConn, SIDE_NAME_SUFFIX, encode};
use crate::modules::{ModuleKind, TsLabel, TsMeta};
use crate::oversized::OVERSIZED_MARK;
use crate::parse::key_display;
use crate::reply::bytes_value;
use crate::risk::CommandRisk;
use crate::session::RedisSession;

/// Most edits in one plan.
pub const MAX_EDITS: usize = 10_000;

/// Most bytes of values (new values and originals) in one plan (8 MiB).
pub const MAX_EDIT_BYTES: u64 = 8 * 1024 * 1024;

/// How far apart two reads of the same expiry may be on servers without
/// `PEXPIRETIME` (Redis 6.2: `TIME` plus `PTTL`).
pub const TTL_TOLERANCE_MS: i64 = 50;

/// Suffix of the client name of the connection an edit runs on.
pub const EDIT_NAME_SUFFIX: &str = "-edit";

/// Characters of a long value shown in a plan's command lines.
const DISPLAY_CHARS: usize = 100;

/// Bytes of a reply string kept in [`EditOutcome::Applied`].
const REPLY_KEEP: u64 = 4096;

/// Values kept of all replies of one transaction; arrays beyond it are
/// summarized.
const REPLY_VALUES: usize = 20_000;

/// What identifies the state of a key when it was read: compared again
/// before whole-key changes (delete, rename, trim).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyVersion {
    /// Type.
    pub key_type: KeyType,
    /// Bytes of a string, entries of a hash, list, set, sorted set or
    /// stream; `None` for other types.
    #[serde(with = "crate::browser::opt_u64_text", default)]
    pub length: Option<u64>,
    /// Digest of the content, read in pieces (see [`crate::DIGEST_TIME_BUDGET`]); `None`
    /// when it could not be taken (`digest_missing` says why).
    #[serde(default)]
    pub digest: Option<String>,
    /// Why there is no digest.
    #[serde(default)]
    pub digest_missing: Option<DigestMissing>,
    /// When the key expires, unix ms on the server's clock; `None` when it
    /// does not expire.
    #[serde(with = "crate::browser::opt_i64_text", default)]
    pub expire_at_ms: Option<i64>,
    /// The id of the background read of a [`DigestMissing::Deferred`]
    /// version's digest, set by the app; sent back unchanged with a plan.
    #[serde(default)]
    pub digest_read: Option<String>,
    /// The logical database the key was read on ([`VersionReader::read_open`]);
    /// a plan on a session now on another database is refused.
    #[serde(default)]
    pub db: Option<u16>,
    /// Why the digest is missing, in words, when the caller knows better
    /// than [`Self::digest_missing`] (for example: the background read
    /// belongs to another key). Shown in an unchecked plan's warning. Never
    /// sent to or taken from the UI.
    #[serde(skip)]
    pub unchecked_reason: Option<String>,
}

/// Why a [`KeyVersion`] has no digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DigestMissing {
    /// The key could not be read within [`crate::DIGEST_TIME_BUDGET`] or
    /// has more than [`crate::DIGEST_MAX_ELEMENTS`] elements.
    TooLarge,
    /// The server refused a command the digest needs (an ACL user).
    NotPermitted,
    /// The key is larger than what is read when it is opened
    /// ([`crate::OPEN_DIGEST_BYTES`], [`crate::OPEN_DIGEST_ELEMENTS`],
    /// [`crate::OPEN_DIGEST_TIME`], [`crate::OPEN_JSON_ELEMENTS`]), or a
    /// module key (never `DUMP`ed when opened): its digest is read in the background
    /// ([`VersionReader::read_full`]) and not there yet.
    Deferred,
    /// The background read of the digest got no answer in time (a stalled
    /// server or network), as opposed to a key too large to read.
    Stalled,
}

/// What the UI read about the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum KeyExpect {
    /// The key must not exist: the plan creates it.
    Absent,
    /// The key exists with this type.
    #[serde(rename_all = "camelCase")]
    Present {
        /// Type.
        key_type: KeyType,
        /// Its version when it was read; required for whole-key and time
        /// to live changes, ignored by plans that only change elements.
        #[serde(default)]
        version: Option<KeyVersion>,
    },
}

/// Which end of a list a push goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ListEnd {
    /// `LPUSH`.
    Head,
    /// `RPUSH`.
    Tail,
}

/// How a stream is trimmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrimStrategy {
    /// Keep at most `threshold` entries.
    MaxLen,
    /// Drop entries with ids below `threshold`.
    MinId,
}

/// One change to a key. Values are [`Value::Text`] or [`Value::Bytes`] as
/// the browser returned them; `original` is what the UI read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum KeyEdit {
    /// `SET ... KEEPTTL`. `original` is `None` only when creating the key.
    StringSet {
        /// New value.
        value: Value,
        /// The value read.
        original: Option<Value>,
    },
    /// `JSON.SET key $ document`. `original` is the document text read,
    /// `None` only when creating the key.
    JsonSet {
        /// New document, JSON text.
        document: String,
        /// The document read.
        original: Option<String>,
    },
    /// `HSET`. `original` `None`: the field must not exist (an add).
    HashSet {
        /// Field.
        field: Value,
        /// New value.
        value: Value,
        /// The value read.
        original: Option<Value>,
    },
    /// `HDEL` of a field that still has `original`.
    HashDelete {
        /// Field.
        field: Value,
        /// The value read.
        original: Value,
    },
    /// `LPUSH` or `RPUSH`.
    ListPush {
        /// Which end.
        end: ListEnd,
        /// Elements, pushed in this order.
        values: Vec<Value>,
    },
    /// `LSET` of the element at `index` (as read).
    ListSet {
        /// Position in the list as read.
        #[serde(with = "crate::browser::u64_text")]
        index: u64,
        /// New element.
        value: Value,
        /// The element read.
        original: Value,
    },
    /// Removes the element at `index` (as read).
    ListRemove {
        /// Position in the list as read.
        #[serde(with = "crate::browser::u64_text")]
        index: u64,
        /// The element read.
        original: Value,
    },
    /// `SADD`.
    SetAdd {
        /// Members.
        members: Vec<Value>,
    },
    /// `SREM` of a member that still exists.
    SetRemove {
        /// Member.
        member: Value,
    },
    /// `ZADD ... NX` of a new member.
    ZsetAdd {
        /// Member.
        member: Value,
        /// Score.
        score: Value,
    },
    /// `ZADD ... XX`: a new score for a member that still has `original`.
    ZsetScore {
        /// Member.
        member: Value,
        /// New score.
        score: Value,
        /// The score read.
        original: Value,
    },
    /// `ZREM` of a member that still has the score `original`.
    ZsetRemove {
        /// Member.
        member: Value,
        /// The score read.
        original: Value,
    },
    /// `XADD`.
    StreamAdd {
        /// Entry id; `None` or `*` lets the server choose.
        id: Option<String>,
        /// Fields in order.
        fields: Vec<FieldValue>,
    },
    /// `XDEL` of an entry that still exists.
    StreamDelete {
        /// Entry id.
        id: String,
    },
    /// `XTRIM`.
    StreamTrim {
        /// How to trim.
        strategy: TrimStrategy,
        /// Most entries (`maxLen`) or the lowest id kept (`minId`).
        threshold: String,
        /// `~`: let the server trim whole nodes only (faster).
        approximate: bool,
    },
    /// `PEXPIRE`.
    #[serde(rename_all = "camelCase")]
    Expire {
        /// Time to live in milliseconds, at least 1.
        #[serde(with = "crate::browser::u64_text")]
        ttl_ms: u64,
    },
    /// `PERSIST`.
    Persist,
    /// `RENAME`.
    #[serde(rename_all = "camelCase")]
    Rename {
        /// New name, display text ([`crate::key_display`]).
        new_key: String,
        /// Overwrite a key of that name; otherwise it must not exist.
        replace: bool,
    },
    /// `UNLINK`.
    Delete,
    /// `TS.ADD` of a sample. With a `timestamp` the series must have no
    /// sample there yet; `None` or `*` lets the server use its clock.
    TsAdd {
        /// Milliseconds, or `*`.
        timestamp: Option<String>,
        /// The value: a finite number, or `nan`.
        value: Value,
    },
    /// `TS.DEL` of the samples from `from` to `to` (milliseconds, both
    /// included; `-` and `+` for the ends). A whole-key change: checked
    /// against the key's version like a stream trim.
    TsDeleteRange {
        /// First timestamp, or `-`.
        from: String,
        /// Last timestamp, or `+`.
        to: String,
    },
    /// `TS.ALTER`: a new retention, labels (replacing all of them) or
    /// duplicate policy. `original` is what `TS.INFO` showed; it must still
    /// hold.
    #[serde(rename_all = "camelCase")]
    TsAlter {
        /// New retention in ms (`0`: keep forever).
        #[serde(with = "crate::browser::opt_u64_text", default)]
        retention_ms: Option<u64>,
        /// New labels.
        #[serde(default)]
        labels: Option<Vec<TsLabel>>,
        /// New duplicate policy (`block`, `first`, `last`, `min`, `max`,
        /// `sum`).
        #[serde(default)]
        duplicate_policy: Option<String>,
        /// What was read.
        original: TsMeta,
    },
    /// `BF.MADD`.
    BloomAdd {
        /// Items.
        items: Vec<Value>,
    },
    /// `CF.ADD` per item, or `CF.ADDNX` (only items not in the filter yet)
    /// with `nx`.
    CuckooAdd {
        /// Items.
        items: Vec<Value>,
        /// Add only items the filter does not have.
        #[serde(default)]
        nx: bool,
    },
    /// `CF.DEL` of an item the filter still reports.
    CuckooDelete {
        /// Item.
        item: Value,
    },
    /// `CMS.INCRBY`.
    CmsIncrBy {
        /// Items and how much to add to each.
        items: Vec<CmsIncrement>,
    },
    /// `TOPK.ADD`.
    TopkAdd {
        /// Items.
        items: Vec<Value>,
    },
    /// `TDIGEST.ADD`.
    TdigestAdd {
        /// Observations (finite numbers).
        values: Vec<Value>,
    },
}

/// An item of a [`KeyEdit::CmsIncrBy`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmsIncrement {
    /// Item.
    pub item: Value,
    /// Amount, at least 1.
    #[serde(with = "crate::browser::u64_text")]
    pub increment: u64,
}

impl KeyEdit {
    /// The `op` name, as serialized.
    #[must_use]
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::StringSet { .. } => "stringSet",
            Self::JsonSet { .. } => "jsonSet",
            Self::HashSet { .. } => "hashSet",
            Self::HashDelete { .. } => "hashDelete",
            Self::ListPush { .. } => "listPush",
            Self::ListSet { .. } => "listSet",
            Self::ListRemove { .. } => "listRemove",
            Self::SetAdd { .. } => "setAdd",
            Self::SetRemove { .. } => "setRemove",
            Self::ZsetAdd { .. } => "zsetAdd",
            Self::ZsetScore { .. } => "zsetScore",
            Self::ZsetRemove { .. } => "zsetRemove",
            Self::StreamAdd { .. } => "streamAdd",
            Self::StreamDelete { .. } => "streamDelete",
            Self::StreamTrim { .. } => "streamTrim",
            Self::Expire { .. } => "expire",
            Self::Persist => "persist",
            Self::Rename { .. } => "rename",
            Self::Delete => "delete",
            Self::TsAdd { .. } => "tsAdd",
            Self::TsDeleteRange { .. } => "tsDeleteRange",
            Self::TsAlter { .. } => "tsAlter",
            Self::BloomAdd { .. } => "bloomAdd",
            Self::CuckooAdd { .. } => "cuckooAdd",
            Self::CuckooDelete { .. } => "cuckooDelete",
            Self::CmsIncrBy { .. } => "cmsIncrBy",
            Self::TopkAdd { .. } => "topkAdd",
            Self::TdigestAdd { .. } => "tdigestAdd",
        }
    }
}

/// A plan as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedisEditPlan {
    /// The key, display text.
    pub key: String,
    /// The logical database the plan applies to.
    pub db: u16,
    /// The commands that run inside `MULTI`/`EXEC`, long values shortened.
    pub commands: Vec<String>,
    /// What is checked before, in words.
    pub checks: Vec<String>,
    /// Highest server classification of the commands.
    pub risk: CommandRisk,
    /// True when the plan deletes or overwrites data other than an edited
    /// value (a key delete, a rename that replaces, a trim, element
    /// removals).
    pub destructive: bool,
    /// True when a whole-key change cannot be checked for concurrent
    /// changes (the key is too large to digest, or the user may not read
    /// it in pieces): applying it always needs explicit confirmation.
    pub unchecked: bool,
    /// Warnings for the user.
    pub warnings: Vec<String>,
}

/// Elements added, changed and removed by a plan, for the activity log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditCounts {
    /// Elements (or keys) added.
    pub added: u64,
    /// Elements changed (values, scores, a TTL, a name).
    pub changed: u64,
    /// Elements (or keys) removed.
    pub removed: u64,
}

/// What to compare a check's reply with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expectation {
    /// A bulk string of this length and digest (`None`: nil).
    Bulk(Option<(u64, u64)>),
    /// An integer.
    Int(i64),
    /// A score equal to this (`None`: nil, no such member).
    Score(Option<f64>),
    /// A non-empty array (an error reply does not hold).
    NonEmpty,
    /// An error reply with code `NOGROUP` (`XPENDING key group`: the group
    /// does not exist).
    NoGroup,
    /// An empty array (`TS.RANGE key t t`: no sample at `t`).
    Empty,
    /// The `XPENDING key group` summary: the group exists, with `total`
    /// pending entries when given, and the consumer with that many pending
    /// entries when given (a missing consumer has 0).
    Pending {
        total: Option<u64>,
        consumer: Option<(Vec<u8>, u64)>,
    },
}

/// How a scripted change reports a failed check: its reply is
/// `{0, now}` (refused, `now` the current state, or `gone`) or `{1, ...}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScriptCheck {
    /// Message when the group or consumer is gone.
    pub(crate) gone: String,
    /// Message when the state changed; `{now}` is replaced.
    pub(crate) changed: String,
    /// What the script reads and changes, for the hint when an ACL user
    /// may not run one of its commands.
    pub(crate) commands: String,
}

/// Length and digest of a value, as [`RawConn::hash_body`] reads it.
pub(crate) fn value_digest(bytes: &[u8]) -> (u64, u64) {
    use std::hash::Hasher;
    let mut h = std::hash::DefaultHasher::new();
    h.write(bytes);
    (bytes.len() as u64, h.finish())
}

/// One check before the transaction.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Check {
    /// Index of the edit it belongs to.
    pub(crate) edit: usize,
    pub(crate) command: Vec<Vec<u8>>,
    pub(crate) expect: Expectation,
    pub(crate) reason: ConflictReason,
    /// The conflict message when the check fails (empty: a generic one).
    pub(crate) what: String,
}

/// A planned edit, ready to run: the commands and checks exactly as
/// planned. Keep it on the server and apply it with
/// [`RedisSession::apply_edit`].
#[derive(Clone, PartialEq)]
pub struct EditProgram {
    pub(crate) key: Vec<u8>,
    pub(crate) db: u16,
    pub(crate) watch: Vec<Vec<u8>>,
    /// `TYPE` names the key may have (`none` for absent).
    pub(crate) type_names: Vec<String>,
    pub(crate) version: Option<KeyVersion>,
    /// Compare the version's length and digest (whole-key changes).
    pub(crate) check_content: bool,
    /// Compare the version's expiry (time to live changes).
    pub(crate) check_expiry: bool,
    /// The server has `PEXPIRETIME` (Redis 7.0 and newer, Valkey).
    pub(crate) exact_expiry: bool,
    /// A Lua script checks and changes at once (consumer groups): how to
    /// read its refusal.
    pub(crate) script: Option<ScriptCheck>,
    /// Command lines to classify instead of the commands (scripts).
    pub(crate) assess: Vec<String>,
    pub(crate) checks: Vec<Check>,
    pub(crate) commands: Vec<Vec<Vec<u8>>>,
    pub(crate) operations: Vec<&'static str>,
    pub(crate) counts: EditCounts,
    pub(crate) view: RedisEditPlan,
    /// A JSON key: the plan warns when the server has no JSON commands.
    pub(crate) json: bool,
}

impl std::fmt::Debug for EditProgram {
    // Values stay out of logs: the view shortens them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditProgram")
            .field("key", &self.view.key)
            .field("db", &self.db)
            .field("commands", &self.view.commands)
            .finish_non_exhaustive()
    }
}

impl EditProgram {
    /// The view for the preview.
    #[must_use]
    pub fn view(&self) -> &RedisEditPlan {
        &self.view
    }

    /// The `op` names of the edits, in order.
    #[must_use]
    pub fn operations(&self) -> &[&'static str] {
        &self.operations
    }

    /// Elements added, changed and removed.
    #[must_use]
    pub fn counts(&self) -> EditCounts {
        self.counts
    }

    /// The command names, for a confirmation dialog.
    #[must_use]
    pub fn command_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        if !self.assess.is_empty() {
            for line in &self.assess {
                let name = line
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            return names;
        }
        for command in &self.commands {
            if let Some(name) = command.first() {
                let name = String::from_utf8_lossy(name).to_ascii_uppercase();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }
}

/// Why a plan did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictReason {
    /// The key is gone, was created, or has another type.
    TypeChanged,
    /// The key's length or digest differs from its version.
    VersionChanged,
    /// An edit's `original` no longer holds.
    ValueChanged,
    /// A rename's new name exists.
    TargetExists,
    /// `EXEC` was aborted by the `WATCH`: someone changed the key while
    /// the checks ran.
    ConcurrentChange,
}

/// The result of [`RedisSession::apply_edit`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum EditOutcome {
    /// Every command ran.
    Applied {
        /// One reply per command.
        replies: Vec<Value>,
    },
    /// A check failed or the transaction was aborted; nothing ran.
    Conflict {
        /// The edit whose check failed, when it was one edit's.
        edit: Option<usize>,
        /// Why.
        reason: ConflictReason,
        /// For the user.
        message: String,
    },
    /// `EXEC` ran and the server rejected command `command`; Redis does not
    /// roll back, so the other commands ran.
    Failed {
        /// Index of the rejected command in [`RedisEditPlan::commands`].
        command: usize,
        /// The server's error.
        error: DriverError,
    },
}

/// Why a plan cannot be made.
#[derive(Debug, Clone, PartialEq)]
pub enum EditError {
    /// The request is not valid; the message says why.
    Invalid(String),
    /// The server could not be asked.
    Driver(DriverError),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::Driver(error) => write!(f, "{}", error.message),
        }
    }
}

impl std::error::Error for EditError {}

impl From<DriverError> for EditError {
    fn from(error: DriverError) -> Self {
        Self::Driver(error)
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> EditError {
    EditError::Invalid(message.into())
}

/// The bytes of an element value.
pub(crate) fn value_bytes(value: &Value, what: &str) -> Result<Vec<u8>, EditError> {
    match value {
        Value::Text(text) => Ok(text.as_bytes().to_vec()),
        Value::Bytes(bytes) => Ok(bytes.clone()),
        Value::Other(text) if text.starts_with(OVERSIZED_MARK) => Err(invalid(format!(
            "The {what} is too large to show, so it cannot be edited here; use the console"
        ))),
        _ => Err(invalid(format!("The {what} must be text or bytes"))),
    }
}

/// A score from the UI and its command argument.
pub(crate) fn score(value: &Value, what: &str) -> Result<(f64, String), EditError> {
    let parsed = match value {
        Value::Float(f) => Some(*f),
        #[allow(clippy::cast_precision_loss)] // scores are doubles in Redis
        Value::Int(i) => Some(*i as f64),
        Value::Text(text) | Value::Other(text) => parse_score(text),
        _ => None,
    };
    match parsed {
        Some(f) if !f.is_nan() => Ok((f, score_arg(f))),
        _ => Err(invalid(format!("The {what} must be a number, inf or -inf"))),
    }
}

pub(crate) fn parse_score(text: &str) -> Option<f64> {
    match text.trim().to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        other => other.parse().ok(),
    }
}

pub(crate) fn score_arg(f: f64) -> String {
    if f == f64::INFINITY {
        "+inf".into()
    } else if f == f64::NEG_INFINITY {
        "-inf".into()
    } else {
        format!("{f}")
    }
}

/// `<ms>-<seq>`, or `<ms>` when `partial`.
pub(crate) fn valid_stream_id(id: &str, partial: bool) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match id.split_once('-') {
        Some((ms, seq)) => digits(ms) && digits(seq),
        None => partial && digits(id),
    }
}

/// Command line for display: long values shortened.
pub(crate) fn display_line(args: &[Vec<u8>]) -> String {
    args.iter()
        .map(|arg| {
            let text = String::from_utf8_lossy(arg);
            if text.chars().count() > DISPLAY_CHARS {
                let head: String = text.chars().take(DISPLAY_CHARS).collect();
                format!(
                    "{}...[{} bytes]",
                    varve_redis_syntax::join_args(&[head.as_bytes()]),
                    arg.len()
                )
            } else {
                varve_redis_syntax::join_args(&[arg.as_slice()])
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Shortened display text of a value for check descriptions.
pub(crate) fn short(bytes: &[u8]) -> String {
    display_line(&[bytes.to_vec()])
}

pub(crate) fn cmd(parts: &[&[u8]]) -> Vec<Vec<u8>> {
    parts.iter().map(|p| p.to_vec()).collect()
}

/// A marker no real element has, for removing list elements by index.
fn list_marker() -> Vec<u8> {
    use std::hash::{BuildHasher, Hasher};
    let random = std::collections::hash_map::RandomState::new();
    let mut a = random.build_hasher();
    a.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    let mut b = random.build_hasher();
    b.write_u32(std::process::id());
    format!("\u{0}varve-removed:{:016x}{:016x}", a.finish(), b.finish()).into_bytes()
}

/// The type a creating edit makes, if it creates.
fn created_type(edit: &KeyEdit) -> Option<KeyType> {
    match edit {
        KeyEdit::StringSet { original: None, .. } => Some(KeyType::String),
        KeyEdit::JsonSet { original: None, .. } => Some(KeyType::Json),
        KeyEdit::HashSet { original: None, .. } => Some(KeyType::Hash),
        KeyEdit::ListPush { .. } => Some(KeyType::List),
        KeyEdit::SetAdd { .. } => Some(KeyType::Set),
        KeyEdit::ZsetAdd { .. } => Some(KeyType::Zset),
        KeyEdit::StreamAdd { .. } => Some(KeyType::Stream),
        _ => None,
    }
}

/// The type an edit works on; `None` for edits of any type.
fn edit_type(edit: &KeyEdit) -> Option<KeyType> {
    Some(match edit {
        KeyEdit::StringSet { .. } => KeyType::String,
        KeyEdit::JsonSet { .. } => KeyType::Json,
        KeyEdit::HashSet { .. } | KeyEdit::HashDelete { .. } => KeyType::Hash,
        KeyEdit::ListPush { .. } | KeyEdit::ListSet { .. } | KeyEdit::ListRemove { .. } => {
            KeyType::List
        }
        KeyEdit::SetAdd { .. } | KeyEdit::SetRemove { .. } => KeyType::Set,
        KeyEdit::ZsetAdd { .. } | KeyEdit::ZsetScore { .. } | KeyEdit::ZsetRemove { .. } => {
            KeyType::Zset
        }
        KeyEdit::StreamAdd { .. } | KeyEdit::StreamDelete { .. } | KeyEdit::StreamTrim { .. } => {
            KeyType::Stream
        }
        KeyEdit::Expire { .. } | KeyEdit::Persist | KeyEdit::Rename { .. } | KeyEdit::Delete => {
            return None;
        }
        KeyEdit::TsAdd { .. }
        | KeyEdit::TsDeleteRange { .. }
        | KeyEdit::TsAlter { .. }
        | KeyEdit::BloomAdd { .. }
        | KeyEdit::CuckooAdd { .. }
        | KeyEdit::CuckooDelete { .. }
        | KeyEdit::CmsIncrBy { .. }
        | KeyEdit::TopkAdd { .. }
        | KeyEdit::TdigestAdd { .. } => return None,
    })
}

/// The module key kind an edit works on; `None` for core edits.
fn edit_module(edit: &KeyEdit) -> Option<ModuleKind> {
    Some(match edit {
        KeyEdit::TsAdd { .. } | KeyEdit::TsDeleteRange { .. } | KeyEdit::TsAlter { .. } => {
            ModuleKind::TimeSeries
        }
        KeyEdit::BloomAdd { .. } => ModuleKind::Bloom,
        KeyEdit::CuckooAdd { .. } | KeyEdit::CuckooDelete { .. } => ModuleKind::Cuckoo,
        KeyEdit::CmsIncrBy { .. } => ModuleKind::CountMin,
        KeyEdit::TopkAdd { .. } => ModuleKind::TopK,
        KeyEdit::TdigestAdd { .. } => ModuleKind::TDigest,
        _ => return None,
    })
}

/// True for the changes checked against the key's whole version (length
/// and content digest).
fn whole_key(edit: &KeyEdit) -> bool {
    matches!(
        edit,
        KeyEdit::Delete
            | KeyEdit::Rename { .. }
            | KeyEdit::StreamTrim { .. }
            | KeyEdit::TsDeleteRange { .. }
    )
}

/// Checks a series' retention, duplicate policy and labels against what
/// was read (`ARGV[1]`, `ARGV[2]`, then the label count and the pairs) and
/// runs `TS.ALTER` with the rest of `ARGV`; `{0, "changed"}` when they
/// differ.
const TS_ALTER_SCRIPT: &str = "local function s(v)
  if type(v) == 'table' then return v.ok or v.err or '' end
  if v == false or v == nil then return '' end
  if type(v) == 'number' then return string.format('%d', v) end
  return v
end
local info = redis.call('TS.INFO', KEYS[1])
local ret, pol, labels = '', '', {}
for i = 1, #info, 2 do
  local k = s(info[i])
  if k == 'retentionTime' then ret = s(info[i + 1])
  elseif k == 'duplicatePolicy' then pol = string.lower(s(info[i + 1]))
  elseif k == 'labels' then labels = info[i + 1] end
end
local n = tonumber(ARGV[3])
local same = ret == ARGV[1] and pol == ARGV[2] and #labels == n
if same then
  for j = 1, n do
    local l = labels[j]
    if s(l[1]) ~= ARGV[2 + 2 * j] or s(l[2]) ~= ARGV[3 + 2 * j] then
      same = false
      break
    end
  end
end
if not same then return {0, 'changed'} end
local args = {}
for j = 4 + 2 * n, #ARGV do args[#args + 1] = ARGV[j] end
return {1, redis.call('TS.ALTER', KEYS[1], unpack(args))}
";

/// Most labels one `TS.ALTER` plan sets: the script unpacks its
/// arguments, and Lua unpacks at most about 8000 values.
pub const MAX_TS_LABELS: usize = 1000;

/// Duplicate policies of `TS.ALTER`.
const DUPLICATE_POLICIES: &[&str] = &["block", "first", "last", "min", "max", "sum"];

/// A sample value: a finite number or `nan`.
fn sample_value(value: &Value) -> Result<String, EditError> {
    let text = match value {
        Value::Float(f) if f.is_finite() => return Ok(format!("{f}")),
        Value::Int(i) => return Ok(i.to_string()),
        Value::Text(text) | Value::Other(text) => text.trim().to_string(),
        _ => String::new(),
    };
    if text.eq_ignore_ascii_case("nan") {
        return Ok("NaN".into());
    }
    match text.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(text),
        _ => Err(invalid("A sample value must be a finite number (or nan)")),
    }
}

/// A finite number argument (t-digest observations).
fn finite(value: &Value, what: &str) -> Result<String, EditError> {
    let text = match value {
        Value::Float(f) if f.is_finite() => return Ok(format!("{f}")),
        Value::Int(i) => return Ok(i.to_string()),
        Value::Text(text) | Value::Other(text) => text.trim().to_string(),
        _ => String::new(),
    };
    match text.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(text),
        _ => Err(invalid(format!("The {what} must be a finite number"))),
    }
}

/// A timestamp in milliseconds (0 to `i64::MAX`), or `end` (`-`, `+`).
fn ts_bound(text: &str, end: Option<&str>) -> Result<String, EditError> {
    let text = text.trim();
    if end == Some(text) {
        return Ok(text.to_string());
    }
    match text.parse::<u64>() {
        Ok(n) if i64::try_from(n).is_ok() => Ok(n.to_string()),
        _ => Err(invalid(match end {
            Some(end) => format!("A timestamp is a number of milliseconds, or {end}"),
            None => "A timestamp is a number of milliseconds, or *".to_string(),
        })),
    }
}

/// Commands in the order they run.
#[derive(Default)]
struct Commands {
    /// Value edits, in the order given.
    main: Vec<Vec<Vec<u8>>>,
    /// `LSET` of list elements (original indexes).
    list_sets: Vec<Vec<Vec<u8>>>,
    /// `LSET` of the elements to remove to the marker.
    list_marks: Vec<Vec<Vec<u8>>>,
    /// Pushes, after the index based commands.
    pushes: Vec<Vec<Vec<u8>>>,
    /// TTL changes, last.
    tail: Vec<Vec<Vec<u8>>>,
}

/// Builds a program from a request (no server involved). `risk` is filled
/// in by the caller.
fn build(
    key: &[u8],
    db: u16,
    expect: &KeyExpect,
    edits: &[KeyEdit],
    compat: (u32, u32),
) -> Result<EditProgram, EditError> {
    if edits.is_empty() {
        return Err(invalid("The plan has no changes"));
    }
    if edits.len() > MAX_EDITS {
        return Err(invalid(format!(
            "A plan holds at most {MAX_EDITS} changes; save in parts"
        )));
    }
    let whole = edits
        .iter()
        .any(|e| matches!(e, KeyEdit::Delete | KeyEdit::Rename { .. }));
    if whole && edits.len() > 1 {
        return Err(invalid(
            "A delete or rename must be the only change of its plan",
        ));
    }
    let (type_name, key_type, version) = match expect {
        KeyExpect::Absent => {
            let created: Vec<KeyType> = edits.iter().filter_map(created_type).collect();
            let Some(first) = created.first().cloned() else {
                return Err(invalid(
                    "A new key needs a change that creates it (a value, field, element, member \
                     or entry)",
                ));
            };
            for edit in edits {
                let fits = match edit {
                    KeyEdit::Expire { .. } => true,
                    other => created_type(other).as_ref() == Some(&first),
                };
                if !fits {
                    return Err(invalid(format!(
                        "A new {} key can only take changes that create it, plus a time to live; \
                         {} does not fit",
                        first.type_name(),
                        edit.op_name()
                    )));
                }
            }
            ("none".to_string(), first, None)
        }
        KeyExpect::Present { key_type, version } => {
            if matches!(key_type, KeyType::Unknown) {
                return Err(invalid(
                    "A key whose name could not be read cannot be edited",
                ));
            }
            if let Some(v) = version
                && v.key_type != *key_type
            {
                return Err(invalid("The version is of another key type"));
            }
            for edit in edits {
                let fits = match (edit_type(edit), edit_module(edit)) {
                    (Some(t), _) => t == *key_type,
                    (None, Some(kind)) => ModuleKind::of(key_type) == Some(kind),
                    (None, None) => true,
                };
                if !fits {
                    return Err(invalid(format!(
                        "{} does not fit a key of type {}",
                        edit.op_name(),
                        key_type.type_name()
                    )));
                }
                let needs_version =
                    whole_key(edit) || matches!(edit, KeyEdit::Expire { .. } | KeyEdit::Persist);
                if needs_version && version.is_none() {
                    return Err(invalid(format!(
                        "{} needs the key's version from when it was read",
                        edit.op_name()
                    )));
                }
                let creates_only = matches!(
                    edit,
                    KeyEdit::StringSet { original: None, .. }
                        | KeyEdit::JsonSet { original: None, .. }
                );
                if creates_only {
                    return Err(invalid(
                        "The key exists: send the value that was read as original",
                    ));
                }
            }
            (
                key_type.type_name().to_string(),
                key_type.clone(),
                version.clone(),
            )
        }
    };
    let creating = matches!(expect, KeyExpect::Absent);

    let mut commands = Commands::default();
    let mut checks: Vec<Check> = Vec::new();
    let mut descriptions: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut counts = EditCounts::default();
    let mut destructive = false;
    let mut watch = vec![key.to_vec()];
    let mut bytes: u64 = 0;
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut once = |what: &str, id: &[u8]| -> Result<(), EditError> {
        let mut tag = what.as_bytes().to_vec();
        tag.push(0);
        tag.extend_from_slice(id);
        if seen.insert(tag) {
            Ok(())
        } else {
            Err(invalid(format!(
                "The plan changes the {what} {} twice",
                short(id)
            )))
        }
    };
    let mut add_bytes = |n: usize| -> Result<(), EditError> {
        bytes = bytes.saturating_add(n as u64);
        if bytes > MAX_EDIT_BYTES {
            Err(invalid(format!(
                "The plan holds more than {} MiB of values; save in parts",
                MAX_EDIT_BYTES / (1024 * 1024)
            )))
        } else {
            Ok(())
        }
    };
    let marker = list_marker();
    let mut script: Option<ScriptCheck> = None;
    let mut assess: Vec<String> = Vec::new();
    let mut shown: Option<String> = None;
    let mut removes_list = false;
    let mut ttl_edits = 0;

    for (index, edit) in edits.iter().enumerate() {
        match edit {
            KeyEdit::StringSet { value, original } => {
                once("value", b"")?;
                let value = value_bytes(value, "value")?;
                add_bytes(value.len())?;
                match original {
                    Some(original) => {
                        let original = value_bytes(original, "value read")?;
                        checks.push(Check {
                            edit: index,
                            command: cmd(&[b"GET", key]),
                            expect: Expectation::Bulk(Some(value_digest(&original))),
                            reason: ConflictReason::ValueChanged,
                            what: String::new(),
                        });
                        descriptions.push("the value is still the one that was read".into());
                        commands
                            .main
                            .push(cmd(&[b"SET", key, value.as_slice(), b"KEEPTTL"]));
                        counts.changed += 1;
                    }
                    None => {
                        commands.main.push(cmd(&[b"SET", key, value.as_slice()]));
                        counts.added += 1;
                    }
                }
            }
            KeyEdit::JsonSet { document, original } => {
                once("document", b"")?;
                if serde_json::from_str::<serde_json::Value>(document).is_err() {
                    return Err(invalid("The document is not valid JSON"));
                }
                add_bytes(document.len())?;
                let mut command = cmd(&[b"JSON.SET", key, b"$", document.as_bytes()]);
                match original {
                    Some(original) => {
                        checks.push(Check {
                            edit: index,
                            command: cmd(&[b"JSON.GET", key]),
                            expect: Expectation::Bulk(Some(value_digest(original.as_bytes()))),
                            reason: ConflictReason::ValueChanged,
                            what: String::new(),
                        });
                        descriptions.push("the document is still the one that was read".into());
                        counts.changed += 1;
                    }
                    None => {
                        command.push(b"NX".to_vec());
                        counts.added += 1;
                    }
                }
                commands.main.push(command);
            }
            KeyEdit::HashSet {
                field,
                value,
                original,
            } => {
                let field = value_bytes(field, "field name")?;
                once("field", &field)?;
                let value = value_bytes(value, "value")?;
                add_bytes(field.len() + value.len())?;
                let original = original
                    .as_ref()
                    .map(|o| value_bytes(o, "value read"))
                    .transpose()?;
                if !creating {
                    descriptions.push(match &original {
                        Some(_) => {
                            format!("field {} still has the value that was read", short(&field))
                        }
                        None => format!("field {} does not exist yet", short(&field)),
                    });
                    checks.push(Check {
                        edit: index,
                        command: cmd(&[b"HGET", key, field.as_slice()]),
                        expect: Expectation::Bulk(original.as_deref().map(value_digest)),
                        reason: ConflictReason::ValueChanged,
                        what: String::new(),
                    });
                }
                if original.is_some() {
                    counts.changed += 1;
                } else {
                    counts.added += 1;
                }
                commands
                    .main
                    .push(cmd(&[b"HSET", key, field.as_slice(), value.as_slice()]));
            }
            KeyEdit::HashDelete { field, original } => {
                let field = value_bytes(field, "field name")?;
                once("field", &field)?;
                let original = value_bytes(original, "value read")?;
                add_bytes(field.len())?;
                descriptions.push(format!(
                    "field {} still has the value that was read",
                    short(&field)
                ));
                checks.push(Check {
                    edit: index,
                    command: cmd(&[b"HGET", key, field.as_slice()]),
                    expect: Expectation::Bulk(Some(value_digest(&original))),
                    reason: ConflictReason::ValueChanged,
                    what: String::new(),
                });
                commands.main.push(cmd(&[b"HDEL", key, field.as_slice()]));
                counts.removed += 1;
                destructive = true;
            }
            KeyEdit::ListPush { end, values } => {
                if values.is_empty() {
                    return Err(invalid("A push needs at least one element"));
                }
                let name: &[u8] = match end {
                    ListEnd::Head => b"LPUSH",
                    ListEnd::Tail => b"RPUSH",
                };
                let mut command = cmd(&[name, key]);
                for value in values {
                    let value = value_bytes(value, "element")?;
                    add_bytes(value.len())?;
                    command.push(value);
                }
                counts.added += values.len() as u64;
                commands.pushes.push(command);
            }
            KeyEdit::ListSet {
                index: at,
                original,
                ..
            }
            | KeyEdit::ListRemove {
                index: at,
                original,
            } => {
                once("element", at.to_string().as_bytes())?;
                if i64::try_from(*at).is_err() {
                    return Err(invalid("The list index is too large"));
                }
                let position = at.to_string();
                let original = value_bytes(original, "element read")?;
                descriptions.push(format!("element {position} is still the one that was read"));
                checks.push(Check {
                    edit: index,
                    command: cmd(&[b"LINDEX", key, position.as_bytes()]),
                    expect: Expectation::Bulk(Some(value_digest(&original))),
                    reason: ConflictReason::ValueChanged,
                    what: String::new(),
                });
                if let KeyEdit::ListSet { value, .. } = edit {
                    let value = value_bytes(value, "element")?;
                    add_bytes(value.len())?;
                    commands.list_sets.push(cmd(&[
                        b"LSET",
                        key,
                        position.as_bytes(),
                        value.as_slice(),
                    ]));
                    counts.changed += 1;
                } else {
                    commands.list_marks.push(cmd(&[
                        b"LSET",
                        key,
                        position.as_bytes(),
                        marker.as_slice(),
                    ]));
                    removes_list = true;
                    counts.removed += 1;
                    destructive = true;
                }
            }
            KeyEdit::SetAdd { members } => {
                if members.is_empty() {
                    return Err(invalid("An add needs at least one member"));
                }
                let mut command = cmd(&[b"SADD", key]);
                for member in members {
                    let member = value_bytes(member, "member")?;
                    once("member", &member)?;
                    add_bytes(member.len())?;
                    command.push(member);
                }
                counts.added += members.len() as u64;
                commands.main.push(command);
            }
            KeyEdit::SetRemove { member } => {
                let member = value_bytes(member, "member")?;
                once("member", &member)?;
                add_bytes(member.len())?;
                descriptions.push(format!("member {} still exists", short(&member)));
                checks.push(Check {
                    edit: index,
                    command: cmd(&[b"SISMEMBER", key, member.as_slice()]),
                    expect: Expectation::Int(1),
                    reason: ConflictReason::ValueChanged,
                    what: String::new(),
                });
                commands.main.push(cmd(&[b"SREM", key, member.as_slice()]));
                counts.removed += 1;
                destructive = true;
            }
            KeyEdit::ZsetAdd { member, score: s } => {
                let member = value_bytes(member, "member")?;
                once("member", &member)?;
                add_bytes(member.len())?;
                let (_, arg) = score(s, "score")?;
                if !creating {
                    descriptions.push(format!("member {} does not exist yet", short(&member)));
                    checks.push(Check {
                        edit: index,
                        command: cmd(&[b"ZSCORE", key, member.as_slice()]),
                        expect: Expectation::Score(None),
                        reason: ConflictReason::ValueChanged,
                        what: String::new(),
                    });
                }
                commands.main.push(cmd(&[
                    b"ZADD",
                    key,
                    b"NX",
                    arg.as_bytes(),
                    member.as_slice(),
                ]));
                counts.added += 1;
            }
            KeyEdit::ZsetScore {
                member,
                score: s,
                original,
            } => {
                let member = value_bytes(member, "member")?;
                once("member", &member)?;
                add_bytes(member.len())?;
                let (_, arg) = score(s, "score")?;
                let (was, _) = score(original, "score read")?;
                descriptions.push(format!(
                    "member {} still has the score {}",
                    short(&member),
                    score_arg(was)
                ));
                checks.push(Check {
                    edit: index,
                    command: cmd(&[b"ZSCORE", key, member.as_slice()]),
                    expect: Expectation::Score(Some(was)),
                    reason: ConflictReason::ValueChanged,
                    what: String::new(),
                });
                commands.main.push(cmd(&[
                    b"ZADD",
                    key,
                    b"XX",
                    arg.as_bytes(),
                    member.as_slice(),
                ]));
                counts.changed += 1;
            }
            KeyEdit::ZsetRemove { member, original } => {
                let member = value_bytes(member, "member")?;
                once("member", &member)?;
                add_bytes(member.len())?;
                let (was, _) = score(original, "score read")?;
                descriptions.push(format!(
                    "member {} still has the score {}",
                    short(&member),
                    score_arg(was)
                ));
                checks.push(Check {
                    edit: index,
                    command: cmd(&[b"ZSCORE", key, member.as_slice()]),
                    expect: Expectation::Score(Some(was)),
                    reason: ConflictReason::ValueChanged,
                    what: String::new(),
                });
                commands.main.push(cmd(&[b"ZREM", key, member.as_slice()]));
                counts.removed += 1;
                destructive = true;
            }
            KeyEdit::StreamAdd { id, fields } => {
                if fields.is_empty() {
                    return Err(invalid("A stream entry needs at least one field"));
                }
                let id = match id.as_deref().map(str::trim) {
                    None | Some("" | "*") => "*".to_string(),
                    Some(id) if valid_stream_id(id, true) => {
                        once("entry", id.as_bytes())?;
                        id.to_string()
                    }
                    Some(_) => {
                        return Err(invalid(
                            "A stream entry id is <milliseconds>-<sequence>, or * to let the \
                             server choose",
                        ));
                    }
                };
                let mut command = cmd(&[b"XADD", key]);
                if !creating {
                    // The stream must still exist: never recreate a
                    // stream someone deleted meanwhile.
                    command.push(b"NOMKSTREAM".to_vec());
                }
                command.push(id.into_bytes());
                for pair in fields {
                    let field = value_bytes(&pair.field, "field name")?;
                    let value = value_bytes(&pair.value, "value")?;
                    add_bytes(field.len() + value.len())?;
                    command.push(field);
                    command.push(value);
                }
                commands.main.push(command);
                counts.added += 1;
            }
            KeyEdit::StreamDelete { id } => {
                let id = id.trim();
                if !valid_stream_id(id, false) {
                    return Err(invalid("A stream entry id is <milliseconds>-<sequence>"));
                }
                once("entry", id.as_bytes())?;
                descriptions.push(format!("entry {id} still exists"));
                checks.push(Check {
                    edit: index,
                    command: cmd(&[b"XRANGE", key, id.as_bytes(), id.as_bytes()]),
                    expect: Expectation::NonEmpty,
                    reason: ConflictReason::ValueChanged,
                    what: String::new(),
                });
                commands.main.push(cmd(&[b"XDEL", key, id.as_bytes()]));
                counts.removed += 1;
                destructive = true;
            }
            KeyEdit::StreamTrim {
                strategy,
                threshold,
                approximate,
            } => {
                once("trim", b"")?;
                let threshold = threshold.trim();
                let (name, ok): (&[u8], bool) = match strategy {
                    TrimStrategy::MaxLen => (b"MAXLEN", threshold.parse::<u64>().is_ok()),
                    TrimStrategy::MinId => (b"MINID", valid_stream_id(threshold, true)),
                };
                if !ok {
                    return Err(invalid(match strategy {
                        TrimStrategy::MaxLen => "The most entries to keep must be a number",
                        TrimStrategy::MinId => "The lowest id to keep is <milliseconds>-<sequence>",
                    }));
                }
                let mut command = cmd(&[b"XTRIM", key, name]);
                if *approximate {
                    command.push(b"~".to_vec());
                }
                command.push(threshold.as_bytes().to_vec());
                commands.main.push(command);
                counts.removed += 1;
                destructive = true;
            }
            KeyEdit::Expire { ttl_ms } => {
                ttl_edits += 1;
                if *ttl_ms == 0 || i64::try_from(*ttl_ms).is_err() {
                    return Err(invalid(
                        "The time to live must be at least 1 ms (use delete to remove the key)",
                    ));
                }
                commands
                    .tail
                    .push(cmd(&[b"PEXPIRE", key, ttl_ms.to_string().as_bytes()]));
                counts.changed += 1;
            }
            KeyEdit::Persist => {
                ttl_edits += 1;
                if creating {
                    return Err(invalid("A new key has no time to live to remove"));
                }
                commands.tail.push(cmd(&[b"PERSIST", key]));
                counts.changed += 1;
            }
            KeyEdit::Rename { new_key, replace } => {
                let target =
                    crate::parse::key_from_display(new_key).map_err(|e| invalid(e.message))?;
                if target.is_empty() {
                    return Err(invalid("The new name is empty"));
                }
                if target == key {
                    return Err(invalid("The new name is the current name"));
                }
                if *replace {
                    destructive = true;
                    warnings.push(format!(
                        "A key named {} is overwritten if it exists",
                        short(&target)
                    ));
                } else {
                    descriptions.push(format!("no key named {} exists", short(&target)));
                    checks.push(Check {
                        edit: index,
                        command: cmd(&[b"EXISTS", target.as_slice()]),
                        expect: Expectation::Int(0),
                        reason: ConflictReason::TargetExists,
                        what: String::new(),
                    });
                }
                watch.push(target.clone());
                commands
                    .main
                    .push(cmd(&[b"RENAME", key, target.as_slice()]));
                counts.changed += 1;
            }
            KeyEdit::Delete => {
                commands.main.push(cmd(&[b"UNLINK", key]));
                counts.removed += 1;
                destructive = true;
            }
            KeyEdit::TsAdd { timestamp, value } => {
                let at = match timestamp.as_deref().map(str::trim) {
                    None | Some("" | "*") => "*".to_string(),
                    Some(text) => ts_bound(text, None)?,
                };
                once("sample", at.as_bytes())?;
                let value = sample_value(value)?;
                if at != "*" {
                    descriptions.push(format!("the series has no sample at {at} yet"));
                    checks.push(Check {
                        edit: index,
                        command: cmd(&[b"TS.RANGE", key, at.as_bytes(), at.as_bytes()]),
                        expect: Expectation::Empty,
                        reason: ConflictReason::ValueChanged,
                        what: format!("a sample at {at} exists now"),
                    });
                }
                commands
                    .main
                    .push(cmd(&[b"TS.ADD", key, at.as_bytes(), value.as_bytes()]));
                counts.added += 1;
            }
            KeyEdit::TsDeleteRange { from, to } => {
                let from = ts_bound(from, Some("-"))?;
                let to = ts_bound(to, Some("+"))?;
                if let (Ok(a), Ok(b)) = (from.parse::<u64>(), to.parse::<u64>())
                    && a > b
                {
                    return Err(invalid("The range starts after it ends"));
                }
                once("range", format!("{from} {to}").as_bytes())?;
                commands
                    .main
                    .push(cmd(&[b"TS.DEL", key, from.as_bytes(), to.as_bytes()]));
                counts.removed += 1;
                destructive = true;
            }
            KeyEdit::TsAlter {
                retention_ms,
                labels,
                duplicate_policy,
                original,
            } => {
                if edits.len() > 1 {
                    return Err(invalid(
                        "A change of the series settings must be the only change of its plan",
                    ));
                }
                if retention_ms.is_none() && labels.is_none() && duplicate_policy.is_none() {
                    return Err(invalid(
                        "Change the retention, the labels or the duplicate policy",
                    ));
                }
                let mut alter: Vec<Vec<u8>> = Vec::new();
                if let Some(retention) = retention_ms {
                    if i64::try_from(*retention).is_err() {
                        return Err(invalid("The retention is too large"));
                    }
                    alter.push(b"RETENTION".to_vec());
                    alter.push(retention.to_string().into_bytes());
                    let shrinks = *retention > 0
                        && (original.retention_ms == 0 || *retention < original.retention_ms);
                    if shrinks {
                        destructive = true;
                        warnings.push(format!(
                            "The retention goes down to {retention} ms: samples older than that, \
                             counted back from the newest sample, are removed"
                        ));
                    }
                }
                if let Some(policy) = duplicate_policy {
                    let policy = policy.trim().to_ascii_lowercase();
                    if !DUPLICATE_POLICIES.contains(&policy.as_str()) {
                        return Err(invalid(format!(
                            "The duplicate policy is one of {}",
                            DUPLICATE_POLICIES.join(", ")
                        )));
                    }
                    alter.push(b"DUPLICATE_POLICY".to_vec());
                    alter.push(policy.to_ascii_uppercase().into_bytes());
                }
                if let Some(labels) = labels {
                    if labels.len() > MAX_TS_LABELS {
                        return Err(invalid(format!(
                            "A series can get at most {MAX_TS_LABELS} labels here (the change \
                             runs as a Lua script, which takes a limited number of arguments); \
                             use TS.ALTER in the console for more"
                        )));
                    }
                    let mut names = HashSet::new();
                    alter.push(b"LABELS".to_vec());
                    for label in labels {
                        // Names and values are sent exactly as given (a
                        // trimmed value would silently rewrite the label).
                        let name = label.name.as_str();
                        if name.is_empty() || label.value.is_empty() {
                            return Err(invalid("A label needs a name and a value"));
                        }
                        if !names.insert(name.to_string()) {
                            return Err(invalid(format!("The label {name} is given twice")));
                        }
                        add_bytes(name.len() + label.value.len())?;
                        alter.push(name.as_bytes().to_vec());
                        alter.push(label.value.as_bytes().to_vec());
                    }
                }
                // TS.INFO counts as a change for a WATCH, so the check runs
                // in the script that alters the series.
                let mut plain = cmd(&[b"TS.ALTER", key]);
                plain.extend(alter.iter().cloned());
                let mut command = cmd(&[b"EVAL", TS_ALTER_SCRIPT.as_bytes(), b"1", key]);
                command.push(original.retention_ms.to_string().into_bytes());
                command.push(
                    original
                        .duplicate_policy
                        .clone()
                        .unwrap_or_default()
                        .into_bytes(),
                );
                command.push(original.labels.len().to_string().into_bytes());
                for label in &original.labels {
                    command.push(label.name.as_bytes().to_vec());
                    command.push(label.value.as_bytes().to_vec());
                }
                command.extend(alter);
                descriptions.push(
                    "the retention, labels and duplicate policy are still the ones that were \
                     read, checked by the same Lua script that changes them"
                        .into(),
                );
                script = Some(ScriptCheck {
                    gone: "The series no longer exists".into(),
                    changed: "The retention, labels or duplicate policy of the series changed \
                              since they were read"
                        .into(),
                    commands: "reads the series' settings (TS.INFO) and then changes them \
                               (TS.ALTER)"
                        .into(),
                });
                assess.push("TS.ALTER".to_string());
                shown = Some(format!(
                    "{} (in a Lua script, after checking the settings that were read)",
                    display_line(&plain)
                ));
                commands.main.push(command);
                counts.changed += 1;
            }
            KeyEdit::BloomAdd { items } | KeyEdit::TopkAdd { items } => {
                if items.is_empty() {
                    return Err(invalid("An add needs at least one item"));
                }
                let name: &[u8] = if matches!(edit, KeyEdit::BloomAdd { .. }) {
                    b"BF.MADD"
                } else {
                    warnings.push("Adding to a Top-K can push other items out of its list".into());
                    b"TOPK.ADD"
                };
                let mut command = cmd(&[name, key]);
                for item in items {
                    let item = value_bytes(item, "item")?;
                    add_bytes(item.len())?;
                    command.push(item);
                }
                counts.added += items.len() as u64;
                commands.main.push(command);
            }
            KeyEdit::CuckooAdd { items, nx } => {
                if items.is_empty() {
                    return Err(invalid("An add needs at least one item"));
                }
                let name: &[u8] = if *nx { b"CF.ADDNX" } else { b"CF.ADD" };
                for item in items {
                    let item = value_bytes(item, "item")?;
                    add_bytes(item.len())?;
                    commands.main.push(cmd(&[name, key, item.as_slice()]));
                }
                counts.added += items.len() as u64;
            }
            KeyEdit::CuckooDelete { item } => {
                let item = value_bytes(item, "item")?;
                once("item", &item)?;
                add_bytes(item.len())?;
                descriptions.push(format!("the filter still reports item {}", short(&item)));
                checks.push(Check {
                    edit: index,
                    command: cmd(&[b"CF.EXISTS", key, item.as_slice()]),
                    expect: Expectation::Int(1),
                    reason: ConflictReason::ValueChanged,
                    what: String::new(),
                });
                commands.main.push(cmd(&[b"CF.DEL", key, item.as_slice()]));
                counts.removed += 1;
                destructive = true;
            }
            KeyEdit::CmsIncrBy { items } => {
                if items.is_empty() {
                    return Err(invalid("An increment needs at least one item"));
                }
                let mut command = cmd(&[b"CMS.INCRBY", key]);
                for entry in items {
                    let item = value_bytes(&entry.item, "item")?;
                    if entry.increment == 0 || i64::try_from(entry.increment).is_err() {
                        return Err(invalid("An increment is a whole number of at least 1"));
                    }
                    add_bytes(item.len())?;
                    command.push(item);
                    command.push(entry.increment.to_string().into_bytes());
                }
                counts.changed += items.len() as u64;
                commands.main.push(command);
            }
            KeyEdit::TdigestAdd { values } => {
                if values.is_empty() {
                    return Err(invalid("An add needs at least one value"));
                }
                let mut command = cmd(&[b"TDIGEST.ADD", key]);
                for value in values {
                    let value = finite(value, "value")?;
                    add_bytes(value.len())?;
                    command.push(value.into_bytes());
                }
                counts.added += values.len() as u64;
                commands.main.push(command);
            }
        }
    }
    if ttl_edits > 1 {
        return Err(invalid("A plan changes the time to live at most once"));
    }
    let json = matches!(key_type, KeyType::Json);

    let mut ordered = commands.main;
    ordered.extend(commands.list_sets);
    ordered.extend(commands.list_marks);
    if removes_list {
        ordered.push(cmd(&[b"LREM", key, b"0", marker.as_slice()]));
    }
    ordered.extend(commands.pushes);
    ordered.extend(commands.tail);

    let mut check_lines = vec![match expect {
        KeyExpect::Absent => "the key does not exist".to_string(),
        KeyExpect::Present { key_type, .. } => {
            format!("the key still exists and is a {}", key_type.type_name())
        }
    }];
    let whole = edits.iter().any(whole_key);
    let ttl = edits
        .iter()
        .any(|e| matches!(e, KeyEdit::Expire { .. } | KeyEdit::Persist));
    let mut unchecked = false;
    if let Some(version) = version.as_ref().filter(|_| whole) {
        if version.digest.is_some() {
            check_lines.push(
                "the key is unchanged since it was read (length and content digest, read again \
                 inside the WATCH)"
                    .into(),
            );
        } else {
            unchecked = true;
            let why = match version.digest_missing {
                _ if version.unchecked_reason.is_some() => {
                    version.unchecked_reason.as_deref().unwrap_or_default()
                }
                Some(DigestMissing::NotPermitted) => {
                    "this user may not read the key in pieces (GETRANGE, LRANGE, XRANGE, HSCAN, \
                     SSCAN, ZSCAN or DUMP is not permitted)"
                }
                Some(DigestMissing::Deferred) => {
                    "the key is too large to read when it is opened, and reading it in the \
                     background had not finished when this plan was made"
                }
                Some(DigestMissing::Stalled) => {
                    "the server did not answer in time while the key was read in the background"
                }
                _ => {
                    "the key is too large to read within the time limit (JSON and module keys \
                     are read only when they are small, since DUMP blocks the server)"
                }
            };
            check_lines.push("the key has the length it had when it was read".into());
            warnings.push(format!(
                "The key cannot be checked for changes made since it was read: {why}. A change \
                 that kept its length would be lost. Confirm to apply without that check."
            ));
        }
    }
    if ttl && version.is_some() {
        check_lines.push("the key's time to live is the one that was read".into());
    }
    check_lines.extend(descriptions);
    check_lines
        .push("nobody changes the key between these checks and the transaction (WATCH)".into());

    let view = RedisEditPlan {
        key: key_display(key),
        db,
        commands: match shown {
            Some(line) => vec![line],
            None => ordered.iter().map(|c| display_line(c)).collect(),
        },
        checks: check_lines,
        risk: CommandRisk::Write,
        destructive,
        unchecked,
        warnings,
    };
    Ok(EditProgram {
        key: key.to_vec(),
        db,
        watch,
        type_names: vec![type_name],
        version,
        check_content: whole,
        check_expiry: ttl,
        exact_expiry: compat >= (7, 0),
        script,
        assess,
        checks,
        commands: ordered,
        operations: edits.iter().map(KeyEdit::op_name).collect(),
        counts,
        view,
        json,
    })
}

/// The length command of a type.
fn length_command(key_type: &KeyType) -> Option<&'static [u8]> {
    Some(match key_type {
        KeyType::String => b"STRLEN",
        KeyType::Hash => b"HLEN",
        KeyType::List => b"LLEN",
        KeyType::Set => b"SCARD",
        KeyType::Zset => b"ZCARD",
        KeyType::Stream => b"XLEN",
        _ => return None,
    })
}

/// An integer reply, `None` for nil or a server error (the error line is
/// read, so the connection stays in step).
async fn int_or_none<S: tokio::io::AsyncRead + Unpin>(
    raw: &mut RawConn<S>,
) -> Result<Option<i64>, DriverError> {
    match raw.head().await? {
        Head::Int(n) => Ok(Some(n)),
        Head::Error(_) | Head::Bulk(None) | Head::Array(None) => Ok(None),
        Head::Bulk(Some(len)) => {
            let text = raw.keep(len.min(64)).await;
            Ok(text
                .ok()
                .and_then(|t| String::from_utf8(t).ok())
                .and_then(|t| t.parse().ok()))
        }
        other => Err(measure::unexpected(&format!("{other:?}"))),
    }
}

/// How much of a key's content a version read takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DigestMode {
    /// No digest.
    Skip,
    /// The bounded digest taken when a key is opened; a larger key is
    /// [`DigestMissing::Deferred`]. Can leave the connection in the
    /// middle of a reply, so the connection is closed afterwards.
    Open,
    /// The full digest (plans and applies).
    Full,
}

/// Reads the version of `key`; `None` when it does not exist.
async fn read_version_parts<S>(
    raw: &mut RawConn<S>,
    key: &[u8],
    exact_expiry: bool,
    mode: DigestMode,
) -> Result<Option<KeyVersion>, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use crate::digest::{Budget, Digest, content_digest};
    raw.send(&[b"TYPE", key]).await?;
    let type_name = match raw.next().await? {
        Head::Simple(name) => name,
        other => return Err(measure::unexpected(&format!("{other:?}"))),
    };
    let Some(key_type) = KeyType::from_type_name(&type_name) else {
        return Ok(None);
    };
    let length = match length_command(&key_type) {
        Some(name) => {
            raw.send(&[name, key]).await?;
            int_or_none(raw).await?.and_then(|n| u64::try_from(n).ok())
        }
        None if ModuleKind::of(&key_type) == Some(ModuleKind::TimeSeries) => {
            crate::digest::ts_length(raw, key).await?
        }
        None => None,
    };
    let expire_at_ms = crate::digest::expire_at(raw, key, exact_expiry).await?;
    let budget = match mode {
        DigestMode::Skip => None,
        DigestMode::Open => Some(Budget::open()),
        DigestMode::Full => Some(Budget::full()),
    };
    let (digest, digest_missing) = match budget {
        None => (None, None),
        Some(budget) => match content_digest(raw, key, &key_type, length, budget).await? {
            Digest::Done(digest) => (Some(digest), None),
            Digest::TooLarge | Digest::Module if mode == DigestMode::Open => {
                (None, Some(DigestMissing::Deferred))
            }
            Digest::Module => (None, Some(DigestMissing::TooLarge)),
            Digest::TooLarge => (None, Some(DigestMissing::TooLarge)),
            Digest::NotPermitted => (None, Some(DigestMissing::NotPermitted)),
        },
    };
    Ok(Some(KeyVersion {
        key_type,
        length,
        digest,
        digest_missing,
        expire_at_ms,
        digest_read: None,
        db: None,
        unchecked_reason: None,
    }))
}

/// Reads a bulk string reply and tells whether it has the expected length
/// and digest (`None`: nil). A value of another length is dropped unread;
/// one of the same length is hashed in pieces, never held.
async fn bulk_digest_matches<S: tokio::io::AsyncRead + Unpin>(
    raw: &mut RawConn<S>,
    expected: Option<(u64, u64)>,
) -> Result<bool, DriverError> {
    use std::hash::Hasher;
    match (raw.next().await?, expected) {
        (Head::Bulk(None) | Head::Array(None), None) => Ok(true),
        (Head::Bulk(None) | Head::Array(None), Some(_)) => Ok(false),
        (Head::Bulk(Some(len)), Some((want_len, want_hash))) if len == want_len => {
            let mut h = std::hash::DefaultHasher::new();
            raw.hash_body(len, &mut h).await?;
            Ok(h.finish() == want_hash)
        }
        (Head::Bulk(Some(len)), _) => {
            raw.skip(len).await?;
            Ok(false)
        }
        (other, _) => Err(measure::unexpected(&format!("{other:?}"))),
    }
}

/// Tells whether a stored and a current expiry are the same.
fn same_expiry(expected: Option<i64>, current: Option<i64>, exact: bool) -> bool {
    match (expected, current) {
        (None, None) => true,
        (Some(a), Some(b)) if exact => a == b,
        (Some(a), Some(b)) => (a - b).abs() <= TTL_TOLERANCE_MS,
        _ => false,
    }
}

/// One reply as a [`Value`]: strings over [`REPLY_KEEP`] become
/// [`Value::Other`] with their size, nested arrays are kept while `budget`
/// lasts (then summarized). An error reply at the top is `Err`; nested
/// errors become [`Value::Other`].
async fn read_bounded<S: tokio::io::AsyncRead + Unpin>(
    raw: &mut RawConn<S>,
    budget: &mut usize,
) -> Result<Result<Value, DriverError>, DriverError> {
    // Open arrays: items so far and items still to come.
    let mut open: Vec<(Vec<Value>, u64)> = Vec::new();
    loop {
        let mut value = match raw.head().await? {
            Head::Error(line) => {
                if open.is_empty() {
                    return Ok(Err(measure::server_error(&line)));
                }
                Value::Other(line)
            }
            Head::Int(n) => Value::Int(n),
            Head::Simple(text) => Value::Text(text),
            Head::Bulk(None) | Head::Array(None) => Value::Null,
            Head::Bulk(Some(len)) if len <= REPLY_KEEP => bytes_value(raw.keep(len).await?),
            Head::Bulk(Some(len)) => {
                raw.skip(len).await?;
                Value::Other(format!("{len} bytes"))
            }
            Head::Array(Some(0)) => Value::Array(Vec::new()),
            Head::Array(Some(n)) if usize::try_from(n).is_ok_and(|n| n <= *budget) => {
                *budget -= usize::try_from(n).unwrap_or(0);
                open.push((Vec::new(), n));
                continue;
            }
            Head::Array(Some(n)) => {
                for _ in 0..n {
                    raw.skip_value().await?;
                }
                Value::Other(format!("{n} elements"))
            }
        };
        loop {
            let Some((items, left)) = open.last_mut() else {
                return Ok(Ok(value));
            };
            items.push(value);
            *left -= 1;
            if *left > 0 {
                break;
            }
            let Some((items, _)) = open.pop() else {
                return Err(measure::unexpected("array state"));
            };
            value = Value::Array(items);
        }
    }
}

/// Reads an `XPENDING key group` summary and tells whether it matches.
/// An error reply (`NOGROUP`) does not.
async fn read_pending_check<S: tokio::io::AsyncRead + Unpin>(
    raw: &mut RawConn<S>,
    total: Option<u64>,
    consumer: Option<&(Vec<u8>, u64)>,
) -> Result<bool, DriverError> {
    let summary = match raw.head().await? {
        Head::Error(_) => return Ok(false),
        Head::Array(Some(n)) => crate::stream_groups::read_pending_summary_body(raw, n).await?,
        other => return Err(measure::unexpected(&format!("{other:?}"))),
    };
    let total_ok = total.is_none_or(|t| summary.count == t);
    let consumer_ok = consumer.is_none_or(|(name, want)| {
        let have = summary
            .consumers
            .iter()
            .find(|(n, _)| n == name)
            .map_or(0, |(_, c)| *c);
        have == *want
    });
    Ok(total_ok && consumer_ok)
}

/// How the transaction ended, when it was sent.
enum Ran {
    Conflict(Option<usize>, ConflictReason, String),
    Done(EditOutcome),
    /// The server refused a command of a check-and-change script before
    /// it changed anything.
    Refused(DriverError),
}

/// True for the error of a command an ACL user may not run, as a script
/// reports it (`NOPERM`, or `ERR ACL failure in script` on Redis 7 and
/// Valkey, "can't run this command" on Redis 6.2).
fn acl_refusal(error: &DriverError) -> bool {
    error.code.as_deref() == Some("NOPERM")
        || [
            "ACL failure",
            "no permissions to run",
            "can't run this command",
        ]
        .iter()
        .any(|text| error.message.contains(text))
}

fn conflict(edit: Option<usize>, reason: ConflictReason, message: impl Into<String>) -> Ran {
    Ran::Conflict(edit, reason, message.into())
}

/// Runs a program on `raw`: `WATCH`, checks, then the transaction.
async fn run<S>(
    raw: &mut RawConn<S>,
    program: &EditProgram,
    exec_sent: &AtomicBool,
) -> Result<Ran, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut watch: Vec<&[u8]> = vec![b"WATCH"];
    watch.extend(program.watch.iter().map(Vec::as_slice));
    raw.expect_ok(&watch).await?;

    // The key itself.
    let key = &program.key;
    let version = program
        .version
        .as_ref()
        .filter(|_| program.check_content || program.check_expiry);
    match version {
        Some(expected) => {
            let with_digest = program.check_content && expected.digest.is_some();
            let mode = if with_digest {
                DigestMode::Full
            } else {
                DigestMode::Skip
            };
            let current = read_version_parts(raw, key, program.exact_expiry, mode).await?;
            match current {
                None => {
                    return Ok(conflict(
                        None,
                        ConflictReason::TypeChanged,
                        "The key no longer exists",
                    ));
                }
                Some(current) if current.key_type != expected.key_type => {
                    return Ok(conflict(
                        None,
                        ConflictReason::TypeChanged,
                        format!(
                            "The key is now a {}, not a {}",
                            current.key_type.type_name(),
                            expected.key_type.type_name()
                        ),
                    ));
                }
                Some(current) => {
                    if program.check_content && current.length != expected.length {
                        return Ok(conflict(
                            None,
                            ConflictReason::VersionChanged,
                            "The key changed since it was read (its length differs)",
                        ));
                    }
                    if with_digest && current.digest != expected.digest {
                        let message = match current.digest_missing {
                            Some(_) => {
                                "The key could not be checked again for changes (it grew too \
                                 large to read in time); read it again"
                            }
                            None => "The key changed since it was read",
                        };
                        return Ok(conflict(None, ConflictReason::VersionChanged, message));
                    }
                    if program.check_expiry
                        && !same_expiry(
                            expected.expire_at_ms,
                            current.expire_at_ms,
                            program.exact_expiry,
                        )
                    {
                        return Ok(conflict(
                            None,
                            ConflictReason::VersionChanged,
                            "The key's time to live changed since it was read",
                        ));
                    }
                }
            }
        }
        None => {
            raw.send(&[b"TYPE", key]).await?;
            let current = match raw.next().await? {
                Head::Simple(name) => name,
                other => return Err(measure::unexpected(&format!("{other:?}"))),
            };
            if !program.type_names.contains(&current) {
                let expected = program.type_names.first().map_or("", String::as_str);
                let message = match (current.as_str(), expected) {
                    ("none", _) => "The key no longer exists".to_string(),
                    (_, "none") => "A key of that name was created meanwhile".to_string(),
                    (now, was) => format!("The key is now a {now}, not a {was}"),
                };
                return Ok(conflict(None, ConflictReason::TypeChanged, message));
            }
        }
    }

    // Every check in one round trip.
    if !program.checks.is_empty() {
        let mut out = Vec::new();
        for check in &program.checks {
            let args: Vec<&[u8]> = check.command.iter().map(Vec::as_slice).collect();
            encode(&args, &mut out);
        }
        raw.send_raw(&out).await?;
        for check in &program.checks {
            let holds = match &check.expect {
                Expectation::Bulk(expected) => bulk_digest_matches(raw, *expected).await?,
                Expectation::Int(n) => raw.int_reply().await? == *n,
                Expectation::Score(expected) => {
                    let current = match raw.next().await? {
                        Head::Bulk(None) => None,
                        Head::Bulk(Some(len)) if len <= 512 => {
                            let text = raw.keep(len).await?;
                            parse_score(&String::from_utf8_lossy(&text))
                        }
                        other => return Err(measure::unexpected(&format!("{other:?}"))),
                    };
                    current == *expected
                }
                Expectation::NonEmpty => match raw.head().await? {
                    Head::Error(_) => false,
                    Head::Array(Some(n)) => {
                        for _ in 0..n {
                            raw.skip_value().await?;
                        }
                        n > 0
                    }
                    Head::Array(None) => false,
                    other => return Err(measure::unexpected(&format!("{other:?}"))),
                },
                Expectation::NoGroup => match raw.head().await? {
                    Head::Error(line) => line.starts_with("NOGROUP"),
                    Head::Array(Some(n)) => {
                        for _ in 0..n {
                            raw.skip_value().await?;
                        }
                        false
                    }
                    other => return Err(measure::unexpected(&format!("{other:?}"))),
                },
                Expectation::Pending { total, consumer } => {
                    read_pending_check(raw, *total, consumer.as_ref()).await?
                }
                Expectation::Empty => match raw.head().await? {
                    Head::Error(_) => false,
                    Head::Array(Some(n)) => {
                        for _ in 0..n {
                            raw.skip_value().await?;
                        }
                        n == 0
                    }
                    Head::Array(None) => true,
                    other => return Err(measure::unexpected(&format!("{other:?}"))),
                },
            };
            if !holds {
                let message = match check.reason {
                    ConflictReason::TargetExists => {
                        "A key with the new name exists now".to_string()
                    }
                    _ if !check.what.is_empty() => {
                        let mut what = check.what.clone();
                        if let Some(first) = what.get_mut(..1) {
                            first.make_ascii_uppercase();
                        }
                        what
                    }
                    _ => format!(
                        "Change {} no longer applies: {}",
                        check.edit + 1,
                        match &check.expect {
                            Expectation::Score(None) | Expectation::Bulk(None) => "it exists now",
                            Expectation::NonEmpty | Expectation::Int(_) => "it no longer exists",
                            _ => "the value changed since it was read",
                        }
                    ),
                };
                return Ok(conflict(Some(check.edit), check.reason, message));
            }
        }
    }

    // The transaction.
    let mut out = Vec::new();
    encode(&[b"MULTI"], &mut out);
    for command in &program.commands {
        let args: Vec<&[u8]> = command.iter().map(Vec::as_slice).collect();
        encode(&args, &mut out);
    }
    encode(&[b"EXEC"], &mut out);
    exec_sent.store(true, Ordering::SeqCst);
    raw.send_raw(&out).await?;
    raw.ok_reply().await?;
    let mut queue_error = None;
    for index in 0..program.commands.len() {
        match raw.head().await? {
            Head::Simple(_) => {}
            Head::Error(line) => {
                if queue_error.is_none() {
                    let mut error = measure::server_error(&line);
                    error.detail =
                        Some(format!("Command {} was refused, so nothing ran", index + 1));
                    queue_error = Some(error);
                }
            }
            other => return Err(measure::unexpected(&format!("{other:?}"))),
        }
    }
    let elements = match raw.head().await? {
        Head::Array(None) => {
            return Ok(conflict(
                None,
                ConflictReason::ConcurrentChange,
                "The key changed while the change was being checked; nothing ran",
            ));
        }
        Head::Array(Some(n)) => n,
        Head::Error(line) => {
            // EXECABORT: a command was refused while queued; nothing ran.
            let mut error = queue_error.unwrap_or_else(|| measure::server_error(&line));
            if program.script.is_some() && error.code.as_deref() == Some("NOPERM") {
                error.hint = Some(
                    "This change runs as a Lua script so that its check and the change happen \
                     at once, and this user may not run scripts (EVAL)"
                        .into(),
                );
            }
            return Err(error);
        }
        other => return Err(measure::unexpected(&format!("{other:?}"))),
    };
    let mut replies = Vec::new();
    let mut failed = None;
    let mut budget = REPLY_VALUES;
    for index in 0..elements {
        let value = match read_bounded(raw, &mut budget).await? {
            Err(error) => {
                if failed.is_none() {
                    failed = Some((index, error));
                }
                Value::Null
            }
            Ok(value) => value,
        };
        replies.push(value);
    }
    if let Some(script) = &program.script {
        match (replies.first(), &failed) {
            (Some(Value::Array(parts)), None) if parts.first() == Some(&Value::Int(0)) => {
                let now = match parts.get(1) {
                    Some(Value::Text(text)) => text.clone(),
                    Some(Value::Int(n)) => n.to_string(),
                    _ => String::new(),
                };
                let message = if now == "gone" {
                    script.gone.clone()
                } else {
                    script.changed.replace("{now}", &now)
                };
                return Ok(conflict(Some(0), ConflictReason::ValueChanged, message));
            }
            (_, Some((_, error))) if error.code.as_deref() == Some("NOGROUP") => {
                return Ok(conflict(
                    Some(0),
                    ConflictReason::ValueChanged,
                    script.gone.clone(),
                ));
            }
            // The script stops at the first refused command; its only
            // write (XGROUP, TS.ALTER) is its last command, so nothing
            // changed.
            (_, Some((_, error))) if acl_refusal(error) => {
                let mut refused =
                    DriverError::new(ErrorKind::Query, error.message.clone()).with_code("NOPERM");
                refused.hint = Some(format!(
                    "This change runs as a Lua script that {}, and this user may not run one of \
                     these commands",
                    script.commands
                ));
                refused.detail = Some("Nothing was changed".into());
                return Ok(Ran::Refused(refused));
            }
            _ => {}
        }
    }
    Ok(Ran::Done(match failed {
        Some((index, error)) => EditOutcome::Failed {
            command: usize::try_from(index).unwrap_or(usize::MAX),
            error,
        },
        None => EditOutcome::Applied { replies },
    }))
}

impl RedisSession {
    /// The version of a key, to send back with whole-key and time to live
    /// changes; `None` when it does not exist. See
    /// [`VersionReader::read_open`]: bounded, on its own connection, and
    /// without holding the session (no other call waits for it).
    ///
    /// # Errors
    ///
    /// Connection errors, Timeout.
    pub async fn key_version(&self, key: &[u8]) -> Result<Option<KeyVersion>, DriverError> {
        self.version_reader().await.read_open(key).await
    }

    /// A reader of key versions which works without the session (see
    /// [`VersionReader`]), on the database the session is on now.
    pub async fn version_reader(&self) -> VersionReader {
        VersionReader {
            endpoint: self.endpoint().clone(),
            init: self.init_commands().to_vec(),
            db: self.current_db().await,
            exact: self.compat_version() >= (7, 0),
        }
    }

    /// Plans changes to one key (see the module documentation). The plan
    /// applies to the database the session is on now.
    ///
    /// # Errors
    ///
    /// [`EditError::Invalid`] for a request that does not fit the rules,
    /// [`EditError::Driver`] with [`ErrorKind::ReadOnly`] on a read-only
    /// session, or connection errors while classifying the commands.
    pub async fn plan_edit(
        &self,
        key: &[u8],
        expect: &KeyExpect,
        edits: &[KeyEdit],
    ) -> Result<EditProgram, EditError> {
        if self.is_read_only() {
            return Err(read_only().into());
        }
        if key.is_empty() {
            return Err(invalid("The key name is empty"));
        }
        let db = self.current_db().await;
        if let KeyExpect::Present {
            version: Some(KeyVersion {
                db: Some(read_on), ..
            }),
            ..
        } = expect
            && *read_on != db
        {
            return Err(invalid(format!(
                "The key was read on database {read_on}, but this tab's session is on database \
                 {db} now (a SELECT in the console); switch back to database {read_on} or read \
                 the key again"
            )));
        }
        let mut program = build(key, db, expect, edits, self.compat_version())?;
        self.finish_plan(&mut program).await?;
        if program.json && self.has_json_commands().await? != Some(true) {
            program.view.warnings.push(JSON_UNAVAILABLE.into());
        }
        Ok(program)
    }

    /// Fills in the risk of a program's commands from the server's
    /// classification (`COMMAND INFO`, with the subcommand of `XGROUP`).
    pub(crate) async fn finish_plan(&self, program: &mut EditProgram) -> Result<(), DriverError> {
        let mut risk = CommandRisk::Read;
        let mut seen = HashSet::new();
        for line in program.assess.clone() {
            if seen.insert(line.clone()) {
                risk = risk.max(self.command_risk(&line).await?);
            }
        }
        let scripted = !program.assess.is_empty();
        for command in program.commands.iter().filter(|_| !scripted) {
            let mut words = command
                .iter()
                .take(2)
                .map(|w| String::from_utf8_lossy(w).to_ascii_uppercase());
            let Some(name) = words.next() else { continue };
            let line = match words.next() {
                Some(sub) if name == "XGROUP" => format!("{name} {sub}"),
                _ => name,
            };
            if seen.insert(line.clone()) {
                risk = risk.max(self.command_risk(&line).await?);
            }
        }
        program.view.risk = risk;
        Ok(())
    }

    /// Applies a planned edit: checks, then one `MULTI`/`EXEC`, on its own
    /// side connection under the session's lock, cancel and statement
    /// timeout. A conflict comes back as [`EditOutcome::Conflict`] (nothing
    /// ran).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::ReadOnly`] on a read-only session; connection errors,
    /// Cancelled, Timeout (when `EXEC` was already sent, the error's
    /// `detail` says the change may have been applied); a command the
    /// server refused while queueing (nothing ran); `NOPERM` with a hint
    /// when an ACL user may not run a command of a consumer group script
    /// (nothing changed).
    pub async fn apply_edit(&self, program: &EditProgram) -> Result<EditOutcome, DriverError> {
        if self.is_read_only() {
            return Err(read_only());
        }
        let call = self.begin().await?;
        let exec_sent = AtomicBool::new(false);
        let (endpoint, init) = (call.endpoint(), call.init());
        let work = async {
            let mut raw = measure::open_named(endpoint, program.db, init, EDIT_NAME_SUFFIX).await?;
            run(&mut raw, program, &exec_sent).await
        };
        match call.watch(work).await {
            Ok(Ran::Done(outcome)) => Ok(outcome),
            Ok(Ran::Refused(error)) => Err(error),
            Ok(Ran::Conflict(edit, reason, message)) => Ok(EditOutcome::Conflict {
                edit,
                reason,
                message,
            }),
            Err(mut error) => {
                if exec_sent.load(Ordering::SeqCst) {
                    error.detail = Some(
                        "The transaction was sent before this happened, so the change may have \
                         been applied; read the key again"
                            .into(),
                    );
                }
                Err(error)
            }
        }
    }
}

/// What [`VersionReader::read_full`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FullDigest {
    /// The digest of the content; the key still has the type and length it
    /// was opened with.
    Done(String),
    /// No digest, and why ([`DigestMissing::TooLarge`],
    /// [`DigestMissing::NotPermitted`] or [`DigestMissing::Stalled`]).
    Missing(DigestMissing),
    /// The key no longer has the type or length it was opened with (or is
    /// gone): it changed since it was read. The text says how.
    Changed(String),
}

/// Reads key versions on short-lived connections of its own (set up like
/// the session's, client name `<name>-measure`), so the session is never
/// held for them: the bounded read when a key is opened
/// ([`Self::read_open`]) and the full digest read right after
/// ([`Self::read_full`]). Get one with [`RedisSession::version_reader`].
#[derive(Clone)]
pub struct VersionReader {
    endpoint: crate::conn::Endpoint,
    init: Vec<crate::parse::ParsedCommand>,
    db: u16,
    exact: bool,
}

impl std::fmt::Debug for VersionReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionReader")
            .field("endpoint", &self.endpoint.display())
            .field("db", &self.db)
            .finish_non_exhaustive()
    }
}

impl VersionReader {
    /// The database it reads on.
    #[must_use]
    pub fn db(&self) -> u16 {
        self.db
    }

    /// The version of `key` as the value viewer opens it: type, length,
    /// expiry, and the content digest only of a small key
    /// ([`crate::OPEN_DIGEST_BYTES`], [`crate::OPEN_DIGEST_ELEMENTS`],
    /// [`crate::OPEN_DIGEST_TIME`]; `DUMP` only of a JSON document that
    /// [`crate::OPEN_JSON_ELEMENTS`] shows small); otherwise
    /// [`DigestMissing::Deferred`]. `None` when the key does not exist.
    ///
    /// # Errors
    ///
    /// Connection errors, Timeout (the connect timeout plus twice the open
    /// budget).
    pub async fn read_open(&self, key: &[u8]) -> Result<Option<KeyVersion>, DriverError> {
        let limit = self.endpoint.connect_timeout() + crate::OPEN_DIGEST_TIME * 2;
        let work = async {
            let mut raw =
                measure::open_named(&self.endpoint, self.db, &self.init, SIDE_NAME_SUFFIX).await?;
            // The bounded digest may stop inside a reply: `raw` is dropped.
            let version = read_version_parts(&mut raw, key, self.exact, DigestMode::Open).await?;
            Ok(version.map(|v| KeyVersion {
                db: Some(self.db),
                ..v
            }))
        };
        tokio::time::timeout(limit, work).await.map_err(|_| {
            DriverError::new(
                ErrorKind::Timeout,
                format!(
                    "Reading the key's version took longer than {:.1} s",
                    limit.as_secs_f32()
                ),
            )
        })?
    }

    /// The time [`Self::read_full`] should get: the connect timeout plus
    /// twice [`crate::DIGEST_TIME_BUDGET`] (the budget is checked between
    /// pieces, so a piece in flight may run past it).
    #[must_use]
    pub fn full_read_limit(&self) -> std::time::Duration {
        self.endpoint.connect_timeout() + crate::DIGEST_TIME_BUDGET * 2
    }

    /// Reads the full digest of `key`, which was opened as `opened`, on its
    /// own connection. The digest stops after
    /// [`crate::DIGEST_TIME_BUDGET`] ([`DigestMissing::TooLarge`]); the
    /// whole read, connecting included, is cut off after `limit`
    /// ([`DigestMissing::Stalled`]).
    ///
    /// # Errors
    ///
    /// Connection errors (refused, lost).
    pub async fn read_full(
        &self,
        key: &[u8],
        opened: &KeyVersion,
        limit: std::time::Duration,
    ) -> Result<FullDigest, DriverError> {
        let work = async {
            let mut raw =
                measure::open_named(&self.endpoint, self.db, &self.init, SIDE_NAME_SUFFIX).await?;
            read_version_parts(&mut raw, key, self.exact, DigestMode::Full).await
        };
        let Ok(current) = tokio::time::timeout(limit, work).await else {
            return Ok(FullDigest::Missing(DigestMissing::Stalled));
        };
        let current = match current {
            Ok(current) => current,
            // Connecting timed out: a stall too.
            Err(error) if error.kind == ErrorKind::Timeout => {
                return Ok(FullDigest::Missing(DigestMissing::Stalled));
            }
            Err(error) => return Err(error),
        };
        let Some(current) = current else {
            return Ok(FullDigest::Changed("The key no longer exists".into()));
        };
        if current.key_type != opened.key_type {
            return Ok(FullDigest::Changed(format!(
                "The key is now a {}, not a {}",
                current.key_type.type_name(),
                opened.key_type.type_name()
            )));
        }
        if current.length != opened.length {
            return Ok(FullDigest::Changed(
                "The key changed since it was read (its length differs)".into(),
            ));
        }
        Ok(match (current.digest, current.digest_missing) {
            (Some(digest), _) => FullDigest::Done(digest),
            (None, missing) => FullDigest::Missing(missing.unwrap_or(DigestMissing::TooLarge)),
        })
    }
}

/// The warning of a JSON plan on a server without JSON commands (or one
/// that may not tell).
pub const JSON_UNAVAILABLE: &str = "This server has no JSON commands (JSON.SET): JSON keys need \
                                    Redis 8, Redis Stack or the RedisJSON module";

impl RedisSession {
    /// Whether the server has the JSON commands (`COMMAND INFO JSON.SET`);
    /// `None` when it cannot tell (`COMMAND INFO` refused, or a console
    /// `MULTI` block is open).
    ///
    /// # Errors
    ///
    /// Connection errors.
    pub async fn has_json_commands(&self) -> Result<Option<bool>, DriverError> {
        let mut call = self.begin().await?;
        if call.in_multi() {
            return Ok(None);
        }
        match call
            .query(redis::cmd("COMMAND").arg("INFO").arg("JSON.SET"))
            .await
        {
            Ok(redis::Value::Array(items)) => Ok(Some(
                items
                    .first()
                    .is_some_and(|i| !matches!(i, redis::Value::Nil)),
            )),
            Ok(_) => Ok(None),
            Err(error) if error.code.is_some() => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn read_only() -> DriverError {
    DriverError::new(
        ErrorKind::ReadOnly,
        "This connection is read-only, so keys cannot be changed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test(
        key: &[u8],
        db: u16,
        expect: &KeyExpect,
        edits: &[KeyEdit],
    ) -> Result<EditProgram, EditError> {
        build(key, db, expect, edits, (8, 10))
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    fn present(key_type: KeyType) -> KeyExpect {
        KeyExpect::Present {
            key_type,
            version: None,
        }
    }

    fn lines(program: &EditProgram) -> Vec<String> {
        program.view.commands.clone()
    }

    #[test]
    fn requests_deserialize_from_the_ui() -> Result<(), serde_json::Error> {
        let expect: KeyExpect = serde_json::from_value(serde_json::json!({
            "state": "present", "keyType": "hash",
            "version": { "keyType": "hash", "length": "3", "digest": "ab" }
        }))?;
        assert!(
            matches!(expect, KeyExpect::Present { version: Some(ref v), .. } if v.length == Some(3))
        );
        let edits: Vec<KeyEdit> = serde_json::from_value(serde_json::json!([
            { "op": "hashSet", "field": { "t": "text", "v": "f" }, "value": { "t": "text", "v": "v" }, "original": null },
            { "op": "listSet", "index": "4", "value": { "t": "text", "v": "x" }, "original": { "t": "bytes", "v": "00ff" } },
            { "op": "zsetScore", "member": { "t": "text", "v": "m" }, "score": { "t": "float", "v": 1.5 }, "original": { "t": "other", "v": "inf" } },
            { "op": "expire", "ttlMs": 1000 },
            { "op": "rename", "newKey": "b", "replace": false },
            { "op": "persist" }, { "op": "delete" },
            { "op": "streamTrim", "strategy": "minId", "threshold": "5-0", "approximate": true }
        ]))?;
        assert_eq!(edits.len(), 8);
        assert_eq!(edits[3], KeyEdit::Expire { ttl_ms: 1000 });
        Ok(())
    }

    #[test]
    fn ts_alter_keeps_labels_exact_and_limits_their_number() -> Result<(), EditError> {
        let meta = TsMeta {
            retention_ms: 0,
            labels: vec![],
            duplicate_policy: None,
        };
        let expect = KeyExpect::Present {
            key_type: KeyType::Other("TSDB-TYPE".into()),
            version: None,
        };
        let alter = |labels: Vec<TsLabel>| KeyEdit::TsAlter {
            retention_ms: None,
            labels: Some(labels),
            duplicate_policy: None,
            original: meta.clone(),
        };
        let program = build_test(
            b"ts",
            0,
            &expect,
            &[alter(vec![TsLabel {
                name: "room".into(),
                value: " a b ".into(),
            }])],
        )?;
        let last = program.commands[0].last().cloned().unwrap_or_default();
        assert_eq!(last, b" a b ");
        let many = (0..=MAX_TS_LABELS)
            .map(|i| TsLabel {
                name: format!("l{i}"),
                value: "v".into(),
            })
            .collect();
        let refused = build_test(b"ts", 0, &expect, &[alter(many)]);
        assert!(
            matches!(&refused, Err(EditError::Invalid(m)) if m.contains("at most 1000 labels")),
            "{refused:?}"
        );
        Ok(())
    }

    #[test]
    fn list_edits_keep_the_indexes_that_were_read() -> Result<(), EditError> {
        let edits = vec![
            KeyEdit::ListPush {
                end: ListEnd::Head,
                values: vec![text("new")],
            },
            KeyEdit::ListRemove {
                index: 1,
                original: text("b"),
            },
            KeyEdit::ListSet {
                index: 3,
                value: text("D"),
                original: text("d"),
            },
            KeyEdit::Expire { ttl_ms: 5000 },
        ];
        let expect = KeyExpect::Present {
            key_type: KeyType::List,
            version: Some(KeyVersion {
                key_type: KeyType::List,
                length: Some(4),
                digest: Some("l-1".into()),
                digest_missing: None,
                digest_read: None,
                db: None,
                unchecked_reason: None,
                expire_at_ms: None,
            }),
        };
        let program = build_test(b"l", 0, &expect, &edits)?;
        assert!(program.check_expiry && !program.check_content);
        assert!(!program.view.unchecked);
        let lines = lines(&program);
        assert_eq!(lines[0], "LSET l 3 D");
        assert!(
            lines[1].starts_with("LSET l 1 \"\\x00varve-removed:"),
            "{}",
            lines[1]
        );
        assert!(lines[2].starts_with("LREM l 0 "));
        assert_eq!(lines[3], "LPUSH l new");
        assert_eq!(lines[4], "PEXPIRE l 5000");
        assert_eq!(program.checks.len(), 2);
        assert!(program.view.destructive);
        assert_eq!(
            program.counts,
            EditCounts {
                added: 1,
                changed: 2,
                removed: 1
            }
        );
        assert_eq!(
            program.command_names(),
            ["LSET", "LREM", "LPUSH", "PEXPIRE"]
        );
        Ok(())
    }

    #[test]
    fn rules_are_enforced() {
        let err =
            |expect: KeyExpect, edits: Vec<KeyEdit>| build_test(b"k", 0, &expect, &edits).err();
        assert!(err(present(KeyType::Hash), vec![]).is_some());
        // Delete alone, and only with a version.
        assert!(err(present(KeyType::Hash), vec![KeyEdit::Delete]).is_some());
        let versioned = KeyExpect::Present {
            key_type: KeyType::Hash,
            version: Some(KeyVersion {
                key_type: KeyType::Hash,
                length: Some(1),
                digest: None,
                digest_missing: None,
                digest_read: None,
                db: None,
                unchecked_reason: None,
                expire_at_ms: None,
            }),
        };
        assert!(err(versioned.clone(), vec![KeyEdit::Delete]).is_none());
        assert!(err(versioned, vec![KeyEdit::Delete, KeyEdit::Persist]).is_some());
        // Type mismatch.
        assert!(
            err(
                present(KeyType::Set),
                vec![KeyEdit::HashDelete {
                    field: text("f"),
                    original: text("v")
                }]
            )
            .is_some()
        );
        // Creating needs a creating edit of one type.
        assert!(err(KeyExpect::Absent, vec![KeyEdit::Expire { ttl_ms: 1 }]).is_some());
        assert!(
            err(
                KeyExpect::Absent,
                vec![
                    KeyEdit::SetAdd {
                        members: vec![text("a")]
                    },
                    KeyEdit::Expire { ttl_ms: 10 }
                ]
            )
            .is_none()
        );
        assert!(
            err(
                KeyExpect::Absent,
                vec![
                    KeyEdit::SetAdd {
                        members: vec![text("a")]
                    },
                    KeyEdit::ListPush {
                        end: ListEnd::Tail,
                        values: vec![text("b")]
                    }
                ]
            )
            .is_some()
        );
        // An existing string needs its original.
        assert!(
            err(
                present(KeyType::String),
                vec![KeyEdit::StringSet {
                    value: text("x"),
                    original: None
                }]
            )
            .is_some()
        );
        // One element once.
        assert!(
            err(
                present(KeyType::Hash),
                vec![
                    KeyEdit::HashDelete {
                        field: text("f"),
                        original: text("v")
                    },
                    KeyEdit::HashSet {
                        field: text("f"),
                        value: text("w"),
                        original: Some(text("v"))
                    }
                ]
            )
            .is_some()
        );
        // Placeholders and other values are refused.
        assert!(
            err(
                present(KeyType::Set),
                vec![KeyEdit::SetRemove {
                    member: Value::Other(format!("{OVERSIZED_MARK}: 20 MiB)"))
                }]
            )
            .is_some()
        );
        assert!(
            err(
                present(KeyType::Set),
                vec![KeyEdit::SetRemove {
                    member: Value::Int(1)
                }]
            )
            .is_some()
        );
        // Bad ids, ttl and JSON.
        assert!(
            err(
                present(KeyType::Stream),
                vec![KeyEdit::StreamDelete { id: "x".into() }]
            )
            .is_some()
        );
        assert!(err(present(KeyType::Hash), vec![KeyEdit::Expire { ttl_ms: 0 }]).is_some());
        assert!(
            err(
                KeyExpect::Absent,
                vec![KeyEdit::JsonSet {
                    document: "{".into(),
                    original: None
                }]
            )
            .is_some()
        );
        assert!(
            err(
                present(KeyType::Hash),
                vec![KeyEdit::Rename {
                    new_key: "k".into(),
                    replace: false
                }]
            )
            .is_some()
        );
        // Too much data.
        let big = Value::Bytes(vec![0; usize::try_from(MAX_EDIT_BYTES).unwrap_or(0) + 1]);
        assert!(
            err(
                present(KeyType::Set),
                vec![KeyEdit::SetAdd { members: vec![big] }]
            )
            .is_some()
        );
    }

    #[test]
    fn creating_keys_skips_element_checks() -> Result<(), EditError> {
        let program = build_test(
            b"s",
            2,
            &KeyExpect::Absent,
            &[KeyEdit::StreamAdd {
                id: None,
                fields: vec![FieldValue {
                    field: text("a"),
                    value: text("1"),
                }],
            }],
        )?;
        assert_eq!(lines(&program), ["XADD s * a 1"]);
        assert_eq!(program.type_names, ["none"]);
        assert_eq!(program.db, 2);
        let program = build_test(
            b"s",
            0,
            &present(KeyType::Stream),
            &[KeyEdit::StreamAdd {
                id: Some("5-1".into()),
                fields: vec![FieldValue {
                    field: text("a"),
                    value: text("1"),
                }],
            }],
        )?;
        assert_eq!(lines(&program), ["XADD s NOMKSTREAM 5-1 a 1"]);
        Ok(())
    }

    #[test]
    fn scores_and_display() -> Result<(), EditError> {
        assert_eq!(score(&Value::Other("-inf".into()), "s")?.1, "-inf");
        assert_eq!(score(&Value::Float(1.5), "s")?.1, "1.5");
        assert_eq!(score(&Value::Int(3), "s")?.0, 3.0);
        assert!(score(&Value::Float(f64::NAN), "s").is_err());
        let long = "x".repeat(300);
        let line = display_line(&[b"SET".to_vec(), b"k".to_vec(), long.into_bytes()]);
        assert!(line.ends_with("...[300 bytes]"), "{line}");
        assert!(line.len() < 150);
        let program = build_test(
            b"z",
            0,
            &present(KeyType::Zset),
            &[KeyEdit::ZsetScore {
                member: text("m"),
                score: Value::Float(2.0),
                original: Value::Float(1.0),
            }],
        )?;
        assert_eq!(lines(&program), ["ZADD z XX 2 m"]);
        assert!(!format!("{program:?}").contains("checks"));
        Ok(())
    }

    /// A connection to a fake server that answers with `replies`.
    fn scripted(replies: &'static [u8]) -> RawConn {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = server.write_all(replies).await;
            let mut sink = Vec::new();
            let _ = server.read_to_end(&mut sink).await;
        });
        let stream: Box<dyn crate::conn::Stream> = Box::new(client);
        RawConn {
            io: tokio::io::BufReader::new(stream),
        }
    }

    fn hash_edit() -> Result<EditProgram, EditError> {
        build_test(
            b"h",
            0,
            &present(KeyType::Hash),
            &[KeyEdit::HashSet {
                field: text("f"),
                value: text("new"),
                original: Some(text("old")),
            }],
        )
    }

    #[tokio::test]
    async fn a_change_between_checks_and_exec_is_a_conflict()
    -> Result<(), Box<dyn std::error::Error>> {
        let program = hash_edit()?;
        // WATCH, TYPE, HGET, MULTI, QUEUED, EXEC aborted by the WATCH.
        let mut raw = scripted(b"+OK\r\n+hash\r\n$3\r\nold\r\n+OK\r\n+QUEUED\r\n*-1\r\n");
        let sent = AtomicBool::new(false);
        match run(&mut raw, &program, &sent).await? {
            Ran::Conflict(None, ConflictReason::ConcurrentChange, _) => {}
            _ => panic!("not a concurrent change"),
        }
        assert!(sent.load(Ordering::SeqCst));
        // The value check fails before anything is sent.
        let mut raw = scripted(b"+OK\r\n+hash\r\n$5\r\nother\r\n");
        let sent = AtomicBool::new(false);
        match run(&mut raw, &program, &sent).await? {
            Ran::Conflict(Some(0), ConflictReason::ValueChanged, _) => {}
            _ => panic!("not a value conflict"),
        }
        assert!(!sent.load(Ordering::SeqCst), "no transaction was sent");
        // A command refused while queued: nothing ran, the error says so.
        let mut raw = scripted(
            b"+OK\r\n+hash\r\n$3\r\nold\r\n+OK\r\n-OOM no memory\r\n-EXECABORT discarded\r\n",
        );
        let error = run(&mut raw, &program, &AtomicBool::new(false)).await.err();
        let error = error.ok_or("EXECABORT was not an error")?;
        assert_eq!(error.code.as_deref(), Some("OOM"));
        assert!(error.detail.unwrap_or_default().contains("nothing ran"));
        // A command rejected by EXEC: failed, with its index.
        let mut raw =
            scripted(b"+OK\r\n+hash\r\n$3\r\nold\r\n+OK\r\n+QUEUED\r\n*1\r\n-ERR boom\r\n");
        match run(&mut raw, &program, &AtomicBool::new(false)).await? {
            Ran::Done(EditOutcome::Failed { command: 0, error }) => {
                assert_eq!(error.code.as_deref(), Some("ERR"));
            }
            _ => panic!("not failed"),
        }
        // The type changed.
        let mut raw = scripted(b"+OK\r\n+none\r\n");
        match run(&mut raw, &program, &AtomicBool::new(false)).await? {
            Ran::Conflict(None, ConflictReason::TypeChanged, message) => {
                assert!(message.contains("no longer exists"));
            }
            _ => panic!("not a type conflict"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn nested_replies_are_bounded() -> Result<(), DriverError> {
        let mut raw = scripted(b"*3\r\n$3\r\n0-0\r\n*1\r\n$3\r\n1-0\r\n*0\r\n");
        let mut budget = 10;
        let value = read_bounded(&mut raw, &mut budget).await?;
        assert_eq!(
            value.ok(),
            Some(Value::Array(vec![
                text("0-0"),
                Value::Array(vec![text("1-0")]),
                Value::Array(vec![])
            ]))
        );
        let mut raw = scripted(b"*3\r\n:1\r\n:2\r\n:3\r\n");
        let mut budget = 2;
        assert_eq!(
            read_bounded(&mut raw, &mut budget).await?.ok(),
            Some(Value::Other("3 elements".into()))
        );
        Ok(())
    }

    #[test]
    fn expiries_compare_exactly_or_within_the_tolerance() {
        assert!(same_expiry(None, None, true));
        assert!(same_expiry(Some(1000), Some(1000), true));
        assert!(!same_expiry(Some(1000), Some(1001), true));
        assert!(same_expiry(
            Some(1000),
            Some(1000 + TTL_TOLERANCE_MS),
            false
        ));
        assert!(!same_expiry(
            Some(1000),
            Some(1001 + TTL_TOLERANCE_MS),
            false
        ));
        assert!(!same_expiry(Some(1000), None, false));
    }

    #[test]
    fn whole_key_changes_without_a_digest_are_unchecked() -> Result<(), EditError> {
        let expect = |missing| KeyExpect::Present {
            key_type: KeyType::Hash,
            version: Some(KeyVersion {
                key_type: KeyType::Hash,
                length: Some(9),
                digest: None,
                digest_missing: Some(missing),
                digest_read: None,
                db: None,
                unchecked_reason: None,
                expire_at_ms: None,
            }),
        };
        let program = build_test(
            b"h",
            0,
            &expect(DigestMissing::TooLarge),
            &[KeyEdit::Delete],
        )?;
        assert!(program.view.unchecked && program.check_content);
        assert!(program.view.warnings[0].contains("too large to read"));
        let program = build_test(
            b"h",
            0,
            &expect(DigestMissing::NotPermitted),
            &[KeyEdit::Delete],
        )?;
        assert!(program.view.warnings[0].contains("may not read the key in pieces"));
        // Still being read in the background, or the read stalled: rename
        // and trim are unchecked too, each with its reason.
        let edits = [
            KeyEdit::Delete,
            KeyEdit::Rename {
                new_key: "h2".into(),
                replace: false,
            },
            KeyEdit::StreamTrim {
                strategy: TrimStrategy::MaxLen,
                threshold: "1".into(),
                approximate: false,
            },
        ];
        for (missing, why) in [
            (DigestMissing::Deferred, "had not finished"),
            (DigestMissing::Stalled, "did not answer in time"),
        ] {
            for edit in &edits {
                let mut expect = expect(missing);
                if matches!(edit, KeyEdit::StreamTrim { .. }) {
                    expect = KeyExpect::Present {
                        key_type: KeyType::Stream,
                        version: match expect {
                            KeyExpect::Present { version, .. } => version.map(|v| KeyVersion {
                                key_type: KeyType::Stream,
                                ..v
                            }),
                            KeyExpect::Absent => None,
                        },
                    };
                }
                let program = build_test(b"h", 0, &expect, std::slice::from_ref(edit))?;
                assert!(program.view.unchecked, "{missing:?} {edit:?}");
                assert!(
                    program.view.warnings[0].contains(why),
                    "{:?}",
                    program.view.warnings
                );
            }
        }
        // A TTL change does not need the digest.
        let program = build_test(
            b"h",
            0,
            &expect(DigestMissing::TooLarge),
            &[KeyEdit::Expire { ttl_ms: 5 }],
        )?;
        assert!(!program.view.unchecked && program.check_expiry && !program.check_content);
        Ok(())
    }

    #[test]
    fn originals_are_kept_as_digests() -> Result<(), EditError> {
        let big = "a".repeat(6 * 1024 * 1024);
        let program = build_test(
            b"s",
            0,
            &present(KeyType::String),
            &[KeyEdit::StringSet {
                value: text("b"),
                original: Some(Value::Text(big.clone())),
            }],
        )?;
        assert_eq!(
            program.checks[0].expect,
            Expectation::Bulk(Some(value_digest(big.as_bytes())))
        );
        Ok(())
    }

    #[test]
    fn markers_differ() {
        assert_ne!(list_marker(), list_marker());
    }
}
