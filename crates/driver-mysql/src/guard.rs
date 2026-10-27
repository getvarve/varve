//! The guard: what a session refuses before a script (or the profile's
//! `init_sql`) runs.
//!
//! **Every session** refuses changes of the client character set
//! ([`check_charset`]): Varve talks utf8mb4 to the server, and its lexer
//! reads scripts as UTF-8. In a multibyte character set such as GBK a quote
//! or backtick byte can be the second byte of a character, so the server
//! would see statements the lexer does not.
//!
//! **Read-only sessions** ([`check`]) also refuse what
//! [`varve_driver::read_only_violation`] cannot see:
//!
//! - **Dynamic SQL.** `PREPARE name FROM @var` and MariaDB's
//!   `EXECUTE IMMEDIATE <expression>` build their statement at run time, so
//!   its text cannot be checked; they are refused. A `PREPARE` or
//!   `EXECUTE IMMEDIATE` from string literals is unescaped and checked like
//!   any other script.
//! - **Text whose statement boundaries depend on the server.** The shared
//!   splitter follows MySQL's comment rules and reads `/*! ... */` as code,
//!   but a backslash before a quote ends a string only under
//!   `NO_BACKSLASH_ESCAPES`; such scripts are refused
//!   ([`varve_driver::split_statements_strict`]). Executable comments are
//!   refused as well, since the server runs their contents only on some
//!   versions.
//! - **Writes outside the transaction access mode**: `SELECT ... INTO
//!   OUTFILE` and `INTO DUMPFILE` (files on the server), `SET GLOBAL` and
//!   `SET PERSIST` (server settings), XA transactions (a failed rollback
//!   would leave writes possible), server administration (`FLUSH`, `RESET`,
//!   `PURGE`, `KILL`, `SHUTDOWN`, `RESTART`, `INSTALL`, `UNINSTALL`, `CLONE`,
//!   `BINLOG`, `CHANGE`, `ALTER INSTANCE`, starting or stopping
//!   replication), and changes of the `session_track_*` variables the
//!   session relies on.

use varve_driver::{Dialect, DriverError, ErrorKind, read_only_violation, split_statements_strict};

/// Why dynamic SQL is refused, part of every such error message.
pub(crate) const DYNAMIC_SQL: &str = "dynamic SQL cannot be checked in a read-only session";

/// Checks a script (or `init_sql`) for a read-only session; `what` names
/// it in the message ("the script", "the init SQL").
pub(crate) fn check(sql: &str, what: &str) -> Result<(), DriverError> {
    check_depth(sql, what, 0)
}

/// Prepared text is checked like a script; it cannot nest much (MySQL does
/// not prepare `PREPARE`), the limit only bounds the work.
const MAX_DEPTH: u32 = 4;

fn check_depth(sql: &str, what: &str, depth: u32) -> Result<(), DriverError> {
    if let Some(statement) = read_only_violation(sql, Dialect::Mysql) {
        return Err(violation(statement, what));
    }
    let boundaries = || {
        refused(
            what,
            sql,
            "has statement boundaries that depend on how the server treats backslashes in \
             strings (NO_BACKSLASH_ESCAPES), so it cannot be checked in a read-only session",
        )
    };
    let statements = split_statements_strict(sql, Dialect::Mysql).map_err(|_| boundaries())?;
    if [true, false]
        .into_iter()
        .any(|backslash_escapes| executable_comment(sql, backslash_escapes))
    {
        return Err(refused(
            what,
            sql,
            "contains an executable /*! */ comment, whose contents the server runs only on some \
             versions, so it cannot be checked in a read-only session",
        ));
    }
    for text in statements.iter().map(|statement| statement.text) {
        if let Some(why) = server_write(text) {
            return Err(refused(what, text, why));
        }
        if let Some(source) = dynamic_source(text) {
            let Some(prepared) = literal_text(source) else {
                return Err(refused(
                    what,
                    text,
                    &format!(
                        "builds a statement at run time, and {DYNAMIC_SQL} (only string \
                         literals without backslashes can be checked)"
                    ),
                ));
            };
            if depth >= MAX_DEPTH {
                return Err(refused(what, text, "nests dynamic SQL too deeply"));
            }
            check_depth(&prepared, what, depth + 1).map_err(|error| {
                refused(
                    what,
                    text,
                    &format!("prepares a statement that is refused: {}", error.message),
                )
            })?;
        }
    }
    Ok(())
}

/// The error for a statement that would switch read-only mode off.
pub(crate) fn violation(statement: &str, what: &str) -> DriverError {
    DriverError::new(
        ErrorKind::ReadOnly,
        format!(
            "The connection is read-only and {what} contains a statement that would switch \
             read-only mode off: {}. Nothing was run.",
            shorten(statement)
        ),
    )
}

fn refused(what: &str, statement: &str, why: &str) -> DriverError {
    DriverError::new(
        ErrorKind::ReadOnly,
        format!(
            "The connection is read-only and {what} contains a statement that {why}: {}. \
             Nothing was run.",
            shorten(statement)
        ),
    )
}

fn shorten(statement: &str) -> String {
    const SHOWN: usize = 200;
    match statement.char_indices().nth(SHOWN) {
        Some((end, _)) => format!("{}...", &statement[..end]),
        None => statement.to_owned(),
    }
}

/// True when `--` starts a comment in MySQL: it must be followed by a
/// space or a control character (or end the text).
fn dash_comment(text: &[u8], at: usize) -> bool {
    text.get(at) == Some(&b'-')
        && text.get(at + 1) == Some(&b'-')
        && text.get(at + 2).is_none_or(|next| *next <= b' ')
}

/// Index after a quoted string or identifier that starts at `start`; the
/// end of the text when it is not closed.
fn skip_quoted(text: &[u8], start: usize, backslash_escapes: bool) -> usize {
    quoted_end(text, start, backslash_escapes).unwrap_or(text.len())
}

/// Index after the closing quote of the quoted string or identifier that
/// starts at `start`, or `None` when it is not closed.
fn quoted_end(text: &[u8], start: usize, backslash_escapes: bool) -> Option<usize> {
    let quote = *text.get(start)?;
    let mut i = start + 1;
    while i < text.len() {
        let c = text[i];
        if backslash_escapes && quote != b'`' && c == b'\\' {
            i += 2;
        } else if c == quote {
            if text.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return Some(i + 1);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Index after the comment that starts at `start` (`#`, `--` or `/*`),
/// or `None` when no comment starts there.
fn skip_comment(text: &[u8], start: usize) -> Option<usize> {
    let line_end = |from: usize| {
        text[from..]
            .iter()
            .position(|c| *c == b'\n')
            .map_or(text.len(), |offset| from + offset + 1)
    };
    match text.get(start) {
        Some(b'#') => Some(line_end(start)),
        Some(b'-') if dash_comment(text, start) => Some(line_end(start)),
        Some(b'/') if text.get(start + 1) == Some(&b'*') => Some(
            text[start + 2..]
                .windows(2)
                .position(|pair| pair == b"*/")
                .map_or(text.len(), |offset| start + 2 + offset + 2),
        ),
        _ => None,
    }
}

/// True when the text holds an executable comment (`/*!` or MariaDB
/// `/*M!`) outside strings, under MySQL's rules with the given backslash
/// mode. Bytes are enough: every delimiter is ASCII, and UTF-8 never uses
/// ASCII bytes inside a multi-byte character.
fn executable_comment(sql: &str, backslash_escapes: bool) -> bool {
    let text = sql.as_bytes();
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'\'' | b'"' | b'`' => i = skip_quoted(text, i, backslash_escapes),
            b'/' if text.get(i + 1) == Some(&b'*') => {
                let body = &text[i + 2..];
                if body.starts_with(b"!") || body.starts_with(b"M!") {
                    return true;
                }
                i = skip_comment(text, i).unwrap_or(text.len());
            }
            b'#' | b'-' => i = skip_comment(text, i).unwrap_or(i + 1),
            _ => i += 1,
        }
    }
    false
}

/// Skips whitespace and comments.
fn skip_blank(text: &[u8], mut i: usize) -> usize {
    loop {
        while text.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        match skip_comment(text, i) {
            Some(next) => i = next,
            None => return i,
        }
    }
}

/// Reads a bare word (letters, digits, `_`, `$`, any non-ASCII byte).
fn word(text: &[u8], start: usize) -> (&[u8], usize) {
    let end = text[start..]
        .iter()
        .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$' || *c >= 0x80))
        .map_or(text.len(), |offset| start + offset);
    (&text[start..end], end)
}

/// A word of a statement, upper-cased, with what stands right before it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    text: String,
    /// `@name`: a user variable, not a keyword.
    user_variable: bool,
    /// `@@name`: a system variable read or written with `@@`.
    system_variable: bool,
    /// A `'...'` or `"..."` string (its text is the contents).
    literal: bool,
    /// Parenthesis depth.
    depth: u32,
    /// A comma stands right before it.
    after_comma: bool,
    /// An opening parenthesis follows it (a function call, a type length,
    /// a column list).
    before_paren: bool,
}

/// True when the next non-blank byte at or after `at` is `(`.
fn paren_follows(text: &[u8], at: usize) -> bool {
    text.get(at..)
        .and_then(|rest| rest.iter().find(|byte| !byte.is_ascii_whitespace()))
        == Some(&b'(')
}

impl Word {
    /// A bare word or backquoted identifier: no variable, no string.
    fn is_keyword(&self) -> bool {
        !self.user_variable && !self.system_variable && !self.literal
    }
}

/// The words of one statement under MySQL's rules with the given backslash
/// mode: bare words, backquoted identifiers and strings (marked as
/// literals), outside comments; executable comments count as code.
fn words(statement: &str, backslash_escapes: bool) -> Vec<Word> {
    scan(
        statement,
        Reading {
            backslash_escapes,
            ansi_quotes: false,
            executable: Executable::Code(None),
        },
    )
    .into_iter()
    .flat_map(|(_, words)| words)
    .collect()
}

/// How an executable comment (`/*! ... */`, `/*M! ... */`) is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Executable {
    /// As code, after a version of at most this many digits (`None`: all
    /// leading digits). MySQL 8.0 reads five digits, MySQL 8.4 and later
    /// and MariaDB also six, so `/*!800001` and `/*!1006001` differ.
    Code(Option<usize>),
    /// As a comment (a server whose version is lower, or MySQL for `/*M!`).
    Comment,
}

/// One way the server may read a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    /// Backslash escapes in strings (off with `NO_BACKSLASH_ESCAPES`).
    backslash_escapes: bool,
    /// `"..."` quotes an identifier (`ANSI_QUOTES`), not a string.
    ansi_quotes: bool,
    executable: Executable,
}

/// Every reading the charset check considers.
fn readings() -> Vec<Reading> {
    let mut out = Vec::new();
    for backslash_escapes in [true, false] {
        for ansi_quotes in [false, true] {
            for executable in [
                Executable::Code(Some(5)),
                Executable::Code(Some(6)),
                Executable::Comment,
            ] {
                out.push(Reading {
                    backslash_escapes,
                    ansi_quotes,
                    executable,
                });
            }
        }
    }
    out
}

/// The statements of `sql` under one reading: the byte range of each and
/// its words. Statements end at `;` outside strings and comments, as the
/// server splits a multi-statement request.
fn scan(sql: &str, reading: Reading) -> Vec<(std::ops::Range<usize>, Vec<Word>)> {
    let statement = sql;
    let backslash_escapes = reading.backslash_escapes;
    let mut statements = Vec::new();
    let mut start = 0;
    let text = statement.as_bytes();
    let mut out: Vec<Word> = Vec::new();
    let mut ats = 0;
    let mut depth: u32 = 0;
    let mut after_comma = false;
    let mut i = 0;
    while i < text.len() {
        let c = text[i];
        let (word, next) = match c {
            // Executable comments are code to the server.
            b'/' if text.get(i + 1) == Some(&b'*')
                && (text.get(i + 2) == Some(&b'!')
                    || (text.get(i + 2) == Some(&b'M') && text.get(i + 3) == Some(&b'!'))) =>
            {
                let mut body = i + if text[i + 2] == b'M' { 4 } else { 3 };
                i = match reading.executable {
                    Executable::Comment => skip_comment(text, i).unwrap_or(text.len()),
                    Executable::Code(limit) => {
                        let mut digits = 0;
                        while text.get(body).is_some_and(u8::is_ascii_digit)
                            && limit.is_none_or(|limit| digits < limit)
                        {
                            body += 1;
                            digits += 1;
                        }
                        body
                    }
                };
                continue;
            }
            b'*' if text.get(i + 1) == Some(&b'/') => {
                i += 2;
                continue;
            }
            b'(' => {
                depth += 1;
                i += 1;
                ats = 0;
                after_comma = false;
                continue;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
                ats = 0;
                after_comma = false;
                continue;
            }
            b',' => {
                i += 1;
                ats = 0;
                after_comma = true;
                continue;
            }
            b';' => {
                statements.push((start..i, std::mem::take(&mut out)));
                i += 1;
                start = i;
                ats = 0;
                depth = 0;
                after_comma = false;
                continue;
            }
            c if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            b'"' if reading.ansi_quotes => {
                let end = skip_quoted(text, i, false);
                let inner = statement.get(i + 1..end.saturating_sub(1)).unwrap_or("");
                (inner.replace("\"\"", "\""), end)
            }
            b'\'' | b'"' => {
                let end = skip_quoted(text, i, backslash_escapes);
                let inner = statement.get(i + 1..end.saturating_sub(1)).unwrap_or("");
                // `@'name'` is a user variable with a quoted name.
                out.push(Word {
                    text: inner.to_ascii_uppercase(),
                    user_variable: ats == 1,
                    system_variable: ats >= 2,
                    literal: ats == 0,
                    depth,
                    after_comma,
                    before_paren: paren_follows(text, end),
                });
                i = end;
                ats = 0;
                after_comma = false;
                continue;
            }
            b'`' => {
                let end = skip_quoted(text, i, false);
                let inner = statement.get(i + 1..end.saturating_sub(1)).unwrap_or("");
                (inner.replace("``", "`"), end)
            }
            b'@' => {
                ats += 1;
                i += 1;
                continue;
            }
            b'#' | b'-' | b'/' => match skip_comment(text, i) {
                Some(next) => {
                    i = next;
                    ats = 0;
                    continue;
                }
                None => {
                    i += 1;
                    ats = 0;
                    continue;
                }
            },
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 => {
                let (word, next) = word(text, i);
                (String::from_utf8_lossy(word).into_owned(), next)
            }
            _ => {
                i += 1;
                ats = 0;
                after_comma = false;
                continue;
            }
        };
        out.push(Word {
            text: word.to_ascii_uppercase(),
            user_variable: ats == 1,
            system_variable: ats >= 2,
            literal: false,
            depth,
            after_comma,
            before_paren: paren_follows(text, next),
        });
        ats = 0;
        after_comma = false;
        i = next;
    }
    if !out.is_empty() {
        statements.push((start..text.len(), out));
    }
    statements
}

/// Why a statement writes outside the transaction access mode, or `None`.
fn server_write(statement: &str) -> Option<&'static str> {
    const ADMIN: [&str; 11] = [
        "FLUSH",
        "RESET",
        "PURGE",
        "KILL",
        "SHUTDOWN",
        "RESTART",
        "INSTALL",
        "UNINSTALL",
        "CLONE",
        "BINLOG",
        "CHANGE",
    ];
    const REPLICATION: [&str; 3] = ["REPLICA", "SLAVE", "GROUP_REPLICATION"];
    const SCOPES: [&str; 3] = ["GLOBAL", "PERSIST", "PERSIST_ONLY"];
    for backslash_escapes in [true, false] {
        let words = words(statement, backslash_escapes);
        let keyword = |index: usize| {
            words
                .get(index)
                .filter(|word| !word.user_variable && !word.literal)
                .map(|word| word.text.as_str())
        };
        let Some(first) = keyword(0) else {
            continue;
        };
        if words.windows(2).any(|pair| {
            pair[0].text == "INTO"
                && pair[0].is_keyword()
                && pair[1].is_keyword()
                && matches!(pair[1].text.as_str(), "OUTFILE" | "DUMPFILE")
        }) {
            return Some("writes a file on the server");
        }
        if first == "SET"
            && words.iter().any(|word| {
                !word.user_variable && !word.literal && SCOPES.contains(&word.text.as_str())
            })
        {
            return Some("changes server settings beyond this session");
        }
        if first == "SET"
            && words.iter().any(|word| {
                !word.user_variable && !word.literal && word.text.starts_with("SESSION_TRACK")
            })
        {
            return Some(
                "changes session state tracking, which a read-only session relies on to see \
                 the server switch read-only mode off",
            );
        }
        let replication = matches!(first, "START" | "STOP")
            && words
                .iter()
                .skip(1)
                .any(|word| word.is_keyword() && REPLICATION.contains(&word.text.as_str()));
        let instance = first == "ALTER" && keyword(1) == Some("INSTANCE");
        if first == "XA" {
            return Some(
                "uses an XA transaction, which a read-only session refuses because rolling it \
                 back from the server side can fail",
            );
        }
        if ADMIN.contains(&first) || replication || instance {
            return Some("changes the server outside this session");
        }
    }
    None
}

/// The session variables of the client character set.
const CHARSET_VARIABLES: [&str; 4] = [
    "CHARACTER_SET_CLIENT",
    "CHARACTER_SET_CONNECTION",
    "CHARACTER_SET_RESULTS",
    "COLLATION_CONNECTION",
];

/// Why the client character set may not change, part of every message.
pub(crate) const CHARSET_OWNED: &str =
    "Varve talks utf8mb4 to the server; use CONVERT() or a column charset instead";

/// True when `word` is a plain value equal to utf8mb4 (`collation`: a
/// `utf8mb4_*` collation name).
fn utf8mb4_value(word: Option<&Word>, collation: bool) -> bool {
    word.is_some_and(|word| {
        word.depth == 0
            && !word.user_variable
            && !word.system_variable
            && if collation {
                word.text.starts_with("UTF8MB4_")
            } else {
                word.text == "UTF8MB4"
            }
    })
}

/// True when a statement sets the client character set to anything but
/// utf8mb4: `SET NAMES`, `SET CHARACTER SET`, `SET CHARSET`, or a `SET`
/// (also `SET STATEMENT ... FOR`) of one of [`CHARSET_VARIABLES`], in any
/// spelling. Setting exactly `utf8mb4` (or, for `collation_connection` and
/// `COLLATE`, a `utf8mb4_*` collation) is allowed: it changes nothing for
/// Varve, and dumps start with it. Reading the variables
/// (`SET @old = @@character_set_client`) is fine.
/// A variable as the value (`SET character_set_client = @saved`) is
/// refused: its value cannot be known before the script runs. Dumps go
/// through [`crate::normalize_dump_boilerplate`] first.
fn sets_other_charset(words: &[Word]) -> bool {
    const SCOPES: [&str; 5] = ["SESSION", "LOCAL", "GLOBAL", "PERSIST", "PERSIST_ONLY"];
    if !words
        .first()
        .is_some_and(|word| word.is_keyword() && word.text == "SET")
    {
        return false;
    }
    let statement_form = words
        .get(1)
        .is_some_and(|word| word.is_keyword() && word.text == "STATEMENT");
    let first_option = if statement_form { 2 } else { 1 };
    // The value ends the assignment: nothing, a comma, or `FOR` follows.
    let ends = |index: usize| {
        words.get(index).is_none_or(|word| {
            (word.after_comma && word.depth == 0)
                || (statement_form && word.is_keyword() && word.text == "FOR")
        })
    };
    for (start, word) in words.iter().enumerate().skip(first_option) {
        if word.depth > 0 || !(start == first_option || word.after_comma) {
            continue;
        }
        if statement_form && word.is_keyword() && word.text == "FOR" {
            break;
        }
        // An optional scope (`SESSION x`, `@@session.x`) before the name.
        let mut target = start;
        if !word.literal
            && !word.user_variable
            && SCOPES.contains(&word.text.as_str())
            && words
                .get(start + 1)
                .is_some_and(|next| !next.after_comma && !next.literal)
        {
            target += 1;
        }
        let Some(name) = words.get(target) else {
            continue;
        };
        if name.user_variable || name.literal {
            continue;
        }
        let allowed = match name.text.as_str() {
            "NAMES" => {
                let value = target + 1;
                if !utf8mb4_value(words.get(value), false) {
                    false
                } else if words
                    .get(value + 1)
                    .is_some_and(|word| word.is_keyword() && word.text == "COLLATE")
                {
                    utf8mb4_value(words.get(value + 2), true) && ends(value + 3)
                } else {
                    ends(value + 1)
                }
            }
            "CHARSET" => utf8mb4_value(words.get(target + 1), false) && ends(target + 2),
            "CHARACTER"
                if words
                    .get(target + 1)
                    .is_some_and(|word| word.is_keyword() && word.text == "SET") =>
            {
                utf8mb4_value(words.get(target + 2), false) && ends(target + 3)
            }
            "COLLATION_CONNECTION" => {
                utf8mb4_value(words.get(target + 1), true) && ends(target + 2)
            }
            other if CHARSET_VARIABLES.contains(&other) => {
                utf8mb4_value(words.get(target + 1), false) && ends(target + 2)
            }
            _ => true,
        };
        if !allowed {
            return true;
        }
    }
    false
}

/// Refuses a script (any session) that changes the client character set;
/// `what` names it in the message.
pub(crate) fn check_charset(sql: &str, what: &str) -> Result<(), DriverError> {
    // Every way the server may read the script: with and without backslash
    // escapes and ANSI_QUOTES, executable comments as code (five or six
    // version digits) or as comments. A charset change in any of them is
    // refused.
    for reading in readings() {
        for (range, words) in scan(sql, reading) {
            if sets_other_charset(&words) {
                let text = sql.get(range).unwrap_or(sql).trim();
                return Err(charset_error(what, text));
            }
        }
    }
    Ok(())
}

fn charset_error(what: &str, statement: &str) -> DriverError {
    DriverError::new(
        ErrorKind::Unsupported,
        format!(
            "{what} changes the client character set ({}), which is not allowed: \
             {CHARSET_OWNED}. Nothing was run.",
            shorten(statement)
        ),
    )
}

/// For `PREPARE name FROM <source>` and `EXECUTE IMMEDIATE <source>`, the
/// source text; `None` for every other statement.
fn dynamic_source(statement: &str) -> Option<&str> {
    let text = statement.as_bytes();
    let i = skip_blank(text, 0);
    let (first, i) = word(text, i);
    if first.eq_ignore_ascii_case(b"PREPARE") {
        let i = skip_blank(text, i);
        // The statement name: a word or a quoted identifier.
        let i = match text.get(i) {
            Some(b'`' | b'"' | b'\'') => skip_quoted(text, i, false),
            _ => word(text, i).1,
        };
        let i = skip_blank(text, i);
        let (from, i) = word(text, i);
        // Anything else is a syntax error on the server; refuse it as
        // unreadable rather than guess.
        if !from.eq_ignore_ascii_case(b"FROM") {
            return Some(&statement[i..]);
        }
        return Some(&statement[skip_blank(text, i)..]);
    }
    if first.eq_ignore_ascii_case(b"EXECUTE") {
        let after = skip_blank(text, i);
        let (second, i) = word(text, after);
        if second.eq_ignore_ascii_case(b"IMMEDIATE") {
            return Some(&statement[skip_blank(text, i)..]);
        }
    }
    None
}

/// The text of a source made only of string literals (adjacent literals
/// are joined, as MySQL does). `None` for anything else: variables,
/// expressions, introducers, hex literals, and literals with a backslash,
/// whose meaning depends on `sql_mode`.
fn literal_text(source: &str) -> Option<String> {
    let text = source.as_bytes();
    let mut out = String::new();
    let mut i = skip_blank(text, 0);
    let mut any = false;
    while let Some(quote @ (b'\'' | b'"')) = text.get(i).copied() {
        let end = quoted_end(text, i, false)?;
        let inner = source.get(i + 1..end - 1)?;
        if inner.contains('\\') {
            return None;
        }
        let doubled = if quote == b'\'' { "''" } else { "\"\"" };
        out.push_str(&inner.replace(doubled, &(quote as char).to_string()));
        any = true;
        i = skip_blank(text, end);
    }
    (any && i == text.len()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused_with(sql: &str) -> Option<String> {
        check(sql, "the script").err().map(|error| {
            assert_eq!(error.kind, ErrorKind::ReadOnly, "{sql}");
            error.message
        })
    }

    #[test]
    fn harmless_scripts_pass() {
        for sql in [
            "SELECT 1",
            "SELECT 'a;b', \"c;d\", `e;f` FROM t; SELECT 2",
            "SELECT 1 -- a comment; with a semicolon\n",
            "SELECT 1 # comment; here\n; SELECT 2",
            "SELECT 1 /* comment; */",
            "SELECT 5 --1",
            "SELECT 'it''s'",
            "PREPARE s FROM 'select ?'; SET @v = 5; EXECUTE s USING @v; DEALLOCATE PREPARE s",
            "PREPARE `my stmt` FROM 'select 1' 'union select 2'",
            "/* lead */ PREPARE s FROM \"select 'x'\"",
            "EXECUTE s USING @v",
            "SELECT /*+ MAX_EXECUTION_TIME(1000) */ 1",
            "SET @x = 'a\\nb'",
        ] {
            assert_eq!(refused_with(sql), None, "{sql}");
        }
    }

    #[test]
    fn dynamic_sql_from_variables_is_refused() {
        for sql in [
            "SET @q = CONCAT('START TRANSACTION ', 'READ WRITE'); PREPARE s FROM @q; EXECUTE s",
            "prepare s from @q",
            "PREPARE s FROM CONCAT('SELECT ', '1')",
            "PREPARE s FROM _utf8mb4'select 1'",
            "PREPARE s FROM X'53454C4543542031'",
            "PREPARE s FROM 'select 1' + @x",
            "EXECUTE IMMEDIATE @q",
            "execute immediate concat('select', ' 1')",
            "PREPARE s FROM 'select \\'x\\''",
        ] {
            let message = refused_with(sql);
            assert!(
                message.as_deref().is_some_and(|m| m.contains(DYNAMIC_SQL)),
                "{sql}: {message:?}"
            );
        }
    }

    #[test]
    fn literal_dynamic_sql_is_checked() {
        for sql in [
            "PREPARE s FROM 'SET SESSION TRANSACTION READ WRITE'",
            "PREPARE s FROM 'START TRANSACTION ' 'READ WRITE'",
            "PREPARE s FROM 'SET @@session.transaction_read_only = 0'",
            "EXECUTE IMMEDIATE 'set tx_read_only = OFF'",
            "PREPARE s FROM 'PREPARE t FROM @q'",
            "PREPARE s FROM 'select 1 /*! , 2 */'",
        ] {
            let message = refused_with(sql);
            assert!(
                message
                    .as_deref()
                    .is_some_and(|m| m.contains("prepares a statement that is refused")),
                "{sql}: {message:?}"
            );
        }
    }

    #[test]
    fn server_dependent_boundaries_are_refused() {
        for sql in [
            // MySQL runs the contents of executable comments.
            "/*! START TRANSACTION READ WRITE */",
            "SELECT 1 /*!80000 , 2 */",
            "SELECT 1 /*M! , 2 */",
            // `--x` is not a comment in MySQL: the `;` ends the statement.
            "SELECT 1 --x; START TRANSACTION READ WRITE",
            // With NO_BACKSLASH_ESCAPES the string ends at the backslash.
            "SELECT 'a\\'; START TRANSACTION READ WRITE; SELECT '",
        ] {
            // Refused either way: the shared splitter now reads `/*! */` as
            // code and `--x` as no comment, so the statement check sees what
            // is hidden there; what it cannot settle is refused here.
            let message = refused_with(sql);
            assert!(
                message
                    .as_deref()
                    .is_some_and(|m| m.contains("Nothing was run")),
                "{sql}: {message:?}"
            );
        }
        let message = refused_with("SELECT 1 /*!80000 , 2 */");
        assert!(
            message.is_some_and(|m| m.contains("executable /*! */ comment")),
            "executable comment"
        );
        let message = refused_with("SELECT 'a\\'; SELECT 'b'; SELECT '");
        assert!(
            message.is_some_and(|m| m.contains("NO_BACKSLASH_ESCAPES")),
            "backslash"
        );
    }

    #[test]
    fn xa_is_refused() {
        for sql in ["XA START 'x'", "xa begin 'x'", "SELECT 1; XA RECOVER"] {
            let message = refused_with(sql);
            assert!(
                message.is_some_and(|m| m.contains("XA transaction")),
                "{sql}"
            );
        }
    }

    #[test]
    fn client_charset_changes_are_refused() {
        for sql in [
            "SET NAMES gbk",
            "set names 'latin1' collate 'latin1_bin'",
            "SET CHARACTER SET gbk",
            "SET CHARSET gbk",
            "SET @@session.character_set_client = gbk",
            "SET SESSION character_set_connection = 'latin1'",
            "SET character_set_results = NULL",
            "SET collation_connection = latin1_swedish_ci",
            "SET @@`character_set_client` = gbk",
            "SET @a = 1, NAMES gbk",
            "SET STATEMENT character_set_client = gbk FOR SELECT 1",
            "/*!40101 SET NAMES gbk */",
            "SELECT 1; SET NAMES gbk; SELECT 2",
            "SET NAMES DEFAULT",
            "SET NAMES utf8mb4 COLLATE latin1_bin",
            "SET character_set_client = @old_client",
            "SET character_set_client = CONCAT('utf8', 'mb4')",
            "SET character_set_client = utf8mb4 + 0",
            "SET collation_connection = 'latin1_swedish_ci'",
            "SET CHARACTER SET latin1",
            "SET STATEMENT character_set_client = utf8mb4, collation_connection = latin1_bin FOR SELECT 1",
        ] {
            let error = check_charset(sql, "the script").err();
            assert!(
                error.as_ref().is_some_and(
                    |e| e.kind == ErrorKind::Unsupported && e.message.contains(CHARSET_OWNED)
                ),
                "{sql}: {error:?}"
            );
        }
        for sql in [
            "SELECT CAST('a' AS CHAR CHARACTER SET latin1)",
            "SET @a = CAST('a' AS CHAR CHARACTER SET latin1)",
            "SET @a = CONVERT('x' USING latin1)",
            "SET @names = 1",
            "SELECT 'SET NAMES gbk'",
            "SELECT names FROM t",
            "SET @a = (SELECT character_set_client FROM information_schema.ROUTINES LIMIT 1)",
            "CREATE TABLE t (a VARCHAR(3) CHARACTER SET latin1)",
            "SET NAMES utf8mb4",
            "set names 'UTF8MB4' collate 'utf8mb4_unicode_ci'",
            "/*!50503 SET NAMES utf8mb4 */",
            "SET CHARACTER SET utf8mb4",
            "SET CHARSET 'utf8mb4'",
            "SET @@session.character_set_client = utf8mb4, character_set_results = 'utf8mb4'",
            "SET collation_connection = utf8mb4_0900_ai_ci",
            "SET STATEMENT character_set_client = utf8mb4 FOR SELECT 1",
            "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */",
            "/*!40101 SET @OLD_COLLATION_CONNECTION=@@COLLATION_CONNECTION */",
            "/*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */",
            "SET @a = 'SET NAMES latin1'",
        ] {
            assert!(check_charset(sql, "the script").is_ok(), "{sql}");
        }
    }

    #[test]
    fn variables_as_values_are_refused() {
        for sql in [
            "SET @v = @@character_set_client; SET character_set_client = @v",
            "/*!40101 SET character_set_client = @saved_cs_client */",
            "/*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */",
            "SET collation_connection = @c",
            "SET @\u{e9} = @@character_set_client; SET @e = 28; SET character_set_client = @\u{e9}",
        ] {
            assert!(check_charset(sql, "the script").is_err(), "{sql}");
        }
        // Reading them is fine.
        assert!(check_charset("SET @v = @@character_set_client", "the script").is_ok());
    }

    #[test]
    fn executable_comments_are_read_both_ways() {
        for sql in [
            // As a comment the SET follows the comment; as code the `;`
            // splits inside it. Either reading must see the SET.
            "/*!99999 ; SELECT */ SET NAMES latin1",
            "/*M!999999 ; SELECT */ SET NAMES latin1",
            // Five or six version digits: `1` or `01` stays in the code.
            "SET @a = /*!800001 , NAMES latin1 */",
            "SET @a = /*!1006001 , NAMES latin1 */",
            "/*!40101 SET NAMES latin1 */",
        ] {
            assert!(check_charset(sql, "the script").is_err(), "{sql}");
        }
        for sql in [
            "/*!50503 SET NAMES utf8mb4 */",
            "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */",
            "/*M!999999\\- enable the sandbox mode */ SELECT 1",
        ] {
            assert!(check_charset(sql, "the script").is_ok(), "{sql}");
        }
    }

    #[test]
    fn sql_mode_readings_are_all_checked() {
        for sql in [
            // NO_BACKSLASH_ESCAPES ends the string at the backslash.
            "SET sql_mode = 'NO_BACKSLASH_ESCAPES'; SELECT 'a\\'; SET NAMES latin1; SELECT '",
            // ANSI_QUOTES makes "..." an identifier without escapes.
            "SET sql_mode = 'ANSI_QUOTES'; SELECT \"a\\\"; SET NAMES latin1; SELECT \"",
        ] {
            assert!(check_charset(sql, "the script").is_err(), "{sql}");
        }
    }

    #[test]
    fn writes_outside_the_access_mode_are_refused() {
        for sql in [
            "SELECT 1 INTO OUTFILE '/tmp/x'",
            "select * from t into dumpfile '/tmp/x'",
            "TABLE t INTO OUTFILE '/tmp/x'",
            "SET GLOBAL max_connections = 151",
            "SET @@global.max_connections = 151",
            "SET session_track_system_variables = ''",
            "SET @@session.session_track_transaction_info = OFF",
            "SET @a = 1, session_track_state_change = OFF",
            "SET @@`global`.max_connections = 151",
            "SET @a = 1, GLOBAL max_connections = 151",
            "SET PERSIST max_connections = 151",
            "set persist_only max_connections = 151",
            "FLUSH TABLES",
            "RESET MASTER",
            "RESET PERSIST",
            "PURGE BINARY LOGS TO 'x'",
            "KILL 12",
            "SHUTDOWN",
            "INSTALL PLUGIN x SONAME 'x.so'",
            "CHANGE REPLICATION SOURCE TO SOURCE_HOST = 'x'",
            "STOP REPLICA",
            "start slave",
            "ALTER INSTANCE ROTATE INNODB MASTER KEY",
            "PREPARE s FROM 'SET GLOBAL max_connections = 151'",
            "SET sql_mode = ''; SELECT 'a\\' INTO OUTFILE '/tmp/x' -- '",
        ] {
            let message = refused_with(sql);
            assert!(
                message.as_deref().is_some_and(|m| m.contains("server")),
                "{sql}: {message:?}"
            );
        }
        for sql in [
            "SET @global = 1",
            "SET @persist = 1, @outfile = 2",
            "SELECT 'INTO OUTFILE'",
            "SELECT a INTO @outfile FROM t",
            "SET SESSION sql_mode = ''",
            "START TRANSACTION READ ONLY",
            "ALTER TABLE t ADD COLUMN c INT",
            "SELECT reset, flush FROM t",
        ] {
            assert_eq!(refused_with(sql), None, "{sql}");
        }
    }

    #[test]
    fn plain_violations_keep_their_message() {
        let message = refused_with("SELECT 1; SET SESSION TRANSACTION READ WRITE");
        assert!(message.is_some_and(|m| m.contains("would switch read-only mode off")));
    }

    #[test]
    fn long_statements_are_shortened() {
        let long = format!("SET tx_read_only = 0 /* {} */", "é".repeat(300));
        let error = violation(&long, "the script");
        assert!(error.message.contains("..."));
        assert!(error.message.len() < 600);
    }

    #[test]
    fn literal_sources() {
        assert_eq!(literal_text("'select 1'").as_deref(), Some("select 1"));
        assert_eq!(literal_text("'a' \"b\" 'c'").as_deref(), Some("abc"));
        assert_eq!(literal_text("'it''s'").as_deref(), Some("it's"));
        assert_eq!(literal_text("'unterminated"), None);
        assert_eq!(literal_text("'a''"), None);
        assert_eq!(literal_text("'"), None);
        assert_eq!(literal_text("'a' @b"), None);
        assert_eq!(literal_text(""), None);
        assert_eq!(literal_text("@q"), None);
    }
}
