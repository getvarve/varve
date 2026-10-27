//! Sentinel topology: monitored primaries (`SENTINEL MASTERS`) with their
//! replicas (`SENTINEL REPLICAS`) and the other Sentinels
//! (`SENTINEL SENTINELS`).
//!
//! The development matrix runs no Sentinel; the parsers are unit tested
//! with replies captured from Sentinels of Redis 6.2, Redis 8.10 and
//! Valkey 9 monitoring a primary with one replica.

use serde::{Deserialize, Serialize};
use varve_catalog::CatalogResult;
use varve_driver::{ServerInfo, Session, Value};

use super::info::{InfoField, RedisInfo, ServerMode};
use super::reply::{Cmd, Fields, array, run, text, unexpected};

/// A server as a Sentinel reports it (a primary, replica or Sentinel).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentinelPeer {
    /// `name` (the primary's name, or `ip:port` / run id for others).
    pub name: String,
    /// `ip`.
    pub ip: Option<String>,
    /// `port`.
    pub port: Option<i64>,
    /// `runid`.
    pub run_id: Option<String>,
    /// `flags` split on commas (`master`, `slave`, `sentinel`, `s_down`,
    /// `o_down`, `disconnected`, ...).
    pub flags: Vec<String>,
    /// Flagged subjectively or objectively down.
    pub down: bool,
    /// `last-ok-ping-reply`, milliseconds ago.
    pub last_ok_ping_reply_ms: Option<i64>,
    /// `master-link-status` (replicas).
    pub master_link_status: Option<String>,
    /// `slave-repl-offset` (replicas).
    pub repl_offset: Option<i64>,
    /// Every field as reported.
    pub fields: Vec<InfoField>,
}

/// A monitored primary.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentinelMaster {
    /// The primary.
    pub master: SentinelPeer,
    /// `quorum`.
    pub quorum: Option<i64>,
    /// `num-slaves`.
    pub num_replicas: Option<i64>,
    /// `num-other-sentinels`.
    pub num_other_sentinels: Option<i64>,
    /// `down-after-milliseconds`.
    pub down_after_ms: Option<i64>,
    /// `failover-timeout`, milliseconds.
    pub failover_timeout_ms: Option<i64>,
    /// `parallel-syncs`.
    pub parallel_syncs: Option<i64>,
    /// `config-epoch`.
    pub config_epoch: Option<i64>,
    /// Replicas of this primary.
    pub replicas: Vec<SentinelPeer>,
    /// The other Sentinels monitoring it.
    pub sentinels: Vec<SentinelPeer>,
}

/// What one Sentinel knows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentinelTopology {
    /// Monitored primaries.
    pub masters: Vec<SentinelMaster>,
}

/// Parses one flat field map of a `SENTINEL` reply.
#[must_use]
pub fn parse_peer(value: &Value) -> Option<SentinelPeer> {
    let f = Fields::of(value)?;
    let flags: Vec<String> = f
        .text("flags")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    Some(SentinelPeer {
        name: f.text("name").unwrap_or_default(),
        ip: f.text("ip"),
        port: f.int("port"),
        run_id: f.text("runid"),
        down: flags.iter().any(|x| x == "s_down" || x == "o_down"),
        flags,
        last_ok_ping_reply_ms: f.int("last-ok-ping-reply"),
        master_link_status: f.text("master-link-status"),
        repl_offset: f.int("slave-repl-offset"),
        fields: f
            .entries()
            .iter()
            .filter_map(|(k, v)| {
                Some(InfoField {
                    key: k.clone(),
                    value: text(v)?,
                })
            })
            .collect(),
    })
}

/// Parses a `SENTINEL REPLICAS` or `SENTINEL SENTINELS` reply.
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Unexpected`] for a malformed reply.
pub fn parse_peers(reply: &Value) -> CatalogResult<Vec<SentinelPeer>> {
    array(reply)
        .ok_or_else(|| unexpected("SENTINEL reply is not an array"))?
        .iter()
        .map(|p| parse_peer(p).ok_or_else(|| unexpected("SENTINEL entry is not a map")))
        .collect()
}

/// Parses a `SENTINEL MASTERS` reply (replicas and Sentinels empty).
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Unexpected`] for a malformed reply.
pub fn parse_masters(reply: &Value) -> CatalogResult<Vec<SentinelMaster>> {
    array(reply)
        .ok_or_else(|| unexpected("SENTINEL MASTERS is not an array"))?
        .iter()
        .map(|m| {
            let f = Fields::of(m).ok_or_else(|| unexpected("SENTINEL entry is not a map"))?;
            Ok(SentinelMaster {
                quorum: f.int("quorum"),
                num_replicas: f.int("num-slaves"),
                num_other_sentinels: f.int("num-other-sentinels"),
                down_after_ms: f.int("down-after-milliseconds"),
                failover_timeout_ms: f.int("failover-timeout"),
                parallel_syncs: f.int("parallel-syncs"),
                config_epoch: f.int("config-epoch"),
                master: parse_peer(m).unwrap_or_default(),
                replicas: Vec::new(),
                sentinels: Vec::new(),
            })
        })
        .collect()
}

/// Reads the topology a Sentinel knows; `None` when the server is not a
/// Sentinel.
///
/// # Errors
///
/// Driver errors.
pub async fn sentinel_topology(
    session: &dyn Session,
    server: &ServerInfo,
) -> CatalogResult<Option<SentinelTopology>> {
    let _ = server;
    let info = run(session, &Cmd::new("INFO").arg("server")).await?;
    if RedisInfo::parse(&text(&info).unwrap_or_default()).mode != ServerMode::Sentinel {
        return Ok(None);
    }
    let mut masters = parse_masters(&run(session, &Cmd::new("SENTINEL").arg("MASTERS")).await?)?;
    for master in &mut masters {
        let name = master.master.name.clone();
        master.replicas =
            parse_peers(&run(session, &Cmd::new("SENTINEL").arg("REPLICAS").arg(&name)).await?)?;
        master.sentinels =
            parse_peers(&run(session, &Cmd::new("SENTINEL").arg("SENTINELS").arg(&name)).await?)?;
    }
    Ok(Some(SentinelTopology { masters }))
}
