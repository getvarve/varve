//! The live server status summary: current values only, no history (the
//! snapshot history built on the same sources is a closed feature).
//!
//! - PostgreSQL: `version()`, `pg_postmaster_start_time()`,
//!   `pg_stat_activity` counts, `max_connections`, the buffer cache hit
//!   ratio from the cumulative `pg_stat_database` counters, database sizes,
//!   and replication from `pg_stat_replication` (downstream) and
//!   `pg_stat_wal_receiver` (upstream, on a standby).
//! - MySQL and MariaDB: `SHOW GLOBAL STATUS` and `SHOW GLOBAL VARIABLES`,
//!   the InnoDB buffer pool hit ratio from its cumulative counters, and
//!   replication from `SHOW REPLICA STATUS` (MySQL 8.0.22 and newer) or
//!   `SHOW SLAVE STATUS`, `SHOW ALL SLAVES STATUS` on MariaDB (every
//!   multi-source connection), plus the replicas a source knows.
//! - MariaDB Galera: `SHOW GLOBAL STATUS LIKE 'wsrep\_%'`.
//!
//! Replication details need privileges (`REPLICATION CLIENT`, `SLAVE
//! MONITOR`, ...); without them [`Replication::visible`] is false and the
//! rest of the summary is still returned.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use varve_activity::{SqlDialect, redact_secrets};
use varve_catalog::rows::{self, Row, Rows};
use varve_core::Engine;
use varve_driver::{ServerInfo, Session};

use super::{Family, LiveError, LiveResult, at_least_patch, float, non_empty};

/// The live status summary of a server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    /// Engine.
    pub engine: Engine,
    /// Version as read while connecting (`17.2`, `11.4.4-MariaDB`).
    pub version: String,
    /// Longer version text: PostgreSQL `version()`, MySQL and MariaDB
    /// `version_comment`.
    pub version_detail: Option<String>,
    /// When the server started (PostgreSQL).
    pub started_at: Option<String>,
    /// Seconds since the server started.
    pub uptime_seconds: Option<i64>,
    /// Open client connections (PostgreSQL client backends, MySQL and
    /// MariaDB `Threads_connected`).
    pub connections: Option<i64>,
    /// The connection limit (`max_connections`).
    pub max_connections: Option<i64>,
    /// Most connections open at the same time since the start (MySQL and
    /// MariaDB `Max_used_connections`).
    pub max_used_connections: Option<i64>,
    /// Statements running now, not counting the one reading this status.
    pub active_queries: Option<i64>,
    /// Share of page reads served from memory since the statistics were last
    /// reset, from 0 to 1: PostgreSQL `blks_hit / (blks_hit + blks_read)`,
    /// InnoDB `1 - Innodb_buffer_pool_reads / Innodb_buffer_pool_read_requests`.
    /// `None` before any read.
    pub cache_hit_ratio: Option<f64>,
    /// Database sizes (PostgreSQL).
    pub databases: Vec<DatabaseSize>,
    /// Replication role and lag.
    pub replication: Replication,
    /// Galera cluster status (MariaDB only).
    pub galera: Option<GaleraStatus>,
}

/// The size of one database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseSize {
    /// Database name.
    pub name: String,
    /// Size on disk in bytes; `None` when the user may not connect to it.
    pub size_bytes: Option<i64>,
    /// Backends connected to it.
    pub connections: Option<i64>,
}

/// The server's place in replication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReplicationRole {
    /// It has replicas and replicates from nothing.
    Primary,
    /// It replicates from another server (it may have replicas of its own).
    Replica,
    /// A MariaDB Galera cluster member ("Galera cluster member" in the UI):
    /// every node is writable and replicates synchronously.
    GaleraMember,
    /// No replication visible.
    Standalone,
    /// The user may not see replication status.
    Unknown,
}

/// Replication as far as the user can see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Replication {
    /// The role.
    pub role: ReplicationRole,
    /// False when a privilege was missing; [`Replication::message`] says
    /// which statement was refused.
    pub visible: bool,
    /// Why replication is not (fully) visible.
    pub message: Option<String>,
    /// Servers replicating from this one.
    pub replicas: Vec<ReplicaInfo>,
    /// Sources this server replicates from (MariaDB multi-source and MySQL
    /// channels give several).
    pub upstream: Vec<UpstreamInfo>,
}

/// A server replicating from this one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaInfo {
    /// PostgreSQL `application_name` (secrets redacted), MySQL and MariaDB
    /// server id.
    pub name: Option<String>,
    /// Address of the replica.
    pub address: Option<String>,
    /// PostgreSQL WAL sender state (`streaming`, `catchup`, ...).
    pub state: Option<String>,
    /// PostgreSQL `sync_state` (`async`, `sync`, `quorum`, ...).
    pub sync_state: Option<String>,
    /// Replay lag in seconds (PostgreSQL `replay_lag`).
    pub lag_seconds: Option<f64>,
    /// WAL bytes not yet replayed by the replica (PostgreSQL).
    pub lag_bytes: Option<i64>,
}

/// A source this server replicates from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamInfo {
    /// MySQL channel or MariaDB connection name (empty for the default).
    pub channel: Option<String>,
    /// Source host.
    pub source_host: Option<String>,
    /// Source port.
    pub source_port: Option<i64>,
    /// PostgreSQL WAL receiver status, MySQL and MariaDB IO thread state.
    pub state: Option<String>,
    /// MySQL and MariaDB IO thread running (`Yes`, `No`, `Connecting`).
    pub io_running: Option<String>,
    /// MySQL and MariaDB SQL thread running.
    pub sql_running: Option<String>,
    /// Lag in seconds: PostgreSQL time since the last replayed transaction,
    /// MySQL `Seconds_Behind_Source`, MariaDB `Seconds_Behind_Master`.
    /// `None` when unknown (a stopped thread).
    pub lag_seconds: Option<f64>,
    /// WAL bytes received but not yet replayed (PostgreSQL).
    pub lag_bytes: Option<i64>,
    /// Seconds since the last replayed transaction was committed on the
    /// primary (PostgreSQL), as reported; grows on an idle primary.
    pub last_replay_age_seconds: Option<f64>,
    /// How [`UpstreamInfo::lag_seconds`] was derived, for the UI.
    pub lag_note: Option<String>,
    /// WAL received so far (PostgreSQL `pg_last_wal_receive_lsn`).
    pub receive_position: Option<String>,
    /// WAL replayed so far (PostgreSQL `pg_last_wal_replay_lsn`).
    pub replay_position: Option<String>,
    /// The primary's end of WAL as last reported to the WAL receiver
    /// (PostgreSQL `latest_end_lsn`); `None` without a WAL receiver.
    pub primary_position: Option<String>,
    /// WAL bytes between [`UpstreamInfo::primary_position`] and the replayed
    /// position.
    pub primary_lag_bytes: Option<i64>,
    /// Last IO or SQL error.
    pub last_error: Option<String>,
    /// GTID position (MySQL executed set, MariaDB `Gtid_IO_Pos`).
    pub gtid_position: Option<String>,
}

/// One `SHOW STATUS` value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusValue {
    /// Variable name.
    pub name: String,
    /// Value as the server reports it.
    pub value: String,
}

/// MariaDB Galera cluster status from the `wsrep_%` status variables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GaleraStatus {
    /// True when `wsrep_on` is on and a provider is loaded: the server is a
    /// Galera node. When false the other fields describe a disconnected
    /// node and the panel shows "not a cluster member".
    pub enabled: bool,
    /// Nodes in the cluster (`wsrep_cluster_size`).
    pub cluster_size: Option<i64>,
    /// `Primary`, `non-Primary` or `Disconnected` (`wsrep_cluster_status`).
    pub cluster_status: Option<String>,
    /// The node accepts queries (`wsrep_ready`).
    pub ready: Option<bool>,
    /// The node is connected to the cluster (`wsrep_connected`).
    pub connected: Option<bool>,
    /// Node state: `Synced`, `Donor/Desynced`, `Joining`, ...
    /// (`wsrep_local_state_comment`).
    pub local_state_comment: Option<String>,
    /// Share of time replication was paused by flow control since the last
    /// `FLUSH STATUS`, 0 to 1 (`wsrep_flow_control_paused`).
    pub flow_control_paused: Option<f64>,
    /// Cluster state UUID (`wsrep_cluster_state_uuid`).
    pub cluster_state_uuid: Option<String>,
    /// Provider name (`wsrep_provider_name`).
    pub provider_name: Option<String>,
    /// Provider version (`wsrep_provider_version`).
    pub provider_version: Option<String>,
    /// Every `wsrep_%` status value, for the full panel.
    pub values: Vec<StatusValue>,
}

/// Reads the live status summary.
///
/// # Errors
///
/// [`LiveError::Unsupported`] for Redis, [`LiveError::PermissionDenied`]
/// when the user may not read the basic status, other server errors.
/// Missing replication privileges are reported in [`Replication`] instead.
pub async fn server_status(session: &dyn Session, server: &ServerInfo) -> LiveResult<ServerStatus> {
    match Family::of(server)? {
        Family::Postgres => pg_status(session, server).await,
        family @ (Family::Mysql | Family::Mariadb) => mysql_status(session, server, family).await,
    }
}

/// Reads the MariaDB Galera status.
///
/// # Errors
///
/// [`LiveError::Unsupported`] for engines other than MariaDB, server
/// errors.
pub async fn galera_status(session: &dyn Session, server: &ServerInfo) -> LiveResult<GaleraStatus> {
    if Family::of(server)? != Family::Mariadb {
        return Err(LiveError::Unsupported {
            message: "the Galera panel is for MariaDB servers".into(),
        });
    }
    let variables = status_map(
        &rows::query(
            session,
            "SHOW GLOBAL VARIABLES WHERE Variable_name IN ('wsrep_on', 'wsrep_provider')",
        )
        .await?,
    )?;
    let status = rows::query(session, "SHOW GLOBAL STATUS LIKE 'wsrep\\_%'").await?;
    galera_from(&variables, &status_values(&status)?)
}

// PostgreSQL --------------------------------------------------------------

// Every name in these statements is qualified: they are parsed under the
// user's search_path, where an unqualified function, operator, type or
// relation could resolve to an object planted in another schema. A chain of
// `OPERATOR(...)` has one precedence and binds left to right, so mixed
// chains are parenthesized.

const PG_SUMMARY: &str = "SELECT pg_catalog.version() AS version_detail, \
     pg_catalog.pg_postmaster_start_time() AS started_at, \
     pg_catalog.date_part('epoch', pg_catalog.now() OPERATOR(pg_catalog.-) \
       pg_catalog.pg_postmaster_start_time())::bigint AS uptime_s, \
     (SELECT pg_catalog.count(*) FROM pg_catalog.pg_stat_activity \
       WHERE backend_type OPERATOR(pg_catalog.=) 'client backend') AS connections, \
     pg_catalog.current_setting('max_connections')::bigint AS max_connections, \
     (SELECT pg_catalog.count(*) FROM pg_catalog.pg_stat_activity \
       WHERE backend_type OPERATOR(pg_catalog.=) 'client backend' \
       AND state OPERATOR(pg_catalog.=) 'active' \
       AND pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid()) AS active_queries, \
     (SELECT CASE WHEN (s.hits OPERATOR(pg_catalog.+) s.reads) OPERATOR(pg_catalog.>) 0 \
       THEN s.hits::pg_catalog.float8 OPERATOR(pg_catalog./) \
         (s.hits OPERATOR(pg_catalog.+) s.reads)::pg_catalog.float8 END \
       FROM (SELECT pg_catalog.sum(blks_hit) AS hits, pg_catalog.sum(blks_read) AS reads \
         FROM pg_catalog.pg_stat_database) AS s) AS cache_hit_ratio, \
     pg_catalog.pg_is_in_recovery() AS in_recovery";

const PG_DATABASES: &str = "SELECT d.datname AS name, \
     CASE WHEN pg_catalog.has_database_privilege(d.oid, 'CONNECT') \
       THEN pg_catalog.pg_database_size(d.oid) END AS size_bytes, \
     s.numbackends AS connections \
     FROM pg_catalog.pg_database d \
     LEFT JOIN pg_catalog.pg_stat_database s ON s.datid OPERATOR(pg_catalog.=) d.oid \
     WHERE NOT d.datistemplate ORDER BY d.datname";

/// Downstream replicas. On a standby (cascading replication) the position
/// to compare with is the received WAL, not the current WAL, which a standby
/// cannot report.
const PG_REPLICAS: &str = "SELECT application_name, pg_catalog.host(client_addr) AS address, \
     state, sync_state, \
     pg_catalog.date_part('epoch', replay_lag)::pg_catalog.float8 AS lag_seconds, \
     pg_catalog.pg_wal_lsn_diff(CASE WHEN pg_catalog.pg_is_in_recovery() \
       THEN pg_catalog.pg_last_wal_receive_lsn() ELSE pg_catalog.pg_current_wal_lsn() END, \
       replay_lsn)::bigint AS lag_bytes \
     FROM pg_catalog.pg_stat_replication ORDER BY application_name, pid";

/// The upstream of a standby; one row even when no WAL receiver runs
/// (archive recovery, a lost connection), so the replay state is always
/// reported. `latest_end_lsn` is the primary's end of WAL as its last
/// message reported it.
const PG_UPSTREAM: &str = "SELECT r.status, r.sender_host, r.sender_port, r.slot_name, \
     pg_catalog.pg_last_wal_receive_lsn() OPERATOR(pg_catalog.=) \
       pg_catalog.pg_last_wal_replay_lsn() AS caught_up, \
     pg_catalog.date_part('epoch', pg_catalog.now() OPERATOR(pg_catalog.-) \
       pg_catalog.pg_last_xact_replay_timestamp())::pg_catalog.float8 AS replay_age_seconds, \
     pg_catalog.pg_wal_lsn_diff(pg_catalog.pg_last_wal_receive_lsn(), \
       pg_catalog.pg_last_wal_replay_lsn())::bigint AS lag_bytes, \
     pg_catalog.pg_last_wal_receive_lsn()::pg_catalog.text AS receive_lsn, \
     pg_catalog.pg_last_wal_replay_lsn()::pg_catalog.text AS replay_lsn, \
     r.latest_end_lsn::pg_catalog.text AS primary_lsn, \
     pg_catalog.pg_wal_lsn_diff(r.latest_end_lsn, \
       pg_catalog.pg_last_wal_replay_lsn())::bigint AS primary_lag_bytes \
     FROM (SELECT 1) AS one LEFT JOIN pg_catalog.pg_stat_wal_receiver r ON true";

async fn pg_status(session: &dyn Session, server: &ServerInfo) -> LiveResult<ServerStatus> {
    let summary = rows::query(session, PG_SUMMARY).await?;
    let summary = summary.single("the server summary")?;
    let in_recovery = summary.bool("in_recovery")?.unwrap_or(false);
    let databases = rows::query(session, PG_DATABASES)
        .await?
        .iter()
        .map(|row| {
            Ok(DatabaseSize {
                name: row.req_text("name")?,
                size_bytes: row.int("size_bytes")?,
                connections: row.int("connections")?,
            })
        })
        .collect::<LiveResult<Vec<_>>>()?;
    let replicas = rows::query(session, PG_REPLICAS)
        .await?
        .iter()
        .map(|row| {
            Ok(ReplicaInfo {
                name: non_empty(&row, "application_name")?
                    .map(|a| redact_secrets(&a, SqlDialect::Postgres)),
                address: row.text("address")?,
                state: row.text("state")?,
                sync_state: row.text("sync_state")?,
                lag_seconds: float(&row, "lag_seconds")?,
                lag_bytes: row.int("lag_bytes")?,
            })
        })
        .collect::<LiveResult<Vec<_>>>()?;
    let upstream = if in_recovery {
        rows::query(session, PG_UPSTREAM)
            .await?
            .iter()
            .map(|row| {
                let state = row.text("status")?;
                let (lag_seconds, lag_note) = pg_time_lag(
                    state.as_deref(),
                    row.bool("caught_up")?,
                    float(&row, "replay_age_seconds")?,
                );
                Ok(UpstreamInfo {
                    channel: row.text("slot_name")?,
                    source_host: row.text("sender_host")?,
                    source_port: row.int("sender_port")?,
                    state,
                    io_running: None,
                    sql_running: None,
                    lag_seconds,
                    lag_bytes: row.int("lag_bytes")?,
                    last_replay_age_seconds: float(&row, "replay_age_seconds")?,
                    lag_note: Some(lag_note),
                    receive_position: row.text("receive_lsn")?,
                    replay_position: row.text("replay_lsn")?,
                    primary_position: row.text("primary_lsn")?,
                    primary_lag_bytes: row.int("primary_lag_bytes")?,
                    last_error: None,
                    gtid_position: None,
                })
            })
            .collect::<LiveResult<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let role = role_of(in_recovery, !replicas.is_empty());
    Ok(ServerStatus {
        engine: server.engine,
        version: server.version.clone(),
        version_detail: summary.text("version_detail")?,
        started_at: summary.text("started_at")?,
        uptime_seconds: summary.int("uptime_s")?,
        connections: summary.int("connections")?,
        max_connections: summary.int("max_connections")?,
        max_used_connections: None,
        active_queries: summary.int("active_queries")?,
        cache_hit_ratio: float(&summary, "cache_hit_ratio")?,
        databases,
        replication: Replication {
            role,
            visible: true,
            message: None,
            replicas,
            upstream,
        },
        galera: None,
    })
}

/// The time lag of a PostgreSQL standby and how it was derived. Only a
/// streaming WAL receiver tells that "received equals replayed" means
/// caught up; without one the standby does not know what it misses.
fn pg_time_lag(
    state: Option<&str>,
    caught_up: Option<bool>,
    replay_age: Option<f64>,
) -> (Option<f64>, String) {
    if state != Some("streaming") {
        return (
            None,
            format!(
                "WAL receiver not streaming (state {}); lag unknown",
                state.unwrap_or("not running")
            ),
        );
    }
    match (caught_up, replay_age) {
        (Some(true), _) => (
            Some(0.0),
            "caught up: all received WAL is replayed (the primary may simply be idle)".into(),
        ),
        (_, Some(age)) => (
            Some(age.max(0.0)),
            "WAL is waiting to be replayed: time since the last replayed transaction was committed on the primary".into(),
        ),
        (_, None) => (
            None,
            "no transaction has been replayed since the standby started".into(),
        ),
    }
}

/// A Galera node is a cluster member unless it is also an asynchronous
/// replica of a server outside the cluster.
fn with_galera(role: ReplicationRole, galera: Option<&GaleraStatus>) -> ReplicationRole {
    match (role, galera) {
        (ReplicationRole::Replica, _) => ReplicationRole::Replica,
        (_, Some(g)) if g.enabled => ReplicationRole::GaleraMember,
        (role, _) => role,
    }
}

fn role_of(replicates: bool, has_replicas: bool) -> ReplicationRole {
    if replicates {
        ReplicationRole::Replica
    } else if has_replicas {
        ReplicationRole::Primary
    } else {
        ReplicationRole::Standalone
    }
}

// MySQL and MariaDB -------------------------------------------------------

const MY_STATUS: &str = "SHOW GLOBAL STATUS WHERE Variable_name IN ('Uptime', \
     'Threads_connected', 'Threads_running', 'Max_used_connections', \
     'Innodb_buffer_pool_reads', 'Innodb_buffer_pool_read_requests')";

const MY_VARIABLES: &str = "SHOW GLOBAL VARIABLES WHERE Variable_name IN ('max_connections', \
     'version_comment', 'wsrep_on', 'wsrep_provider')";

/// `SHOW STATUS` / `SHOW VARIABLES` rows by lowercased name.
fn status_map(rows: &Rows) -> LiveResult<BTreeMap<String, String>> {
    rows.iter()
        .map(|row| {
            Ok((
                row.req_text("Variable_name")?.to_ascii_lowercase(),
                row.text("Value")?.unwrap_or_default(),
            ))
        })
        .collect()
}

fn status_values(rows: &Rows) -> LiveResult<Vec<StatusValue>> {
    rows.iter()
        .map(|row| {
            Ok(StatusValue {
                name: row.req_text("Variable_name")?,
                value: row.text("Value")?.unwrap_or_default(),
            })
        })
        .collect()
}

fn map_int(map: &BTreeMap<String, String>, name: &str) -> Option<i64> {
    map.get(name).and_then(|v| v.trim().parse().ok())
}

fn map_float(map: &BTreeMap<String, String>, name: &str) -> Option<f64> {
    map.get(name).and_then(|v| v.trim().parse().ok())
}

fn map_bool(map: &BTreeMap<String, String>, name: &str) -> Option<bool> {
    map.get(name)
        .and_then(|v| match v.trim().to_ascii_uppercase().as_str() {
            "ON" | "YES" | "TRUE" | "1" => Some(true),
            "OFF" | "NO" | "FALSE" | "0" => Some(false),
            _ => None,
        })
}

fn map_text(map: &BTreeMap<String, String>, name: &str) -> Option<String> {
    map.get(name).filter(|v| !v.is_empty()).cloned()
}

/// InnoDB buffer pool hit ratio from its cumulative counters.
fn innodb_hit_ratio(status: &BTreeMap<String, String>) -> Option<f64> {
    let requests = map_float(status, "innodb_buffer_pool_read_requests")?;
    let reads = map_float(status, "innodb_buffer_pool_reads")?;
    (requests > 0.0).then(|| (1.0 - reads / requests).clamp(0.0, 1.0))
}

async fn mysql_status(
    session: &dyn Session,
    server: &ServerInfo,
    family: Family,
) -> LiveResult<ServerStatus> {
    let status = status_map(&rows::query(session, MY_STATUS).await?)?;
    let variables = status_map(&rows::query(session, MY_VARIABLES).await?)?;
    let replication = mysql_replication(session, server, family).await?;
    let mut replication = replication;
    let galera = if family == Family::Mariadb {
        let wsrep = rows::query(session, "SHOW GLOBAL STATUS LIKE 'wsrep\\_%'").await?;
        Some(galera_from(&variables, &status_values(&wsrep)?)?)
    } else {
        None
    };
    replication.role = with_galera(replication.role, galera.as_ref());
    Ok(ServerStatus {
        engine: server.engine,
        version: server.version.clone(),
        version_detail: map_text(&variables, "version_comment"),
        started_at: None,
        uptime_seconds: map_int(&status, "uptime"),
        connections: map_int(&status, "threads_connected"),
        max_connections: map_int(&variables, "max_connections"),
        max_used_connections: map_int(&status, "max_used_connections"),
        // Threads_running counts the thread reading the status.
        active_queries: map_int(&status, "threads_running").map(|n| (n - 1).max(0)),
        cache_hit_ratio: innodb_hit_ratio(&status),
        databases: Vec::new(),
        replication,
        galera,
    })
}

/// Runs a replication `SHOW` statement; a missing privilege gives `None`
/// and the refusal message.
async fn optional_show(session: &dyn Session, sql: &str) -> LiveResult<Result<Rows, String>> {
    match rows::query(session, sql).await.map_err(LiveError::from) {
        Ok(rows) => Ok(Ok(rows)),
        Err(LiveError::PermissionDenied { message, .. }) => Ok(Err(format!("{sql}: {message}"))),
        Err(error) => Err(error),
    }
}

async fn mysql_replication(
    session: &dyn Session,
    server: &ServerInfo,
    family: Family,
) -> LiveResult<Replication> {
    let new_syntax = family == Family::Mysql && at_least_patch(server, 8, 0, 22);
    let (status_sql, hosts_sql) = match (family, new_syntax) {
        (Family::Mariadb, _) => ("SHOW ALL SLAVES STATUS", "SHOW SLAVE HOSTS"),
        (_, true) => ("SHOW REPLICA STATUS", "SHOW REPLICAS"),
        (_, false) => ("SHOW SLAVE STATUS", "SHOW SLAVE HOSTS"),
    };
    let mut messages = Vec::new();
    let upstream = match optional_show(session, status_sql).await? {
        Ok(rows) => Some(
            rows.iter()
                .map(|row| upstream_from(&row))
                .collect::<LiveResult<Vec<_>>>()?,
        ),
        Err(message) => {
            messages.push(message);
            None
        }
    };
    let replicas = match optional_show(session, hosts_sql).await? {
        Ok(rows) => Some(
            rows.iter()
                .map(|row| replica_host_from(&row))
                .collect::<LiveResult<Vec<_>>>()?,
        ),
        Err(message) => {
            messages.push(message);
            None
        }
    };
    let role = match (&upstream, &replicas) {
        (Some(up), _) if !up.is_empty() => ReplicationRole::Replica,
        (_, Some(down)) if !down.is_empty() => ReplicationRole::Primary,
        (Some(_), Some(_)) => ReplicationRole::Standalone,
        _ => ReplicationRole::Unknown,
    };
    Ok(Replication {
        role,
        visible: messages.is_empty(),
        message: (!messages.is_empty()).then(|| messages.join("\n")),
        replicas: replicas.unwrap_or_default(),
        upstream: upstream.unwrap_or_default(),
    })
}

/// The first of several column names (new `Source`/`Replica` names, then
/// the old `Master`/`Slave` ones) that the row has.
fn any_text(row: &Row<'_>, columns: &[&str]) -> LiveResult<Option<String>> {
    for column in columns {
        if let Ok(value) = row.value(column) {
            return Ok(value.to_plain_text().filter(|text| !text.is_empty()));
        }
    }
    Ok(None)
}

fn upstream_from(row: &Row<'_>) -> LiveResult<UpstreamInfo> {
    let io_error = any_text(row, &["Last_IO_Error"])?;
    let sql_error = any_text(row, &["Last_SQL_Error"])?;
    let last_error = match (io_error, sql_error) {
        (Some(io), Some(sql)) => Some(format!("{io}\n{sql}")),
        (io, sql) => io.or(sql),
    };
    let lag_seconds = any_text(row, &["Seconds_Behind_Source", "Seconds_Behind_Master"])?
        .and_then(|text| text.trim().parse::<f64>().ok());
    Ok(UpstreamInfo {
        channel: any_text(row, &["Channel_Name", "Connection_name"])?,
        source_host: any_text(row, &["Source_Host", "Master_Host"])?,
        source_port: any_text(row, &["Source_Port", "Master_Port"])?
            .and_then(|text| text.trim().parse().ok()),
        state: any_text(row, &["Replica_IO_State", "Slave_IO_State"])?,
        io_running: any_text(row, &["Replica_IO_Running", "Slave_IO_Running"])?,
        sql_running: any_text(row, &["Replica_SQL_Running", "Slave_SQL_Running"])?,
        lag_seconds,
        lag_bytes: None,
        last_replay_age_seconds: None,
        lag_note: None,
        receive_position: None,
        replay_position: None,
        primary_position: None,
        primary_lag_bytes: None,
        last_error,
        gtid_position: any_text(row, &["Gtid_IO_Pos", "Executed_Gtid_Set"])?,
    })
}

fn replica_host_from(row: &Row<'_>) -> LiveResult<ReplicaInfo> {
    let host = any_text(row, &["Host"])?;
    let port = any_text(row, &["Port"])?;
    Ok(ReplicaInfo {
        name: any_text(row, &["Server_Id", "Server_id"])?,
        address: match (host, port) {
            (Some(host), Some(port)) => Some(format!("{host}:{port}")),
            (host, _) => host,
        },
        state: None,
        sync_state: None,
        lag_seconds: None,
        lag_bytes: None,
    })
}

fn galera_from(
    variables: &BTreeMap<String, String>,
    values: &[StatusValue],
) -> LiveResult<GaleraStatus> {
    let status: BTreeMap<String, String> = values
        .iter()
        .map(|v| (v.name.to_ascii_lowercase(), v.value.clone()))
        .collect();
    let provider = map_text(variables, "wsrep_provider");
    let enabled = map_bool(variables, "wsrep_on").unwrap_or(false)
        && provider.is_some_and(|p| !p.eq_ignore_ascii_case("none"));
    Ok(GaleraStatus {
        enabled,
        cluster_size: map_int(&status, "wsrep_cluster_size"),
        cluster_status: map_text(&status, "wsrep_cluster_status"),
        ready: map_bool(&status, "wsrep_ready"),
        connected: map_bool(&status, "wsrep_connected"),
        local_state_comment: map_text(&status, "wsrep_local_state_comment"),
        flow_control_paused: map_float(&status, "wsrep_flow_control_paused"),
        cluster_state_uuid: map_text(&status, "wsrep_cluster_state_uuid"),
        provider_name: map_text(&status, "wsrep_provider_name"),
        provider_version: map_text(&status, "wsrep_provider_version"),
        values: values.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_driver::{Column, ExecOutput, StatementResult, Value};

    fn rows(columns: &[&str], values: Vec<Vec<&str>>) -> LiveResult<Rows> {
        Ok(Rows::first(ExecOutput {
            results: vec![StatementResult::Rows {
                columns: columns
                    .iter()
                    .map(|name| Column {
                        name: (*name).into(),
                        type_name: "text".into(),
                    })
                    .collect(),
                rows: values
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|v| {
                                if v == "NULL" {
                                    Value::Null
                                } else {
                                    Value::Text(v.into())
                                }
                            })
                            .collect()
                    })
                    .collect(),
                truncated: false,
            }],
            notices: vec![],
            elapsed_ms: 0,
        })?)
    }

    fn show(pairs: &[(&str, &str)]) -> LiveResult<Rows> {
        rows(
            &["Variable_name", "Value"],
            pairs.iter().map(|(n, v)| vec![*n, *v]).collect(),
        )
    }

    #[test]
    fn galera_cluster_member() -> LiveResult<()> {
        let variables = status_map(&show(&[
            ("wsrep_on", "ON"),
            ("wsrep_provider", "/usr/lib/galera/libgalera_smm.so"),
        ])?)?;
        let values = status_values(&show(&[
            ("wsrep_cluster_size", "3"),
            ("wsrep_cluster_status", "Primary"),
            ("wsrep_ready", "ON"),
            ("wsrep_connected", "ON"),
            ("wsrep_local_state_comment", "Synced"),
            ("wsrep_flow_control_paused", "0.012"),
            ("wsrep_local_index", "18446744073709551615"),
            ("wsrep_provider_version", "26.4.20(r7f52e9f)"),
        ])?)?;
        let galera = galera_from(&variables, &values)?;
        assert!(galera.enabled);
        assert_eq!(galera.cluster_size, Some(3));
        assert_eq!(galera.cluster_status.as_deref(), Some("Primary"));
        assert_eq!(galera.ready, Some(true));
        assert_eq!(galera.connected, Some(true));
        assert_eq!(galera.local_state_comment.as_deref(), Some("Synced"));
        assert_eq!(galera.flow_control_paused, Some(0.012));
        assert_eq!(galera.values.len(), 8);
        Ok(())
    }

    #[test]
    fn galera_disabled() -> LiveResult<()> {
        let variables = status_map(&show(&[("wsrep_on", "OFF"), ("wsrep_provider", "none")])?)?;
        let values = status_values(&show(&[
            ("wsrep_cluster_size", "0"),
            ("wsrep_cluster_status", "Disconnected"),
            ("wsrep_ready", "OFF"),
            ("wsrep_connected", "OFF"),
            ("wsrep_provider_name", ""),
        ])?)?;
        let galera = galera_from(&variables, &values)?;
        assert!(!galera.enabled);
        assert_eq!(galera.ready, Some(false));
        assert_eq!(galera.provider_name, None);
        // wsrep_on without a provider is not a cluster member either.
        let on_without_provider =
            status_map(&show(&[("wsrep_on", "ON"), ("wsrep_provider", "none")])?)?;
        assert!(!galera_from(&on_without_provider, &values)?.enabled);
        Ok(())
    }

    #[test]
    fn innodb_ratio() -> LiveResult<()> {
        let status = status_map(&show(&[
            ("Innodb_buffer_pool_read_requests", "1000"),
            ("Innodb_buffer_pool_reads", "10"),
        ])?)?;
        assert_eq!(innodb_hit_ratio(&status), Some(0.99));
        let empty = status_map(&show(&[
            ("Innodb_buffer_pool_read_requests", "0"),
            ("Innodb_buffer_pool_reads", "0"),
        ])?)?;
        assert_eq!(innodb_hit_ratio(&empty), None);
        Ok(())
    }

    #[test]
    fn mysql_replica_status_new_names() -> LiveResult<()> {
        let set = rows(
            &[
                "Replica_IO_State",
                "Source_Host",
                "Source_Port",
                "Replica_IO_Running",
                "Replica_SQL_Running",
                "Seconds_Behind_Source",
                "Last_IO_Error",
                "Last_SQL_Error",
                "Executed_Gtid_Set",
                "Channel_Name",
            ],
            vec![vec![
                "Waiting for source to send event",
                "db1",
                "3306",
                "Yes",
                "Yes",
                "4",
                "",
                "",
                "uuid:1-10",
                "",
            ]],
        )?;
        let up = upstream_from(&set.single("row")?)?;
        assert_eq!(up.source_host.as_deref(), Some("db1"));
        assert_eq!(up.source_port, Some(3306));
        assert_eq!(up.lag_seconds, Some(4.0));
        assert_eq!(up.io_running.as_deref(), Some("Yes"));
        assert_eq!(up.last_error, None);
        assert_eq!(up.channel, None);
        assert_eq!(up.gtid_position.as_deref(), Some("uuid:1-10"));
        Ok(())
    }

    #[test]
    fn mariadb_slave_status_old_names() -> LiveResult<()> {
        let set = rows(
            &[
                "Connection_name",
                "Slave_IO_State",
                "Master_Host",
                "Master_Port",
                "Slave_IO_Running",
                "Slave_SQL_Running",
                "Seconds_Behind_Master",
                "Last_IO_Error",
                "Last_SQL_Error",
                "Gtid_IO_Pos",
            ],
            vec![vec![
                "east",
                "",
                "db2",
                "3307",
                "No",
                "Yes",
                "NULL",
                "error connecting",
                "",
                "0-1-100",
            ]],
        )?;
        let up = upstream_from(&set.single("row")?)?;
        assert_eq!(up.channel.as_deref(), Some("east"));
        assert_eq!(up.state, None);
        assert_eq!(up.lag_seconds, None);
        assert_eq!(up.last_error.as_deref(), Some("error connecting"));
        assert_eq!(up.gtid_position.as_deref(), Some("0-1-100"));
        Ok(())
    }

    #[test]
    fn replica_hosts() -> LiveResult<()> {
        let set = rows(
            &["Server_Id", "Host", "Port", "Source_Id", "Replica_UUID"],
            vec![vec!["2", "replica1", "3306", "1", "u"]],
        )?;
        let replica = replica_host_from(&set.single("row")?)?;
        assert_eq!(replica.name.as_deref(), Some("2"));
        assert_eq!(replica.address.as_deref(), Some("replica1:3306"));
        Ok(())
    }

    #[test]
    fn standby_lag() {
        let streaming = Some("streaming");
        let (lag, note) = pg_time_lag(streaming, Some(true), Some(294.0));
        assert_eq!(lag, Some(0.0));
        assert!(note.contains("caught up"));
        let (lag, note) = pg_time_lag(streaming, Some(false), Some(3.5));
        assert_eq!(lag, Some(3.5));
        assert!(note.contains("waiting"));
        assert_eq!(pg_time_lag(streaming, None, None).0, None);
        assert_eq!(pg_time_lag(streaming, None, Some(-0.2)).0, Some(0.0));
        // A disconnected standby is never "caught up".
        let (lag, note) = pg_time_lag(None, Some(true), Some(10.0));
        assert_eq!(lag, None);
        assert_eq!(
            note,
            "WAL receiver not streaming (state not running); lag unknown"
        );
        let (lag, note) = pg_time_lag(Some("waiting"), Some(true), Some(10.0));
        assert_eq!(lag, None);
        assert!(note.contains("state waiting"));
    }

    #[test]
    fn galera_role() -> LiveResult<()> {
        let variables = status_map(&show(&[("wsrep_on", "ON"), ("wsrep_provider", "/x.so")])?)?;
        let member = galera_from(&variables, &[])?;
        assert_eq!(
            with_galera(ReplicationRole::Standalone, Some(&member)),
            ReplicationRole::GaleraMember
        );
        assert_eq!(
            with_galera(ReplicationRole::Unknown, Some(&member)),
            ReplicationRole::GaleraMember
        );
        assert_eq!(
            with_galera(ReplicationRole::Replica, Some(&member)),
            ReplicationRole::Replica
        );
        let off = status_map(&show(&[("wsrep_on", "OFF")])?)?;
        assert_eq!(
            with_galera(ReplicationRole::Standalone, Some(&galera_from(&off, &[])?)),
            ReplicationRole::Standalone
        );
        assert_eq!(
            serde_json::to_value(ReplicationRole::GaleraMember).map_err(|e| {
                LiveError::Unexpected {
                    message: e.to_string(),
                }
            })?,
            "galeraMember"
        );
        Ok(())
    }

    #[test]
    fn roles() {
        assert_eq!(role_of(true, true), ReplicationRole::Replica);
        assert_eq!(role_of(false, true), ReplicationRole::Primary);
        assert_eq!(role_of(false, false), ReplicationRole::Standalone);
    }
}
