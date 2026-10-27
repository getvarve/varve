//! Content digests of whole keys, read in pieces.
//!
//! A [`crate::KeyVersion`] carries a digest of the key's content so a
//! whole-key change (delete, rename, trim) can tell whether anyone changed
//! the key since it was read, also when the length stayed the same. The
//! digest is read in pieces on a side connection and never holds the key:
//!
//! - strings: `GETRANGE` in [`STRING_PIECE`] slices, hashed in order;
//! - lists: `LRANGE` pages of [`PAGE`], hashed in order;
//! - streams: `XRANGE` pages of [`PAGE`] (ids and fields), hashed in order;
//! - hashes, sets and sorted sets: `HSCAN`/`SSCAN`/`ZSCAN` steps, every
//!   element hashed on its own and the element hashes combined without
//!   order (a scan has no stable order and can return an element twice;
//!   duplicates are counted once);
//! - JSON: `DUMP`, read as a stream, only after its size was checked
//!   without walking it ([`JSON_DIGEST_ELEMENTS`], [`JSON_DIGEST_BYTES`]):
//!   the server builds the whole `DUMP` reply at once and blocks meanwhile.
//!   When a key is opened only a small document ([`OPEN_JSON_ELEMENTS`],
//!   [`OPEN_DIGEST_BYTES`]).
//! - time series: `TS.RANGE` pages of [`PAGE`] samples (timestamps and
//!   values), hashed in order; the length is the sample count, read with
//!   a `count` aggregation (never `TS.INFO`, which breaks a `WATCH`).
//!   Other module types are not read at all.
//!
//! It stops after [`DIGEST_TIME_BUDGET`] or [`DIGEST_MAX_ELEMENTS`]
//! elements ([`Digest::TooLarge`]); a refused command (an ACL user without
//! the permission) gives [`Digest::NotPermitted`]. At apply time it runs
//! inside the edit's `WATCH`, so a change while it reads aborts the
//! transaction; a read racing a change can only give a false conflict.
//!
//! Opening a key in the value viewer only takes the digest of a small key
//! ([`Budget::open`]: at most [`OPEN_DIGEST_BYTES`] bytes and
//! [`OPEN_DIGEST_ELEMENTS`] elements, within [`OPEN_DIGEST_TIME`]); a larger
//! key's digest is taken when a whole-key change is planned
//! ([`crate::VersionReader`]), on its own connection.

use std::collections::HashSet;
use std::hash::Hasher;
use std::time::{Duration, Instant};

use varve_driver::DriverError;

use crate::browser::KeyType;
use crate::measure::{Head, RawConn};
use crate::modules::ModuleKind;

/// Most time one digest may take (2 s); a key that takes longer is too
/// large to check.
pub const DIGEST_TIME_BUDGET: Duration = Duration::from_secs(2);

/// Most elements of a hash, set or sorted set one digest keeps track of.
pub const DIGEST_MAX_ELEMENTS: usize = 2_000_000;

/// Bytes of a string read per `GETRANGE` (1 MiB).
pub const STRING_PIECE: u64 = 1024 * 1024;

/// Elements per `LRANGE`/`XRANGE` page and `COUNT` of a scan step.
pub const PAGE: u64 = 1000;

/// Most bytes of values read for the digest taken when a key is opened
/// (1 MiB); a larger key's digest waits for a plan that needs it.
pub const OPEN_DIGEST_BYTES: u64 = 1024 * 1024;

/// Most elements of a key whose digest is taken when it is opened.
pub const OPEN_DIGEST_ELEMENTS: u64 = 10_000;

/// Most time the digest taken when a key is opened may take.
pub const OPEN_DIGEST_TIME: Duration = Duration::from_millis(500);

/// Most top-level elements (array items, object members) of a JSON key
/// whose digest is read (with `DUMP`, which the server builds at once).
pub const JSON_DIGEST_ELEMENTS: u64 = 10_000;

/// Most memory (`JSON.DEBUG MEMORY`) of a JSON key whose digest is read.
pub const JSON_DIGEST_BYTES: u64 = 16 * 1024 * 1024;

/// Most top-level elements of a JSON key whose digest is read when it is
/// opened (and at most [`OPEN_DIGEST_BYTES`] of memory). Only then is
/// `JSON.DEBUG MEMORY` asked, which walks the document (about 0.2 ms per
/// MB on Redis 8.10, against about 4 ms per MB for `DUMP`).
pub const OPEN_JSON_ELEMENTS: u64 = 1000;

/// What one digest may read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    /// The bounded read when a key is opened: `DUMP` only of a small JSON
    /// document.
    opening: bool,
    deadline: Instant,
    /// Bytes of values still allowed; `None`: no byte limit.
    bytes_left: Option<u64>,
    /// Most elements a key may have to be read at all (strings: the byte
    /// limit applies).
    max_length: Option<u64>,
}

impl Budget {
    /// The full digest of a plan or an apply: [`DIGEST_TIME_BUDGET`], no
    /// byte limit.
    pub(crate) fn full() -> Self {
        Self {
            opening: false,
            deadline: Instant::now() + DIGEST_TIME_BUDGET,
            bytes_left: None,
            max_length: None,
        }
    }

    /// The bounded digest taken when a key is opened. Running out of it
    /// can leave the connection in the middle of a reply, so it is only
    /// used on a connection that is closed afterwards.
    pub(crate) fn open() -> Self {
        Self {
            opening: true,
            deadline: Instant::now() + OPEN_DIGEST_TIME,
            bytes_left: Some(OPEN_DIGEST_BYTES),
            max_length: Some(OPEN_DIGEST_ELEMENTS),
        }
    }

    fn expired(&self) -> bool {
        Instant::now() > self.deadline
    }

    /// Takes `len` bytes; false when they do not fit.
    fn take(&mut self, len: u64) -> bool {
        match &mut self.bytes_left {
            None => true,
            Some(left) if *left >= len => {
                *left -= len;
                true
            }
            Some(_) => false,
        }
    }
}

/// The outcome of a digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Digest {
    /// The digest.
    Done(String),
    /// The key is too large to read within the budget.
    TooLarge,
    /// The server refused a command the digest needs.
    NotPermitted,
    /// A module key that is not read: its size cannot be checked, and
    /// `DUMP` builds the whole value at once on the server.
    Module,
}

fn hasher() -> std::hash::DefaultHasher {
    // SipHash with fixed keys: equal within one run of the app, which is
    // all a version token needs (plans live 30 minutes).
    std::hash::DefaultHasher::new()
}

/// A refused command: the error line was read, the connection is in step.
fn refused(error: &DriverError) -> bool {
    error.code.is_some()
}

/// Reads the digest of `key` of type `key_type` on `raw` within `budget`.
/// [`Digest::TooLarge`] when the key does not fit it; with a byte limit
/// the connection may then be in the middle of a reply (see
/// [`Budget::open`]).
///
/// # Errors
///
/// Connection and protocol errors (a server error reply is
/// [`Digest::NotPermitted`]).
pub(crate) async fn content_digest<S>(
    raw: &mut RawConn<S>,
    key: &[u8],
    key_type: &KeyType,
    length: Option<u64>,
    mut budget: Budget,
) -> Result<Digest, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if let (Some(max), Some(length)) = (budget.max_length, length) {
        let too_long = match key_type {
            KeyType::String => budget.bytes_left.is_some_and(|left| length > left),
            _ => length > max,
        };
        if too_long {
            return Ok(Digest::TooLarge);
        }
    }
    let b = &mut budget;
    let outcome = match key_type {
        KeyType::String => string(raw, key, length.unwrap_or(0), b).await,
        KeyType::List => list(raw, key, b).await,
        KeyType::Stream => stream(raw, key, b).await,
        KeyType::Hash => scanned(raw, b"HSCAN", key, 2, b).await,
        KeyType::Set => scanned(raw, b"SSCAN", key, 1, b).await,
        KeyType::Zset => scanned(raw, b"ZSCAN", key, 2, b).await,
        KeyType::Json => json(raw, key, b).await,
        KeyType::Other(name)
            if ModuleKind::from_type_name(name) == Some(ModuleKind::TimeSeries) =>
        {
            time_series(raw, key, b).await
        }
        _ if b.opening => Ok(Digest::Module),
        KeyType::Other(_) | KeyType::Unknown => Ok(Digest::Module),
    };
    match outcome {
        Err(error) if refused(&error) => Ok(Digest::NotPermitted),
        other => other,
    }
}

/// Reads a bulk string reply into `h` in pieces (its length first, so
/// concatenations stay apart; a marker for nil), without holding it,
/// within the byte budget. False when it does not fit: its body is then
/// left unread. A bounded and a full digest of the same key are equal.
async fn bulk<S>(
    raw: &mut RawConn<S>,
    h: &mut impl Hasher,
    budget: &mut Budget,
) -> Result<bool, DriverError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let len = match raw.next().await? {
        Head::Bulk(Some(len)) => len,
        Head::Bulk(None) => {
            h.write_u8(0);
            return Ok(true);
        }
        other => return Err(crate::measure::unexpected(&format!("{other:?}"))),
    };
    if !budget.take(len) {
        return Ok(false);
    }
    h.write_u8(1);
    h.write_u64(len);
    raw.hash_body(len, h).await?;
    Ok(true)
}

fn done(kind: &str, hash: u64) -> Digest {
    Digest::Done(format!("{kind}-{hash:016x}"))
}

async fn string<S>(
    raw: &mut RawConn<S>,
    key: &[u8],
    length: u64,
    budget: &mut Budget,
) -> Result<Digest, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut h = hasher();
    let mut offset = 0u64;
    while offset < length {
        let end = (offset + STRING_PIECE - 1).min(length - 1);
        if budget.expired() || !budget.take(end - offset + 1) {
            return Ok(Digest::TooLarge);
        }
        raw.send(&[
            b"GETRANGE",
            key,
            offset.to_string().as_bytes(),
            end.to_string().as_bytes(),
        ])
        .await?;
        match raw.next().await? {
            Head::Bulk(Some(len)) => {
                raw.hash_body(len, &mut h).await?;
                if len == 0 {
                    break;
                }
                offset += len;
            }
            other => return Err(crate::measure::unexpected(&format!("{other:?}"))),
        }
    }
    h.write_u64(offset);
    Ok(done("s", h.finish()))
}

async fn list<S>(
    raw: &mut RawConn<S>,
    key: &[u8],
    budget: &mut Budget,
) -> Result<Digest, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut h = hasher();
    let mut start = 0u64;
    loop {
        if budget.expired() {
            return Ok(Digest::TooLarge);
        }
        let stop = start + PAGE - 1;
        raw.send(&[
            b"LRANGE",
            key,
            start.to_string().as_bytes(),
            stop.to_string().as_bytes(),
        ])
        .await?;
        let n = raw.array().await?;
        for _ in 0..n {
            if !bulk(raw, &mut h, budget).await? {
                return Ok(Digest::TooLarge);
            }
        }
        start += n;
        if n < PAGE {
            break;
        }
    }
    h.write_u64(start);
    Ok(done("l", h.finish()))
}

async fn stream<S>(
    raw: &mut RawConn<S>,
    key: &[u8],
    budget: &mut Budget,
) -> Result<Digest, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut h = hasher();
    let mut start = "-".to_string();
    let mut count = 0u64;
    loop {
        if budget.expired() {
            return Ok(Digest::TooLarge);
        }
        raw.send(&[
            b"XRANGE",
            key,
            start.as_bytes(),
            b"+",
            b"COUNT",
            PAGE.to_string().as_bytes(),
        ])
        .await?;
        let n = raw.array().await?;
        let mut last = None;
        for _ in 0..n {
            if raw.array().await? != 2 {
                return Err(crate::measure::unexpected(
                    "a stream entry without two parts",
                ));
            }
            let id = raw.short().await?;
            h.write(id.as_bytes());
            let fields = raw.array().await?;
            h.write_u64(fields);
            for _ in 0..fields {
                if !bulk(raw, &mut h, budget).await? {
                    return Ok(Digest::TooLarge);
                }
            }
            last = Some(id);
        }
        count += n;
        match last {
            Some(id) if n == PAGE => start = format!("({id}"),
            _ => break,
        }
    }
    h.write_u64(count);
    Ok(done("x", h.finish()))
}

/// Bytes a sample counts against the byte budget (timestamp and value).
const SAMPLE_BYTES: u64 = 16;

/// The number of samples of a time series; `None` when the server refuses
/// the read (the error line is read).
///
/// # Errors
///
/// Connection and protocol errors.
pub(crate) async fn ts_length<S>(
    raw: &mut RawConn<S>,
    key: &[u8],
) -> Result<Option<u64>, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Not `TS.INFO`: it counts as a change of the key for a `WATCH`, and
    // this runs inside the `WATCH` of an edit. One `count` bucket as wide
    // as the timestamps go (two at most) holds every sample.
    // `countall` (Redis 8) counts NaN samples too; older servers only
    // have `count`, which skips them. Either way both reads of one version
    // use the same aggregation, and the digest covers the NaN samples.
    let width = i64::MAX.to_string();
    let mut reply = None;
    for aggregation in [&b"countall"[..], b"count"] {
        raw.send(&[
            b"TS.RANGE",
            key,
            b"-",
            b"+",
            b"AGGREGATION",
            aggregation,
            width.as_bytes(),
        ])
        .await?;
        match raw.read_resp(crate::modules::INFO_REPLY_LIMIT).await {
            Ok(found) => {
                reply = Some(found);
                break;
            }
            Err(error) if refused(&error) => {}
            Err(error) => return Err(error),
        }
    }
    let Some(reply) = reply else {
        return Ok(None);
    };
    let redis::Value::Array(buckets) = reply else {
        return Err(crate::measure::unexpected(
            "a TS.RANGE reply that is not an array",
        ));
    };
    let mut total = 0u64;
    for bucket in &buckets {
        let redis::Value::Array(parts) = bucket else {
            return Err(crate::measure::unexpected(
                "a TS.RANGE bucket that is not a pair",
            ));
        };
        let count = parts
            .get(1)
            .and_then(crate::reply::as_text)
            .and_then(|t| t.trim().parse::<f64>().ok())
            .ok_or_else(|| crate::measure::unexpected("a TS.RANGE count that is not a number"))?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let count = count as u64;
        total += count;
    }
    Ok(Some(total))
}

async fn time_series<S>(
    raw: &mut RawConn<S>,
    key: &[u8],
    budget: &mut Budget,
) -> Result<Digest, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut h = hasher();
    let mut from = "-".to_string();
    let mut count = 0u64;
    loop {
        if budget.expired() {
            return Ok(Digest::TooLarge);
        }
        raw.send(&[
            b"TS.RANGE",
            key,
            from.as_bytes(),
            b"+",
            b"COUNT",
            PAGE.to_string().as_bytes(),
        ])
        .await?;
        let n = raw.array().await?;
        let mut last = None;
        for _ in 0..n {
            if raw.array().await? != 2 {
                return Err(crate::measure::unexpected("a sample without two parts"));
            }
            let timestamp = match raw.next().await? {
                Head::Int(t) => t,
                other => return Err(crate::measure::unexpected(&format!("{other:?}"))),
            };
            let value = match raw.next().await? {
                Head::Simple(text) => text,
                Head::Bulk(Some(len)) if len <= 512 => {
                    String::from_utf8_lossy(&raw.keep(len).await?).into_owned()
                }
                other => return Err(crate::measure::unexpected(&format!("{other:?}"))),
            };
            if !budget.take(SAMPLE_BYTES) {
                return Ok(Digest::TooLarge);
            }
            h.write_i64(timestamp);
            h.write(value.as_bytes());
            h.write_u8(0);
            last = Some(timestamp);
        }
        count += n;
        match last.and_then(|t| t.checked_add(1)) {
            Some(next) if n == PAGE => from = next.to_string(),
            _ => break,
        }
    }
    h.write_u64(count);
    Ok(done("t", h.finish()))
}

/// Hashes, sets and sorted sets: every element (`parts` replies: a field
/// and its value, a member, or a member and its score) hashed alone, the
/// distinct element hashes combined without order.
async fn scanned<S>(
    raw: &mut RawConn<S>,
    command: &[u8],
    key: &[u8],
    parts: u64,
    budget: &mut Budget,
) -> Result<Digest, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut seen: HashSet<u64> = HashSet::new();
    let mut cursor = 0u64;
    loop {
        if budget.expired() || seen.len() > DIGEST_MAX_ELEMENTS {
            return Ok(Digest::TooLarge);
        }
        raw.send(&[
            command,
            key,
            cursor.to_string().as_bytes(),
            b"COUNT",
            PAGE.to_string().as_bytes(),
        ])
        .await?;
        let (next, n) = raw.scan_frame().await?;
        if n % parts != 0 {
            return Err(crate::measure::unexpected("a scan reply of odd parts"));
        }
        for _ in 0..n / parts {
            let mut h = hasher();
            for _ in 0..parts {
                if !bulk(raw, &mut h, budget).await? {
                    return Ok(Digest::TooLarge);
                }
            }
            seen.insert(h.finish());
        }
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    let (mut sum, mut xor) = (0u64, 0u64);
    for hash in &seen {
        sum = sum.wrapping_add(*hash);
        xor ^= hash.rotate_left(17);
    }
    let mut h = hasher();
    h.write_u64(seen.len() as u64);
    h.write_u64(sum);
    h.write_u64(xor);
    Ok(done("c", h.finish()))
}

/// A JSON key: its size is checked first with commands that do not walk
/// it (`JSON.TYPE`, then `JSON.ARRLEN`, `JSON.OBJLEN` or `JSON.STRLEN` at
/// the root: at most [`JSON_DIGEST_ELEMENTS`]), then `JSON.DEBUG MEMORY`
/// (at most [`JSON_DIGEST_BYTES`]); only then is it read with `DUMP`.
async fn json<S>(
    raw: &mut RawConn<S>,
    key: &[u8],
    budget: &mut Budget,
) -> Result<Digest, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    raw.send(&[b"JSON.TYPE", key]).await?;
    let root = match raw.next().await? {
        Head::Simple(name) => name,
        Head::Bulk(Some(len)) => String::from_utf8_lossy(&raw.keep(len).await?).into_owned(),
        other => return Err(crate::measure::unexpected(&format!("{other:?}"))),
    };
    let count: Option<&[u8]> = match root.as_str() {
        "array" => Some(b"JSON.ARRLEN"),
        "object" => Some(b"JSON.OBJLEN"),
        "string" => Some(b"JSON.STRLEN"),
        _ => None,
    };
    let (max_bytes, max_elements) = if budget.opening {
        (OPEN_DIGEST_BYTES, OPEN_JSON_ELEMENTS)
    } else {
        (JSON_DIGEST_BYTES, JSON_DIGEST_ELEMENTS)
    };
    if let Some(command) = count {
        raw.send(&[command, key]).await?;
        let n = raw.int_reply().await?;
        let limit = if root == "string" {
            max_bytes
        } else {
            max_elements
        };
        if u64::try_from(n).unwrap_or(u64::MAX) > limit {
            return Ok(Digest::TooLarge);
        }
    }
    raw.send(&[b"JSON.DEBUG", b"MEMORY", key]).await?;
    let bytes = raw.int_reply().await?;
    if u64::try_from(bytes).unwrap_or(u64::MAX) > max_bytes {
        return Ok(Digest::TooLarge);
    }
    dump(raw, key, budget).await
}

async fn dump<S>(
    raw: &mut RawConn<S>,
    key: &[u8],
    budget: &mut Budget,
) -> Result<Digest, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    raw.send(&[b"DUMP", key]).await?;
    let mut h = hasher();
    if !bulk(raw, &mut h, budget).await? {
        return Ok(Digest::TooLarge);
    }
    Ok(done("d", h.finish()))
}

/// The absolute expiry of `key` in unix ms on the server's clock; `None`
/// when it does not expire (or does not exist). Exact with `PEXPIRETIME`
/// (Redis 7.0 and newer, Valkey); on older servers `TIME` plus `PTTL`,
/// sent together (off by a few milliseconds at most).
///
/// # Errors
///
/// Connection and protocol errors.
pub(crate) async fn expire_at<S>(
    raw: &mut RawConn<S>,
    key: &[u8],
    exact: bool,
) -> Result<Option<i64>, DriverError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if exact {
        raw.send(&[b"PEXPIRETIME", key]).await?;
        let at = raw.int_reply().await?;
        return Ok((at >= 0).then_some(at));
    }
    let mut out = Vec::new();
    crate::measure::encode(&[b"TIME"], &mut out);
    crate::measure::encode(&[b"PTTL", key], &mut out);
    raw.send_raw(&out).await?;
    if raw.array().await? != 2 {
        return Err(crate::measure::unexpected("TIME without two parts"));
    }
    let seconds: i64 = raw
        .short()
        .await?
        .parse()
        .map_err(|_| crate::measure::unexpected("TIME seconds"))?;
    let micros: i64 = raw
        .short()
        .await?
        .parse()
        .map_err(|_| crate::measure::unexpected("TIME microseconds"))?;
    let ttl = raw.int_reply().await?;
    Ok((ttl >= 0).then(|| seconds * 1000 + micros / 1000 + ttl))
}
