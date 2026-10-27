//! Memory analysis: `MEMORY USAGE` per key prefix and the biggest keys per
//! type from one `SCAN` pass (never `KEYS`) with a key and time budget,
//! plus `MEMORY STATS` and `MEMORY DOCTOR`.
//!
//! The scan runs on the session's current database. Every scanned key is
//! measured, so the result covers a sample of the keyspace when a budget
//! ends the scan early; [`MemoryAnalysis::complete`] says whether it did.
//!
//! The time budget bounds the work: `SCAN COUNT` is clamped to
//! [`MAX_SCAN_COUNT`], keys are measured in batches of at most
//! [`MAX_MEASURE_BATCH`] keys sized from the time the previous batch took,
//! and the budget is checked before every `SCAN` step and every batch. The
//! scan can overrun the budget by at most about one batch.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use varve_catalog::{CatalogError, CatalogResult};
use varve_driver::{ServerInfo, Session, Value};

use super::reply::{
    Cmd, Fields, array, bytes, int, invalid, name_value, run, run_batch, text, unexpected,
};

/// Largest `SCAN COUNT` sent; larger requests are clamped.
pub const MAX_SCAN_COUNT: u32 = 1_000;

/// Most keys measured in one call (two commands each: `TYPE` and
/// `MEMORY USAGE`).
pub const MAX_MEASURE_BATCH: usize = 100;

/// Most biggest keys kept per type.
pub const MAX_TOP_KEYS: usize = 100;

/// Most prefixes returned; the rest are summed in
/// [`MemoryAnalysis::other_prefixes`].
pub const MAX_PREFIXES: usize = 1_000;

/// Options of [`analyze_memory`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryScanOptions {
    /// `SCAN MATCH` pattern; `None` for all keys.
    pub pattern: Option<String>,
    /// Separator of key prefixes (default `:`).
    pub delimiter: String,
    /// Prefix depth: how many delimiter-separated parts form the prefix
    /// (default 1, at least 1).
    pub depth: usize,
    /// Most keys to scan and measure (default 10,000).
    pub max_keys: usize,
    /// Most time to spend (default 10 seconds).
    pub time_budget_ms: u64,
    /// `SCAN COUNT` hint per step (default 500, clamped to
    /// 1..=[`MAX_SCAN_COUNT`]).
    pub scan_count: u32,
    /// `MEMORY USAGE ... SAMPLES` for nested values (default 5, the
    /// server default; 0 measures every element and is slow).
    pub samples: u32,
    /// Biggest keys kept per type (default 10, at most [`MAX_TOP_KEYS`]).
    pub top_keys: usize,
    /// Prefixes returned, biggest first (default 100, clamped to
    /// 1..=[`MAX_PREFIXES`]).
    pub max_prefixes: usize,
}

impl Default for MemoryScanOptions {
    fn default() -> Self {
        Self {
            pattern: None,
            delimiter: ":".into(),
            depth: 1,
            max_keys: 10_000,
            time_budget_ms: 10_000,
            scan_count: 500,
            samples: 5,
            top_keys: 10,
            max_prefixes: 100,
        }
    }
}

/// Memory of all scanned keys sharing one prefix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrefixUsage {
    /// The prefix including its trailing delimiter (`user:`); `None` for
    /// keys without the delimiter.
    pub prefix: Option<Value>,
    /// Keys scanned with this prefix.
    pub keys: u64,
    /// Total `MEMORY USAGE` of those keys, bytes.
    pub bytes: u64,
    /// Average per key, bytes.
    pub avg_bytes: u64,
    /// Largest key of this prefix.
    pub max_key: Value,
    /// Its size, bytes.
    pub max_bytes: u64,
    /// `bytes` for the whole database: equal to `bytes` after a complete
    /// scan; scaled by `DBSIZE` / keys scanned after an incomplete scan
    /// without a pattern; `None` after an incomplete scan with a pattern,
    /// where the number of matching keys is unknown.
    pub estimated_bytes: Option<u64>,
}

/// Totals of the prefixes beyond [`MemoryScanOptions::max_prefixes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherPrefixes {
    /// Prefixes not listed.
    pub prefixes: u64,
    /// Their keys.
    pub keys: u64,
    /// Their total size, bytes.
    pub bytes: u64,
    /// As [`PrefixUsage::estimated_bytes`].
    pub estimated_bytes: Option<u64>,
}

/// One big key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BigKey {
    /// Key name.
    pub key: Value,
    /// `MEMORY USAGE`, bytes.
    pub bytes: u64,
    /// Elements (`STRLEN`, `LLEN`, `HLEN`, `SCARD`, `ZCARD`, `XLEN`); `None`
    /// for module types, when the key changed meanwhile, or when the time
    /// budget was used up before the lengths were read.
    pub length: Option<i64>,
}

/// Big keys of one type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeUsage {
    /// `TYPE` name (`string`, `hash`, `ReJSON-RL`, ...).
    pub key_type: String,
    /// Keys of this type scanned.
    pub keys: u64,
    /// Their total size, bytes.
    pub bytes: u64,
    /// Biggest first.
    pub biggest: Vec<BigKey>,
}

/// Why the scan stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScanStop {
    /// The whole database was scanned.
    Complete,
    /// The key budget was used up.
    KeyBudget,
    /// The time budget was used up.
    TimeBudget,
    /// The user stopped the analysis ([`analyze_memory_until`]); the result
    /// holds what was measured until then.
    Stopped,
}

/// Result of [`analyze_memory`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAnalysis {
    /// Keys scanned and measured.
    pub scanned_keys: u64,
    /// `DBSIZE` at the start.
    pub db_size: i64,
    /// True when the scan reached the end of the database; false for a
    /// partial result cut short by a budget.
    pub complete: bool,
    /// Why the scan stopped.
    pub stopped_by: ScanStop,
    /// Time spent, milliseconds.
    pub elapsed_ms: u64,
    /// Total size of the measured keys, bytes.
    pub total_bytes: u64,
    /// Prefixes with the most bytes, biggest first, at most
    /// [`MemoryScanOptions::max_prefixes`].
    pub prefixes: Vec<PrefixUsage>,
    /// Totals of the other prefixes, when there were more.
    pub other_prefixes: Option<OtherPrefixes>,
    /// Per type, biggest total first.
    pub types: Vec<TypeUsage>,
}

/// One measured key.
#[derive(Debug, Clone)]
pub(crate) struct Measured {
    pub(crate) key: Vec<u8>,
    pub(crate) key_type: String,
    pub(crate) bytes: u64,
}

/// The prefix of `key`: its first `depth` parts and the delimiter.
pub(crate) fn key_prefix(key: &[u8], delimiter: &[u8], depth: usize) -> Option<Vec<u8>> {
    if delimiter.is_empty() {
        return None;
    }
    let mut end = 0;
    let mut found = 0;
    let mut at = 0;
    while at + delimiter.len() <= key.len() {
        if &key[at..at + delimiter.len()] == delimiter {
            found += 1;
            end = at + delimiter.len();
            if found == depth.max(1) {
                break;
            }
            at += delimiter.len();
        } else {
            at += 1;
        }
    }
    (found > 0).then(|| key[..end].to_vec())
}

/// How measured bytes scale to the whole database.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Scale {
    /// The scan was complete: estimates equal the measured bytes.
    Exact,
    /// Multiply by this factor (`DBSIZE` / keys scanned).
    Factor(f64),
    /// No estimate (incomplete scan with a pattern).
    Unknown,
}

impl Scale {
    pub(crate) fn of(complete: bool, pattern: bool, scanned: u64, db_size: i64) -> Self {
        if complete {
            Self::Exact
        } else if pattern || scanned == 0 {
            Self::Unknown
        } else {
            #[allow(clippy::cast_precision_loss)]
            let factor = (db_size.max(0) as f64 / scanned as f64).max(1.0);
            Self::Factor(factor)
        }
    }

    fn apply(self, bytes: u64) -> Option<u64> {
        match self {
            Self::Exact => Some(bytes),
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            Self::Factor(factor) => Some((bytes as f64 * factor).round() as u64),
            Self::Unknown => None,
        }
    }
}

#[derive(Default)]
struct PrefixAcc {
    keys: u64,
    bytes: u64,
    max_key: Vec<u8>,
    max_bytes: u64,
}

/// Groups measured keys by prefix and type in one pass (hash maps), keeps
/// the `max_prefixes` biggest prefixes and sums the rest.
pub(crate) fn summarize(
    measured: &[Measured],
    options: &MemoryScanOptions,
    scale: Scale,
) -> (Vec<PrefixUsage>, Option<OtherPrefixes>, Vec<TypeUsage>) {
    let mut prefixes: HashMap<Option<Vec<u8>>, PrefixAcc> = HashMap::new();
    let mut types: HashMap<&str, (u64, u64, Vec<&Measured>)> = HashMap::new();
    for m in measured {
        let prefix = key_prefix(&m.key, options.delimiter.as_bytes(), options.depth);
        let acc = prefixes.entry(prefix).or_default();
        acc.keys += 1;
        acc.bytes += m.bytes;
        if m.bytes > acc.max_bytes || acc.keys == 1 {
            acc.max_key.clone_from(&m.key);
            acc.max_bytes = m.bytes;
        }
        let t = types.entry(m.key_type.as_str()).or_default();
        t.0 += 1;
        t.1 += m.bytes;
        t.2.push(m);
    }
    let mut prefixes: Vec<(Option<Vec<u8>>, PrefixAcc)> = prefixes.into_iter().collect();
    prefixes.sort_unstable_by(|a, b| {
        b.1.bytes
            .cmp(&a.1.bytes)
            .then(b.1.keys.cmp(&a.1.keys))
            .then_with(|| a.0.cmp(&b.0))
    });
    let limit = options.max_prefixes.clamp(1, MAX_PREFIXES);
    let other = (prefixes.len() > limit).then(|| {
        let rest = &prefixes[limit..];
        let bytes = rest.iter().map(|(_, a)| a.bytes).sum();
        OtherPrefixes {
            prefixes: rest.len() as u64,
            keys: rest.iter().map(|(_, a)| a.keys).sum(),
            bytes,
            estimated_bytes: scale.apply(bytes),
        }
    });
    prefixes.truncate(limit);
    let prefixes = prefixes
        .into_iter()
        .map(|(prefix, acc)| PrefixUsage {
            prefix: prefix.map(name_value),
            keys: acc.keys,
            bytes: acc.bytes,
            avg_bytes: acc.bytes / acc.keys.max(1),
            max_key: name_value(acc.max_key),
            max_bytes: acc.max_bytes,
            estimated_bytes: scale.apply(acc.bytes),
        })
        .collect();
    let top = options.top_keys.min(MAX_TOP_KEYS);
    let mut types: Vec<TypeUsage> = types
        .into_iter()
        .map(|(key_type, (keys, bytes, mut members))| {
            members.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.key.cmp(&b.key)));
            TypeUsage {
                key_type: key_type.to_string(),
                keys,
                bytes,
                biggest: members
                    .into_iter()
                    .take(top)
                    .map(|m| BigKey {
                        key: name_value(m.key.clone()),
                        bytes: m.bytes,
                        length: None,
                    })
                    .collect(),
            }
        })
        .collect();
    types.sort_unstable_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.key_type.cmp(&b.key_type))
    });
    (prefixes, other, types)
}

fn length_command(key_type: &str) -> Option<&'static str> {
    Some(match key_type {
        "string" => "STRLEN",
        "list" => "LLEN",
        "hash" => "HLEN",
        "set" => "SCARD",
        "zset" => "ZCARD",
        "stream" => "XLEN",
        _ => return None,
    })
}

fn validate(options: &MemoryScanOptions) -> CatalogResult<()> {
    if options.max_keys == 0 {
        return Err(invalid("The key budget must be at least 1"));
    }
    if options.depth == 0 {
        return Err(invalid("The prefix depth must be at least 1"));
    }
    Ok(())
}

/// Tracks the time budget and sizes measurement batches to fit it.
struct Budget {
    started: Instant,
    limit: Duration,
    /// Measured time per key so far.
    per_key: Option<Duration>,
}

impl Budget {
    fn used_up(&self) -> bool {
        self.started.elapsed() >= self.limit
    }

    /// Keys for the next batch: what fits the remaining time at the speed
    /// seen so far, between 1 and [`MAX_MEASURE_BATCH`].
    fn batch_size(&self) -> usize {
        let remaining = self.limit.saturating_sub(self.started.elapsed());
        match self.per_key {
            Some(per_key) if !per_key.is_zero() => {
                let fit = remaining.as_nanos() / per_key.as_nanos();
                usize::try_from(fit)
                    .unwrap_or(MAX_MEASURE_BATCH)
                    .clamp(1, MAX_MEASURE_BATCH)
            }
            // First batch: start small to learn the round-trip time.
            _ => 10,
        }
    }

    fn record(&mut self, keys: usize, took: Duration) {
        if keys > 0 {
            self.per_key = Some(took / u32::try_from(keys).unwrap_or(u32::MAX));
        }
    }
}

/// Measures one batch of keys: `TYPE` and `MEMORY USAGE` each.
async fn measure(
    session: &dyn Session,
    keys: Vec<Vec<u8>>,
    samples: u32,
    out: &mut Vec<Measured>,
) -> CatalogResult<()> {
    let mut cmds = Vec::with_capacity(keys.len() * 2);
    for key in &keys {
        cmds.push(Cmd::new("TYPE").arg(key));
        cmds.push(
            Cmd::new("MEMORY")
                .arg("USAGE")
                .arg(key)
                .arg("SAMPLES")
                .arg(samples.to_string()),
        );
    }
    let replies = run_batch(session, &cmds).await?;
    for (key, pair) in keys.into_iter().zip(replies.chunks(2)) {
        let [key_type, usage] = pair else { continue };
        // A key deleted since SCAN returned it has type `none` and no usage.
        let (Some(key_type), Some(usage)) = (text(key_type), int(usage)) else {
            continue;
        };
        if key_type == "none" {
            continue;
        }
        out.push(Measured {
            key,
            key_type,
            bytes: u64::try_from(usage).unwrap_or(0),
        });
    }
    Ok(())
}

/// Scans the current database within the key and time budget, measures
/// every scanned key with `MEMORY USAGE`, and groups the sizes per prefix
/// and per type. The biggest keys get their element count when time is
/// left.
///
/// # Errors
///
/// [`CatalogError::Invalid`] for a zero key budget or depth; driver errors.
pub async fn analyze_memory(
    session: &dyn Session,
    server: &ServerInfo,
    options: &MemoryScanOptions,
) -> CatalogResult<MemoryAnalysis> {
    analyze_memory_until(session, server, options, &AtomicBool::new(false)).await
}

/// [`analyze_memory`] that also stops when `stop` is set: the scan loop
/// checks it before every `SCAN` step and every measurement batch (each
/// bounded: at most [`MAX_SCAN_COUNT`] keys per step and
/// [`MAX_MEASURE_BATCH`] per batch), so a stop takes effect after at most
/// one round trip. The result keeps what was measured, with
/// [`ScanStop::Stopped`] and `complete: false`; the element counts of the
/// biggest keys are then skipped.
///
/// # Errors
///
/// As [`analyze_memory`].
pub async fn analyze_memory_until(
    session: &dyn Session,
    server: &ServerInfo,
    options: &MemoryScanOptions,
    stop: &AtomicBool,
) -> CatalogResult<MemoryAnalysis> {
    let _ = server;
    validate(options)?;
    let mut budget = Budget {
        started: Instant::now(),
        limit: Duration::from_millis(options.time_budget_ms),
        per_key: None,
    };
    let scan_count = options.scan_count.clamp(1, MAX_SCAN_COUNT).to_string();
    let db_size = int(&run(session, &Cmd::new("DBSIZE")).await?)
        .ok_or_else(|| unexpected("DBSIZE is not a number"))?;
    let mut measured: Vec<Measured> = Vec::new();
    let mut pending: Vec<Vec<u8>> = Vec::new();
    let mut cursor = String::from("0");
    let mut scan_done = false;
    let stopped_by = loop {
        if pending.is_empty() && scan_done {
            break ScanStop::Complete;
        }
        if stop.load(Ordering::SeqCst) {
            break ScanStop::Stopped;
        }
        if measured.len() >= options.max_keys {
            break ScanStop::KeyBudget;
        }
        if budget.used_up() {
            break ScanStop::TimeBudget;
        }
        if pending.is_empty() {
            let mut scan = Cmd::new("SCAN").arg(&cursor);
            if let Some(pattern) = &options.pattern {
                scan = scan.arg("MATCH").arg(pattern);
            }
            scan = scan.arg("COUNT").arg(&scan_count);
            let reply = run(session, &scan).await?;
            let (next, keys) = match array(&reply) {
                Some([next, keys]) => (
                    text(next).ok_or_else(|| unexpected("SCAN cursor is not text"))?,
                    array(keys).ok_or_else(|| unexpected("SCAN keys are not an array"))?,
                ),
                _ => return Err(unexpected("SCAN did not return a cursor and keys")),
            };
            // Measured from the end of `pending`; reverse to keep SCAN order.
            pending.extend(keys.iter().rev().filter_map(bytes));
            scan_done = next == "0";
            cursor = next;
            continue;
        }
        let room = options.max_keys - measured.len();
        let take = budget.batch_size().min(room).min(pending.len());
        let batch: Vec<Vec<u8>> = pending
            .split_off(pending.len() - take)
            .into_iter()
            .rev()
            .collect();
        let started = Instant::now();
        measure(session, batch, options.samples, &mut measured).await?;
        budget.record(take, started.elapsed());
    };
    let complete = stopped_by == ScanStop::Complete;
    let scale = Scale::of(
        complete,
        options.pattern.is_some(),
        measured.len() as u64,
        db_size,
    );
    let (prefixes, other_prefixes, mut types) = summarize(&measured, options, scale);
    if stopped_by != ScanStop::Stopped {
        add_lengths(session, &mut types, &budget, stop).await?;
    }
    Ok(MemoryAnalysis {
        scanned_keys: measured.len() as u64,
        db_size,
        complete,
        stopped_by,
        elapsed_ms: u64::try_from(budget.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        total_bytes: measured.iter().map(|m| m.bytes).sum(),
        prefixes,
        other_prefixes,
        types,
    })
}

/// Fills in the element counts of the big keys, in batches, while time is
/// left. A key that changed type since it was scanned makes a batch fail
/// with `WRONGTYPE`; that batch is then read one by one and the key's
/// length stays `None`.
async fn add_lengths(
    session: &dyn Session,
    types: &mut [TypeUsage],
    budget: &Budget,
    stop: &AtomicBool,
) -> CatalogResult<()> {
    let mut targets: Vec<(usize, usize, Cmd)> = Vec::new();
    for (t, usage) in types.iter().enumerate() {
        let Some(command) = length_command(&usage.key_type) else {
            continue;
        };
        for (k, big) in usage.biggest.iter().enumerate() {
            if let Some(key) = bytes(&big.key) {
                targets.push((t, k, Cmd::new(command).arg(key)));
            }
        }
    }
    for chunk in targets.chunks(MAX_MEASURE_BATCH) {
        if budget.used_up() || stop.load(Ordering::SeqCst) {
            break;
        }
        let cmds: Vec<Cmd> = chunk.iter().map(|(_, _, c)| c.clone()).collect();
        let lengths: Vec<Option<i64>> = match run_batch(session, &cmds).await {
            Ok(replies) => replies.iter().map(int).collect(),
            Err(CatalogError::Driver { error }) if error.code.as_deref() == Some("WRONGTYPE") => {
                let mut one_by_one = Vec::with_capacity(cmds.len());
                for cmd in &cmds {
                    one_by_one.push(match run(session, cmd).await {
                        Ok(reply) => int(&reply),
                        Err(CatalogError::Driver { error })
                            if error.code.as_deref() == Some("WRONGTYPE") =>
                        {
                            None
                        }
                        Err(error) => return Err(error),
                    });
                }
                one_by_one
            }
            Err(error) => return Err(error),
        };
        for ((t, k, _), length) in chunk.iter().zip(lengths) {
            if let Some(big) = types.get_mut(*t).and_then(|u| u.biggest.get_mut(*k)) {
                big.length = length;
            }
        }
    }
    Ok(())
}

/// Memory of one key (`MEMORY USAGE`); `None` when the key does not exist.
///
/// # Errors
///
/// Driver errors.
pub async fn key_memory(
    session: &dyn Session,
    server: &ServerInfo,
    key: &[u8],
    samples: u32,
) -> CatalogResult<Option<u64>> {
    let _ = server;
    let reply = run(
        session,
        &Cmd::new("MEMORY")
            .arg("USAGE")
            .arg(key)
            .arg("SAMPLES")
            .arg(samples.to_string()),
    )
    .await?;
    Ok(int(&reply).and_then(|n| u64::try_from(n).ok()))
}

/// One database's overhead in `MEMORY STATS`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbOverhead {
    /// Database index.
    pub db: u32,
    /// `overhead.hashtable.main`, bytes.
    pub main_bytes: Option<i64>,
    /// `overhead.hashtable.expires`, bytes.
    pub expires_bytes: Option<i64>,
}

/// A `MEMORY STATS` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatField {
    /// Name as reported.
    pub name: String,
    /// The value: integer, float or text.
    pub value: Value,
}

/// `MEMORY STATS`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStats {
    /// `peak.allocated`, bytes.
    pub peak_allocated: Option<i64>,
    /// `total.allocated`, bytes.
    pub total_allocated: Option<i64>,
    /// `startup.allocated`, bytes.
    pub startup_allocated: Option<i64>,
    /// `overhead.total`, bytes.
    pub overhead_total: Option<i64>,
    /// `dataset.bytes`.
    pub dataset_bytes: Option<i64>,
    /// `dataset.percentage`.
    pub dataset_percentage: Option<f64>,
    /// `keys.count`.
    pub keys_count: Option<i64>,
    /// `keys.bytes-per-key`.
    pub bytes_per_key: Option<i64>,
    /// `fragmentation` ratio.
    pub fragmentation: Option<f64>,
    /// `db.N` entries.
    pub databases: Vec<DbOverhead>,
    /// Every scalar field in reply order (the set grows with each release).
    pub fields: Vec<StatField>,
}

/// Parses a `MEMORY STATS` reply. Ratios arrive as strings (RESP2) or
/// doubles (RESP3).
///
/// # Errors
///
/// [`CatalogError::Unexpected`] when the reply is not a map.
pub fn parse_memory_stats(reply: &Value) -> CatalogResult<MemoryStats> {
    let fields = Fields::of(reply).ok_or_else(|| unexpected("MEMORY STATS is not a map"))?;
    let mut databases = Vec::new();
    let mut scalars = Vec::new();
    for (name, value) in fields.entries() {
        if let Some(db) = name.strip_prefix("db.").and_then(|n| n.parse().ok()) {
            let inner = Fields::of(value);
            databases.push(DbOverhead {
                db,
                main_bytes: inner
                    .as_ref()
                    .and_then(|f| f.int("overhead.hashtable.main")),
                expires_bytes: inner
                    .as_ref()
                    .and_then(|f| f.int("overhead.hashtable.expires")),
            });
        } else if !matches!(value, Value::Array(_)) {
            let value = match value {
                Value::Text(s) => s
                    .parse::<i64>()
                    .map(Value::Int)
                    .or_else(|_| s.parse::<f64>().map(Value::Float))
                    .unwrap_or_else(|_| Value::Text(s.clone())),
                other => (*other).clone(),
            };
            scalars.push(StatField {
                name: name.clone(),
                value,
            });
        }
    }
    Ok(MemoryStats {
        peak_allocated: fields.int("peak.allocated"),
        total_allocated: fields.int("total.allocated"),
        startup_allocated: fields.int("startup.allocated"),
        overhead_total: fields.int("overhead.total"),
        dataset_bytes: fields.int("dataset.bytes"),
        dataset_percentage: fields.float("dataset.percentage"),
        keys_count: fields.int("keys.count"),
        bytes_per_key: fields.int("keys.bytes-per-key"),
        fragmentation: fields.float("fragmentation"),
        databases,
        fields: scalars,
    })
}

/// Reads `MEMORY STATS`.
///
/// # Errors
///
/// Driver errors.
pub async fn memory_stats(
    session: &dyn Session,
    server: &ServerInfo,
) -> CatalogResult<MemoryStats> {
    let _ = server;
    parse_memory_stats(&run(session, &Cmd::new("MEMORY").arg("STATS")).await?)
}

/// Reads the `MEMORY DOCTOR` report text.
///
/// # Errors
///
/// Driver errors.
pub async fn memory_doctor(session: &dyn Session, server: &ServerInfo) -> CatalogResult<String> {
    let _ = server;
    let reply = run(session, &Cmd::new("MEMORY").arg("DOCTOR")).await?;
    text(&reply).ok_or_else(|| unexpected("MEMORY DOCTOR did not return text"))
}
