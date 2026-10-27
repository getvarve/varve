//! Slow log (`SLOWLOG GET`, `LEN`, `RESET`) and latency monitor
//! (`LATENCY LATEST`, `HISTORY`, `DOCTOR`, `RESET`).
//!
//! Differences handled: Redis 8 adds a seventh field to slow log entries
//! and Valkey 8.1 and newer add the sum and count of spikes to
//! `LATENCY LATEST`; both are kept. Arguments that can carry passwords are
//! masked (Redis 6.2 does not redact `CONFIG SET requirepass`).

use serde::{Deserialize, Serialize};
use varve_catalog::{CatalogError, CatalogResult};
use varve_driver::{ServerInfo, Session, Value};

use super::config::config_value;
use super::redact::redact_args;
use super::reply::{Cmd, array, int, invalid, is_unavailable, run, run_batch, text, unexpected};

/// One slow log entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlowLogEntry {
    /// Entry id (increasing).
    pub id: i64,
    /// Unix time the command ran, seconds.
    pub time: i64,
    /// Execution time, microseconds.
    pub duration_us: i64,
    /// Command and arguments (the server shortens long ones). Secrets are
    /// replaced by `(redacted)`.
    pub args: Vec<Value>,
    /// Client address (`ip:port`).
    pub client_addr: Option<String>,
    /// Client name.
    pub client_name: Option<String>,
    /// Fields after the client name that newer servers add (Redis 8).
    pub extra: Vec<Value>,
}

/// The slow log with its configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlowLog {
    /// Newest first.
    pub entries: Vec<SlowLogEntry>,
    /// `SLOWLOG LEN`: entries kept on the server.
    pub length: i64,
    /// `slowlog-log-slower-than`, microseconds (`None` without `CONFIG`
    /// permission).
    pub threshold_us: Option<i64>,
    /// `slowlog-max-len` (`None` without `CONFIG` permission).
    pub max_len: Option<i64>,
}

/// Parses a `SLOWLOG GET` reply.
///
/// # Errors
///
/// [`CatalogError::Unexpected`] for a malformed entry.
pub fn parse_slowlog(reply: &Value) -> CatalogResult<Vec<SlowLogEntry>> {
    let entries = array(reply).ok_or_else(|| unexpected("SLOWLOG GET did not return an array"))?;
    entries
        .iter()
        .map(|entry| {
            let fields =
                array(entry).ok_or_else(|| unexpected("slow log entry is not an array"))?;
            let number = |at: usize| {
                fields
                    .get(at)
                    .and_then(int)
                    .ok_or_else(|| unexpected("slow log entry without id, time or duration"))
            };
            let mut args = fields
                .get(3)
                .and_then(array)
                .map(<[Value]>::to_vec)
                .unwrap_or_default();
            redact_args(&mut args);
            Ok(SlowLogEntry {
                id: number(0)?,
                time: number(1)?,
                duration_us: number(2)?,
                args,
                client_addr: fields.get(4).and_then(text),
                client_name: fields.get(5).and_then(text).filter(|n| !n.is_empty()),
                extra: fields.iter().skip(6).cloned().collect(),
            })
        })
        .collect()
}

/// Optional configuration value as a number: `None` when the server does
/// not allow `CONFIG GET` for this user.
async fn config_number(session: &dyn Session, name: &str) -> CatalogResult<Option<i64>> {
    match config_value(session, name).await {
        Ok(value) => Ok(value.and_then(|v| v.trim().parse().ok())),
        Err(error) if is_unavailable(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reads the newest `count` slow log entries, the log length and its
/// configuration.
///
/// # Errors
///
/// Driver errors.
pub async fn slowlog(
    session: &dyn Session,
    server: &ServerInfo,
    count: u32,
) -> CatalogResult<SlowLog> {
    let _ = server;
    let replies = run_batch(
        session,
        &[
            Cmd::new("SLOWLOG").arg("GET").arg(count.to_string()),
            Cmd::new("SLOWLOG").arg("LEN"),
        ],
    )
    .await?;
    let entries = parse_slowlog(&replies[0])?;
    let length = int(&replies[1]).ok_or_else(|| unexpected("SLOWLOG LEN is not a number"))?;
    Ok(SlowLog {
        entries,
        length,
        threshold_us: config_number(session, "slowlog-log-slower-than").await?,
        max_len: config_number(session, "slowlog-max-len").await?,
    })
}

/// Clears the slow log (`SLOWLOG RESET`).
///
/// # Errors
///
/// Driver errors (a read-only connection refuses it).
pub async fn reset_slowlog(session: &dyn Session, server: &ServerInfo) -> CatalogResult<()> {
    let _ = server;
    run(session, &Cmd::new("SLOWLOG").arg("RESET")).await?;
    Ok(())
}

/// The latest spike of one latency event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyEvent {
    /// Event name (`command`, `fast-command`, `fork`, `expire-cycle`, ...).
    pub event: String,
    /// Unix time of the latest spike, seconds.
    pub time: i64,
    /// Latency of the latest spike, milliseconds.
    pub latest_ms: i64,
    /// All-time maximum, milliseconds.
    pub max_ms: i64,
    /// Sum of all spikes, milliseconds (Valkey 8.1 and newer).
    pub sum_ms: Option<i64>,
    /// Number of spikes (Valkey 8.1 and newer).
    pub count: Option<i64>,
}

/// One point of `LATENCY HISTORY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencySample {
    /// Unix time, seconds.
    pub time: i64,
    /// Latency, milliseconds.
    pub ms: i64,
}

/// `LATENCY LATEST` with the monitor threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyReport {
    /// `latency-monitor-threshold`, milliseconds; 0 means the monitor is
    /// off. `None` without `CONFIG` permission.
    pub threshold_ms: Option<i64>,
    /// Latest spike per event.
    pub events: Vec<LatencyEvent>,
}

/// Parses a `LATENCY LATEST` reply.
///
/// # Errors
///
/// [`CatalogError::Unexpected`] for a malformed entry.
pub fn parse_latency_latest(reply: &Value) -> CatalogResult<Vec<LatencyEvent>> {
    let entries =
        array(reply).ok_or_else(|| unexpected("LATENCY LATEST did not return an array"))?;
    entries
        .iter()
        .map(|entry| {
            let fields = array(entry).ok_or_else(|| unexpected("latency entry is not an array"))?;
            let number = |at: usize| {
                fields
                    .get(at)
                    .and_then(int)
                    .ok_or_else(|| unexpected("latency entry without time or latency"))
            };
            Ok(LatencyEvent {
                event: fields
                    .first()
                    .and_then(text)
                    .ok_or_else(|| unexpected("latency entry without event name"))?,
                time: number(1)?,
                latest_ms: number(2)?,
                max_ms: number(3)?,
                sum_ms: fields.get(4).and_then(int),
                count: fields.get(5).and_then(int),
            })
        })
        .collect()
}

/// Parses a `LATENCY HISTORY` reply.
///
/// # Errors
///
/// [`CatalogError::Unexpected`] for a malformed entry.
pub fn parse_latency_history(reply: &Value) -> CatalogResult<Vec<LatencySample>> {
    let entries =
        array(reply).ok_or_else(|| unexpected("LATENCY HISTORY did not return an array"))?;
    entries
        .iter()
        .map(|entry| match array(entry) {
            Some([time, ms, ..]) => Ok(LatencySample {
                time: int(time).ok_or_else(|| unexpected("latency sample without time"))?,
                ms: int(ms).ok_or_else(|| unexpected("latency sample without latency"))?,
            }),
            _ => Err(unexpected("latency sample is not a pair")),
        })
        .collect()
}

/// Reads `LATENCY LATEST` and the monitor threshold.
///
/// # Errors
///
/// Driver errors.
pub async fn latency(session: &dyn Session, server: &ServerInfo) -> CatalogResult<LatencyReport> {
    let _ = server;
    let reply = run(session, &Cmd::new("LATENCY").arg("LATEST")).await?;
    Ok(LatencyReport {
        events: parse_latency_latest(&reply)?,
        threshold_ms: config_number(session, "latency-monitor-threshold").await?,
    })
}

fn check_event(event: &str) -> CatalogResult<()> {
    if event.is_empty() || event.contains(char::is_whitespace) {
        return Err(invalid(format!("{event:?} is not a latency event name")));
    }
    Ok(())
}

/// Reads `LATENCY HISTORY <event>` (up to 160 samples, oldest first).
///
/// # Errors
///
/// [`CatalogError::Invalid`] for an empty name; driver errors.
pub async fn latency_history(
    session: &dyn Session,
    server: &ServerInfo,
    event: &str,
) -> CatalogResult<Vec<LatencySample>> {
    let _ = server;
    check_event(event)?;
    let reply = run(session, &Cmd::new("LATENCY").arg("HISTORY").arg(event)).await?;
    parse_latency_history(&reply)
}

/// Reads the `LATENCY DOCTOR` report text.
///
/// # Errors
///
/// Driver errors.
pub async fn latency_doctor(session: &dyn Session, server: &ServerInfo) -> CatalogResult<String> {
    let _ = server;
    let reply = run(session, &Cmd::new("LATENCY").arg("DOCTOR")).await?;
    text(&reply).ok_or_else(|| unexpected("LATENCY DOCTOR did not return text"))
}

/// Clears the latency history of `events` (all events when empty) and
/// returns how many event histories were cleared.
///
/// # Errors
///
/// [`CatalogError::Invalid`] for an empty event name; driver errors.
pub async fn reset_latency(
    session: &dyn Session,
    server: &ServerInfo,
    events: &[String],
) -> CatalogResult<i64> {
    let _ = server;
    for event in events {
        check_event(event)?;
    }
    let reply = run(session, &Cmd::new("LATENCY").arg("RESET").args(events)).await?;
    int(&reply).ok_or_else(|| CatalogError::Unexpected {
        message: "LATENCY RESET did not return a number".into(),
    })
}
