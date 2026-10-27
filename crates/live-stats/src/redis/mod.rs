//! Redis live server panels (design doc 4.2): server info, databases with
//! key counts, memory analysis, slow log, latency monitor, client list with
//! kill, configuration, cluster and Sentinel topology, functions and
//! scripts, and a Pub/Sub snapshot.
//!
//! Every function takes the session and its [`varve_driver::ServerInfo`]
//! and talks to the server only through [`varve_driver::Session::execute`]
//! with console lines built by [`varve_redis_syntax::join_args`], so keys
//! and values are never formatted into command text by hand. Replies are
//! read from the returned values, in RESP2 and RESP3 shapes alike.
//!
//! Changes are planned before they run: the `*_plan` functions return a
//! [`CommandPlan`] for a preview, and [`apply_plan`] runs it. Direct
//! actions (`SLOWLOG RESET`, `LATENCY RESET`, `CLIENT KILL`) run at once.
//!
//! Version differences handled: Redis 6.2 has no functions, no
//! `CLUSTER SHARDS`, no shard channels and no multi-parameter
//! `CONFIG SET`; Valkey reports `server_mode` instead of `redis_mode` and
//! extra `LATENCY LATEST` fields; Redis 8 adds slow log fields. Valkey 8
//! and 9 have every Redis 7 feature used here, and their
//! [`varve_driver::ServerInfo`] reports the Valkey version (8 or 9), so
//! `at_least(7, 0)` gates both.
//!
//! Not here: live Pub/Sub message streaming and `MONITOR`, which need a
//! dedicated subscriber connection mode in the driver.

mod clients;
mod cluster;
mod config;
mod config_refusals;
mod info;
mod memory;
mod pubsub;
mod redact;
mod reply;
mod scripting;
mod sentinel;
mod slowlog;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use varve_catalog::{CatalogError, CatalogResult};
use varve_driver::{ServerInfo, Session, Value};

pub use clients::{ClientInfo, client_list, kill_client, parse_client_list};
pub use cluster::{
    ClusterNode, ClusterState, ClusterTopology, LinkState, NodeRole, SLOT_COUNT, SlotMigration,
    SlotOwner, SlotRange, TopologySource, cluster_topology, parse_cluster_info,
    parse_cluster_nodes, parse_cluster_shards, parse_cluster_slots,
};
pub use config::{
    ConfigChange, ConfigEntry, MASK, Refusal, canonical_config, config_entries, config_get,
    config_refusal, config_rewrite_plan, config_set_plan, config_warning, is_immutable_config,
    is_protected_config, is_sensitive_config, plan_config_rewrite, plan_config_set,
};
pub use info::{
    ClientsSection, ClusterSection, CommandStat, CpuSection, DatabaseKeys, Databases, Flavor,
    InfoField, InfoSection, KeyspaceDb, MemorySection, ModuleInfo, PersistenceSection, RedisInfo,
    ReplicaLink, ReplicationSection, SentinelMasterSummary, SentinelSection, ServerMode,
    ServerSection, StatsSection, databases, parse_info, server_info,
};
pub use memory::{
    BigKey, DbOverhead, MAX_MEASURE_BATCH, MAX_PREFIXES, MAX_SCAN_COUNT, MAX_TOP_KEYS,
    MemoryAnalysis, MemoryScanOptions, MemoryStats, OtherPrefixes, PrefixUsage, ScanStop,
    StatField, TypeUsage, analyze_memory, analyze_memory_until, key_memory, memory_doctor,
    memory_stats, parse_memory_stats,
};
pub use pubsub::{ChannelSubscribers, PubSubSnapshot, parse_numsub, pubsub_snapshot};
pub use redact::REDACTED;
pub use scripting::{
    FlushMode, FunctionInfo, FunctionLibrary, ScriptPresence, function_delete_plan, function_list,
    function_load_plan, library_name, parse_function_list, script_exists, script_flush_plan,
};
pub use sentinel::{
    SentinelMaster, SentinelPeer, SentinelTopology, parse_masters, parse_peer, parse_peers,
    sentinel_topology,
};
pub use slowlog::{
    LatencyEvent, LatencyReport, LatencySample, SlowLog, SlowLogEntry, latency, latency_doctor,
    latency_history, parse_latency_history, parse_latency_latest, parse_slowlog, reset_latency,
    reset_slowlog, slowlog,
};

/// Redis commands a panel produced for a change, shown before they run.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandPlan {
    /// Console lines to run in order, one command each. May contain
    /// secrets (a new `requirepass`); never log them.
    pub commands: Vec<String>,
    /// The same lines for display, with secret values replaced by
    /// [`MASK`].
    pub display: Vec<String>,
    /// True when the plan can lose data or lock clients out: the UI asks
    /// for confirmation regardless of the environment.
    pub destructive: bool,
    /// True when `commands` carries a secret value.
    pub sensitive: bool,
    /// Warnings for the user.
    pub warnings: Vec<String>,
}

impl std::fmt::Debug for CommandPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandPlan")
            .field("commands", &self.display)
            .field("destructive", &self.destructive)
            .field("sensitive", &self.sensitive)
            .field("warnings", &self.warnings)
            .finish()
    }
}

/// Runs a plan's commands in order and returns each reply. Stops at the
/// first failure; the error's detail says which command failed.
///
/// # Errors
///
/// The failing command's driver error.
pub async fn apply_plan(
    session: &dyn Session,
    server: &ServerInfo,
    plan: &CommandPlan,
) -> CatalogResult<Vec<Value>> {
    let _ = server;
    let total = plan.commands.len();
    let mut replies = Vec::with_capacity(total);
    for (index, line) in plan.commands.iter().enumerate() {
        let output = session
            .execute(line, &varve_driver::ExecOptions::default())
            .await
            .map_err(|mut error| {
                error.detail = Some(format!("Command {} of {total}", index + 1));
                CatalogError::from(error)
            })?;
        for result in output.results {
            replies.push(reply::reply_value(result)?);
        }
    }
    Ok(replies)
}
