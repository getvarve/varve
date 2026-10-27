//! Credential scrubbing for free text: passwords in URLs
//! (`postgres://user:pass@host`) and in key/value strings
//! (`host=db password=secret`, `"password":"secret"`).
//!
//! These functions work on plain text. The SQL and Redis redactors call them
//! on the content of single literals, so a replacement can never change where
//! a literal starts or ends.

use std::borrow::Cow;

use super::REDACTED;
use super::fuzzy::{like_password, within};

/// Where a URL authority ends when scanning for `user:pass@`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UrlScope {
    /// Inside one literal or a plain string: only whitespace ends the
    /// authority, so a password containing quotes is still caught.
    Content,
    /// In statement code: quotes end the authority too, so a replacement
    /// never removes a quote that opens or closes a literal.
    Code,
}

/// Byte ranges of secrets to replace with [`REDACTED`].
pub(crate) type Ranges = Vec<(usize, usize)>;

/// Replaces `ranges` (possibly overlapping, in any order) in `text` with
/// [`REDACTED`] and appends every removed fragment to `removed`. A range
/// that already holds [`REDACTED`] is kept as it is and not reported.
pub(crate) fn splice<'a>(
    text: &'a str,
    mut ranges: Ranges,
    removed: &mut Vec<String>,
) -> Cow<'a, str> {
    ranges.retain(|(start, end)| start < end);
    if ranges.is_empty() {
        return Cow::Borrowed(text);
    }
    ranges.sort_unstable();
    let mut merged: Ranges = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, end) in merged {
        out.push_str(&text[copied..start]);
        let fragment = &text[start..end];
        if fragment != REDACTED {
            removed.push(fragment.to_string());
        }
        out.push_str(REDACTED);
        copied = end;
    }
    out.push_str(&text[copied..]);
    if out == text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(out)
    }
}

/// Redacts URL passwords and key/value secrets in `text`.
pub(crate) fn redact_credentials(text: &str) -> Cow<'_, str> {
    splice(text, credential_ranges(text), &mut Vec::new())
}

/// How many levels of backslash escaping are undone to find a key or a
/// header (`\n` before a header, JSON inside JSON inside a SQL string).
const MAX_UNESCAPE: usize = 3;

/// The ranges [`redact_credentials`] replaces: URL passwords and secret
/// query parameters, the values of secret keys, credential headers. The
/// patterns run on the text as written and on it with up to
/// [`MAX_UNESCAPE`] levels of backslash escapes undone, so `\n` before a
/// key and JSON escaped inside a string do not hide a secret.
pub(crate) fn credential_ranges(text: &str) -> Ranges {
    let mut ranges = plain_credential_ranges(text);
    let mut current: Option<(String, Vec<usize>)> = None;
    for _ in 0..MAX_UNESCAPE {
        let source = current.as_ref().map_or(text, |(s, _)| s.as_str());
        if !source.contains('\\') {
            break;
        }
        let (unescaped, map) = unescape(source);
        if unescaped == source {
            break;
        }
        // Offsets into the original text.
        let map: Vec<usize> = match &current {
            Some((_, outer)) => map.iter().map(|&i| outer[i]).collect(),
            None => map,
        };
        for (start, end) in plain_credential_ranges(&unescaped) {
            if let (Some(&a), Some(&b)) = (map.get(start), map.get(end)) {
                ranges.push((a, b.max(a)));
            }
        }
        current = Some((unescaped, map));
    }
    ranges
}

fn plain_credential_ranges(text: &str) -> Ranges {
    let mut ranges = url_ranges(text, UrlScope::Content);
    ranges.extend(key_value_ranges(text));
    ranges.extend(conninfo_bare_ranges(text));
    ranges.extend(header_ranges(text));
    ranges.extend(bearer_ranges(text));
    ranges.extend(spaced_key_ranges(text));
    ranges
}

/// Longest quoted key of several words read by [`spaced_key_ranges`].
const MAX_SPACED_KEY: usize = 64;

/// JSON and YAML keys of several words in quotes (`"Password Hash": "x"`,
/// `'db password': x`): the value of a key with a secret word. The key
/// position (quotes, then `:`) says it is a key, so any word counts.
fn spaced_key_ranges(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let quote = bytes[i];
        if !matches!(quote, b'"' | b'\'') {
            i += 1;
            continue;
        }
        let start = i + 1;
        let Some(len) = bytes[start..]
            .iter()
            .take(MAX_SPACED_KEY + 1)
            .position(|&b| b == quote || b == b'\n')
        else {
            i = start;
            continue;
        };
        let end = start + len;
        let key = &text[start..end];
        i = start;
        if len > MAX_SPACED_KEY || bytes[end] != quote || !key.contains(' ') {
            continue;
        }
        let colon = skip_spaces(bytes, end + 1);
        if bytes.get(colon) != Some(&b':') || !is_secret_key(key.trim()) || descriptive_key(key) {
            continue;
        }
        let value = skip_spaces(bytes, colon + 1);
        let (value_start, value_end) = match bytes.get(value) {
            Some(&q @ (b'"' | b'\'')) => {
                let marker = if q == b'"' { "\"" } else { "'" };
                (value + 1, quoted_value_end(bytes, value + 1, marker))
            }
            Some(_) => (value, unquoted_value_end(bytes, value)),
            None => continue,
        };
        if value_end > value_start && !text[value_start..].starts_with(REDACTED) {
            ranges.push((value_start, value_end));
            i = value_end;
        }
    }
    ranges
}

/// A bearer token written without a header name (`'Bearer abc.def'`):
/// the token after `Bearer` when it looks like a secret.
fn bearer_ranges(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut search = 0;
    while let Some(found) = find_ascii_case_insensitive(&text[search..], "bearer ") {
        let at = search + found;
        search = at + 7;
        if at > 0 && is_key_char(bytes[at - 1]) {
            continue;
        }
        let token = skip_spaces(bytes, at + 7);
        let end = unquoted_value_end(bytes, token);
        if end > token && secret_shaped(&text[token..end]) && !text[token..].starts_with(REDACTED) {
            ranges.push((token, end));
            search = end;
        }
    }
    ranges
}

/// The first offset of `needle` (ASCII, lower case) in `haystack`,
/// ignoring ASCII case.
fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// The value of up to `max` hex digits at the start of `bytes`, and how
/// many were read.
fn hex_prefix(bytes: &[u8], max: usize) -> (u32, usize) {
    let mut value = 0u32;
    let mut count = 0;
    for &b in bytes.iter().take(max) {
        let Some(digit) = (b as char).to_digit(16) else {
            break;
        };
        value = value * 16 + digit;
        count += 1;
    }
    (value, count)
}

/// The character an escape sequence at `bytes[0] == b'\\'` stands for, and
/// the length of the sequence: `\n`, `\r`, `\t`, `\\`, `\"`, `\'`, `\/`,
/// `\b`, `\f`, MySQL `\0` and `\Z`, `\xHH`, octal `\NNN`, `\uXXXX` and
/// `\UXXXXXXXX`. A code that is no character, or a byte escape above
/// ASCII (which alone is no UTF-8), is not undone.
fn escape_at(bytes: &[u8]) -> Option<(char, usize)> {
    let next = *bytes.get(1)?;
    let simple = match next {
        b'n' => Some('\n'),
        b'r' => Some('\r'),
        b't' => Some('\t'),
        b'b' => Some('\u{8}'),
        b'f' => Some('\u{c}'),
        b'Z' => Some('\u{1a}'),
        b'\\' | b'"' | b'\'' | b'/' => Some(char::from(next)),
        _ => None,
    };
    if let Some(c) = simple {
        return Some((c, 2));
    }
    let (code, len) = match next {
        b'x' => {
            let (value, count) = hex_prefix(&bytes[2..], 2);
            (value, 2 + count)
        }
        b'u' => {
            let (value, count) = hex_prefix(&bytes[2..], 4);
            if count < 4 {
                return None;
            }
            (value, 6)
        }
        b'U' => {
            let (value, count) = hex_prefix(&bytes[2..], 8);
            if count < 8 {
                return None;
            }
            (value, 10)
        }
        b'0'..=b'7' => {
            let mut value = 0u32;
            let mut count = 0;
            for &b in bytes[1..].iter().take(3) {
                if !(b'0'..=b'7').contains(&b) {
                    break;
                }
                value = value * 8 + u32::from(b - b'0');
                count += 1;
            }
            (value, 1 + count)
        }
        _ => return None,
    };
    if (next == b'x' && len == 2) || (matches!(next, b'x' | b'0'..=b'7') && code > 0x7f) {
        return None;
    }
    char::from_u32(code).map(|c| (c, len))
}

/// Undoes one level of backslash escapes ([`escape_at`]), with a map from
/// every byte of the result to its offset in `text` (one entry more, for
/// the end). Linear in the length of `text`.
pub(crate) fn unescape(text: &str) -> (String, Vec<usize>) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut map = Vec::with_capacity(bytes.len() + 1);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && let Some((c, len)) = escape_at(&bytes[i..])
        {
            for _ in 0..c.len_utf8() {
                map.push(i);
            }
            out.push(c);
            i += len;
        } else {
            let len = char_len(bytes[i]).min(bytes.len() - i);
            for k in 0..len {
                map.push(i + k);
            }
            out.push_str(&text[i..i + len]);
            i += len;
        }
    }
    map.push(bytes.len());
    (out, map)
}

/// Keys that mark text as a libpq-style connection string.
const CONNINFO_KEYS: [&str; 9] = [
    "host",
    "hostaddr",
    "port",
    "dbname",
    "user",
    "sslmode",
    "service",
    "application_name",
    "connect_timeout",
];

/// True when `text` holds a `key=value` pair with a libpq key.
fn looks_like_conninfo(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !is_key_char(bytes[i]) || (i > 0 && is_key_char(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_key_char(bytes[i]) {
            i += 1;
        }
        let key = text[start..i].to_ascii_lowercase();
        if CONNINFO_KEYS.contains(&key.as_str()) && bytes.get(skip_spaces(bytes, i)) == Some(&b'=')
        {
            return true;
        }
    }
    false
}

/// In a connection string, a secret key written without `=`
/// (`host=db password hunter2`, which libpq rejects and the server echoes)
/// is still followed by the secret: the next word is redacted.
fn conninfo_bare_ranges(text: &str) -> Ranges {
    if !looks_like_conninfo(text) {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_key_char(bytes[i]) || (i > 0 && is_key_char(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_key_char(bytes[i]) {
            i += 1;
        }
        if !is_secret_text_key(&text[start..i]) {
            continue;
        }
        let j = skip_spaces(bytes, i);
        if j == i || j >= bytes.len() || bytes[j] == b'=' {
            continue;
        }
        let end = unquoted_value_end(bytes, j);
        // A function word is prose, not a value (`password for the app`).
        let prose = NOT_COMMENT_VALUES
            .iter()
            .any(|w| w.eq_ignore_ascii_case(&text[j..end]));
        if end > j && !prose && !text[j..].starts_with(REDACTED) {
            ranges.push((j, end));
        }
        i = end.max(i);
    }
    ranges
}

fn is_scheme_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-')
}

fn ends_authority(b: u8, scope: UrlScope) -> bool {
    b.is_ascii_whitespace() || (scope == UrlScope::Code && matches!(b, b'\'' | b'"' | b'`'))
}

/// Query parameters that hold a credential in URLs only (`?key=`, a SAS
/// `sig=`, presigned URL signatures); every secret key name counts too.
const URL_SECRET_PARAMETERS: &[&str] = &[
    "key",
    "sig",
    "signature",
    "x-amz-signature",
    "x-amz-security-token",
    "x-amz-credential",
    "x-goog-signature",
    "x-goog-credential",
];

/// The password of every `scheme://user:password@host` in `text`, and the
/// values of secret query parameters.
pub(crate) fn url_ranges(text: &str, scope: UrlScope) -> Ranges {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut search = 0;
    while let Some(rel) = text[search..].find("://") {
        let sep = search + rel;
        let authority = sep + 3;
        search = authority;
        let mut scheme_start = sep;
        while scheme_start > 0 && is_scheme_char(bytes[scheme_start - 1]) {
            scheme_start -= 1;
        }
        if scheme_start == sep || !bytes[scheme_start].is_ascii_alphabetic() {
            continue;
        }
        let mut segment_end = authority;
        while segment_end < bytes.len() && !ends_authority(bytes[segment_end], scope) {
            segment_end += 1;
        }
        query_ranges(text, authority, segment_end, &mut ranges);
        // The last `@` ends the user info, so passwords containing `@` or `/`
        // are covered. A later `@` in the path only widens the redaction.
        let Some(at) = text[authority..segment_end]
            .rfind('@')
            .map(|p| authority + p)
        else {
            search = segment_end;
            continue;
        };
        if let Some(colon) = text[authority..at].find(':').map(|p| authority + p) {
            let start = colon + 1;
            if start < at && !text[start..].starts_with(REDACTED) {
                ranges.push((start, at));
            }
        }
        search = segment_end.max(authority);
    }
    ranges
}

/// Secret parameters in the query of the URL between `start` and `end`
/// (`name=value` or the percent-encoded `name%3Dvalue`).
fn query_ranges(text: &str, start: usize, end: usize, ranges: &mut Ranges) {
    let Some(question) = text[start..end].find('?').map(|p| start + p) else {
        return;
    };
    let mut at = question + 1;
    while at < end {
        let next = text[at..end].find(['&', '#', ';']).map_or(end, |p| at + p);
        let parameter = &text[at..next];
        let split = parameter
            .find('=')
            .map(|p| (p, 1))
            .or_else(|| parameter.to_ascii_lowercase().find("%3d").map(|p| (p, 3)));
        if let Some((p, len)) = split {
            let name = parameter[..p].to_ascii_lowercase();
            let value = at + p + len;
            if (URL_SECRET_PARAMETERS.contains(&name.as_str()) || is_secret_text_key(&name))
                && value < next
                && !text[value..].starts_with(REDACTED)
            {
                ranges.push((value, next));
            }
        }
        at = next + 1;
    }
}

/// Key names that hold a credential, compared as a whole without `-`,
/// `_`, `.` and spaces.
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "pw",
    "passphrase",
    "requirepass",
    "masterauth",
    "rediscliauth",
    "secret",
    "token",
    "sas",
    "sig",
    "authorization",
    "proxyauthorization",
    "cookie",
    "setcookie",
];

/// Endings of compound key names that hold a credential (`api_key`,
/// `X-Api-Key`, `clientSecret`, `refresh_token`, `aes_key`, ...).
const SECRET_KEY_ENDINGS: &[&str] = &[
    "apikey",
    "accesskey",
    "secretkey",
    "privatekey",
    "encryptionkey",
    "signingkey",
    "masterkey",
    "aeskey",
    "subscriptionkey",
    "clientsecret",
    "apisecret",
    "appsecret",
    "sharedsecret",
    "accesstoken",
    "refreshtoken",
    "authtoken",
    "apitoken",
    "bearertoken",
    "sessiontoken",
    "idtoken",
    "privatetoken",
    "securitytoken",
];

/// True for key names that hold a secret in connection strings, config
/// strings and JSON. Names are matched as whole tokens: `password`,
/// `db_password`, `sslpassword`, `pwd`, `requirepass`, `auth-pass`,
/// `api_key`, `clientSecret`, `refresh_token`, `otp_secret`,
/// `remember_token`, `two_factor_recovery_codes` count; `pass`, `bypass`,
/// `compass`, `is_secret`, `max_token`, `next_page_token`, `api_key_id`
/// do not.
pub(crate) fn is_secret_key(key: &str) -> bool {
    let parts = key_parts(key);
    if flag_key(&parts) {
        return false;
    }
    let mut parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    // A version or encoding suffix says nothing about the value
    // (`jwt_secret_v2`, `privateKeyPem`).
    while parts.len() > 1
        && parts.last().is_some_and(|last| {
            matches!(*last, "pem" | "der" | "b64" | "base64" | "hex" | "enc")
                || last.bytes().all(|b| b.is_ascii_digit())
                || (last.len() > 1
                    && last.starts_with('v')
                    && last[1..].bytes().all(|b| b.is_ascii_digit()))
        })
    {
        parts.pop();
    }
    let compact: String = parts.concat();
    // Words that hold `password` or `token` but name something else.
    if parts
        .iter()
        .any(|p| matches!(*p, "passport" | "passports" | "passwordless"))
    {
        return false;
    }
    SECRET_KEYS.contains(&compact.as_str())
        || parts
            .iter()
            .any(|p| p.contains("password") || p.contains("passwd") || *p == "passphrase")
        || (parts.len() >= 2 && matches!(parts.last(), Some(&("pass" | "pwd" | "pw"))))
        // `otp_secret`, `remember_token`, `confirmation_token`,
        // `unlock_token`: a compound ending in `secret` or `token`, unless
        // it names a limit, a cursor, a parser token or a public thing
        // (`max_token`, `next_page_token`, `syntax_token`,
        // `cancellation_token`, `top_secret`, `trade_secret`, `utf8_token`).
        || (parts.len() >= 2
            && matches!(parts.last(), Some(&("secret" | "token")))
            && !parts[..parts.len() - 1].iter().any(|p| {
                matches!(
                    *p,
                    "max"
                        | "min"
                        | "num"
                        | "total"
                        | "next"
                        | "page"
                        | "prev"
                        | "previous"
                        | "first"
                        | "last"
                        | "current"
                        | "start"
                        | "end"
                        | "eof"
                        | "syntax"
                        | "lexer"
                        | "parser"
                        | "cancellation"
                        | "cancel"
                        | "continuation"
                        | "sync"
                        | "word"
                        | "top"
                        | "trade"
                        // A text unit: `utf8_token`, `byte_token`.
                        | "utf8"
                        | "utf16"
                        | "unicode"
                        | "ascii"
                        | "byte"
                        | "char"
                        | "bpe"
                )
            }))
        // `token_hash`, `secret_key_base`, recovery and backup codes (not
        // a disaster recovery code), security answers.
        || ([
            "tokenhash",
            "secrethash",
            "secretkeybase",
            "recoverycode",
            "recoverycodes",
            "backupcode",
            "backupcodes",
            "secretanswer",
            "securityanswer",
        ]
        .iter()
        .any(|ending| compact.ends_with(ending))
            && !parts.contains(&"disaster"))
        || SECRET_KEY_ENDINGS
            .iter()
            .any(|ending| compact.ends_with(ending))
}

/// The lower-case parts of a key name, split at `_`, `-`, `.`, spaces and
/// camelCase humps (`rememberToken` -> `remember`, `token`; `APIKey` ->
/// `api`, `key`) of a piece without digits.
pub(crate) fn key_parts(key: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for piece in key
        .split(['_', '-', '.', ' ', ':'])
        .filter(|p| !p.is_empty())
    {
        let chars: Vec<char> = piece.chars().collect();
        let mut humps = Vec::new();
        let mut current = String::new();
        for (i, &c) in chars.iter().enumerate() {
            let hump = i > 0
                && c.is_ascii_uppercase()
                && (chars[i - 1].is_ascii_lowercase()
                    || chars[i - 1].is_ascii_digit()
                    || chars.get(i + 1).is_some_and(char::is_ascii_lowercase)
                        && chars[i - 1].is_ascii_uppercase());
            if hump && !current.is_empty() {
                humps.push(std::mem::take(&mut current));
            }
            current.push(c.to_ascii_lowercase());
        }
        if !current.is_empty() {
            humps.push(current);
        }
        // Humps that read as words with an optional number (`oauth2Token`,
        // `apiV2Token`) are parts; a piece mixing digits into its letters
        // (`k3yZq9wSecret`) is a value and stays whole.
        if humps.iter().all(|hump| name_like(hump)) {
            parts.extend(humps);
        } else {
            parts.push(piece.to_ascii_lowercase());
        }
    }
    parts
}

/// True for a word of a name: letters with an optional number after them
/// (`token`, `oauth2`, `v2`), or a number.
fn name_like(part: &str) -> bool {
    let letters = part.bytes().take_while(u8::is_ascii_alphabetic).count();
    part.bytes().skip(letters).all(|b| b.is_ascii_digit())
}

/// True for a key that is a yes/no flag about a secret, not the secret:
/// `is_secret`, `has_password`, `password_required`, `token_enabled`.
pub(crate) fn flag_key(parts: &[String]) -> bool {
    parts.len() >= 2
        && (matches!(
            parts.first().map(String::as_str),
            Some("is" | "has" | "no" | "show" | "with" | "use" | "needs" | "requires")
        ) || matches!(
            parts.last().map(String::as_str),
            Some(
                "required"
                    | "enabled"
                    | "disabled"
                    | "set"
                    | "present"
                    | "exists"
                    | "expired"
                    | "valid"
            )
        ))
}

/// Last parts of a name that make it a count, time or size about a secret
/// rather than the secret (`password_reset_at`, `password_history`,
/// `token_expires`, `login_attempts`).
const METADATA_ENDINGS: &[&str] = &[
    "age",
    "at",
    "attempts",
    "count",
    "counter",
    "date",
    "days",
    "errors",
    "expiration",
    "expires",
    "expiry",
    "failures",
    "history",
    "hours",
    "interval",
    "len",
    "length",
    "lifetime",
    "minutes",
    "on",
    "policy",
    "rounds",
    "seconds",
    "time",
    "timestamp",
    "tries",
    "ts",
    "version",
];

/// True for a name whose value is a number about a secret, not the secret
/// (`user:1:password_reset_at`, `password_history`): its last part is a
/// count, time or size. Only a plain number is kept for such a name.
pub(crate) fn metadata_key(key: &str) -> bool {
    key_parts(key).last().is_some_and(|last| {
        METADATA_ENDINGS
            .iter()
            .any(|ending| ending.eq_ignore_ascii_case(last))
    })
}

/// Words that make a key of several words a description of a secret
/// rather than the secret (`'password policy'`, `'password changed at'`,
/// `'password length'`). Lower case.
const DESCRIPTIVE_WORDS: &[&str] = &[
    "age",
    "at",
    "attempts",
    "changed",
    "count",
    "expires",
    "expiration",
    "expiry",
    "hint",
    "history",
    "length",
    "lifetime",
    "max",
    "maximum",
    "min",
    "minimum",
    "policy",
    "rules",
    "strength",
    "updated",
];

/// True for a key of several words that only describes a secret: one of
/// its words is a [`DESCRIPTIVE_WORDS`] word.
pub(crate) fn descriptive_key(key: &str) -> bool {
    key.contains(' ')
        && key
            .split([' ', '_', '-', '.'])
            .any(|w| DESCRIPTIVE_WORDS.iter().any(|d| d.eq_ignore_ascii_case(w)))
}

/// True for a plain unsigned number (`5`, `1700000000`).
pub(crate) fn plain_number(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// Like [`is_secret_key`], for a key written in text (`pass=x` in a
/// connection string or query): a bare `pass` counts there, where it is a
/// password key; as a SQL column name it does not.
fn is_secret_text_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("pass") || is_secret_key(key)
}

/// Keys whose value may start with an authentication scheme that is kept.
fn is_authorization_key(key: &str) -> bool {
    let compact: String = key
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .collect();
    compact.ends_with("authorization")
}

/// HTTP authentication schemes, kept in front of the credential.
const AUTH_SCHEMES: &[&str] = &[
    "basic",
    "bearer",
    "digest",
    "token",
    "negotiate",
    "ntlm",
    "hawk",
    "oauth",
    "apikey",
    "sharedkey",
    "aws4-hmac-sha256",
];

/// The end of an authentication scheme word at `start` followed by a
/// space, or `None`.
fn scheme_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut end = start;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-') {
        end += 1;
    }
    let word = std::str::from_utf8(&bytes[start..end])
        .ok()?
        .to_ascii_lowercase();
    (AUTH_SCHEMES.contains(&word.as_str()) && bytes.get(end) == Some(&b' ')).then_some(end)
}

/// True when an unquoted word looks like a secret rather than prose: it
/// holds a digit, a symbol, mixed case, or is long.
pub(crate) fn secret_shaped(word: &str) -> bool {
    let word = word.trim_end_matches(['.', ',', ';', ':', ')']);
    !word.is_empty()
        && (word.bytes().any(|b| b.is_ascii_digit())
            || word.bytes().any(|b| b"!@#$%^&*+=?~/|<>[]{}".contains(&b))
            || (word.bytes().any(|b| b.is_ascii_lowercase())
                && word.bytes().skip(1).any(|b| b.is_ascii_uppercase()))
            || word.len() >= 20)
}

/// True for an HTTP header that carries a credential: `Authorization`,
/// `Proxy-Authorization`, `Cookie`, and API key or token headers
/// (`X-Api-Key`, `X-Auth-Token`, `PRIVATE-TOKEN`,
/// `Ocp-Apim-Subscription-Key`).
fn is_secret_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('-').filter(|p| !p.is_empty()).collect();
    is_secret_key(&lower)
        || (parts.len() >= 2
            && matches!(
                parts.last(),
                Some(&("token" | "key" | "secret" | "signature" | "credential"))
            ))
}

/// True for `Cookie`-like headers whose whole value is secret.
fn is_cookie_header(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "cookie" | "set-cookie")
}

/// End of a header value at `start`: the end of the line, an unescaped
/// quote, or an escaped line break (`\r`, `\n` written as two characters).
fn header_value_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' | b'\n' | b'"' => break,
            b'\\' if matches!(bytes.get(i + 1), Some(b'r' | b'n')) => break,
            b'\\' if i + 1 < bytes.len() => i += 1 + char_len(bytes[i + 1]),
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => i += 2,
            b'\'' => break,
            _ => i += 1,
        }
    }
    i.min(bytes.len())
}

/// True when a header name can start at `i`: the start of the text or a
/// line, or after a space, quote, bracket or separator, or an escaped line
/// break.
fn header_start(bytes: &[u8], i: usize) -> bool {
    i == 0
        || matches!(
            bytes[i - 1],
            b' ' | b'\t' | b'\n' | b'\r' | b'\'' | b'"' | b'{' | b'[' | b',' | b'(' | b';'
        )
        || (i >= 2 && bytes[i - 2] == b'\\' && matches!(bytes[i - 1], b'n' | b'r' | b't'))
}

/// True when position `i` starts a token: the start of the text, after a
/// character that cannot continue a name, or after an escaped line break.
fn token_start(bytes: &[u8], i: usize) -> bool {
    i == 0
        || !is_key_char(bytes[i - 1])
        || (i >= 2 && bytes[i - 2] == b'\\' && matches!(bytes[i - 1], b'n' | b'r' | b't'))
}

/// Values of credential headers written inside text
/// (`Authorization: Bearer abc`, `X-Api-Key: abc`, `Cookie: s=abc`). The
/// scheme of `Authorization` (`Bearer`, `Basic`, ...) is kept. Without a
/// scheme, the value must look like a secret, so a sentence such as
/// `Authorization: required for admins` stays.
fn header_ranges(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_key_char(bytes[i]) || !token_start(bytes, i) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_key_char(bytes[i]) {
            i += 1;
        }
        let name = &text[start..i];
        if bytes.get(i) != Some(&b':') || !is_secret_header(name) || !header_start(bytes, start) {
            continue;
        }
        // Without a space after the colon only a real header name counts
        // (`Authorization:Bearer x`), not a key path such as
        // `session:token:42`.
        let spaced = bytes.get(i + 1).is_some_and(|b| *b == b' ' || *b == b'\t');
        if !spaced && !(name.contains('-') || is_authorization_key(name) || is_cookie_header(name))
        {
            continue;
        }
        let value = skip_spaces(bytes, i + 1);
        if value >= bytes.len() || matches!(bytes[value], b'\r' | b'\n') {
            continue;
        }
        let (value_start, value_end) = if is_cookie_header(name) {
            (value, header_value_end(bytes, value))
        } else if let Some(end) = scheme_end(bytes, value) {
            let token = skip_spaces(bytes, end);
            (token, header_value_end(bytes, token))
        } else {
            let end = unquoted_value_end(bytes, value);
            if !secret_shaped(&text[value..end]) {
                continue;
            }
            (value, end)
        };
        let trimmed = text[value_start..value_end].trim_end().len();
        if trimmed > 0 && !text[value_start..].starts_with(REDACTED) {
            ranges.push((value_start, value_start + trimmed));
        }
        i = value_end.max(i);
    }
    ranges
}

fn is_key_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Quote markers a value can start with, longest first: SQL-doubled quotes
/// and backslash-escaped quotes appear when the key/value string sits inside
/// a SQL literal or a JSON string.
const QUOTE_MARKERS: [&str; 5] = ["\\\"", "\\'", "''", "\"", "'"];

fn skip_spaces(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}

/// Like [`skip_spaces`], line breaks included.
fn skip_whitespace(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn ends_unquoted_value(b: u8) -> bool {
    b.is_ascii_whitespace()
        || matches!(
            b,
            b'&' | b';' | b',' | b')' | b'}' | b']' | b'\'' | b'"' | b'\\'
        )
}

/// Length in bytes of the character that starts with byte `b`.
pub(crate) fn char_len(b: u8) -> usize {
    match b {
        0xf0.. => 4,
        0xe0.. => 3,
        0xc0.. => 2,
        _ => 1,
    }
}

/// End of an unquoted value starting at `start`. An escaped character
/// (`\'`, `\\`, ...) and a doubled quote (`''` or `""`, how a quote is
/// written inside a SQL literal) belong to the value, so the password in
/// `'host=h password=ab''cdef'` is not cut at the quote.
fn unquoted_value_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && escaped_break_before_key(&bytes[i..]) {
            // `password=x\nhost=h`: an escaped line break before the next
            // key ends the value.
            break;
        } else if b == b'\\' && i + 1 < bytes.len() {
            i += 1 + char_len(bytes[i + 1]);
        } else if matches!(b, b'\'' | b'"') && bytes.get(i + 1) == Some(&b) {
            i += 2;
        } else if ends_unquoted_value(b) {
            break;
        } else {
            i += 1;
        }
    }
    i.min(bytes.len())
}

/// True when `bytes` starts with an escaped line break or tab (`\n`,
/// `\r`, `\t`) followed by a `key=` word.
fn escaped_break_before_key(bytes: &[u8]) -> bool {
    if !matches!(bytes.get(1), Some(b'n' | b'r' | b't')) {
        return false;
    }
    let rest = &bytes[2..];
    let key = rest.iter().take_while(|&&b| is_key_char(b)).count();
    key > 0 && rest.get(key) == Some(&b'=')
}

/// End of a value opened by `marker` whose content starts at `start`: the
/// position of the closing marker, or the end of the text. Escaped and
/// doubled quotes inside the value do not close it.
fn quoted_value_end(bytes: &[u8], start: usize, marker: &str) -> usize {
    let marker = marker.as_bytes();
    let mut i = start;
    let mut escape_pending = false;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let escaped = rest[0] == b'\\' && rest.len() > 1;
        match marker {
            // `'...'` and `"..."`: a backslash escapes, a doubled quote is a
            // quote.
            [quote] => {
                if escaped {
                    i += 1 + char_len(rest[1]);
                } else if rest[0] == *quote && rest.get(1) == Some(quote) {
                    i += 2;
                } else if rest[0] == *quote {
                    return i;
                } else {
                    i += 1;
                }
            }
            // `\"...\"`: the value is a JSON string inside an escaped string.
            // One level down, `\\` is a backslash and `\"` a quote; a quote
            // after a backslash of that level is part of the value.
            [b'\\', _] => {
                if rest.starts_with(b"\\\\") {
                    escape_pending = !escape_pending;
                    i += 2;
                } else if rest.starts_with(marker) {
                    if !escape_pending {
                        return i;
                    }
                    escape_pending = false;
                    i += 2;
                } else {
                    escape_pending = false;
                    i += 1;
                }
            }
            // A doubled quote: the value is quoted inside a SQL literal,
            // where a backslash before the doubled quote escapes it and four
            // quotes stand for one quote in the value.
            _ => {
                if rest[0] == b'\\' && rest[1..].starts_with(marker) {
                    i += 1 + marker.len();
                } else if escaped {
                    i += 1 + char_len(rest[1]);
                } else if rest.starts_with(marker) && rest[marker.len()..].starts_with(marker) {
                    i += 2 * marker.len();
                } else if rest.starts_with(marker) {
                    return i;
                } else {
                    i += 1;
                }
            }
        }
    }
    bytes.len()
}

/// The values of secret keys in `key=value`, `key => value`,
/// `"key": value`, `key%3Dvalue` and YAML-style `key: value` pairs (the
/// last only when the value looks like a secret, so `password: YES` in a
/// sentence stays). For `Authorization`-like keys the scheme is kept.
pub(crate) fn key_value_ranges(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_key_char(bytes[i]) || !token_start(bytes, i) {
            i += 1;
            continue;
        }
        let key_start = i;
        while i < bytes.len() && is_key_char(bytes[i]) {
            i += 1;
        }
        let key_end = i;
        let key = &text[key_start..key_end];
        if !is_secret_text_key(key) {
            continue;
        }
        let mut j = key_end;
        let mut quoted_key = false;
        for marker in QUOTE_MARKERS {
            if text[j..].starts_with(marker) {
                j += marker.len();
                quoted_key = true;
                break;
            }
        }
        j = if quoted_key {
            skip_whitespace(bytes, j)
        } else {
            skip_spaces(bytes, j)
        };
        let single = |sep: u8| bytes.get(j) == Some(&sep) && bytes.get(j + 1) != Some(&sep);
        let mut needs_shape = false;
        if text[j..].starts_with("=>") {
            j += 2;
        } else if single(b'=') || (quoted_key && single(b':')) {
            j += 1;
        } else if text
            .get(j..j + 3)
            .is_some_and(|s| s.eq_ignore_ascii_case("%3d"))
        {
            j += 3;
        } else if single(b':') && bytes.get(j + 1).is_some_and(|b| *b == b' ' || *b == b'\t') {
            // `password: value` in YAML or a sentence.
            j += 1;
            needs_shape = true;
        } else {
            continue;
        }
        j = if quoted_key {
            skip_whitespace(bytes, j)
        } else {
            skip_spaces(bytes, j)
        };
        if text[j..].starts_with(REDACTED) {
            i = j + REDACTED.len();
            continue;
        }
        let (mut value_start, value_end) =
            match QUOTE_MARKERS.iter().find(|m| text[j..].starts_with(**m)) {
                Some(marker) => {
                    let start = j + marker.len();
                    (start, quoted_value_end(bytes, start, marker))
                }
                None => {
                    if is_authorization_key(key)
                        && let Some(end) = scheme_end(bytes, j)
                    {
                        let token = skip_spaces(bytes, end);
                        (token, unquoted_value_end(bytes, token))
                    } else {
                        (j, unquoted_value_end(bytes, j))
                    }
                }
            };
        if is_authorization_key(key)
            && let Some(end) = scheme_end(bytes, value_start)
            && end < value_end
        {
            value_start = skip_spaces(bytes, end).min(value_end);
        }
        let quoted = value_start > j;
        if needs_shape && !quoted && !secret_shaped(&text[value_start..value_end]) {
            i = value_end.max(key_end);
            continue;
        }
        if value_end > value_start && !text[value_start..].starts_with(REDACTED) {
            ranges.push((value_start, value_end));
        }
        i = value_end.max(key_end);
    }
    ranges
}

/// Unquoted words after a password keyword in comment text that are no
/// password: option keywords of account statements (`PASSWORD EXPIRE`,
/// `PASSWORD NULL`, `IDENTIFIED BY RANDOM PASSWORD`) and English function
/// words of prose (`PASSWORD is managed by vault`). Upper case.
const NOT_COMMENT_VALUES: &[&str] = &[
    "A",
    "ACCOUNT",
    "ALSO",
    "AN",
    "AND",
    "ARE",
    "AS",
    "AT",
    "ATTRIBUTE",
    "BE",
    "BEEN",
    "BY",
    "CAN",
    "COMMENT",
    "CONNECTION",
    "COULD",
    "CURRENT",
    "DEFAULT",
    "DID",
    "DO",
    "DOES",
    "ENCRYPTED",
    "EXPIRE",
    "FAILED_LOGIN_ATTEMPTS",
    "FOR",
    "FROM",
    "HAD",
    "HAS",
    "HAVE",
    "HISTORY",
    "IF",
    "IN",
    "IS",
    "IT",
    "ITS",
    "JUST",
    "LOCK",
    "LOGIN",
    "MAY",
    "MIGHT",
    "MUST",
    "NEVER",
    "NO",
    "NOLOGIN",
    "NOT",
    "NOW",
    "NULL",
    "OF",
    "ON",
    "ONLY",
    "OR",
    "PASSWORD",
    "PASSWORD_LOCK_TIME",
    "RANDOM",
    "REQUIRE",
    "REUSE",
    "SHALL",
    "SHOULD",
    "STILL",
    "THAT",
    "THE",
    "THEN",
    "THERE",
    "THESE",
    "THIS",
    "THOSE",
    "TO",
    "UNLOCK",
    "VALID",
    "VIA",
    "WAS",
    "WERE",
    "WILL",
    "WITH",
    "WOULD",
];

/// Secret patterns in comment text, read as plain text (an apostrophe in
/// `don't` cannot hide what follows): everything [`credential_ranges`]
/// finds, and the value after a password keyword (`PASSWORD 'x'`,
/// `password: x`, `PASSWORD('x')`, misspellings included) or after
/// `IDENTIFIED [WITH plugin] BY | AS` and `USING`. A quoted value is always
/// a secret; an unquoted one when it looks like one, or, on a line that
/// holds an account statement (`ALTER ROLE`, `CREATE USER`, `GRANT`), when
/// it is no keyword or function word.
pub(crate) fn comment_secret_ranges(text: &str) -> Ranges {
    comment_rules(text, false)
}

/// [`comment_secret_ranges`] for a field of table data: a command line at
/// its start is read like one after a list bullet, since a data value is
/// as often prose (`mysql -performance tips`) as a command.
pub(crate) fn data_field_ranges(text: &str) -> Ranges {
    comment_rules(text, true)
}

/// The comment rules; with `weak_start`, see [`data_field_ranges`].
fn comment_rules(text: &str, weak_start: bool) -> Ranges {
    let mut ranges = credential_ranges(text);
    ranges.extend(text_key_call_ranges(text));
    let mut cli = Vec::new();
    let map: Vec<usize> = (0..=text.len()).collect();
    cli_scan(text, &map, 0, &mut cli, weak_start);
    ranges.extend(cli);
    let bytes = text.as_bytes();
    // The current statement holds an account statement head (`ALTER
    // ROLE`, `CREATE USER`, `GRANT`, ...), also over several comment lines
    // (`-- ALTER ROLE app\n--   PASSWORD x`), up to a `;`: a password after
    // `PASSWORD` or `IDENTIFIED BY` needs no shape.
    let mut account_line = false;
    // A password word or `IDENTIFIED` was seen in the current statement:
    // `REPLACE 'current'` gives the current password.
    let mut password_line = false;
    let mut previous: Option<String> = None;
    let mut last_end = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !is_word_byte(bytes[i]) || (i > 0 && is_word_byte(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_word_byte(bytes[i]) {
            i += 1;
        }
        let between = &text[last_end..start];
        if between.contains(';') {
            account_line = false;
            password_line = false;
        }
        if between.contains(['\n', ';']) {
            previous = None;
        }
        last_end = i;
        let word = text[start..i].to_ascii_uppercase();
        if word == "GRANT"
            || (previous
                .as_deref()
                .is_some_and(|p| within(p, "ALTER", 1) || within(p, "CREATE", 1))
                && ["USER", "ROLE", "GROUP"]
                    .iter()
                    .any(|o| within(&word, o, 1)))
        {
            account_line = true;
        }
        let value = if like_password(&word) {
            password_line = true;
            let next = skip_whitespace(bytes, i);
            if bytes
                .get(next..next + 3)
                .is_some_and(|w| w.eq_ignore_ascii_case(b"FOR"))
                && !bytes.get(next + 3).is_some_and(|&b| is_word_byte(b))
            {
                // `SET PASSWORD FOR user = 'x'`: the value follows the `=`
                // right after the account name, never one further on.
                account_eq(bytes, next + 3)
                    .map(|eq| (eq, false))
                    .or_else(|| prose_eq(bytes, next + 3).map(|eq| (eq, true)))
            } else {
                Some((i, !account_line))
            }
        } else if within(&word, "IDENTIFIED", 2) {
            password_line = true;
            value_after_identified(text, i).map(|at| (at, !account_line))
        } else if word == "REPLACE" && (password_line || account_line) {
            // `SET PASSWORD ... = 'new' REPLACE 'current'`.
            Some((i, !account_line))
        } else {
            None
        };
        // `password_min_length 12`: a count, time or policy about a password
        // keeps a number or date.
        let about = !account_line && metadata_key(&word.to_ascii_lowercase());
        previous = Some(word);
        if let Some((at, needs_shape)) = value
            && let Some(range) = comment_value(text, at, needs_shape)
            && !(about && super::sql::metadata_text(&text[range.0..range.1], false))
        {
            i = range.1.max(i);
            last_end = i;
            ranges.push(range);
        }
    }
    ranges
}

/// Longest stretch read by [`prose_eq`].
const PROSE_EQ_WINDOW: usize = 256;

/// After `password for` in prose (`set password for the app user = x`):
/// the `=` later on the same line, within [`PROSE_EQ_WINDOW`] bytes. The
/// value after it must look like a secret.
fn prose_eq(bytes: &[u8], at: usize) -> Option<usize> {
    bytes
        .iter()
        .skip(at)
        .take(PROSE_EQ_WINDOW)
        .take_while(|&&b| b != b'\n' && b != b';')
        .position(|&b| b == b'=')
        .map(|p| at + p)
}

/// The matching `)` of the `(` at `open`, skipping quoted strings, or the
/// end of the line.
fn call_close(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() && bytes[i] != b'\n' {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            quote @ (b'\'' | b'"') => {
                let marker = if quote == b'\'' { "'" } else { "\"" };
                i = quoted_value_end(bytes, i + 1, marker);
            }
            _ => {}
        }
        i += 1;
    }
    i.min(bytes.len())
}

/// After `SET PASSWORD FOR` at `at`: the position of the `=` that follows
/// the account names (`u`, `'u'@'%'`, `` `u`@localhost ``, a list `a, b`),
/// with whitespace and line breaks around them, or `None` when something
/// else follows.
fn account_eq(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = skip_whitespace(bytes, at);
    let mut parts = 0;
    loop {
        match bytes.get(i) {
            Some(&quote @ (b'\'' | b'"' | b'`')) => {
                let marker = match quote {
                    b'\'' => "'",
                    b'"' => "\"",
                    _ => "`",
                };
                i = (quoted_value_end(bytes, i + 1, marker) + 1).min(bytes.len());
            }
            Some(_) => {
                let start = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && !matches!(bytes[i], b'=' | b'@' | b',')
                {
                    i += 1;
                }
                if i == start {
                    return None;
                }
            }
            None => return None,
        }
        parts += 1;
        i = skip_whitespace(bytes, i);
        match bytes.get(i) {
            Some(b'@') if parts == 1 => i = skip_whitespace(bytes, i + 1),
            // `SET PASSWORD FOR a, b = ...`: another account follows.
            Some(b',') => {
                parts = 0;
                i = skip_whitespace(bytes, i + 1);
            }
            Some(b'=') => return Some(i),
            // `SET PASSWORD FOR app TO 'x'`.
            Some(_)
                if bytes
                    .get(i..i + 2)
                    .is_some_and(|w| w.eq_ignore_ascii_case(b"TO"))
                    && !bytes.get(i + 2).is_some_and(|&b| is_word_byte(b)) =>
            {
                return Some(i + 2);
            }
            _ => return None,
        }
    }
}

/// A database client named on a command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Client {
    /// `mysql`, `mysqldump`, `mariadb`, ...: `-p<password>`,
    /// `--password <password>`.
    Mysql,
    /// `mysqladmin`: like [`Client::Mysql`], and `password <new>` sets one.
    MysqlAdmin,
    /// `redis-cli`, `valkey-cli`: `-a <password>`, `--pass <password>`, and
    /// a Redis command given as arguments (`AUTH user pass`).
    Redis,
    /// `psql`, `pg_dump`, ...: `--password` and `-W` are flags that ask
    /// for the password and take no value.
    Postgres,
}

/// The tool a command word names: its last path part (`/` or `\`),
/// without a `.exe` suffix (any case).
fn tool_name(word: &str) -> &str {
    let tool = word.rsplit(['/', '\\']).next().unwrap_or_default();
    match tool.len().checked_sub(4) {
        Some(cut) if tool.is_char_boundary(cut) && tool[cut..].eq_ignore_ascii_case(".exe") => {
            &tool[..cut]
        }
        _ => tool,
    }
}

/// The client a command word names, if any (see [`tool_name`]), any case.
fn client_named(word: &str) -> Option<Client> {
    match tool_name(word).to_ascii_lowercase().as_str() {
        "mysqladmin" | "mariadb-admin" => Some(Client::MysqlAdmin),
        "mysql" | "mysqldump" | "mysqlimport" | "mysqlcheck" | "mysqlpump" | "mysqlshow"
        | "mysqlsh" | "mysqlslap" | "mysqlbinlog" | "mariadb" | "mariadb-dump"
        | "mariadb-import" | "mariadb-check" | "mariadb-show" | "mariadb-slap"
        | "mariadb-binlog" => Some(Client::Mysql),
        "redis-cli" | "valkey-cli" => Some(Client::Redis),
        "psql" | "pg_dump" | "pg_dumpall" | "pg_restore" | "pgbench" | "createdb" | "dropdb" => {
            Some(Client::Postgres)
        }
        _ => None,
    }
}

/// One word of a shell command line: its raw byte range in the text read
/// (quotes included) and its value with the quoting undone.
#[derive(Debug, Clone)]
struct ShellWord {
    start: usize,
    end: usize,
    value: String,
}

/// One piece of a shell command line read by [`shell_items`].
#[derive(Debug, Clone)]
enum ShellItem {
    Word(ShellWord),
    /// A command separator: a line end, `;`, `|`, `&`, `(`, `)`, `$(`, a
    /// backtick, or a comment marker word (`#`, `/*`, `*/`).
    Separator,
}

/// How deep [`cli_scan`] reads command lines held in quoted arguments
/// (`sh -c "mysql -p..."`, `ssh host 'mysqldump -p...'`).
const MAX_SHELL_DEPTH: usize = 3;

/// The text of a quoted part of a shell word with POSIX quoting undone,
/// and the offset in `text` of every byte of it (one more for its end).
/// In `"..."` a backslash escapes `"`, `\`, `$` and a backtick; in
/// `$'...'` (ANSI-C quoting) it escapes any character; in `'...'` nothing.
fn unquote(text: &str, open: usize, close: usize, escapes: Escapes) -> (String, Vec<usize>) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(close - open);
    let mut map = Vec::with_capacity(close - open + 1);
    let mut i = open;
    while i < close {
        let escaped = bytes[i] == b'\\'
            && i + 1 < close
            && match escapes {
                Escapes::None => false,
                Escapes::Double => matches!(bytes[i + 1], b'"' | b'\\' | b'$' | b'`'),
                Escapes::AnsiC => true,
            };
        let from = if escaped { i + 1 } else { i };
        let len = char_len(bytes[from]).min(close - from);
        for _ in 0..len {
            map.push(i);
        }
        out.push_str(&text[from..from + len]);
        i = from + len;
    }
    map.push(close);
    (out, map)
}

/// The escapes a quoted part of a shell word knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escapes {
    /// `'...'`.
    None,
    /// `"..."`.
    Double,
    /// `$'...'`.
    AnsiC,
}

/// The end of a quoted part opened at `open` (the byte after the opening
/// quote), honouring `escapes`, or the end of the text.
fn quote_end(bytes: &[u8], open: usize, quote: u8, escapes: Escapes) -> usize {
    let mut i = open;
    while i < bytes.len() && bytes[i] != quote {
        if bytes[i] == b'\\' && escapes != Escapes::None {
            i += 1;
        }
        i += 1;
    }
    i.min(bytes.len())
}

/// The words and separators of shell text whose bytes sit at `map`
/// (absolute offsets, one more for the end). A word runs to unquoted
/// whitespace or a separator; quoted parts belong to the word, and their
/// content is read again as a command line (up to [`MAX_SHELL_DEPTH`]). A
/// backslash before a line break continues the line. An `&` inside a
/// redirection (`2>&1`, `&>`) is no separator.
fn shell_items(text: &str, map: &[usize], depth: usize, out: &mut Ranges) -> Vec<ShellItem> {
    let bytes = text.as_bytes();
    let mut items = Vec::new();
    let mut word: Option<ShellWord> = None;
    let flush = |items: &mut Vec<ShellItem>, word: &mut Option<ShellWord>, at: usize| {
        if let Some(mut w) = word.take() {
            w.end = map[at];
            if matches!(w.value.as_str(), "#" | "/*" | "*/") {
                items.push(ShellItem::Separator);
            } else {
                items.push(ShellItem::Word(w));
            }
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && matches!(bytes.get(i + 1), Some(b'\n' | b'\r')) {
            flush(&mut items, &mut word, i);
            i += 2;
            if bytes.get(i - 1) == Some(&b'\r') && bytes.get(i) == Some(&b'\n') {
                i += 1;
            }
            continue;
        }
        let redirect_amp = b == b'&'
            && (matches!(bytes.get(i + 1), Some(b'>'))
                || (i > 0 && matches!(bytes[i - 1], b'>' | b'<')));
        let separator = b == b'\n'
            || matches!(b, b';' | b'|' | b'(' | b')' | b'`')
            || (b == b'&' && !redirect_amp)
            || (b == b'$' && bytes.get(i + 1) == Some(&b'('));
        if separator {
            flush(&mut items, &mut word, i);
            items.push(ShellItem::Separator);
            i += if b == b'$' { 2 } else { 1 };
            continue;
        }
        if b.is_ascii_whitespace() {
            flush(&mut items, &mut word, i);
            i += 1;
            continue;
        }
        let current = word.get_or_insert_with(|| ShellWord {
            start: map[i],
            end: map[i],
            value: String::new(),
        });
        let ansi = b == b'$' && bytes.get(i + 1) == Some(&b'\'');
        if matches!(b, b'\'' | b'"') || ansi {
            let (quote, escapes, open) = match (ansi, b) {
                (true, _) => (b'\'', Escapes::AnsiC, i + 2),
                (false, b'"') => (b'"', Escapes::Double, i + 1),
                _ => (b'\'', Escapes::None, i + 1),
            };
            let close = quote_end(bytes, open, quote, escapes);
            let (inner, inner_map) = unquote(text, open, close, escapes);
            let absolute: Vec<usize> = inner_map.iter().map(|&k| map[k]).collect();
            if depth < MAX_SHELL_DEPTH && !inner.is_empty() {
                cli_scan(&inner, &absolute, depth + 1, out, false);
            }
            current.value.push_str(&inner);
            i = (close + 1).min(bytes.len());
            continue;
        }
        let len = char_len(b).min(bytes.len() - i);
        current.value.push_str(&text[i..i + len]);
        i += len;
    }
    flush(&mut items, &mut word, bytes.len());
    items
}

/// One client command being read by [`cli_scan`].
struct Command {
    client: Client,
    /// The client is the command of its segment (see [`cli_scan`]) and its
    /// tool name is lower case, as a shell runs it.
    command_position: bool,
    /// The client reached command position only through a list bullet
    /// (`- mysqldump ...`), which prose uses too.
    bulleted: bool,
    /// The item index of the client word.
    at: usize,
    /// Another option than `-p...` was given.
    flagged: bool,
    /// `-p<password>` values with their item index, kept until the command
    /// ends.
    candidates: Vec<(usize, usize, String, usize)>,
    /// `mysqladmin`: `password` was the previous word.
    admin_expect: bool,
    /// `mysqladmin password NEW`: the new password, whether it is quoted or
    /// looks like a secret, and its item index.
    admin_value: Option<(usize, usize, bool, usize)>,
    /// The item index of the command's last word so far.
    last: usize,
    /// `redis-cli`: the words after its options, a Redis command.
    redis_args: Vec<ShellWord>,
    /// `redis-cli`: the previous word was an option that takes a value.
    redis_option_value: bool,
}

impl Command {
    /// The ranges to redact once the command ends: every `-p` value of a
    /// client in command position or of a command with another option, and
    /// elsewhere a value that looks like a secret (`run mysql -phunter` and
    /// `MySQL docs -performance tips` stay). After a list bullet, a `-p...`
    /// right after the client name needs another option or a secret shape
    /// (`- mysql -performance_schema is on` is prose). For `redis-cli`, the
    /// secret arguments of the Redis command it runs.
    fn finish(self, ranges: &mut Ranges) {
        for (start, end, value, index) in self.candidates {
            let position = self.command_position && !(self.bulleted && index == self.at + 1);
            if position || self.flagged || secret_shaped(&value) {
                ranges.push((start, end));
            }
        }
        // `mysqladmin password NEW`: the new password when it is quoted,
        // looks like one, ends the command, or the command has options
        // (`mysqladmin password reset is documented` is prose).
        if let Some((start, end, shaped, index)) = self.admin_value
            && (shaped || self.flagged || index == self.last)
        {
            ranges.push((start, end));
        }
        redis_cli_ranges(&self.redis_args, ranges);
    }
}

/// `redis-cli` and `valkey-cli` options that take a value (from `--help`
/// of redis-cli 6.2 and 8.10 and valkey-cli 9; `--askpass`, `-X`, `-c`,
/// `-e`, `-x` and the mode flags take none).
const REDIS_CLI_VALUE_OPTIONS: &[&str] = &[
    "-h",
    "-p",
    "-s",
    "-a",
    "-u",
    "-n",
    "-r",
    "-i",
    "-d",
    "-t",
    "-D",
    "--user",
    "--pass",
    "--cacert",
    "--cacertdir",
    "--cert",
    "--key",
    "--sni",
    "--tls-ciphers",
    "--tls-ciphersuites",
    "--name",
    "--show-pushes",
    "--pattern",
    "--quoted-pattern",
    "--count",
    "--cursor",
    "--top",
    "--rdb",
    "--functions-rdb",
    "--eval",
    "--memkeys-samples",
    "--keystats-samples",
    "--hotkeys-count",
    "--pipe-timeout",
    "--intrinsic-latency",
    "--lru-test",
    "--latency-percentiles",
    "--cluster",
    "--vset-recall",
    "--vset-recall-count",
    "--vset-recall-ef",
    "--vset-recall-ele",
];

/// The secret arguments of a Redis command given to `redis-cli`: `AUTH
/// [user] pass`, `AUTH2 user pass`, `HELLO ... AUTH user pass`, `CONFIG
/// SET name value` for a secret parameter, `MIGRATE ... AUTH pass |
/// AUTH2 user pass`, and `ACL SETUSER` password rules (`>pw`, `<pw`,
/// `#hash`, `!hash`).
fn redis_cli_ranges(args: &[ShellWord], ranges: &mut Ranges) {
    let upper = |w: &ShellWord| w.value.to_ascii_uppercase();
    // The Redis command: the first argument when it is a command with
    // secret arguments, else a `CONFIG SET` or `ACL SETUSER` anywhere (an
    // option the table above does not know shifts the arguments).
    let first = args.first().map(upper).unwrap_or_default();
    let start = if matches!(
        first.as_str(),
        "AUTH" | "AUTH2" | "HELLO" | "MIGRATE" | "CONFIG" | "ACL"
    ) {
        Some(0)
    } else {
        args.windows(2).position(|pair| {
            matches!(
                (upper(&pair[0]).as_str(), upper(&pair[1]).as_str()),
                ("CONFIG", "SET") | ("ACL", "SETUSER")
            )
        })
    };
    let Some(start) = start else {
        return;
    };
    let args = &args[start..];
    let mut push = |w: &ShellWord| ranges.push((w.start, w.end));
    match upper(&args[0]).as_str() {
        "AUTH" => {
            if let Some(last) = args.last().filter(|_| args.len() > 1) {
                push(last);
            }
        }
        "AUTH2" => {
            if let Some(pass) = args.get(2) {
                push(pass);
            }
        }
        "CONFIG" if args.get(1).is_some_and(|w| upper(w) == "SET") => {
            for pair in args[2..].chunks(2) {
                if let [key, value] = pair {
                    let key = key.value.to_ascii_lowercase();
                    if key.contains("pass") || key.contains("auth") || key.contains("secret") {
                        push(value);
                    }
                }
            }
        }
        "ACL" => {
            for rule in &args[1..] {
                if rule.value.starts_with(['>', '<', '#', '!']) && rule.value.len() > 1 {
                    push(rule);
                }
            }
        }
        name => {
            // `HELLO 3 AUTH user pass`, `MIGRATE ... AUTH pass | AUTH2 user
            // pass`.
            for (k, word) in args.iter().enumerate() {
                let pass = match upper(word).as_str() {
                    "AUTH" if name == "HELLO" => args.get(k + 2),
                    "AUTH" => args.get(k + 1),
                    "AUTH2" => args.get(k + 2),
                    _ => None,
                };
                if let Some(pass) = pass {
                    push(pass);
                }
            }
        }
    }
}

/// Database command lines in text: `mysql -uroot -pSECRET`, `mysqldump
/// --password SECRET`, `redis-cli -a SECRET`, `--pass SECRET`. A client
/// name is found as any word (with a path, `\` or `/`, `.exe`, and in
/// quotes: `& "C:\Program Files\...\mysql.exe"`), also after `&&`, `;`,
/// `|`, `$(`, a backtick, or inside a quoted argument (`sh -c "mysql
/// -p..."`, `ssh host 'mysqldump -p...'`), read with POSIX quoting; its
/// command runs to the next separator or unescaped line break, over `\`
/// line continuations.
///
/// When the client is the command of its segment, every `-p<value>` is a
/// password. Command position is the first word of a segment, also after
/// a shell prompt (`$`, `#`, `user@host:~$`, `[root@db1 ~]#`, zsh
/// `user@host ~ %`, fish `~>`, `PS C:\>`, cmd `C:\>` even glued to the
/// command, `mysql> \!`), cron time fields or `@daily` (and one user field
/// after them), a list bullet (where a `-p...` right after the client name
/// still needs another option or a secret shape: `- mysql
/// -performance_schema is on` is prose), the `*` of a comment block line, `NAME=value`
/// assignments, redirections (`2>&1`), anything after a wrapper (`sudo`,
/// `env`, `exec`, `docker exec ...`, `ssh host`, `stdbuf`, ...), and, on a
/// line that reads as shell (a pipe, `;`, `&&`, a redirection, `$(`, a
/// backtick, a prompt), after `do`, `then`, `else`, `{`, `!` or `time`.
/// The tool name must be lower case, as a shell runs it. Elsewhere (`run
/// mysql with -pX`) a `-p<value>` is one when the command has another
/// option or the value looks like a secret. `--password <value>` always
/// is. `mysqladmin password NEW` is when the value is quoted, looks like a
/// secret, ends the command or the command has options. `psql --password`
/// and `-W` are flags without a value. After `--pass` or `--password` on a
/// command of no known client only a quoted or secret-shaped word is taken
/// (`--password to be prompted` is prose).
pub(crate) fn cli_ranges(text: &str) -> Ranges {
    let mut ranges = Vec::new();
    let map: Vec<usize> = (0..=text.len()).collect();
    cli_scan(text, &map, 0, &mut ranges, false);
    ranges
}

/// Commands that run another command given after their own arguments
/// (`sudo mysql`, `docker exec -it db mysql`, `ssh host mysqldump`), and
/// shell words after which a command follows (`do`, `then`, `{`, `!`).
fn is_wrapper(word: &str) -> bool {
    matches!(
        word.rsplit('/').next().unwrap_or_default(),
        "sudo"
            | "doas"
            | "env"
            | "exec"
            | "nohup"
            | "nice"
            | "ionice"
            | "timeout"
            | "watch"
            | "xargs"
            | "command"
            | "builtin"
            | "stdbuf"
            | "docker"
            | "podman"
            | "kubectl"
            | "oc"
            | "ssh"
            | "sh"
            | "bash"
            | "zsh"
            | "su"
            | "runuser"
    )
}

/// Shell grammar words after which a command follows (`do`, `then`,
/// `else`, `{`, `!`, `time`). They are English words too (`do not use
/// mysql -performance_schema`), so they only count on a line that already
/// reads as shell ([`looks_like_shell`]), and only for the next word.
fn is_shell_keyword(word: &str) -> bool {
    matches!(word, "do" | "then" | "else" | "elif" | "{" | "!" | "time")
}

/// True when `text` reads as a shell line: it has a pipe, a `;`, `&&`, a
/// redirection, `$(` or a backtick, or starts with a prompt.
fn looks_like_shell(text: &str) -> bool {
    text.contains(['|', ';', '<', '>', '`'])
        || text.contains("$(")
        || text.contains("&&")
        || text.split_whitespace().next().is_some_and(is_prompt)
}

/// A shell prompt word: `$`, `#`, `%`, `>`, `PS`, `PS>`, `C:\>`,
/// `user@host:~$`, `[root@db1`, `~]#`, `mysql>`, and the `\!` shell escape
/// of the MySQL client.
fn is_prompt(word: &str) -> bool {
    matches!(word, "$" | "#" | "%" | ">" | "PS" | "\\!")
        || (word.len() > 1 && word.ends_with(['$', '#', '>']))
        || (word.starts_with('[') && word.contains('@'))
}

/// True for a cron time field (`0`, `*/5`, `1-5`, `@daily`).
fn is_cron_field(word: &str) -> bool {
    let fields = word
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'*' | b'/' | b',' | b'-'))
        && word.bytes().any(|b| b.is_ascii_digit() || b == b'*');
    let special = matches!(
        word,
        "@reboot"
            | "@yearly"
            | "@annually"
            | "@monthly"
            | "@weekly"
            | "@daily"
            | "@midnight"
            | "@hourly"
    );
    fields || special
}

/// Words that keep the next word in command position without starting a
/// command: a shell prompt ([`is_prompt`]), a cron time field (`0`, `*/5`,
/// `1-5`, `@daily`), the `*` of a comment block line, a list bullet, a
/// variable assignment (`MYSQL_PWD=x`), and a redirection (`2>&1`,
/// `>log`).
fn is_transparent(word: &str) -> bool {
    let assignment = word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !name.as_bytes()[0].is_ascii_digit()
    });
    // A list bullet (`- mysqldump ...`).
    let bullet = matches!(word, "-" | "+" | "\u{2022}");
    // The `user@host` and working directory (`~`, `~/x`) parts of a prompt
    // (`kim@macbook ~ % ...`).
    let user_host = word.split_once('@').is_some_and(|(user, host)| {
        !user.is_empty()
            && !host.is_empty()
            && !word.contains(['/', '=', '\'', '"'])
            && user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    });
    let cwd = word.starts_with('~');
    let redirection = word
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .starts_with(['<', '>']);
    assignment
        || is_prompt(word)
        || bullet
        || user_host
        || cwd
        || is_cron_field(word)
        || redirection
}

/// Words that keep command position open but also start prose: a list
/// bullet as the first word (`-`, `+`, `*`, `•`), a `user@host` word and a
/// `~` word.
fn weak_transparent(word: &str, segment_words: usize) -> bool {
    let bullet = segment_words == 1 && matches!(word, "-" | "+" | "*" | "\u{2022}");
    let user_host =
        word.contains('@') && !word.starts_with('@') && !word.ends_with(['$', '#', '>', '%']);
    bullet || user_host || word.starts_with('~') && !word.ends_with(['$', '#', '>', '%'])
}

/// [`cli_ranges`] for `text` whose bytes sit at `map`, `depth` quotes deep;
/// with `weak_start`, command position at the very start is weak (as after
/// a list bullet).
fn cli_scan(text: &str, map: &[usize], depth: usize, ranges: &mut Ranges, weak_start: bool) {
    let items = shell_items(text, map, depth, ranges);
    let mut command: Option<Command> = None;
    // The next word is a password: required by the client, or only when it
    // looks like one (unknown tools).
    let mut take_next: Option<bool> = None;
    let mut redirect_next = false;
    // The next word can still be the command of this segment.
    let mut position_open = true;
    let mut wrapped = false;
    let shell = looks_like_shell(text);
    // Cron time fields read at the start of the segment; after five, one
    // user name (`/etc/crontab`) may come before the command.
    let mut cron_fields = 0usize;
    let mut cron_user = false;
    // Command position is open only through a list bullet so far.
    let mut bulleted = weak_start;
    // Words read in the current segment.
    let mut segment_words = 0usize;
    for (index, item) in items.iter().enumerate() {
        let ShellItem::Word(word) = item else {
            if let Some(done) = command.take() {
                done.finish(ranges);
            }
            take_next = None;
            redirect_next = false;
            position_open = true;
            wrapped = false;
            cron_fields = 0;
            cron_user = false;
            bulleted = false;
            segment_words = 0;
            continue;
        };
        segment_words += 1;
        let (mut start, end) = (word.start, word.end);
        let mut w = word.value.as_str();
        // `C:\>mysqldump`: a cmd prompt glued to the command.
        if let Some(at) = w.rfind('>')
            && at + 1 < w.len()
            && w[..=at].contains(":\\")
            && position_open
            && word.value.len() == end - start
        {
            start += at + 1;
            w = &w[at + 1..];
        }
        let at_command = position_open;
        if position_open && is_cron_field(w) {
            // `@daily` stands for all five time fields.
            cron_fields += if w.starts_with('@') { 5 } else { 1 };
        }
        // Words that also start prose (a bullet, `user@host`, `~`) open
        // command position only weakly; a prompt, a wrapper, cron fields or
        // an assignment make it certain.
        if position_open && weak_transparent(w, segment_words) {
            bulleted = true;
        } else if position_open && (is_transparent(w) || is_wrapper(w)) {
            bulleted = false;
        } else if position_open {
            bulleted = bulleted && client_named(w).is_some();
        }
        if is_wrapper(w) {
            wrapped = true;
        } else if wrapped || (shell && is_shell_keyword(w)) || is_transparent(w) {
            // Command position stays open.
        } else if position_open && cron_fields >= 5 && !cron_user && client_named(w).is_none() {
            cron_user = true;
        } else {
            position_open = false;
        }
        if let Some(required) = take_next.take()
            && !w.starts_with('-')
        {
            // Quoted (the word as written is longer than its value), or
            // shaped like a secret.
            let quoted = word.value.len() + 2 <= end - start;
            let shaped = quoted || secret_shaped(w);
            if required || shaped {
                ranges.push((start, end));
                continue;
            }
        }
        if let Some(client) = client_named(w) {
            if let Some(done) = command.take() {
                done.finish(ranges);
            }
            let lower = !tool_name(w).bytes().any(|b| b.is_ascii_uppercase());
            command = Some(Command {
                client,
                command_position: at_command && lower,
                bulleted: bulleted && cron_fields < 5,
                at: index,
                flagged: false,
                candidates: Vec::new(),
                admin_expect: false,
                admin_value: None,
                last: index,
                redis_args: Vec::new(),
                redis_option_value: false,
            });
            continue;
        }
        if redirect_next {
            redirect_next = false;
            continue;
        }
        if matches!(w, "<" | ">" | ">>" | "2>" | "2>>") {
            redirect_next = true;
            continue;
        }
        let lower = w.to_ascii_lowercase();
        let Some(current) = command.as_mut() else {
            if matches!(lower.as_str(), "--pass" | "--password") {
                take_next = Some(false);
            }
            continue;
        };
        current.last = index;
        if std::mem::take(&mut current.admin_expect) {
            let quoted = word.value.len() + 2 <= end - start;
            current.admin_value = Some((start, end, quoted || secret_shaped(w), index));
            continue;
        }
        match current.client {
            Client::Mysql | Client::MysqlAdmin => {
                if w.len() > 2 && w.starts_with("-p") && !w.starts_with("--") {
                    // The raw value: after `-p` in the word as written.
                    let value_start = (start + 2).min(end);
                    current
                        .candidates
                        .push((value_start, end, w[2..].to_string(), index));
                } else if lower == "--password" || lower == "--pass" {
                    current.flagged = true;
                    take_next = Some(true);
                } else if w.starts_with('-') {
                    current.flagged = true;
                } else if current.client == Client::MysqlAdmin && lower == "password" {
                    // `mysqladmin ... password NEW`.
                    current.admin_expect = true;
                }
            }
            Client::Redis => {
                if current.redis_option_value {
                    current.redis_option_value = false;
                } else if !current.redis_args.is_empty() || !w.starts_with('-') {
                    current.redis_args.push(word.clone());
                } else if REDIS_CLI_VALUE_OPTIONS.contains(&lower.as_str()) {
                    if w == "-a" || lower == "--pass" {
                        take_next = Some(true);
                    } else {
                        current.redis_option_value = true;
                    }
                }
            }
            Client::Postgres => {}
        }
    }
    if let Some(done) = command.take() {
        done.finish(ranges);
    }
}

/// The key argument of an encryption function written in text, by name.
fn text_key_argument(word: &str) -> Option<usize> {
    match word.to_ascii_uppercase().as_str() {
        "AES_ENCRYPT"
        | "AES_DECRYPT"
        | "DES_ENCRYPT"
        | "DES_DECRYPT"
        | "PGP_SYM_ENCRYPT"
        | "PGP_SYM_DECRYPT"
        | "PGP_SYM_ENCRYPT_BYTEA"
        | "PGP_SYM_DECRYPT_BYTEA"
        | "HMAC"
        | "ENCRYPT_IV"
        | "DECRYPT_IV" => Some(1),
        "PGP_PUB_DECRYPT" | "PGP_PUB_DECRYPT_BYTEA" => Some(2),
        "CRYPT" => Some(0),
        _ => None,
    }
}

/// One open group in [`text_key_call_ranges`].
struct TextFrame {
    /// The key argument of the call, if it is a key function.
    target: Option<usize>,
    argument: usize,
    /// Quoted strings here are secrets because an enclosing call's key
    /// argument holds this group.
    inherited: bool,
}

impl TextFrame {
    fn secret(&self) -> bool {
        self.inherited || self.target == Some(self.argument)
    }
}

/// In text, the quoted strings in the key arguments of encryption
/// function calls, nested calls included: one forward pass with a stack
/// of open groups. Quotes may be doubled or backslash-escaped.
fn text_key_call_ranges(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut frames: Vec<TextFrame> = Vec::new();
    let mut last_word: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if is_word_byte(b) {
            let start = i;
            while i < bytes.len() && is_word_byte(bytes[i]) {
                i += 1;
            }
            last_word = Some((start, i));
            continue;
        }
        match b {
            b'(' | b'[' => {
                // A call when a word ends right before (spaces allowed).
                let target = last_word
                    .filter(|&(_, end)| text[end..i].trim().is_empty())
                    .and_then(|(start, end)| text_key_argument(&text[start..end]));
                let inherited = frames.last().is_some_and(TextFrame::secret);
                frames.push(TextFrame {
                    target,
                    argument: 0,
                    inherited,
                });
            }
            b')' | b']' => {
                frames.pop();
            }
            b',' => {
                if let Some(top) = frames.last_mut() {
                    top.argument += 1;
                }
            }
            quote @ (b'\'' | b'"') => {
                let marker = if quote == b'\'' { "'" } else { "\"" };
                let end = quoted_value_end(bytes, i + 1, marker);
                if frames.last().is_some_and(TextFrame::secret) && end > i + 1 {
                    ranges.push((i + 1, end));
                }
                i = end + 1;
                last_word = None;
                continue;
            }
            _ => {}
        }
        if !b.is_ascii_whitespace() {
            last_word = None;
        }
        i += 1;
    }
    ranges
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// After `IDENTIFIED`: the position after `BY`, `AS` or `USING` (with an
/// optional `WITH | VIA plugin` in between).
fn value_after_identified(text: &str, at: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = skip_whitespace(bytes, at);
    for _ in 0..3 {
        let start = i;
        while i < bytes.len() && is_word_byte(bytes[i]) {
            i += 1;
        }
        let word = text[start..i].to_ascii_uppercase();
        if matches!(word.as_str(), "BY" | "AS" | "USING") {
            return Some(i);
        }
        if word.is_empty() {
            return None;
        }
        i = skip_whitespace(bytes, i);
    }
    None
}

/// The secret value at `at` in comment text: after optional whitespace,
/// `=`, `:`, `:=` and `(`, a quoted value, or a word (that must look like a
/// secret when `needs_shape` is set).
fn comment_value(text: &str, at: usize, needs_shape: bool) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = skip_whitespace(bytes, at);
    for separator in [":=", "=", ":", "("] {
        if text[i..].starts_with(separator) {
            i = skip_whitespace(bytes, i + separator.len());
            break;
        }
    }
    // A call (`PASSWORD('x')`, `ENCRYPT('x')`, `PASSWORD(CONCAT('a',
    // 'b'))`): a single quoted argument is the value; any other argument
    // list is redacted whole.
    let word_end = i + bytes[i..].iter().take_while(|&&b| is_word_byte(b)).count();
    if word_end > i {
        let open = skip_whitespace(bytes, word_end);
        if bytes.get(open) == Some(&b'(') {
            let first = skip_whitespace(bytes, open + 1);
            let close = call_close(bytes, open);
            let single = match bytes.get(first) {
                Some(&quote @ (b'\'' | b'"')) => {
                    let marker = if quote == b'\'' { "'" } else { "\"" };
                    let end = quoted_value_end(bytes, first + 1, marker);
                    (skip_whitespace(bytes, end + 1) == close).then_some((first + 1, end))
                }
                _ => None,
            };
            let inner = (open + 1, close);
            return match single {
                Some((a, b)) => (b > a).then_some((a, b)),
                None => {
                    let (a, b) = inner;
                    let args = text[a..b].trim();
                    (!args.is_empty() && !args.starts_with(REDACTED)).then_some((a, b))
                }
            };
        }
    }
    match bytes.get(i) {
        Some(&quote @ (b'\'' | b'"')) => {
            let end = quoted_value_end(bytes, i + 1, if quote == b'\'' { "'" } else { "\"" });
            (end > i + 1).then_some((i + 1, end))
        }
        Some(_) => {
            let end = unquoted_value_end(bytes, i);
            let value = &text[i..end];
            let keyword = NOT_COMMENT_VALUES
                .iter()
                .any(|w| w.eq_ignore_ascii_case(value.trim_end_matches(['.', ',', ':', '!', '?'])));
            (end > i
                && !keyword
                && (!needs_shape || secret_shaped(value))
                && !text[i..].starts_with(REDACTED))
            .then_some((i, end))
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact_urls(text: &str, scope: UrlScope) -> Cow<'_, str> {
        splice(text, url_ranges(text, scope), &mut Vec::new())
    }

    fn redact_key_values(text: &str) -> Cow<'_, str> {
        splice(text, key_value_ranges(text), &mut Vec::new())
    }

    #[test]
    fn url_passwords_are_redacted() {
        let cases = [
            (
                "postgres://alice:S3cret@db.example.com:5432/app",
                "postgres://alice:[redacted]@db.example.com:5432/app",
            ),
            (
                "postgresql://alice:S3cret@db/app?sslmode=require",
                "postgresql://alice:[redacted]@db/app?sslmode=require",
            ),
            (
                "MYSQL://root:S3cret@127.0.0.1:3306/shop",
                "MYSQL://root:[redacted]@127.0.0.1:3306/shop",
            ),
            (
                "mariadb://root:S3cret@localhost/shop",
                "mariadb://root:[redacted]@localhost/shop",
            ),
            (
                "redis://:S3cret@cache:6379/0",
                "redis://:[redacted]@cache:6379/0",
            ),
            (
                "rediss://default:S3cret@cache:6380",
                "rediss://default:[redacted]@cache:6380",
            ),
            (
                "postgres://u:p@ss/w:rd@host/db",
                "postgres://u:[redacted]@host/db",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_urls(input, UrlScope::Content), expected, "{input}");
        }
    }

    #[test]
    fn urls_without_password_are_untouched() {
        for input in [
            "postgres://alice@db/app",
            "https://example.com/a@b",
            "redis://cache:6379",
            "no url here",
            "://nothing",
        ] {
            assert_eq!(redact_urls(input, UrlScope::Content), input);
        }
    }

    #[test]
    fn code_scope_stops_at_quotes() {
        let input = "x 'postgres://u:pw' || '@host'";
        assert_eq!(redact_urls(input, UrlScope::Code), input);
        assert_eq!(
            redact_urls("postgres://u:pw@h", UrlScope::Code),
            "postgres://u:[redacted]@h"
        );
    }

    #[test]
    fn key_value_secrets_are_redacted() {
        let cases = [
            (
                "host=db port=5432 password=S3cret dbname=app",
                "host=db port=5432 password=[redacted] dbname=app",
            ),
            (
                "host=db PASSWORD = 'S3 cret' dbname=app",
                "host=db PASSWORD = '[redacted]' dbname=app",
            ),
            (
                "host=db password=''S3cret'' dbname=app",
                "host=db password=''[redacted]'' dbname=app",
            ),
            (
                "jdbc:postgresql://db/app?user=a&password=S3cret&ssl=true",
                "jdbc:postgresql://db/app?user=a&password=[redacted]&ssl=true",
            ),
            (
                "Server=db;Uid=a;Pwd=S3cret;",
                "Server=db;Uid=a;Pwd=[redacted];",
            ),
            (
                r#"{"user":"a","password":"S3cret"}"#,
                r#"{"user":"a","password":"[redacted]"}"#,
            ),
            (
                r#"{\"password\": \"S3cret\"}"#,
                r#"{\"password\": \"[redacted]\"}"#,
            ),
            ("sslpassword=S3cret", "sslpassword=[redacted]"),
            ("\"password\"=>\"S3cret\"", "\"password\"=>\"[redacted]\""),
            ("requirepass=S3cret", "requirepass=[redacted]"),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_key_values(input), expected, "{input}");
        }
    }

    #[test]
    fn escaped_and_doubled_quotes_stay_inside_the_value() {
        let cases = [
            // SQL-doubled quote inside an unquoted value (raw literal text).
            ("host=h password=ab''cdef", "host=h password=[redacted]"),
            ("password=ab\\'cdef x", "password=[redacted] x"),
            ("password=ab\\cd;x", "password=[redacted];x"),
            ("password=ab\"\"cd&x=1", "password=[redacted]&x=1"),
            // Quoted values with escaped or doubled quotes inside.
            ("password='ab\\'cd' x=1", "password='[redacted]' x=1"),
            ("password='ab''cd' x=1", "password='[redacted]' x=1"),
            ("password=''ab\\''cd'' x=1", "password=''[redacted]'' x=1"),
            ("password=''ab''''cd'' x=1", "password=''[redacted]'' x=1"),
            (
                r#"{\"password\":\"ab\\\"cd\"}"#,
                r#"{\"password\":\"[redacted]\"}"#,
            ),
            ("password=ü\\é' x", "password=[redacted]' x"),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_key_values(input), expected, "{input}");
        }
    }

    #[test]
    fn api_keys_tokens_and_headers() {
        for (input, expected) in [
            ("api_key=k3yZq9w", "api_key=[redacted]"),
            (
                "https://h/x?api-key=k3yZq9w&q=1",
                "https://h/x?api-key=[redacted]&q=1",
            ),
            ("apikey=k3yZq9w", "apikey=[redacted]"),
            (
                "access_key=k3yZq9w accesskey=k3yZq9w",
                "access_key=[redacted] accesskey=[redacted]",
            ),
            (
                "secret_key=k3yZq9w secretkey=k3yZq9w",
                "secret_key=[redacted] secretkey=[redacted]",
            ),
            ("client_secret=k3yZq9w", "client_secret=[redacted]"),
            (
                "auth_token=k3yZq9w&refresh_token=k3yZq9w",
                "auth_token=[redacted]&refresh_token=[redacted]",
            ),
            ("access_token=k3yZq9w", "access_token=[redacted]"),
            ("private_key=k3yZq9w", "private_key=[redacted]"),
            (
                "https://acct.blob/c?sv=2024&sas=k3yZq9w",
                "https://acct.blob/c?sv=2024&sas=[redacted]",
            ),
            (
                r#"{"api_key": "k3yZq9w", "user": "a"}"#,
                r#"{"api_key": "[redacted]", "user": "a"}"#,
            ),
            (
                "Authorization: Bearer k3yZq9w.abc",
                "Authorization: Bearer [redacted]",
            ),
            (
                "authorization: Basic dTpwdw==\nHost: h",
                "authorization: Basic [redacted]\nHost: h",
            ),
            ("Authorization: k3yZq9w", "Authorization: [redacted]"),
            ("X-Api-Key: k3yZq9w", "X-Api-Key: [redacted]"),
            ("X-Auth-Token: k3yZq9w", "X-Auth-Token: [redacted]"),
        ] {
            assert_eq!(redact_credentials(input), expected, "{input}");
        }
        // Prose and ordinary headers are left alone.
        for input in [
            "(using password: YES)",
            "Content-Type: application/json",
            "the key is set",
            "monkey=1",
        ] {
            assert_eq!(redact_credentials(input), input, "{input}");
        }
    }

    #[test]
    fn conninfo_without_equals_sign() {
        assert_eq!(
            redact_credentials("host=db password hunter2xyz dbname=x"),
            "host=db password [redacted] dbname=x"
        );
        // Not a connection string: plain prose is left alone.
        for text in [
            "password authentication failed for user alice",
            "the password is set",
        ] {
            assert_eq!(redact_credentials(text), text);
        }
    }

    #[test]
    fn splice_reports_removed_fragments_once() {
        let mut removed = Vec::new();
        let out = splice(
            "a=[redacted] pw",
            vec![(2, 12), (13, 15), (13, 14)],
            &mut removed,
        );
        assert_eq!(out, "a=[redacted] [redacted]");
        assert_eq!(removed, ["pw"]);
    }

    #[test]
    fn non_secret_keys_and_comparisons_are_untouched() {
        for input in [
            "user=alice host=db",
            "password_hash is set",
            "password == other",
            "password::text",
            "(using password: YES)",
            "compassion",
        ] {
            assert_eq!(redact_key_values(input), input, "{input}");
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = redact_credentials("postgres://a:b@h password=c");
        let twice = redact_credentials(&once);
        assert_eq!(once, twice);
        assert_eq!(once, "postgres://a:[redacted]@h password=[redacted]");
    }
}
