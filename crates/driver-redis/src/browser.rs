//! Read-only key browser (design doc 4.2): `SCAN` pages, key details and
//! paged values per type. Every call is bounded and never uses `KEYS`.
//!
//! A value page has two budgets: at most `count` entries and about
//! [`MAX_PAGE_BYTES`] of element data. Entries are fetched in batches sized
//! from `MEMORY USAGE` and the sizes seen so far; the page stops once the
//! byte budget is used up and returns a cursor for the rest. For hashes and
//! sets the server decides how many entries one `HSCAN`/`SSCAN` step returns
//! (`COUNT` is only a hint), so such a page can go over by one step. Strings come in
//! [`STRING_CHUNK_BYTES`] slices. No single request reads more than
//! [`crate::MAX_REPLY_BYTES`]: a batch over it is retried one element at a
//! time, and an element larger than that becomes a placeholder
//! ([`crate::OVERSIZED_MARK`]) so paging goes on past it. A page converts at most [`crate::MAX_RESULT_VALUES`] values.

use redis::Value as Resp;
use serde::{Deserialize, Serialize};
use varve_driver::{DriverError, ErrorKind, Value};

use crate::conn::info_fields;
use crate::error;
use crate::oversized::{self, Served};
use crate::pagestate::{self, Pending, STATE_CURSOR_PREFIX, ScanKind, ScanState};
use crate::parse::key_display;
use crate::reply::{
    ValueBudget, as_int, as_text, bytes_value, double_value, server_error_text, to_value,
};
use crate::session::{Call, RedisSession};

/// Largest slice of a string value returned by one [`RedisSession::read_value`].
pub const STRING_CHUNK_BYTES: u64 = 512 * 1024;

/// Most entries asked for in one page.
pub const MAX_PAGE_COUNT: u32 = 10_000;

/// Element bytes one value page aims for (4 MiB). A page ends after the
/// batch that reaches it, so it can go over by one batch: batches of lists,
/// sorted sets and streams are sized to fit the remaining budget from the
/// sizes seen so far; an `HSCAN`/`SSCAN` step can return more entries than
/// asked for. No batch is ever larger than [`crate::MAX_REPLY_BYTES`].
pub const MAX_PAGE_BYTES: u64 = 4 * 1024 * 1024;

/// JSON documents larger than this are not loaded by the browser.
pub const MAX_JSON_BYTES: i64 = 8 * 1024 * 1024;

/// Type of a key, as `TYPE` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyType {
    /// `string`.
    String,
    /// `list`.
    List,
    /// `set`.
    Set,
    /// `zset` (sorted set).
    Zset,
    /// `hash`.
    Hash,
    /// `stream`.
    Stream,
    /// `ReJSON-RL` (RedisJSON module, built into Redis 8).
    Json,
    /// Any other module type, with the name `TYPE` returned.
    Other(String),
    /// Not known: only for a placeholder [`KeyEntry`] of a key whose name
    /// is too large to read, so its type could not be asked for (and the
    /// key cannot be opened).
    Unknown,
}

impl KeyType {
    /// Parses a `TYPE` reply. `None` for `none` (the key does not exist).
    #[must_use]
    pub fn from_type_name(name: &str) -> Option<KeyType> {
        Some(match name {
            "none" => return None,
            "string" => KeyType::String,
            "list" => KeyType::List,
            "set" => KeyType::Set,
            "zset" => KeyType::Zset,
            "hash" => KeyType::Hash,
            "stream" => KeyType::Stream,
            "ReJSON-RL" => KeyType::Json,
            other => KeyType::Other(other.to_string()),
        })
    }

    /// The name `TYPE` reports and `SCAN ... TYPE` expects.
    #[must_use]
    pub fn type_name(&self) -> &str {
        match self {
            KeyType::String => "string",
            KeyType::List => "list",
            KeyType::Set => "set",
            KeyType::Zset => "zset",
            KeyType::Hash => "hash",
            KeyType::Stream => "stream",
            KeyType::Json => "ReJSON-RL",
            KeyType::Other(name) => name,
            // No server type has this name, so a `SCAN ... TYPE unknown`
            // filter finds nothing.
            KeyType::Unknown => "unknown",
        }
    }
}

/// One key of a scan page.
///
/// A key whose name is too large to read (over [`crate::MAX_REPLY_BYTES`])
/// is a placeholder: `placeholder` is true, `name` is empty, `display`
/// starts with [`crate::OVERSIZED_MARK`] and gives the name's size, the
/// type is [`KeyType::Unknown`] (or the type filter of the scan, which the
/// server applied) and the time to live is `None` (unknown). It cannot be
/// opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyEntry {
    /// Key bytes, serialized as lowercase hex.
    #[serde(with = "hex_bytes")]
    pub name: Vec<u8>,
    /// Lossless display text: see [`crate::key_display`].
    pub display: String,
    /// Type.
    pub key_type: KeyType,
    /// Time to live in milliseconds; `None` when the key does not expire
    /// (or, for a placeholder, when it is not known).
    #[serde(with = "opt_i64_text")]
    pub ttl_ms: Option<i64>,
    /// True for a key whose name is too large to read; see above.
    #[serde(default)]
    pub placeholder: bool,
}

/// One `SCAN` step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanPage {
    /// Cursor for the next step; `0` when the scan is complete.
    #[serde(with = "u64_text")]
    pub cursor: u64,
    /// Keys found in this step (can be empty while the cursor is not 0).
    pub keys: Vec<KeyEntry>,
}

/// Details of one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyInfo {
    /// Type.
    pub key_type: KeyType,
    /// Time to live in milliseconds; `None` when the key does not expire.
    #[serde(with = "opt_i64_text")]
    pub ttl_ms: Option<i64>,
    /// `MEMORY USAGE`, when the server allows it.
    #[serde(with = "opt_u64_text")]
    pub memory_bytes: Option<u64>,
    /// `OBJECT ENCODING`, for example `listpack` or `embstr`.
    pub encoding: Option<String>,
    /// Bytes of a string, entries of a hash, list, set, sorted set or
    /// stream. `None` for other types.
    #[serde(with = "opt_u64_text")]
    pub length: Option<u64>,
}

/// Which part of a value to read.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValuePage {
    /// Opaque position from [`ValueChunk::next_cursor`]; empty for the
    /// start. It is a byte offset for strings, a rank for lists and sorted
    /// sets, a `HSCAN`/`SSCAN` cursor or a `p:<id>` page state for hashes and
    /// sets, and an entry id
    /// for streams.
    pub cursor: String,
    /// Entries to read (ignored for strings, which come in chunks of
    /// [`STRING_CHUNK_BYTES`]). Clamped to 1..=[`MAX_PAGE_COUNT`]. A page
    /// can hold fewer when [`MAX_PAGE_BYTES`] is reached first; its
    /// [`ValueChunk::next_cursor`] continues from there.
    pub count: u32,
}

impl ValuePage {
    /// The first page with `count` entries.
    #[must_use]
    pub fn first(count: u32) -> Self {
        Self {
            cursor: String::new(),
            count,
        }
    }
}

/// A field and value of a hash, or a field of a stream entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldValue {
    /// Field name.
    pub field: Value,
    /// Value.
    pub value: Value,
}

/// A list element with its index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListItem {
    /// 0-based index in the list.
    #[serde(with = "u64_text")]
    pub index: u64,
    /// Element.
    pub value: Value,
}

/// A sorted set member with its score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredMember {
    /// Member.
    pub member: Value,
    /// Score: [`Value::Float`], or [`Value::Other`] with `inf` or `-inf`
    /// (JSON has no infinity).
    pub score: Value,
}

/// A stream entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamEntry {
    /// Entry id, `<ms>-<seq>`.
    pub id: String,
    /// Fields in order.
    pub fields: Vec<FieldValue>,
}

/// The data of one page, by type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ValueData {
    /// A slice of a string.
    String {
        /// Text when the slice is valid UTF-8, otherwise bytes.
        value: Value,
        /// Byte offset of the slice.
        #[serde(with = "u64_text")]
        offset: u64,
        /// True when the string continues after this slice.
        truncated: bool,
    },
    /// Hash fields.
    Hash {
        /// Fields and values.
        entries: Vec<FieldValue>,
    },
    /// List elements.
    List {
        /// Elements with their index.
        items: Vec<ListItem>,
    },
    /// Set members.
    Set {
        /// Members.
        members: Vec<Value>,
    },
    /// Sorted set members by rank.
    Zset {
        /// Members with scores.
        members: Vec<ScoredMember>,
    },
    /// Stream entries.
    Stream {
        /// Entries in id order.
        entries: Vec<StreamEntry>,
    },
    /// A JSON document (`JSON.GET`).
    Json {
        /// The document, or its text when it does not parse.
        document: Value,
    },
    /// A time series: `TS.INFO` and a page of samples (`TS.RANGE`).
    TimeSeries {
        /// The series' info.
        info: crate::modules::TsInfo,
        /// Samples, oldest first.
        samples: Vec<crate::modules::TsSample>,
    },
    /// A Bloom or Cuckoo filter, Count-Min sketch, Top-K or t-digest: its
    /// info reply, and for a Top-K its list (`TOPK.LIST WITHCOUNT`).
    #[serde(rename_all = "camelCase")]
    Probabilistic {
        /// Which kind.
        module_kind: crate::modules::ModuleKind,
        /// The info fields (a t-digest adds `Min` and `Max`).
        fields: Vec<crate::modules::InfoField>,
        /// The Top-K items with their counts, highest first; empty for the
        /// other kinds.
        top: Vec<crate::modules::TopKItem>,
    },
}

/// One page of a value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValueChunk {
    /// Type.
    pub key_type: KeyType,
    /// Total length (bytes of a string, entries otherwise), when known.
    #[serde(with = "opt_u64_text")]
    pub total: Option<u64>,
    /// Cursor for the next page; `None` when this was the last.
    pub next_cursor: Option<String>,
    /// The data.
    pub data: ValueData,
}

/// Key counts of one logical database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbKeyspace {
    /// Database index.
    pub db: u16,
    /// Number of keys.
    #[serde(with = "u64_text")]
    pub keys: u64,
    /// Keys with a time to live.
    #[serde(with = "u64_text")]
    pub expires: u64,
}

impl RedisSession {
    /// One `SCAN` step with `MATCH`, `COUNT` and, when given, a type filter
    /// (`SCAN ... TYPE` on Redis 6 and newer, emulated with `TYPE` lookups
    /// on older servers). Type and TTL of every key come from one pipeline.
    /// Keys that vanish between the steps are left out.
    ///
    /// # Errors
    ///
    /// Query errors from the server, connection errors, Cancelled.
    pub async fn scan_keys(
        &self,
        cursor: u64,
        pattern: &str,
        type_filter: Option<KeyType>,
        count: u32,
    ) -> Result<ScanPage, DriverError> {
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        let native_type = call.details().compat >= (6, 0);
        let pattern = if pattern.is_empty() { "*" } else { pattern };
        let count = count.clamp(1, MAX_PAGE_COUNT);
        let mut scan = redis::cmd("SCAN");
        scan.arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(count);
        if native_type && let Some(filter) = &type_filter {
            scan.arg("TYPE").arg(filter.type_name());
        }
        let reply = match call.send(&scan).await {
            Ok(Resp::ServerError(e)) => return Err(query_error(&e)),
            Ok(reply) => reply,
            // Key names of the step over MAX_REPLY_BYTES: the connection is
            // reopened once (it stopped mid-reply) and the step is read as a
            // stream on the side connection, without the names that do not
            // fit.
            Err(_) if call.reply_was_too_large() => {
                call.reconnect().await?;
                let (next, keys) =
                    oversized::scan_step(&mut call, cursor, pattern, type_filter.as_ref(), count)
                        .await?;
                return Ok(ScanPage { cursor: next, keys });
            }
            Err(e) => return Err(e),
        };
        let (next, names) = scan_reply(reply)?;
        let next = next
            .parse::<u64>()
            .map_err(|_| error::protocol(format!("SCAN returned the cursor {next}")))?;

        let mut pipe = redis::pipe();
        for name in &names {
            pipe.cmd("TYPE").arg(name.as_slice());
            pipe.cmd("PTTL").arg(name.as_slice());
        }
        let replies = call.send_pipeline(&pipe).await?;
        let mut keys = Vec::with_capacity(names.len());
        for (name, pair) in names.into_iter().zip(replies.chunks(2)) {
            let [type_reply, ttl_reply] = pair else {
                return Err(error::protocol("short pipeline reply"));
            };
            let Some(key_type) = as_text(type_reply).and_then(|t| KeyType::from_type_name(&t))
            else {
                continue;
            };
            if type_filter.as_ref().is_some_and(|f| *f != key_type) {
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
        Ok(ScanPage { cursor: next, keys })
    }

    /// Type, TTL, memory, encoding and length of a key. `None` when the key
    /// does not exist.
    ///
    /// # Errors
    ///
    /// Connection errors, Cancelled.
    pub async fn key_info(&self, key: &[u8]) -> Result<Option<KeyInfo>, DriverError> {
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        let mut pipe = redis::pipe();
        pipe.cmd("TYPE").arg(key);
        pipe.cmd("PTTL").arg(key);
        pipe.cmd("MEMORY").arg("USAGE").arg(key);
        pipe.cmd("OBJECT").arg("ENCODING").arg(key);
        let replies = call.send_pipeline(&pipe).await?;
        let [type_reply, ttl_reply, memory_reply, encoding_reply] = replies.as_slice() else {
            return Err(error::protocol("short pipeline reply"));
        };
        if let Resp::ServerError(e) = type_reply {
            return Err(query_error(e));
        }
        let Some(key_type) = as_text(type_reply).and_then(|t| KeyType::from_type_name(&t)) else {
            return Ok(None);
        };
        let ttl = as_int(ttl_reply);
        if ttl == Some(-2) {
            return Ok(None);
        }
        let memory_bytes = as_int(memory_reply).and_then(|m| u64::try_from(m).ok());
        let encoding = match encoding_reply {
            Resp::ServerError(_) | Resp::Nil => None,
            other => as_text(other),
        };
        let length = match length_cmd(&key_type, key) {
            Some(cmd) => match call.send(&cmd).await? {
                Resp::ServerError(_) => None,
                reply => as_int(&reply).and_then(|n| u64::try_from(n).ok()),
            },
            None => None,
        };
        Ok(Some(KeyInfo {
            key_type,
            ttl_ms: ttl.filter(|t| *t >= 0),
            memory_bytes,
            encoding,
            length,
        }))
    }

    /// Reads one page of a value. `None` when the key does not exist.
    ///
    /// # Errors
    ///
    /// Unsupported for module types without a reader (the error code is the
    /// raw type name; TimeSeries and the probabilistic types have one, see
    /// [`crate::ModuleKind`]), Query for an invalid cursor, connection
    /// errors, Cancelled.
    pub async fn read_value(
        &self,
        key: &[u8],
        page: &ValuePage,
    ) -> Result<Option<ValueChunk>, DriverError> {
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        let type_reply = call.query(redis::cmd("TYPE").arg(key)).await?;
        let Some(key_type) = as_text(&type_reply).and_then(|t| KeyType::from_type_name(&t)) else {
            return Ok(None);
        };
        let count = page.count.clamp(1, MAX_PAGE_COUNT);
        let chunk = match &key_type {
            KeyType::String => read_string(&mut call, key, &page.cursor).await?,
            KeyType::Hash => read_hash(&mut call, key, &page.cursor, count).await?,
            KeyType::List => read_list(&mut call, key, &page.cursor, count).await?,
            KeyType::Set => read_set(&mut call, key, &page.cursor, count).await?,
            KeyType::Zset => read_zset(&mut call, key, &page.cursor, count).await?,
            KeyType::Stream => read_stream(&mut call, key, &page.cursor, count).await?,
            KeyType::Json => read_json(&mut call, key).await?,
            // `TYPE` never reports it; only scan placeholders have it.
            KeyType::Unknown => return Err(error::protocol("TYPE returned no type")),
            KeyType::Other(name) => match crate::modules::ModuleKind::from_type_name(name) {
                Some(kind) => {
                    crate::modules::read_module(&mut call, key, kind, &page.cursor, count).await?
                }
                None => {
                    return Err(DriverError::new(
                        ErrorKind::Unsupported,
                        format!("Keys of type {name} cannot be shown yet"),
                    )
                    .with_code(name.clone()));
                }
            },
        };
        Ok(chunk.map(|(total, next_cursor, data)| ValueChunk {
            key_type,
            total,
            next_cursor,
            data,
        }))
    }

    /// Key counts per database from `INFO keyspace`. Databases without keys
    /// are not listed by the server.
    ///
    /// # Errors
    ///
    /// Connection errors, Cancelled.
    pub async fn keyspace(&self) -> Result<Vec<DbKeyspace>, DriverError> {
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        let reply = call.query(redis::cmd("INFO").arg("keyspace")).await?;
        let text = as_text(&reply).ok_or_else(|| error::protocol("INFO did not return text"))?;
        Ok(parse_keyspace(&text))
    }

    /// Number of logical databases (`CONFIG GET databases`); `None` when
    /// the user may not read the configuration.
    ///
    /// # Errors
    ///
    /// Connection errors, Cancelled.
    pub async fn database_count(&self) -> Result<Option<u16>, DriverError> {
        let mut call = self.begin().await?;
        call.no_open_transaction()?;
        let reply = call
            .send(redis::cmd("CONFIG").arg("GET").arg("databases"))
            .await?;
        let value = match reply {
            Resp::Array(items) => items.get(1).and_then(as_text),
            Resp::Map(pairs) => pairs.first().and_then(|(_, v)| as_text(v)),
            _ => None,
        };
        Ok(value.and_then(|v| v.parse().ok()))
    }
}

/// `(total, next_cursor, data)` of one page, `None` when the key vanished.
type Page = Option<(Option<u64>, Option<String>, ValueData)>;

pub(crate) fn query_error(e: &redis::ServerError) -> DriverError {
    DriverError::new(ErrorKind::Query, server_error_text(e)).with_code(e.code())
}

fn length_cmd(key_type: &KeyType, key: &[u8]) -> Option<redis::Cmd> {
    let name = match key_type {
        KeyType::String => "STRLEN",
        KeyType::Hash => "HLEN",
        KeyType::List => "LLEN",
        KeyType::Set => "SCARD",
        KeyType::Zset => "ZCARD",
        KeyType::Stream => "XLEN",
        KeyType::Json | KeyType::Other(_) | KeyType::Unknown => return None,
    };
    let mut cmd = redis::cmd(name);
    cmd.arg(key);
    Some(cmd)
}

fn bad_cursor(cursor: &str) -> DriverError {
    DriverError::new(ErrorKind::Query, format!("Invalid page cursor {cursor:?}"))
}

/// Largest offset or rank Redis takes (its ranges are signed 64-bit).
const MAX_OFFSET: u64 = i64::MAX as u64;

/// A byte offset or rank cursor: `0` when empty, at most [`MAX_OFFSET`].
fn offset_cursor(cursor: &str) -> Result<u64, DriverError> {
    if cursor.is_empty() {
        return Ok(0);
    }
    cursor
        .parse()
        .ok()
        .filter(|offset| *offset <= MAX_OFFSET)
        .ok_or_else(|| bad_cursor(cursor))
}

/// The inclusive end of a range of `n` items from `start`, kept within
/// what Redis accepts.
fn range_end(start: u64, n: u64) -> u64 {
    start.saturating_add(n.saturating_sub(1)).min(MAX_OFFSET)
}

fn total(reply: &Resp) -> Option<u64> {
    as_int(reply).and_then(|n| u64::try_from(n).ok())
}

/// Two pipelined replies: a length and a page, error replies mapped.
async fn length_and_page(
    call: &mut Call<'_>,
    length: redis::Cmd,
    page: redis::Cmd,
) -> Result<(Option<u64>, Resp), DriverError> {
    let mut pipe = redis::pipe();
    pipe.add_command(length).add_command(page);
    let mut replies = call.send_pipeline(&pipe).await?.into_iter();
    let (Some(length), Some(page)) = (replies.next(), replies.next()) else {
        return Err(error::protocol("short pipeline reply"));
    };
    if let Resp::ServerError(e) = &page {
        return Err(query_error(e));
    }
    Ok((total(&length), page))
}

/// The length of a value and an estimate of its bytes per element from
/// `MEMORY USAGE` (sampled by the server). The estimate is `None` when the
/// user may not run `MEMORY USAGE` or the value is empty.
async fn length_and_estimate(
    call: &mut Call<'_>,
    length_command: &str,
    key: &[u8],
) -> Result<(Option<u64>, Option<u64>), DriverError> {
    let mut pipe = redis::pipe();
    pipe.cmd(length_command).arg(key);
    pipe.cmd("MEMORY").arg("USAGE").arg(key);
    let replies = call.send_pipeline(&pipe).await?;
    let [length, memory] = replies.as_slice() else {
        return Err(error::protocol("short pipeline reply"));
    };
    let length = total(length);
    let memory = total(memory);
    let estimate = match (length, memory) {
        (Some(n), Some(bytes)) if n > 0 && bytes > 0 => Some(bytes.div_ceil(n)),
        _ => None,
    };
    Ok((length, estimate))
}

/// Entry and byte budget of one value page; sizes the batches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageBudget {
    /// Entries still wanted.
    items_left: u32,
    /// Element bytes read so far.
    bytes_used: u64,
    /// Entries read so far.
    items_seen: u64,
    /// Bytes per entry from `MEMORY USAGE`.
    estimate: Option<u64>,
    /// Largest batch to ask for; lowered after a batch was too large.
    cap: u32,
}

impl PageBudget {
    fn new(count: u32, estimate: Option<u64>) -> Self {
        Self {
            items_left: count,
            bytes_used: 0,
            items_seen: 0,
            estimate,
            cap: count.max(1),
        }
    }

    /// Entries to ask for next; `None` when the page is full. Without any
    /// size information the first batch is a single entry.
    fn next_batch(&self) -> Option<u32> {
        if self.is_full() {
            return None;
        }
        let observed = (self.items_seen > 0).then(|| self.bytes_used.div_ceil(self.items_seen));
        let per_item = match (observed, self.estimate) {
            (Some(seen), Some(estimate)) => seen.max(estimate),
            (Some(known), None) | (None, Some(known)) => known,
            (None, None) => return Some(1),
        }
        .max(1);
        let fits = (MAX_PAGE_BYTES - self.bytes_used) / per_item;
        let most = self.items_left.min(self.cap).max(1);
        Some(u32::try_from(fits).unwrap_or(u32::MAX).clamp(1, most))
    }

    /// True when the page has all its entries or bytes.
    pub(crate) fn is_full(&self) -> bool {
        self.items_left == 0 || self.bytes_used >= MAX_PAGE_BYTES
    }

    /// Accounts for a batch that was read.
    pub(crate) fn record(&mut self, items: usize, bytes: u64) {
        self.items_seen += items as u64;
        self.bytes_used += bytes;
        self.items_left = self
            .items_left
            .saturating_sub(u32::try_from(items).unwrap_or(u32::MAX));
    }

    /// A batch of `asked` entries was over [`crate::MAX_REPLY_BYTES`]: ask for at
    /// most half as many from now on.
    fn shrink(&mut self, asked: u32) {
        self.cap = (asked / 2).max(1);
    }
}

/// The outcome of one batch request.
enum Batch {
    /// The reply, and how many entries were asked for.
    Reply(u32, Resp),
    /// A single entry is larger than [`crate::MAX_REPLY_BYTES`]; the call
    /// has reconnected.
    Oversized,
}

/// Sends the request for a batch of `n` entries built by `build`. A reply
/// over [`crate::MAX_REPLY_BYTES`] closes the connection; the call then
/// reconnects and asks for a single entry, and reports
/// [`Batch::Oversized`] when even that is too large. Error replies become
/// Query errors.
async fn fetch_batch(
    call: &mut Call<'_>,
    budget: &mut PageBudget,
    n: u32,
    build: impl Fn(u32) -> redis::Cmd,
) -> Result<Batch, DriverError> {
    let mut asked = n;
    loop {
        match call.send(&build(asked)).await {
            Ok(Resp::ServerError(e)) => return Err(query_error(&e)),
            Ok(reply) => return Ok(Batch::Reply(asked, reply)),
            Err(_) if call.reply_was_too_large() => {
                budget.shrink(asked);
                call.reconnect().await?;
                if asked == 1 {
                    return Ok(Batch::Oversized);
                }
                asked = 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Bytes of element data in a reply: string lengths, 8 for numbers.
pub(crate) fn reply_bytes(reply: &Resp) -> u64 {
    match reply {
        Resp::BulkString(bytes) => bytes.len() as u64,
        Resp::SimpleString(text) | Resp::VerbatimString { text, .. } => text.len() as u64,
        Resp::BigNumber(digits) => digits.len() as u64,
        Resp::Array(items) | Resp::Set(items) | Resp::Push { data: items, .. } => {
            items.iter().map(reply_bytes).sum()
        }
        Resp::Map(pairs) => pairs
            .iter()
            .map(|(k, v)| reply_bytes(k) + reply_bytes(v))
            .sum(),
        Resp::Attribute { data, .. } => reply_bytes(data),
        _ => 8,
    }
}

async fn read_string(call: &mut Call<'_>, key: &[u8], cursor: &str) -> Result<Page, DriverError> {
    let offset = offset_cursor(cursor)?;
    let end = range_end(offset, STRING_CHUNK_BYTES);
    let mut length = redis::cmd("STRLEN");
    length.arg(key);
    let mut range = redis::cmd("GETRANGE");
    range.arg(key).arg(offset).arg(end);
    let (total, reply) = length_and_page(call, length, range).await?;
    let Resp::BulkString(mut bytes) = reply else {
        return Ok(None);
    };
    let total_len = total.unwrap_or(0);
    let mut read_to = offset + bytes.len() as u64;
    let truncated = read_to < total_len;
    if truncated {
        // Do not split a UTF-8 character across chunks.
        if let Err(e) = std::str::from_utf8(&bytes)
            && e.error_len().is_none()
            && e.valid_up_to() > 0
        {
            bytes.truncate(e.valid_up_to());
            read_to = offset + bytes.len() as u64;
        }
    }
    Ok(Some((
        total,
        truncated.then(|| read_to.to_string()),
        ValueData::String {
            value: bytes_value(bytes),
            offset,
            truncated,
        },
    )))
}

fn scan_reply(reply: Resp) -> Result<(String, Vec<Vec<u8>>), DriverError> {
    let Resp::Array(mut parts) = reply else {
        return Err(error::protocol("unexpected SCAN reply"));
    };
    if parts.len() != 2 {
        return Err(error::protocol("unexpected SCAN reply"));
    }
    let items = parts.pop();
    let cursor = parts.pop().as_ref().and_then(as_text);
    let (Some(cursor), Some(Resp::Array(items) | Resp::Set(items))) = (cursor, items) else {
        return Err(error::protocol("unexpected SCAN reply"));
    };
    let names = items
        .into_iter()
        .map(|item| match item {
            Resp::BulkString(bytes) => bytes,
            other => as_text(&other).unwrap_or_default().into_bytes(),
        })
        .collect();
    Ok((cursor, names))
}

/// Where a hash or set page starts: a real `HSCAN`/`SSCAN` cursor, or a
/// page state kept in the session (see [`crate::pagestate`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScanCursor {
    /// A scan cursor; `0` starts the scan.
    Scan(u64),
    /// The id of a page state.
    State(String),
}

impl ScanCursor {
    fn parse(cursor: &str) -> Result<Self, DriverError> {
        if cursor.is_empty() {
            return Ok(Self::Scan(0));
        }
        if let Some(id) = cursor.strip_prefix(STATE_CURSOR_PREFIX) {
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(bad_cursor(cursor));
            }
            return Ok(Self::State(id.to_string()));
        }
        cursor
            .parse()
            .map(Self::Scan)
            .map_err(|_| bad_cursor(cursor))
    }
}

/// How many empty scan steps the end-of-page look-ahead follows before it
/// gives up and returns the cursor it reached.
const LOOKAHEAD_STEPS: usize = 64;

/// Progress of a hash or set page through the scan.
struct Walk {
    /// The real scan cursor of the next step (when no state is active).
    position: u64,
    /// The kept step being served, and how many of its entries this page
    /// used.
    state: Option<(ScanState, usize)>,
    /// The scan is complete.
    done: bool,
    /// The state this page started from and its id, until the page has
    /// succeeded; put back when it fails, so the cursor can be retried.
    original: Option<String>,
    /// The original state after it was used up during this page.
    spent: Option<ScanState>,
}

fn scan_cmd(kind: ScanKind, key: &[u8], cursor: u64, count: u32) -> redis::Cmd {
    let mut cmd = redis::cmd(match kind {
        ScanKind::Hash => "HSCAN",
        ScanKind::Set => "SSCAN",
    });
    cmd.arg(key).arg(cursor).arg("COUNT").arg(count);
    cmd
}

fn bulk_bytes(reply: Resp) -> Vec<u8> {
    match reply {
        Resp::BulkString(bytes) => bytes,
        other => as_text(&other).unwrap_or_default().into_bytes(),
    }
}

/// Raw scan items as pending entries (look-ahead step kept for the next
/// page).
fn to_pending(kind: ScanKind, items: Vec<Resp>) -> Vec<Pending> {
    match kind {
        ScanKind::Hash => pairs(items)
            .into_iter()
            .map(|(f, v)| Pending::Pair(bulk_bytes(f), bulk_bytes(v)))
            .collect(),
        ScanKind::Set => items
            .into_iter()
            .map(|m| Pending::Member(bulk_bytes(m)))
            .collect(),
    }
}

/// Reads one hash or set page. Whole scan steps are read with
/// `HSCAN`/`SSCAN`; a step too large for one reply is read once without
/// its values (see [`oversized`]); a page that ends inside a step keeps the
/// rest in the session and returns a `p:<id>` cursor. When the page is
/// full, the next non-empty step is read ahead and kept too, so the last
/// page reports the end of the scan instead of leaving an empty page (a
/// page is still empty when every entry kept for it was deleted).
async fn read_scanned(
    call: &mut Call<'_>,
    kind: ScanKind,
    key: &[u8],
    cursor: &str,
    count: u32,
) -> Result<(Option<u64>, Option<String>, Vec<Served>), DriverError> {
    let start = ScanCursor::parse(cursor)?;
    let mut walk = Walk {
        position: 0,
        state: None,
        done: false,
        original: None,
        spent: None,
    };
    match start {
        ScanCursor::Scan(at) => walk.position = at,
        ScanCursor::State(id) => {
            let state = call.pages().take(&id)?;
            if state.kind != kind || state.db != call.db() || state.key != key {
                call.pages().restore(id, state);
                return Err(pagestate::expired());
            }
            walk.state = Some((state, 0));
            walk.original = Some(id);
        }
    }
    let length = match kind {
        ScanKind::Hash => "HLEN",
        ScanKind::Set => "SCARD",
    };
    let outcome = async {
        let (total, estimate) = length_and_estimate(call, length, key).await?;
        let mut page = PageBudget::new(count, estimate);
        let mut values = ValueBudget::new();
        let mut items = Vec::new();
        walk_scan(
            call,
            kind,
            key,
            &mut walk,
            &mut page,
            &mut values,
            &mut items,
        )
        .await?;
        Ok::<_, DriverError>((total, items))
    }
    .await;
    match outcome {
        Ok((total, items)) => {
            let next = match walk.state.take() {
                Some((mut state, used)) => {
                    state.pending.drain(..used);
                    Some(call.keep_page_state(state))
                }
                None => (!walk.done).then(|| walk.position.to_string()),
            };
            Ok((total, next, items))
        }
        Err(error) => {
            if let Some(id) = walk.original {
                let original = match (walk.spent, walk.state) {
                    (Some(spent), _) => Some(spent),
                    (None, Some((state, _))) => Some(state),
                    (None, None) => None,
                };
                if let Some(state) = original {
                    call.pages().restore(id, state);
                }
            }
            Err(error)
        }
    }
}

async fn walk_scan(
    call: &mut Call<'_>,
    kind: ScanKind,
    key: &[u8],
    walk: &mut Walk,
    page: &mut PageBudget,
    values: &mut ValueBudget,
    items: &mut Vec<Served>,
) -> Result<(), DriverError> {
    loop {
        if let Some((state, used)) = &mut walk.state {
            let (n, served) =
                oversized::serve_pending(call, key, &state.pending[*used..], page, values).await?;
            *used += n;
            items.extend(served);
            if *used < state.pending.len() {
                // The page is full inside the kept step.
                return Ok(());
            }
            walk.position = state.next;
            if let Some((state, _)) = walk.state.take()
                && walk.original.is_some()
                && walk.spent.is_none()
            {
                walk.spent = Some(state);
            }
            if walk.position == 0 {
                walk.done = true;
                return Ok(());
            }
            continue;
        }
        let Some(n) = page.next_batch() else {
            break;
        };
        let position = walk.position;
        match fetch_batch(call, page, n, |n| scan_cmd(kind, key, position, n)).await? {
            Batch::Reply(_, reply) => {
                let (next, raw) = scan_values(reply)?;
                page.record(
                    match kind {
                        ScanKind::Hash => raw.len() / 2,
                        ScanKind::Set => raw.len(),
                    },
                    raw.iter().map(reply_bytes).sum(),
                );
                match kind {
                    ScanKind::Hash => items.extend(
                        field_values(Resp::Array(raw), values)?
                            .into_iter()
                            .map(Served::Field),
                    ),
                    ScanKind::Set => {
                        for member in raw {
                            items.push(Served::Member(to_value(member, values)?));
                        }
                    }
                }
                walk.position = parse_scan_cursor(&next)?;
                if walk.position == 0 {
                    walk.done = true;
                    return Ok(());
                }
            }
            Batch::Oversized => {
                let (next, pending) = match kind {
                    ScanKind::Hash => oversized::capture_hash_step(call, key, position).await?,
                    ScanKind::Set => oversized::capture_set_step(call, key, position).await?,
                };
                walk.state = Some((
                    ScanState {
                        kind,
                        db: call.db(),
                        key: key.to_vec(),
                        pending,
                        next,
                    },
                    0,
                ));
            }
        }
    }
    look_ahead(call, kind, key, walk).await
}

/// The page is full and the scan not complete: read the next non-empty
/// step and keep it for the next page, or find that the scan has ended.
async fn look_ahead(
    call: &mut Call<'_>,
    kind: ScanKind,
    key: &[u8],
    walk: &mut Walk,
) -> Result<(), DriverError> {
    for _ in 0..LOOKAHEAD_STEPS {
        match call.send(&scan_cmd(kind, key, walk.position, 1)).await {
            Ok(Resp::ServerError(e)) => return Err(query_error(&e)),
            Ok(reply) => {
                let (next, raw) = scan_values(reply)?;
                let next = parse_scan_cursor(&next)?;
                if raw.is_empty() {
                    walk.position = next;
                    if next == 0 {
                        walk.done = true;
                        return Ok(());
                    }
                    continue;
                }
                walk.state = Some((
                    ScanState {
                        kind,
                        db: call.db(),
                        key: key.to_vec(),
                        pending: to_pending(kind, raw),
                        next,
                    },
                    0,
                ));
                return Ok(());
            }
            // The step is there but too large to read ahead: the next page
            // reads it the usual way.
            Err(_) if call.reply_was_too_large() => {
                call.reconnect().await?;
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The cursor of a `HSCAN`/`SSCAN` reply.
fn parse_scan_cursor(text: &str) -> Result<u64, DriverError> {
    text.parse()
        .map_err(|_| error::protocol(format!("the server returned the scan cursor {text}")))
}

fn pairs(items: Vec<Resp>) -> Vec<(Resp, Resp)> {
    let mut out = Vec::with_capacity(items.len() / 2);
    let mut iter = items.into_iter();
    while let (Some(a), Some(b)) = (iter.next(), iter.next()) {
        out.push((a, b));
    }
    out
}

fn field_values(reply: Resp, values: &mut ValueBudget) -> Result<Vec<FieldValue>, DriverError> {
    let list = match reply {
        Resp::Map(map) => map,
        Resp::Array(items) => pairs(items),
        _ => Vec::new(),
    };
    list.into_iter()
        .map(|(field, value)| {
            Ok(FieldValue {
                field: to_value(field, values)?,
                value: to_value(value, values)?,
            })
        })
        .collect()
}

async fn read_hash(
    call: &mut Call<'_>,
    key: &[u8],
    cursor: &str,
    count: u32,
) -> Result<Page, DriverError> {
    let (total, next, items) = read_scanned(call, ScanKind::Hash, key, cursor, count).await?;
    let entries = items
        .into_iter()
        .filter_map(|item| match item {
            Served::Field(field) => Some(field),
            Served::Member(_) => None,
        })
        .collect();
    Ok(Some((total, next, ValueData::Hash { entries })))
}

fn scan_values(reply: Resp) -> Result<(String, Vec<Resp>), DriverError> {
    let Resp::Array(mut parts) = reply else {
        return Err(error::protocol("unexpected scan reply"));
    };
    let items = parts.pop();
    let cursor = parts.pop().as_ref().and_then(as_text);
    match (cursor, items) {
        (Some(cursor), Some(Resp::Array(items) | Resp::Set(items))) => Ok((cursor, items)),
        (Some(cursor), Some(Resp::Map(map))) => {
            Ok((cursor, map.into_iter().flat_map(|(k, v)| [k, v]).collect()))
        }
        _ => Err(error::protocol("unexpected scan reply")),
    }
}

async fn read_list(
    call: &mut Call<'_>,
    key: &[u8],
    cursor: &str,
    count: u32,
) -> Result<Page, DriverError> {
    let offset = offset_cursor(cursor)?;
    let (total, estimate) = length_and_estimate(call, "LLEN", key).await?;
    let mut budget = PageBudget::new(count, estimate);
    let mut values = ValueBudget::new();
    let mut items = Vec::new();
    let mut next = offset;
    while let Some(n) = budget.next_batch() {
        if total.is_some_and(|t| next >= t) {
            break;
        }
        let start = next;
        let batch = fetch_batch(call, &mut budget, n, |n| {
            let mut range = redis::cmd("LRANGE");
            range
                .arg(key)
                .arg(start)
                .arg(range_end(start, u64::from(n)));
            range
        })
        .await?;
        let Batch::Reply(asked, reply) = batch else {
            match oversized::list_item(call, key, start).await? {
                Some(item) => items.push(item),
                // The list got shorter meanwhile.
                None => break,
            }
            budget.record(1, 0);
            next = start + 1;
            continue;
        };
        let Resp::Array(batch) = reply else {
            return Err(error::protocol("unexpected LRANGE reply"));
        };
        let got = batch.len();
        budget.record(got, batch.iter().map(reply_bytes).sum());
        for (value, index) in batch.into_iter().zip(start..) {
            items.push(ListItem {
                index,
                value: to_value(value, &mut values)?,
            });
        }
        next = start + got as u64;
        if (got as u64) < u64::from(asked) {
            break;
        }
    }
    Ok(Some((
        total,
        total.is_some_and(|t| next < t).then(|| next.to_string()),
        ValueData::List { items },
    )))
}

async fn read_set(
    call: &mut Call<'_>,
    key: &[u8],
    cursor: &str,
    count: u32,
) -> Result<Page, DriverError> {
    let (total, next, items) = read_scanned(call, ScanKind::Set, key, cursor, count).await?;
    let members = items
        .into_iter()
        .filter_map(|item| match item {
            Served::Member(member) => Some(member),
            Served::Field(_) => None,
        })
        .collect();
    Ok(Some((total, next, ValueData::Set { members })))
}

/// A score as a cell value: non-finite scores keep their text.
fn score(reply: &Resp) -> Option<Value> {
    match reply {
        Resp::Double(f) => Some(double_value(*f)),
        Resp::Int(i) => Some(Value::Float(*i as f64)),
        other => as_text(other)?.parse().ok().map(double_value),
    }
}

/// Members and scores of a `ZRANGE ... WITHSCORES` reply. RESP2 sends
/// member, score, member, score; RESP3 sends pairs.
fn zset_members(
    items: Vec<Resp>,
    values: &mut ValueBudget,
) -> Result<Vec<ScoredMember>, DriverError> {
    let nested = items.iter().all(|i| matches!(i, Resp::Array(_))) && !items.is_empty();
    let raw_pairs = if nested {
        items
            .into_iter()
            .filter_map(|pair| match pair {
                Resp::Array(mut two) if two.len() == 2 => {
                    let s = two.pop();
                    let m = two.pop();
                    m.zip(s)
                }
                _ => None,
            })
            .collect()
    } else {
        pairs(items)
    };
    raw_pairs
        .into_iter()
        .map(|(member, score_reply)| {
            Ok(ScoredMember {
                member: to_value(member, values)?,
                score: score(&score_reply).ok_or_else(|| {
                    error::protocol("ZRANGE returned a score that is not a number")
                })?,
            })
        })
        .collect()
}

async fn read_zset(
    call: &mut Call<'_>,
    key: &[u8],
    cursor: &str,
    count: u32,
) -> Result<Page, DriverError> {
    let offset = offset_cursor(cursor)?;
    let (total, estimate) = length_and_estimate(call, "ZCARD", key).await?;
    let mut budget = PageBudget::new(count, estimate);
    let mut values = ValueBudget::new();
    let mut members = Vec::new();
    let mut next = offset;
    while let Some(n) = budget.next_batch() {
        if total.is_some_and(|t| next >= t) {
            break;
        }
        let start = next;
        let batch = fetch_batch(call, &mut budget, n, |n| {
            let mut range = redis::cmd("ZRANGE");
            range
                .arg(key)
                .arg(start)
                .arg(range_end(start, u64::from(n)))
                .arg("WITHSCORES");
            range
        })
        .await?;
        let Batch::Reply(asked, reply) = batch else {
            match oversized::zset_member(call, key, start).await? {
                Some(member) => members.push(member),
                None => break,
            }
            budget.record(1, 0);
            next = start + 1;
            continue;
        };
        let Resp::Array(items) = reply else {
            return Err(error::protocol("unexpected ZRANGE reply"));
        };
        let bytes = items.iter().map(reply_bytes).sum();
        let batch = zset_members(items, &mut values)?;
        let got = batch.len();
        budget.record(got, bytes);
        members.extend(batch);
        next = start + got as u64;
        if (got as u64) < u64::from(asked) {
            break;
        }
    }
    Ok(Some((
        total,
        total.is_some_and(|t| next < t).then(|| next.to_string()),
        ValueData::Zset { members },
    )))
}

/// The smallest stream id after `id` (`<ms>-<seq>`), for exclusive paging
/// that works on every server version.
fn next_stream_id(id: &str) -> Option<String> {
    let (ms, seq) = id.split_once('-')?;
    let ms: u64 = ms.parse().ok()?;
    let seq: u64 = seq.parse().ok()?;
    Some(match seq.checked_add(1) {
        Some(seq) => format!("{ms}-{seq}"),
        None => format!("{}-0", ms.checked_add(1)?),
    })
}

fn stream_entries(
    items: Vec<Resp>,
    values: &mut ValueBudget,
) -> Result<Vec<StreamEntry>, DriverError> {
    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        let Resp::Array(mut parts) = item else {
            return Err(error::protocol("unexpected XRANGE entry"));
        };
        let fields = parts.pop();
        let id = parts.pop().as_ref().and_then(as_text);
        let Some(id) = id else {
            return Err(error::protocol("XRANGE entry without an id"));
        };
        entries.push(StreamEntry {
            id,
            fields: match fields {
                Some(fields) => field_values(fields, values)?,
                None => Vec::new(),
            },
        });
    }
    Ok(entries)
}

async fn read_stream(
    call: &mut Call<'_>,
    key: &[u8],
    cursor: &str,
    count: u32,
) -> Result<Page, DriverError> {
    let mut start = if cursor.is_empty() {
        "-".to_string()
    } else {
        cursor.to_string()
    };
    if start != "-" && next_stream_id(&start).is_none() {
        return Err(bad_cursor(cursor));
    }
    let (total, estimate) = length_and_estimate(call, "XLEN", key).await?;
    let mut budget = PageBudget::new(count, estimate);
    let mut values = ValueBudget::new();
    let mut entries = Vec::new();
    let mut next = None;
    while let Some(n) = budget.next_batch() {
        let from = start.clone();
        let batch = fetch_batch(call, &mut budget, n, |n| {
            let mut range = redis::cmd("XRANGE");
            range.arg(key).arg(&from).arg("+").arg("COUNT").arg(n);
            range
        })
        .await?;
        let Batch::Reply(asked, reply) = batch else {
            let Some(entry) = oversized::stream_entry(call, key, &start).await? else {
                next = None;
                break;
            };
            next = next_stream_id(&entry.id);
            budget.record(1, 0);
            entries.push(entry);
            match &next {
                Some(id) => start.clone_from(id),
                None => break,
            }
            continue;
        };
        let Resp::Array(items) = reply else {
            return Err(error::protocol("unexpected XRANGE reply"));
        };
        let bytes = items.iter().map(reply_bytes).sum();
        let batch = stream_entries(items, &mut values)?;
        let got = batch.len();
        budget.record(got, bytes);
        entries.extend(batch);
        next = if (got as u64) < u64::from(asked) {
            None
        } else {
            entries.last().and_then(|e| next_stream_id(&e.id))
        };
        match &next {
            Some(id) => start.clone_from(id),
            None => break,
        }
    }
    // The page ended on a full batch: make sure something follows, so the
    // last page reports the end instead of leaving an empty page.
    if let Some(from) = &next {
        let mut peek = redis::cmd("XRANGE");
        peek.arg(key).arg(from).arg("+").arg("COUNT").arg(1);
        match call.send(&peek).await {
            Ok(Resp::Array(items)) if items.is_empty() => next = None,
            Ok(Resp::ServerError(e)) => return Err(query_error(&e)),
            Ok(_) => {}
            // An entry follows, too large to read here.
            Err(_) if call.reply_was_too_large() => call.reconnect().await?,
            Err(e) => return Err(e),
        }
    }
    Ok(Some((total, next, ValueData::Stream { entries })))
}

async fn read_json(call: &mut Call<'_>, key: &[u8]) -> Result<Page, DriverError> {
    // Refuse documents too large for the browser before loading them.
    if let Resp::Int(size) = call
        .send(redis::cmd("JSON.DEBUG").arg("MEMORY").arg(key))
        .await?
        && size > MAX_JSON_BYTES
    {
        return Err(DriverError::new(
            ErrorKind::Unsupported,
            format!("The JSON document uses {size} bytes, too large to show here; use the console"),
        )
        .with_code(KeyType::Json.type_name()));
    }
    let reply = call.send(redis::cmd("JSON.GET").arg(key)).await?;
    let text = match reply {
        Resp::Nil => return Ok(None),
        Resp::ServerError(e) => {
            if e.code() == "ERR" && e.details().is_some_and(|d| d.contains("unknown command")) {
                return Err(DriverError::new(
                    ErrorKind::Unsupported,
                    "This server has no JSON support to read ReJSON-RL keys",
                )
                .with_code(KeyType::Json.type_name()));
            }
            return Err(query_error(&e));
        }
        other => as_text(&other).ok_or_else(|| error::protocol("unexpected JSON.GET reply"))?,
    };
    // Kept as the server's text: key order and number precision survive.
    let document = Value::json(text);
    Ok(Some((None, None, ValueData::Json { document })))
}

/// Parses `INFO keyspace` text (`db0:keys=1,expires=0,avg_ttl=0`).
#[must_use]
pub fn parse_keyspace(text: &str) -> Vec<DbKeyspace> {
    let mut out: Vec<DbKeyspace> = info_fields(text)
        .into_iter()
        .filter_map(|(name, values)| {
            let db = name.strip_prefix("db")?.parse().ok()?;
            let mut keys = 0;
            let mut expires = 0;
            for part in values.split(',') {
                match part.split_once('=') {
                    Some(("keys", v)) => keys = v.parse().ok()?,
                    Some(("expires", v)) => expires = v.parse().ok()?,
                    _ => {}
                }
            }
            Some(DbKeyspace { db, keys, expires })
        })
        .collect();
    out.sort_by_key(|d| d.db);
    out
}

/// Unsigned 64-bit numbers cross to the UI as decimal strings: JavaScript
/// numbers lose precision above 2^53, and `SCAN` cursors use the whole
/// range. Input accepts a string or a number.
pub(crate) mod u64_text {
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(super) enum Text {
        Number(u64),
        Text(String),
    }

    impl Text {
        pub(super) fn value<E: serde::de::Error>(self) -> Result<u64, E> {
            match self {
                Text::Number(n) => Ok(n),
                Text::Text(text) => text
                    .parse()
                    .map_err(|_| E::custom(format!("not an unsigned 64-bit number: {text:?}"))),
            }
        }
    }

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        Text::deserialize(deserializer)?.value()
    }
}

/// [`u64_text`] for optional numbers; `None` is `null`.
pub(crate) mod opt_u64_text {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::u64_text::Text;

    #[allow(clippy::ref_option)] // the signature serde's `with` expects
    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.collect_str(value),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        Option::<Text>::deserialize(deserializer)?
            .map(Text::value)
            .transpose()
    }
}

/// Optional signed 64-bit numbers as decimal strings (`PTTL` can exceed
/// 2^53); `None` is `null`. Input accepts a string or a number.
pub(crate) mod opt_i64_text {
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Text {
        Number(i64),
        Text(String),
    }

    #[allow(clippy::ref_option)] // the signature serde's `with` expects
    pub fn serialize<S: Serializer>(value: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.collect_str(value),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<i64>, D::Error> {
        Option::<Text>::deserialize(deserializer)?
            .map(|text| match text {
                Text::Number(n) => Ok(n),
                Text::Text(text) => text.parse().map_err(|_| {
                    serde::de::Error::custom(format!("not a signed 64-bit number: {text:?}"))
                }),
            })
            .transpose()
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        use std::fmt::Write;
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(out, "{b:02x}");
        }
        serializer.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.len() % 2 != 0 {
            return Err(serde::de::Error::custom("hex string has an odd length"));
        }
        (0..text.len())
            .step_by(2)
            .map(|i| {
                text.get(i..i + 2)
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                    .ok_or_else(|| serde::de::Error::custom("invalid hex"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_types_round_trip() {
        for name in [
            "string",
            "list",
            "set",
            "zset",
            "hash",
            "stream",
            "ReJSON-RL",
            "TSDB-TYPE",
        ] {
            let parsed = KeyType::from_type_name(name);
            assert_eq!(parsed.as_ref().map(KeyType::type_name), Some(name));
        }
        assert_eq!(KeyType::from_type_name("none"), None);
        assert_eq!(KeyType::from_type_name("ReJSON-RL"), Some(KeyType::Json));
    }

    #[test]
    fn parses_keyspace() {
        let text = "# Keyspace\r\ndb15:keys=2000,expires=10,avg_ttl=0\r\ndb0:keys=3,expires=0,avg_ttl=0,subexpiry=0\r\n";
        assert_eq!(
            parse_keyspace(text),
            vec![
                DbKeyspace {
                    db: 0,
                    keys: 3,
                    expires: 0
                },
                DbKeyspace {
                    db: 15,
                    keys: 2000,
                    expires: 10
                },
            ]
        );
        assert!(parse_keyspace("# Keyspace\r\n").is_empty());
    }

    #[test]
    fn stream_ids_advance() {
        assert_eq!(next_stream_id("1-0").as_deref(), Some("1-1"));
        assert_eq!(
            next_stream_id(&format!("5-{}", u64::MAX)).as_deref(),
            Some("6-0")
        );
        assert_eq!(next_stream_id("abc"), None);
    }

    #[test]
    fn scan_replies() -> Result<(), DriverError> {
        let reply = Resp::Array(vec![
            Resp::BulkString(b"17".to_vec()),
            Resp::Array(vec![Resp::BulkString(vec![0xff, b'k'])]),
        ]);
        let (cursor, names) = scan_reply(reply)?;
        assert_eq!(cursor, "17");
        assert_eq!(names, vec![vec![0xff, b'k']]);
        assert!(scan_reply(Resp::Nil).is_err());
        Ok(())
    }

    #[test]
    fn key_entries_serialize_names_as_hex() -> Result<(), serde_json::Error> {
        let entry = KeyEntry {
            name: vec![0xff, b'a'],
            display: key_display(&[0xff, b'a']),
            key_type: KeyType::Other("TSDB-TYPE".into()),
            ttl_ms: None,
            placeholder: false,
        };
        let json = serde_json::to_value(&entry)?;
        assert_eq!(json["name"], "ff61");
        assert_eq!(json["display"], "\\xffa");
        assert_eq!(json["keyType"], serde_json::json!({ "other": "TSDB-TYPE" }));
        assert_eq!(json["placeholder"], false);
        let unknown = KeyEntry {
            name: Vec::new(),
            display: "(too large to show: 9 bytes)".into(),
            key_type: KeyType::Unknown,
            ttl_ms: None,
            placeholder: true,
        };
        let unknown_json = serde_json::to_value(&unknown)?;
        assert_eq!(unknown_json["keyType"], "unknown");
        assert_eq!(unknown_json["placeholder"], true);
        assert_eq!(serde_json::from_value::<KeyEntry>(unknown_json)?, unknown);
        // Older payloads without the flag read as not a placeholder.
        let old: KeyEntry =
            serde_json::from_str(r#"{"name":"61","display":"a","keyType":"string","ttlMs":null}"#)?;
        assert!(!old.placeholder);
        let back: KeyEntry = serde_json::from_value(json)?;
        assert_eq!(back, entry);
        Ok(())
    }

    #[test]
    fn u64_numbers_cross_as_strings() -> Result<(), serde_json::Error> {
        let page = ScanPage {
            cursor: u64::MAX,
            keys: Vec::new(),
        };
        let json = serde_json::to_value(&page)?;
        assert_eq!(json["cursor"], "18446744073709551615");
        assert_eq!(serde_json::from_value::<ScanPage>(json)?, page);
        // Numbers are accepted on input too (when exact).
        let from_number: ScanPage = serde_json::from_str(r#"{"cursor":7,"keys":[]}"#)?;
        assert_eq!(from_number.cursor, 7);
        assert!(serde_json::from_str::<ScanPage>(r#"{"cursor":"-1","keys":[]}"#).is_err());

        let info = KeyInfo {
            key_type: KeyType::Hash,
            ttl_ms: Some(i64::MAX),
            memory_bytes: Some(u64::MAX),
            encoding: None,
            length: None,
        };
        let json = serde_json::to_value(&info)?;
        assert_eq!(json["memoryBytes"], "18446744073709551615");
        assert_eq!(json["ttlMs"], "9223372036854775807");
        let from_number: KeyInfo = serde_json::from_str(
            r#"{"keyType":"string","ttlMs":5,"memoryBytes":null,"encoding":null,"length":null}"#,
        )?;
        assert_eq!(from_number.ttl_ms, Some(5));
        assert_eq!(json["length"], serde_json::Value::Null);
        assert_eq!(serde_json::from_value::<KeyInfo>(json)?, info);

        let chunk = ValueChunk {
            key_type: KeyType::List,
            total: Some(u64::MAX),
            next_cursor: Some(u64::MAX.to_string()),
            data: ValueData::List {
                items: vec![ListItem {
                    index: u64::MAX,
                    value: Value::Null,
                }],
            },
        };
        let json = serde_json::to_value(&chunk)?;
        assert_eq!(json["total"], "18446744073709551615");
        assert_eq!(json["nextCursor"], "18446744073709551615");
        assert_eq!(json["data"]["items"][0]["index"], "18446744073709551615");
        assert_eq!(serde_json::from_value::<ValueChunk>(json)?, chunk);

        let text = ValueData::String {
            value: Value::Text("x".into()),
            offset: u64::MAX,
            truncated: false,
        };
        let json = serde_json::to_value(&text)?;
        assert_eq!(json["offset"], "18446744073709551615");
        assert_eq!(serde_json::from_value::<ValueData>(json)?, text);

        let keyspace = DbKeyspace {
            db: 0,
            keys: u64::MAX,
            expires: 1,
        };
        let json = serde_json::to_value(keyspace)?;
        assert_eq!(json["keys"], "18446744073709551615");
        assert_eq!(json["expires"], "1");
        assert_eq!(serde_json::from_value::<DbKeyspace>(json)?, keyspace);
        Ok(())
    }

    #[test]
    fn page_cursors_parse_strictly() -> Result<(), DriverError> {
        assert_eq!(ScanCursor::parse("")?, ScanCursor::Scan(0));
        assert_eq!(
            ScanCursor::parse("18446744073709551615")?,
            ScanCursor::Scan(u64::MAX)
        );
        assert_eq!(
            ScanCursor::parse("p:1a2b")?,
            ScanCursor::State("1a2b".into())
        );
        for bad in [
            "p:",
            "p:xyz",
            "0+1",
            "18446744073709551616",
            "-1",
            " 1",
            "abc",
        ] {
            assert!(ScanCursor::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(offset_cursor("")?, 0);
        assert_eq!(offset_cursor("9223372036854775807")?, MAX_OFFSET);
        for bad in ["9223372036854775808", "18446744073709551615", "-1", "1+1"] {
            assert!(offset_cursor(bad).is_err(), "{bad}");
        }
        assert_eq!(range_end(MAX_OFFSET, 10_000), MAX_OFFSET);
        assert_eq!(range_end(5, 1), 5);
        assert_eq!(range_end(5, 0), 5);
        assert_eq!(next_stream_id(&format!("{}-{}", u64::MAX, u64::MAX)), None);
        Ok(())
    }

    #[test]
    fn page_budget_starts_small_without_sizes() {
        let mut budget = PageBudget::new(500, None);
        assert_eq!(budget.next_batch(), Some(1));
        // One 1 MiB element: three more fit the 4 MiB budget.
        budget.record(1, 1024 * 1024);
        assert_eq!(budget.next_batch(), Some(3));
        budget.record(3, 3 * 1024 * 1024);
        assert_eq!(budget.next_batch(), None);
    }

    #[test]
    fn page_budget_uses_the_estimate_and_the_entry_limit() {
        // Tiny entries: the entry limit decides.
        let budget = PageBudget::new(200, Some(40));
        assert_eq!(budget.next_batch(), Some(200));
        // 64 KiB entries: 64 fit.
        let mut budget = PageBudget::new(10_000, Some(64 * 1024));
        assert_eq!(budget.next_batch(), Some(64));
        // Entries turn out larger than estimated: the larger size wins.
        budget.record(2, 2 * 1024 * 1024);
        assert_eq!(budget.next_batch(), Some(2));
        // The entry limit is honored across batches.
        let mut budget = PageBudget::new(10, Some(1));
        budget.record(7, 7);
        assert_eq!(budget.next_batch(), Some(3));
        budget.record(5, 5);
        assert_eq!(budget.next_batch(), None);
    }

    #[test]
    fn page_budget_shrinks_after_a_reply_over_the_limit() {
        let mut budget = PageBudget::new(1000, Some(10));
        assert_eq!(budget.next_batch(), Some(1000));
        budget.shrink(1000);
        assert_eq!(budget.next_batch(), Some(500));
        budget.shrink(1);
        assert_eq!(budget.next_batch(), Some(1));
    }

    #[test]
    fn reply_bytes_count_element_data() {
        let reply = Resp::Array(vec![
            Resp::BulkString(vec![0; 100]),
            Resp::Array(vec![Resp::SimpleString("abc".into()), Resp::Int(1)]),
            Resp::Map(vec![(Resp::BulkString(b"k".to_vec()), Resp::Nil)]),
        ]);
        assert_eq!(reply_bytes(&reply), 100 + 3 + 8 + 1 + 8);
    }

    #[test]
    fn infinite_scores_keep_their_text() -> Result<(), DriverError> {
        let flat = vec![
            Resp::BulkString(b"low".to_vec()),
            Resp::BulkString(b"-inf".to_vec()),
            Resp::BulkString(b"mid".to_vec()),
            Resp::BulkString(b"2.5".to_vec()),
            Resp::BulkString(b"high".to_vec()),
            Resp::BulkString(b"inf".to_vec()),
        ];
        let members = zset_members(flat, &mut ValueBudget::new())?;
        let scores: Vec<Value> = members.iter().map(|m| m.score.clone()).collect();
        assert_eq!(
            scores,
            [
                Value::Other("-inf".into()),
                Value::Float(2.5),
                Value::Other("inf".into())
            ]
        );
        let nested = vec![Resp::Array(vec![
            Resp::BulkString(b"top".to_vec()),
            Resp::Double(f64::INFINITY),
        ])];
        assert_eq!(
            zset_members(nested, &mut ValueBudget::new())?[0].score,
            Value::Other("inf".into())
        );
        let json =
            serde_json::to_string(&members[2]).map_err(|e| error::protocol(e.to_string()))?;
        assert!(
            json.contains(r#""score":{"t":"other","v":"inf"}"#),
            "{json}"
        );
        assert!(zset_members(vec![Resp::Nil, Resp::Nil], &mut ValueBudget::new()).is_err());
        Ok(())
    }

    #[test]
    fn hash_fields_from_flat_arrays_and_maps() {
        let flat = Resp::Array(vec![
            Resp::BulkString(b"f".to_vec()),
            Resp::BulkString(b"v".to_vec()),
        ]);
        let map = Resp::Map(vec![(
            Resp::BulkString(b"f".to_vec()),
            Resp::BulkString(b"v".to_vec()),
        )]);
        let mut values = ValueBudget::new();
        assert_eq!(
            field_values(flat, &mut values).ok(),
            field_values(map, &mut values).ok()
        );
    }
}
