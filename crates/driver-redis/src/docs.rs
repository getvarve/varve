//! Console help for one command (design doc 4.2, Redis console): `COMMAND
//! DOCS` where the server has it (Redis 7.0 and newer, Valkey), with the
//! syntax built from its argument tree. Redis 6.2 has no `COMMAND DOCS`;
//! the help then says so and gives what `COMMAND INFO` knows (arity and
//! flags).
//!
//! The lookup runs on the session connection, or on a side connection
//! while a console `MULTI` block is open (so it is never queued into the
//! user's transaction).

use redis::Value as Resp;
use serde::{Deserialize, Serialize};
use varve_driver::DriverError;

use crate::measure;
use crate::modules::info_pairs;
use crate::reply::{as_int, as_text};
use crate::session::{Call, RedisSession};

/// Most bytes of strings of a `COMMAND DOCS` reply (some module commands
/// have long documentation).
const DOCS_LIMIT: u64 = 4 * 1024 * 1024;

/// Help for one command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandHelp {
    /// The command as looked up, upper case (`CONFIG SET`).
    pub name: String,
    /// True when the server knows the command.
    pub known: bool,
    /// True when the text comes from `COMMAND DOCS`.
    pub from_docs: bool,
    /// One-line summary.
    pub summary: Option<String>,
    /// Syntax built from the argument tree, for example
    /// `SET key value [NX | XX] [GET] ...`.
    pub syntax: Option<String>,
    /// The version that added it.
    pub since: Option<String>,
    /// Group (`string`, `generic`, a module name, ...).
    pub group: Option<String>,
    /// Time complexity.
    pub complexity: Option<String>,
    /// The version that deprecated it, when it is.
    pub deprecated_since: Option<String>,
    /// What replaces it, when deprecated.
    pub replaced_by: Option<String>,
    /// Subcommands (`CONFIG GET`, `CONFIG SET`, ...), when it has them.
    pub subcommands: Vec<String>,
    /// `COMMAND INFO` arity (negative: at least that many words).
    pub arity: Option<i64>,
    /// `COMMAND INFO` flags.
    pub flags: Vec<String>,
    /// A note for the user, for example why there are no docs.
    pub note: Option<String>,
}

/// An argument of the `COMMAND DOCS` tree.
struct Arg {
    name: String,
    kind: String,
    token: Option<String>,
    display: Option<String>,
    optional: bool,
    multiple: bool,
    multiple_token: bool,
    children: Vec<Arg>,
}

fn parse_arg(reply: &Resp) -> Option<Arg> {
    let pairs = info_pairs(reply);
    let get = |name: &str| pairs.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
    let flags: Vec<String> = match get("flags") {
        Some(Resp::Array(items)) => items.iter().filter_map(as_text).collect(),
        _ => Vec::new(),
    };
    let children = match get("arguments") {
        Some(Resp::Array(items)) => items.iter().filter_map(parse_arg).collect(),
        _ => Vec::new(),
    };
    Some(Arg {
        name: get("name").and_then(as_text)?,
        kind: get("type").and_then(as_text).unwrap_or_default(),
        token: get("token").and_then(as_text),
        display: get("display_text").and_then(as_text),
        optional: flags.iter().any(|f| f == "optional"),
        multiple: flags.iter().any(|f| f == "multiple"),
        multiple_token: flags.iter().any(|f| f == "multiple_token"),
        children,
    })
}

/// The syntax of one argument, as the Redis documentation writes it.
fn render(arg: &Arg) -> String {
    let inner = match arg.kind.as_str() {
        "pure-token" => arg.token.clone().unwrap_or_else(|| arg.name.to_uppercase()),
        "oneof" => {
            let choices: Vec<String> = arg.children.iter().map(render).collect();
            let joined = choices.join(" | ");
            if arg.optional {
                joined
            } else {
                format!("<{joined}>")
            }
        }
        "block" => arg
            .children
            .iter()
            .map(render)
            .collect::<Vec<_>>()
            .join(" "),
        _ => arg.display.clone().unwrap_or_else(|| arg.name.clone()),
    };
    let one = match (&arg.token, arg.kind.as_str()) {
        (Some(token), kind) if kind != "pure-token" => format!("{token} {inner}"),
        _ => inner,
    };
    let repeated = if arg.multiple {
        if arg.multiple_token {
            format!("{one} [{one} ...]")
        } else {
            match &arg.token {
                Some(token) if arg.kind != "pure-token" => {
                    let bare = one.strip_prefix(&format!("{token} ")).unwrap_or(&one);
                    format!("{token} {bare} [{bare} ...]")
                }
                _ => format!("{one} [{one} ...]"),
            }
        }
    } else {
        one
    };
    if arg.optional {
        format!("[{repeated}]")
    } else {
        repeated
    }
}

/// Fills `help` from one `COMMAND DOCS` entry (the docs map).
fn fill_from_docs(help: &mut CommandHelp, docs: &Resp, words: &str) {
    let pairs = info_pairs(docs);
    let text = |name: &str| {
        pairs
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| as_text(v))
    };
    help.summary = text("summary");
    help.since = text("since");
    help.group = text("group");
    help.complexity = text("complexity");
    help.deprecated_since = text("deprecated_since");
    help.replaced_by = text("replaced_by");
    let args: Vec<Arg> = match pairs.iter().find(|(n, _)| n == "arguments") {
        Some((_, Resp::Array(items))) => items.iter().filter_map(parse_arg).collect(),
        _ => Vec::new(),
    };
    let mut syntax = words.to_string();
    for arg in &args {
        syntax.push(' ');
        syntax.push_str(&render(arg));
    }
    help.syntax = Some(syntax);
    help.subcommands = match pairs.iter().find(|(n, _)| n == "subcommands") {
        Some((_, reply)) => info_pairs(reply)
            .into_iter()
            .map(|(name, _)| name.replace('|', " ").to_ascii_uppercase())
            .collect(),
        None => Vec::new(),
    };
}

/// The docs entry of `name` (or of `name|sub`) in a `COMMAND DOCS` reply.
fn find_docs<'a>(reply: &'a Resp, name: &str, sub: Option<&str>) -> Option<&'a Resp> {
    let entries = info_pairs(reply);
    let (_, docs) = entries
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))?;
    let Some(sub) = sub else {
        return Some(docs);
    };
    let pairs = info_pairs(docs);
    let (_, subs) = pairs.into_iter().find(|(n, _)| n == "subcommands")?;
    let wanted = format!("{name}|{sub}");
    info_pairs(subs)
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(&wanted))
        .map(|(_, d)| d)
}

/// Runs one lookup command, on the side connection while a `MULTI` block
/// is open; an error reply comes back as `Ok(Err(..))`.
async fn lookup(
    call: &mut Call<'_>,
    args: &[&[u8]],
) -> Result<Result<Resp, DriverError>, DriverError> {
    if call.in_multi() {
        return match measure::side_query(call, args, DOCS_LIMIT).await {
            Ok(reply) => Ok(Ok(reply)),
            Err(error) if error.code.is_some() => Ok(Err(error)),
            Err(error) => Err(error),
        };
    }
    let mut cmd = redis::cmd(&String::from_utf8_lossy(args[0]));
    for arg in &args[1..] {
        cmd.arg(*arg);
    }
    match call.send(&cmd).await? {
        Resp::ServerError(e) => Ok(Err(crate::browser::query_error(&e))),
        reply => Ok(Ok(reply)),
    }
}

impl RedisSession {
    /// Help for the command `name` (one word, or a command and its
    /// subcommand: `CONFIG SET`, `config|set`): see the module
    /// documentation. An unknown command comes back with `known: false`.
    ///
    /// # Errors
    ///
    /// Connection errors, Cancelled.
    pub async fn command_help(&self, name: &str) -> Result<CommandHelp, DriverError> {
        let mut words = name
            .split(|c: char| c.is_whitespace() || c == '|')
            .filter(|w| !w.is_empty())
            .map(str::to_ascii_uppercase);
        let command = words.next().unwrap_or_default();
        let sub = words.next();
        let mut help = CommandHelp {
            name: match &sub {
                Some(sub) => format!("{command} {sub}"),
                None => command.clone(),
            },
            known: false,
            from_docs: false,
            summary: None,
            syntax: None,
            since: None,
            group: None,
            complexity: None,
            deprecated_since: None,
            replaced_by: None,
            subcommands: Vec::new(),
            arity: None,
            flags: Vec::new(),
            note: None,
        };
        if command.is_empty() {
            help.note = Some("Put the cursor on a command to see its help".into());
            return Ok(help);
        }
        let mut call = self.begin().await?;
        // COMMAND INFO: arity and flags, also on Redis 6.2.
        let info_name = match &sub {
            Some(sub) => format!("{command}|{sub}"),
            None => command.clone(),
        };
        let mut info = lookup(&mut call, &[b"COMMAND", b"INFO", info_name.as_bytes()]).await?;
        // Redis 6.2 does not describe subcommands on their own.
        if sub.is_some()
            && matches!(&info, Ok(Resp::Array(e)) if matches!(e.first(), Some(Resp::Nil) | None))
        {
            info = lookup(&mut call, &[b"COMMAND", b"INFO", command.as_bytes()]).await?;
            if call.details().compat < (7, 0) {
                // Without COMMAND DOCS the second word cannot be told apart
                // from an argument: describe the command itself.
                help.name.clone_from(&command);
            }
        }
        let mut info_known = false;
        if let Ok(Resp::Array(entries)) = &info
            && let Some(Resp::Array(entry)) = entries.first()
        {
            info_known = true;
            help.arity = entry.get(1).and_then(as_int);
            help.flags = match entry.get(2) {
                Some(Resp::Array(flags) | Resp::Set(flags)) => {
                    flags.iter().filter_map(as_text).collect()
                }
                _ => Vec::new(),
            };
        }
        let docs = lookup(&mut call, &[b"COMMAND", b"DOCS", command.as_bytes()]).await?;
        match docs {
            Ok(reply) => match find_docs(&reply, &command, sub.as_deref()).or_else(|| {
                // The second word is an argument, not a subcommand (`GET
                // key`): the help of the command itself.
                let entry = find_docs(&reply, &command, None)?;
                help.name.clone_from(&command);
                Some(entry)
            }) {
                Some(entry) => {
                    help.known = true;
                    help.from_docs = true;
                    let words = help.name.clone();
                    fill_from_docs(&mut help, entry, &words);
                }
                None => {
                    help.known = info_known;
                    help.note = Some(if info_known {
                        format!("The server has no documentation for {}", help.name)
                    } else {
                        format!("The server does not know the command {}", help.name)
                    });
                }
            },
            Err(error) if error.code.as_deref() == Some("NOPERM") => {
                help.known = info_known;
                help.note = Some(
                    "This user may not run COMMAND DOCS (an ACL rule), so the documentation \
                     cannot be shown; the arity and flags come from COMMAND INFO when it is \
                     permitted"
                        .into(),
                );
            }
            Err(_) => {
                help.known = info_known;
                help.note = Some(if info_known {
                    "This server has no COMMAND DOCS (it needs Redis 7.0 or newer, or Valkey); \
                     only the arity and flags from COMMAND INFO are shown"
                        .into()
                } else {
                    format!(
                        "This server has no COMMAND DOCS (it needs Redis 7.0 or newer, or \
                         Valkey), and COMMAND INFO does not know {}",
                        help.name
                    )
                });
            }
        }
        Ok(help)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(text: &str) -> Resp {
        Resp::BulkString(text.as_bytes().to_vec())
    }

    fn arg(fields: Vec<(&str, Resp)>) -> Resp {
        Resp::Array(fields.into_iter().flat_map(|(k, v)| [bulk(k), v]).collect())
    }

    #[test]
    fn syntax_follows_the_argument_tree() {
        let set_docs = arg(vec![
            ("summary", bulk("Sets the string value of a key")),
            ("since", bulk("1.0.0")),
            ("group", bulk("string")),
            ("complexity", bulk("O(1)")),
            (
                "arguments",
                Resp::Array(vec![
                    arg(vec![("name", bulk("key")), ("type", bulk("key"))]),
                    arg(vec![("name", bulk("value")), ("type", bulk("string"))]),
                    arg(vec![
                        ("name", bulk("condition")),
                        ("type", bulk("oneof")),
                        ("flags", Resp::Array(vec![bulk("optional")])),
                        (
                            "arguments",
                            Resp::Array(vec![
                                arg(vec![
                                    ("name", bulk("nx")),
                                    ("type", bulk("pure-token")),
                                    ("token", bulk("NX")),
                                ]),
                                arg(vec![
                                    ("name", bulk("xx")),
                                    ("type", bulk("pure-token")),
                                    ("token", bulk("XX")),
                                ]),
                            ]),
                        ),
                    ]),
                    arg(vec![
                        ("name", bulk("seconds")),
                        ("type", bulk("integer")),
                        ("token", bulk("EX")),
                        ("flags", Resp::Array(vec![bulk("optional")])),
                    ]),
                    arg(vec![
                        ("name", bulk("field")),
                        ("type", bulk("string")),
                        ("flags", Resp::Array(vec![bulk("multiple")])),
                    ]),
                ]),
            ),
        ]);
        let reply = Resp::Array(vec![bulk("set"), set_docs]);
        let mut help = CommandHelp {
            name: "SET".into(),
            known: false,
            from_docs: false,
            summary: None,
            syntax: None,
            since: None,
            group: None,
            complexity: None,
            deprecated_since: None,
            replaced_by: None,
            subcommands: Vec::new(),
            arity: None,
            flags: Vec::new(),
            note: None,
        };
        let entry = find_docs(&reply, "SET", None);
        assert!(entry.is_some());
        if let Some(entry) = entry {
            fill_from_docs(&mut help, entry, "SET");
        }
        assert_eq!(
            help.syntax.as_deref(),
            Some("SET key value [NX | XX] [EX seconds] field [field ...]")
        );
        assert_eq!(help.group.as_deref(), Some("string"));
        assert!(find_docs(&reply, "SET", Some("x")).is_none());
    }

    #[test]
    fn subcommands_are_found() {
        let reply = Resp::Array(vec![
            bulk("config"),
            arg(vec![
                ("summary", bulk("A container")),
                (
                    "subcommands",
                    Resp::Array(vec![
                        bulk("config|get"),
                        arg(vec![("summary", bulk("Returns the effective values"))]),
                    ]),
                ),
            ]),
        ]);
        let entry = find_docs(&reply, "CONFIG", Some("GET"));
        let summary = entry.map(info_pairs).and_then(|p| {
            p.into_iter()
                .find(|(n, _)| n == "summary")
                .map(|(_, v)| as_text(v))
        });
        assert_eq!(
            summary.flatten().as_deref(),
            Some("Returns the effective values")
        );
    }
}
