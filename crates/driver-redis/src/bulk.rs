//! Bulk time to live changes (design doc 4.2, "TTL: E"): set or clear the
//! time to live of every key that matches a key browser filter.
//!
//! A [`BulkTtl`] works on short-lived connections of its own (client name
//! `<name>-bulk`, set up like the session's, on the database the session
//! was on when it was made), never on the session connection, so the
//! console stays free while a long run goes on:
//!
//! - [`BulkTtl::count`] previews how many keys match, within a key and a
//!   time limit (the count is then a lower bound);
//! - [`BulkTtl::apply`] walks the keyspace with `SCAN ... MATCH ... COUNT
//!   ... [TYPE ...]` in steps of at most [`BULK_BATCH`] keys and sends one
//!   pipeline of `PEXPIRE` or `PERSIST` per step. It checks its stop flag
//!   before every step, so a stop takes effect after at most one step, and
//!   reports progress in [`BulkProgress`] as it goes.
//!
//! `SCAN` can return a key more than once while the keyspace changes; a
//! repeated `PEXPIRE` sets the same time to live again (from that moment)
//! and is counted again. A key created after the run passed its slot is
//! not changed. Keys that vanish meanwhile are counted as skipped.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use redis::Value as Resp;
use serde::{Deserialize, Serialize};
use varve_driver::{DriverError, ErrorKind};

use crate::browser::KeyType;
use crate::measure::{self, RawConn, encode};
use crate::parse::{ParsedCommand, key_display};
use crate::reply::as_text;
use crate::session::{MAX_REPLY_BYTES, RedisSession};

/// Most keys changed by one pipeline (and the `COUNT` of each `SCAN`).
pub const BULK_BATCH: u32 = 500;

/// Suffix of the client name of bulk connections.
pub const BULK_NAME_SUFFIX: &str = "-bulk";

/// Longest one step (a `SCAN` plus its pipeline) may take before the run
/// fails with a timeout.
pub const BULK_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// `COUNT` of the `SCAN` steps of a preview.
const COUNT_STEP: u32 = 1000;

/// Key names shown in a preview.
const SAMPLE_KEYS: usize = 20;

/// Which keys a bulk change takes: the key browser's filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkFilter {
    /// `SCAN ... MATCH` pattern; empty is `*`.
    pub pattern: String,
    /// `SCAN ... TYPE` filter.
    #[serde(default)]
    pub key_type: Option<KeyType>,
}

impl BulkFilter {
    fn pattern(&self) -> &str {
        if self.pattern.is_empty() {
            "*"
        } else {
            &self.pattern
        }
    }

    /// The filter in words, for plans and the activity log.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.key_type {
            Some(t) => format!("keys matching {} of type {}", self.pattern(), t.type_name()),
            None => format!("keys matching {}", self.pattern()),
        }
    }
}

/// What a bulk change does to every key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BulkTtlAction {
    /// `PEXPIRE key ttl_ms`.
    #[serde(rename_all = "camelCase")]
    Expire {
        /// Time to live in ms, at least 1.
        #[serde(with = "crate::browser::u64_text")]
        ttl_ms: u64,
    },
    /// `PERSIST key`: the keys no longer expire.
    Persist,
}

impl BulkTtlAction {
    /// Checks the values.
    ///
    /// # Errors
    ///
    /// A Query error with code `INVALID_RANGE` for a time to live of 0 or
    /// above `i64::MAX`.
    pub fn validate(self) -> Result<(), DriverError> {
        match self {
            Self::Expire { ttl_ms } if ttl_ms == 0 || i64::try_from(ttl_ms).is_err() => Err(
                crate::modules::invalid("The time to live must be at least 1 ms"),
            ),
            _ => Ok(()),
        }
    }

    /// The command name.
    #[must_use]
    pub fn command(self) -> &'static str {
        match self {
            Self::Expire { .. } => "PEXPIRE",
            Self::Persist => "PERSIST",
        }
    }
}

/// A preview of a bulk change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkCount {
    /// Keys that match (counted once each; a key `SCAN` returned twice
    /// counts twice).
    #[serde(with = "crate::browser::u64_text")]
    pub keys: u64,
    /// True when the whole keyspace was scanned; otherwise `keys` is a
    /// lower bound (the limit was reached).
    pub complete: bool,
    /// The first few matching key names, display text.
    pub sample: Vec<String>,
    /// The logical database counted.
    pub db: u16,
    /// The server runs in cluster mode: `SCAN` (and so the count and the
    /// change) only reaches the keys of the node this connection is on.
    pub cluster: bool,
}

/// Progress of a running [`BulkTtl::apply`], shared with whoever watches.
#[derive(Debug, Default)]
pub struct BulkProgress {
    /// `SCAN` steps done.
    pub steps: AtomicU64,
    /// Keys the filter matched so far.
    pub matched: AtomicU64,
    /// Keys whose time to live changed (`PEXPIRE` or `PERSIST` replied 1).
    pub changed: AtomicU64,
    /// Matched keys that were not changed: gone meanwhile, or (`PERSIST`)
    /// without a time to live.
    pub skipped: AtomicU64,
}

impl BulkProgress {
    /// The counts now.
    #[must_use]
    pub fn snapshot(&self) -> BulkCounts {
        BulkCounts {
            steps: self.steps.load(Ordering::SeqCst),
            matched: self.matched.load(Ordering::SeqCst),
            changed: self.changed.load(Ordering::SeqCst),
            skipped: self.skipped.load(Ordering::SeqCst),
        }
    }
}

/// A snapshot of [`BulkProgress`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkCounts {
    /// `SCAN` steps done.
    #[serde(with = "crate::browser::u64_text")]
    pub steps: u64,
    /// Keys matched.
    #[serde(with = "crate::browser::u64_text")]
    pub matched: u64,
    /// Keys changed.
    #[serde(with = "crate::browser::u64_text")]
    pub changed: u64,
    /// Keys skipped.
    #[serde(with = "crate::browser::u64_text")]
    pub skipped: u64,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BulkEnd {
    /// The whole keyspace was walked.
    Complete,
    /// The stop flag was set; the keys changed so far stay changed.
    Stopped,
}

/// Bulk TTL changes on connections of their own. Get one with
/// [`RedisSession::bulk_ttl`].
#[derive(Clone)]
pub struct BulkTtl {
    endpoint: crate::conn::Endpoint,
    init: Vec<ParsedCommand>,
    db: u16,
}

impl std::fmt::Debug for BulkTtl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BulkTtl")
            .field("endpoint", &self.endpoint.display())
            .field("db", &self.db)
            .finish_non_exhaustive()
    }
}

impl RedisSession {
    /// A bulk TTL runner on the database the session is on now.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::ReadOnly`] on a read-only session.
    pub async fn bulk_ttl(&self) -> Result<BulkTtl, DriverError> {
        if self.is_read_only() {
            return Err(DriverError::new(
                ErrorKind::ReadOnly,
                "This connection is read-only, so time to live values cannot be changed",
            ));
        }
        Ok(BulkTtl {
            endpoint: self.endpoint().clone(),
            init: self.init_commands().to_vec(),
            db: self.current_db().await,
        })
    }
}

/// One `SCAN` step: the next cursor and the key names.
async fn scan_step(
    raw: &mut RawConn,
    cursor: u64,
    filter: &BulkFilter,
    count: u32,
) -> Result<(u64, Vec<Vec<u8>>), DriverError> {
    let cursor = cursor.to_string();
    let count = count.to_string();
    let mut args: Vec<&[u8]> = vec![
        b"SCAN",
        cursor.as_bytes(),
        b"MATCH",
        filter.pattern().as_bytes(),
        b"COUNT",
        count.as_bytes(),
    ];
    if let Some(key_type) = &filter.key_type {
        args.push(b"TYPE");
        args.push(key_type.type_name().as_bytes());
    }
    raw.send(&args).await?;
    let reply = raw.read_resp(MAX_REPLY_BYTES).await?;
    let Resp::Array(mut parts) = reply else {
        return Err(measure::unexpected("a SCAN reply that is not an array"));
    };
    let names = parts.pop();
    let next = parts.pop().as_ref().and_then(as_text);
    let (Some(next), Some(Resp::Array(names))) = (next, names) else {
        return Err(measure::unexpected(
            "a SCAN reply without a cursor and keys",
        ));
    };
    let next = next
        .parse()
        .map_err(|_| measure::unexpected(&format!("SCAN cursor {next}")))?;
    let names = names
        .into_iter()
        .filter_map(|n| match n {
            Resp::BulkString(bytes) => Some(bytes),
            other => as_text(&other).map(String::into_bytes),
        })
        .collect();
    Ok((next, names))
}

/// `cluster_enabled:1` in `INFO cluster`.
fn cluster_enabled(info: &str) -> bool {
    crate::conn::info_fields(info)
        .iter()
        .any(|(name, value)| *name == "cluster_enabled" && *value == "1")
}

fn step_timeout() -> DriverError {
    DriverError::new(
        ErrorKind::Timeout,
        format!(
            "One step of the bulk change took longer than {} s",
            BULK_STEP_TIMEOUT.as_secs()
        ),
    )
}

impl BulkTtl {
    /// The database it works on.
    #[must_use]
    pub fn db(&self) -> u16 {
        self.db
    }

    async fn open(&self) -> Result<RawConn, DriverError> {
        measure::open_named(&self.endpoint, self.db, &self.init, BULK_NAME_SUFFIX).await
    }

    /// Counts the keys `filter` matches, scanning at most `max_keys` keys
    /// or for `time_limit`, whichever comes first.
    ///
    /// # Errors
    ///
    /// Connect errors, server errors (an invalid pattern or type), Timeout.
    pub async fn count(
        &self,
        filter: &BulkFilter,
        max_keys: u64,
        time_limit: Duration,
    ) -> Result<BulkCount, DriverError> {
        let mut raw = self.open().await?;
        raw.send(&[b"INFO", b"cluster"]).await?;
        let cluster = match raw.read_resp(64 * 1024).await? {
            Resp::BulkString(text) => cluster_enabled(&String::from_utf8_lossy(&text)),
            _ => false,
        };
        let started = Instant::now();
        let mut cursor = 0u64;
        let mut keys = 0u64;
        let mut sample = Vec::new();
        let complete = loop {
            let (next, names) = tokio::time::timeout(
                BULK_STEP_TIMEOUT,
                scan_step(&mut raw, cursor, filter, COUNT_STEP),
            )
            .await
            .map_err(|_| step_timeout())??;
            keys += names.len() as u64;
            for name in names {
                if sample.len() >= SAMPLE_KEYS {
                    break;
                }
                sample.push(key_display(&name));
            }
            cursor = next;
            if cursor == 0 {
                break true;
            }
            if keys >= max_keys || started.elapsed() >= time_limit {
                break false;
            }
        };
        Ok(BulkCount {
            keys,
            complete,
            sample,
            db: self.db,
            cluster,
        })
    }

    /// Sets or clears the time to live of every key `filter` matches (see
    /// the module documentation). Stops before the next step once `stop`
    /// is set.
    ///
    /// # Errors
    ///
    /// Connect errors, server errors (an ACL user without `PEXPIRE`: the
    /// keys changed before stay changed, `progress` says how many),
    /// Timeout of a step.
    pub async fn apply(
        &self,
        filter: &BulkFilter,
        action: BulkTtlAction,
        stop: &AtomicBool,
        progress: &BulkProgress,
    ) -> Result<BulkEnd, DriverError> {
        action.validate()?;
        let mut raw = self.open().await?;
        let ttl = match action {
            BulkTtlAction::Expire { ttl_ms } => ttl_ms.to_string(),
            BulkTtlAction::Persist => String::new(),
        };
        let mut cursor = 0u64;
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(BulkEnd::Stopped);
            }
            let step = async {
                let (next, names) = scan_step(&mut raw, cursor, filter, BULK_BATCH).await?;
                progress
                    .matched
                    .fetch_add(names.len() as u64, Ordering::SeqCst);
                if !names.is_empty() {
                    let mut out = Vec::new();
                    for name in &names {
                        match action {
                            BulkTtlAction::Expire { .. } => {
                                encode(&[b"PEXPIRE", name, ttl.as_bytes()], &mut out);
                            }
                            BulkTtlAction::Persist => encode(&[b"PERSIST", name], &mut out),
                        }
                    }
                    raw.send_raw(&out).await?;
                    let mut first_error = None;
                    for _ in &names {
                        match raw.int_reply().await {
                            Ok(1) => {
                                progress.changed.fetch_add(1, Ordering::SeqCst);
                            }
                            Ok(_) => {
                                progress.skipped.fetch_add(1, Ordering::SeqCst);
                            }
                            // The error line was read: keep reading the rest.
                            Err(error) if error.code.is_some() => {
                                if first_error.is_none() {
                                    first_error = Some(error);
                                }
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    if let Some(error) = first_error {
                        return Err(error);
                    }
                }
                progress.steps.fetch_add(1, Ordering::SeqCst);
                Ok::<_, DriverError>(next)
            };
            cursor = tokio::time::timeout(BULK_STEP_TIMEOUT, step)
                .await
                .map_err(|_| step_timeout())??;
            if cursor == 0 {
                return Ok(BulkEnd::Complete);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_mode_is_read_from_info() {
        assert!(cluster_enabled("# Cluster\r\ncluster_enabled:1\r\n"));
        assert!(!cluster_enabled("# Cluster\r\ncluster_enabled:0\r\n"));
        assert!(!cluster_enabled(""));
    }

    #[test]
    fn actions_validate_and_deserialize() -> Result<(), serde_json::Error> {
        let action: BulkTtlAction =
            serde_json::from_value(serde_json::json!({ "kind": "expire", "ttlMs": "60000" }))?;
        assert_eq!(action, BulkTtlAction::Expire { ttl_ms: 60_000 });
        assert!(action.validate().is_ok());
        assert!(BulkTtlAction::Expire { ttl_ms: 0 }.validate().is_err());
        assert!(
            BulkTtlAction::Expire { ttl_ms: u64::MAX }
                .validate()
                .is_err()
        );
        let persist: BulkTtlAction =
            serde_json::from_value(serde_json::json!({ "kind": "persist" }))?;
        assert_eq!(persist.command(), "PERSIST");
        let filter: BulkFilter =
            serde_json::from_value(serde_json::json!({ "pattern": "", "keyType": "hash" }))?;
        assert_eq!(filter.describe(), "keys matching * of type hash");
        Ok(())
    }
}
