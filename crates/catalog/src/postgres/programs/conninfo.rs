//! libpq connection strings: a parser for both forms libpq accepts
//! (`keyword=value` pairs and `postgresql://` URIs, following libpq's
//! `conninfo_parse` and `conninfo_uri_parse_options`) and the removal of
//! every secret from them.
//!
//! A string that does not parse is never returned in any form: it might
//! hold a password where the parser did not expect one.

/// Every keyword libpq accepts in the supported versions (13 to 18). An
/// unknown keyword is a parse error, as it is for libpq.
const KEYWORDS: &[&str] = &[
    "application_name",
    "authtype",
    "channel_binding",
    "client_encoding",
    "connect_timeout",
    "dbname",
    "fallback_application_name",
    "gssdelegation",
    "gssencmode",
    "gsslib",
    "host",
    "hostaddr",
    "keepalives",
    "keepalives_count",
    "keepalives_idle",
    "keepalives_interval",
    "krbsrvname",
    "load_balance_hosts",
    "max_protocol_version",
    "min_protocol_version",
    "oauth_ca_file",
    "oauth_client_id",
    "oauth_client_secret",
    "oauth_issuer",
    "oauth_scope",
    "options",
    "passfile",
    "password",
    "port",
    "replication",
    "require_auth",
    "requiressl",
    "requirepeer",
    "scram_client_key",
    "scram_server_key",
    "service",
    "servicefile",
    "ssl_max_protocol_version",
    "ssl_min_protocol_version",
    "sslcert",
    "sslcertmode",
    "sslcompression",
    "sslcrl",
    "sslcrldir",
    "sslkey",
    "sslmode",
    "sslnegotiation",
    "sslpassword",
    "sslrootcert",
    "sslsni",
    "target_session_attrs",
    "tcp_user_timeout",
    "tty",
    "user",
];

/// Keywords whose values are secrets. `passfile`, `sslkey` and the other
/// file settings are paths and are kept.
const SECRET_KEYWORDS: &[&str] = &[
    "password",
    "sslpassword",
    "oauth_client_secret",
    "scram_client_key",
    "scram_server_key",
];

/// How deep `dbname` values that are connection strings themselves are
/// followed.
const MAX_NESTING: usize = 4;

/// The connection string without any secret, in `keyword=value` form.
/// `dbname` values that are connection strings (libpq expands them) are
/// cleaned the same way. `None` when the text is not a valid libpq
/// connection string.
#[must_use]
pub fn strip_passwords(conninfo: &str) -> Option<String> {
    clean(conninfo, 0)
}

fn clean(conninfo: &str, depth: usize) -> Option<String> {
    if depth > MAX_NESTING {
        return None;
    }
    let pairs = parse(conninfo)?;
    let mut kept = Vec::new();
    for (key, value) in pairs {
        if SECRET_KEYWORDS.contains(&key.as_str()) {
            continue;
        }
        let value = if key == "dbname" && is_connection_string(&value) {
            clean(&value, depth + 1)?
        } else {
            value
        };
        kept.push(format!("{key}={}", quote(&value)));
    }
    Some(kept.join(" "))
}

/// libpq's `recognized_connection_string`.
fn is_connection_string(text: &str) -> bool {
    uri_prefix_len(text).is_some() || text.contains('=')
}

fn uri_prefix_len(text: &str) -> Option<usize> {
    ["postgresql://", "postgres://"]
        .iter()
        .find(|prefix| text.starts_with(*prefix))
        .map(|prefix| prefix.len())
}

/// Parses either form into `(keyword, value)` pairs in order.
fn parse(conninfo: &str) -> Option<Vec<(String, String)>> {
    let pairs = match uri_prefix_len(conninfo) {
        Some(len) => parse_uri(&conninfo[len..])?,
        None => parse_keywords(conninfo)?,
    };
    pairs
        .iter()
        .all(|(key, value)| {
            KEYWORDS.contains(&key.as_str()) && (key != "port" || valid_ports(value))
        })
        .then_some(pairs)
}

/// White space as libpq sees it: C `isspace()` in the C locale, which
/// includes the vertical tab and form feed that
/// [`char::is_ascii_whitespace`] leaves out (a value after one of them
/// would otherwise be read as part of the previous value).
fn is_libpq_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r')
}

fn quote(value: &str) -> String {
    let plain = !value.is_empty()
        && !value
            .chars()
            .any(|c| is_libpq_space(c) || c == '\'' || c == '\\');
    if plain {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// `keyword = value` pairs separated by white space; values are either
/// single-quoted (`\` escapes any character) or run to the next white
/// space (`\` escapes any character there too).
fn parse_keywords(conninfo: &str) -> Option<Vec<(String, String)>> {
    let mut chars = conninfo.chars().peekable();
    let mut pairs = Vec::new();
    let skip_space = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
        while chars.peek().is_some_and(|&c| is_libpq_space(c)) {
            chars.next();
        }
    };
    loop {
        skip_space(&mut chars);
        if chars.peek().is_none() {
            return Some(pairs);
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || is_libpq_space(c) {
                break;
            }
            key.push(c);
            chars.next();
        }
        skip_space(&mut chars);
        if chars.next() != Some('=') {
            return None;
        }
        skip_space(&mut chars);
        let mut value = String::new();
        if chars.peek() == Some(&'\'') {
            chars.next();
            loop {
                match chars.next()? {
                    '\\' => value.push(chars.next()?),
                    '\'' => break,
                    c => value.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if is_libpq_space(c) {
                    break;
                }
                chars.next();
                if c == '\\' {
                    value.push(chars.next()?);
                } else {
                    value.push(c);
                }
            }
        }
        pairs.push((key, value));
    }
}

/// The part of a URI after `postgresql://`:
/// `[user[:password]@][host[:port][,...]][/dbname][?key=value[&...]]`.
fn parse_uri(rest: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    // libpq looks for the credentials designator before the first '/'
    // only; '?', ':' and anything else may be part of the password.
    let mut text = rest;
    if let Some(at) = rest
        .find(['@', '/'])
        .filter(|&i| rest.as_bytes()[i] == b'@')
    {
        let userinfo = &rest[..at];
        let (user, password) = match userinfo.find(':') {
            Some(colon) => (&userinfo[..colon], Some(&userinfo[colon + 1..])),
            None => (userinfo, None),
        };
        if !user.is_empty() {
            pairs.push(("user".to_owned(), decode(user)?));
        }
        if let Some(password) = password {
            pairs.push(("password".to_owned(), decode(password)?));
        }
        text = &rest[at + 1..];
    }
    // An `@` after the credentials designator means the credentials went
    // unrecognized (a password with a `/` or `@` in it): parts of the
    // password would show as host, port or database name.
    if text.contains('@') {
        return None;
    }
    let mut hosts = Vec::new();
    let mut ports = Vec::new();
    let mut any_port = false;
    loop {
        let host;
        if let Some(inner) = text.strip_prefix('[') {
            let end = inner.find(']')?;
            host = &inner[..end];
            text = &inner[end + 1..];
            if !(text.is_empty() || text.starts_with([':', '/', '?', ','])) {
                return None;
            }
        } else {
            let end = text.find([':', '/', '?', ',']).unwrap_or(text.len());
            host = &text[..end];
            text = &text[end..];
        }
        hosts.push(decode(host)?);
        if let Some(after) = text.strip_prefix(':') {
            let end = after.find(['/', '?', ',']).unwrap_or(after.len());
            ports.push(decode(&after[..end])?);
            any_port = true;
            text = &after[end..];
        } else {
            ports.push(String::new());
        }
        match text.strip_prefix(',') {
            Some(after) => text = after,
            None => break,
        }
    }
    if hosts.iter().any(|h| !h.is_empty()) || hosts.len() > 1 {
        pairs.push(("host".to_owned(), hosts.join(",")));
    }
    if any_port {
        pairs.push(("port".to_owned(), ports.join(",")));
    }
    if let Some(after) = text.strip_prefix('/') {
        let end = after.find('?').unwrap_or(after.len());
        let dbname = decode(&after[..end])?;
        if !dbname.is_empty() {
            pairs.push(("dbname".to_owned(), dbname));
        }
        text = &after[end..];
    }
    if let Some(query) = text.strip_prefix('?') {
        for (key, value) in uri_params(query)? {
            let key = decode(key)?;
            let value = decode(value)?;
            // libpq maps the JDBC-style ssl=true to sslmode=require.
            if key == "ssl" && value == "true" {
                pairs.push(("sslmode".to_owned(), "require".to_owned()));
            } else {
                pairs.push((key, value));
            }
        }
    } else if !text.is_empty() {
        return None;
    }
    Some(pairs)
}

/// The `key=value` parameters of a URI query string, split the way libpq's
/// `conninfo_uri_parse_params` does: each parameter runs to the next `&`
/// or the end and needs exactly one `=`. An empty query (`?` alone) has no
/// parameters, and one `&` may end the string, but an empty parameter
/// anywhere else (`?&a=b`, `?a=b&&c=d`, `?&`) is an error, as it is for
/// libpq.
fn uri_params(query: &str) -> Option<Vec<(&str, &str)>> {
    let mut params = Vec::new();
    let mut rest = query;
    while !rest.is_empty() {
        let (param, next) = rest.split_once('&').unwrap_or((rest, ""));
        let (key, value) = param.split_once('=')?;
        if value.contains('=') {
            return None;
        }
        params.push((key, value));
        rest = next;
    }
    Some(params)
}

/// True for a `port` value libpq can use: empty (the default) or a number
/// from 1 to 65535, per host of a comma-separated list. Anything else
/// fails when libpq connects, and in a URI it is most likely the start of
/// a password that holds a `/` (`postgresql://user:pass/word@host` reads
/// as host `user`, port `pass`), so a string with such a port is refused
/// like one that does not parse.
fn valid_ports(value: &str) -> bool {
    value.split(',').all(|port| {
        let port = port.trim_matches(|c: char| is_libpq_space(c));
        port.is_empty()
            || (port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n)))
    })
}

/// Percent-decoding as libpq does it: `%` must be followed by two hex
/// digits and `%00` is refused.
fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            if byte == 0 {
                return None;
            }
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_strings_lose_their_secrets() {
        assert_eq!(
            strip_passwords("host=db port=5432 password=hunter2 user=app").as_deref(),
            Some("host=db port=5432 user=app")
        );
        assert_eq!(
            strip_passwords(
                "host = 'my db' password = 'it\\'s secret' sslpassword=x dbname=a\\ b \
                 sslkey=/k passfile=/p"
            )
            .as_deref(),
            Some("host='my db' dbname='a b' sslkey=/k passfile=/p")
        );
        assert_eq!(strip_passwords("").as_deref(), Some(""));
    }

    #[test]
    fn every_libpq_space_separates_keywords() {
        for space in [' ', '\t', '\n', '\x0B', '\x0C', '\r'] {
            let conninfo = format!("port=1{space}password=SECRET{space}host=h");
            assert_eq!(
                strip_passwords(&conninfo).as_deref(),
                Some("port=1 host=h"),
                "separator {space:?}"
            );
            let quoted = format!("application_name='a{space}b'");
            let cleaned = strip_passwords(&quoted);
            assert_eq!(
                cleaned.as_deref(),
                Some(format!("application_name='a{space}b'").as_str()),
                "values with {space:?} stay quoted"
            );
            assert_eq!(cleaned.as_deref().and_then(strip_passwords), cleaned);
        }
    }

    #[test]
    fn unparsable_strings_are_never_returned() {
        assert_eq!(strip_passwords("host=db password='open"), None);
        assert_eq!(strip_passwords("host"), None);
        assert_eq!(strip_passwords("host=db pasword=typo"), None);
        assert_eq!(strip_passwords("postgresql://h/db?password"), None);
        assert_eq!(strip_passwords("postgresql://u:p%zz@h"), None);
        assert_eq!(strip_passwords("postgresql://[::1"), None);
    }

    #[test]
    fn uris_follow_libpq() {
        assert_eq!(
            strip_passwords("postgresql://repl:pa?SECRETQ@h:1/db").as_deref(),
            Some("user=repl host=h port=1 dbname=db")
        );
        assert_eq!(
            strip_passwords("postgresql://app:hunter2@db:5432/main?sslmode=require&password=x")
                .as_deref(),
            Some("user=app host=db port=5432 dbname=main sslmode=require")
        );
        assert_eq!(
            strip_passwords("postgres://db/main?pass%77ord=x&sslpassword=y").as_deref(),
            Some("host=db dbname=main")
        );
        assert_eq!(
            strip_passwords("postgresql://a%40b:s%3Acret@h1:1,[::1]:2,h3/d%20b?ssl=true")
                .as_deref(),
            Some("user=a@b host=h1,::1,h3 port=1,2, dbname='d b' sslmode=require")
        );
        assert_eq!(strip_passwords("postgresql://").as_deref(), Some(""));
        assert_eq!(
            strip_passwords("postgresql://h/db?").as_deref(),
            Some("host=h dbname=db")
        );
        assert_eq!(
            strip_passwords("postgresql://h/db?sslmode=require&").as_deref(),
            Some("host=h dbname=db sslmode=require")
        );
        assert_eq!(
            strip_passwords("postgresql://h/db?application_name=&password=x").as_deref(),
            Some("host=h dbname=db application_name=''")
        );
    }

    #[test]
    fn uri_query_strings_libpq_refuses_are_refused() {
        // libpq: "missing key/value separator" for an empty parameter
        // anywhere but at the very end, "extra key/value separator" for a
        // second `=`, "invalid URI query parameter" for an empty key.
        for conninfo in [
            "postgresql://h/db?&password=SECRET",
            "postgresql://h/db?sslmode=require&&password=SECRET",
            "postgresql://u:SECRET@h/db?&",
            "postgresql://h/db?password",
            "postgresql://h/db?password=a=b",
            "postgresql://h/db?=SECRET",
        ] {
            assert_eq!(strip_passwords(conninfo), None, "{conninfo}");
        }
    }

    #[test]
    fn misplaced_credentials_are_never_shown() {
        // libpq reads these as host "u", port "SECRET" and database
        // "x@h/db" (the credentials designator is only looked for before
        // the first `/`); parts of the password must not come out.
        for conninfo in [
            "postgresql://u:SECRET/x@h/db",
            "postgresql://u:12/SECRET@h/db",
            "postgresql://h/db?application_name=SECRET@x",
            "host=h port=SECRET",
            "host=h,h2 port=1,SECRET",
            "host=h port=65536",
            "host=h port=0",
        ] {
            assert_eq!(strip_passwords(conninfo), None, "{conninfo}");
        }
        assert_eq!(
            strip_passwords("host=h,h2 port=5432,").as_deref(),
            Some("host=h,h2 port=5432,")
        );
        assert_eq!(
            strip_passwords("postgresql://h:/db").as_deref(),
            Some("host=h port='' dbname=db")
        );
    }

    #[test]
    fn nested_dbname_strings_are_cleaned() {
        assert_eq!(
            strip_passwords("host=h dbname='postgresql://u:SECRET@h/db'").as_deref(),
            Some("host=h dbname='user=u host=h dbname=db'")
        );
        assert_eq!(
            strip_passwords("dbname='host=x password=SECRET dbname=y'").as_deref(),
            Some("dbname='host=x dbname=y'")
        );
        assert_eq!(strip_passwords("dbname='host=x stray'"), None);
        let cleaned = strip_passwords("host=h dbname='postgresql://u:SECRET@h/db'");
        assert_eq!(
            cleaned.as_deref().and_then(strip_passwords),
            cleaned,
            "cleaning is idempotent"
        );
    }
}
