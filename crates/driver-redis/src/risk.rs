//! Command classification (design doc 3.2).
//!
//! Commands are classified by their real name: the name in the server's
//! `COMMAND INFO` entry, so a command renamed with `rename-command` (say
//! `CONFIG` to `MYCONF`) is classified as what it is.
//!
//! Read-only sessions use an allowlist. A command runs only when:
//!
//! - it is listed in [`READ_ONLY_EXTRAS`] (vetted connection and
//!   introspection commands without the `readonly` flag, such as `PING`,
//!   `INFO` or `CLIENT LIST`), or
//! - it is a core command the server flags `readonly` (without `write` or
//!   `may_replicate`) and not in the `@admin` or `@dangerous` ACL category
//!   (so `KEYS` and `SORT_RO` are refused), or
//! - it is a module command listed in [`READ_ONLY_MODULE_COMMANDS`] that the
//!   server flags `readonly` too (module flags are not trusted alone),
//!
//! and it is not in [`READ_ONLY_DENY`] (read-only scripts, which can run
//! non-write admin commands). Everything else is refused, unknown commands
//! and unknown subcommands included. The confirmation risk follows the
//! same line: a command a read-only session allows is
//! [`CommandRisk::Read`], every other one is at least
//! [`CommandRisk::Write`], and [`CommandRisk::Destructive`] when it is in
//! the `@dangerous` category or a fixed destructive list.
//!
//! Redis 6.2 describes a command with subcommands as one entry with the
//! flags of all its subcommands together; for those, the subcommand flags
//! of the newer supported servers in [`crate::risk_table`] decide. When
//! `COMMAND INFO` itself is not available (renamed away, or refused by an
//! ACL) the typed name is looked up in those tables; a renamed command is
//! then simply unknown, so a read-only session refuses it.

use std::collections::{HashMap, HashSet};

use redis::Value as Resp;
use serde::{Deserialize, Serialize};

use crate::reply::as_text;
use crate::risk_table;

/// How risky a command is, for the production confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandRisk {
    /// Only reads: the commands a read-only session allows (see the module
    /// documentation).
    Read,
    /// Changes keys or server state, or is not known to be read-only.
    Write,
    /// Can destroy data or stall the server: a fixed list of commands whose
    /// flags understate the risk (for example `FLUSHALL`, `FUNCTION FLUSH`,
    /// `SCRIPT FLUSH`, `CONFIG SET`, `CLIENT KILL`, `KEYS`), scripts that
    /// may write (`EVAL`, `EVALSHA`, `FCALL`: what they do is unknown), and
    /// other commands in the `@dangerous` ACL category that are not
    /// read-only.
    Destructive,
}

/// Commands that are destructive whatever their flags say, as
/// `(name, subcommand)` by real name; a `None` subcommand matches the
/// command as a whole.
pub(crate) const ALWAYS_DESTRUCTIVE: &[(&str, Option<&str>)] = &[
    ("ACL", Some("DELUSER")),
    ("ACL", Some("SETUSER")),
    ("CLIENT", Some("KILL")),
    ("CLUSTER", Some("RESET")),
    ("CONFIG", Some("RESETSTAT")),
    ("CONFIG", Some("REWRITE")),
    ("CONFIG", Some("SET")),
    ("DEBUG", None),
    ("EVAL", None),
    ("EVALSHA", None),
    ("FAILOVER", None),
    ("FCALL", None),
    ("FLUSHALL", None),
    ("FLUSHDB", None),
    ("FUNCTION", Some("DELETE")),
    ("FUNCTION", Some("FLUSH")),
    ("FUNCTION", Some("RESTORE")),
    ("KEYS", None),
    ("MIGRATE", None),
    ("MODULE", Some("LOAD")),
    ("MODULE", Some("LOADEX")),
    ("MODULE", Some("UNLOAD")),
    ("REPLICAOF", None),
    ("RESTORE", None),
    ("SCRIPT", Some("FLUSH")),
    ("SHUTDOWN", None),
    ("SLAVEOF", None),
    ("SWAPDB", None),
];

/// Commands a read-only session allows although the server does not flag
/// them `readonly`: connection handling, transactions (every queued
/// command is checked on its own) and introspection that changes nothing.
/// By real name; a `None` subcommand allows every subcommand.
pub const READ_ONLY_EXTRAS: &[(&str, Option<&str>)] = &[
    ("ACL", Some("CAT")),
    ("ACL", Some("DRYRUN")),
    ("ACL", Some("GETUSER")),
    ("ACL", Some("HELP")),
    ("ACL", Some("LIST")),
    ("ACL", Some("USERS")),
    ("ACL", Some("WHOAMI")),
    ("CLIENT", Some("GETNAME")),
    ("CLIENT", Some("GETREDIR")),
    ("CLIENT", Some("HELP")),
    ("CLIENT", Some("ID")),
    ("CLIENT", Some("INFO")),
    ("CLIENT", Some("LIST")),
    ("CLIENT", Some("TRACKINGINFO")),
    ("CLUSTER", Some("COUNT-FAILURE-REPORTS")),
    ("CLUSTER", Some("COUNTKEYSINSLOT")),
    ("CLUSTER", Some("GETKEYSINSLOT")),
    ("CLUSTER", Some("HELP")),
    ("CLUSTER", Some("INFO")),
    ("CLUSTER", Some("KEYSLOT")),
    ("CLUSTER", Some("LINKS")),
    ("CLUSTER", Some("MYID")),
    ("CLUSTER", Some("MYSHARDID")),
    ("CLUSTER", Some("NODES")),
    ("CLUSTER", Some("REPLICAS")),
    ("CLUSTER", Some("SHARDS")),
    ("CLUSTER", Some("SLAVES")),
    ("CLUSTER", Some("SLOTS")),
    ("COMMAND", None),
    ("COMMANDLOG", Some("GET")),
    ("COMMANDLOG", Some("HELP")),
    ("COMMANDLOG", Some("LEN")),
    ("CONFIG", Some("GET")),
    ("CONFIG", Some("HELP")),
    ("DISCARD", None),
    ("ECHO", None),
    ("EXEC", None),
    ("FUNCTION", Some("DUMP")),
    ("FUNCTION", Some("HELP")),
    ("FUNCTION", Some("LIST")),
    ("FUNCTION", Some("STATS")),
    ("INFO", None),
    ("LASTSAVE", None),
    ("LATENCY", Some("DOCTOR")),
    ("LATENCY", Some("GRAPH")),
    ("LATENCY", Some("HELP")),
    ("LATENCY", Some("HISTOGRAM")),
    ("LATENCY", Some("HISTORY")),
    ("LATENCY", Some("LATEST")),
    ("MEMORY", Some("DOCTOR")),
    ("MEMORY", Some("HELP")),
    ("MEMORY", Some("MALLOC-STATS")),
    ("MEMORY", Some("STATS")),
    ("MODULE", Some("HELP")),
    ("MODULE", Some("LIST")),
    ("MULTI", None),
    ("OBJECT", Some("HELP")),
    ("PING", None),
    ("PUBSUB", None),
    ("QUIT", None),
    ("ROLE", None),
    ("SCRIPT", Some("EXISTS")),
    ("SCRIPT", Some("HELP")),
    ("SELECT", None),
    ("SLOWLOG", Some("GET")),
    ("SLOWLOG", Some("HELP")),
    ("SLOWLOG", Some("LEN")),
    ("TIME", None),
    ("UNWATCH", None),
    ("WATCH", None),
    ("XINFO", Some("HELP")),
];

/// Module commands a read-only session allows: reads of the search, JSON,
/// time series, probabilistic and vector set modules, plus `FT.CURSOR DEL`
/// (it only drops a cursor a read-only `FT.AGGREGATE ... WITHCURSOR` may
/// have created). The server's `readonly` flag alone is not trusted for
/// module commands, because modules flag state-changing commands such as
/// `SEARCH.CLUSTERSET` or `CF.COMPACT` `readonly`. A module command runs
/// read-only only when it is listed here and the server flags it
/// `readonly` too, outside `@admin` and `@dangerous` (so `FT._LIST` and
/// `FT.TAGVALS` are refused).
pub const READ_ONLY_MODULE_COMMANDS: &[(&str, Option<&str>)] = &[
    ("BF.CARD", None),
    ("BF.EXISTS", None),
    ("BF.INFO", None),
    ("BF.MEXISTS", None),
    ("CF.COUNT", None),
    ("CF.EXISTS", None),
    ("CF.INFO", None),
    ("CF.MEXISTS", None),
    ("CMS.INFO", None),
    ("CMS.QUERY", None),
    ("FT.AGGREGATE", None),
    ("FT.ALIASLIST", None),
    ("FT.CONFIG", Some("GET")),
    ("FT.CONFIG", Some("HELP")),
    ("FT.CURSOR", Some("DEL")),
    ("FT.CURSOR", Some("READ")),
    ("FT.DICTDUMP", None),
    ("FT.EXPLAIN", None),
    ("FT.EXPLAINCLI", None),
    ("FT.GET", None),
    ("FT.HYBRID", None),
    ("FT.INFO", None),
    ("FT.MGET", None),
    ("FT.PROFILE", None),
    ("FT.SEARCH", None),
    ("FT.SPELLCHECK", None),
    ("FT.SUGGET", None),
    ("FT.SUGLEN", None),
    ("FT.SYNDUMP", None),
    ("JSON.ARRINDEX", None),
    ("JSON.ARRLEN", None),
    ("JSON.GET", None),
    ("JSON.MGET", None),
    ("JSON.OBJKEYS", None),
    ("JSON.OBJLEN", None),
    ("JSON.RESP", None),
    ("JSON.STRLEN", None),
    ("JSON.TYPE", None),
    ("TDIGEST.BYRANK", None),
    ("TDIGEST.BYREVRANK", None),
    ("TDIGEST.CDF", None),
    ("TDIGEST.INFO", None),
    ("TDIGEST.MAX", None),
    ("TDIGEST.MIN", None),
    ("TDIGEST.QUANTILE", None),
    ("TDIGEST.RANK", None),
    ("TDIGEST.REVRANK", None),
    ("TDIGEST.TRIMMED_MEAN", None),
    ("TOPK.COUNT", None),
    ("TOPK.INFO", None),
    ("TOPK.LIST", None),
    ("TOPK.QUERY", None),
    ("TS.GET", None),
    ("TS.INFO", None),
    ("TS.MGET", None),
    ("TS.MRANGE", None),
    ("TS.MREVRANGE", None),
    ("TS.NRANGE", None),
    ("TS.NREVRANGE", None),
    ("TS.QUERYINDEX", None),
    ("TS.QUERYLABELS", None),
    ("TS.RANGE", None),
    ("TS.REVRANGE", None),
    ("VCARD", None),
    ("VDIM", None),
    ("VEMB", None),
    ("VGETATTR", None),
    ("VINFO", None),
    ("VISMEMBER", None),
    ("VLINKS", None),
    ("VRANDMEMBER", None),
    ("VRANGE", None),
    ("VSIM", None),
];

/// Commands a read-only session refuses although the server flags them
/// `readonly`: read-only scripts may still run commands that are neither
/// `write` nor `may_replicate`, such as `SLOWLOG RESET` or `MEMORY PURGE`.
/// Their risk is at least [`CommandRisk::Write`].
pub const READ_ONLY_DENY: &[(&str, Option<&str>)] =
    &[("EVALSHA_RO", None), ("EVAL_RO", None), ("FCALL_RO", None)];

/// True when `(name, sub)` (upper case) is in `table`.
fn listed(table: &[(&str, Option<&str>)], name: &str, sub: Option<&str>) -> bool {
    table.iter().any(|(n, s)| {
        *n == name
            && match s {
                None => true,
                Some(s) => sub == Some(*s),
            }
    })
}

/// True when `name` (any case) has subcommands on a supported server.
fn is_container(name: &str) -> bool {
    risk_table::CONTAINERS
        .binary_search(&name.to_ascii_lowercase().as_str())
        .is_ok()
}

/// True when the generated `table` (lower-case `name` or `name|sub`) has
/// the command; for a command with subcommands a given subcommand is
/// looked up as `name|sub`.
fn generated(table: &[&str], name: &str, sub: Option<&str>) -> bool {
    let name = name.to_ascii_lowercase();
    let found = |key: &str| table.binary_search(&key).is_ok();
    match sub {
        Some(sub) if is_container(&name) => found(&format!("{name}|{}", sub.to_ascii_lowercase())),
        _ => found(&name),
    }
}

/// Flags and ACL categories of one command, from `COMMAND INFO`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandSpec {
    /// The real name, lower case (`config`, or `config|set` for a
    /// subcommand): what the server runs, whatever name was typed.
    pub name: String,
    /// Lower-case flags such as `write`, `readonly`, `admin`.
    pub flags: HashSet<String>,
    /// Lower-case ACL categories such as `@write`, `@dangerous`.
    pub categories: HashSet<String>,
    /// Subcommands by upper-case name (Redis 7 and newer).
    pub subcommands: HashMap<String, CommandSpec>,
}

impl CommandSpec {
    /// Parses one entry of a `COMMAND INFO` reply. `None` for a nil entry
    /// (unknown command).
    pub fn parse(entry: &Resp) -> Option<CommandSpec> {
        let Resp::Array(fields) = entry else {
            return None;
        };
        let words = |index: usize| -> HashSet<String> {
            match fields.get(index) {
                Some(Resp::Array(items) | Resp::Set(items)) => items
                    .iter()
                    .filter_map(as_text)
                    .map(|w| w.to_ascii_lowercase())
                    .collect(),
                _ => HashSet::new(),
            }
        };
        let mut subcommands = HashMap::new();
        if let Some(Resp::Array(subs)) = fields.get(9) {
            for sub in subs {
                let Resp::Array(sub_fields) = sub else {
                    continue;
                };
                let Some(full) = sub_fields.first().and_then(as_text) else {
                    continue;
                };
                let short = full
                    .split_once('|')
                    .map_or(full.as_str(), |(_, s)| s)
                    .to_ascii_uppercase();
                if let Some(spec) = CommandSpec::parse(sub) {
                    subcommands.insert(short, spec);
                }
            }
        }
        Some(CommandSpec {
            name: fields
                .first()
                .and_then(as_text)
                .unwrap_or_default()
                .to_ascii_lowercase(),
            flags: words(2),
            categories: words(6),
            subcommands,
        })
    }

    /// The real command name, upper case (`CONFIG` for `config|set`).
    #[must_use]
    pub fn command_name(&self) -> String {
        self.name
            .split('|')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase()
    }

    fn read_only_flagged(&self) -> bool {
        self.flags.contains("readonly")
            && !self.flags.contains("write")
            && !self.flags.contains("may_replicate")
    }
}

/// Result of classifying one command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assessment {
    /// Risk for the confirmation dialog.
    pub risk: CommandRisk,
    /// True when a read-only session must refuse the command: it is not
    /// known to be read-only.
    pub writes: bool,
}

/// Classifies a command from the typed name, its first argument and its
/// `COMMAND INFO` entry (`None` when the server does not know the typed
/// name or `COMMAND INFO` is not available).
#[must_use]
pub fn assess(name: &str, sub: Option<&str>, spec: Option<&CommandSpec>) -> Assessment {
    let name = spec.map_or_else(|| name.to_ascii_uppercase(), CommandSpec::command_name);
    let sub = sub.map(str::to_ascii_uppercase);
    let sub = sub.as_deref();
    // Redis 6.2 lists commands with subcommands as one coarse entry.
    let coarse =
        sub.is_some() && spec.is_some_and(|s| s.subcommands.is_empty()) && is_container(&name);
    // The entry describing exactly this command or subcommand.
    let exact = match (spec, sub) {
        (Some(spec), Some(sub)) if !spec.subcommands.is_empty() => spec.subcommands.get(sub),
        (Some(_), _) if coarse => None,
        (Some(spec), _) => Some(spec),
        (None, _) => None,
    };
    // Whether the server's `readonly` flag can be trusted, and whether the
    // command comes from a module.
    let (trusted, module) = match exact {
        Some(entry) => (
            entry.read_only_flagged()
                && !entry.categories.contains("@admin")
                && !entry.categories.contains("@dangerous"),
            entry.flags.contains("module"),
        ),
        // Unknown subcommand of a server that lists subcommands: refused.
        None if spec.is_some() && !coarse => (false, false),
        // Coarse 6.2 entry, or no `COMMAND INFO`: the generated tables,
        // which hold only core, non-admin, non-dangerous read commands.
        None => (generated(risk_table::READONLY_COMMANDS, &name, sub), false),
    };
    let allowed = !listed(READ_ONLY_DENY, &name, sub)
        && (listed(READ_ONLY_EXTRAS, &name, sub)
            || (trusted && (!module || listed(READ_ONLY_MODULE_COMMANDS, &name, sub))));
    let writes = !allowed;
    let dangerous = exact.is_some_and(|s| s.categories.contains("@dangerous"))
        // A coarse Redis 6.2 entry has the categories of all its
        // subcommands together: the generated subcommand tables decide.
        || spec.is_some_and(|s| !coarse && s.categories.contains("@dangerous"))
        || generated(risk_table::DANGEROUS_COMMANDS, &name, sub);
    let risk = if listed(ALWAYS_DESTRUCTIVE, &name, sub) || (writes && dangerous) {
        CommandRisk::Destructive
    } else if writes {
        CommandRisk::Write
    } else {
        CommandRisk::Read
    };
    Assessment { risk, writes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, flags: &[&str], categories: &[&str]) -> CommandSpec {
        CommandSpec {
            name: name.into(),
            flags: flags.iter().map(|s| (*s).to_string()).collect(),
            categories: categories.iter().map(|s| (*s).to_string()).collect(),
            subcommands: HashMap::new(),
        }
    }

    fn with_subs(mut container: CommandSpec, subs: &[CommandSpec]) -> CommandSpec {
        for sub in subs {
            let short = sub
                .name
                .split('|')
                .nth(1)
                .unwrap_or_default()
                .to_ascii_uppercase();
            container.subcommands.insert(short, sub.clone());
        }
        container
    }

    fn status(s: &str) -> Resp {
        Resp::SimpleString(s.into())
    }

    fn risk(name: &str, sub: Option<&str>, spec: Option<&CommandSpec>) -> (CommandRisk, bool) {
        let a = assess(name, sub, spec);
        (a.risk, a.writes)
    }

    #[test]
    fn parses_command_info_entries() {
        let entry = Resp::Array(vec![
            Resp::BulkString(b"set".to_vec()),
            Resp::Int(-3),
            Resp::Array(vec![status("write"), status("denyoom")]),
            Resp::Int(1),
            Resp::Int(1),
            Resp::Int(1),
            Resp::Array(vec![status("@write"), status("@string"), status("@slow")]),
        ]);
        let parsed = CommandSpec::parse(&entry);
        assert_eq!(parsed.as_ref().map(|p| p.name.as_str()), Some("set"));
        assert_eq!(
            parsed.as_ref().map(|p| p.flags.contains("write")),
            Some(true)
        );
        assert_eq!(parsed.map(|p| p.categories.contains("@string")), Some(true));
        assert_eq!(CommandSpec::parse(&Resp::Nil), None);
    }

    #[test]
    fn parses_subcommands() {
        let sub = Resp::Array(vec![
            Resp::BulkString(b"config|set".to_vec()),
            Resp::Int(-4),
            Resp::Array(vec![status("admin"), status("noscript")]),
            Resp::Int(0),
            Resp::Int(0),
            Resp::Int(0),
            Resp::Array(vec![status("@admin"), status("@dangerous")]),
        ]);
        let entry = Resp::Array(vec![
            Resp::BulkString(b"config".to_vec()),
            Resp::Int(-2),
            Resp::Array(vec![]),
            Resp::Int(0),
            Resp::Int(0),
            Resp::Int(0),
            Resp::Array(vec![status("@slow")]),
            Resp::Array(vec![]),
            Resp::Array(vec![]),
            Resp::Array(vec![sub]),
        ]);
        let parsed = CommandSpec::parse(&entry);
        let set = parsed.as_ref().and_then(|p| p.subcommands.get("SET"));
        assert_eq!(set.map(|s| s.name.as_str()), Some("config|set"));
        assert_eq!(
            risk("config", Some("set"), parsed.as_ref()),
            (CommandRisk::Destructive, true)
        );
        // Not listed by the server: refused.
        assert_eq!(
            risk("CONFIG", Some("NOSUCH"), parsed.as_ref()),
            (CommandRisk::Write, true)
        );
    }

    #[test]
    fn read_only_is_an_allowlist() {
        let get = spec("get", &["readonly", "fast"], &["@read", "@string"]);
        let set = spec("set", &["write", "denyoom"], &["@write", "@string"]);
        let pfcount62 = spec("pfcount", &["readonly", "may_replicate"], &["@read"]);
        let ping = spec("ping", &["fast", "stale"], &["@fast", "@connection"]);
        let info = spec("info", &["loading", "stale"], &["@slow", "@dangerous"]);
        let module_write = spec("myjson.set", &["write", "module"], &["@write"]);
        let module_admin = spec("mymod.reset", &["admin", "module"], &[]);

        assert_eq!(risk("GET", None, Some(&get)), (CommandRisk::Read, false));
        assert_eq!(risk("SET", None, Some(&set)), (CommandRisk::Write, true));
        assert_eq!(
            risk("PFCOUNT", None, Some(&pfcount62)),
            (CommandRisk::Write, true)
        );
        assert_eq!(risk("PING", None, Some(&ping)), (CommandRisk::Read, false));
        assert_eq!(
            risk("INFO", Some("all"), Some(&info)),
            (CommandRisk::Read, false)
        );
        assert!(risk("MYJSON.SET", None, Some(&module_write)).1);
        assert!(risk("MYMOD.RESET", None, Some(&module_admin)).1);
        // Unknown to the server and to the tables: refused.
        assert_eq!(risk("NOSUCH", None, None), (CommandRisk::Write, true));
        // Without COMMAND INFO the generated tables decide.
        assert_eq!(risk("GET", None, None), (CommandRisk::Read, false));
        assert_eq!(risk("SET", None, None), (CommandRisk::Write, true));
        assert_eq!(risk("SORT", None, None), (CommandRisk::Destructive, true));
    }

    #[test]
    fn scripts_flushes_and_keys() {
        let flush = spec(
            "flushdb",
            &["write"],
            &["@keyspace", "@write", "@dangerous"],
        );
        let keys = spec("keys", &["readonly"], &["@keyspace", "@read", "@dangerous"]);
        let debug = spec("debug", &["admin", "noscript"], &["@admin", "@dangerous"]);
        let eval = spec("eval", &["noscript", "script_runner"], &["@scripting"]);
        let evalsha = spec("evalsha", &["noscript", "may_replicate"], &["@scripting"]);
        let fcall = spec("fcall", &["noscript", "script_runner"], &["@scripting"]);
        let eval_ro = spec("eval_ro", &["readonly", "noscript"], &["@scripting"]);
        assert_eq!(
            risk("flushdb", None, Some(&flush)),
            (CommandRisk::Destructive, true)
        );
        // KEYS reads but is @dangerous (it can stall the server): refused.
        assert_eq!(
            risk("KEYS", Some("*"), Some(&keys)),
            (CommandRisk::Destructive, true)
        );
        assert_eq!(
            risk("DEBUG", Some("SLEEP"), Some(&debug)),
            (CommandRisk::Destructive, true)
        );
        for (name, s) in [("EVAL", &eval), ("evalsha", &evalsha), ("FCALL", &fcall)] {
            assert_eq!(
                risk(name, None, Some(s)),
                (CommandRisk::Destructive, true),
                "{name}"
            );
        }
        assert_eq!(risk("FCALL", None, None), (CommandRisk::Destructive, true));
        // Read-only scripts can run non-write admin commands: refused.
        assert_eq!(
            risk("EVAL_RO", None, Some(&eval_ro)),
            (CommandRisk::Write, true)
        );
        let fcall_ro = spec("fcall_ro", &["readonly", "noscript"], &["@scripting"]);
        assert!(risk("FCALL_RO", None, Some(&fcall_ro)).1);
        assert!(risk("EVALSHA_RO", None, None).1);
    }

    #[test]
    fn renamed_commands_are_classified_by_their_real_name() {
        // Redis 7 and newer: the entry of the renamed command has its real
        // name and its subcommands.
        let config = with_subs(
            spec("config", &[], &["@slow"]),
            &[
                spec(
                    "config|set",
                    &["admin", "noscript"],
                    &["@admin", "@dangerous"],
                ),
                spec(
                    "config|get",
                    &["admin", "noscript"],
                    &["@admin", "@dangerous"],
                ),
            ],
        );
        assert_eq!(
            risk("MYCONF", Some("SET"), Some(&config)),
            (CommandRisk::Destructive, true)
        );
        assert_eq!(
            risk("myconf", Some("get"), Some(&config)),
            (CommandRisk::Read, false)
        );
        let script = with_subs(
            spec("script", &[], &["@slow"]),
            &[spec("script|flush", &["noscript"], &["@scripting"])],
        );
        assert_eq!(
            risk("MYSCRIPT", Some("FLUSH"), Some(&script)),
            (CommandRisk::Destructive, true)
        );
        let shutdown = spec(
            "shutdown",
            &["admin", "noscript"],
            &["@admin", "@dangerous"],
        );
        assert_eq!(
            risk("MYSHUT", None, Some(&shutdown)),
            (CommandRisk::Destructive, true)
        );
        // Redis 6.2: one coarse entry with the real name.
        let config62 = spec("config", &["admin", "noscript"], &["@admin", "@dangerous"]);
        assert_eq!(
            risk("MYCONF", Some("SET"), Some(&config62)),
            (CommandRisk::Destructive, true)
        );
        assert_eq!(
            risk("MYCONF", Some("REWRITE"), Some(&config62)).0,
            CommandRisk::Destructive
        );
        assert_eq!(
            risk("MYCONF", Some("GET"), Some(&config62)),
            (CommandRisk::Read, false)
        );
        // Without COMMAND INFO a renamed command is unknown: refused.
        assert!(risk("MYCONF", Some("GET"), None).1);
    }

    #[test]
    fn admin_commands_without_write_flags_are_refused() {
        let admin = |name: &str| {
            spec(
                name,
                &["admin", "noscript"],
                &["@admin", "@slow", "@dangerous"],
            )
        };
        let plain = |name: &str| spec(name, &["noscript", "loading", "stale"], &["@slow"]);
        let client = with_subs(
            spec("client", &[], &["@slow"]),
            &[
                plain("client|unblock"),
                plain("client|no-evict"),
                admin("client|list"),
                plain("client|id"),
            ],
        );
        assert!(risk("CLIENT", Some("UNBLOCK"), Some(&client)).1);
        assert!(risk("CLIENT", Some("NO-EVICT"), Some(&client)).1);
        assert_eq!(
            risk("CLIENT", Some("LIST"), Some(&client)),
            (CommandRisk::Read, false)
        );
        assert_eq!(
            risk("CLIENT", Some("ID"), Some(&client)),
            (CommandRisk::Read, false)
        );
        for (container, sub) in [
            ("acl", "log"),
            ("memory", "purge"),
            ("script", "load"),
            ("commandlog", "reset"),
            ("cluster", "migrateslots"),
            ("cluster", "migration"),
            ("hotkeys", "start"),
            ("backup", "start"),
        ] {
            let entry = with_subs(
                spec(container, &[], &[]),
                &[plain(&format!("{container}|{sub}"))],
            );
            assert!(
                risk(container, Some(sub), Some(&entry)).1,
                "{container} {sub}"
            );
            // Also when the server does not know it at all.
            assert!(
                risk(container, Some(sub), None).1,
                "{container} {sub} unknown"
            );
        }
        // Redis 6.2 lists MEMORY as one read-only entry: the subcommand
        // flags of the newer servers decide.
        let memory62 = spec("memory", &["readonly", "random"], &["@read", "@slow"]);
        assert_eq!(
            risk("MEMORY", Some("USAGE"), Some(&memory62)),
            (CommandRisk::Read, false)
        );
        assert!(risk("MEMORY", Some("PURGE"), Some(&memory62)).1);
        let script62 = spec(
            "script",
            &["noscript", "may_replicate"],
            &["@slow", "@scripting"],
        );
        assert_eq!(
            risk("SCRIPT", Some("EXISTS"), Some(&script62)),
            (CommandRisk::Read, false)
        );
        assert!(risk("SCRIPT", Some("LOAD"), Some(&script62)).1);
        assert_eq!(
            risk("SCRIPT", Some("FLUSH"), Some(&script62)).0,
            CommandRisk::Destructive
        );
    }

    #[test]
    fn module_commands_need_vetting_and_admin_or_dangerous_overrides_readonly() {
        let module =
            |name: &str, categories: &[&str]| spec(name, &["readonly", "module"], categories);
        // Vetted module reads run when the server flags them readonly.
        assert_eq!(
            risk(
                "JSON.GET",
                None,
                Some(&module("json.get", &["@json", "@read"]))
            ),
            (CommandRisk::Read, false)
        );
        assert_eq!(
            risk("FT.SEARCH", None, Some(&module("FT.SEARCH", &["@search"]))),
            (CommandRisk::Read, false)
        );
        // A vetted name with a write flag is still refused.
        let json_write = spec("json.get", &["write", "module"], &[]);
        assert!(risk("JSON.GET", None, Some(&json_write)).1);
        // Module commands flagged readonly that change state: refused.
        for name in [
            "search.clusterset",
            "search.clusterrefresh",
            "timeseries.refreshcluster",
            "timeseries.clusterset",
            "cf.compact",
        ] {
            assert_eq!(
                risk(name, None, Some(&module(name, &[]))),
                (CommandRisk::Write, true),
                "{name}"
            );
        }
        let cursor = with_subs(
            spec("ft.cursor", &["readonly", "module"], &[]),
            &[
                module("ft.cursor|del", &[]),
                module("ft.cursor|gc", &[]),
                module("ft.cursor|read", &[]),
            ],
        );
        assert_eq!(
            risk("FT.CURSOR", Some("DEL"), Some(&cursor)),
            (CommandRisk::Read, false)
        );
        assert!(risk("FT.CURSOR", Some("GC"), Some(&cursor)).1);
        assert_eq!(
            risk("FT.CURSOR", Some("READ"), Some(&cursor)),
            (CommandRisk::Read, false)
        );
        // @admin or @dangerous overrides the readonly flag.
        let debug = with_subs(
            spec(
                "_ft.debug",
                &["readonly", "module"],
                &["@admin", "@dangerous"],
            ),
            &[module(
                "_ft.debug|gc_forceinvoke",
                &["@admin", "@dangerous"],
            )],
        );
        assert_eq!(
            risk("_FT.DEBUG", Some("GC_FORCEINVOKE"), Some(&debug)),
            (CommandRisk::Destructive, true)
        );
        let core_admin = spec("lolwut", &["readonly"], &["@admin"]);
        assert_eq!(
            risk("LOLWUT", None, Some(&core_admin)),
            (CommandRisk::Write, true)
        );
        let sort_ro = spec("sort_ro", &["readonly"], &["@read", "@dangerous"]);
        assert_eq!(
            risk("SORT_RO", None, Some(&sort_ro)),
            (CommandRisk::Destructive, true)
        );
        // Without COMMAND INFO module commands are not in the core tables.
        assert!(risk("JSON.GET", None, None).1);
    }

    #[test]
    fn extras_are_sorted_and_unique() {
        assert!(READ_ONLY_EXTRAS.windows(2).all(|w| w[0] < w[1]));
        assert!(READ_ONLY_MODULE_COMMANDS.windows(2).all(|w| w[0] < w[1]));
        assert!(READ_ONLY_DENY.windows(2).all(|w| w[0] < w[1]));
        assert!(ALWAYS_DESTRUCTIVE.windows(2).all(|w| w[0] < w[1]));
        // Nothing is both allowed read-only and always destructive.
        for (name, sub) in ALWAYS_DESTRUCTIVE {
            assert!(!listed(READ_ONLY_EXTRAS, name, *sub), "{name} {sub:?}");
        }
    }

    #[test]
    fn generated_tables_are_sorted_lower_case_and_complete() {
        for table in [
            risk_table::READONLY_COMMANDS,
            risk_table::DANGEROUS_COMMANDS,
            risk_table::CONTAINERS,
        ] {
            assert!(!table.is_empty());
            assert!(
                table.windows(2).all(|w| w[0] < w[1]),
                "not sorted or unique"
            );
            assert!(table.iter().all(|c| *c == c.to_ascii_lowercase()));
        }
        for read in ["get", "scan", "memory|usage", "object|freq", "xinfo|stream"] {
            assert!(risk_table::READONLY_COMMANDS.contains(&read), "{read}");
        }
        for not in [
            "set",
            "memory|purge",
            "client|list",
            "script|load",
            "pfcount",
            "keys",
            "sort_ro",
            "json.get",
            "search.clusterset",
            "cf.compact",
        ] {
            assert!(!risk_table::READONLY_COMMANDS.contains(&not), "{not}");
        }
        for dangerous in ["flushall", "keys", "config|set", "debug"] {
            assert!(
                risk_table::DANGEROUS_COMMANDS.contains(&dangerous),
                "{dangerous}"
            );
        }
    }
}

/// Generates [`crate::risk_table`] from the servers and checks the committed
/// file against them (ignored: needs the dev database matrix).
#[cfg(test)]
mod generated_tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use varve_core::Engine;
    use varve_driver::{ConnectParams, Session, TlsMode};
    use varve_secrets::Secret;

    use super::*;
    use crate::session::RedisSession;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[derive(Default)]
    struct Tables {
        /// Read-only on some server.
        readonly: BTreeSet<String>,
        /// Not read-only on some server.
        not_readonly: BTreeSet<String>,
        dangerous: BTreeSet<String>,
        containers: BTreeSet<String>,
    }

    impl Tables {
        fn read_only(&self) -> BTreeSet<String> {
            self.readonly
                .difference(&self.not_readonly)
                .cloned()
                .collect()
        }
    }

    fn words(entry: &[Resp], index: usize) -> BTreeSet<String> {
        match entry.get(index) {
            Some(Resp::Array(items) | Resp::Set(items)) => items
                .iter()
                .filter_map(as_text)
                .map(|w| w.to_ascii_lowercase())
                .collect(),
            _ => BTreeSet::new(),
        }
    }

    fn collect(entry: &Resp, tables: &mut Tables) {
        let Resp::Array(fields) = entry else {
            return;
        };
        let Some(name) = fields.first().and_then(as_text) else {
            return;
        };
        let name = name.to_ascii_lowercase();
        let flags = words(fields, 2);
        let categories = words(fields, 6);
        if flags.contains("readonly")
            && !flags.contains("write")
            && !flags.contains("may_replicate")
            && !flags.contains("module")
            && !categories.contains("@admin")
            && !categories.contains("@dangerous")
        {
            tables.readonly.insert(name.clone());
        } else {
            tables.not_readonly.insert(name.clone());
        }
        if categories.contains("@dangerous") {
            tables.dangerous.insert(name.clone());
        }
        if let Some(Resp::Array(subs)) = fields.get(9)
            && !subs.is_empty()
        {
            tables.containers.insert(name);
            for sub in subs {
                collect(sub, tables);
            }
        }
    }

    fn servers() -> Vec<(String, u16)> {
        let endpoint =
            |var: &str, default: &str| std::env::var(var).unwrap_or_else(|_| default.to_string());
        [
            endpoint("VARVE_REDIS_62", "127.0.0.1:63062"),
            endpoint("VARVE_REDIS_810", "127.0.0.1:63081"),
            endpoint("VARVE_VALKEY_9", "127.0.0.1:63190"),
        ]
        .into_iter()
        .filter_map(|e| {
            let (host, port) = e.rsplit_once(':')?;
            Some((host.to_string(), port.parse().ok()?))
        })
        .collect()
    }

    fn render(versions: &[String], tables: &Tables) -> String {
        let list = |doc: &str, name: &str, items: &BTreeSet<String>| {
            let mut out = format!("/// {doc}\npub(crate) const {name}: &[&str] = &[\n");
            for item in items {
                out.push_str(&format!("    {item:?},\n"));
            }
            out.push_str("];\n");
            out
        };
        format!(
            "//! Command risk tables from the `COMMAND` replies of the supported\n\
             //! servers.\n\
             //!\n\
             //! Generated, do not edit by hand. Regenerate with\n\
             //! `VARVE_WRITE_RISK_TABLES=1 cargo test -p varve-driver-redis --lib --\n\
             //! --include-ignored risk_tables_match_the_servers`; without the\n\
             //! variable the test fails when the servers disagree with this file.\n\
             //! Sources: {}.\n\
             //!\n\
             //! Names are lower case, `name` or `name|subcommand`, sorted. A command\n\
             //! is read-only only when every server that has it says so; it is\n\
             //! dangerous or has subcommands when one of them says so.\n\n\
             {}\n{}\n{}",
            versions.join(", "),
            list(
                "Core commands with the `readonly` flag, neither `write` nor `may_replicate`,\n/// and not in `@admin` or `@dangerous`, on every server that has them.",
                "READONLY_COMMANDS",
                &tables.read_only()
            ),
            list(
                "Commands in the `@dangerous` ACL category.",
                "DANGEROUS_COMMANDS",
                &tables.dangerous
            ),
            list(
                "Commands with subcommands (Redis 7 and newer list them separately).",
                "CONTAINERS",
                &tables.containers
            ),
        )
    }

    #[tokio::test]
    #[ignore = "needs the dev database matrix"]
    async fn risk_tables_match_the_servers() -> TestResult {
        let mut tables = Tables::default();
        let mut versions = Vec::new();
        for (host, port) in servers() {
            let mut p = ConnectParams::new(Engine::Redis, host, port);
            p.password = Some(Secret::new(
                std::env::var("VARVE_REDIS_PASSWORD").unwrap_or_else(|_| "varve_dev".into()),
            ));
            p.tls.mode = TlsMode::Disable;
            p.connect_timeout = Duration::from_secs(5);
            let session = RedisSession::connect(&p).await?;
            let version = session.server_info().version.clone();
            versions.push(if version.ends_with("-valkey") {
                format!("Valkey {}", version.trim_end_matches("-valkey"))
            } else {
                format!("Redis {version}")
            });
            let mut call = session.begin().await?;
            let Resp::Array(entries) = call.query(&redis::cmd("COMMAND")).await? else {
                return Err("COMMAND did not return an array".into());
            };
            drop(call);
            for entry in &entries {
                collect(entry, &mut tables);
            }
        }
        let committed = |table: &[&str]| -> BTreeSet<String> {
            table.iter().map(|s| (*s).to_string()).collect()
        };
        let same = committed(risk_table::READONLY_COMMANDS) == tables.read_only()
            && committed(risk_table::DANGEROUS_COMMANDS) == tables.dangerous
            && committed(risk_table::CONTAINERS) == tables.containers;
        if same {
            return Ok(());
        }
        let text = render(&versions, &tables);
        if std::env::var("VARVE_WRITE_RISK_TABLES").is_ok() {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/risk_table.rs");
            std::fs::write(path, text)?;
            return Ok(());
        }
        Err(format!(
            "src/risk_table.rs differs from the servers ({}); regenerate it:\n{text}",
            versions.join(", ")
        )
        .into())
    }
}
