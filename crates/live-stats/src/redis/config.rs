//! Server configuration: `CONFIG GET` (with secrets masked), `CONFIG SET`
//! plans with validation and warnings, and `CONFIG REWRITE` plans.
//!
//! Redis 7.0 and newer (and Valkey) set several parameters in one atomic
//! `CONFIG SET`; Redis 6.2 takes one parameter per call, so a 6.2 plan has
//! one command per parameter and says that a failure leaves the earlier
//! ones applied.

use serde::{Deserialize, Serialize};
use varve_catalog::CatalogResult;
use varve_driver::{ServerInfo, Session};

use super::CommandPlan;
use super::config_refusals::TABLES;
use super::info::RedisInfo;
use super::reply::{Cmd, invalid, pairs, run, text, unexpected};

/// Parameters whose values are secrets. Their values are never returned.
const SENSITIVE: &[&str] = &[
    "requirepass",
    "masterauth",
    "primaryauth",
    "tls-key-file-pass",
    "tls-client-key-file-pass",
    "auth-pass",
    "sentinel-pass",
];

/// Why `CONFIG SET` refuses a parameter (see [`config_refusal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Refusal {
    /// Only settable at startup or in the configuration file ("can't set
    /// immutable config"; Redis 6.2: "Unsupported CONFIG parameter").
    Immutable,
    /// A protected parameter (Redis 7 and Valkey: `dir`, `dbfilename`),
    /// settable only when the server runs with `enable-protected-configs`.
    Protected,
    /// The server refused to set it to its own current value when probed
    /// (for example `bind` on 6.2, or `rdma-bind` without RDMA support);
    /// other values may work.
    Rejected,
}

/// True for a Valkey server (its version string ends in `-valkey`).
fn is_valkey(server: &ServerInfo) -> bool {
    server.version.ends_with("-valkey")
}

/// The refusal table for `server`: same flavor, the highest table whose
/// lowest version is not above the server's (the oldest when none is).
fn refusal_table(server: &ServerInfo) -> &'static [(&'static str, Refusal, &'static str)] {
    let flavor = if is_valkey(server) { "valkey" } else { "redis" };
    let version = (server.major, server.minor);
    let candidates = TABLES.iter().filter(|t| t.0 == flavor);
    let first = candidates.clone().next();
    candidates
        .filter(|t| t.1 <= version)
        .max_by_key(|t| t.1)
        .or(first)
        .map_or(&[], |t| t.3)
}

/// Whether and why `CONFIG SET` refuses `name` on `server`, from tables
/// probed on Redis 6.2, Redis 8.10 and Valkey 9 (`config_refusals.rs`):
/// Redis before 7.0 uses the 6.2 table, later Redis the 8.10 table, Valkey
/// the Valkey 9 table. Aliases (`slave-*`, `master*`) share an entry.
/// Returns the kind and the server's reason.
#[must_use]
pub fn config_refusal(name: &str, server: &ServerInfo) -> Option<(Refusal, &'static str)> {
    let name = name.trim().to_ascii_lowercase();
    let table = refusal_table(server);
    table
        .iter()
        .find(|(n, _, _)| *n == name)
        .or_else(|| {
            let canonical = canonical_config(&name);
            table
                .iter()
                .find(|(n, _, _)| canonical_config(n) == canonical)
        })
        .map(|(_, refusal, reason)| (*refusal, *reason))
}

/// True when `name` cannot be changed with `CONFIG SET` on `server` at all
/// ([`Refusal::Immutable`]).
#[must_use]
pub fn is_immutable_config(name: &str, server: &ServerInfo) -> bool {
    matches!(config_refusal(name, server), Some((Refusal::Immutable, _)))
}

/// True for a protected parameter on `server` ([`Refusal::Protected`]).
#[must_use]
pub fn is_protected_config(name: &str, server: &ServerInfo) -> bool {
    matches!(config_refusal(name, server), Some((Refusal::Protected, _)))
}

/// The canonical name of a parameter that has aliases: `slave-*` is
/// `replica-*`, `master*` is `primary*` (Valkey), `*-ziplist-*` is
/// `*-listpack-*` (Redis 7) and `lua-time-limit` is `busy-reply-threshold`.
/// Two names with the same canonical name are the same parameter.
#[must_use]
pub fn canonical_config(name: &str) -> String {
    let name = name.trim().to_ascii_lowercase();
    if name == "lua-time-limit" {
        return "busy-reply-threshold".into();
    }
    name.replace("slave", "replica")
        .replace("master", "primary")
        .replace("ziplist", "listpack")
}

/// Values of parameters that take one of a fixed set of words.
const ENUMS: &[(&str, &[&str])] = &[
    ("oom-score-adj", &["no", "yes", "relative", "absolute"]),
    ("sanitize-dump-payload", &["no", "yes", "clients"]),
    ("tls-auth-clients", &["no", "yes", "optional"]),
    ("appendfsync", &["always", "everysec", "no"]),
    (
        "loglevel",
        &["debug", "verbose", "notice", "warning", "nothing"],
    ),
];

/// Parameters that take `yes` or `no` (canonical names).
const BOOLEANS: &[&str] = &[
    "appendonly",
    "activedefrag",
    "activerehashing",
    "aof-load-truncated",
    "aof-rewrite-incremental-fsync",
    "aof-use-rdb-preamble",
    "cluster-allow-reads-when-down",
    "cluster-replica-no-failover",
    "cluster-require-full-coverage",
    "crash-log-enabled",
    "crash-memcheck-enabled",
    "dynamic-hz",
    "jemalloc-bg-thread",
    "latency-tracking",
    "lazyfree-lazy-eviction",
    "lazyfree-lazy-expire",
    "lazyfree-lazy-server-del",
    "lazyfree-lazy-user-del",
    "lazyfree-lazy-user-flush",
    "no-appendfsync-on-rewrite",
    "protected-mode",
    "rdb-del-sync-files",
    "rdb-save-incremental-fsync",
    "rdbcompression",
    "repl-disable-tcp-nodelay",
    "repl-diskless-sync",
    "replica-announced",
    "replica-lazy-flush",
    "replica-read-only",
    "replica-serve-stale-data",
    "stop-writes-on-bgsave-error",
    "tls-cluster",
    "tls-prefer-server-ciphers",
    "tls-replication",
    "tls-session-caching",
];

/// An integer with an optional memory unit (`b`, `k`, `kb`, `m`, `mb`, `g`,
/// `gb`, any case) or a percent sign, as `CONFIG SET` accepts for numeric
/// parameters. Decimals are refused by the server.
fn is_integer_value(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    let v = v.strip_prefix('-').unwrap_or(&v);
    let digits_end = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let (number, unit) = v.split_at(digits_end);
    !number.is_empty() && matches!(unit, "" | "b" | "k" | "kb" | "m" | "mb" | "g" | "gb" | "%")
}

/// Checks a value for `name` (canonical) given its current value. Known
/// enum and yes/no parameters are checked against their words; any other
/// parameter whose current value is an integer takes an integer (with an
/// optional unit); everything else is left to the server.
fn check_value(name: &str, display: &str, old: Option<&str>, value: &str) -> CatalogResult<()> {
    let v = value.trim().to_ascii_lowercase();
    if let Some((_, words)) = ENUMS.iter().find(|(n, _)| *n == name) {
        if !words.contains(&v.as_str()) {
            return Err(invalid(format!(
                "{display} takes one of: {}",
                words.join(", ")
            )));
        }
        return Ok(());
    }
    if BOOLEANS.contains(&name) {
        if v != "yes" && v != "no" {
            return Err(invalid(format!("{display} takes yes or no")));
        }
        return Ok(());
    }
    if let Some(old) = old.map(str::trim)
        && !old.is_empty()
        && old.parse::<i64>().is_ok()
        && !v.is_empty()
        && !is_integer_value(&v)
    {
        return Err(invalid(format!(
            "{display} takes a whole number, optionally with a unit such as mb or gb"
        )));
    }
    Ok(())
}

/// True for a parameter whose value is a secret (`requirepass`,
/// `masterauth`, TLS key passphrases, and any name ending in `pass`,
/// `auth` or containing `password`). `name` must be lower case.
#[must_use]
pub fn is_sensitive_config(name: &str) -> bool {
    SENSITIVE.contains(&name)
        || name.ends_with("pass")
        || name.ends_with("auth")
        || name.contains("password")
}

/// Why changing a parameter is risky, or `None`. `name` must be lower case.
#[must_use]
pub fn config_warning(name: &str) -> Option<&'static str> {
    Some(match name {
        "dir" | "dbfilename" | "appendfilename" => {
            "Changes where the server writes files on its host. Writing the RDB or AOF file to an \
             arbitrary path is a known way to overwrite files on the server, and a wrong path \
             makes the next save fail."
        }
        "requirepass" => {
            "Changes the password of the default user. Connections that authenticate with the \
             old password, including Varve's, fail on their next reconnect."
        }
        "masterauth" | "masteruser" | "primaryauth" | "primaryuser" => {
            "Changes how this replica authenticates to its primary. A wrong value breaks \
             replication at the next reconnect."
        }
        "protected-mode" => "Turning protected mode off can expose the server to the network.",
        "bind" | "port" | "tls-port" | "unixsocket" => {
            "Changes where the server accepts connections. Clients, including this connection, \
             may lose access."
        }
        "maxmemory" => {
            "A limit below the memory in use makes the server evict keys or reject writes at once, \
             depending on maxmemory-policy."
        }
        "maxmemory-policy" => "Changes which keys the server evicts when it reaches maxmemory.",
        "appendonly" => {
            "Turning AOF on starts a background rewrite of the whole dataset (fork and disk I/O); \
             turning it off stops append-only persistence."
        }
        "save" => {
            "Changes when RDB snapshots are taken. An empty value turns snapshots off, so a \
             restart can lose data."
        }
        "replica-read-only" | "slave-read-only" => {
            "A writable replica accepts writes that are lost at the next resynchronization."
        }
        "replica-serve-stale-data" | "slave-serve-stale-data" => {
            "Changes whether a replica answers with possibly outdated data while disconnected."
        }
        _ => return None,
    })
}

/// One configuration parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigEntry {
    /// Parameter name, lower case.
    pub name: String,
    /// The value; always `None` for a sensitive parameter.
    pub value: Option<String>,
    /// The value is a secret and was masked.
    pub sensitive: bool,
    /// For a sensitive parameter: whether a value is set (not empty).
    pub has_value: bool,
    /// Changing this parameter is risky (see [`config_warning`]).
    pub dangerous: bool,
    /// The parameter can only be set at startup.
    pub immutable: bool,
    /// Protected: settable only with `enable-protected-configs`.
    pub protected: bool,
}

/// Builds the entries of a `CONFIG GET` reply, masking secrets, sorted by
/// name.
#[must_use]
pub fn config_entries(server: &ServerInfo, reply: &varve_driver::Value) -> Vec<ConfigEntry> {
    let mut entries: Vec<ConfigEntry> = pairs(reply)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, value)| {
            let name = name.to_ascii_lowercase();
            let value = text(value).unwrap_or_default();
            let sensitive = is_sensitive_config(&name);
            ConfigEntry {
                dangerous: config_warning(&name).is_some(),
                immutable: is_immutable_config(&name, server),
                protected: is_protected_config(&name, server),
                has_value: !value.is_empty(),
                value: (!sensitive).then_some(value),
                sensitive,
                name,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Reads `CONFIG GET <pattern>` (`*` for all), with secrets masked.
///
/// # Errors
///
/// Driver errors (a Sentinel has no `CONFIG GET`; ACL users may lack the
/// permission).
pub async fn config_get(
    session: &dyn Session,
    server: &ServerInfo,
    pattern: &str,
) -> CatalogResult<Vec<ConfigEntry>> {
    let reply = run(session, &Cmd::new("CONFIG").arg("GET").arg(pattern)).await?;
    if pairs(&reply).is_none() {
        return Err(unexpected("CONFIG GET did not return name/value pairs"));
    }
    Ok(config_entries(server, &reply))
}

/// One parameter change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChange {
    /// Parameter name.
    pub name: String,
    /// New value.
    pub value: String,
}

/// Text shown instead of a secret value in a plan.
pub const MASK: &str = "********";

/// Builds the `CONFIG SET` plan for `changes`, checked against the current
/// configuration `current` (from [`config_get`] with `*`).
///
/// Validation: names must exist on the server and be settable at runtime,
/// may appear once, and values must not contain line breaks; a parameter
/// whose current value is a number takes a number (with an optional
/// `kb`/`mb`/`gb` or `%` suffix), and a `yes`/`no` parameter takes `yes` or
/// `no`. Risky parameters add a warning and mark the plan destructive.
/// Sensitive values are masked in [`CommandPlan::display`].
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Invalid`] naming the first problem.
pub fn config_set_plan(
    server: &ServerInfo,
    current: &[ConfigEntry],
    changes: &[ConfigChange],
) -> CatalogResult<CommandPlan> {
    if changes.is_empty() {
        return Err(invalid("No configuration change to apply"));
    }
    let mut seen: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut destructive = false;
    let mut sensitive_any = false;
    for change in changes {
        let name = change.name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(invalid("A parameter name is empty"));
        }
        let canonical = canonical_config(&name);
        if let Some(other) = seen.iter().find(|n| canonical_config(n) == canonical) {
            return Err(invalid(if *other == name {
                format!("{name} is changed twice")
            } else {
                format!("{other} and {name} are the same parameter")
            }));
        }
        seen.push(name.clone());
        let Some(entry) = current.iter().find(|e| e.name == name) else {
            return Err(invalid(format!("The server has no parameter {name}")));
        };
        if is_immutable_config(&name, server) {
            return Err(invalid(format!(
                "{name} can only be set in the configuration file or at startup on this server"
            )));
        }
        match config_refusal(&name, server) {
            Some((Refusal::Protected, _)) => {
                let mode = current
                    .iter()
                    .find(|e| e.name == "enable-protected-configs")
                    .and_then(|e| e.value.as_deref())
                    .unwrap_or("no");
                match mode {
                    "yes" => {}
                    "local" => warnings.push(format!(
                        "{name} is a protected parameter: the server accepts the change only \
                         from a local (loopback or Unix socket) connection."
                    )),
                    _ => {
                        return Err(invalid(format!(
                            "{name} is a protected parameter: the server refuses to change it \
                             unless it runs with enable-protected-configs"
                        )));
                    }
                }
            }
            Some((Refusal::Rejected, reason)) => warnings.push(format!(
                "{name}: the server refused to set this parameter to its own current value when \
                 probed ({reason}); it may refuse this change too."
            )),
            _ => {}
        }
        if change.value.contains(['\n', '\r']) {
            return Err(invalid(format!(
                "The value of {name} contains a line break"
            )));
        }
        check_value(&canonical, &name, entry.value.as_deref(), &change.value)?;
        if let Some(warning) = config_warning(&name) {
            destructive = true;
            warnings.push(format!("{name}: {warning}"));
        }
        sensitive_any |= entry.sensitive;
    }
    let names: Vec<String> = seen;
    let mut commands = Vec::new();
    let mut display = Vec::new();
    let line = |pairs: &[(&str, &str)], masked: bool| {
        let mut cmd = Cmd::new("CONFIG").arg("SET");
        for (name, value) in pairs {
            let shown = if masked && is_sensitive_config(name) {
                MASK
            } else {
                value
            };
            cmd = cmd.arg(name).arg(shown);
        }
        cmd.line()
    };
    let all: Vec<(&str, &str)> = names
        .iter()
        .zip(changes)
        .map(|(n, c)| (n.as_str(), c.value.as_str()))
        .collect();
    if server.at_least(7, 0) {
        commands.push(line(&all, false));
        display.push(line(&all, true));
    } else {
        for pair in &all {
            commands.push(line(std::slice::from_ref(pair), false));
            display.push(line(std::slice::from_ref(pair), true));
        }
        if all.len() > 1 {
            warnings.push(
                "Redis 6.2 sets one parameter per command: when one fails, the parameters \
                 before it stay changed."
                    .into(),
            );
        }
    }
    warnings.push(
        "CONFIG SET changes the running server only; run CONFIG REWRITE to keep the change \
         after a restart."
            .into(),
    );
    Ok(CommandPlan {
        commands,
        display,
        destructive,
        sensitive: sensitive_any,
        warnings,
    })
}

/// Builds the `CONFIG REWRITE` plan: rewrites the configuration file the
/// server was started with so it matches the running configuration.
///
/// # Errors
///
/// [`varve_catalog::CatalogError::Invalid`] when the server runs without a
/// configuration file (`config_file` empty in `INFO server`), where
/// `CONFIG REWRITE` always fails.
pub fn config_rewrite_plan(server: &ServerInfo, info: &RedisInfo) -> CatalogResult<CommandPlan> {
    let _ = server;
    let file = info
        .server
        .config_file
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if file.is_empty() {
        return Err(invalid(
            "The server was started without a configuration file, so CONFIG REWRITE has \
             nothing to write",
        ));
    }
    let line = Cmd::new("CONFIG").arg("REWRITE").line();
    Ok(CommandPlan {
        commands: vec![line.clone()],
        display: vec![line],
        destructive: true,
        sensitive: false,
        warnings: vec![format!(
            "Rewrites {file} on the server host to match the running configuration. Changes \
             made to the file by hand since startup are replaced; comments are kept."
        )],
    })
}

/// Reads `INFO server` and builds the [`config_rewrite_plan`].
///
/// # Errors
///
/// Driver errors and the errors of [`config_rewrite_plan`].
pub async fn plan_config_rewrite(
    session: &dyn Session,
    server: &ServerInfo,
) -> CatalogResult<CommandPlan> {
    let reply = run(session, &Cmd::new("INFO").arg("server")).await?;
    let text = text(&reply).ok_or_else(|| unexpected("INFO did not return text"))?;
    config_rewrite_plan(server, &RedisInfo::parse(&text))
}

/// Reads the configuration and builds the [`config_set_plan`].
///
/// # Errors
///
/// Driver errors and the errors of [`config_set_plan`].
pub async fn plan_config_set(
    session: &dyn Session,
    server: &ServerInfo,
    changes: &[ConfigChange],
) -> CatalogResult<CommandPlan> {
    let current = config_get(session, server, "*").await?;
    config_set_plan(server, &current, changes)
}

/// The value of one non-sensitive parameter, or `None` when the server
/// does not have it.
pub(crate) async fn config_value(
    session: &dyn Session,
    name: &str,
) -> CatalogResult<Option<String>> {
    let reply = run(session, &Cmd::new("CONFIG").arg("GET").arg(name)).await?;
    Ok(pairs(&reply).and_then(|p| {
        p.into_iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| text(v))
    }))
}
