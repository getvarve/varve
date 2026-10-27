//! Redis command redactor and normalizer.
//!
//! Input is what the console runs. Lines and arguments are read with
//! `varve_redis_syntax`, the splitter the Redis driver uses, so every
//! argument the driver sends is an argument the redactor sees (`\0` and
//! other `redis-cli` separators, quotes that start mid-argument, a quote
//! that is never closed). Comment lines are not run, but their text is
//! redacted like a command too; the normalizer drops them.

use std::borrow::Cow;

use varve_redis_syntax::{Arg, lines, split_args_lenient};

use super::REDACTED;
use super::credentials::{
    Ranges, UrlScope, cli_ranges, comment_secret_ranges, credential_ranges, is_secret_key,
    key_value_ranges, metadata_key, plain_number, splice, url_ranges,
};
use super::fuzzy::{like_password, within};

/// One line of a script, split into arguments with byte offsets into the
/// whole script.
struct Command {
    args: Vec<Arg>,
    /// A `#` comment line: `redis-cli` skips it. The arguments are those of
    /// the text after the `#`.
    comment: bool,
}

/// Splits `src` into commands, one per non-blank line.
fn commands(src: &str) -> Vec<Command> {
    lines(src)
        .filter_map(|line| {
            let body = line.comment_body_offset().unwrap_or(0);
            let args: Vec<Arg> = split_args_lenient(&line.text[body..])
                .into_iter()
                .map(|arg| arg.shifted(line.offset + body))
                .collect();
            (!args.is_empty()).then_some(Command {
                args,
                comment: line.comment,
            })
        })
        .collect()
}

fn text(arg: &Arg) -> Cow<'_, str> {
    arg.text()
}

fn upper(arg: Option<&Arg>) -> String {
    arg.map(|a| text(a).to_ascii_uppercase())
        .unwrap_or_default()
}

fn is(arg: &Arg, keyword: &str) -> bool {
    arg.value.eq_ignore_ascii_case(keyword.as_bytes())
}

/// A configuration parameter whose value is a secret (`requirepass`,
/// `masterauth`, `tls-key-file-pass`, `auth-pass`, ...).
fn is_secret_parameter(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("pass") || name.contains("auth") || name.contains("secret")
}

fn redact_arg(out: &mut Ranges, arg: &Arg) {
    out.push((arg.content_start, arg.content_end));
}

fn redact_all(out: &mut Ranges, args: &[Arg]) {
    args.iter().for_each(|arg| redact_arg(out, arg));
}

/// Redacts `CONFIG SET`-style `name value` pairs starting at `from`.
fn redact_pairs(out: &mut Ranges, args: &[Arg], from: usize) {
    let mut k = from;
    while k + 1 < args.len() {
        if is_secret_parameter(&text(&args[k])) {
            redact_arg(out, &args[k + 1]);
        }
        k += 2;
    }
}

/// True for an argument after which a secret follows: `AUTH`, `AUTH2`, a
/// secret key (`requirepass`, `masterauth`, `client_secret`, ...) or a word
/// that looks like `password`.
fn introduces_secret(arg: &Arg) -> bool {
    let value = text(arg);
    is(arg, "AUTH") || is(arg, "AUTH2") || is_secret_key(&value) || like_password(&value)
}

/// Any command: the argument after a secret-introducing argument is
/// redacted (two after `AUTH2`, which takes a user name and a password).
fn redact_after_introducers(out: &mut Ranges, args: &[Arg]) {
    for (k, arg) in args.iter().enumerate() {
        if !introduces_secret(arg) {
            continue;
        }
        // `SET user:1:password_reset_at 1700000000`: a time or count about
        // a password, not the password.
        let name = text(arg);
        if metadata_key(&name)
            && args
                .get(k + 1)
                .is_some_and(|next| plain_number(&text(next)))
        {
            continue;
        }
        let count = if is(arg, "AUTH2") { 2 } else { 1 };
        redact_all(
            out,
            args.get(k + 1..k + 1 + count).unwrap_or(&args[k + 1..]),
        );
    }
}

/// Rules of `ACL SETUSER` that never hold a secret.
const ACL_RULE_WORDS: &[&str] = &[
    "on",
    "off",
    "nopass",
    "resetpass",
    "reset",
    "allkeys",
    "allcommands",
    "allchannels",
    "resetkeys",
    "resetchannels",
    "nocommands",
    "sanitize-payload",
    "skip-sanitize-payload",
    "clearselectors",
];

/// True for an `ACL SETUSER` rule that holds no secret: a known word, a
/// key pattern (`~`, `%R~`, `%W~`, `%RW~`), a channel pattern (`&`), a
/// command or category rule (`+`, `-`), or a selector of such rules.
fn acl_rule_known(rule: &str) -> bool {
    let lower = rule.to_ascii_lowercase();
    if let Some(inner) = lower.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        return inner.split_whitespace().all(acl_rule_known);
    }
    ACL_RULE_WORDS.contains(&lower.as_str())
        || ["~", "&", "+", "-", "%r~", "%w~", "%rw~"]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
}

/// Redacts every `>pw`, `<pw`, `#hash` and `!hash` rule in `args`, also
/// inside a selector (`(>pw ~k*)`).
fn redact_password_rules(src: &str, args: &[Arg], out: &mut Ranges) {
    for rule in args {
        let raw = &src[rule.content_start..rule.content_end];
        let body = raw.trim_start_matches('(');
        if body.starts_with(['>', '<', '#', '!']) {
            redact_marked(src, rule, out);
        } else if raw.starts_with('(') && raw.contains(['>', '<', '#', '!']) {
            redact_arg(out, rule);
        }
    }
}

/// Redacts a rule that starts with a password marker, keeping the marker
/// when it is written as is.
fn redact_marked(src: &str, rule: &Arg, out: &mut Ranges) {
    let Some(&first) = rule.value.first() else {
        return;
    };
    let start = if src.as_bytes().get(rule.content_start) == Some(&first) {
        rule.content_start + 1
    } else {
        rule.content_start
    };
    out.push((start, rule.content_end));
}

/// `ACL SETUSER <user> <rule> ...`: `>pw`, `<pw`, `#hash` and `!hash` are
/// always redacted; any other rule that is not a known rule (a password
/// typed without `>`) is redacted whole.
fn redact_acl_setuser(src: &str, args: &[Arg], out: &mut Ranges) {
    for (k, rule) in args.iter().enumerate() {
        match rule.value.first() {
            Some(b'>' | b'<' | b'#' | b'!') => {
                // Keep the rule's marker when it is written as is, so the
                // log still shows which kind of rule it was.
                redact_marked(src, rule, out);
            }
            // The user name.
            _ if k == 0 => {}
            _ if acl_rule_known(&text(rule)) => {}
            _ => redact_arg(out, rule),
        }
    }
}

/// `HELLO [protover [AUTH username password] [SETNAME clientname]]`.
fn redact_hello(args: &[Arg], out: &mut Ranges) {
    let mut k = usize::from(args.first().is_some_and(|a| text(a).parse::<u32>().is_ok()));
    while k < args.len() {
        if is(&args[k], "AUTH") {
            match (args.get(k + 1), args.get(k + 2)) {
                // `AUTH password SETNAME ...`: the user name was left out.
                (Some(only), Some(next)) if is(next, "SETNAME") => {
                    redact_arg(out, only);
                    k += 2;
                    continue;
                }
                (Some(_), Some(password)) => redact_arg(out, password),
                (Some(only), None) => redact_arg(out, only),
                _ => {}
            }
            k += 3;
        } else if is(&args[k], "SETNAME") {
            k += 2;
        } else {
            k += 1;
        }
    }
}

/// `MIGRATE ... [AUTH password | AUTH2 username password] [KEYS ...]`,
/// with the options found wherever they are, also after `KEYS`.
fn redact_migrate(args: &[Arg], out: &mut Ranges) {
    let mut k = 0;
    while k < args.len() {
        if is(&args[k], "AUTH") {
            if let Some(arg) = args.get(k + 1) {
                redact_arg(out, arg);
            }
            k += 2;
        } else if is(&args[k], "AUTH2") {
            // AUTH2 username password: only the password is secret.
            if let Some(arg) = args.get(k + 2).or_else(|| args.get(k + 1)) {
                redact_arg(out, arg);
            }
            k += 3;
        } else {
            k += 1;
        }
    }
}

/// `MODULE LOAD path [arg ...]` and `MODULE LOADEX path [CONFIG name value
/// ...] [ARGS arg ...]`: module arguments are redacted, and so are config
/// values with secret names. Any other subcommand except `LIST`, `UNLOAD`
/// and `HELP` (a typo) has all its arguments redacted.
fn redact_module(args: &[Arg], out: &mut Ranges) {
    let sub = upper(args.first());
    match sub.as_str() {
        "LIST" | "UNLOAD" | "HELP" => {}
        "LOAD" => redact_all(out, args.get(2..).unwrap_or_default()),
        "LOADEX" => {
            let mut k = 2;
            while k < args.len() {
                if is(&args[k], "ARGS") {
                    redact_all(out, &args[k + 1..]);
                    break;
                } else if is(&args[k], "CONFIG") {
                    if let (Some(name), Some(value)) = (args.get(k + 1), args.get(k + 2))
                        && is_secret_parameter(&text(name))
                    {
                        redact_arg(out, value);
                    }
                    k += 3;
                } else {
                    k += 1;
                }
            }
        }
        _ => redact_all(out, args.get(1..).unwrap_or_default()),
    }
}

/// True when a script mentions authentication, a password, ACLs or the
/// configuration.
fn script_mentions_secret(script: &str) -> bool {
    let lower = script.to_ascii_lowercase();
    ["auth", "pass", "secret", "acl", "config"]
        .iter()
        .any(|word| lower.contains(word))
}

/// Ranges of the Lua string literals in `raw` (the raw text of a script
/// argument, possibly still escaped for the console): `'...'`, `"..."`,
/// their escaped forms `\'...\'` and `\"...\"`, and long strings
/// `[[...]]` / `[==[...]==]`.
fn lua_string_ranges(raw: &str, base: usize) -> Ranges {
    let bytes = raw.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'[' {
            let mut level = 0;
            while bytes.get(i + 1 + level) == Some(&b'=') {
                level += 1;
            }
            if bytes.get(i + 1 + level) == Some(&b'[') {
                let start = i + 2 + level;
                let close = format!("]{}]", "=".repeat(level));
                let end = raw[start..].find(&close).map_or(raw.len(), |p| start + p);
                ranges.push((base + start, base + end));
                i = (end + close.len()).min(raw.len());
                continue;
            }
            i += 1;
        } else if b == b'\\' && matches!(bytes.get(i + 1), Some(b'\'' | b'"')) {
            let quote = bytes[i + 1];
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && !(bytes[j] == b'\\' && bytes.get(j + 1) == Some(&quote)) {
                j += if bytes[j] == b'\\' { 2 } else { 1 };
            }
            let end = j.min(bytes.len());
            ranges.push((base + start, base + end));
            i = (end + 2).min(bytes.len());
        } else if matches!(b, b'\'' | b'"') {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b {
                j += if bytes[j] == b'\\' { 2 } else { 1 };
            }
            let end = j.min(bytes.len());
            ranges.push((base + start, base + end));
            i = (end + 1).min(bytes.len());
        } else {
            i += 1;
        }
    }
    ranges
}

/// `EVAL script numkeys key ... arg ...` (and `EVALSHA`, the `_RO`
/// variants, `FUNCTION LOAD [REPLACE] code`): when the script mentions
/// authentication, every string literal in it and every script argument
/// is redacted (the numkeys count included).
fn redact_script(src: &str, script: &Arg, argv: &[Arg], out: &mut Ranges) {
    let raw = &src[script.content_start..script.content_end];
    if !script_mentions_secret(raw) && !script_mentions_secret(&text(script)) {
        return;
    }
    out.extend(lua_string_ranges(raw, script.content_start));
    redact_all(out, argv);
}

/// Commands an unknown command name may be a misspelling of.
const SECRET_COMMANDS: [&str; 6] = ["AUTH", "HELLO", "ACL", "CONFIG", "MIGRATE", "MODULE"];

fn collect(src: &str, command: &[Arg], out: &mut Ranges) {
    let Some(name) = command.first() else {
        return;
    };
    let args = &command[1..];
    let name = upper(Some(name));
    match name.as_str() {
        "AUTH" | "AUTH2" => {
            if args.len() == 2 {
                redact_arg(out, &args[1]);
            } else {
                redact_all(out, args);
            }
        }
        "HELLO" => redact_hello(args, out),
        "ACL" => {
            let sub = upper(args.first());
            let known = [
                "CAT", "DELUSER", "DRYRUN", "GENPASS", "GETUSER", "HELP", "LIST", "LOAD", "LOG",
                "SAVE", "USERS", "WHOAMI",
            ];
            let split_setuser = sub == "SET" && upper(args.get(1)) == "USER";
            if split_setuser {
                // `ACL SET USER name ...`
                redact_acl_setuser(src, args.get(2..).unwrap_or_default(), out);
            } else if sub == "SETUSER"
                || (!known.contains(&sub.as_str()) && within(&sub, "SETUSER", 2))
            {
                redact_acl_setuser(src, args.get(1..).unwrap_or_default(), out);
            } else {
                redact_after_introducers(out, args);
            }
            // Password rules are redacted whatever the subcommand.
            redact_password_rules(src, args.get(1..).unwrap_or_default(), out);
        }
        "MIGRATE" => redact_migrate(args, out),
        "MODULE" => redact_module(args, out),
        "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" => {
            if let Some(script) = args.first() {
                // Keys and arguments alike: a script can take the secret
                // as either.
                redact_script(src, script, args.get(1..).unwrap_or_default(), out);
            }
        }
        "SCRIPT" if upper(args.first()) == "LOAD" => {
            if let Some(script) = args.get(1) {
                redact_script(src, script, &[], out);
            }
        }
        "FUNCTION" if upper(args.first()) == "LOAD" => {
            let code = if upper(args.get(1)) == "REPLACE" {
                args.get(2)
            } else {
                args.get(1)
            };
            if let Some(code) = code {
                redact_script(src, code, &[], out);
            }
        }
        _ => {
            match name.as_str() {
                "CONFIG" if upper(args.first()) == "SET" && args.len().is_multiple_of(2) => {
                    // The parameters and values do not pair up: everything
                    // that is not a known parameter name may be a value.
                    for arg in &args[1..] {
                        if !known_config(&text(arg)) {
                            redact_arg(out, arg);
                        }
                    }
                }
                "CONFIG" if upper(args.first()) == "SET" => redact_pairs(out, args, 1),
                // SENTINEL SET <master> <option> <value> ...
                "SENTINEL" if upper(args.first()) == "SET" => redact_pairs(out, args, 2),
                // SENTINEL CONFIG SET <option> <value> ...
                "SENTINEL" if upper(args.first()) == "CONFIG" && upper(args.get(1)) == "SET" => {
                    redact_pairs(out, args, 2);
                }
                _ => {}
            }
            let unknown = !is_known_command(&name);
            if unknown
                && (SECRET_COMMANDS.iter().any(|c| within(&name, c, 2))
                    || is_secret_parameter(&name)
                    || like_password(&name))
            {
                // A misspelled AUTH / CONFIG / ..., or a pasted config file
                // line (`requirepass secret`).
                redact_all(out, args);
            } else {
                redact_after_introducers(out, args);
            }
        }
    }
}

/// Commands written in RESP framing (`*2`, `$4`, `AUTH`, `$10`, `pw`, one
/// per line, as pasted from a protocol dump): each array becomes one
/// command whose arguments are the bulk string lines.
fn resp_commands(src: &str) -> Vec<RespCommand> {
    let all: Vec<_> = lines(src).filter(|line| !line.comment).collect();
    let count = |text: &str, marker: char| {
        text.trim()
            .strip_prefix(marker)
            .and_then(|n| n.parse::<usize>().ok())
    };
    let mut found = Vec::new();
    let mut i = 0;
    while i < all.len() {
        let Some(n) = count(all[i].text, '*') else {
            i += 1;
            continue;
        };
        let mut args = Vec::new();
        let mut j = i + 1;
        while args.len() < n && j < all.len() {
            // `$-1` is a null bulk string: an argument slot with no line.
            if all[j].text.trim() == "$-1" {
                let at = all[j].offset;
                args.push(Arg {
                    value: Vec::new(),
                    start: at,
                    end: at,
                    content_start: at,
                    content_end: at,
                });
                j += 1;
                continue;
            }
            if j + 1 >= all.len() || count(all[j].text, '$').is_none() {
                break;
            }
            let line = all[j + 1];
            let start = line.offset;
            let end = line.offset + line.text.len();
            args.push(Arg {
                value: line.text.as_bytes().to_vec(),
                start,
                end,
                content_start: start,
                content_end: end,
            });
            j += 2;
        }
        if !args.is_empty() {
            let last = all[(j - 1).min(all.len() - 1)];
            found.push(RespCommand {
                start: all[i].offset,
                end: last.offset + last.text.len(),
                args,
            });
        }
        i = j.max(i + 1);
    }
    found
}

/// A command decoded from RESP framing.
struct RespCommand {
    /// Byte range of its lines in the script, from `*N` to its last value.
    start: usize,
    end: usize,
    /// The bulk string values, with offsets into the script.
    args: Vec<Arg>,
}

/// Removes passwords from Redis commands, comment lines included, and
/// appends every removed fragment to `removed`.
pub(crate) fn redact(src: &str, removed: &mut Vec<String>) -> String {
    // 1. Secret arguments of AUTH, HELLO, ACL SETUSER, CONFIG SET, ...
    let mut ranges = Vec::new();
    for command in commands(src) {
        collect(src, &command.args, &mut ranges);
    }
    for command in resp_commands(src) {
        collect(src, &command.args, &mut ranges);
    }
    let stage1 = splice(src, ranges, removed).into_owned();

    // 2. Connection strings inside single arguments.
    let mut ranges = Vec::new();
    for command in commands(&stage1) {
        for arg in &command.args {
            let content = &stage1[arg.content_start..arg.content_end];
            ranges.extend(
                credential_ranges(content)
                    .into_iter()
                    .map(|(start, end)| (arg.content_start + start, arg.content_start + end)),
            );
        }
    }
    let stage2 = splice(&stage1, ranges, removed).into_owned();

    // 3. Connection strings typed as bare arguments (`password='a b'` split
    // over two arguments) are caught per line, and database command lines
    // (`redis-cli -a x`, `mysql -px`) on any line. A `#` line is a comment:
    // the comment rules apply to it as in SQL comments.
    let mut ranges = Vec::new();
    let mut offset = 0;
    for line in stage2.split('\n') {
        let trimmed = line.trim_start();
        let line_ranges = if let Some(body) = trimmed.strip_prefix('#') {
            // The comment text after its `#` (`#mysql -p...` too).
            let at = line.len() - body.len();
            comment_secret_ranges(body)
                .into_iter()
                .map(|(start, end)| (at + start, at + end))
                .collect()
        } else {
            let mut found = key_value_ranges(line);
            found.extend(url_ranges(line, UrlScope::Code));
            found.extend(cli_ranges(line));
            // The arguments of a command can spell a command line of their
            // own (`ECHO mysqldump appdb -p...`).
            let first = trimmed.split_whitespace().next().unwrap_or_default();
            let at = line.len() - trimmed.len() + first.len();
            found.extend(
                cli_ranges(&line[at..])
                    .into_iter()
                    .map(|(start, end)| (at + start, at + end)),
            );
            found
        };
        ranges.extend(
            line_ranges
                .into_iter()
                .map(|(start, end)| (offset + start, offset + end)),
        );
        offset += line.len() + 1;
    }
    splice(&stage2, ranges, removed).into_owned()
}

/// Which arguments of a command are key names.
#[derive(Debug, Clone, Copy)]
enum Keys {
    /// No keys.
    None,
    /// The first argument.
    First,
    /// The first `n` arguments.
    FirstN(usize),
    /// Every argument.
    All,
    /// Every argument except the last (a timeout).
    AllButLast,
    /// Every other argument, starting with the first (`MSET k v k v`).
    Alternate,
    /// `lead` leading keys, then a key count at index `at` followed by that
    /// many keys.
    Counted { lead: usize, at: usize },
    /// The keys after `STREAMS`, which take the first half of what follows.
    Streams,
    /// `MIGRATE`: the key at index 2 and the keys after `KEYS`.
    Migrate,
    /// `CONFIG GET`: known parameter names.
    ConfigGet,
    /// `CONFIG SET`: known parameter names, when the arguments pair up.
    ConfigSet,
}

/// Where option keywords (`EX`, `NX`, `WITHSCORES`, ...) are kept.
#[derive(Debug, Clone, Copy)]
enum Options {
    /// Never: every non-key argument becomes `?`.
    Never,
    /// From this argument index on.
    From(usize),
    /// After the keys of a [`Keys::Counted`] command.
    AfterKeys,
}

const OPTION_KEYWORDS: &[&str] = &[
    "ABSTTL",
    "AFTER",
    "AGGREGATE",
    "ALPHA",
    "ASC",
    "ASYNC",
    "AUTH",
    "AUTH2",
    "BEFORE",
    "BIT",
    "BLOCK",
    "BY",
    "BYLEX",
    "BYSCORE",
    "BYTE",
    "CH",
    "COPY",
    "COUNT",
    "DESC",
    "EX",
    "EXAT",
    "FREQ",
    "GET",
    "GROUP",
    "GT",
    "IDLETIME",
    "INCR",
    "KEEPTTL",
    "KEYS",
    "LEFT",
    "LIMIT",
    "LT",
    "MATCH",
    "MAX",
    "MAXLEN",
    "MIN",
    "NOACK",
    "NX",
    "PERSIST",
    "PX",
    "PXAT",
    "RANK",
    "REPLACE",
    "REV",
    "RIGHT",
    "STORE",
    "STREAMS",
    "SUM",
    "SYNC",
    "TYPE",
    "WEIGHTS",
    "WITHSCORE",
    "WITHSCORES",
    "XX",
];

/// Commands whose first argument is a subcommand, kept in the output.
const CONTAINER_COMMANDS: &[&str] = &[
    "ACL", "CLIENT", "CLUSTER", "COMMAND", "CONFIG", "DEBUG", "FUNCTION", "LATENCY", "MEMORY",
    "MODULE", "OBJECT", "PUBSUB", "SCRIPT", "SENTINEL", "SLOWLOG", "XGROUP", "XINFO",
];

fn command_spec(name: &str) -> Option<(Keys, Options)> {
    let spec = match name {
        "AUTH" | "AUTH2" | "HELLO" | "PING" | "ECHO" | "SELECT" | "SWAPDB" | "DBSIZE" | "INFO"
        | "TIME" | "MULTI" | "EXEC" | "DISCARD" | "UNWATCH" | "SAVE" | "BGSAVE"
        | "BGREWRITEAOF" | "LASTSAVE" | "SHUTDOWN" | "QUIT" | "RESET" | "MONITOR" | "ROLE"
        | "REPLICAOF" | "SLAVEOF" | "WAIT" | "WAITAOF" | "RANDOMKEY" | "READONLY" | "READWRITE"
        | "FAILOVER" | "LOLWUT" | "SYNC" | "PSYNC" => (Keys::None, Options::Never),
        "FLUSHDB" | "FLUSHALL" => (Keys::None, Options::From(0)),
        "SCAN" => (Keys::None, Options::From(1)),
        "DEL" | "UNLINK" | "EXISTS" | "MGET" | "TOUCH" | "WATCH" | "SINTER" | "SUNION"
        | "SDIFF" | "SINTERSTORE" | "SUNIONSTORE" | "SDIFFSTORE" | "PFCOUNT" | "PFMERGE"
        | "RENAME" | "RENAMENX" | "SUBSCRIBE" | "UNSUBSCRIBE" | "PSUBSCRIBE" | "PUNSUBSCRIBE"
        | "SSUBSCRIBE" | "SUNSUBSCRIBE" => (Keys::All, Options::Never),
        "SMOVE" | "RPOPLPUSH" | "BRPOPLPUSH" | "ZRANGESTORE" | "GEOSEARCHSTORE" => {
            (Keys::FirstN(2), Options::Never)
        }
        "LMOVE" | "BLMOVE" | "COPY" | "LCS" => (Keys::FirstN(2), Options::From(2)),
        "BLPOP" | "BRPOP" | "BZPOPMIN" | "BZPOPMAX" => (Keys::AllButLast, Options::Never),
        "MSET" | "MSETNX" => (Keys::Alternate, Options::Never),
        "ZUNION" | "ZINTER" | "ZDIFF" | "LMPOP" | "ZMPOP" | "SINTERCARD" => {
            (Keys::Counted { lead: 0, at: 0 }, Options::AfterKeys)
        }
        "ZUNIONSTORE" | "ZINTERSTORE" | "ZDIFFSTORE" => {
            (Keys::Counted { lead: 1, at: 1 }, Options::AfterKeys)
        }
        "BLMPOP" | "BZMPOP" => (Keys::Counted { lead: 0, at: 1 }, Options::AfterKeys),
        // Scripts can take secrets as keys or arguments: none is kept.
        "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" | "FCALL" | "FCALL_RO" => {
            (Keys::None, Options::Never)
        }
        "XREAD" | "XREADGROUP" => (Keys::Streams, Options::From(0)),
        "MIGRATE" => (Keys::Migrate, Options::From(5)),
        "SET" | "EXPIRE" | "PEXPIRE" | "EXPIREAT" | "PEXPIREAT" | "SSCAN" | "HSCAN" | "ZSCAN"
        | "LPOS" => (Keys::First, Options::From(2)),
        "GETEX" | "SORT" | "SORT_RO" => (Keys::First, Options::From(1)),
        "ZRANGE" | "ZREVRANGE" | "ZRANGEBYSCORE" | "ZREVRANGEBYSCORE" | "ZRANGEBYLEX"
        | "ZREVRANGEBYLEX" | "RESTORE" | "BITCOUNT" => (Keys::First, Options::From(3)),
        name if FIRST_KEY_COMMANDS.contains(&name) => (Keys::First, Options::Never),
        _ => return None,
    };
    Some(spec)
}

/// True for a command the console knows.
fn is_known_command(name: &str) -> bool {
    command_spec(name).is_some() || CONTAINER_COMMANDS.contains(&name)
}

/// Known commands whose first argument is a key (or a channel or pattern)
/// and whose other arguments are values.
const FIRST_KEY_COMMANDS: &[&str] = &[
    // Strings and bits.
    "GET",
    "SETNX",
    "SETEX",
    "PSETEX",
    "GETSET",
    "GETDEL",
    "APPEND",
    "STRLEN",
    "INCR",
    "DECR",
    "INCRBY",
    "DECRBY",
    "INCRBYFLOAT",
    "GETRANGE",
    "SETRANGE",
    "SUBSTR",
    "GETBIT",
    "SETBIT",
    "BITPOS",
    "BITFIELD",
    "BITFIELD_RO",
    "PFADD",
    // Generic key commands.
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "PERSIST",
    "DUMP",
    "KEYS",
    // Hashes.
    "HSET",
    "HSETNX",
    "HGET",
    "HMSET",
    "HMGET",
    "HDEL",
    "HLEN",
    "HSTRLEN",
    "HKEYS",
    "HVALS",
    "HGETALL",
    "HEXISTS",
    "HINCRBY",
    "HINCRBYFLOAT",
    "HRANDFIELD",
    "HEXPIRE",
    "HPEXPIRE",
    "HEXPIREAT",
    "HPEXPIREAT",
    "HTTL",
    "HPTTL",
    "HEXPIRETIME",
    "HPEXPIRETIME",
    "HPERSIST",
    "HGETDEL",
    "HGETEX",
    "HSETEX",
    // Lists.
    "LPUSH",
    "RPUSH",
    "LPUSHX",
    "RPUSHX",
    "LPOP",
    "RPOP",
    "LLEN",
    "LRANGE",
    "LINDEX",
    "LSET",
    "LINSERT",
    "LREM",
    "LTRIM",
    // Sets.
    "SADD",
    "SREM",
    "SCARD",
    "SISMEMBER",
    "SMISMEMBER",
    "SMEMBERS",
    "SPOP",
    "SRANDMEMBER",
    // Sorted sets.
    "ZADD",
    "ZREM",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZSCORE",
    "ZMSCORE",
    "ZINCRBY",
    "ZRANK",
    "ZREVRANK",
    "ZPOPMIN",
    "ZPOPMAX",
    "ZRANDMEMBER",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
    "ZREMRANGEBYLEX",
    // Geo.
    "GEOADD",
    "GEOPOS",
    "GEODIST",
    "GEOHASH",
    "GEORADIUS",
    "GEORADIUSBYMEMBER",
    "GEORADIUS_RO",
    "GEORADIUSBYMEMBER_RO",
    "GEOSEARCH",
    // Streams.
    "XADD",
    "XLEN",
    "XRANGE",
    "XREVRANGE",
    "XDEL",
    "XTRIM",
    "XACK",
    "XCLAIM",
    "XAUTOCLAIM",
    "XPENDING",
    "XSETID",
    "XACKDEL",
    "XDELEX",
    // Pub/sub: the channel.
    "PUBLISH",
    "SPUBLISH",
];

/// The subcommands of each container command. Any other subcommand (a
/// typo) becomes `?` in normalized text, with all its arguments.
const KNOWN_SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "ACL",
        &[
            "CAT", "DELUSER", "DRYRUN", "GENPASS", "GETUSER", "HELP", "LIST", "LOAD", "LOG",
            "SAVE", "SETUSER", "USERS", "WHOAMI",
        ],
    ),
    (
        "CLIENT",
        &[
            "CACHING",
            "GETNAME",
            "GETREDIR",
            "HELP",
            "ID",
            "INFO",
            "KILL",
            "LIST",
            "NO-EVICT",
            "NO-TOUCH",
            "PAUSE",
            "REPLY",
            "SETINFO",
            "SETNAME",
            "TRACKING",
            "TRACKINGINFO",
            "UNBLOCK",
            "UNPAUSE",
        ],
    ),
    (
        "CLUSTER",
        &[
            "ADDSLOTS",
            "ADDSLOTSRANGE",
            "BUMPEPOCH",
            "COUNT-FAILURE-REPORTS",
            "COUNTKEYSINSLOT",
            "DELSLOTS",
            "DELSLOTSRANGE",
            "FAILOVER",
            "FLUSHSLOTS",
            "FORGET",
            "GETKEYSINSLOT",
            "HELP",
            "INFO",
            "KEYSLOT",
            "LINKS",
            "MEET",
            "MYID",
            "MYSHARDID",
            "NODES",
            "REPLICAS",
            "REPLICATE",
            "RESET",
            "SAVECONFIG",
            "SET-CONFIG-EPOCH",
            "SETSLOT",
            "SHARDS",
            "SLAVES",
            "SLOTS",
        ],
    ),
    (
        "COMMAND",
        &[
            "COUNT",
            "DOCS",
            "GETKEYS",
            "GETKEYSANDFLAGS",
            "HELP",
            "INFO",
            "LIST",
        ],
    ),
    ("CONFIG", &["GET", "HELP", "RESETSTAT", "REWRITE", "SET"]),
    ("DEBUG", &["HELP", "OBJECT", "RELOAD", "SLEEP"]),
    (
        "FUNCTION",
        &[
            "DELETE", "DUMP", "FLUSH", "HELP", "KILL", "LIST", "LOAD", "RESTORE", "STATS",
        ],
    ),
    (
        "LATENCY",
        &[
            "DOCTOR",
            "GRAPH",
            "HELP",
            "HISTOGRAM",
            "HISTORY",
            "LATEST",
            "RESET",
        ],
    ),
    (
        "MEMORY",
        &["DOCTOR", "HELP", "MALLOC-STATS", "PURGE", "STATS", "USAGE"],
    ),
    ("MODULE", &["HELP", "LIST", "LOAD", "LOADEX", "UNLOAD"]),
    (
        "OBJECT",
        &["ENCODING", "FREQ", "HELP", "IDLETIME", "REFCOUNT"],
    ),
    (
        "PUBSUB",
        &[
            "CHANNELS",
            "HELP",
            "NUMPAT",
            "NUMSUB",
            "SHARDCHANNELS",
            "SHARDNUMSUB",
        ],
    ),
    (
        "SCRIPT",
        &["DEBUG", "EXISTS", "FLUSH", "HELP", "KILL", "LOAD"],
    ),
    (
        "SENTINEL",
        &[
            "CKQUORUM",
            "CONFIG",
            "DEBUG",
            "FAILOVER",
            "FLUSHCONFIG",
            "GET-MASTER-ADDR-BY-NAME",
            "HELP",
            "INFO-CACHE",
            "IS-MASTER-DOWN-BY-ADDR",
            "MASTER",
            "MASTERS",
            "MONITOR",
            "MYID",
            "PENDING-SCRIPTS",
            "REMOVE",
            "REPLICAS",
            "RESET",
            "SENTINELS",
            "SET",
            "SIMULATE-FAILURE",
            "SLAVES",
        ],
    ),
    ("SLOWLOG", &["GET", "HELP", "LEN", "RESET"]),
    (
        "XGROUP",
        &[
            "CREATE",
            "CREATECONSUMER",
            "DELCONSUMER",
            "DESTROY",
            "HELP",
            "SETID",
        ],
    ),
    ("XINFO", &["CONSUMERS", "GROUPS", "HELP", "STREAM"]),
];

/// True when `sub` is a subcommand of the container `command`.
fn is_known_subcommand(command: &str, sub: &str) -> bool {
    KNOWN_SUBCOMMANDS
        .iter()
        .any(|(name, subs)| *name == command && subs.contains(&sub))
}

/// Redis configuration parameters: the only `CONFIG GET | SET` arguments
/// kept in normalized text. Anything else there may be a value.
const KNOWN_CONFIG: &[&str] = &[
    "aclfile",
    "active-expire-effort",
    "activedefrag",
    "appendfilename",
    "appendfsync",
    "appendonly",
    "bind",
    "busy-reply-threshold",
    "client-output-buffer-limit",
    "cluster-enabled",
    "cluster-node-timeout",
    "databases",
    "dbfilename",
    "dir",
    "dynamic-hz",
    "enable-debug-command",
    "hash-max-listpack-entries",
    "hash-max-listpack-value",
    "hash-max-ziplist-entries",
    "hash-max-ziplist-value",
    "hz",
    "io-threads",
    "latency-monitor-threshold",
    "lazyfree-lazy-eviction",
    "lazyfree-lazy-expire",
    "list-max-listpack-size",
    "list-max-ziplist-size",
    "logfile",
    "loglevel",
    "lua-time-limit",
    "masterauth",
    "masteruser",
    "maxclients",
    "maxmemory",
    "maxmemory-clients",
    "maxmemory-policy",
    "maxmemory-samples",
    "min-replicas-max-lag",
    "min-replicas-to-write",
    "notify-keyspace-events",
    "port",
    "proto-max-bulk-len",
    "protected-mode",
    "rdbchecksum",
    "rdbcompression",
    "repl-backlog-size",
    "repl-timeout",
    "replica-read-only",
    "replicaof",
    "requirepass",
    "save",
    "set-max-intset-entries",
    "slaveof",
    "slowlog-log-slower-than",
    "slowlog-max-len",
    "stop-writes-on-bgsave-error",
    "tcp-keepalive",
    "timeout",
    "tls-auth-clients",
    "tls-ca-cert-file",
    "tls-cert-file",
    "tls-key-file",
    "tls-key-file-pass",
    "tls-port",
    "zset-max-listpack-entries",
    "zset-max-ziplist-entries",
];

fn known_config(name: &str) -> bool {
    KNOWN_CONFIG.contains(&name.to_ascii_lowercase().as_str())
}

/// Spec for the arguments after a container subcommand.
fn subcommand_spec(command: &str, sub: &str) -> (Keys, Options) {
    match (command, sub) {
        ("OBJECT", "ENCODING" | "FREQ" | "IDLETIME" | "REFCOUNT")
        | ("XINFO", "STREAM" | "GROUPS" | "CONSUMERS")
        | ("XGROUP", "CREATE" | "SETID" | "DESTROY" | "CREATECONSUMER" | "DELCONSUMER")
        | ("MEMORY", "USAGE")
        | ("DEBUG", "OBJECT")
        | ("CLUSTER", "KEYSLOT" | "COUNTKEYSINSLOT") => (Keys::First, Options::Never),
        // Parameter names are not secret; their values become `?`.
        ("CONFIG", "GET") => (Keys::ConfigGet, Options::Never),
        ("CONFIG", "SET") => (Keys::ConfigSet, Options::Never),
        ("ACL", "SETUSER" | "GETUSER") => (Keys::First, Options::Never),
        ("ACL", "DELUSER") => (Keys::All, Options::Never),
        // Unknown subcommands (`CONFIG SETT`, `ACL SETUSR`) and those
        // without keys keep nothing after the subcommand.
        _ => (Keys::None, Options::Never),
    }
}

/// Marks which arguments stay as written.
fn kept_arguments(keys: Keys, options: Options, args: &[Arg]) -> Vec<bool> {
    let n = args.len();
    let mut kept = vec![false; n];
    let mut keys_end = 0;
    // The arguments do not match the command's shape: only what came
    // before the mismatch is kept.
    let mut mismatch = false;
    let mark = |kept: &mut Vec<bool>, range: std::ops::Range<usize>| {
        let (start, end) = (range.start.min(n), range.end.min(n));
        if start < end {
            kept[start..end].fill(true);
        }
    };
    match keys {
        Keys::None => {}
        Keys::First => mark(&mut kept, 0..1),
        Keys::FirstN(count) => mark(&mut kept, 0..count),
        Keys::All => mark(&mut kept, 0..n),
        Keys::AllButLast => mark(&mut kept, 0..n.saturating_sub(1)),
        // With an odd count the pairs do not line up: nothing is kept.
        Keys::Alternate if n.is_multiple_of(2) => {
            for k in (0..n).step_by(2) {
                kept[k] = true;
            }
        }
        Keys::Alternate => {}
        Keys::ConfigGet => {
            for (k, arg) in args.iter().enumerate() {
                kept[k] = known_config(&text(arg));
            }
        }
        Keys::ConfigSet if n.is_multiple_of(2) => {
            for k in (0..n).step_by(2) {
                kept[k] = known_config(&text(&args[k]));
            }
        }
        Keys::ConfigSet => {}
        Keys::Counted { lead, at } => {
            mark(&mut kept, 0..lead);
            let count = args.get(at).and_then(|a| text(a).parse::<usize>().ok());
            let first = at + 1;
            match count {
                Some(count) if first.saturating_add(count) <= n => {
                    mark(&mut kept, first..first + count);
                    keys_end = first + count;
                }
                // The count does not match the arguments: nothing after
                // it is kept, options included.
                _ => mismatch = true,
            }
        }
        Keys::Streams => {
            if let Some(p) = args.iter().position(|a| is(a, "STREAMS")) {
                let rest = n - p - 1;
                if rest.is_multiple_of(2) {
                    mark(&mut kept, p + 1..p + 1 + rest / 2);
                } else {
                    mismatch = true;
                }
                keys_end = p + 1;
            }
        }
        Keys::Migrate => {
            if args.get(2).is_some_and(|a| !a.value.is_empty()) {
                kept[2] = true;
            }
            if let Some(p) = args.iter().position(|a| is(a, "KEYS")) {
                // Key names after KEYS, but never the argument of an
                // AUTH / AUTH2 option written after them.
                let mut k = p + 1;
                while k < n {
                    if is(&args[k], "AUTH") {
                        k += 2;
                    } else if is(&args[k], "AUTH2") {
                        k += 3;
                    } else {
                        kept[k] = true;
                        k += 1;
                    }
                }
            }
        }
    }
    let options_from = match options {
        _ if mismatch => None,
        Options::Never => None,
        Options::From(k) => Some(k),
        Options::AfterKeys => Some(keys_end),
    };
    if let Some(from) = options_from {
        let to = if matches!(keys, Keys::Streams) {
            keys_end
        } else {
            n
        };
        for k in from..to.min(n) {
            if OPTION_KEYWORDS.contains(&upper(Some(&args[k])).as_str()) {
                kept[k] = true;
            }
        }
    }
    kept
}

/// Keeps command names, subcommands and key names; replaces every other
/// argument with `?`. Unknown commands and subcommands become `?`
/// themselves. One command per line; comment lines are dropped.
pub(crate) fn normalize(src: &str) -> String {
    let resp = resp_commands(src);
    let mut lines = Vec::new();
    for command in commands(src) {
        if command.comment {
            continue;
        }
        let Some(first) = command.args.first() else {
            continue;
        };
        // Lines of a RESP frame are normalized as the decoded command,
        // once, at the frame's first line.
        if let Some(frame) = resp
            .iter()
            .find(|f| first.start >= f.start && first.start < f.end)
        {
            if first.start == frame.start {
                lines.push(normalize_command(src, &frame.args));
            }
            continue;
        }
        lines.push(normalize_command(src, &command.args));
    }
    lines.join("\n")
}

/// Every argument after a command name (values and key names alike),
/// which normalized text may show as `?`: an error message at text level
/// `normalized` must not show them either.
pub(crate) fn argument_values(src: &str) -> Vec<String> {
    commands(src)
        .iter()
        .filter(|command| !command.comment)
        .flat_map(|command| command.args.iter().skip(1))
        .map(|arg| text(arg).into_owned())
        .collect()
}

/// One command, normalized.
fn normalize_command(src: &str, command: &[Arg]) -> String {
    let Some(name) = command.first() else {
        return String::new();
    };
    let upper_name = upper(Some(name));
    let mut args = &command[1..];
    if !is_known_command(&upper_name) {
        // An unknown command (a typo such as `AUHT pw`, a module command,
        // a stray RESP line): its name may itself be a secret, so nothing
        // of it is kept.
        return vec!["?"; command.len()].join(" ");
    }
    let mut parts = vec![&src[name.start..name.end]];
    let (keys, options) = if CONTAINER_COMMANDS.contains(&upper_name.as_str()) {
        match args.first() {
            Some(sub) => {
                let upper_sub = upper(Some(sub));
                let known = is_known_subcommand(&upper_name, &upper_sub);
                parts.push(if known { &src[sub.start..sub.end] } else { "?" });
                args = &args[1..];
                if known {
                    subcommand_spec(&upper_name, &upper_sub)
                } else {
                    (Keys::None, Options::Never)
                }
            }
            None => (Keys::None, Options::Never),
        }
    } else {
        command_spec(&upper_name).unwrap_or((Keys::None, Options::Never))
    };
    let kept = kept_arguments(keys, options, args);
    for (arg, keep) in args.iter().zip(kept) {
        let raw = &src[arg.start..arg.end];
        // `[redacted]` never shows in normalized text: it is a value.
        parts.push(if keep && !raw.contains(REDACTED) {
            raw
        } else {
            "?"
        });
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(src: &str) -> Vec<Vec<String>> {
        commands(src)
            .iter()
            .map(|c| c.args.iter().map(|a| text(a).into_owned()).collect())
            .collect()
    }

    #[test]
    fn commands_use_the_driver_splitter() {
        assert_eq!(
            values("SET \"a b\" 'it\\'s' \"\\x41\\n\"\r\nGET\0k\n\n# AUTH pw\n"),
            [
                vec!["SET", "a b", "it's", "A\n"],
                vec!["GET", "k"],
                vec!["AUTH", "pw"],
            ]
        );
        let found = commands("PING\n  # AUTH pw");
        assert!(!found[0].comment);
        assert!(found[1].comment);
        // Offsets point into the whole script.
        assert_eq!(found[1].args[1].start, "PING\n  # AUTH ".len());
    }

    #[test]
    fn a_quote_never_spans_lines() {
        assert_eq!(
            values("SET k \"unterminated\nAUTH pw"),
            [vec!["SET", "k", "unterminated"], vec!["AUTH", "pw"]]
        );
    }
}
