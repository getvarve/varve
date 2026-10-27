//! Unit tests against replies captured from Redis 6.2, Redis 8.10 and
//! Valkey 9 (`testdata/*.json`: the console results the Redis driver
//! returned, keyed by command line). Cluster and Sentinel replies come from
//! six-node clusters and two Sentinels per version started only to capture
//! them; the development matrix runs neither. The `requirepass` value in
//! the fixtures is a placeholder.

use std::collections::HashMap;

use varve_catalog::CatalogError;
use varve_core::Engine;
use varve_driver::{ServerInfo, StatementResult, Value};

use super::info::{config_count, database_list};
use super::memory::{Measured, Scale, key_prefix, summarize};
use super::reply::{reply_value, text};
use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SERVERS: [&str; 3] = ["redis-6.2", "redis-8.10", "valkey-9"];

fn fixture_text(server: &str) -> &'static str {
    match server {
        "redis-6.2" => include_str!("testdata/redis-6.2.json"),
        "redis-8.10" => include_str!("testdata/redis-8.10.json"),
        _ => include_str!("testdata/valkey-9.json"),
    }
}

/// The captured reply of `command` on `server`, or the captured error text.
fn captured(
    server: &str,
    command: &str,
) -> Result<Result<Value, String>, Box<dyn std::error::Error>> {
    let map: HashMap<String, serde_json::Value> = serde_json::from_str(fixture_text(server))?;
    let entry = map
        .get(command)
        .ok_or_else(|| format!("no capture of {command} for {server}"))?
        .clone();
    if let Some(error) = entry.get("error") {
        return Ok(Err(error.as_str().unwrap_or_default().to_string()));
    }
    let result: StatementResult = serde_json::from_value(entry)?;
    Ok(Ok(reply_value(result)?))
}

fn reply(server: &str, command: &str) -> Result<Value, Box<dyn std::error::Error>> {
    captured(server, command)?.map_err(|e| format!("{server} {command} failed: {e}").into())
}

fn reply_text(server: &str, command: &str) -> Result<String, Box<dyn std::error::Error>> {
    text(&reply(server, command)?).ok_or_else(|| "not text".into())
}

fn server_info(server: &str) -> ServerInfo {
    let (version, major, minor) = match server {
        "redis-6.2" => ("6.2.24", 6, 2),
        "redis-8.10" => ("8.10.2", 8, 10),
        _ => ("9.1.2-valkey", 9, 1),
    };
    ServerInfo {
        engine: Engine::Redis,
        version: version.into(),
        major,
        minor,
        patch: None,
    }
}

#[test]
fn info_parses_on_every_server() -> TestResult {
    for server in SERVERS {
        let info = RedisInfo::parse(&reply_text(server, "INFO everything")?);
        assert_eq!(info.mode, ServerMode::Standalone, "{server}");
        assert!(info.memory.used_memory.is_some_and(|m| m > 0), "{server}");
        assert_eq!(info.memory.maxmemory_policy.as_deref(), Some("noeviction"));
        assert!(info.memory.used_memory_scripts_vm.is_some(), "{server}");
        assert_eq!(info.replication.role.as_deref(), Some("master"));
        assert_eq!(info.replication.connected_replicas, Some(0));
        assert!(info.cpu.used_cpu_sys.is_some());
        assert_eq!(info.stats.hit_ratio, None, "no lookups yet");
        assert!(info.clients.connected_clients.is_some_and(|c| c >= 1));
        assert!(!info.cluster.enabled);
        assert_eq!(info.persistence.aof_enabled, Some(false));
        assert!(info.server.run_id.is_some());
        assert!(info.sentinel.is_none());
        // Unknown fields are kept, not dropped.
        assert!(info.server.other.iter().any(|f| f.key == "gcc_version"));
    }
    let redis6 = RedisInfo::parse(&reply_text("redis-6.2", "INFO everything")?);
    assert_eq!(redis6.flavor, Flavor::Redis);
    assert_eq!(redis6.server.redis_version.as_deref(), Some("6.2.24"));
    assert!(redis6.modules.is_empty());
    assert_eq!(
        redis6.clients.pubsub_clients, None,
        "6.2 has no pubsub_clients"
    );

    let redis8 = RedisInfo::parse(&reply_text("redis-8.10", "INFO everything")?);
    let search = redis8.modules.iter().find(|m| m.name == "search");
    assert_eq!(
        search.map(|m| m.using.clone()),
        Some(vec!["ReJSON".to_string()])
    );
    assert_eq!(redis8.modules.len(), 5);
    assert!(
        redis8
            .commandstats
            .iter()
            .any(|c| c.command == "config|get" && c.calls == 2)
    );
    assert!(redis8.sections.iter().any(|s| s.name == "search_version"));
    assert!(redis8.sections.iter().any(|s| s.name == "Latencystats"));

    let valkey = RedisInfo::parse(&reply_text("valkey-9", "INFO everything")?);
    assert_eq!(valkey.flavor, Flavor::Valkey);
    assert_eq!(valkey.server.valkey_version.as_deref(), Some("9.1.2"));
    assert_eq!(
        valkey.server.mode.as_deref(),
        Some("standalone"),
        "from server_mode"
    );
    assert!(
        valkey
            .sections
            .iter()
            .any(|s| s.name == "Scripting Engines")
    );
    assert!(valkey.sections.iter().any(|s| s.name == "TLS"));
    assert_eq!(valkey.modules.first().map(|m| m.name.as_str()), Some("lua"));
    Ok(())
}

#[test]
fn info_reads_cluster_sentinel_and_replication() -> TestResult {
    for server in SERVERS {
        let cluster = RedisInfo::parse(&format!(
            "{}\r\n{}",
            reply_text(server, "cluster: INFO server")?,
            reply_text(server, "cluster: INFO cluster")?
        ));
        assert_eq!(cluster.mode, ServerMode::Cluster, "{server}");
        assert!(cluster.cluster.enabled);

        let sentinel = RedisInfo::parse(&format!(
            "{}\r\n{}",
            reply_text(server, "sentinel: INFO server")?,
            reply_text(server, "sentinel: INFO sentinel")?
        ));
        assert_eq!(sentinel.mode, ServerMode::Sentinel, "{server}");
        let section = sentinel.sentinel.ok_or("no sentinel section")?;
        assert_eq!(section.masters_count, Some(1));
        let master = section.masters.first().ok_or("no master")?;
        assert_eq!(master.name.as_deref(), Some("mymaster"));
        assert_eq!(master.status.as_deref(), Some("ok"));
        assert_eq!(master.replicas, Some(1));

        let primary = RedisInfo::parse(&reply_text(server, "master: INFO replication")?);
        let link = primary
            .replication
            .replicas
            .first()
            .ok_or("no replica line")?;
        assert_eq!(link.port, Some(6381));
        assert_eq!(link.state.as_deref(), Some("online"));
        assert_eq!(primary.replication.connected_replicas, Some(1));

        let replica = RedisInfo::parse(&reply_text(server, "replica: INFO replication")?);
        assert_eq!(replica.replication.role.as_deref(), Some("slave"));
        assert_eq!(replica.replication.master_port, Some(6380));
        assert_eq!(
            replica.replication.master_link_status.as_deref(),
            Some("up")
        );
        assert!(replica.replication.replica_repl_offset.is_some());
        assert!(replica.replication.replicas.is_empty());
        assert!(
            replica
                .replication
                .other
                .iter()
                .any(|f| f.key == "slave_priority"),
            "slave_* fields that are not replica lines stay"
        );
    }
    let redis8 = RedisInfo::parse(&reply_text("redis-8.10", "master: INFO replication")?);
    assert!(
        redis8.replication.replicas[0]
            .other
            .iter()
            .any(|f| f.key == "io-thread")
    );
    Ok(())
}

#[test]
fn keyspace_and_database_list() {
    let info = RedisInfo::parse(
        "# Keyspace\r\ndb0:keys=3,expires=1,avg_ttl=500\r\ndb12:keys=7,expires=0,avg_ttl=0,subexpiry=2\r\n",
    );
    assert_eq!(info.keyspace.len(), 2);
    assert_eq!(info.keyspace[1].db, 12);
    assert_eq!(info.keyspace[1].other[0].key, "subexpiry");
    let all = database_list(Some(16), &info.keyspace);
    assert_eq!(all.len(), 16);
    assert_eq!(all[0].keys, 3);
    assert_eq!(all[0].avg_ttl_ms, 500);
    assert_eq!(all[1].keys, 0);
    assert_eq!(all[12].keys, 7);
    let known = database_list(None, &info.keyspace);
    assert_eq!(
        known.iter().map(|d| d.index).collect::<Vec<_>>(),
        vec![0, 12]
    );
}

#[test]
fn client_list_marks_own_connection() -> TestResult {
    for server in SERVERS {
        let own = super::reply::int(&reply(server, "CLIENT ID")?).ok_or("no id")?;
        let list = parse_client_list(&reply_text(server, "CLIENT LIST")?, Some(own));
        let me = list
            .iter()
            .find(|c| c.is_self)
            .ok_or("own client not marked")?;
        assert_eq!(me.id, own);
        assert_eq!(me.name.as_deref(), Some("m2_redislive_capture"));
        assert_eq!(me.db, Some(10));
        assert_eq!(me.flags, "N");
        assert!(me.flag_names.is_empty());
        assert_eq!(me.user.as_deref(), Some("default"));
        assert!(me.cmd.as_deref().is_some_and(|c| c.starts_with("client")));
    }
    let redis6 = parse_client_list(&reply_text("redis-6.2", "CLIENT LIST")?, None);
    assert_eq!(redis6[0].ssub, None, "6.2 has no shard subscriptions");
    assert_eq!(redis6[0].lib_name, None);
    let redis8 = parse_client_list(&reply_text("redis-8.10", "CLIENT LIST")?, None);
    assert_eq!(redis8[0].lib_name.as_deref(), Some("varve"));
    assert!(redis8[0].other.iter().any(|f| f.key == "io-thread"));
    let valkey = parse_client_list(&reply_text("valkey-9", "CLIENT LIST")?, None);
    assert!(valkey[0].other.iter().any(|f| f.key == "capa"));

    let blocked = parse_client_list(
        "id=9 addr=1.2.3.4:5 name= flags=bxU db=0 cmd=blpop\nid=10 addr=1.2.3.4:6 flags=S\n",
        Some(1),
    );
    assert_eq!(blocked.len(), 2);
    assert_eq!(blocked[0].name, None);
    assert_eq!(
        blocked[0].flag_names,
        ["blocked", "in MULTI/EXEC", "unix socket"]
    );
    assert!(!blocked[0].is_self);
    assert_eq!(blocked[1].flag_names, ["replica"]);
    Ok(())
}

#[test]
fn slowlog_entries_on_every_server() -> TestResult {
    for server in SERVERS {
        let entries = parse_slowlog(&reply(server, "SLOWLOG GET 3")?)?;
        assert_eq!(entries.len(), 3, "{server}");
        assert!(
            entries.windows(2).all(|w| w[0].id > w[1].id),
            "newest first"
        );
        let eval = &entries[0];
        assert!(eval.duration_us > 10_000);
        assert_eq!(eval.args.first(), Some(&Value::Text("EVAL".into())));
        assert_eq!(eval.client_name.as_deref(), Some("m2_redislive_capture"));
        assert!(eval.client_addr.is_some());
        assert!(super::reply::int(&reply(server, "SLOWLOG LEN")?).is_some_and(|n| n >= 3));
    }
    let redis8 = parse_slowlog(&reply("redis-8.10", "SLOWLOG GET 3")?)?;
    assert_eq!(redis8[0].extra.len(), 1, "Redis 8 adds a field");
    let redis6 = parse_slowlog(&reply("redis-6.2", "SLOWLOG GET 3")?)?;
    assert!(redis6[0].extra.is_empty());

    // Redis 6.2 logs CONFIG SET requirepass with its value; it is hidden.
    let secret = Value::Array(vec![Value::Array(vec![
        Value::Int(1),
        Value::Int(1_700_000_000),
        Value::Int(12),
        Value::Array(vec![
            Value::Text("CONFIG".into()),
            Value::Text("SET".into()),
            Value::Text("requirepass".into()),
            Value::Text("hunter2".into()),
        ]),
        Value::Text("1.2.3.4:5".into()),
        Value::Text(String::new()),
    ])]);
    let parsed = parse_slowlog(&secret)?;
    assert_eq!(parsed[0].args[3], Value::Text(REDACTED.into()));
    assert_eq!(parsed[0].client_name, None);
    assert!(!serde_json::to_string(&parsed)?.contains("hunter2"));
    assert!(parse_slowlog(&Value::Int(1)).is_err());
    Ok(())
}

#[test]
fn latency_events_on_every_server() -> TestResult {
    for server in SERVERS {
        let events = parse_latency_latest(&reply(server, "LATENCY LATEST")?)?;
        let command = events.first().ok_or("no event")?;
        assert_eq!(command.event, "command");
        assert!(command.latest_ms > 0 && command.max_ms >= command.latest_ms);
        let history = parse_latency_history(&reply(server, "LATENCY HISTORY command")?)?;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].time, command.time);
        assert!(reply_text(server, "LATENCY DOCTOR")?.contains("command"));
    }
    let valkey = parse_latency_latest(&reply("valkey-9", "LATENCY LATEST")?)?;
    assert_eq!(
        valkey[0].count,
        Some(1),
        "Valkey reports the number of spikes"
    );
    assert!(valkey[0].sum_ms.is_some());
    let redis = parse_latency_latest(&reply("redis-8.10", "LATENCY LATEST")?)?;
    assert_eq!(redis[0].count, None);
    Ok(())
}

#[test]
fn memory_stats_on_every_server() -> TestResult {
    for server in SERVERS {
        let stats = parse_memory_stats(&reply(server, "MEMORY STATS")?)?;
        assert!(stats.total_allocated.is_some_and(|n| n > 0), "{server}");
        assert_eq!(stats.keys_count, Some(6));
        assert!(
            stats.fragmentation.is_some_and(|f| f > 0.0),
            "ratio from a string"
        );
        assert!(stats.dataset_percentage.is_some());
        assert!(
            stats
                .databases
                .iter()
                .any(|d| d.db == 10 && d.main_bytes.is_some())
        );
        assert!(
            stats
                .fields
                .iter()
                .any(|f| f.name == "fragmentation" && matches!(f.value, Value::Float(_)))
        );
        assert_eq!(
            super::reply::int(&reply(server, "MEMORY USAGE m2_redislive:user:1")?).map(|n| n > 0),
            Some(true)
        );
        assert_eq!(
            reply(server, "MEMORY USAGE m2_redislive:nokey")?,
            Value::Null
        );
        assert!(!reply_text(server, "MEMORY DOCTOR")?.is_empty());
    }
    Ok(())
}

#[test]
fn memory_grouping_by_prefix_and_type() -> TestResult {
    assert_eq!(key_prefix(b"user:1:name", b":", 1), Some(b"user:".to_vec()));
    assert_eq!(
        key_prefix(b"user:1:name", b":", 2),
        Some(b"user:1:".to_vec())
    );
    assert_eq!(key_prefix(b"user:1", b":", 3), Some(b"user:".to_vec()));
    assert_eq!(key_prefix(b"plain", b":", 1), None);
    assert_eq!(key_prefix(b"a::b", b"::", 1), Some(b"a::".to_vec()));
    assert_eq!(key_prefix(b"a:b", b"", 1), None);
    let m = |key: &[u8], key_type: &str, bytes: u64| Measured {
        key: key.to_vec(),
        key_type: key_type.into(),
        bytes,
    };
    let measured = vec![
        m(b"user:1", "hash", 100),
        m(b"user:2", "hash", 300),
        m(b"session:a", "string", 50),
        m(b"plain", "string", 10),
        m(&[0xff, b':', b'x'], "list", 5),
    ];
    let options = MemoryScanOptions {
        top_keys: 1,
        ..MemoryScanOptions::default()
    };
    let (prefixes, other, types) = summarize(&measured, &options, Scale::of(false, false, 5, 10));
    assert_eq!(other, None);
    assert_eq!(prefixes[0].prefix, Some(Value::Text("user:".into())));
    assert_eq!(prefixes[0].keys, 2);
    assert_eq!(prefixes[0].bytes, 400);
    assert_eq!(prefixes[0].avg_bytes, 200);
    assert_eq!(prefixes[0].max_key, Value::Text("user:2".into()));
    assert_eq!(
        prefixes[0].estimated_bytes,
        Some(800),
        "5 of 10 keys scanned"
    );
    assert!(prefixes.iter().any(|p| p.prefix.is_none() && p.bytes == 10));
    assert!(
        prefixes
            .iter()
            .any(|p| p.prefix == Some(Value::Bytes(vec![0xff, b':'])))
    );
    assert_eq!(types[0].key_type, "hash");
    assert_eq!(types[0].biggest.len(), 1);
    assert_eq!(types[0].biggest[0].bytes, 300);
    let (complete, _, _) = summarize(&measured, &options, Scale::of(true, true, 5, 10));
    assert_eq!(complete[0].estimated_bytes, Some(400));
    // With a pattern the share of matching keys in DBSIZE is unknown.
    let (patterned, _, _) = summarize(&measured, &options, Scale::of(false, true, 5, 10));
    assert_eq!(patterned[0].estimated_bytes, None);

    // Prefixes beyond the limit are summed.
    let capped = MemoryScanOptions {
        max_prefixes: 2,
        ..MemoryScanOptions::default()
    };
    let (top, other, _) = summarize(&measured, &capped, Scale::Exact);
    assert_eq!(top.len(), 2);
    let other = other.ok_or("no other bucket")?;
    assert_eq!(other.prefixes, 2);
    assert_eq!(other.keys, 2);
    assert_eq!(other.bytes, 15);
    assert_eq!(other.estimated_bytes, Some(15));
    Ok(())
}

#[test]
fn memory_grouping_is_linear() -> TestResult {
    // 50,000 keys over 20,000 prefixes: a linear search per key took 35 s.
    let measured: Vec<Measured> = (0..50_000_u64)
        .map(|i| Measured {
            key: format!("p{}:{i}", i % 20_000).into_bytes(),
            key_type: if i % 3 == 0 { "hash" } else { "string" }.into(),
            bytes: i,
        })
        .collect();
    let started = std::time::Instant::now();
    let (prefixes, other, types) =
        summarize(&measured, &MemoryScanOptions::default(), Scale::Exact);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(prefixes.len(), 100);
    let other = other.ok_or("no other bucket")?;
    assert_eq!(other.prefixes, 19_900);
    assert_eq!(
        prefixes.iter().map(|p| p.keys).sum::<u64>() + other.keys,
        50_000
    );
    assert_eq!(types.len(), 2);
    assert_eq!(types[0].biggest[0].bytes, 49_999);
    Ok(())
}

#[test]
fn config_masks_secrets() -> TestResult {
    for server in SERVERS {
        let entries = config_entries(&server_info(server), &reply(server, "CONFIG GET *")?);
        assert!(entries.len() > 100, "{server}");
        let pass = entries
            .iter()
            .find(|e| e.name == "requirepass")
            .ok_or("no requirepass")?;
        assert!(pass.sensitive && pass.has_value && pass.dangerous);
        assert_eq!(pass.value, None);
        let auth = entries
            .iter()
            .find(|e| e.name == "masterauth")
            .ok_or("no masterauth")?;
        assert!(auth.sensitive && !auth.has_value);
        let json = serde_json::to_string(&entries)?;
        assert!(
            !json.contains("fixture-secret"),
            "{server} leaks the password"
        );
        assert!(entries.iter().any(|e| e.name == "dir" && e.dangerous));
        assert!(entries.iter().any(|e| e.name == "databases" && e.immutable));
        let maxmemory = entries
            .iter()
            .find(|e| e.name == "maxmemory")
            .ok_or("no maxmemory")?;
        assert_eq!(maxmemory.value.as_deref(), Some("0"));
        assert!(entries.windows(2).all(|w| w[0].name <= w[1].name));
        let single = config_entries(
            &server_info(server),
            &reply(server, "CONFIG GET requirepass")?,
        );
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].value, None);
    }
    assert!(is_sensitive_config("tls-key-file-pass"));
    assert!(!is_sensitive_config("tls-auth-clients"));
    assert!(!is_sensitive_config("masteruser"));
    Ok(())
}

fn change(name: &str, value: &str) -> ConfigChange {
    ConfigChange {
        name: name.into(),
        value: value.into(),
    }
}

fn invalid_message(result: varve_catalog::CatalogResult<CommandPlan>) -> String {
    match result {
        Err(CatalogError::Invalid { message }) => message,
        other => format!("not invalid: {other:?}"),
    }
}

#[test]
fn config_set_plans_and_validation() -> TestResult {
    let redis6 = config_entries(
        &server_info("redis-6.2"),
        &reply("redis-6.2", "CONFIG GET *")?,
    );
    let redis8 = config_entries(
        &server_info("redis-8.10"),
        &reply("redis-8.10", "CONFIG GET *")?,
    );
    let changes = [
        change("maxmemory-policy", "allkeys-lru"),
        change("requirepass", "new secret"),
    ];
    let plan8 = config_set_plan(&server_info("redis-8.10"), &redis8, &changes)?;
    assert_eq!(
        plan8.commands,
        ["CONFIG SET maxmemory-policy allkeys-lru requirepass \"new secret\""]
    );
    assert_eq!(
        plan8.display,
        ["CONFIG SET maxmemory-policy allkeys-lru requirepass ********"]
    );
    assert!(plan8.destructive && plan8.sensitive);
    assert!(plan8.warnings.iter().any(|w| w.starts_with("requirepass:")));
    assert!(
        !format!("{plan8:?}").contains("new secret"),
        "Debug shows the masked lines"
    );

    let plan6 = config_set_plan(&server_info("redis-6.2"), &redis6, &changes)?;
    assert_eq!(
        plan6.commands.len(),
        2,
        "6.2 sets one parameter per command"
    );
    assert!(
        plan6
            .warnings
            .iter()
            .any(|w| w.contains("one parameter per command"))
    );

    let plain = config_set_plan(
        &server_info("valkey-9"),
        &config_entries(
            &server_info("valkey-9"),
            &reply("valkey-9", "CONFIG GET *")?,
        ),
        &[change("maxmemory", "1gb"), change("save", "3600 1")],
    )?;
    assert_eq!(plain.commands, ["CONFIG SET maxmemory 1gb save \"3600 1\""]);
    assert!(!plain.sensitive);
    let hz = config_set_plan(&server_info("redis-8.10"), &redis8, &[change("hz", "20")])?;
    assert!(!hz.destructive);

    let s = server_info("redis-8.10");
    assert!(
        invalid_message(config_set_plan(&s, &redis8, &[change("nosuch", "1")]))
            .contains("no parameter")
    );
    assert!(
        invalid_message(config_set_plan(&s, &redis8, &[change("databases", "4")]))
            .contains("startup")
    );
    assert!(
        invalid_message(config_set_plan(&s, &redis8, &[change("hz", "fast")]))
            .contains("whole number")
    );
    assert!(
        invalid_message(config_set_plan(
            &s,
            &redis8,
            &[change("appendonly", "true")]
        ))
        .contains("yes or no")
    );
    assert!(
        invalid_message(config_set_plan(
            &s,
            &redis8,
            &[change("replica-announce-ip", "a\nb")]
        ))
        .contains("line break")
    );
    assert!(
        invalid_message(config_set_plan(
            &s,
            &redis8,
            &[change("hz", "1"), change("HZ", "2")]
        ))
        .contains("twice")
    );
    assert!(invalid_message(config_set_plan(&s, &redis8, &[])).contains("No configuration"));
    Ok(())
}

#[test]
fn config_values_aliases_and_immutables_per_version() -> TestResult {
    let redis6 = server_info("redis-6.2");
    let redis8 = server_info("redis-8.10");
    let valkey = server_info("valkey-9");
    let conf6 = config_entries(&redis6, &reply("redis-6.2", "CONFIG GET *")?);
    let conf8 = config_entries(&redis8, &reply("redis-8.10", "CONFIG GET *")?);
    let conf9 = config_entries(&valkey, &reply("valkey-9", "CONFIG GET *")?);
    // Words beyond yes/no that the server accepts.
    for (name, value) in [
        ("oom-score-adj", "relative"),
        ("oom-score-adj", "absolute"),
        ("sanitize-dump-payload", "clients"),
        ("tls-auth-clients", "optional"),
        ("maxmemory-clients", "10%"),
        ("maxmemory", "512MB"),
        ("save", ""),
        ("slave-read-only", "yes"),
    ] {
        config_set_plan(&redis8, &conf8, &[change(name, value)])
            .map_err(|e| format!("{name}={value}: {e}"))?;
    }
    for (name, value, message) in [
        ("oom-score-adj", "sometimes", "one of"),
        ("sanitize-dump-payload", "maybe", "one of"),
        ("appendonly", "1", "yes or no"),
        ("maxmemory", "1.5gb", "whole number"),
        ("hz", "0x10", "whole number"),
        ("port", "1e3", "whole number"),
    ] {
        assert!(
            invalid_message(config_set_plan(&redis8, &conf8, &[change(name, value)]))
                .contains(message),
            "{name}={value}"
        );
    }
    // Aliases name one parameter.
    assert!(
        invalid_message(config_set_plan(
            &redis8,
            &conf8,
            &[
                change("replica-read-only", "yes"),
                change("slave-read-only", "yes")
            ]
        ))
        .contains("same parameter")
    );
    assert_eq!(
        canonical_config("slave-serve-stale-data"),
        "replica-serve-stale-data"
    );
    assert_eq!(
        canonical_config("hash-max-ziplist-entries"),
        "hash-max-listpack-entries"
    );
    assert_eq!(canonical_config("lua-time-limit"), "busy-reply-threshold");
    assert_eq!(
        canonical_config("masterauth"),
        canonical_config("primaryauth")
    );
    // io-threads: immutable on Redis, settable on Valkey.
    assert!(
        invalid_message(config_set_plan(
            &redis8,
            &conf8,
            &[change("io-threads", "4")]
        ))
        .contains("startup")
    );
    assert!(
        invalid_message(config_set_plan(
            &redis6,
            &conf6,
            &[change("io-threads", "4")]
        ))
        .contains("startup")
    );
    config_set_plan(&valkey, &conf9, &[change("io-threads", "4")])?;
    // disable-thp: settable on 6.2, immutable from 7.0.
    assert!(
        conf6
            .iter()
            .any(|e| e.name == "io-threads-do-reads" && e.immutable)
    );
    assert!(!is_immutable_config("disable-thp", &redis6));
    assert!(is_immutable_config("disable-thp", &redis8));
    assert!(
        conf8
            .iter()
            .any(|e| e.name == "cluster-port" && e.immutable)
    );
    Ok(())
}

#[test]
fn config_refusals_from_the_probed_tables() -> TestResult {
    let redis6 = server_info("redis-6.2");
    let redis8 = server_info("redis-8.10");
    let valkey = server_info("valkey-9");
    for server in [&redis6, &redis8, &valkey] {
        for name in [
            "rdbchecksum",
            "slaveof",
            "replicaof",
            "server_cpulist",
            "bio_cpulist",
            "daemonize",
        ] {
            assert!(
                is_immutable_config(name, server),
                "{name} on {}",
                server.version
            );
        }
        assert!(!is_immutable_config("maxmemory", server));
    }
    for name in [
        "socket-mark-id",
        "backupdirname",
        "preload-file",
        "search-workers",
        "ts-num-threads",
    ] {
        if config_refusal(name, &redis8).is_some() {
            assert!(is_immutable_config(name, &redis8), "{name}");
        }
    }
    assert!(is_immutable_config("socket-mark-id", &redis8));
    assert!(is_immutable_config("preload-file", &redis8));
    for name in [
        "hash-seed",
        "mptcp",
        "repl-mptcp",
        "unixsocketgroup",
        "cluster-databases",
        "rdma-rx-size",
    ] {
        assert!(is_immutable_config(name, &valkey), "{name}");
    }
    // A Redis 7.4 server uses the 8.10 table, Valkey 8 the Valkey 9 table.
    let redis74 = ServerInfo {
        version: "7.4.1".into(),
        major: 7,
        minor: 4,
        ..redis8.clone()
    };
    assert!(is_immutable_config("disable-thp", &redis74));
    assert!(!is_immutable_config("disable-thp", &redis6));
    let valkey8 = ServerInfo {
        version: "8.1.0-valkey".into(),
        major: 8,
        minor: 1,
        ..valkey.clone()
    };
    assert!(is_immutable_config("hash-seed", &valkey8));
    assert!(!is_immutable_config("io-threads", &valkey8));

    // dir and dbfilename are protected on 7.0+ unless enable-protected-configs.
    assert!(is_protected_config("dir", &redis8) && is_protected_config("dbfilename", &valkey));
    assert!(!is_protected_config("dir", &redis6));
    let conf8 = config_entries(&redis8, &reply("redis-8.10", "CONFIG GET *")?);
    assert!(conf8.iter().any(|e| e.name == "dir" && e.protected));
    assert!(
        invalid_message(config_set_plan(&redis8, &conf8, &[change("dir", "/tmp")]))
            .contains("enable-protected-configs")
    );
    let allowed: Vec<ConfigEntry> = conf8
        .iter()
        .cloned()
        .map(|mut e| {
            if e.name == "enable-protected-configs" {
                e.value = Some("local".into());
            }
            e
        })
        .collect();
    let plan = config_set_plan(&redis8, &allowed, &[change("dir", "/tmp")])?;
    assert!(plan.warnings.iter().any(|w| w.contains("local")));
    let conf6 = config_entries(&redis6, &reply("redis-6.2", "CONFIG GET *")?);
    config_set_plan(&redis6, &conf6, &[change("dir", "/tmp")])?;
    // rdbchecksum is not offered as an editable yes/no parameter.
    assert!(
        invalid_message(config_set_plan(
            &redis8,
            &conf8,
            &[change("rdbchecksum", "no")]
        ))
        .contains("startup")
    );
    // A parameter the server refused at its own value only warns.
    let bind = config_set_plan(&redis6, &conf6, &[change("bind", "127.0.0.1")])?;
    assert!(bind.warnings.iter().any(|w| w.contains("refused")));
    Ok(())
}

#[test]
fn config_rewrite_needs_a_file() -> TestResult {
    let server = server_info("redis-8.10");
    let without = RedisInfo::parse(&reply_text("redis-8.10", "INFO everything")?);
    assert!(
        invalid_message(config_rewrite_plan(&server, &without))
            .contains("without a configuration file")
    );
    let with = RedisInfo::parse(&reply_text("valkey-9", "sentinel: INFO server")?);
    let plan = config_rewrite_plan(&server, &with)?;
    assert_eq!(plan.commands, ["CONFIG REWRITE"]);
    assert!(plan.destructive);
    assert!(plan.warnings[0].contains("/tmp/sentinel.conf"));
    Ok(())
}

#[test]
fn pubsub_replies() -> TestResult {
    for server in SERVERS {
        let counts = parse_numsub(&reply(server, "PUBSUB NUMSUB a b")?)?;
        assert_eq!(counts.len(), 2);
        assert_eq!(counts[0].channel, Value::Text("a".into()));
        assert_eq!(counts[0].subscribers, 0);
        assert_eq!(reply(server, "PUBSUB CHANNELS")?, Value::Array(Vec::new()));
    }
    assert!(
        captured("redis-6.2", "PUBSUB SHARDCHANNELS")?.is_err(),
        "7.0 only"
    );
    assert_eq!(
        parse_numsub(&reply("valkey-9", "PUBSUB SHARDNUMSUB a")?)?.len(),
        1
    );
    let resp3 = Value::Array(vec![Value::Array(vec![
        Value::Bytes(vec![0xff]),
        Value::Int(3),
    ])]);
    let parsed = parse_numsub(&resp3)?;
    assert_eq!(parsed[0].channel, Value::Bytes(vec![0xff]));
    assert_eq!(parsed[0].subscribers, 3);
    Ok(())
}

#[test]
fn functions_and_scripts() -> TestResult {
    for server in ["redis-8.10", "valkey-9"] {
        let libs = parse_function_list(&reply(
            server,
            "FUNCTION LIST WITHCODE LIBRARYNAME m2_redislive*",
        )?)?;
        assert_eq!(libs.len(), 1);
        let lib = &libs[0];
        assert_eq!(lib.name, "m2_redislive_lib");
        assert_eq!(lib.engine, "LUA");
        assert!(lib.code.as_deref().is_some_and(|c| c.starts_with("#!lua")));
        let ro = lib
            .functions
            .iter()
            .find(|f| f.name == "m2_redislive_ro")
            .ok_or("no function")?;
        assert_eq!(ro.flags, ["no-writes"]);
        assert_eq!(ro.description.as_deref(), Some("read only"));
        let plain = lib
            .functions
            .iter()
            .find(|f| f.name == "m2_redislive_f")
            .ok_or("no function")?;
        assert_eq!(plain.description, None);
        let without =
            parse_function_list(&reply(server, "FUNCTION LIST LIBRARYNAME m2_redislive*")?)?;
        assert_eq!(without[0].code, None);
        let exists = reply(
            server,
            "SCRIPT EXISTS e0e1f9fabfc9d4800c877a703b823ac0578ff8db ffffffffffffffffffffffffffffffffffffffff",
        )?;
        assert_eq!(exists, Value::Array(vec![Value::Int(1), Value::Int(0)]));
    }
    assert!(captured("redis-6.2", "FUNCTION LIST LIBRARYNAME m2_redislive*")?.is_err());

    let code = "#!lua name=lib\nredis.register_function('f', function() return 1 end)";
    assert_eq!(library_name(code).as_deref(), Some("lib"));
    assert_eq!(library_name("return 1"), None);
    let plan = function_load_plan(&server_info("redis-8.10"), code, true)?;
    assert_eq!(plan.commands.len(), 1);
    assert!(!plan.commands[0].contains('\n'), "one console line");
    let args = varve_redis_syntax::split_args(&plan.commands[0])?;
    assert_eq!(args.len(), 4);
    assert_eq!(args[2].text(), "REPLACE");
    assert_eq!(args[3].text(), code, "the code round trips");
    assert!(plan.destructive);
    assert!(matches!(
        function_load_plan(&server_info("redis-6.2"), code, false),
        Err(CatalogError::Unsupported { .. })
    ));
    assert!(matches!(
        function_load_plan(&server_info("valkey-9"), "return 1", false),
        Err(CatalogError::Invalid { .. })
    ));
    let delete = function_delete_plan(&server_info("valkey-9"), "my lib")?;
    assert_eq!(delete.commands, ["FUNCTION DELETE \"my lib\""]);
    let flush = script_flush_plan(&server_info("redis-6.2"), FlushMode::Async);
    assert_eq!(flush.commands, ["SCRIPT FLUSH ASYNC"]);
    assert!(flush.destructive);
    Ok(())
}

#[test]
fn cluster_topology_from_nodes_shards_and_slots() -> TestResult {
    for server in SERVERS {
        let nodes = parse_cluster_nodes(&reply_text(server, "cluster: CLUSTER NODES")?);
        assert_eq!(nodes.len(), 6, "{server}");
        let my_id = reply_text(server, "cluster: CLUSTER MYID")?;
        let state = parse_cluster_info(&reply_text(server, "cluster: CLUSTER INFO")?);
        assert_eq!(state.state.as_deref(), Some("ok"));
        assert_eq!(state.slots_assigned, Some(16_384));
        assert_eq!(state.size, Some(3));
        let mut topology =
            ClusterTopology::new(TopologySource::Nodes, state, Some(my_id.clone()), nodes);
        let primaries: Vec<&ClusterNode> = topology
            .nodes
            .iter()
            .filter(|n| n.role == NodeRole::Primary)
            .collect();
        assert_eq!(primaries.len(), 3);
        assert_eq!(topology.nodes[0].role, NodeRole::Primary);
        assert_eq!(
            topology.nodes[1].role,
            NodeRole::Replica,
            "replicas follow their primary"
        );
        assert_eq!(
            topology.nodes[1].primary_id.as_deref(),
            Some(topology.nodes[0].id.as_str())
        );
        assert!(
            topology
                .nodes
                .iter()
                .all(|n| n.link_state == Some(LinkState::Connected))
        );
        assert!(topology.nodes.iter().any(|n| n.myself && n.id == my_id));
        assert_eq!(topology.slot_map.len(), 3);
        assert_eq!(
            topology.slot_map[0].range,
            SlotRange {
                start: 0,
                end: 5460
            }
        );
        assert_eq!(
            topology.slot_map.iter().map(|o| o.range.len()).sum::<u32>(),
            u32::from(SLOT_COUNT)
        );
        assert!(topology.unassigned.is_empty());
        assert_eq!(
            topology.nodes[0].cluster_port,
            Some(17_000 + topology.nodes[0].port - 7000)
        );

        let slots = parse_cluster_slots(&reply(server, "cluster: CLUSTER SLOTS")?)?;
        let from_slots =
            ClusterTopology::new(TopologySource::Slots, ClusterState::default(), None, slots);
        assert_eq!(from_slots.nodes.len(), 6);
        assert_eq!(
            from_slots.slot_map, topology.slot_map,
            "{server}: SLOTS agrees with NODES"
        );

        match captured(server, "cluster: CLUSTER SHARDS")? {
            Ok(reply) => {
                let shards = parse_cluster_shards(&reply)?;
                assert_eq!(shards.len(), 6);
                let from_shards = ClusterTopology::new(
                    TopologySource::Shards,
                    ClusterState::default(),
                    None,
                    shards.clone(),
                );
                assert_eq!(from_shards.slot_map, topology.slot_map);
                topology.merge_shards(&shards);
                assert!(
                    topology
                        .nodes
                        .iter()
                        .all(|n| n.health.as_deref() == Some("online"))
                );
                assert!(
                    topology
                        .nodes
                        .iter()
                        .all(|n| n.replication_offset.is_some())
                );
            }
            Err(error) => {
                assert_eq!(
                    server, "redis-6.2",
                    "only 6.2 lacks CLUSTER SHARDS: {error}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn cluster_nodes_aux_fields() {
    let text = "\
a1 10.0.0.1:7000@17000, myself,master - 0 0 1 connected 0-5
b1 2001:db8::1:7001@17001 slave a1 0 0 1 connected
c1 10.0.0.3:7002@17002,h3,shard-id=abc,tls-port=0 master - 0 0 2 connected 6-10 [93-<-a1]
e1 10.0.0.5:7004@17004,,shard-id=def,hostname=h5 master - 0 0 4 connected 16383 16384 9-20000 10-6
";
    let nodes = parse_cluster_nodes(text);
    assert_eq!(nodes.len(), 4);
    assert_eq!(nodes[0].hostname, None, "empty host name");
    assert_eq!(nodes[0].port, 7000);
    assert_eq!(nodes[1].ip, "2001:db8::1");
    assert_eq!(nodes[1].port, 7001);
    assert_eq!(nodes[2].hostname.as_deref(), Some("h3"));
    assert_eq!(nodes[2].shard_id.as_deref(), Some("abc"));
    assert_eq!(nodes[2].cluster_port, Some(17_002));
    assert_eq!(nodes[2].aux.len(), 2);
    assert_eq!(nodes[3].hostname.as_deref(), Some("h5"));
    assert_eq!(nodes[3].shard_id.as_deref(), Some("def"));
    assert_eq!(
        nodes[3].slots,
        [SlotRange {
            start: 16_383,
            end: 16_383
        }],
        "out-of-range and reversed slot ranges are dropped"
    );
    assert_eq!(SlotRange::new(10, 6), None);
    assert_eq!(SlotRange::new(6, 10), Some(SlotRange { start: 6, end: 10 }));
    assert_eq!(SlotRange::new(0, SLOT_COUNT), None);
    let reversed = Value::Array(vec![Value::Array(vec![
        Value::Int(10),
        Value::Int(6),
        Value::Array(vec![
            Value::Text("1.2.3.4".into()),
            Value::Int(1),
            Value::Text("n".into()),
        ]),
    ])]);
    assert!(parse_cluster_slots(&reversed).is_err());
    let shard = Value::Array(vec![Value::Array(vec![
        Value::Text("slots".into()),
        Value::Array(vec![
            Value::Int(10),
            Value::Int(6),
            Value::Int(0),
            Value::Int(5),
        ]),
        Value::Text("nodes".into()),
        Value::Array(vec![Value::Array(vec![
            Value::Text("id".into()),
            Value::Text("n".into()),
            Value::Text("role".into()),
            Value::Text("master".into()),
        ])]),
    ])]);
    assert_eq!(
        parse_cluster_shards(&shard)
            .map(|n| n[0].slots.clone())
            .ok(),
        Some(vec![SlotRange { start: 0, end: 5 }])
    );
}

#[test]
fn cluster_nodes_edge_cases() {
    let text = "\
a 10.0.0.1:7000@17000,host-a myself,master - 0 0 1 connected 0-100 200 [300->-b]
b [::1]:7001@17001 master,fail? - 5 6 2 disconnected 101-199 [300-<-a]
c :0@0 noaddr,slave a 0 0 1 disconnected
d 10.0.0.4:7003@17003 handshake - 0 0 0 connected
";
    let nodes = parse_cluster_nodes(text);
    assert_eq!(nodes.len(), 4);
    assert_eq!(nodes[0].hostname.as_deref(), Some("host-a"));
    assert_eq!(
        nodes[0].slots,
        [
            SlotRange { start: 0, end: 100 },
            SlotRange {
                start: 200,
                end: 200
            }
        ]
    );
    assert_eq!(
        nodes[0].migrating,
        [SlotMigration {
            slot: 300,
            node_id: "b".into()
        }]
    );
    assert_eq!(nodes[1].ip, "[::1]");
    assert_eq!(nodes[1].port, 7001);
    assert!(nodes[1].failing);
    assert_eq!(nodes[1].link_state, Some(LinkState::Disconnected));
    assert_eq!(nodes[1].importing[0].slot, 300);
    assert_eq!(nodes[2].role, NodeRole::Replica);
    assert_eq!(nodes[2].ip, "");
    assert_eq!(nodes[3].role, NodeRole::Unknown);
    let topology =
        ClusterTopology::new(TopologySource::Nodes, ClusterState::default(), None, nodes);
    assert!(topology.nodes[0].myself);
    assert_eq!(
        topology.unassigned,
        [SlotRange {
            start: 201,
            end: 16_383
        }]
    );
    assert_eq!(topology.nodes.last().map(|n| n.id.as_str()), Some("d"));
}

#[test]
fn sentinel_replies() -> TestResult {
    for server in SERVERS {
        let masters = parse_masters(&reply(server, "sentinel: SENTINEL MASTERS")?)?;
        assert_eq!(masters.len(), 1, "{server}");
        let master = &masters[0];
        assert_eq!(master.master.name, "mymaster");
        assert_eq!(master.master.port, Some(6380));
        assert_eq!(master.master.flags, ["master"]);
        assert!(!master.master.down);
        assert_eq!(master.quorum, Some(1));
        assert_eq!(master.num_replicas, Some(1));
        let replicas = parse_peers(&reply(server, "sentinel: SENTINEL REPLICAS mymaster")?)?;
        assert_eq!(replicas.len(), 1);
        assert_eq!(replicas[0].port, Some(6381));
        assert_eq!(replicas[0].master_link_status.as_deref(), Some("ok"));
        assert!(replicas[0].repl_offset.is_some());
        let legacy = parse_peers(&reply(server, "sentinel: SENTINEL SLAVES mymaster")?)?;
        assert_eq!(legacy[0].name, replicas[0].name);
        let sentinels = parse_peers(&reply(server, "sentinel: SENTINEL SENTINELS mymaster")?)?;
        assert_eq!(sentinels.len(), 1);
        assert_eq!(sentinels[0].port, Some(26_380));
        assert_eq!(sentinels[0].flags, ["sentinel"]);
    }
    let down = Value::Array(vec![
        Value::Text("name".into()),
        Value::Text("m".into()),
        Value::Text("flags".into()),
        Value::Text("master,s_down,o_down".into()),
    ]);
    assert!(parse_peer(&down).is_some_and(|p| p.down));
    Ok(())
}

#[test]
fn plans_serialize_camel_case() -> TestResult {
    let plan = script_flush_plan(&server_info("redis-8.10"), FlushMode::Default);
    let json = serde_json::to_value(&plan)?;
    assert!(json.get("commands").is_some() && json.get("display").is_some());
    assert_eq!(json["destructive"], serde_json::json!(true));
    let entry = ConfigEntry {
        name: "x".into(),
        value: None,
        sensitive: true,
        has_value: true,
        dangerous: false,
        immutable: false,
        protected: false,
    };
    assert!(serde_json::to_value(&entry)?.get("hasValue").is_some());
    Ok(())
}

#[test]
fn cluster_database_count() {
    use varve_driver::Value;
    // Valkey 9 in cluster mode with cluster-databases 16 (RESP2 flat pairs).
    let valkey = Value::Array(vec![
        Value::Text("cluster-databases".into()),
        Value::Text("16".into()),
    ]);
    assert_eq!(config_count(&valkey, "cluster-databases"), Some(16));
    // Redis and older Valkey: the setting is unknown, so one database.
    assert_eq!(
        config_count(&Value::Array(vec![]), "cluster-databases"),
        None
    );
    assert_eq!(config_count(&valkey, "databases"), None);
}
