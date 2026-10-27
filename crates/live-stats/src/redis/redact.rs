//! Hides secrets in command arguments the server reports back (slow log).
//!
//! Redis 7 and Valkey already replace the arguments of `AUTH`, `HELLO ...
//! AUTH` and `MIGRATE ... AUTH` with `(redacted)` in the slow log, but
//! Redis 6.2 does not hide `CONFIG SET requirepass` or `ACL SETUSER`
//! passwords. This module masks every argument that can carry a password,
//! whatever the server already did.

use varve_driver::Value;

use super::config::is_sensitive_config;
use super::reply::text;

/// Text that replaces a hidden argument.
pub const REDACTED: &str = "(redacted)";

/// Replaces secret arguments of one command (name first) in place and
/// returns true when anything was hidden.
pub(crate) fn redact_args(args: &mut [Value]) -> bool {
    let words: Vec<String> = args
        .iter()
        .map(|a| text(a).unwrap_or_default().to_ascii_uppercase())
        .collect();
    let mut hide: Vec<usize> = Vec::new();
    match words.first().map(String::as_str) {
        Some("AUTH") => hide.extend(1..args.len()),
        // HELLO [protover [AUTH username password] [SETNAME clientname]]:
        // walked option by option, so a client named `auth` is not taken
        // for the AUTH option.
        Some("HELLO") => {
            let mut at = 2;
            while at < words.len() {
                match words[at].as_str() {
                    "AUTH" => {
                        hide.push(at + 2);
                        at += 3;
                    }
                    "SETNAME" => at += 2,
                    _ => at += 1,
                }
            }
        }
        // MIGRATE host port key|"" db timeout [COPY] [REPLACE]
        //   [AUTH password | AUTH2 username password] [KEYS key ...]:
        // options start after the timeout and end at KEYS.
        Some("MIGRATE") => {
            let mut at = 6;
            while at < words.len() {
                match words[at].as_str() {
                    "AUTH" => {
                        hide.push(at + 1);
                        at += 2;
                    }
                    "AUTH2" => {
                        hide.push(at + 2);
                        at += 3;
                    }
                    "KEYS" => break,
                    _ => at += 1,
                }
            }
        }
        Some("CONFIG") if words.get(1).is_some_and(|w| w == "SET") => {
            let mut at = 2;
            while at + 1 < words.len() {
                if is_sensitive_config(&words[at].to_ascii_lowercase()) {
                    hide.push(at + 1);
                }
                at += 2;
            }
        }
        Some("ACL") if words.get(1).is_some_and(|w| w == "SETUSER") => {
            for (at, word) in words.iter().enumerate().skip(3) {
                if word.starts_with(['>', '<', '#', '!']) {
                    hide.push(at);
                }
            }
        }
        // SENTINEL SET <master> <option> <value> ... and
        // SENTINEL CONFIG SET <option> <value> ...: option/value pairs.
        Some("SENTINEL") => {
            let first = match (
                words.get(1).map(String::as_str),
                words.get(2).map(String::as_str),
            ) {
                (Some("SET"), _) => Some(3),
                (Some("CONFIG"), Some("SET")) => Some(3),
                _ => None,
            };
            if let Some(mut at) = first {
                while at + 1 < words.len() {
                    if is_sensitive_config(&words[at].to_ascii_lowercase()) {
                        hide.push(at + 1);
                    }
                    at += 2;
                }
            }
        }
        _ => {}
    }
    let mut changed = false;
    for at in hide {
        if let Some(arg) = args.get_mut(at)
            && text(arg).as_deref() != Some(REDACTED)
        {
            *arg = Value::Text(REDACTED.into());
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<Value> {
        words.iter().map(|w| Value::Text((*w).into())).collect()
    }

    fn shown(values: &[Value]) -> Vec<String> {
        values.iter().filter_map(text).collect()
    }

    #[test]
    fn hides_passwords() {
        let mut auth = args(&["AUTH", "user", "secret"]);
        assert!(redact_args(&mut auth));
        assert_eq!(shown(&auth), ["AUTH", REDACTED, REDACTED]);

        let mut config = args(&["config", "set", "maxmemory", "1gb", "requirepass", "s3"]);
        assert!(redact_args(&mut config));
        assert_eq!(
            shown(&config),
            ["config", "set", "maxmemory", "1gb", "requirepass", REDACTED]
        );

        let mut acl = args(&["ACL", "SETUSER", "bob", "on", ">pw", "~*", "+@all"]);
        assert!(redact_args(&mut acl));
        assert_eq!(
            shown(&acl),
            ["ACL", "SETUSER", "bob", "on", REDACTED, "~*", "+@all"]
        );

        let mut hello = args(&["HELLO", "3", "AUTH", "default", "pw"]);
        assert!(redact_args(&mut hello));
        assert_eq!(shown(&hello)[4], REDACTED);
        // A client named `auth` is the SETNAME value, not the AUTH option.
        let mut named = args(&["HELLO", "2", "SETNAME", "auth", "AUTH", "default", "pw"]);
        assert!(redact_args(&mut named));
        assert_eq!(
            shown(&named),
            ["HELLO", "2", "SETNAME", "auth", "AUTH", "default", REDACTED]
        );
        let mut after = args(&["HELLO", "2", "AUTH", "default", "pw", "SETNAME", "auth"]);
        assert!(redact_args(&mut after));
        assert_eq!(
            shown(&after),
            ["HELLO", "2", "AUTH", "default", REDACTED, "SETNAME", "auth"]
        );
        let mut plain = args(&["HELLO", "3", "SETNAME", "auth"]);
        assert!(!redact_args(&mut plain));

        let mut migrate = args(&[
            "MIGRATE", "h", "1", "", "0", "5000", "AUTH2", "u", "pw", "KEYS", "k",
        ]);
        assert!(redact_args(&mut migrate));
        assert_eq!(shown(&migrate)[8], REDACTED);
        assert_eq!(shown(&migrate)[7], "u");
        // A key named AUTH is a key, not the option.
        let mut key_auth = args(&[
            "MIGRATE", "h", "1", "AUTH", "0", "5000", "KEYS", "AUTH", "x",
        ]);
        assert!(!redact_args(&mut key_auth));

        let mut sentinel = args(&[
            "SENTINEL",
            "SET",
            "mymaster",
            "quorum",
            "2",
            "auth-pass",
            "pw",
        ]);
        assert!(redact_args(&mut sentinel));
        assert_eq!(shown(&sentinel)[6], REDACTED);
        assert_eq!(shown(&sentinel)[4], "2");
        let mut named = args(&["SENTINEL", "SET", "auth-pass", "quorum", "2"]);
        assert!(
            !redact_args(&mut named),
            "a primary named auth-pass is a name"
        );
        let mut config = args(&["SENTINEL", "CONFIG", "SET", "sentinel-pass", "pw"]);
        assert!(redact_args(&mut config));
        assert_eq!(shown(&config)[4], REDACTED);
    }

    #[test]
    fn leaves_other_commands_and_already_redacted_args() {
        let mut get = args(&["GET", "password"]);
        assert!(!redact_args(&mut get));
        let mut done = args(&["AUTH", REDACTED]);
        assert!(!redact_args(&mut done));
    }
}
