//! Error messages under the text levels of design doc 9.3.
//!
//! Servers echo statement text in their errors:
//!
//! - PostgreSQL: `syntax error at or near "hunter2"`
//! - MySQL: `... near 'IDENTIFED BY 'hunter2'' at line 1`
//! - Redis: `ERR unknown command 'AUHT', with args beginning with: 'hunter2'`
//!
//! So a server message is statement text too:
//!
//! | Level | Message stored |
//! |---|---|
//! | `none` | [`ERROR_MESSAGE_HIDDEN`] (the code is kept) |
//! | `normalized` | echoed and quoted fragments, and the literal values of the statement, replaced by `?`; for syntax errors (PostgreSQL `42601`, MySQL `1064` / `1149`, Redis unknown command) [`SYNTAX_ERROR_HIDDEN`] |
//! | `full` | the message after the secret redactor of its dialect |
//!
//! At every level, credentials are redacted, and so is every secret the
//! redactor removed from the statement of the same event. When that
//! statement could not be redacted and was hidden whole, its secrets are
//! unknown and the message is replaced by [`ERROR_MESSAGE_WITHHELD`].

use std::borrow::Cow;
use std::collections::HashSet;

use aho_corasick::{AhoCorasick, AhoCorasickKind, Anchored, Input, MatchKind, StartKind};

use super::credentials::{Ranges, redact_credentials, splice};
use super::message_words::MESSAGE_WORDS;
use super::{REDACTED, SqlDialect, TextLevel, redact_secrets};

/// Message stored instead of a server message at [`TextLevel::None`].
pub const ERROR_MESSAGE_HIDDEN: &str = "error message not stored at text level none";

/// Message stored instead of a server message at [`TextLevel::Normalized`]
/// or [`TextLevel::Full`] when the message could not be checked for
/// secrets: it is longer than the redactor reads, a secret removed from
/// the statement is too short to find reliably, or the search would take
/// too long. The code is kept.
pub const ERROR_MESSAGE_OMITTED: &str =
    "error message not stored: it could not be checked for secrets";

/// Message stored instead of a server message when the statement of the
/// same event could not be redacted (too long, nested too deep, or over
/// its work budget) and was hidden: its secrets are unknown, so a message
/// that may echo them is not stored either. The code is kept.
pub const ERROR_MESSAGE_WITHHELD: &str =
    "error message not stored: the statement could not be redacted";

/// Message stored instead of a syntax error message at
/// [`TextLevel::Normalized`]: such messages quote the statement.
pub const SYNTAX_ERROR_HIDDEN: &str =
    "syntax error (the message quotes the statement and is not stored at text level normalized)";

/// MySQL: `... near '<statement tail>' at line N`.
const MYSQL_NEAR: &str = "near '";
const MYSQL_AT_LINE: &str = "' at line ";
/// PostgreSQL: `... at or near "<token>"`.
const POSTGRES_NEAR: &str = "at or near \"";
/// Redis: `... with args beginning with: '<arg>' '<arg>' `.
const REDIS_ARGS: &str = "with args beginning with:";

/// The message to store for an error at `level`.
///
/// `dialect` is the engine whose server produced the message, or `None` for
/// messages that cannot echo statement text (SSH tunnel errors, client-side
/// errors): those only have their credentials redacted, at every level.
/// `code` is the engine error code (SQLSTATE, MySQL error number, Redis
/// error prefix). Never panics into the caller: should a redactor fail,
/// the message is [`ERROR_MESSAGE_WITHHELD`].
#[must_use]
pub fn prepare_error_message(
    message: &str,
    code: Option<&str>,
    dialect: Option<SqlDialect>,
    level: TextLevel,
) -> String {
    super::guarded("prepare_error_message", || {
        prepare_error_message_with(message, code, dialect, level, Some(&[]), &[], false)
    })
    .unwrap_or_else(|| ERROR_MESSAGE_WITHHELD.to_string())
}

/// Like [`prepare_error_message`], and also removes `statement_secrets`:
/// what the secret redactor removed from the statement of the same event.
/// `None` means the statement could not be redacted and was hidden: the
/// message is then hidden too ([`ERROR_MESSAGE_WITHHELD`]), at every
/// level, since nothing tells which of its words are secrets.
///
/// At [`TextLevel::Normalized`], `statement_literals` (the literal values
/// the normalized statement shows as `?`) become `?` in the message too
/// (all of them when `statement_raises`: the statement holds `RAISE` or
/// `SIGNAL`, so the message may be its own text):
/// `RAISE EXCEPTION 'customer % not found', 'x'` and `SIGNAL ... SET
/// MESSAGE_TEXT = 'x'` put them there.
pub(crate) fn prepare_error_message_with(
    message: &str,
    code: Option<&str>,
    dialect: Option<SqlDialect>,
    level: TextLevel,
    statement_secrets: Option<&[String]>,
    statement_literals: &[String],
    statement_raises: bool,
) -> String {
    // At text level none no server message is stored.
    if level == TextLevel::None && dialect.is_some() {
        return ERROR_MESSAGE_HIDDEN.to_string();
    }
    let Some(statement_secrets) = statement_secrets else {
        return ERROR_MESSAGE_WITHHELD.to_string();
    };
    // A message too long to read through is not stored.
    if message.len() > MAX_MESSAGE_BYTES {
        return ERROR_MESSAGE_OMITTED.to_string();
    }
    // A removed unquoted run can end in a separator the server does not
    // echo (`abc,` of `PASSWORD=abc, ...`): the trimmed form counts too.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut secrets: Vec<&str> = Vec::new();
    for secret in statement_secrets {
        let secret = secret.trim();
        for form in [secret, secret.trim_end_matches([',', ')', ';'])] {
            if !form.is_empty() && form != REDACTED && seen.insert(form) {
                secrets.push(form);
            }
        }
    }
    if secrets.iter().any(|s| s.chars().count() < MIN_SECRET_CHARS) {
        // Too short to find reliably: as a token it may be echoed in part,
        // as a substring it would hit ordinary words.
        return ERROR_MESSAGE_OMITTED.to_string();
    }
    let text = match (dialect, level) {
        (None, _) => message.to_string(),
        (Some(_), TextLevel::None) => return ERROR_MESSAGE_HIDDEN.to_string(),
        (Some(dialect), TextLevel::Normalized) => {
            if is_syntax_error(code, message, dialect) {
                return SYNTAX_ERROR_HIDDEN.to_string();
            }
            let text = drop_quoted(&drop_details(&drop_echoes(message)));
            match drop_literals(
                &text,
                statement_literals,
                statement_raises || raised_by_user(code, dialect),
            ) {
                Some(text) => text,
                None => return ERROR_MESSAGE_OMITTED.to_string(),
            }
        }
        // A Redis message is not a command: running the command redactor
        // on `WRONGPASS invalid ...` would only mangle it.
        (Some(SqlDialect::Redis), TextLevel::Full) => message.to_string(),
        (Some(dialect), TextLevel::Full) => {
            redact_secrets(&redact_echoes(message, dialect), dialect)
        }
    };
    let text = redact_credentials(&text).into_owned();
    let text = redact_echo_overlap(&text, &secrets);
    match remove_secrets(&text, &secrets) {
        Some(text) => text.into_owned(),
        None => ERROR_MESSAGE_OMITTED.to_string(),
    }
}

/// Literal values of the statement replaced by `?` in a message, at
/// [`TextLevel::Normalized`]. Values shorter than [`MIN_LITERAL_CHARS`]
/// characters are left: as tokens they are common words and numbers. A
/// value that is a single word of the servers' message templates
/// ([`MESSAGE_WORDS`]: `null`, `division`, `deadlock`) is left too, so the
/// template stays readable, unless the message was raised by the user
/// (`RAISE`, `SIGNAL`): its whole text then comes from the statement.
/// `None` when the search would need more than its budget.
fn drop_literals(text: &str, literals: &[String], user_message: bool) -> Option<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let needles: Vec<&str> = literals
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.chars().count() >= MIN_LITERAL_CHARS && seen.insert(l))
        .filter(|l| user_message || !template_word(l))
        .collect();
    replace_tokens(text, &needles, "?").map(Cow::into_owned)
}

/// True for a single word of the servers' message templates.
fn template_word(value: &str) -> bool {
    value.bytes().all(|b| b.is_ascii_alphabetic())
        && MESSAGE_WORDS
            .binary_search(&value.to_ascii_lowercase().as_str())
            .is_ok()
}

/// True for an error the statement raised itself, whose message text is
/// the statement's own: PostgreSQL `RAISE` (SQLSTATE class `P0`), MySQL and
/// MariaDB `SIGNAL` / `RESIGNAL` (1642 to 1644, SQLSTATE `45000`).
fn raised_by_user(code: Option<&str>, dialect: SqlDialect) -> bool {
    let code = code.map(str::trim).unwrap_or_default();
    match dialect {
        SqlDialect::Postgres => code.starts_with("P0"),
        SqlDialect::Mysql => {
            matches!(
                code.split_whitespace().next().unwrap_or_default(),
                "1642" | "1643" | "1644"
            ) || code.contains("45000")
        }
        SqlDialect::Redis => false,
    }
}

/// Literal values shorter than this many characters stay in a message at
/// [`TextLevel::Normalized`].
const MIN_LITERAL_CHARS: usize = 4;

/// Longest error message read, in bytes; a longer one is hidden.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// A removed secret shorter than this many characters hides the message.
const MIN_SECRET_CHARS: usize = 4;

/// Content range of the statement fragment a server echoed (without its
/// quotes).
fn echo_content(message: &str) -> Vec<(usize, usize)> {
    [mysql_echo(message), postgres_echo(message)]
        .into_iter()
        .flatten()
        .map(|(open, end)| {
            let quote = message.as_bytes()[open];
            let closed = end > open + 1 && message.as_bytes()[end - 1] == quote;
            (open + 1, if closed { end - 1 } else { end })
        })
        .collect()
}

/// The longest `k` such that the last `k` bytes of `a` equal the first `k`
/// bytes of `b`, on character boundaries.
fn overlap(a: &str, b: &str) -> usize {
    // Only the last `m` bytes of `a` and the first `m` of `b` can overlap.
    let m = a.len().min(b.len());
    let (a_tail, b_head) = (&a.as_bytes()[a.len() - m..], &b.as_bytes()[..m]);
    // Prefix function (KMP) of `b_head`, then run `a_tail` through it: the
    // state at the end is the longest suffix of `a` that is a prefix of
    // `b`, in time linear in `m`.
    let mut failure = vec![0usize; m];
    let mut k = 0;
    for i in 1..m {
        while k > 0 && b_head[i] != b_head[k] {
            k = failure[k - 1];
        }
        if b_head[i] == b_head[k] {
            k += 1;
        }
        failure[i] = k;
    }
    let mut state = 0;
    for &byte in a_tail {
        while state > 0 && (state == m || byte != b_head[state]) {
            state = failure[state - 1];
        }
        if state < m && byte == b_head[state] {
            state += 1;
        }
    }
    // Shorter overlaps along the failure chain, until one sits on
    // character boundaries.
    while state > 0 && !(b.is_char_boundary(state) && a.is_char_boundary(a.len() - state)) {
        state = failure[state - 1];
    }
    state
}

/// Redacts the part of an echoed statement fragment that belongs to a
/// secret: servers echo from the error position, which can be inside an
/// unquoted password (`near '@ss!w0rd'`, `at or near "123h"`), and MySQL
/// cuts the echo short, which can end inside one.
fn redact_echo_overlap(text: &str, secrets: &[&str]) -> String {
    let mut ranges: Ranges = Vec::new();
    for (start, end) in echo_content(text) {
        let fragment = &text[start..end];
        if fragment.is_empty() {
            continue;
        }
        for secret in secrets {
            // A secret shorter than the fragment cannot contain it.
            if secret.len() >= fragment.len() && secret.contains(fragment) {
                ranges.push((start, end));
                continue;
            }
            let head = overlap(secret, fragment);
            if head >= 2 {
                ranges.push((start, start + head));
            }
            let tail = overlap(fragment, secret);
            if tail >= 2 {
                ranges.push((end - tail, end));
            }
        }
    }
    splice(text, ranges, &mut Vec::new()).into_owned()
}

/// Syntax errors quote the statement around the error, so their message is
/// dropped whole below [`TextLevel::Full`].
fn is_syntax_error(code: Option<&str>, message: &str, dialect: SqlDialect) -> bool {
    let code = code.map(str::trim).unwrap_or_default();
    match dialect {
        SqlDialect::Postgres => code == "42601",
        SqlDialect::Mysql => {
            // The driver reports `1064 (42000)`.
            let number = code.split_whitespace().next().unwrap_or_default();
            matches!(number, "1064" | "1149")
                || message.starts_with("You have an error in your SQL syntax")
        }
        SqlDialect::Redis => message.to_ascii_lowercase().contains("unknown command"),
    }
}

/// Byte range of the echoed MySQL statement tail, quotes included.
fn mysql_echo(message: &str) -> Option<(usize, usize)> {
    let open = message.find(MYSQL_NEAR)? + MYSQL_NEAR.len() - 1;
    let end = message[open + 1..]
        .rfind(MYSQL_AT_LINE)
        .map_or(message.len(), |p| open + 1 + p + 1);
    Some((open, end))
}

/// Byte range of the echoed PostgreSQL token, quotes included. The token
/// itself may hold quotes, so the range ends at the last quote.
fn postgres_echo(message: &str) -> Option<(usize, usize)> {
    let open = message.find(POSTGRES_NEAR)? + POSTGRES_NEAR.len() - 1;
    let end = message[open + 1..]
        .rfind('"')
        .map_or(message.len(), |p| open + 1 + p + 1);
    Some((open, end))
}

/// Byte range of the arguments Redis echoes after an unknown command.
fn redis_echo(message: &str) -> Option<(usize, usize)> {
    let start = message.find(REDIS_ARGS)? + REDIS_ARGS.len();
    Some((start, message.len()))
}

/// Replaces every echoed statement fragment with `?`.
fn drop_echoes(message: &str) -> String {
    let mut text = message.to_string();
    if let Some((start, end)) = mysql_echo(&text) {
        text.replace_range(start..end, "?");
    }
    if let Some((start, end)) = postgres_echo(&text) {
        text.replace_range(start..end, "?");
    }
    if let Some((start, end)) = redis_echo(&text) {
        text.replace_range(start..end, " ?");
    }
    text
}

/// Runs the echoed MySQL statement tail through the secret redactor. The
/// whole message is redacted afterwards, but its quotes do not line up with
/// those of the echoed statement, so the tail is redacted on its own first.
fn redact_echoes(message: &str, dialect: SqlDialect) -> String {
    let Some((start, end)) = mysql_echo(message) else {
        return message.to_string();
    };
    let quoted = end > start + 1 && message[start + 1..end].ends_with('\'');
    let content_end = if quoted { end - 1 } else { end };
    let tail = &message[start + 1..content_end];
    format!(
        "{}{}{}",
        &message[..=start],
        redact_secrets(tail, dialect),
        &message[content_end..]
    )
}

/// Replaces every quoted fragment (`'...'`, `"..."`, `` `...` ``) with `?`.
/// A quote opens a fragment unless it follows a letter or digit (an
/// apostrophe), and closes it when no letter or digit follows. An
/// unterminated fragment runs to the end.
fn drop_quoted(message: &str) -> String {
    let bytes = message.as_bytes();
    let mut out = String::with_capacity(message.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let quote = bytes[i];
        let opens = matches!(quote, b'\'' | b'"' | b'`')
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric());
        if !opens {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len()
            && !(bytes[j] == quote && bytes.get(j + 1).is_none_or(|c| !c.is_ascii_alphanumeric()))
        {
            j += 1;
        }
        let end = (j + 1).min(bytes.len());
        out.push_str(&message[copied..i]);
        out.push('?');
        copied = end;
        i = end;
    }
    out.push_str(&message[copied..]);
    out
}

fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// How much search work [`remove_secrets`] may do per byte of message, and
/// regardless of its length; a message that needs more is hidden.
const SEARCH_STEPS_PER_BYTE: usize = 64;
const SEARCH_BASE_STEPS: usize = 1 << 16;

/// True when `text[start..end]` stands as a whole token: not glued to a
/// letter, digit or `_` on a side where the match itself ends in one.
fn whole_token(text: &str, start: usize, end: usize) -> bool {
    let found = &text[start..end];
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    let joined = |c: Option<char>, edge: Option<char>| {
        c.is_some_and(is_token_char) && edge.is_some_and(is_token_char)
    };
    !joined(before, found.chars().next()) && !joined(after, found.chars().next_back())
}

/// True when the match `text[start..end]` is glued to a token character
/// on its left: every match starting there is.
fn joined_before(text: &str, start: usize) -> bool {
    let before = text[..start].chars().next_back();
    let first = text[start..].chars().next();
    before.is_some_and(is_token_char) && first.is_some_and(is_token_char)
}

/// Replaces every whole-token occurrence of a needle (a secret, a
/// literal) in `text` with `replacement`, ignoring ASCII case: an
/// occurrence inside a longer word (`p` in `corresponds`) is not the
/// needle.
///
/// The search takes the leftmost-longest match from each position and
/// merges what it found as it goes, so memory stays linear in the message
/// whatever the secrets. A longest match glued to a word on its right is
/// retried with the shorter secrets that start at the same place. Every
/// step is charged to a budget linear in the message length; `None` when
/// the search would need more, and the message must be hidden.
fn replace_tokens<'a>(text: &'a str, secrets: &[&str], replacement: &str) -> Option<Cow<'a, str>> {
    // A secret longer than the message cannot occur in it.
    let secrets: Vec<&str> = secrets
        .iter()
        .copied()
        .filter(|s| s.len() <= text.len())
        .collect();
    if secrets.is_empty() {
        return Some(Cow::Borrowed(text));
    }
    // One automaton for all secrets. ASCII case is ignored: PostgreSQL
    // folds an unquoted name to lower case before it echoes it (`column
    // "zq9wsecret" does not exist`). The contiguous NFA builds in time
    // linear in the total length of the secrets (a DFA would not).
    let Ok(automaton) = AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .match_kind(MatchKind::LeftmostLongest)
        .start_kind(StartKind::Both)
        .kind(Some(AhoCorasickKind::ContiguousNFA))
        .build(&secrets)
    else {
        // The secrets could not be searched for: hide the message.
        return None;
    };
    let budget = text
        .len()
        .saturating_mul(SEARCH_STEPS_PER_BYTE)
        .saturating_add(SEARCH_BASE_STEPS);
    let mut spent = 0usize;
    let mut ranges: Ranges = Vec::new();
    let mut from = 0;
    while from < text.len() {
        let Some(found) = automaton.find(Input::new(text).span(from..text.len())) else {
            break;
        };
        let start = found.start();
        let mut end = found.end();
        spent = spent.saturating_add(end - from + 1);
        loop {
            if spent > budget {
                return None;
            }
            if !text.is_char_boundary(start) || joined_before(text, start) {
                break;
            }
            if text.is_char_boundary(end) && whole_token(text, start, end) {
                match ranges.last_mut() {
                    Some(last) if start <= last.1 => last.1 = last.1.max(end),
                    _ => ranges.push((start, end)),
                }
                break;
            }
            // Glued on the right: a shorter secret from the same start may
            // still stand alone (`pass` in `pass words` when `pass word`
            // is a secret too).
            let shorter = Input::new(text)
                .span(start..end - 1)
                .anchored(Anchored::Yes);
            spent = spent.saturating_add(end - start);
            match automaton.find(shorter) {
                Some(m) => end = m.end(),
                None => break,
            }
        }
        // Matches that start inside this one are still searched for: they
        // may run past its end.
        from = start + 1;
        while from < text.len() && !text.is_char_boundary(from) {
            from += 1;
        }
    }
    if ranges.is_empty() {
        return Some(Cow::Borrowed(text));
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, end) in ranges {
        out.push_str(&text[copied..start]);
        out.push_str(replacement);
        copied = end;
    }
    out.push_str(&text[copied..]);
    Some(Cow::Owned(out))
}

/// Replaces every whole-token occurrence of a secret in `text` with
/// [`REDACTED`] ([`replace_tokens`]); `None` when the search would need
/// more than its budget.
fn remove_secrets<'a>(text: &'a str, secrets: &[&str]) -> Option<Cow<'a, str>> {
    replace_tokens(text, secrets, REDACTED)
}

/// Values a server adds after its message (PostgreSQL `DETAIL` and
/// `CONTEXT`, which a caller may append): `Key (col)=(value)`, `Failing
/// row contains (...)` and the data quoted by `CONTEXT` lines become `?`.
fn drop_details(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for (index, line) in message.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("CONTEXT:") {
            out.push_str(&line[..line.len() - trimmed.len()]);
            out.push_str("CONTEXT: ?");
            continue;
        }
        out.push_str(&drop_parenthesized_values(line));
    }
    out
}

/// What PostgreSQL writes after the values of a `Key (cols)=(values)`
/// detail. The values are printed unquoted and may hold `)` and `,`, so
/// the list ends at the last `)` followed by one of these, not at the
/// first balanced `)`.
const KEY_DETAIL_ENDINGS: [&str; 4] = [
    ") already exists",
    ") is not present in table",
    ") is still referenced from table",
    ") conflicts with existing key",
];

/// `...)=(value)` and `contains (values)` with the values replaced by `?`.
/// A value list runs to the last `)` before a known ending of the detail
/// (`) already exists`), or else to the last `)` of the line: values are
/// unquoted and can hold any character, `)` included.
fn drop_parenthesized_values(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let next = [rest.find(")=("), rest.find("contains (")]
            .into_iter()
            .flatten()
            .min();
        let Some(at) = next else {
            out.push_str(rest);
            return out;
        };
        let open = at + rest[at..].find('(').map_or(0, |p| p + 1);
        let tail = &rest[open..];
        let close = KEY_DETAIL_ENDINGS
            .iter()
            .filter_map(|ending| tail.rfind(ending))
            .max()
            .or_else(|| tail.rfind(')'))
            .map_or(rest.len(), |p| open + p);
        out.push_str(&rest[..open]);
        out.push('?');
        rest = &rest[close..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEVELS: [TextLevel; 3] = [TextLevel::None, TextLevel::Normalized, TextLevel::Full];

    /// The three messages from the review, with the statements that caused
    /// them. Each echoes `hunter2`.
    const MYSQL: (&str, &str, &str) = (
        "CREATE USER u IDENTIFED BY 'hunter2'",
        "1064 (42000)",
        "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'IDENTIFED BY 'hunter2'' at line 1",
    );
    const POSTGRES: (&str, &str, &str) = (
        "ALTER ROLE a PASSWORD hunter2",
        "42601",
        "syntax error at or near \"hunter2\"",
    );
    const REDIS: (&str, &str, &str) = (
        "AUHT hunter2",
        "ERR",
        "ERR unknown command 'AUHT', with args beginning with: 'hunter2' ",
    );

    fn prepared(message: &str, code: &str, dialect: SqlDialect, level: TextLevel) -> String {
        prepare_error_message(message, Some(code), Some(dialect), level)
    }

    #[test]
    fn level_none_keeps_no_server_text() {
        for (dialect, (_, code, message)) in [
            (SqlDialect::Mysql, MYSQL),
            (SqlDialect::Postgres, POSTGRES),
            (SqlDialect::Redis, REDIS),
        ] {
            assert_eq!(
                prepared(message, code, dialect, TextLevel::None),
                ERROR_MESSAGE_HIDDEN
            );
        }
        assert_eq!(
            prepared(
                "relation \"users\" does not exist",
                "42P01",
                SqlDialect::Postgres,
                TextLevel::None
            ),
            ERROR_MESSAGE_HIDDEN
        );
    }

    #[test]
    fn normalized_drops_syntax_error_messages() {
        for (dialect, (_, code, message)) in [
            (SqlDialect::Mysql, MYSQL),
            (SqlDialect::Postgres, POSTGRES),
            (SqlDialect::Redis, REDIS),
        ] {
            assert_eq!(
                prepared(message, code, dialect, TextLevel::Normalized),
                SYNTAX_ERROR_HIDDEN
            );
        }
    }

    #[test]
    fn normalized_drops_echoed_and_quoted_fragments() {
        let cases = [
            (
                SqlDialect::Postgres,
                "22P02",
                "invalid input syntax for type integer: \"hunter2\"",
                "invalid input syntax for type integer: ?",
            ),
            (
                SqlDialect::Postgres,
                "XX000",
                "unterminated quoted string at or near \"'hun\"ter2\"",
                "unterminated quoted string at or near ?",
            ),
            (
                SqlDialect::Postgres,
                "42P01",
                "relation \"secret_table\" does not exist",
                "relation ? does not exist",
            ),
            (
                SqlDialect::Mysql,
                "1062 (23000)",
                "Duplicate entry 'it's hunter2' for key 'users.email'",
                "Duplicate entry ? for key ?",
            ),
            (
                SqlDialect::Mysql,
                "1105 (HY000)",
                "Unexpected near 'PASSWORD 'hunter2'' at line 3",
                "Unexpected near ? at line 3",
            ),
            (
                SqlDialect::Redis,
                "ERR",
                "ERR unknown subcommand `hunter2`. Try CONFIG HELP.",
                "ERR unknown subcommand ?. Try CONFIG HELP.",
            ),
            (
                SqlDialect::Redis,
                "ERR",
                "ERR something with args beginning with: `hunter2`, ",
                "ERR something with args beginning with: ?",
            ),
            (
                SqlDialect::Redis,
                "WRONGTYPE",
                "WRONGTYPE Operation against a key holding the wrong kind of value",
                "WRONGTYPE Operation against a key holding the wrong kind of value",
            ),
        ];
        for (dialect, code, message, expected) in cases {
            assert_eq!(
                prepared(message, code, dialect, TextLevel::Normalized),
                expected,
                "{message}"
            );
        }
    }

    #[test]
    fn full_runs_the_redactor_of_the_dialect() {
        // The echoed MySQL statement tail is redacted on its own.
        assert_eq!(
            prepared(
                "You have an error in your SQL syntax; check the manual for the right syntax to use near 'USR u IDENTIFIED BY 'hunter2'' at line 1",
                "1064 (42000)",
                SqlDialect::Mysql,
                TextLevel::Full
            ),
            "You have an error in your SQL syntax; check the manual for the right syntax to use near 'USR u IDENTIFIED BY '[redacted]'' at line 1"
        );
        assert_eq!(
            prepared(
                "could not connect: postgres://u:hunter2@db/x",
                "08006",
                SqlDialect::Postgres,
                TextLevel::Full
            ),
            "could not connect: postgres://u:[redacted]@db/x"
        );
        assert_eq!(
            prepared(
                "ERR invalid address redis://default:hunter2@cache:6379",
                "ERR",
                SqlDialect::Redis,
                TextLevel::Full
            ),
            "ERR invalid address redis://default:[redacted]@cache:6379"
        );
        // Common messages are kept as they are.
        for (dialect, message) in [
            (
                SqlDialect::Postgres,
                "password authentication failed for user \"alice\"",
            ),
            (
                SqlDialect::Mysql,
                "Access denied for user 'bob'@'localhost' (using password: YES)",
            ),
            (SqlDialect::Redis, "WRONGTYPE Operation against a key"),
        ] {
            assert_eq!(prepared(message, "x", dialect, TextLevel::Full), message);
        }
    }

    #[test]
    fn statement_secrets_are_removed_at_every_level() {
        for (dialect, (statement, code, message)) in
            [(SqlDialect::Mysql, MYSQL), (SqlDialect::Postgres, POSTGRES)]
        {
            let (_, secrets) = super::super::redact_secrets_tracked(statement, dialect);
            assert!(secrets.iter().any(|s| s == "hunter2"), "{secrets:?}");
            for level in LEVELS {
                let out = prepare_error_message_with(
                    message,
                    Some(code),
                    Some(dialect),
                    level,
                    Some(&secrets),
                    &[],
                    false,
                );
                assert!(!out.contains("hunter2"), "{level}: {out}");
            }
        }
    }

    #[test]
    fn messages_without_a_dialect_only_lose_credentials() {
        for level in LEVELS {
            assert_eq!(
                prepare_error_message(
                    "ssh: auth failed for sftp://u:hunter2@bastion \"key\"",
                    None,
                    None,
                    level
                ),
                "ssh: auth failed for sftp://u:[redacted]@bastion \"key\""
            );
        }
    }

    #[test]
    fn normalized_drops_detail_and_context_values() {
        for (message, expected) in [
            (
                "insert or update on table \"t\" violates foreign key constraint \"f\"\nDETAIL: Key (ref)=(XYZZY_SECRET_1) is not present in table \"r\".",
                "insert or update on table ? violates foreign key constraint ?\nDETAIL: Key (ref)=(?) is not present in table ?.",
            ),
            (
                "null value in column \"a\" violates not-null constraint\nDETAIL: Failing row contains (null, XYZZY_SECRET_1).",
                "null value in column ? violates not-null constraint\nDETAIL: Failing row contains (?).",
            ),
            // Values are printed unquoted and may hold `)` and `,`.
            (
                "duplicate key value violates unique constraint \"k\"\nDETAIL: Key (a, b)=(x) XYZZY_SECRET_1, y) already exists.",
                "duplicate key value violates unique constraint ?\nDETAIL: Key (a, b)=(?) already exists.",
            ),
            (
                "null value in column \"a\" violates not-null constraint\nDETAIL: Failing row contains (null, a)b XYZZY_SECRET_1).",
                "null value in column ? violates not-null constraint\nDETAIL: Failing row contains (?).",
            ),
            (
                "conflicting key value violates exclusion constraint \"e\"\nDETAIL: Key (r)=(x) XYZZY_SECRET_1) conflicts with existing key (r)=(y) XYZZY_SECRET_1).",
                "conflicting key value violates exclusion constraint ?\nDETAIL: Key (r)=(?) conflicts with existing key (r)=(?).",
            ),
            (
                "invalid input syntax for type json\nDETAIL: Token \"XYZZY_SECRET_1\" is invalid.\nCONTEXT: JSON data, line 1: {\"x\": XYZZY_SECRET_1...",
                "invalid input syntax for type json\nDETAIL: Token ? is invalid.\nCONTEXT: ?",
            ),
        ] {
            assert_eq!(
                prepare_error_message(
                    message,
                    Some("23503"),
                    Some(SqlDialect::Postgres),
                    TextLevel::Normalized
                ),
                expected
            );
        }
    }

    #[test]
    fn many_secrets_and_long_messages_stay_fast() {
        let secrets: Vec<String> = (0..32_000).map(|i| format!("pw{i:06}x")).collect();
        let message = format!(
            "near '{}' at line 1",
            (0..6_000)
                .map(|i| format!("pw{i:06}x"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(message.len() > 60_000);
        let start = std::time::Instant::now();
        let out = prepare_error_message_with(
            &message,
            Some("1105"),
            Some(SqlDialect::Mysql),
            TextLevel::Full,
            Some(&secrets),
            &[],
            false,
        );
        let took = start.elapsed();
        assert!(!out.contains("pw000001x"), "{}", &out[..200]);
        let bound = if cfg!(debug_assertions) {
            10_000
        } else {
            1_000
        };
        assert!(took.as_millis() < bound, "took {took:?}");
    }

    #[test]
    fn a_huge_secret_does_not_slow_the_search() {
        let secrets = vec!["ab".repeat(500_000)];
        let message = format!(
            "at or near \"{}c{}\"",
            "ab".repeat(16_000),
            "ab".repeat(16_000)
        );
        let start = std::time::Instant::now();
        let out = prepare_error_message_with(
            &message,
            Some("XX000"),
            Some(SqlDialect::Postgres),
            TextLevel::Full,
            Some(&secrets),
            &[],
            false,
        );
        let took = start.elapsed();
        assert_eq!(out, "at or near \"[redacted]c[redacted]\"");
        let bound = if cfg!(debug_assertions) { 5_000 } else { 500 };
        assert!(took.as_millis() < bound, "took {took:?}");
    }

    #[test]
    fn nested_secrets_are_searched_in_bounded_time_and_memory() {
        // Secrets that are prefixes of each other and of the message: every
        // position starts a match of every length. The search stops at its
        // budget and hides the message instead of collecting every match.
        let secrets: Vec<String> = (4..1304).map(|n| "-".repeat(n)).collect();
        let message = "-".repeat(60_000);
        let start = std::time::Instant::now();
        let out = prepare_error_message_with(
            &message,
            Some("1105"),
            Some(SqlDialect::Mysql),
            TextLevel::Full,
            Some(&secrets),
            &[],
            false,
        );
        let took = start.elapsed();
        assert_eq!(out, ERROR_MESSAGE_OMITTED);
        let bound = if cfg!(debug_assertions) { 5_000 } else { 500 };
        assert!(took.as_millis() < bound, "took {took:?}");
    }

    #[test]
    fn overlapping_and_shorter_secrets_are_all_removed() {
        // A match that starts inside an earlier one and runs past it.
        assert_eq!(
            remove_secrets("x ab-c-d y", &["ab-c", "c-d"]).as_deref(),
            Some("x [redacted] y")
        );
        // The longest secret is glued to a word; a shorter one from the
        // same start stands alone.
        assert_eq!(
            remove_secrets("pass words", &["pass word", "pass"]).as_deref(),
            Some("[redacted] words")
        );
        // Whole tokens only, any ASCII case.
        assert_eq!(
            remove_secrets("Zq9wSecret corresponds zq9wsecret", &["zq9wsecret", "p"]).as_deref(),
            Some("[redacted] corresponds [redacted]")
        );
    }

    #[test]
    fn overlap_is_the_longest_suffix_prefix() {
        assert_eq!(overlap("p@ss!w0rdXY", "w0rdXY; SELECT 1"), 6);
        assert_eq!(overlap("abcabc", "abcabd"), 3);
        assert_eq!(overlap("abc", "xyz"), 0);
        assert_eq!(overlap("é1é", "é2"), 2);
    }

    #[test]
    fn secrets_are_replaced_as_whole_tokens_only() {
        assert_eq!(
            remove_secrets(
                "near 'p@ssw0rd' corresponds to p@ssw0rd2 and (p@ssw0rd)",
                &["p@ssw0rd"]
            )
            .as_deref(),
            Some("near '[redacted]' corresponds to p@ssw0rd2 and ([redacted])")
        );
        assert_eq!(
            remove_secrets("hunter2xyz hunter2xyzz xhunter2xyz", &["hunter2xyz"]).as_deref(),
            Some("[redacted] hunter2xyzz xhunter2xyz")
        );
    }

    #[test]
    fn short_secrets_hide_the_message() {
        let secrets = vec!["p".to_string()];
        for level in [TextLevel::Normalized, TextLevel::Full] {
            assert_eq!(
                prepare_error_message_with(
                    "check the manual that corresponds to your server near 'p'",
                    Some("1064 (42000)"),
                    Some(SqlDialect::Mysql),
                    level,
                    Some(&secrets),
                    &[],
                    false,
                ),
                ERROR_MESSAGE_OMITTED
            );
        }
    }

    #[test]
    fn echoes_that_overlap_a_secret_are_redacted() {
        let secrets = vec!["p@ss!w0rdXY".to_string(), "123hunterXY".to_string()];
        for (message, expected) in [
            (
                "right syntax to use near '@ss!w0rdXY' at line 1",
                "right syntax to use near '[redacted]' at line 1",
            ),
            (
                "trailing junk after numeric literal at or near \"123h\"",
                "trailing junk after numeric literal at or near \"[redacted]\"",
            ),
            (
                "right syntax to use near 'w0rdXY; SELECT 1' at line 1",
                "right syntax to use near '[redacted]; SELECT 1' at line 1",
            ),
            (
                "right syntax to use near 'SELECT 1; ALTER USER u BY p@ss' at line 1",
                "right syntax to use near 'SELECT 1; ALTER USER u BY [redacted]' at line 1",
            ),
        ] {
            assert_eq!(
                prepare_error_message_with(
                    message,
                    Some("1105"),
                    Some(SqlDialect::Mysql),
                    TextLevel::Full,
                    Some(&secrets),
                    &[],
                    false,
                ),
                expected,
                "{message}"
            );
        }
    }
}
