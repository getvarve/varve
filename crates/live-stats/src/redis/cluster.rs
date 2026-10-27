//! Cluster topology from `CLUSTER NODES`, `CLUSTER SHARDS` (7.0 and newer)
//! and `CLUSTER SLOTS`: nodes with roles, link state and slot ranges, and
//! the slot map.
//!
//! [`cluster_topology`] reads `CLUSTER NODES` (all versions) and, on 7.0
//! and newer, adds health and replication offsets from `CLUSTER SHARDS`.
//! Managed services sometimes rename or block `CLUSTER NODES`; then the
//! topology comes from `CLUSTER SHARDS`, or from `CLUSTER SLOTS` on 6.2
//! (which lacks link state and flags).
//!
//! The development matrix runs no cluster; the parsers are unit tested
//! with replies captured from six-node clusters (three primaries, three
//! replicas) on Redis 6.2, Redis 8.10 and Valkey 9.

use serde::{Deserialize, Serialize};
use varve_catalog::CatalogResult;
use varve_driver::{ServerInfo, Session, Value};

use super::info::{InfoField, RedisInfo, Take};
use super::reply::{Cmd, Fields, array, int, is_unavailable, run, text, unexpected};

/// Number of hash slots in a Redis cluster.
pub const SLOT_COUNT: u16 = 16_384;

/// An inclusive range of hash slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotRange {
    /// First slot.
    pub start: u16,
    /// Last slot (inclusive).
    pub end: u16,
}

impl SlotRange {
    /// A range when `start <= end < SLOT_COUNT`; `None` for a reversed or
    /// out-of-range pair, which no server sends and which is dropped.
    #[must_use]
    pub fn new(start: u16, end: u16) -> Option<Self> {
        (start <= end && end < SLOT_COUNT).then_some(Self { start, end })
    }

    /// Slots in the range.
    #[must_use]
    pub fn len(&self) -> u32 {
        u32::from(self.end) - u32::from(self.start) + 1
    }

    /// Always false: a range holds at least one slot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }
}

/// A slot being moved to or from another node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotMigration {
    /// The slot.
    pub slot: u16,
    /// The other node's id.
    pub node_id: String,
}

/// Role of a cluster node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeRole {
    /// Primary (`master`).
    Primary,
    /// Replica (`slave`).
    Replica,
    /// Not known yet (handshake, no address).
    #[default]
    Unknown,
}

/// State of the cluster bus link to a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkState {
    /// `connected`.
    Connected,
    /// `disconnected`.
    Disconnected,
}

/// One cluster node.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterNode {
    /// Node id.
    pub id: String,
    /// IP address (empty for `noaddr`).
    pub ip: String,
    /// Client port.
    pub port: u16,
    /// Cluster bus port, when reported.
    pub cluster_port: Option<u16>,
    /// Announced host name (7.0 and newer).
    pub hostname: Option<String>,
    /// Shard id (`shard-id` aux field, Redis 7.2 and newer and Valkey).
    pub shard_id: Option<String>,
    /// Aux fields of the address (`key=value` after the host name).
    pub aux: Vec<InfoField>,
    /// Preferred endpoint (`CLUSTER SHARDS`).
    pub endpoint: Option<String>,
    /// Primary or replica.
    pub role: NodeRole,
    /// The node this session is connected to.
    pub myself: bool,
    /// Flagged `fail` or `fail?`.
    pub failing: bool,
    /// Flags as reported (`myself`, `master`, `slave`, `fail?`, `fail`,
    /// `handshake`, `noaddr`, `nofailover`, `noflags`).
    pub flags: Vec<String>,
    /// For a replica: its primary's id.
    pub primary_id: Option<String>,
    /// Unix time in ms the last ping was sent (0 when none is pending).
    pub ping_sent: Option<i64>,
    /// Unix time in ms the last pong was received.
    pub pong_received: Option<i64>,
    /// Configuration epoch.
    pub config_epoch: Option<i64>,
    /// Cluster bus link state (`None` from `CLUSTER SHARDS` or `SLOTS`).
    pub link_state: Option<LinkState>,
    /// Slots served (primaries).
    pub slots: Vec<SlotRange>,
    /// Slots being moved away from this node.
    pub migrating: Vec<SlotMigration>,
    /// Slots being moved to this node.
    pub importing: Vec<SlotMigration>,
    /// `health` from `CLUSTER SHARDS`: `online`, `failed` or `loading`.
    pub health: Option<String>,
    /// `replication-offset` from `CLUSTER SHARDS`.
    pub replication_offset: Option<i64>,
}

/// Where the topology came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TopologySource {
    /// `CLUSTER NODES` (with `CLUSTER SHARDS` details on 7.0 and newer).
    Nodes,
    /// `CLUSTER SHARDS`.
    Shards,
    /// `CLUSTER SLOTS`.
    Slots,
}

/// A slot range and the primary that serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotOwner {
    /// The slots.
    pub range: SlotRange,
    /// The primary's node id.
    pub node_id: String,
}

/// `CLUSTER INFO`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterState {
    /// `cluster_state`: `ok` or `fail`.
    pub state: Option<String>,
    /// `cluster_slots_assigned`.
    pub slots_assigned: Option<i64>,
    /// `cluster_slots_ok`.
    pub slots_ok: Option<i64>,
    /// `cluster_slots_pfail`.
    pub slots_pfail: Option<i64>,
    /// `cluster_slots_fail`.
    pub slots_fail: Option<i64>,
    /// `cluster_known_nodes`.
    pub known_nodes: Option<i64>,
    /// `cluster_size`: primaries serving slots.
    pub size: Option<i64>,
    /// `cluster_current_epoch`.
    pub current_epoch: Option<i64>,
    /// `cluster_my_epoch`.
    pub my_epoch: Option<i64>,
    /// Fields not listed above.
    pub other: Vec<InfoField>,
}

/// The cluster as seen from one node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterTopology {
    /// Where the node list came from.
    pub source: TopologySource,
    /// `CLUSTER INFO`.
    pub state: ClusterState,
    /// Id of the node this session is connected to.
    pub my_id: Option<String>,
    /// Primaries first (by first slot), each followed by its replicas.
    pub nodes: Vec<ClusterNode>,
    /// Assigned slot ranges in slot order.
    pub slot_map: Vec<SlotOwner>,
    /// Slot ranges no primary serves.
    pub unassigned: Vec<SlotRange>,
}

/// Parses `CLUSTER INFO` text.
#[must_use]
pub fn parse_cluster_info(text: &str) -> ClusterState {
    let fields = text
        .lines()
        .map(str::trim)
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| InfoField {
            key: k.into(),
            value: v.into(),
        })
        .collect();
    let mut t = Take::new(fields);
    ClusterState {
        state: t.str(&["cluster_state"]),
        slots_assigned: t.int(&["cluster_slots_assigned"]),
        slots_ok: t.int(&["cluster_slots_ok"]),
        slots_pfail: t.int(&["cluster_slots_pfail"]),
        slots_fail: t.int(&["cluster_slots_fail"]),
        known_nodes: t.int(&["cluster_known_nodes"]),
        size: t.int(&["cluster_size"]),
        current_epoch: t.int(&["cluster_current_epoch"]),
        my_epoch: t.int(&["cluster_my_epoch"]),
        other: t.rest(),
    }
}

fn parse_slot(text: &str) -> Option<u16> {
    text.parse().ok().filter(|s| *s < SLOT_COUNT)
}

/// A parsed `CLUSTER NODES` address field.
struct Address {
    ip: String,
    port: u16,
    cluster_port: Option<u16>,
    hostname: Option<String>,
    aux: Vec<InfoField>,
}

/// Splits `ip:port@cport[,hostname[,key=value...]]` (aux fields from Redis
/// 7.2: `shard-id=...`, `hostname=...`, `tls-port=...`); the IP may be
/// IPv6.
fn parse_address(field: &str) -> Address {
    let mut parts = field.split(',');
    let address = parts.next().unwrap_or_default();
    let mut hostname = None;
    let mut aux = Vec::new();
    for (index, part) in parts.enumerate() {
        match part.split_once('=') {
            Some((key, value)) => {
                if key == "hostname" && !value.is_empty() {
                    hostname = Some(value.to_string());
                }
                aux.push(InfoField {
                    key: key.into(),
                    value: value.into(),
                });
            }
            None if index == 0 && !part.is_empty() => hostname = Some(part.to_string()),
            None => {}
        }
    }
    let (endpoint, cluster_port) = match address.split_once('@') {
        Some((e, c)) => (e, c.parse().ok()),
        None => (address, None),
    };
    let (ip, port) = match endpoint.rsplit_once(':') {
        Some((ip, port)) => (ip.to_string(), port.parse().unwrap_or(0)),
        None => (endpoint.to_string(), 0),
    };
    Address {
        ip,
        port,
        cluster_port,
        hostname,
        aux,
    }
}

/// Parses `CLUSTER NODES` text.
#[must_use]
pub fn parse_cluster_nodes(text: &str) -> Vec<ClusterNode> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 8 {
                return None;
            }
            let address = parse_address(parts[1]);
            let flags: Vec<String> = parts[2].split(',').map(str::to_string).collect();
            let has = |f: &str| flags.iter().any(|x| x == f);
            let role = if has("master") {
                NodeRole::Primary
            } else if has("slave") {
                NodeRole::Replica
            } else {
                NodeRole::Unknown
            };
            let mut node = ClusterNode {
                id: parts[0].to_string(),
                shard_id: address
                    .aux
                    .iter()
                    .find(|f| f.key == "shard-id")
                    .map(|f| f.value.clone()),
                ip: address.ip,
                port: address.port,
                cluster_port: address.cluster_port,
                hostname: address.hostname,
                aux: address.aux,
                role,
                myself: has("myself"),
                failing: has("fail") || has("fail?"),
                primary_id: (parts[3] != "-").then(|| parts[3].to_string()),
                ping_sent: parts[4].parse().ok(),
                pong_received: parts[5].parse().ok(),
                config_epoch: parts[6].parse().ok(),
                link_state: match parts[7] {
                    "connected" => Some(LinkState::Connected),
                    "disconnected" => Some(LinkState::Disconnected),
                    _ => None,
                },
                flags,
                ..ClusterNode::default()
            };
            for slot in &parts[8..] {
                if let Some(inner) = slot.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                    if let Some((s, id)) = inner.split_once("->-") {
                        if let Some(slot) = parse_slot(s) {
                            node.migrating.push(SlotMigration {
                                slot,
                                node_id: id.into(),
                            });
                        }
                    } else if let Some((s, id)) = inner.split_once("-<-")
                        && let Some(slot) = parse_slot(s)
                    {
                        node.importing.push(SlotMigration {
                            slot,
                            node_id: id.into(),
                        });
                    }
                } else if let Some((a, b)) = slot.split_once('-') {
                    if let (Some(start), Some(end)) = (parse_slot(a), parse_slot(b))
                        && let Some(range) = SlotRange::new(start, end)
                    {
                        node.slots.push(range);
                    }
                } else if let Some(s) = parse_slot(slot) {
                    node.slots.push(SlotRange { start: s, end: s });
                }
            }
            Some(node)
        })
        .collect()
}

fn slot_pairs(value: Option<&Value>) -> Vec<SlotRange> {
    let items = value.and_then(array).unwrap_or_default();
    items
        .chunks(2)
        .filter_map(|c| match c {
            [a, b] => SlotRange::new(u16::try_from(int(a)?).ok()?, u16::try_from(int(b)?).ok()?),
            _ => None,
        })
        .collect()
}

fn port(value: Option<i64>) -> u16 {
    value.and_then(|p| u16::try_from(p).ok()).unwrap_or(0)
}

/// Parses a `CLUSTER SHARDS` reply (7.0 and newer) into nodes; primaries
/// carry the shard's slots and replicas point at their shard's primary.
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Unexpected`] for a malformed reply.
pub fn parse_cluster_shards(reply: &Value) -> CatalogResult<Vec<ClusterNode>> {
    let shards = array(reply).ok_or_else(|| unexpected("CLUSTER SHARDS is not an array"))?;
    let mut nodes = Vec::new();
    for shard in shards {
        let shard = Fields::of(shard).ok_or_else(|| unexpected("shard is not a map"))?;
        let slots = slot_pairs(shard.get("slots"));
        let shard_id = shard.text("id");
        let members = shard.get("nodes").and_then(array).unwrap_or_default();
        let mut shard_nodes: Vec<ClusterNode> = members
            .iter()
            .filter_map(Fields::of)
            .map(|n| {
                let role = match n.text("role").as_deref() {
                    Some("master") => NodeRole::Primary,
                    Some("replica" | "slave") => NodeRole::Replica,
                    _ => NodeRole::Unknown,
                };
                ClusterNode {
                    id: n.text("id").unwrap_or_default(),
                    shard_id: shard_id.clone(),
                    ip: n.text("ip").unwrap_or_default(),
                    port: port(n.int("port").or_else(|| n.int("tls-port"))),
                    hostname: n.text("hostname").filter(|h| !h.is_empty()),
                    endpoint: n.text("endpoint"),
                    role,
                    health: n.text("health"),
                    failing: n.text("health").as_deref() == Some("failed"),
                    replication_offset: n.int("replication-offset"),
                    slots: if role == NodeRole::Primary {
                        slots.clone()
                    } else {
                        Vec::new()
                    },
                    ..ClusterNode::default()
                }
            })
            .collect();
        let primary = shard_nodes
            .iter()
            .find(|n| n.role == NodeRole::Primary)
            .map(|n| n.id.clone());
        for node in &mut shard_nodes {
            if node.role == NodeRole::Replica {
                node.primary_id.clone_from(&primary);
            }
        }
        nodes.extend(shard_nodes);
    }
    Ok(nodes)
}

/// Parses a `CLUSTER SLOTS` reply into nodes (address and id only; Redis
/// 7 adds a metadata map with the host name).
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Unexpected`] for a malformed reply.
pub fn parse_cluster_slots(reply: &Value) -> CatalogResult<Vec<ClusterNode>> {
    let ranges = array(reply).ok_or_else(|| unexpected("CLUSTER SLOTS is not an array"))?;
    let mut nodes: Vec<ClusterNode> = Vec::new();
    for range in ranges {
        let fields = array(range).ok_or_else(|| unexpected("slot range is not an array"))?;
        let slot = |at: usize| {
            fields
                .get(at)
                .and_then(int)
                .and_then(|n| u16::try_from(n).ok())
                .ok_or_else(|| unexpected("slot range without bounds"))
        };
        let range = SlotRange::new(slot(0)?, slot(1)?).ok_or_else(|| {
            unexpected("CLUSTER SLOTS sent a reversed or out-of-range slot range")
        })?;
        let mut primary: Option<String> = None;
        for (index, member) in fields.iter().skip(2).enumerate() {
            let Some(member) = array(member) else {
                continue;
            };
            let ip = member.first().and_then(text).unwrap_or_default();
            let port = port(member.get(1).and_then(int));
            let id = member
                .get(2)
                .and_then(text)
                .unwrap_or_else(|| format!("{ip}:{port}"));
            let hostname = member
                .get(3)
                .and_then(Fields::of)
                .and_then(|m| m.text("hostname"));
            let role = if index == 0 {
                NodeRole::Primary
            } else {
                NodeRole::Replica
            };
            let node = match nodes.iter_mut().position(|n| n.id == id) {
                Some(at) => &mut nodes[at],
                None => {
                    nodes.push(ClusterNode {
                        id: id.clone(),
                        ip,
                        port,
                        hostname,
                        role,
                        ..ClusterNode::default()
                    });
                    let last = nodes.len() - 1;
                    &mut nodes[last]
                }
            };
            if role == NodeRole::Primary {
                node.slots.push(range);
                primary = Some(id);
            } else {
                node.primary_id.clone_from(&primary);
            }
        }
    }
    Ok(nodes)
}

impl ClusterTopology {
    /// Builds the topology from a node list: orders the nodes, builds the
    /// slot map and finds unassigned slots.
    #[must_use]
    pub fn new(
        source: TopologySource,
        state: ClusterState,
        my_id: Option<String>,
        nodes: Vec<ClusterNode>,
    ) -> Self {
        let mut nodes = nodes;
        if let Some(id) = &my_id {
            for node in &mut nodes {
                node.myself |= &node.id == id;
            }
        }
        let first_slot =
            |n: &ClusterNode| n.slots.iter().map(|r| r.start).min().unwrap_or(u16::MAX);
        let mut primaries: Vec<ClusterNode> = nodes
            .iter()
            .filter(|n| n.role == NodeRole::Primary)
            .cloned()
            .collect();
        primaries.sort_by(|a, b| first_slot(a).cmp(&first_slot(b)).then(a.id.cmp(&b.id)));
        let mut ordered = Vec::with_capacity(nodes.len());
        for primary in primaries {
            let id = primary.id.clone();
            ordered.push(primary);
            let mut replicas: Vec<ClusterNode> = nodes
                .iter()
                .filter(|n| n.role != NodeRole::Primary && n.primary_id.as_deref() == Some(&id))
                .cloned()
                .collect();
            replicas.sort_by(|a, b| a.id.cmp(&b.id));
            ordered.extend(replicas);
        }
        for node in &nodes {
            if !ordered.iter().any(|o| o.id == node.id) {
                ordered.push(node.clone());
            }
        }
        let mut slot_map: Vec<SlotOwner> = ordered
            .iter()
            .filter(|n| n.role == NodeRole::Primary)
            .flat_map(|n| {
                n.slots.iter().map(|r| SlotOwner {
                    range: *r,
                    node_id: n.id.clone(),
                })
            })
            .collect();
        slot_map.sort_by_key(|o| o.range);
        let mut unassigned = Vec::new();
        let mut next: u32 = 0;
        for owner in &slot_map {
            if u32::from(owner.range.start) > next {
                unassigned.push(SlotRange {
                    start: u16::try_from(next).unwrap_or(u16::MAX),
                    end: owner.range.start - 1,
                });
            }
            next = next.max(u32::from(owner.range.end) + 1);
        }
        if next < u32::from(SLOT_COUNT) {
            unassigned.push(SlotRange {
                start: u16::try_from(next).unwrap_or(u16::MAX),
                end: SLOT_COUNT - 1,
            });
        }
        Self {
            source,
            state,
            my_id,
            nodes: ordered,
            slot_map,
            unassigned,
        }
    }

    /// Adds `CLUSTER SHARDS` details (health, replication offset, host
    /// name, endpoint) to nodes read from `CLUSTER NODES`.
    pub fn merge_shards(&mut self, shards: &[ClusterNode]) {
        for node in &mut self.nodes {
            if let Some(shard) = shards.iter().find(|s| s.id == node.id) {
                node.health.clone_from(&shard.health);
                node.replication_offset = shard.replication_offset;
                node.endpoint.clone_from(&shard.endpoint);
                if node.hostname.is_none() {
                    node.hostname.clone_from(&shard.hostname);
                }
                if node.shard_id.is_none() {
                    node.shard_id.clone_from(&shard.shard_id);
                }
            }
        }
    }
}

/// Reads the cluster topology; `None` when the server does not run in
/// cluster mode.
///
/// # Errors
///
/// Driver errors other than an unavailable `CLUSTER` subcommand.
pub async fn cluster_topology(
    session: &dyn Session,
    server: &ServerInfo,
) -> CatalogResult<Option<ClusterTopology>> {
    let info = run(session, &Cmd::new("INFO").arg("cluster")).await?;
    let info = RedisInfo::parse(&text(&info).unwrap_or_default());
    if !info.cluster.enabled {
        return Ok(None);
    }
    let state = parse_cluster_info(
        &text(&run(session, &Cmd::new("CLUSTER").arg("INFO")).await?).unwrap_or_default(),
    );
    let my_id = optional(run(session, &Cmd::new("CLUSTER").arg("MYID")).await)?
        .as_ref()
        .and_then(text);
    let shards = if server.at_least(7, 0) {
        optional(run(session, &Cmd::new("CLUSTER").arg("SHARDS")).await)?
            .map(|reply| parse_cluster_shards(&reply))
            .transpose()?
    } else {
        None
    };
    let nodes = optional(run(session, &Cmd::new("CLUSTER").arg("NODES")).await)?;
    let topology = match (nodes, shards) {
        (Some(nodes), shards) => {
            let nodes = parse_cluster_nodes(&text(&nodes).unwrap_or_default());
            let mut topology = ClusterTopology::new(TopologySource::Nodes, state, my_id, nodes);
            if let Some(shards) = shards {
                topology.merge_shards(&shards);
            }
            topology
        }
        (None, Some(shards)) => ClusterTopology::new(TopologySource::Shards, state, my_id, shards),
        (None, None) => {
            let slots = run(session, &Cmd::new("CLUSTER").arg("SLOTS")).await?;
            ClusterTopology::new(
                TopologySource::Slots,
                state,
                my_id,
                parse_cluster_slots(&slots)?,
            )
        }
    };
    Ok(Some(topology))
}

/// `Ok(None)` for an unavailable command.
fn optional(result: CatalogResult<Value>) -> CatalogResult<Option<Value>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if is_unavailable(&error) => Ok(None),
        Err(error) => Err(error),
    }
}
