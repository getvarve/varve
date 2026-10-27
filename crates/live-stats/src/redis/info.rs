//! `INFO` parsed into typed sections, and the database list with key counts.
//!
//! Field names differ between servers: Valkey reports `server_mode` where
//! Redis reports `redis_mode`, Redis 7 renamed `used_memory_lua` to
//! `used_memory_vm_eval` (keeping the old name as an alias), and every
//! release adds fields. Typed fields read every known name; fields a section
//! does not know are kept in its `other` list, and sections this module
//! does not type (TLS, threads, error stats, latency stats, module sections)
//! are kept whole in [`RedisInfo::sections`].

use serde::{Deserialize, Serialize};
use varve_catalog::CatalogResult;
use varve_driver::{ServerInfo, Session, Value};

use super::reply::{Cmd, int, is_unavailable, pairs, run, run_batch, text, unexpected};

/// One `key:value` line of `INFO`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoField {
    /// Field name as reported.
    pub key: String,
    /// Value as reported.
    pub value: String,
}

/// One `# Name` section of `INFO` with its fields in reply order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoSection {
    /// Section name as reported (`Server`, `Cluster Info`, `search_version`).
    /// Fields before the first header land in a section named `""`.
    pub name: String,
    /// Fields in reply order.
    pub fields: Vec<InfoField>,
}

/// Splits `INFO` text into sections.
#[must_use]
pub fn parse_info(text: &str) -> Vec<InfoSection> {
    let mut sections: Vec<InfoSection> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('#') {
            sections.push(InfoSection {
                name: name.trim().to_string(),
                fields: Vec::new(),
            });
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if sections.is_empty() {
            sections.push(InfoSection {
                name: String::new(),
                fields: Vec::new(),
            });
        }
        if let Some(section) = sections.last_mut() {
            section.fields.push(InfoField {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
    }
    sections
}

/// Splits `a=1,b=2` into pairs.
pub(crate) fn sub_fields(value: &str) -> Vec<InfoField> {
    value
        .split(',')
        .filter_map(|part| part.split_once('='))
        .map(|(k, v)| InfoField {
            key: k.to_string(),
            value: v.to_string(),
        })
        .collect()
}

/// Takes known fields out of a section, leaving the unknown ones.
pub(crate) struct Take {
    fields: Vec<InfoField>,
}

impl Take {
    pub(crate) fn new(fields: Vec<InfoField>) -> Self {
        Self { fields }
    }

    /// The first of `keys` present, removed from the remaining fields.
    pub(crate) fn str(&mut self, keys: &[&str]) -> Option<String> {
        for key in keys {
            if let Some(at) = self
                .fields
                .iter()
                .position(|f| f.key.eq_ignore_ascii_case(key))
            {
                return Some(self.fields.remove(at).value);
            }
        }
        None
    }

    /// Integer value of the first of `keys`. A value that is not an integer
    /// stays in the remaining fields.
    pub(crate) fn int(&mut self, keys: &[&str]) -> Option<i64> {
        self.parsed(keys, |v| v.trim().parse().ok())
    }

    /// Number value of the first of `keys`.
    pub(crate) fn float(&mut self, keys: &[&str]) -> Option<f64> {
        self.parsed(keys, |v| v.trim().trim_end_matches('%').parse().ok())
    }

    /// `1`/`0` or `yes`/`no` value of the first of `keys`.
    pub(crate) fn flag(&mut self, keys: &[&str]) -> Option<bool> {
        self.parsed(keys, |v| match v.trim() {
            "1" | "yes" => Some(true),
            "0" | "no" => Some(false),
            _ => None,
        })
    }

    fn parsed<T>(&mut self, keys: &[&str], parse: impl Fn(&str) -> Option<T>) -> Option<T> {
        for key in keys {
            if let Some(at) = self
                .fields
                .iter()
                .position(|f| f.key.eq_ignore_ascii_case(key))
            {
                let parsed = parse(&self.fields[at].value)?;
                self.fields.remove(at);
                return Some(parsed);
            }
        }
        None
    }

    /// Removes and returns the numbered fields `<prefix>0`, `<prefix>1`, ...
    pub(crate) fn numbered(&mut self, prefix: &str) -> Vec<InfoField> {
        let (matching, rest) = std::mem::take(&mut self.fields).into_iter().partition(|f| {
            f.key
                .strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        });
        self.fields = rest;
        matching
    }

    /// The fields not taken.
    pub(crate) fn rest(self) -> Vec<InfoField> {
        self.fields
    }
}

/// Redis or Valkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Flavor {
    /// Redis.
    #[default]
    Redis,
    /// Valkey.
    Valkey,
}

/// How the server runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServerMode {
    /// A single server (possibly a primary or replica).
    #[default]
    Standalone,
    /// A cluster node.
    Cluster,
    /// A Sentinel.
    Sentinel,
}

impl ServerMode {
    /// Mode from the `redis_mode` / `server_mode` text.
    #[must_use]
    pub fn from_text(mode: &str) -> Self {
        match mode.trim() {
            "cluster" => Self::Cluster,
            "sentinel" => Self::Sentinel,
            _ => Self::Standalone,
        }
    }
}

/// `INFO server`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSection {
    /// `redis_version` (Valkey reports the Redis version it is compatible with).
    pub redis_version: Option<String>,
    /// `valkey_version` (Valkey 8 and newer).
    pub valkey_version: Option<String>,
    /// `server_name` (Valkey).
    pub server_name: Option<String>,
    /// `redis_mode` (Redis) or `server_mode` (Valkey).
    pub mode: Option<String>,
    /// `os`.
    pub os: Option<String>,
    /// `arch_bits`.
    pub arch_bits: Option<i64>,
    /// `process_id`.
    pub process_id: Option<i64>,
    /// `run_id`.
    pub run_id: Option<String>,
    /// `tcp_port`.
    pub tcp_port: Option<i64>,
    /// `uptime_in_seconds`.
    pub uptime_in_seconds: Option<i64>,
    /// `hz`.
    pub hz: Option<i64>,
    /// `executable`.
    pub executable: Option<String>,
    /// `config_file`; empty when the server runs without one.
    pub config_file: Option<String>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// `INFO clients`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientsSection {
    /// `connected_clients`.
    pub connected_clients: Option<i64>,
    /// `blocked_clients`.
    pub blocked_clients: Option<i64>,
    /// `tracking_clients`.
    pub tracking_clients: Option<i64>,
    /// `pubsub_clients` (7.4 and newer, Valkey 8).
    pub pubsub_clients: Option<i64>,
    /// `watching_clients` (7.4 and newer, Valkey 8).
    pub watching_clients: Option<i64>,
    /// `maxclients`.
    pub maxclients: Option<i64>,
    /// `cluster_connections`.
    pub cluster_connections: Option<i64>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// `INFO memory`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySection {
    /// `used_memory`, bytes.
    pub used_memory: Option<i64>,
    /// `used_memory_rss`, bytes.
    pub used_memory_rss: Option<i64>,
    /// `used_memory_peak`, bytes.
    pub used_memory_peak: Option<i64>,
    /// `used_memory_overhead`, bytes.
    pub used_memory_overhead: Option<i64>,
    /// `used_memory_dataset`, bytes.
    pub used_memory_dataset: Option<i64>,
    /// Lua VM memory for `EVAL`: `used_memory_vm_eval` (7.0 and newer) or
    /// `used_memory_lua` (6.2).
    pub used_memory_scripts_vm: Option<i64>,
    /// `total_system_memory`, bytes.
    pub total_system_memory: Option<i64>,
    /// `maxmemory`, bytes; 0 for no limit.
    pub maxmemory: Option<i64>,
    /// `maxmemory_policy`.
    pub maxmemory_policy: Option<String>,
    /// `mem_fragmentation_ratio`.
    pub mem_fragmentation_ratio: Option<f64>,
    /// `mem_allocator`.
    pub mem_allocator: Option<String>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// `INFO persistence`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceSection {
    /// `loading`.
    pub loading: Option<bool>,
    /// `rdb_changes_since_last_save`.
    pub rdb_changes_since_last_save: Option<i64>,
    /// `rdb_bgsave_in_progress`.
    pub rdb_bgsave_in_progress: Option<bool>,
    /// `rdb_last_save_time`, Unix seconds.
    pub rdb_last_save_time: Option<i64>,
    /// `rdb_last_bgsave_status` (`ok` or `err`).
    pub rdb_last_bgsave_status: Option<String>,
    /// `aof_enabled`.
    pub aof_enabled: Option<bool>,
    /// `aof_rewrite_in_progress`.
    pub aof_rewrite_in_progress: Option<bool>,
    /// `aof_last_bgrewrite_status`.
    pub aof_last_bgrewrite_status: Option<String>,
    /// `aof_last_write_status`.
    pub aof_last_write_status: Option<String>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// `INFO stats`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsSection {
    /// `total_connections_received`.
    pub total_connections_received: Option<i64>,
    /// `total_commands_processed`.
    pub total_commands_processed: Option<i64>,
    /// `instantaneous_ops_per_sec`.
    pub instantaneous_ops_per_sec: Option<i64>,
    /// `total_net_input_bytes`.
    pub total_net_input_bytes: Option<i64>,
    /// `total_net_output_bytes`.
    pub total_net_output_bytes: Option<i64>,
    /// `rejected_connections`.
    pub rejected_connections: Option<i64>,
    /// `expired_keys`.
    pub expired_keys: Option<i64>,
    /// `evicted_keys`.
    pub evicted_keys: Option<i64>,
    /// `keyspace_hits`.
    pub keyspace_hits: Option<i64>,
    /// `keyspace_misses`.
    pub keyspace_misses: Option<i64>,
    /// Hits divided by hits plus misses; `None` before the first lookup.
    pub hit_ratio: Option<f64>,
    /// `pubsub_channels`.
    pub pubsub_channels: Option<i64>,
    /// `pubsub_patterns`.
    pub pubsub_patterns: Option<i64>,
    /// `pubsubshard_channels` (7.0 and newer).
    pub pubsubshard_channels: Option<i64>,
    /// `latest_fork_usec`.
    pub latest_fork_usec: Option<i64>,
    /// `total_error_replies`.
    pub total_error_replies: Option<i64>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// A replica as its primary reports it (`slave0:ip=...,port=...`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaLink {
    /// `ip`.
    pub ip: Option<String>,
    /// `port`.
    pub port: Option<i64>,
    /// `state` (`online`, `wait_bgsave`, `send_bulk`).
    pub state: Option<String>,
    /// `offset`.
    pub offset: Option<i64>,
    /// `lag`, seconds.
    pub lag: Option<i64>,
    /// Fields not listed above (`io-thread` on Redis 8, `type` on Valkey).
    pub other: Vec<InfoField>,
}

/// `INFO replication`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationSection {
    /// `role`: `master` or `slave`.
    pub role: Option<String>,
    /// `connected_slaves`.
    pub connected_replicas: Option<i64>,
    /// The `slaveN` lines of a primary.
    pub replicas: Vec<ReplicaLink>,
    /// `master_host` (replicas).
    pub master_host: Option<String>,
    /// `master_port` (replicas).
    pub master_port: Option<i64>,
    /// `master_link_status`: `up` or `down` (replicas).
    pub master_link_status: Option<String>,
    /// `master_last_io_seconds_ago` (replicas).
    pub master_last_io_seconds_ago: Option<i64>,
    /// `slave_repl_offset` (replicas).
    pub replica_repl_offset: Option<i64>,
    /// `master_replid`.
    pub master_replid: Option<String>,
    /// `master_repl_offset`.
    pub master_repl_offset: Option<i64>,
    /// `repl_backlog_size`.
    pub repl_backlog_size: Option<i64>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// `INFO cpu`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuSection {
    /// `used_cpu_sys`, seconds.
    pub used_cpu_sys: Option<f64>,
    /// `used_cpu_user`, seconds.
    pub used_cpu_user: Option<f64>,
    /// `used_cpu_sys_children`, seconds.
    pub used_cpu_sys_children: Option<f64>,
    /// `used_cpu_user_children`, seconds.
    pub used_cpu_user_children: Option<f64>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// One database of `INFO keyspace` (`db0:keys=1,expires=0,avg_ttl=0`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyspaceDb {
    /// Database index.
    pub db: u32,
    /// `keys`.
    pub keys: i64,
    /// `expires`: keys with a time to live.
    pub expires: i64,
    /// `avg_ttl`, milliseconds; 0 when unknown.
    pub avg_ttl_ms: i64,
    /// Fields not listed above (`subexpiry` on Redis 7.4 and newer).
    pub other: Vec<InfoField>,
}

/// A loaded module (`module:name=search,ver=81001,...`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleInfo {
    /// `name`.
    pub name: String,
    /// `ver`.
    pub version: Option<i64>,
    /// `api`.
    pub api: Option<i64>,
    /// `usedby`.
    pub used_by: Vec<String>,
    /// `using`.
    pub using: Vec<String>,
    /// `options`.
    pub options: Vec<String>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// `INFO cluster`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterSection {
    /// `cluster_enabled`.
    pub enabled: bool,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// A monitored primary as `INFO sentinel` reports it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentinelMasterSummary {
    /// `name`.
    pub name: Option<String>,
    /// `status` (`ok`, `sdown`, `odown`).
    pub status: Option<String>,
    /// `address` (`host:port`).
    pub address: Option<String>,
    /// `slaves`.
    pub replicas: Option<i64>,
    /// `sentinels`.
    pub sentinels: Option<i64>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// `INFO sentinel` (Sentinel only).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentinelSection {
    /// `sentinel_masters`.
    pub masters_count: Option<i64>,
    /// `sentinel_tilt`.
    pub tilt: Option<bool>,
    /// `sentinel_running_scripts`.
    pub running_scripts: Option<i64>,
    /// The `masterN` lines.
    pub masters: Vec<SentinelMasterSummary>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// One command of `INFO commandstats`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandStat {
    /// Command name, `container|sub` for subcommands (`config|get`).
    pub command: String,
    /// `calls`.
    pub calls: i64,
    /// `usec`: total time, microseconds.
    pub usec: i64,
    /// `usec_per_call`.
    pub usec_per_call: Option<f64>,
    /// `rejected_calls` (6.2 and newer).
    pub rejected_calls: Option<i64>,
    /// `failed_calls` (6.2 and newer).
    pub failed_calls: Option<i64>,
}

/// `INFO everything`, parsed.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedisInfo {
    /// Redis or Valkey.
    pub flavor: Flavor,
    /// Standalone, cluster node or Sentinel.
    pub mode: ServerMode,
    /// `# Server`.
    pub server: ServerSection,
    /// `# Clients`.
    pub clients: ClientsSection,
    /// `# Memory`.
    pub memory: MemorySection,
    /// `# Persistence`.
    pub persistence: PersistenceSection,
    /// `# Stats`.
    pub stats: StatsSection,
    /// `# Replication`.
    pub replication: ReplicationSection,
    /// `# CPU`.
    pub cpu: CpuSection,
    /// `# Keyspace`, one entry per database that has keys.
    pub keyspace: Vec<KeyspaceDb>,
    /// `# Modules`.
    pub modules: Vec<ModuleInfo>,
    /// `# Cluster`.
    pub cluster: ClusterSection,
    /// `# Sentinel`, only on a Sentinel.
    pub sentinel: Option<SentinelSection>,
    /// `# Commandstats`.
    pub commandstats: Vec<CommandStat>,
    /// Every other section, unchanged (TLS, threads, error stats, latency
    /// stats, keysizes, module sections, ...).
    pub sections: Vec<InfoSection>,
}

impl RedisInfo {
    /// Parses `INFO` text (any set of sections).
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut info = Self::default();
        let mut sentinel_seen = false;
        for section in parse_info(text) {
            let fields = section.fields;
            match section.name.to_ascii_lowercase().as_str() {
                "server" => info.server = server_section(fields),
                "clients" => info.clients = clients_section(fields),
                "memory" => info.memory = memory_section(fields),
                "persistence" => info.persistence = persistence_section(fields),
                "stats" => info.stats = stats_section(fields),
                "replication" => info.replication = replication_section(fields),
                "cpu" => info.cpu = cpu_section(fields),
                "keyspace" => info.keyspace = fields.into_iter().filter_map(keyspace_db).collect(),
                "modules" => info.modules = fields.into_iter().map(module_info).collect(),
                "cluster" => info.cluster = cluster_section(fields),
                "sentinel" => {
                    sentinel_seen = true;
                    info.sentinel = Some(sentinel_section(fields));
                }
                "commandstats" => {
                    info.commandstats = fields.into_iter().filter_map(command_stat).collect();
                }
                _ => info.sections.push(InfoSection {
                    name: section.name,
                    fields,
                }),
            }
        }
        info.flavor = if info.server.valkey_version.is_some()
            || info
                .server
                .server_name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case("valkey"))
        {
            Flavor::Valkey
        } else {
            Flavor::Redis
        };
        info.mode = match info.server.mode.as_deref() {
            Some(mode) => ServerMode::from_text(mode),
            None if sentinel_seen => ServerMode::Sentinel,
            None if info.cluster.enabled => ServerMode::Cluster,
            None => ServerMode::Standalone,
        };
        info
    }
}

fn server_section(fields: Vec<InfoField>) -> ServerSection {
    let mut t = Take::new(fields);
    ServerSection {
        redis_version: t.str(&["redis_version"]),
        valkey_version: t.str(&["valkey_version"]),
        server_name: t.str(&["server_name"]),
        mode: t.str(&["redis_mode", "server_mode"]),
        os: t.str(&["os"]),
        arch_bits: t.int(&["arch_bits"]),
        process_id: t.int(&["process_id"]),
        run_id: t.str(&["run_id"]),
        tcp_port: t.int(&["tcp_port"]),
        uptime_in_seconds: t.int(&["uptime_in_seconds"]),
        hz: t.int(&["hz"]),
        executable: t.str(&["executable"]),
        config_file: t.str(&["config_file"]),
        other: t.rest(),
    }
}

fn clients_section(fields: Vec<InfoField>) -> ClientsSection {
    let mut t = Take::new(fields);
    ClientsSection {
        connected_clients: t.int(&["connected_clients"]),
        blocked_clients: t.int(&["blocked_clients"]),
        tracking_clients: t.int(&["tracking_clients"]),
        pubsub_clients: t.int(&["pubsub_clients"]),
        watching_clients: t.int(&["watching_clients"]),
        maxclients: t.int(&["maxclients"]),
        cluster_connections: t.int(&["cluster_connections"]),
        other: t.rest(),
    }
}

fn memory_section(fields: Vec<InfoField>) -> MemorySection {
    let mut t = Take::new(fields);
    MemorySection {
        used_memory: t.int(&["used_memory"]),
        used_memory_rss: t.int(&["used_memory_rss"]),
        used_memory_peak: t.int(&["used_memory_peak"]),
        used_memory_overhead: t.int(&["used_memory_overhead"]),
        used_memory_dataset: t.int(&["used_memory_dataset"]),
        used_memory_scripts_vm: t.int(&["used_memory_vm_eval", "used_memory_lua"]),
        total_system_memory: t.int(&["total_system_memory"]),
        maxmemory: t.int(&["maxmemory"]),
        maxmemory_policy: t.str(&["maxmemory_policy"]),
        mem_fragmentation_ratio: t.float(&["mem_fragmentation_ratio"]),
        mem_allocator: t.str(&["mem_allocator"]),
        other: t.rest(),
    }
}

fn persistence_section(fields: Vec<InfoField>) -> PersistenceSection {
    let mut t = Take::new(fields);
    PersistenceSection {
        loading: t.flag(&["loading"]),
        rdb_changes_since_last_save: t.int(&["rdb_changes_since_last_save"]),
        rdb_bgsave_in_progress: t.flag(&["rdb_bgsave_in_progress"]),
        rdb_last_save_time: t.int(&["rdb_last_save_time"]),
        rdb_last_bgsave_status: t.str(&["rdb_last_bgsave_status"]),
        aof_enabled: t.flag(&["aof_enabled"]),
        aof_rewrite_in_progress: t.flag(&["aof_rewrite_in_progress"]),
        aof_last_bgrewrite_status: t.str(&["aof_last_bgrewrite_status"]),
        aof_last_write_status: t.str(&["aof_last_write_status"]),
        other: t.rest(),
    }
}

fn stats_section(fields: Vec<InfoField>) -> StatsSection {
    let mut t = Take::new(fields);
    let keyspace_hits = t.int(&["keyspace_hits"]);
    let keyspace_misses = t.int(&["keyspace_misses"]);
    let hit_ratio = match (keyspace_hits, keyspace_misses) {
        (Some(hits), Some(misses)) if hits + misses > 0 => {
            Some(ratio(hits, hits.saturating_add(misses)))
        }
        _ => None,
    };
    StatsSection {
        total_connections_received: t.int(&["total_connections_received"]),
        total_commands_processed: t.int(&["total_commands_processed"]),
        instantaneous_ops_per_sec: t.int(&["instantaneous_ops_per_sec"]),
        total_net_input_bytes: t.int(&["total_net_input_bytes"]),
        total_net_output_bytes: t.int(&["total_net_output_bytes"]),
        rejected_connections: t.int(&["rejected_connections"]),
        expired_keys: t.int(&["expired_keys"]),
        evicted_keys: t.int(&["evicted_keys"]),
        keyspace_hits,
        keyspace_misses,
        hit_ratio,
        pubsub_channels: t.int(&["pubsub_channels"]),
        pubsub_patterns: t.int(&["pubsub_patterns"]),
        pubsubshard_channels: t.int(&["pubsubshard_channels"]),
        latest_fork_usec: t.int(&["latest_fork_usec"]),
        total_error_replies: t.int(&["total_error_replies"]),
        other: t.rest(),
    }
}

/// `part / whole` as a float.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn ratio(part: i64, whole: i64) -> f64 {
    part as f64 / whole as f64
}

fn replication_section(fields: Vec<InfoField>) -> ReplicationSection {
    let mut t = Take::new(fields);
    let replicas = t
        .numbered("slave")
        .into_iter()
        .map(|f| {
            let mut r = Take::new(sub_fields(&f.value));
            ReplicaLink {
                ip: r.str(&["ip"]),
                port: r.int(&["port"]),
                state: r.str(&["state"]),
                offset: r.int(&["offset"]),
                lag: r.int(&["lag"]),
                other: r.rest(),
            }
        })
        .collect();
    ReplicationSection {
        role: t.str(&["role"]),
        connected_replicas: t.int(&["connected_slaves", "connected_replicas"]),
        replicas,
        master_host: t.str(&["master_host", "primary_host"]),
        master_port: t.int(&["master_port", "primary_port"]),
        master_link_status: t.str(&["master_link_status", "primary_link_status"]),
        master_last_io_seconds_ago: t.int(&["master_last_io_seconds_ago"]),
        replica_repl_offset: t.int(&["slave_repl_offset", "replica_repl_offset"]),
        master_replid: t.str(&["master_replid"]),
        master_repl_offset: t.int(&["master_repl_offset"]),
        repl_backlog_size: t.int(&["repl_backlog_size"]),
        other: t.rest(),
    }
}

fn cpu_section(fields: Vec<InfoField>) -> CpuSection {
    let mut t = Take::new(fields);
    CpuSection {
        used_cpu_sys: t.float(&["used_cpu_sys"]),
        used_cpu_user: t.float(&["used_cpu_user"]),
        used_cpu_sys_children: t.float(&["used_cpu_sys_children"]),
        used_cpu_user_children: t.float(&["used_cpu_user_children"]),
        other: t.rest(),
    }
}

fn keyspace_db(field: InfoField) -> Option<KeyspaceDb> {
    let db = field.key.strip_prefix("db")?.parse().ok()?;
    let mut t = Take::new(sub_fields(&field.value));
    Some(KeyspaceDb {
        db,
        keys: t.int(&["keys"]).unwrap_or(0),
        expires: t.int(&["expires"]).unwrap_or(0),
        avg_ttl_ms: t.int(&["avg_ttl"]).unwrap_or(0),
        other: t.rest(),
    })
}

fn list_value(value: Option<String>) -> Vec<String> {
    value
        .map(|v| {
            v.trim_start_matches('[')
                .trim_end_matches(']')
                .split('|')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn module_info(field: InfoField) -> ModuleInfo {
    let mut t = Take::new(sub_fields(&field.value));
    ModuleInfo {
        name: t.str(&["name"]).unwrap_or(field.key),
        version: t.int(&["ver"]),
        api: t.int(&["api"]),
        used_by: list_value(t.str(&["usedby"])),
        using: list_value(t.str(&["using"])),
        options: list_value(t.str(&["options"])),
        other: t.rest(),
    }
}

fn cluster_section(fields: Vec<InfoField>) -> ClusterSection {
    let mut t = Take::new(fields);
    ClusterSection {
        enabled: t.flag(&["cluster_enabled"]).unwrap_or(false),
        other: t.rest(),
    }
}

fn sentinel_section(fields: Vec<InfoField>) -> SentinelSection {
    let mut t = Take::new(fields);
    let masters = t
        .numbered("master")
        .into_iter()
        .map(|f| {
            let mut m = Take::new(sub_fields(&f.value));
            SentinelMasterSummary {
                name: m.str(&["name"]),
                status: m.str(&["status"]),
                address: m.str(&["address"]),
                replicas: m.int(&["slaves", "replicas"]),
                sentinels: m.int(&["sentinels"]),
                other: m.rest(),
            }
        })
        .collect();
    SentinelSection {
        masters_count: t.int(&["sentinel_masters"]),
        tilt: t.flag(&["sentinel_tilt"]),
        running_scripts: t.int(&["sentinel_running_scripts"]),
        masters,
        other: t.rest(),
    }
}

fn command_stat(field: InfoField) -> Option<CommandStat> {
    let command = field.key.strip_prefix("cmdstat_")?.to_string();
    let mut t = Take::new(sub_fields(&field.value));
    Some(CommandStat {
        command,
        calls: t.int(&["calls"]).unwrap_or(0),
        usec: t.int(&["usec"]).unwrap_or(0),
        usec_per_call: t.float(&["usec_per_call"]),
        rejected_calls: t.int(&["rejected_calls"]),
        failed_calls: t.int(&["failed_calls"]),
    })
}

/// Reads and parses `INFO everything` (all sections, including command
/// statistics and module sections).
///
/// # Errors
///
/// Driver errors; [`varve_catalog::CatalogError::Unexpected`] when `INFO`
/// does not return text.
pub async fn server_info(session: &dyn Session, server: &ServerInfo) -> CatalogResult<RedisInfo> {
    let _ = server;
    let reply = run(session, &Cmd::new("INFO").arg("everything")).await?;
    let text = text(&reply).ok_or_else(|| unexpected("INFO did not return text"))?;
    Ok(RedisInfo::parse(&text))
}

/// One logical database with its key counts.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseKeys {
    /// Database index.
    pub index: u32,
    /// Keys in the database.
    pub keys: i64,
    /// Keys with a time to live.
    pub expires: i64,
    /// Average time to live in milliseconds; 0 when unknown.
    pub avg_ttl_ms: i64,
}

/// The logical databases of a server.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Databases {
    /// Standalone, cluster node or Sentinel.
    pub mode: ServerMode,
    /// Number of databases (`CONFIG GET databases`; 1 in cluster mode).
    /// `None` when the server does not tell (no `CONFIG` permission, or a
    /// Sentinel, which has no keyspace).
    pub count: Option<u32>,
    /// Every database `0..count` (or, when the count is unknown, every
    /// database that has keys), with its key counts.
    pub databases: Vec<DatabaseKeys>,
}

/// Lists the logical databases with their key counts (`INFO keyspace` and
/// `CONFIG GET databases`). In cluster mode Redis has only database 0;
/// Valkey 9 can have more (`cluster-databases`).
///
/// # Errors
///
/// Driver errors other than a missing `CONFIG` permission.
pub async fn databases(session: &dyn Session, server: &ServerInfo) -> CatalogResult<Databases> {
    let _ = server;
    let replies = run_batch(
        session,
        &[
            Cmd::new("INFO").arg("server"),
            Cmd::new("INFO").arg("cluster"),
            Cmd::new("INFO").arg("keyspace"),
        ],
    )
    .await?;
    let joined = replies
        .iter()
        .filter_map(text)
        .collect::<Vec<_>>()
        .join("\r\n");
    let info = RedisInfo::parse(&joined);
    let config = |name: &'static str| Cmd::new("CONFIG").arg("GET").arg(name);
    let count = match info.mode {
        ServerMode::Sentinel => None,
        // Redis and Valkey before 9 answer with an empty list: one database.
        ServerMode::Cluster => match run(session, &config("cluster-databases")).await {
            Ok(reply) => Some(config_count(&reply, "cluster-databases").unwrap_or(1)),
            Err(error) if is_unavailable(&error) => Some(1),
            Err(error) => return Err(error),
        },
        ServerMode::Standalone => match run(session, &config("databases")).await {
            Ok(reply) => config_count(&reply, "databases"),
            Err(error) if is_unavailable(&error) => None,
            Err(error) => return Err(error),
        },
    };
    Ok(Databases {
        mode: info.mode,
        count,
        databases: database_list(count, &info.keyspace),
    })
}

/// The number `CONFIG GET name` returned, if the server knows the setting.
pub(crate) fn config_count(reply: &Value, name: &str) -> Option<u32> {
    pairs(reply)
        .and_then(|p| p.into_iter().find(|(k, _)| k.eq_ignore_ascii_case(name)))
        .and_then(|(_, v)| int(v))
        .and_then(|n| u32::try_from(n).ok())
}

/// Merges the database count with the keyspace counts.
pub(crate) fn database_list(count: Option<u32>, keyspace: &[KeyspaceDb]) -> Vec<DatabaseKeys> {
    let entry = |index: u32| {
        keyspace.iter().find(|k| k.db == index).map_or(
            DatabaseKeys {
                index,
                ..DatabaseKeys::default()
            },
            |k| DatabaseKeys {
                index,
                keys: k.keys,
                expires: k.expires,
                avg_ttl_ms: k.avg_ttl_ms,
            },
        )
    };
    match count {
        Some(n) => (0..n).map(entry).collect(),
        None => keyspace.iter().map(|k| entry(k.db)).collect(),
    }
}
