//! SQL tokenizer, secret redactor and normalizer for PostgreSQL and
//! MySQL / MariaDB.
//!
//! | | PostgreSQL | MySQL, MariaDB |
//! |---|---|---|
//! | `'...'` | `''` escape (backslash only in `E'...'`) | `''` and backslash escapes |
//! | `"..."` | quoted identifier | string literal |
//! | `` `...` `` | operator character | quoted identifier |
//! | `$tag$...$tag$` | string literal | identifier character |
//! | `--` comment | always | only when followed by whitespace |
//! | `#` comment | no | yes |
//! | `/* */` | nests | does not nest; `/*! ... */` is code |

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use super::REDACTED;
use super::credentials::{
    UrlScope, char_len, comment_secret_ranges, credential_ranges, data_field_ranges,
    descriptive_key, flag_key, is_secret_key, key_parts, metadata_key, plain_number, splice,
    url_ranges,
};
use super::fuzzy::{like_password, within};

/// Lexical rules of a SQL engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    /// PostgreSQL.
    Postgres,
    /// MySQL and MariaDB.
    Mysql,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Space,
    Comment,
    Word,
    QuotedIdent,
    Str,
    Number,
    Punct,
    /// MySQL `/*!12345` or `/*M!` opening an executable comment, and its `*/`.
    ExecMark,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Tok {
    pub(crate) kind: Kind,
    pub(crate) start: usize,
    pub(crate) end: usize,
    /// Content range: inside the quotes for literals and quoted identifiers,
    /// the whole token otherwise.
    content_start: usize,
    content_end: usize,
    /// Whether a backslash escapes the next character in this literal.
    backslash: bool,
}

impl Tok {
    fn plain(kind: Kind, start: usize, end: usize) -> Self {
        Tok {
            kind,
            start,
            end,
            content_start: start,
            content_end: end,
            backslash: false,
        }
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn is_word_start(b: u8, flavor: Flavor) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80 || (flavor == Flavor::Mysql && b == b'$')
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Scans a quoted body starting at `content_start`. Returns the content end
/// and the token end; an unterminated literal runs to the end of the text.
fn scan_quoted(bytes: &[u8], content_start: usize, quote: u8, backslash: bool) -> (usize, usize) {
    let mut i = content_start;
    while i < bytes.len() {
        let b = bytes[i];
        if backslash && b == b'\\' {
            i += 2;
        } else if b == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return (i, i + 1);
            }
        } else {
            i += 1;
        }
    }
    (bytes.len(), bytes.len())
}

/// End of a `$tag$` opener at `start`, if there is one.
fn dollar_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    match bytes.get(i) {
        Some(b'$') => return Some(i),
        Some(&b) if b.is_ascii_alphabetic() || b == b'_' || b >= 0x80 => {}
        _ => return None,
    }
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] >= 0x80)
    {
        i += 1;
    }
    (bytes.get(i) == Some(&b'$')).then_some(i)
}

fn find_from(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from > bytes.len() {
        return None;
    }
    bytes[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

fn quoted_tok(
    bytes: &[u8],
    kind: Kind,
    start: usize,
    content_start: usize,
    quote: u8,
    backslash: bool,
) -> Tok {
    let (content_end, end) = scan_quoted(bytes, content_start, quote, backslash);
    Tok {
        kind,
        start,
        end,
        content_start,
        content_end,
        backslash,
    }
}

/// A string literal with a one or two character prefix (`E'..'`, `X'..'`,
/// `B'..'`, `N'..'`, `U&'..'`) or a `U&"..."` identifier, if one starts at `i`.
fn prefixed_literal(bytes: &[u8], i: usize, flavor: Flavor, backslash: bool) -> Option<Tok> {
    let first = bytes[i].to_ascii_lowercase();
    let second = bytes.get(i + 1).copied();
    if second == Some(b'\'') {
        let backslash = match (flavor, first) {
            (Flavor::Postgres, b'e') => true,
            (Flavor::Postgres, b'b' | b'x' | b'n') => false,
            (Flavor::Mysql, b'b' | b'x' | b'n') => backslash,
            _ => return None,
        };
        return Some(quoted_tok(bytes, Kind::Str, i, i + 2, b'\'', backslash));
    }
    if flavor == Flavor::Postgres && first == b'u' && second == Some(b'&') {
        return match bytes.get(i + 2) {
            Some(b'\'') => Some(quoted_tok(bytes, Kind::Str, i, i + 3, b'\'', false)),
            Some(b'"') => Some(quoted_tok(bytes, Kind::QuotedIdent, i, i + 3, b'"', false)),
            _ => None,
        };
    }
    None
}

fn scan_number(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    if bytes[i] == b'0' {
        let radix_digit: Option<fn(u8) -> bool> = match bytes.get(i + 1) {
            Some(b'x' | b'X') => Some(|b: u8| b.is_ascii_hexdigit() || b == b'_'),
            Some(b'b' | b'B') => Some(|b: u8| b == b'0' || b == b'1' || b == b'_'),
            Some(b'o' | b'O') => Some(|b: u8| (b'0'..=b'7').contains(&b) || b == b'_'),
            _ => None,
        };
        if let Some(is_digit) = radix_digit
            && bytes.get(i + 2).is_some_and(|&b| is_digit(b))
        {
            i += 2;
            while i < bytes.len() && is_digit(bytes[i]) {
                i += 1;
            }
            return i;
        }
    }
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
        i += 1;
    }
    if bytes.get(i) == Some(&b'.') && bytes.get(i + 1) != Some(&b'.') {
        i += 1;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
            i += 1;
        }
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(bytes.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        if bytes.get(j).is_some_and(u8::is_ascii_digit) {
            i = j;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    i
}

pub(crate) fn lex(src: &str, flavor: Flavor) -> Vec<Tok> {
    lex_mode(src, flavor, true)
}

/// Like [`lex`]; `backslash` says whether a backslash escapes in MySQL
/// strings (off under `NO_BACKSLASH_ESCAPES`).
pub(crate) fn lex_mode(src: &str, flavor: Flavor, backslash: bool) -> Vec<Tok> {
    Lexer::new(src, flavor, backslash, 0).collect()
}

/// The tokens of a text one at a time, from a start position that a
/// caller may move forward ([`Lexer::skip_to`]), so a script can be read
/// in one pass even when parts of it are no SQL (`COPY ... FROM stdin`
/// data).
pub(crate) struct Lexer<'a> {
    bytes: &'a [u8],
    flavor: Flavor,
    backslash: bool,
    in_exec_comment: bool,
    i: usize,
}

impl<'a> Lexer<'a> {
    pub(crate) fn new(src: &'a str, flavor: Flavor, backslash: bool, from: usize) -> Self {
        Lexer {
            bytes: src.as_bytes(),
            flavor,
            backslash,
            in_exec_comment: false,
            i: from,
        }
    }

    /// Continues reading at `at` (not before the current position), in a
    /// fresh state.
    pub(crate) fn skip_to(&mut self, at: usize) {
        self.i = self.i.max(at).min(self.bytes.len());
        self.in_exec_comment = false;
    }
}

impl Iterator for Lexer<'_> {
    type Item = Tok;

    fn next(&mut self) -> Option<Tok> {
        let bytes = self.bytes;
        let n = bytes.len();
        let flavor = self.flavor;
        let backslash = self.backslash;
        let mut in_exec_comment = self.in_exec_comment;
        let mut i = self.i;
        if i >= n {
            return None;
        }
        let start = i;
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        let tok = if is_space(b) {
            while i < n && is_space(bytes[i]) {
                i += 1;
            }
            Tok::plain(Kind::Space, start, i)
        } else if (b == b'-'
            && next == Some(b'-')
            && (flavor == Flavor::Postgres || bytes.get(i + 2).is_none_or(|&c| c <= b' ')))
            || (b == b'#' && flavor == Flavor::Mysql)
        {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            Tok::plain(Kind::Comment, start, i)
        } else if b == b'/' && next == Some(b'*') {
            let exec_prefix = match (flavor, bytes.get(i + 2), bytes.get(i + 3)) {
                (Flavor::Mysql, Some(b'!'), _) => Some(3),
                (Flavor::Mysql, Some(b'M'), Some(b'!')) => Some(4),
                _ => None,
            };
            if let Some(len) = exec_prefix {
                i += len;
                while i < n && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                in_exec_comment = true;
                Tok::plain(Kind::ExecMark, start, i)
            } else {
                i += 2;
                let mut depth = 1usize;
                while i < n {
                    if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        i += 2;
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    } else if flavor == Flavor::Postgres
                        && bytes[i] == b'/'
                        && bytes.get(i + 1) == Some(&b'*')
                    {
                        i += 2;
                        depth += 1;
                    } else {
                        i += 1;
                    }
                }
                Tok::plain(Kind::Comment, start, i.min(n))
            }
        } else if b == b'*' && next == Some(b'/') && in_exec_comment {
            i += 2;
            in_exec_comment = false;
            Tok::plain(Kind::ExecMark, start, i)
        } else if b == b'\'' {
            quoted_tok(
                bytes,
                Kind::Str,
                start,
                i + 1,
                b'\'',
                flavor == Flavor::Mysql && backslash,
            )
        } else if b == b'"' {
            match flavor {
                Flavor::Postgres => quoted_tok(bytes, Kind::QuotedIdent, start, i + 1, b'"', false),
                Flavor::Mysql => quoted_tok(bytes, Kind::Str, start, i + 1, b'"', backslash),
            }
        } else if b == b'`' && flavor == Flavor::Mysql {
            quoted_tok(bytes, Kind::QuotedIdent, start, i + 1, b'`', false)
        } else if b == b'$' && flavor == Flavor::Postgres {
            if next.is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
                while i < n && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                Tok::plain(Kind::Word, start, i)
            } else if let Some(tag_end) = dollar_tag_end(bytes, i) {
                let tag = &bytes[start..=tag_end];
                let content_start = tag_end + 1;
                let (content_end, end) = match find_from(bytes, content_start, tag) {
                    Some(p) => (p, p + tag.len()),
                    None => (n, n),
                };
                Tok {
                    kind: Kind::Str,
                    start,
                    end,
                    content_start,
                    content_end,
                    backslash: false,
                }
            } else {
                Tok::plain(Kind::Punct, start, i + 1)
            }
        } else if is_word_start(b, flavor) {
            if let Some(tok) = prefixed_literal(bytes, i, flavor, backslash) {
                tok
            } else {
                while i < n && is_word_char(bytes[i]) {
                    i += 1;
                }
                Tok::plain(Kind::Word, start, i)
            }
        } else if b.is_ascii_digit() || (b == b'.' && next.is_some_and(|c| c.is_ascii_digit())) {
            i = scan_number(bytes, i);
            // MySQL identifiers may start with a digit (`1st_column`).
            if flavor == Flavor::Mysql && i < n && is_word_char(bytes[i]) {
                while i < n && is_word_char(bytes[i]) {
                    i += 1;
                }
                Tok::plain(Kind::Word, start, i)
            } else {
                Tok::plain(Kind::Number, start, i)
            }
        } else {
            Tok::plain(Kind::Punct, start, i + 1)
        };
        self.i = tok.end.max(start + 1).min(n);
        self.in_exec_comment = in_exec_comment;
        Some(tok)
    }
}

/// Where the statement holding the first sign of broken quoting starts,
/// or `None` when the text lexes cleanly in at least one backslash mode
/// of its dialect. Broken quoting: a literal, quoted identifier or block
/// comment that runs to the end of the text unterminated, or a literal
/// glued to a following word that is no keyword or to another quoted token
/// (`'D'Angelo'`, an unescaped quote inside a value). From there on the lexer reads data as code and code as data,
/// so nothing it says about the rest can be trusted: the caller hides it.
pub(crate) fn broken_quoting(src: &str, flavor: Flavor) -> Option<usize> {
    broken_quoting_at(src, flavor).map(|(cut, _)| cut)
}

/// Like [`broken_quoting`], with where the first sign of broken quoting
/// itself is (at or after the statement start).
pub(crate) fn broken_quoting_at(src: &str, flavor: Flavor) -> Option<(usize, usize)> {
    let modes: &[bool] = match flavor {
        Flavor::Postgres => &[true],
        Flavor::Mysql => &[true, false],
    };
    let mut cut: Option<(usize, usize)> = None;
    for &backslash in modes {
        let toks = lex_mode(src, flavor, backslash);
        let at = first_break(src, &toks, flavor)?;
        cut = Some(cut.map_or(at, |c| c.min(at)));
    }
    cut
}

/// The start of the statement holding the first break in `toks` (see
/// [`broken_quoting`]), and where the break is.
fn first_break(src: &str, toks: &[Tok], flavor: Flavor) -> Option<(usize, usize)> {
    let mut statement_start = 0;
    for (k, tok) in toks.iter().enumerate() {
        let text = &src[tok.start..tok.end];
        let broken = match tok.kind {
            Kind::Punct => {
                if text == ";" {
                    statement_start = tok.end;
                }
                false
            }
            Kind::Str | Kind::QuotedIdent if tok.end == tok.content_end => true,
            // `'D'Angelo'`: a word that continues the literal. A keyword
            // right after it is valid SQL (`'a'AS x`, `$$...$$LANGUAGE`).
            Kind::Str => toks.get(k + 1).is_some_and(|next| {
                next.start == tok.end
                    && match next.kind {
                        // `'D'Or'`: even a keyword is a continuation when a
                        // single quote follows it at once.
                        Kind::Word => {
                            !super::sql_normalize::is_keyword(&src[next.start..next.end], flavor)
                                || toks.get(k + 2).is_some_and(|after| {
                                    // `'a'AS"b"` and `'x'AS\`y\`` are aliases.
                                    after.start == next.end
                                        && after.kind == Kind::Str
                                        && src.as_bytes()[after.start] == b'\''
                                })
                        }
                        // `'5 o'9`: no SQL puts a number right after a
                        // literal.
                        Kind::Str | Kind::QuotedIdent | Kind::Number => true,
                        _ => false,
                    }
            }),
            // A block comment that runs to the end of the text: open, or,
            // in PostgreSQL where they nest, with more `/*` than `*/`.
            Kind::Comment => {
                text.starts_with("/*")
                    && tok.end == src.len()
                    && (text.len() < 4
                        || !text.ends_with("*/")
                        || (flavor == Flavor::Postgres
                            && text.matches("/*").count() > text.matches("*/").count()))
            }
            _ => false,
        };
        if broken {
            return Some((statement_start, tok.start));
        }
    }
    None
}

/// Calls that raise an error from a routine body in another language than
/// SQL (`plpy.error`, `plpy.fatal`, `plv8.elog`, `elog`, Perl `die`).
const SCRIPT_RAISES: &[&str] = &["plpy.error(", "plpy.fatal(", "elog(", "ereport("];

/// True when a routine body raises an error in a script language: a call
/// of [`SCRIPT_RAISES`], PL/v8 `throw`, Tcl `elog ERROR`, `error "..."` and
/// `return -code error`, Perl `die`, `croak` and `confess` as a statement
/// (`die "x"`, `... or die "x"`), or Python `assert ..., "x"`; not the
/// word in a string, an assignment (`error = 1`) or a declaration.
fn script_raises(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if SCRIPT_RAISES.iter().any(|call| lower.contains(call)) {
        return true;
    }
    lower.split(['\n', ';', '{', '}']).any(|command| {
        let command = command.trim_start();
        // The word starts the command and a value follows (`"x"`, `'x'`,
        // `(...)`, `[...]`), not an assignment such as `error = 1` or a
        // declaration such as `error text`.
        let call = |word: &str| {
            command.strip_prefix(word).is_some_and(|rest| {
                let value = rest.trim_start();
                (value.len() < rest.len() || rest.starts_with('('))
                    && value.starts_with(['"', '\'', '(', '[', '$'])
            })
        };
        let assert = command.starts_with("assert ") && command.contains(['"', '\'']);
        call("throw")
            || call("die")
            || call("croak")
            || call("confess")
            || call("error")
            || assert
            || command.starts_with("throw new ")
            || command.starts_with("elog error")
            || command.starts_with("elog fatal")
            || command.starts_with("return -code error")
            || command.contains(" or die ")
            || command.contains("|| die ")
            || command.contains(" or croak ")
    })
}

/// True when SQL text raises its own error: `RAISE`, `SIGNAL` or
/// `RESIGNAL` as a statement (first word of a statement or after `BEGIN`,
/// `THEN`, `ELSE`, `LOOP`, `DO`, ...; a column named `signal` or the word
/// in a comment does not count), also in a routine body (to
/// [`LITERAL_DEPTH`] levels), or an error raised by a script body
/// ([`script_raises`]).
pub(crate) fn raises_own_error(src: &str, flavor: Flavor) -> bool {
    let mut work = src.len().saturating_mul(LITERAL_WORK_PER_BYTE);
    let mut pending: Vec<(String, usize)> = vec![(src.to_string(), 0)];
    while let Some((text, depth)) = pending.pop() {
        let Some(left) = work.checked_sub(text.len()) else {
            // Too costly to read through: count it as raised, which only
            // hides more of the message.
            return true;
        };
        work = left;
        let mut statement_start = true;
        for tok in lex(&text, flavor) {
            match tok.kind {
                Kind::Space | Kind::Comment => continue,
                Kind::Str => {
                    let content = &text[tok.content_start..tok.content_end];
                    let lower = content.to_ascii_lowercase();
                    let quote = text.as_bytes()[tok.content_start.saturating_sub(1)];
                    // Only a body (dollar-quoted, or over several lines) is
                    // script code; `'die now'` is a value.
                    if (quote == b'$' || content.contains('\n')) && script_raises(content) {
                        return true;
                    }
                    if depth + 1 < LITERAL_DEPTH
                        && (quote == b'$' || content.contains(['\'', '"']))
                        && ["raise", "signal"].iter().any(|w| lower.contains(w))
                    {
                        pending.push((content.to_string(), depth + 1));
                    }
                    statement_start = false;
                }
                Kind::Word => {
                    let upper = text[tok.start..tok.end].to_ascii_uppercase();
                    if statement_start && matches!(upper.as_str(), "RAISE" | "SIGNAL" | "RESIGNAL")
                    {
                        return true;
                    }
                    statement_start = matches!(
                        upper.as_str(),
                        "BEGIN" | "THEN" | "ELSE" | "LOOP" | "REPEAT" | "DO" | "DECLARE"
                    );
                }
                Kind::Punct => statement_start = &text[tok.start..tok.end] == ";",
                _ => statement_start = false,
            }
        }
    }
    false
}

/// Limits of [`statement_literals`]: how deep it reads SQL held in
/// literals (routine bodies), and how many bytes per byte of text.
const LITERAL_DEPTH: usize = 4;
const LITERAL_WORK_PER_BYTE: usize = 8;

/// The literal values of a SQL text, which normalized text shows as `?`:
/// string contents (as written, and with `''`, `\'` and `\\` undone) and
/// numbers, also inside routine bodies and other SQL held in strings (up
/// to [`LITERAL_DEPTH`] levels). `None` when reading them would cost more
/// than [`LITERAL_WORK_PER_BYTE`] per byte of text.
pub(crate) fn statement_literals(src: &str, flavor: Flavor) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut work = src.len().saturating_mul(LITERAL_WORK_PER_BYTE);
    let mut pending: Vec<(String, usize)> = vec![(src.to_string(), 0)];
    while let Some((text, depth)) = pending.pop() {
        work = work.checked_sub(text.len())?;
        let toks = lex(&text, flavor);
        let sig: Vec<&Tok> = toks
            .iter()
            .filter(|t| !matches!(t.kind, Kind::Space | Kind::Comment))
            .collect();
        // A routine in another language: its dollar-quoted body is read as
        // that language, whose strings (single, double, triple quoted,
        // template) are the literals.
        let script = sig.iter().enumerate().find_map(|(k, t)| {
            (t.kind == Kind::Word && text[t.start..t.end].eq_ignore_ascii_case("LANGUAGE"))
                .then(|| sig.get(k + 1))
                .flatten()
                .and_then(|name| {
                    let name = match name.kind {
                        Kind::Str | Kind::QuotedIdent => {
                            &text[name.content_start..name.content_end]
                        }
                        _ => &text[name.start..name.end],
                    };
                    ScriptComments::of_language(name)
                })
        });
        for (k, tok) in sig.iter().enumerate() {
            match tok.kind {
                Kind::Number => out.push(text[tok.start..tok.end].to_string()),
                Kind::Str
                    if script.is_some()
                        && text.as_bytes()[tok.content_start.saturating_sub(1)] == b'$' =>
                {
                    let body = &text[tok.content_start..tok.content_end];
                    let parts = script_parts(body, script.unwrap_or(ScriptComments::Hash));
                    for (start, end) in parts.strings {
                        let raw = &body[start..end];
                        // `"admin\n"`: the value the script prints.
                        let unescaped = super::credentials::unescape(raw).0;
                        if unescaped != raw {
                            out.push(unescaped);
                        }
                        out.push(raw.to_string());
                    }
                    for (start, end) in parts.sql {
                        if depth + 1 < LITERAL_DEPTH {
                            pending.push((body[start..end].to_string(), depth + 1));
                        }
                    }
                }
                Kind::Str => {
                    let content = &text[tok.content_start..tok.content_end];
                    let quote = text.as_bytes()[tok.content_start.saturating_sub(1)];
                    // A dollar-quoted body escapes nothing: it is read as
                    // written. A quoted literal is decoded first.
                    // `U&'...' UESCAPE '!'`: the escape character.
                    let uescape = (sig
                        .get(k + 1)
                        .is_some_and(|t| text[t.start..t.end].eq_ignore_ascii_case("UESCAPE")))
                    .then(|| sig.get(k + 2))
                    .flatten()
                    .and_then(|t| text[t.content_start..t.content_end].chars().next())
                    .unwrap_or('\\');
                    let value = if quote == b'$' {
                        content.to_string()
                    } else {
                        decoded_literal(
                            &text[tok.start..tok.content_start],
                            content,
                            tok.backslash,
                            uescape,
                        )
                    };
                    if depth + 1 < LITERAL_DEPTH && (quote == b'$' || value.contains(['\'', '"'])) {
                        pending.push((value.clone(), depth + 1));
                    }
                    if value != content {
                        out.push(value);
                    }
                    out.push(content.to_string());
                }
                _ => {}
            }
        }
    }
    Some(out)
}

/// The value of a quoted literal as the server reads it: `''` is a quote,
/// backslash escapes are undone in an escape string (`E'...'`, MySQL), and
/// `\XXXX` / `\+XXXXXX` in a Unicode string (`U&'...'`, with the escape
/// character of its `UESCAPE` clause).
fn decoded_literal(prefix: &str, content: &str, backslash: bool, uescape: char) -> String {
    let content = if backslash {
        super::credentials::unescape(content).0
    } else if prefix.len() >= 2 && prefix[..2].eq_ignore_ascii_case("u&") {
        unicode_escapes(content, uescape)
    } else {
        content.to_string()
    };
    content.replace("''", "'")
}

/// Undoes the escapes of a PostgreSQL `U&'...'` string with escape
/// character `escape` (`\` unless `UESCAPE` names another): `\XXXX`,
/// `\+XXXXXX` and a doubled escape character.
fn unicode_escapes(content: &str, escape: char) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(at) = rest.find(escape) {
        out.push_str(&rest[..at]);
        let tail = &rest[at + escape.len_utf8()..];
        let (digits, skip) = match tail.chars().next() {
            Some(c) if c == escape => {
                out.push(escape);
                rest = &tail[c.len_utf8()..];
                continue;
            }
            Some('+') => (tail.get(1..7), 7),
            _ => (tail.get(..4), 4),
        };
        match digits
            .and_then(|d| u32::from_str_radix(d, 16).ok())
            .and_then(char::from_u32)
        {
            Some(c) => {
                out.push(c);
                rest = &tail[skip..];
            }
            None => {
                out.push(escape);
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A part of a SQL script: SQL text, or the data rows of a PostgreSQL
/// `COPY ... FROM stdin` (as `pg_dump` and `psql` scripts write them, up to
/// the `\.` line), which are data, not SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Section {
    Sql(std::ops::Range<usize>),
    /// A psql meta-command line (`\connect db`, `\set name value`,
    /// `\! shell command`), up to and including its line break. `inside`:
    /// it falls inside a statement that is still open (psql keeps its
    /// query buffer across it), so the SQL around it must also be read as
    /// one text ([`joined_ranges`]).
    Meta {
        range: std::ops::Range<usize>,
        inside: bool,
    },
    CopyData {
        range: std::ops::Range<usize>,
        format: CopyFormat,
        /// The columns of the `COPY` column list (empty without one).
        columns: Vec<CopyColumn>,
    },
}

/// What a column of a `COPY` column list holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyColumn {
    Plain,
    /// A secret (`password_hash`, `remember_token`).
    Secret,
    /// A time, count or policy about a secret (`password_changed_at`);
    /// `true` for a policy name, whose value may be a word.
    Metadata(bool),
}

impl CopyColumn {
    fn of_name(name: &str) -> Self {
        if secret_word(name) {
            CopyColumn::Secret
        } else if metadata_key(name) && about_secret(name) && !flag_key(&key_parts(name)) {
            CopyColumn::Metadata(policy_name(name))
        } else {
            CopyColumn::Plain
        }
    }
}

/// The data format of a `COPY ... FROM stdin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CopyFormat {
    /// Fields split by a delimiter (tab by default), `\N` for NULL.
    Text { delimiter: u8 },
    /// Comma-separated (or another delimiter), fields quoted with `quote`
    /// (`"` by default), which `escape` (the quote by default) escapes.
    Csv {
        delimiter: u8,
        header: bool,
        quote: u8,
        escape: u8,
    },
    /// Binary rows: no fields can be told apart.
    Binary,
}

/// A `COPY` option whose value comes next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyOption {
    Format,
    Delimiter,
    Quote,
    Escape,
    Header,
    /// An option with a value that does not matter here (`NULL`,
    /// `ENCODING`, `DEFAULT`, `FREEZE`, ...).
    Other,
}

/// What [`sections`] knows of the statement being read.
#[derive(Default)]
struct CopyHead {
    /// The statement starts with `COPY` (or psql's `\copy`).
    copy: bool,
    /// A psql meta-command (`\copy`, `\connect`, ...): it ends at the end
    /// of its line.
    meta_line_end: Option<usize>,
    /// `COPY (query) TO ...`: no data follows.
    query: bool,
    /// Significant tokens of the statement so far.
    words: usize,
    /// Parenthesis depth outside the options.
    depth: usize,
    /// `FROM STDIN` was read at depth 0.
    stdin: bool,
    /// The previous word was `FROM`.
    after_from: bool,
    /// Column list: open, and what its columns hold so far.
    in_columns: bool,
    columns_done: bool,
    current_column: Option<CopyColumn>,
    columns: Vec<CopyColumn>,
    /// Options after `STDIN`.
    csv: bool,
    binary: bool,
    header: bool,
    delimiter: Option<u8>,
    quote: Option<u8>,
    escape: Option<u8>,
    /// The option whose value comes next.
    pending: Option<CopyOption>,
    /// The last option value was a `U&'...'` string: its option, prefix and
    /// raw content, read again when `UESCAPE` follows.
    unicode_value: Option<(CopyOption, String, String)>,
    /// `UESCAPE` was read: its character comes next.
    uescape_next: bool,
    /// The options (bits 1 `DELIMITER`, 2 `QUOTE`, 4 `ESCAPE`) whose
    /// value the rows cannot be split by. A value read again with its
    /// `UESCAPE` character replaces the first reading.
    unsplittable: u8,
}

/// Splits a script into SQL and `COPY ... FROM stdin` data sections
/// (PostgreSQL only; MySQL text is one SQL section), in one pass over the
/// text however many `COPY` blocks it holds. A data section starts after
/// a `COPY ... FROM stdin [options];` (or psql's `\copy ... from stdin`)
/// that the lexer reads as code (never inside a literal or comment),
/// followed on its line by nothing but comments, and runs to the next
/// `\.` line, wherever it is (as psql reads it), or to the end of the
/// text when there is none.
pub(crate) fn sections(src: &str, flavor: Flavor) -> Vec<Section> {
    let mut out = Vec::new();
    if flavor != Flavor::Postgres {
        out.push(Section::Sql(0..src.len()));
        return out;
    }
    // The ends of all `\.` lines, found once: each data section takes the
    // first one after its start, so many COPY blocks cost one pass.
    let terminators = copy_terminators(src);
    let mut next_terminator = 0;
    let mut lexer = Lexer::new(src, flavor, true, 0);
    let mut head = CopyHead::default();
    let mut sql_start = 0;
    // Parenthesis depth of the statement being read.
    let mut depth = 0usize;
    // A statement has begun since the last `;`, data section or sending
    // meta-command.
    let mut open = false;
    // The data section after a statement or meta-command that ends at
    // `end`, if it is a `COPY ... FROM stdin` / `\copy ... from stdin`.
    let data_after = |found: &CopyHead, end: usize, next_terminator: &mut usize| {
        let data_start = found.data_start(src, end)?;
        if found.query || !found.copy || !found.stdin {
            return None;
        }
        while terminators
            .get(*next_terminator)
            .is_some_and(|&(line, _)| line < data_start)
        {
            *next_terminator += 1;
        }
        let data_end = terminators
            .get(*next_terminator)
            .map_or(src.len(), |&(_, end)| end);
        Some((data_start, data_end))
    };
    while let Some(tok) = lexer.next() {
        if matches!(tok.kind, Kind::Space | Kind::Comment) {
            continue;
        }
        let text = &src[tok.start..tok.end];
        if tok.kind == Kind::Punct && text == "\\" {
            // A psql meta-command (`\connect`, `\set`, `\echo`, `\gset`
            // after a query, `\copy`): it runs to the end of its line, and
            // nothing on that line opens a literal, a comment or a body.
            if tok.start > sql_start {
                out.push(Section::Sql(sql_start..tok.start));
            }
            let line_end = src[tok.end..].find('\n').map_or(src.len(), |p| tok.end + p);
            let mut meta = CopyHead::default();
            meta.read(src, &tok);
            for t in Lexer::new(&src[..line_end], flavor, true, tok.end) {
                if !matches!(t.kind, Kind::Space | Kind::Comment) {
                    meta.read(src, &t);
                }
            }
            let next_line = (line_end + 1).min(src.len());
            let meta_start = sql_start.max(tok.start);
            match data_after(&meta, line_end, &mut next_terminator) {
                Some((data_start, data_end)) => {
                    out.push(Section::Meta {
                        range: meta_start..data_start,
                        inside: open,
                    });
                    out.push(Section::CopyData {
                        range: data_start..data_end,
                        format: meta.format(),
                        columns: meta.columns,
                    });
                    sql_start = data_end;
                    open = false;
                }
                None => {
                    out.push(Section::Meta {
                        range: meta_start..next_line,
                        inside: open,
                    });
                    sql_start = next_line;
                }
            }
            // psql keeps the query buffer across other meta-commands, so a
            // `\x` line inside a column list leaves the statement open;
            // `\g`, `\gset`, `\watch` and the like send it, `\r` clears it.
            if meta_ends_statement(&src[tok.end..line_end]) {
                head = CopyHead::default();
                depth = 0;
                open = false;
            }
            lexer.skip_to(sql_start);
            continue;
        }
        if tok.kind == Kind::Punct && text == ";" {
            let found = std::mem::take(&mut head);
            depth = 0;
            open = false;
            if let Some((data_start, data_end)) = data_after(&found, tok.end, &mut next_terminator)
            {
                out.push(Section::Sql(sql_start..data_start));
                out.push(Section::CopyData {
                    range: data_start..data_end,
                    format: found.format(),
                    columns: found.columns,
                });
                sql_start = data_end;
                lexer.skip_to(data_end);
            }
            continue;
        }
        match text {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            _ => {}
        }
        // `COPY` at the start of a line after a statement without its `;`
        // (`SELECT 1` then `COPY ... FROM stdin;`) starts a statement too;
        // a column named `copy` in a list does not.
        if head.words > 0
            && !head.copy
            && depth == 0
            && text.eq_ignore_ascii_case("COPY")
            && src[..tok.start]
                .rsplit('\n')
                .next()
                .is_some_and(|line| line.trim().is_empty())
            && tok.start > 0
            && copy_statement_follows(src, tok.end, flavor)
        {
            head = CopyHead::default();
            let line_start = src[..tok.start].rfind('\n').map_or(0, |p| p + 1);
            if line_start > sql_start {
                out.push(Section::Sql(sql_start..line_start));
                sql_start = line_start;
            }
        }
        open = true;
        head.read(src, &tok);
    }
    if sql_start < src.len() || out.is_empty() {
        out.push(Section::Sql(sql_start..src.len()));
    }
    out
}

/// Whether the psql meta-command after the backslash (`rest` of its line)
/// sends or clears the query buffer, so the statement being read ends
/// there: `\g` and its forms (`\gset`, `\gx`, `\gexec`, `\gdesc`),
/// `\crosstabview`, `\watch`, `\sendpipeline`, `\r` / `\reset`, and the
/// editors `\e` / `\edit`, whose buffer is sent when they close. Any other
/// meta-command leaves the buffer as it is.
fn meta_ends_statement(rest: &str) -> bool {
    let command = rest
        .split(|c: char| c.is_whitespace() || c == '\\')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        command.as_str(),
        "g" | "gx"
            | "gset"
            | "gexec"
            | "gdesc"
            | "crosstabview"
            | "watch"
            | "sendpipeline"
            | "r"
            | "reset"
            | "e"
            | "edit"
    )
}

/// Byte ranges of a text.
type ByteRanges = Vec<(usize, usize)>;

/// Where the secret rules find secrets when the SQL around a meta-command
/// line inside an open statement is read as psql sends it: as one text.
///
/// Each run of SQL sections joined by such lines (`ALTER ROLE app WITH
/// PASSWORD`, `\echo x`, `'hunter2';`) is read again with those lines
/// blanked out (a block comment of the same length, so offsets stay the
/// same). The ranges found before the first broken quoting of that run are
/// returned in `src` offsets; from the statement holding the break to the
/// end of the run, the SQL is returned as ranges to hide (white space at
/// their ends left), as the split reading cannot be trusted there either.
/// The meta-command lines themselves are never in a range: their own rules
/// apply. Returns the ranges found and the ranges to hide.
pub(crate) fn joined_ranges(
    src: &str,
    sections: &[Section],
    flavor: Flavor,
) -> (ByteRanges, ByteRanges) {
    let mut out = Vec::new();
    let mut hide = Vec::new();
    let mut i = 0;
    while i < sections.len() {
        let mut j = i;
        let mut inside = false;
        while let Some(section) = sections.get(j) {
            match section {
                Section::Sql(_) => {}
                Section::Meta { inside: true, .. } => inside = true,
                _ => break,
            }
            j += 1;
        }
        if j == i {
            i += 1;
            continue;
        }
        let run = &sections[i..j];
        i = j;
        if !inside {
            continue;
        }
        let range_of = |section: &Section| match section {
            Section::Sql(range) | Section::Meta { range, .. } => range.clone(),
            Section::CopyData { range, .. } => range.clone(),
        };
        let start = run.first().map_or(0, |s| range_of(s).start);
        let end = run.last().map_or(0, |s| range_of(s).end);
        let mut bytes = src.as_bytes()[start..end].to_vec();
        for section in run {
            if let Section::Meta { range, .. } = section {
                let line = &src[range.clone()];
                let len = line.trim_end_matches(['\r', '\n']).len();
                let blank = &mut bytes[range.start - start..range.start - start + len];
                blank.fill(b' ');
                if len >= 4 {
                    blank[..2].copy_from_slice(b"/*");
                    blank[len - 2..].copy_from_slice(b"*/");
                }
            }
        }
        // Whole meta-command lines were overwritten with ASCII bytes, so
        // the text stays UTF-8; should it not, the split reading stands.
        let Ok(joined) = String::from_utf8(bytes) else {
            continue;
        };
        let cut = match broken_quoting_at(&joined, flavor) {
            // A sending meta-command line (`\gset`) between the statement
            // start and the break ends the statement there for psql: the
            // query before it (`SELECT 1 \gset`) is no part of the broken
            // one.
            Some((cut, at)) => run
                .iter()
                .filter_map(|section| match section {
                    Section::Meta { range, .. }
                        if range.start >= start + cut
                            && range.end <= start + at
                            && meta_ends_statement(
                                src[range.clone()].get(1..).unwrap_or_default(),
                            ) =>
                    {
                        Some(range.end - start)
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(cut),
            None => joined.len(),
        };
        if let Some(found) = statement_ranges(&joined[..cut], flavor) {
            out.extend(found.into_iter().map(|(a, b)| (start + a, start + b)));
        }
        if cut < joined.len() {
            for section in run {
                let Section::Sql(range) = section else {
                    continue;
                };
                let from = range.start.max(start + cut);
                if from >= range.end {
                    continue;
                }
                let piece = &src[from..range.end];
                let lead = piece.len() - piece.trim_start().len();
                let body = piece.trim();
                if !body.is_empty() {
                    hide.push((from + lead, from + lead + body.len()));
                }
            }
        }
    }
    (out, hide)
}

/// Whether the text after a `COPY` word at `from` reads as a `COPY`
/// statement: an optional `BINARY`, a (qualified) table name, an optional
/// column list, then `FROM` or `TO`. A column or value named `copy` at the
/// start of a line (`INSERT INTO t (a, b)` then `copy VALUES (...)`) does
/// not start a statement. The look-ahead ends at the first token that does
/// not fit, and a column list is read once, so it stays linear.
fn copy_statement_follows(src: &str, from: usize, flavor: Flavor) -> bool {
    let mut toks = Lexer::new(src, flavor, true, from)
        .filter(|t| !matches!(t.kind, Kind::Space | Kind::Comment));
    let text = |t: &Tok| &src[t.start..t.end];
    let mut next = toks.next();
    if next
        .as_ref()
        .is_some_and(|t| text(t).eq_ignore_ascii_case("BINARY"))
    {
        next = toks.next();
    }
    // The table name: words and quoted names joined by dots.
    let mut name = false;
    loop {
        match next {
            Some(ref t) if !name && matches!(t.kind, Kind::Word | Kind::QuotedIdent) => {
                name = true;
            }
            Some(ref t) if name && text(t) == "." => name = false,
            _ => break,
        }
        next = toks.next();
    }
    if !name {
        return false;
    }
    if next.as_ref().is_some_and(|t| text(t) == "(") {
        let mut depth = 1usize;
        for t in toks.by_ref() {
            match text(&t) {
                "(" => depth += 1,
                ")" => depth -= 1,
                ";" => return false,
                _ => {}
            }
            if depth == 0 {
                break;
            }
        }
        next = toks.next();
    }
    next.is_some_and(|t| {
        let word = text(&t);
        word.eq_ignore_ascii_case("FROM") || word.eq_ignore_ascii_case("TO")
    })
}

/// The secrets of a psql meta-command line: what the comment rules find
/// (connection strings and URLs of `\connect`, command lines of `\!`,
/// `PASSWORD 'x'`), and the value of `\set name value` when the name is a
/// secret (`\set pw hunter2`).
pub(crate) fn meta_ranges(line: &str) -> Vec<(usize, usize)> {
    // A range never takes the line break: the next line stays a line of
    // its own (`\prompt 'Password: ' pw` then a statement).
    let line_end = line.trim_end_matches(['\r', '\n']).len();
    let mut ranges: Vec<(usize, usize)> = comment_secret_ranges(&line[..line_end])
        .into_iter()
        .filter(|&(start, end)| end > start)
        .collect();
    let body = line.trim_start().trim_start_matches('\\');
    let offset = line.len() - body.len();
    let mut words = body[..line_end.saturating_sub(offset)]
        .split([' ', '\t'])
        .scan(offset, |at, word| {
            let start = *at;
            *at += word.len() + 1;
            Some((start, word))
        })
        .filter(|(_, word)| !word.is_empty());
    // psql reads `\set` in lower case only; `\SET` is an error, still
    // recorded, and read the same way.
    if let (Some((_, command)), Some((_, name))) = (words.next(), words.next())
        && command.eq_ignore_ascii_case("set")
    {
        let value_start = words.next().map_or(line_end, |(start, _)| start);
        let value = &line[value_start.min(line_end)..line_end];
        let secret = match CopyColumn::of_name(name) {
            CopyColumn::Secret => true,
            // `\set password_min_length 12`: a number or date is kept.
            CopyColumn::Metadata(policy) => !metadata_text(value, policy),
            // `\set pass x`, `\set db_pass x`: short names psql scripts
            // use for a password variable.
            CopyColumn::Plain => key_parts(name)
                .last()
                .is_some_and(|part| matches!(part.as_str(), "pass" | "pwd" | "passwd")),
        };
        if secret && line_end > value_start {
            ranges.push((value_start, line_end));
        }
    }
    ranges
}

/// A psql meta-command line at text level `normalized`: its command and
/// `?` for any arguments (`\connect ?`).
pub(crate) fn meta_normalized(line: &str) -> String {
    let body = line.trim();
    let command = body.split_whitespace().next().unwrap_or_default();
    if body.len() > command.len() {
        format!("{command} ?")
    } else {
        command.to_string()
    }
}

impl CopyHead {
    /// Where the data of this statement starts when it ends at `end`: after
    /// the line break that follows it, past blanks and comments.
    fn data_start(&self, src: &str, end: usize) -> Option<usize> {
        let bytes = src.as_bytes();
        let mut j = end;
        loop {
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\r') {
                j += 1;
            }
            if src[j..].starts_with("--") {
                j = src[j..].find('\n').map_or(bytes.len(), |p| j + p);
            } else if src[j..].starts_with("/*") {
                j = src[j + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |p| j + 2 + p + 2);
            } else {
                break;
            }
        }
        (j < bytes.len() && bytes[j] == b'\n').then_some(j + 1)
    }

    /// The data format the options name.
    fn format(&self) -> CopyFormat {
        // Rows that cannot be split are treated like binary data: removed
        // whole.
        if self.binary || self.unsplittable != 0 {
            CopyFormat::Binary
        } else if self.csv {
            let quote = self.quote.unwrap_or(b'"');
            CopyFormat::Csv {
                delimiter: self.delimiter.unwrap_or(b','),
                header: self.header || self.pending == Some(CopyOption::Header),
                quote,
                escape: self.escape.unwrap_or(quote),
            }
        } else {
            CopyFormat::Text {
                delimiter: self.delimiter.unwrap_or(b'\t'),
            }
        }
    }

    /// Takes in the next significant token of the statement.
    fn read(&mut self, src: &str, tok: &Tok) {
        let text = &src[tok.start..tok.end];
        self.words += 1;
        let upper = text.to_ascii_uppercase();
        if self.words == 1 {
            self.copy = upper == "COPY";
            // A psql meta-command (`\connect`, `\set`, `\restrict`,
            // `\copy`): it ends at the end of its line.
            if text == "\\" {
                self.meta_line_end =
                    Some(src[tok.end..].find('\n').map_or(src.len(), |p| tok.end + p));
                // Bytes, never a slice: a multi-byte character may follow.
                self.copy = src.as_bytes()[tok.end..]
                    .get(..4)
                    .is_some_and(|word| word.eq_ignore_ascii_case(b"copy"));
            }
            return;
        }
        // The old `COPY BINARY table` form.
        if self.words == 2 && self.meta_line_end.is_none() && upper == "BINARY" {
            self.binary = true;
            self.words -= 1;
            return;
        }
        if !self.copy {
            return;
        }
        if self.meta_line_end.is_some() && self.words == 2 && upper == "COPY" {
            return;
        }
        if self.stdin {
            self.read_option(src, tok, text, &upper);
            return;
        }
        match text {
            "(" => {
                // `COPY (query) TO`: a query, not a table.
                if self.words == 2 && self.meta_line_end.is_none()
                    || (self.meta_line_end.is_some() && self.words == 3)
                {
                    self.query = true;
                }
                if !self.columns_done && !self.in_columns && self.depth == 0 && !self.query {
                    self.in_columns = true;
                }
                self.depth += 1;
                return;
            }
            ")" => {
                self.depth = self.depth.saturating_sub(1);
                if self.in_columns && self.depth == 0 {
                    self.columns
                        .push(self.current_column.take().unwrap_or(CopyColumn::Plain));
                    self.in_columns = false;
                    self.columns_done = true;
                }
                return;
            }
            "," if self.in_columns && self.depth == 1 => {
                self.columns
                    .push(self.current_column.take().unwrap_or(CopyColumn::Plain));
                return;
            }
            _ => {}
        }
        if self.in_columns {
            let name = match tok.kind {
                Kind::QuotedIdent => &src[tok.content_start..tok.content_end],
                _ => text,
            };
            self.current_column = Some(CopyColumn::of_name(name));
            return;
        }
        if self.depth > 0 {
            return;
        }
        if self.after_from && upper == "STDIN" {
            self.stdin = true;
            return;
        }
        self.after_from = upper == "FROM";
        if self.after_from {
            self.columns_done = true;
        }
    }

    /// Sets `DELIMITER`, `QUOTE` or `ESCAPE` to `value`. The server takes
    /// only one single-byte character (and no backslash as a text
    /// delimiter); any other value leaves the rows unsplittable, so they
    /// are removed whole.
    fn set_char_option(&mut self, option: CopyOption, value: &str) {
        let byte = match value.as_bytes() {
            [b] if b.is_ascii() => Some(*b),
            _ => None,
        };
        let (slot, bit) = match option {
            CopyOption::Delimiter => (&mut self.delimiter, 1),
            CopyOption::Quote => (&mut self.quote, 2),
            CopyOption::Escape => (&mut self.escape, 4),
            _ => return,
        };
        *slot = byte;
        if byte.is_none() || (option == CopyOption::Delimiter && byte == Some(b'\\')) {
            self.unsplittable |= bit;
        } else {
            self.unsplittable &= !bit;
        }
    }

    /// Reads one token of the options after `STDIN`, in the new (`WITH
    /// (FORMAT csv, HEADER false, ...)`) and the old (`CSV HEADER
    /// DELIMITER AS ','`) syntax.
    fn read_option(&mut self, src: &str, tok: &Tok, text: &str, upper: &str) {
        // `U&'!007C' UESCAPE '!'`: the value of the option before is read
        // again with that escape character.
        if std::mem::take(&mut self.uescape_next) {
            if let (Some((option, prefix, raw)), Kind::Str) = (self.unicode_value.take(), tok.kind)
            {
                let escape = src[tok.content_start..tok.content_end]
                    .chars()
                    .next()
                    .unwrap_or('\\');
                let value = decoded_literal(&prefix, &raw, false, escape);
                self.set_char_option(option, &value);
            }
            return;
        }
        let unicode = self.unicode_value.take();
        if upper == "UESCAPE" && unicode.is_some() {
            self.unicode_value = unicode;
            self.uescape_next = true;
            return;
        }
        if matches!(text, "(" | ")" | ",") {
            if self.pending == Some(CopyOption::Header) {
                self.header = true;
            }
            self.pending = None;
            return;
        }
        if let Some(option) = self.pending {
            if upper == "AS" {
                return;
            }
            let value = match tok.kind {
                // `E'\t'`, `U&'\007C'`, a doubled quote.
                Kind::Str => {
                    let prefix = &src[tok.start..tok.content_start];
                    let raw = &src[tok.content_start..tok.content_end];
                    if prefix.len() >= 2 && prefix.as_bytes()[..2].eq_ignore_ascii_case(b"u&") {
                        self.unicode_value = Some((option, prefix.to_string(), raw.to_string()));
                    }
                    decoded_literal(prefix, raw, tok.backslash, '\\')
                }
                // `FORMAT "csv"`.
                Kind::QuotedIdent => src[tok.content_start..tok.content_end].to_string(),
                _ => text.to_string(),
            };
            let lower = value.to_ascii_lowercase();
            let boolean = matches!(
                lower.as_str(),
                "true" | "false" | "on" | "off" | "0" | "1" | "match"
            );
            match option {
                CopyOption::Header if boolean => {
                    self.header = !matches!(lower.as_str(), "false" | "off" | "0");
                    self.pending = None;
                    return;
                }
                CopyOption::Header => {
                    // `HEADER` alone, followed by the next option.
                    self.header = true;
                    self.pending = None;
                }
                CopyOption::Format => {
                    self.csv = lower == "csv";
                    self.binary = lower == "binary";
                    self.pending = None;
                    return;
                }
                CopyOption::Delimiter => {
                    self.set_char_option(option, &value);
                    self.pending = None;
                    return;
                }
                CopyOption::Quote => {
                    self.set_char_option(option, &value);
                    self.pending = None;
                    return;
                }
                CopyOption::Escape => {
                    self.set_char_option(option, &value);
                    self.pending = None;
                    return;
                }
                CopyOption::Other => {
                    self.pending = None;
                    if tok.kind == Kind::Str || boolean || tok.kind == Kind::Number {
                        return;
                    }
                }
            }
        }
        self.pending = match upper {
            "CSV" => {
                self.csv = true;
                None
            }
            "BINARY" => {
                self.binary = true;
                None
            }
            "HEADER" => Some(CopyOption::Header),
            "FORMAT" => Some(CopyOption::Format),
            // `DELIMITER` and the old `USING DELIMITERS`.
            "DELIMITER" | "DELIMITERS" => Some(CopyOption::Delimiter),
            "QUOTE" => Some(CopyOption::Quote),
            "ESCAPE" => Some(CopyOption::Escape),
            "NULL" | "ENCODING" | "DEFAULT" | "FREEZE" | "ON_ERROR" | "LOG_VERBOSITY" => {
                Some(CopyOption::Other)
            }
            _ => None,
        };
    }
}

/// The `\.` lines of a text that end a `COPY` data section: for each, the
/// start of the line and the position after `\.`.
fn copy_terminators(src: &str) -> Vec<(usize, usize)> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut line = 0;
    while line < bytes.len() {
        let end = src[line..].find('\n').map_or(bytes.len(), |p| line + p);
        let text = src[line..end].trim_end_matches('\r');
        if text == "\\." {
            out.push((line, line + 2));
        }
        line = end + 1;
    }
    out
}

/// The secrets of `COPY` data rows, field by field, so a rule never runs
/// from one field into the next:
///
/// - a field of a secret column is removed;
/// - a field of a time, count or policy column (`password_changed_at`) is
///   removed unless it reads as one ([`metadata_text`]);
/// - in any other field, what the comment rules find (credentials, URLs,
///   `PASSWORD 'x'`, command lines) and what the SQL secret rules find in
///   SQL written into the field (`INSERT INTO users (name, password) ...`).
///
/// Binary data is removed whole; the `\.` line stays.
pub(crate) fn copy_data_ranges(
    data: &str,
    format: &CopyFormat,
    columns: &[CopyColumn],
    flavor: Flavor,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    if *format == CopyFormat::Binary {
        let body_end = data
            .rfind("\\.")
            .filter(|&p| p == 0 || data.as_bytes()[p - 1] == b'\n')
            .unwrap_or(data.len());
        let body = data[..body_end].trim_end_matches(['\r', '\n']);
        if !body.is_empty() {
            ranges.push((0, body.len()));
        }
        return ranges;
    }
    let quote = match *format {
        CopyFormat::Csv { quote, .. } => Some(quote),
        _ => None,
    };
    let any_secret = columns.iter().any(|c| *c != CopyColumn::Plain);
    for_each_copy_field(data, format, |field| {
        let CopyField {
            start,
            end,
            column,
            header,
            broken,
        } = field;
        if end <= start {
            return;
        }
        // A quoted field or an escaped line break still open at the end of
        // the data (a stray quote, the usual reason such a COPY fails):
        // the columns after it cannot be told apart, so the rest goes when
        // any column is secret.
        if broken && any_secret {
            ranges.push((start, end));
            return;
        }
        if header {
            return;
        }
        let value = &data[start..end];
        match columns.get(column).copied().unwrap_or(CopyColumn::Plain) {
            CopyColumn::Secret if value != "\\N" => ranges.push((start, end)),
            CopyColumn::Metadata(policy) if !metadata_text(value, policy) => {
                ranges.push((start, end));
            }
            _ => {
                ranges.extend(
                    data_field_ranges(value)
                        .into_iter()
                        .map(|(a, b)| (start + a, start + b)),
                );
                // A quoted CSV field is checked as the server reads it too
                // (`"{""password"": ""x""}"`): what that finds removes the
                // whole field.
                if let Some(q) = quote
                    && value.len() >= 2
                    && value.as_bytes()[0] == q
                {
                    let quote_char = char::from(q);
                    let inner = value[1..].strip_suffix(quote_char).unwrap_or(&value[1..]);
                    let doubled: String = [quote_char, quote_char].iter().collect();
                    let decoded = inner.replace(&doubled, &quote_char.to_string());
                    if !data_field_ranges(&decoded).is_empty() {
                        ranges.push((start, end));
                    }
                }
                // SQL written into a field (or rows read as data that are
                // SQL): only a field with words and spaces can hold one.
                if value.contains(' ') && value.bytes().any(|b| b.is_ascii_alphabetic()) {
                    match statement_ranges(value, flavor) {
                        Some(found) => {
                            ranges.extend(found.into_iter().map(|(a, b)| (start + a, start + b)));
                        }
                        None => ranges.push((start, end)),
                    }
                }
            }
        }
    });
    ranges
}

/// One field of `COPY` data found by [`for_each_copy_field`].
struct CopyField {
    start: usize,
    end: usize,
    column: usize,
    /// In the header record of CSV data with `HEADER`.
    header: bool,
    /// A quoted field or escaped line break was still open at the end of
    /// the data: everything from `start` on is one unsplittable piece.
    broken: bool,
}

/// True when a `\.` line starts at `i` of `data` (at the start of a line,
/// followed by a line break or the end).
fn terminator_at(data: &str, i: usize) -> bool {
    let bytes = data.as_bytes();
    (i == 0 || bytes.get(i - 1) == Some(&b'\n'))
        && bytes.get(i..i + 2) == Some(b"\\.")
        && matches!(bytes.get(i + 2), None | Some(b'\n' | b'\r'))
}

/// Calls `visit` for every field of `COPY` data in text or CSV format, up
/// to its `\.` line, which always ends the data (as psql reads it), even
/// inside a quoted field or after an escaped line break. Text fields are
/// split at the delimiter and line breaks that no backslash escapes
/// (`a\|b`, `line one\` + line break); CSV fields honour the quote and
/// escape characters, and the first record is the header when the options
/// say so. Only ASCII delimiters, quotes and escapes reach here, so every
/// field boundary is a character boundary.
fn for_each_copy_field(data: &str, format: &CopyFormat, mut visit: impl FnMut(CopyField)) {
    let bytes = data.as_bytes();
    let (delimiter, csv) = match *format {
        CopyFormat::Text { delimiter } => (delimiter, None),
        CopyFormat::Csv {
            delimiter,
            header,
            quote,
            escape,
        } => (delimiter, Some((header, quote, escape))),
        CopyFormat::Binary => return,
    };
    let mut record = 0usize;
    let mut column = 0usize;
    let mut field_start = 0usize;
    let mut quoted = false;
    // The field holds an escaped line break (text format).
    let mut continued = false;
    let mut i = 0;
    loop {
        let header = csv.is_some_and(|(header, _, _)| header) && record == 0;
        if terminator_at(data, i) {
            if i > field_start {
                // A field still open across lines ends before the `\.`
                // line, which stays.
                let mut end = i;
                while end > field_start && matches!(bytes[end - 1], b'\n' | b'\r') {
                    end -= 1;
                }
                visit(CopyField {
                    start: field_start,
                    end,
                    column,
                    header,
                    broken: quoted || continued,
                });
            }
            return;
        }
        let b = bytes.get(i).copied();
        match (csv, b) {
            (Some((_, quote, escape)), Some(c)) if quoted => {
                // An escaped character, or a doubled quote.
                if (c == escape && escape != quote && i + 1 < bytes.len())
                    || (c == quote && bytes.get(i + 1) == Some(&quote))
                {
                    i += 2;
                } else {
                    if c == quote {
                        quoted = false;
                    }
                    i += 1;
                }
                continue;
            }
            (Some((_, quote, _)), Some(c)) if c == quote => {
                quoted = true;
                i += 1;
                continue;
            }
            (None, Some(b'\\')) => {
                // An escaped character, delimiter or line break.
                if bytes.get(i + 1) == Some(&b'\n') {
                    continued = true;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
            (_, Some(c)) if c != delimiter && c != b'\n' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let end = i.min(bytes.len());
        let mut value_end = end;
        if value_end > field_start && bytes[value_end - 1] == b'\r' {
            value_end -= 1;
        }
        visit(CopyField {
            start: field_start,
            end: value_end.max(field_start),
            column,
            header,
            broken: quoted,
        });
        match b {
            None => return,
            Some(b'\n') => {
                record += 1;
                column = 0;
            }
            Some(_) => column += 1,
        }
        continued = false;
        i += 1;
        field_start = i;
    }
}

/// Words that may follow a password keyword without being the password.
const VALUE_KEYWORDS: &[&str] = &[
    "ACCOUNT",
    "ADMIN",
    "AND",
    "ATTRIBUTE",
    "BYPASSRLS",
    "COMMENT",
    "CONNECTION",
    "CREATEDB",
    "CREATEROLE",
    "CURRENT",
    "DEFAULT",
    "ENCRYPTED",
    "EXPIRE",
    "FAILED_LOGIN_ATTEMPTS",
    "HISTORY",
    "IN",
    "INHERIT",
    "INTERVAL",
    "LOGIN",
    "NEVER",
    "NOBYPASSRLS",
    "NOCREATEDB",
    "NOCREATEROLE",
    "NOINHERIT",
    "NOLOGIN",
    "NOREPLICATION",
    "NOSUPERUSER",
    "NULL",
    "OR",
    "PASSWORD",
    "PASSWORD_LOCK_TIME",
    "RANDOM",
    "REPLICATION",
    "REQUIRE",
    "REUSE",
    "ROLE",
    "SUPERUSER",
    "SYSID",
    "UNENCRYPTED",
    "USER",
    "VALID",
    "WITH",
];

/// Every keyword and option value of `CREATE / ALTER USER | ROLE | GROUP`
/// (PostgreSQL, MySQL, MariaDB) and `SET PASSWORD`. Inside those
/// statements any other bare word is redacted, so a misspelled keyword or
/// an unquoted password never survives.
const ACCOUNT_KEYWORDS: &[&str] = &[
    "ACCOUNT",
    "ADD",
    "ADMIN",
    "ALL",
    "ALTER",
    "AND",
    "AS",
    "ATTRIBUTE",
    "AUTHENTICATION",
    "BY",
    "BYPASSRLS",
    "CHALLENGE_RESPONSE",
    "CIPHER",
    "COMMENT",
    "CONNECTION",
    "CREATE",
    "CREATEDB",
    "CREATEROLE",
    "CURRENT",
    "CURRENT_ROLE",
    "CURRENT_USER",
    "DATABASE",
    "DAY",
    "DEFAULT",
    "DISCARD",
    "DROP",
    "ENCRYPTED",
    "EXISTS",
    "EXPIRE",
    "FACTOR",
    "FAILED_LOGIN_ATTEMPTS",
    "FINISH",
    "FOR",
    "FROM",
    "GROUP",
    "HISTORY",
    "IDENTIFIED",
    "IF",
    "IN",
    "INHERIT",
    "INITIAL",
    "INITIATE",
    "INTERVAL",
    "ISSUER",
    "LIMIT",
    "LOCK",
    "LOGIN",
    "MAX_CONNECTIONS_PER_HOUR",
    "MAX_QUERIES_PER_HOUR",
    "MAX_STATEMENT_TIME",
    "MAX_UPDATES_PER_HOUR",
    "MAX_USER_CONNECTIONS",
    "MODIFY",
    "NEVER",
    "NOBYPASSRLS",
    "NOCREATEDB",
    "NOCREATEROLE",
    "NOINHERIT",
    "NOLOGIN",
    "NONE",
    "NOREPLICATION",
    "NOSUPERUSER",
    "NOT",
    "NULL",
    "OLD",
    "OLD_PASSWORD",
    "ON",
    "OPTION",
    "OPTIONAL",
    "OR",
    "PASSWORD",
    "PASSWORD_LOCK_TIME",
    "PUBLIC",
    "RANDOM",
    "REGISTRATION",
    "RENAME",
    "REPLACE",
    "REPLICATION",
    "REQUIRE",
    "RESET",
    "RETAIN",
    "REUSE",
    "ROLE",
    "SESSION_USER",
    "SET",
    "SSL",
    "SUBJECT",
    "SUPERUSER",
    "SYSID",
    "TO",
    "UNBOUNDED",
    "UNENCRYPTED",
    "UNLOCK",
    "UNREGISTER",
    "UNTIL",
    "USER",
    "USING",
    "VALID",
    "VIA",
    "WITH",
    "X509",
];

/// Server settings and account options whose name holds `password` but
/// whose value is a policy number or switch, never a password (MySQL,
/// MariaDB and PostgreSQL), upper case.
const PASSWORD_POLICY_SETTINGS: &[&str] = &[
    "CACHING_SHA2_PASSWORD_AUTO_GENERATE_RSA_KEYS",
    "CACHING_SHA2_PASSWORD_DIGEST_ROUNDS",
    "DEFAULT_PASSWORD_LIFETIME",
    "DISCONNECT_ON_EXPIRED_PASSWORD",
    "GENERATED_RANDOM_PASSWORD_LENGTH",
    "LOG_BUILTIN_AS_IDENTIFIED_BY_PASSWORD",
    "MAX_PASSWORD_ERRORS",
    "MYSQL_NATIVE_PASSWORD_PROXY_USERS",
    "OLD_PASSWORDS",
    "PASSWORD_ENCRYPTION",
    "PASSWORD_HISTORY",
    "PASSWORD_LOCK_TIME",
    "PASSWORD_REQUIRE_CURRENT",
    "PASSWORD_REUSE_CHECK_INTERVAL",
    "PASSWORD_REUSE_INTERVAL",
    "SHA256_PASSWORD_AUTO_GENERATE_RSA_KEYS",
    "SHA256_PASSWORD_PROXY_USERS",
    "SIMPLE_PASSWORD_CHECK_DIGITS",
    "SIMPLE_PASSWORD_CHECK_LETTERS_SAME_CASE",
    "SIMPLE_PASSWORD_CHECK_MINIMAL_LENGTH",
    "SIMPLE_PASSWORD_CHECK_OTHER_CHARACTERS",
    "STRICT_PASSWORD_VALIDATION",
];

/// Values of a password policy setting that are kept: switches and
/// algorithm names (numbers are kept too), upper case.
const POLICY_VALUES: &[&str] = &[
    "DEFAULT",
    "FALSE",
    "MD5",
    "NO",
    "OFF",
    "ON",
    "SCRAM-SHA-256",
    "TRUE",
    "YES",
];

/// Words after which a role or user name follows.
const NAME_INTRODUCERS: &[&str] = &["ROLE", "USER", "GROUP", "ADMIN", "FOR", "DATABASE"];

/// Statements whose values are all treated as possible secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Account {
    /// `CREATE / ALTER USER | ROLE | GROUP`, also misspelled.
    Role,
    /// `SET PASSWORD ...` and a `SET` of a variable that looks like
    /// `password` (`SET PASWORD = '...'`).
    SetPassword,
}

/// True for a name that holds a secret: a secret key, a word that looks
/// like `password`, or a name with such a part (`MASTER_PASWORD`,
/// `app.api_token`).
fn secret_word(name: &str) -> bool {
    // A time, count, length or policy about a secret is no secret
    // (`password_changed_at`, `token_count`, `password_policy`).
    // A yes/no flag about a secret is no secret either (`has_password`,
    // `password_required`).
    !metadata_key(name) && !flag_key(&key_parts(name)) && names_secret(name)
}

/// True for a name about a secret: a secret name, or one with a part
/// such as `token`, `secret` or `otp` (`token_count`, `secret_rotated_at`).
fn about_secret(name: &str) -> bool {
    names_secret(name)
        || key_parts(name).iter().any(|part| {
            matches!(
                part.as_str(),
                "token" | "secret" | "otp" | "totp" | "mfa" | "apikey" | "pwd" | "passwd"
            )
        })
}

/// [`secret_word`] without the exception for times, counts and policies.
fn names_secret(name: &str) -> bool {
    // Words close to `password` that name something else.
    if key_parts(name)
        .iter()
        .any(|p| matches!(p.as_str(), "passport" | "passports" | "passwordless"))
    {
        return false;
    }
    is_secret_key(name)
        || name.to_ascii_lowercase().contains("passphrase")
        || like_password(name)
        || name.split(['_', '-', '.']).any(like_password)
}

/// True when `text` mentions a password or builds an account statement
/// (`IDENTIFIED`, also misspelled).
fn mentions_account(text: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| like_password(word) || within(word, "IDENTIFIED", 2))
}

/// True when `text` holds an account statement: `IDENTIFIED` (also
/// misspelled), `CREATE | ALTER USER | ROLE | GROUP` or `SET PASSWORD`.
fn holds_account_statement(text: &str) -> bool {
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect();
    words.iter().any(|w| within(w, "IDENTIFIED", 2))
        || words.windows(2).any(|pair| {
            let verb = (within(pair[0], "CREATE", 1) || within(pair[0], "ALTER", 1))
                && ["USER", "ROLE", "GROUP"]
                    .iter()
                    .any(|object| within(pair[1], object, 1));
            let set = pair[0].eq_ignore_ascii_case("SET") && like_password(pair[1]);
            verb || set
        })
}

/// True when `text` builds a credential statement: it holds an account
/// statement ([`holds_account_statement`]) or leaves a slot for a password
/// right after a password word (`' PASSWORD '`, `'PASSWORD %L'`,
/// `'password = '`). Text that only mentions a password (`'Your password
/// was reset at '`, `'password reset for %s'`) does not.
fn builds_credential(text: &str) -> bool {
    holds_account_statement(text) || password_slot(text)
}

/// True when a word like `password` in `text` is followed only by
/// separators (`=`, `:`, quotes, `(`) up to the end of the text or a
/// `format` placeholder (`%L`, `%s`, `%2$L`).
fn password_slot(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !(bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if like_password(&text[start..i]) {
            let rest = text[i..].trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, '=' | ':' | '\'' | '"' | '(')
            });
            if rest.is_empty() || rest.starts_with('%') {
                return true;
            }
        }
    }
    false
}

/// True for punctuation that ends an unquoted run of an unknown word.
fn ends_chunk(tok: &Tok, src: &str) -> bool {
    tok.kind == Kind::Space
        || (tok.kind == Kind::Punct
            && matches!(&src[tok.start..tok.end], "," | ";" | "(" | ")" | "="))
}

/// Where each byte of a text assembled from pieces of the source (decoded
/// literal bodies, comment bodies) came from: the source span it was read
/// from, or `None` for a byte that is not in the source (a separator).
/// An escaped character spans its escape too (`''`, `\x`), so a range
/// mapped back never leaves part of an escape sequence behind.
#[derive(Debug, Default)]
struct SourceMap {
    spans: Vec<Option<(usize, usize)>>,
}

impl SourceMap {
    fn push(&mut self, start: usize, end: usize) {
        self.spans.push(Some((start, end)));
    }

    fn push_gap(&mut self) {
        self.spans.push(None);
    }

    fn len(&self) -> usize {
        self.spans.len()
    }
}

/// Decodes the body of a quoted literal (`''` and, with `backslash`, `\x`
/// escapes) at source offset `base`, appending the decoded bytes to `out`
/// and their source spans to `map`.
fn decode_literal(
    raw: &str,
    base: usize,
    quote: u8,
    backslash: bool,
    out: &mut Vec<u8>,
    map: &mut SourceMap,
) {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if backslash && bytes[i] == b'\\' && i + 1 < bytes.len() {
            let len = char_len(bytes[i + 1]).min(bytes.len() - i - 1);
            for k in 0..len {
                // The first byte also covers the backslash.
                let start = if k == 0 { i } else { i + 1 + k };
                map.push(base + start, base + i + 2 + k);
                out.push(bytes[i + 1 + k]);
            }
            i += 1 + len;
        } else if bytes[i] == quote && bytes.get(i + 1) == Some(&quote) {
            map.push(base + i, base + i + 2);
            out.push(quote);
            i += 2;
        } else {
            map.push(base + i, base + i + 1);
            out.push(bytes[i]);
            i += 1;
        }
    }
}

/// Sorts `ranges` and merges the ones that overlap or touch, dropping
/// empty ones.
pub(crate) fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.retain(|(start, end)| start < end);
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Maps `ranges` of a text assembled from pieces of the source back to
/// source ranges, split wherever the pieces are not contiguous. The ranges
/// are merged first, so every byte of the text is visited at most once:
/// the work is linear in the length of the text, however many ranges
/// overlap.
fn push_mapped(out: &mut Vec<(usize, usize)>, ranges: Vec<(usize, usize)>, map: &SourceMap) {
    for (a, b) in merge_ranges(ranges) {
        let mut piece: Option<(usize, usize)> = None;
        for span in map.spans.get(a..b.min(map.len())).unwrap_or_default() {
            match (piece, *span) {
                (Some((start, end)), Some((from, to))) if from <= end => {
                    piece = Some((start, end.max(to)));
                }
                (_, span) => {
                    if let Some(done) = piece.take() {
                        out.push(done);
                    }
                    piece = span;
                }
            }
        }
        if let Some(done) = piece {
            out.push(done);
        }
    }
}

/// The text of a comment without its markers (`--`, `#`, `/*`, `*/`).
fn comment_body(src: &str, tok: Tok) -> (usize, usize) {
    let text = &src[tok.start..tok.end];
    if let Some(body) = text.strip_prefix("/*") {
        let body_end = body.strip_suffix("*/").map_or(body.len(), str::len);
        (tok.start + 2, tok.start + 2 + body_end)
    } else if text.starts_with("--") {
        (tok.start + 2, tok.end)
    } else {
        (tok.start + 1, tok.end)
    }
}

/// How deep SQL held in strings may nest (a routine body holding a
/// routine body, an `init_connect` value holding SQL with strings). Deeper
/// text is hidden as a whole rather than redacted, so the work stays linear
/// and the stack bounded.
const MAX_NESTING: usize = 16;

/// The arguments of a function call that hold a key, a password or a
/// plaintext secret, by function name (upper case), dialect and number of
/// arguments.
fn key_arguments(name: &str, flavor: Flavor, arguments: usize) -> &'static [usize] {
    match (name, flavor) {
        ("AES_ENCRYPT" | "AES_DECRYPT" | "DES_ENCRYPT" | "DES_DECRYPT", _)
        | (
            "PGP_SYM_ENCRYPT"
            | "PGP_SYM_DECRYPT"
            | "PGP_SYM_ENCRYPT_BYTEA"
            | "PGP_SYM_DECRYPT_BYTEA"
            | "HMAC",
            _,
        ) => &[1],
        // pgp_pub_decrypt(msg, key, psw): the key's passphrase.
        ("PGP_PUB_DECRYPT" | "PGP_PUB_DECRYPT_BYTEA", _) => &[2],
        // pgcrypto encrypt(data, key, type); MySQL's ENCRYPT(str, salt)
        // takes no key.
        ("ENCRYPT" | "DECRYPT" | "ENCRYPT_IV" | "DECRYPT_IV", Flavor::Postgres) => &[1],
        // crypt(password, salt): the plaintext password.
        ("CRYPT", Flavor::Postgres) => &[0],
        // MySQL ENCODE(str, pass) and DECODE(crypt, pass); other DECODE
        // forms (Oracle style, PostgreSQL decode(text, format)) differ.
        ("ENCODE" | "DECODE", Flavor::Mysql) if arguments == 2 => &[1],
        _ => &[],
    }
}

/// MySQL server variables holding SQL run on every connection or replica
/// start.
const SQL_VARIABLES: &[&str] = &["INIT_CONNECT", "INIT_REPLICA", "INIT_SLAVE"];

/// How much work the redactor may do on one text, per byte of input. Every
/// pass charges what it reads; a text that would cost more is hidden as a
/// whole instead of being redacted in part.
pub(crate) const STEPS_PER_BYTE: u64 = 64;

/// Steps every text may use regardless of its length.
pub(crate) const BASE_STEPS: u64 = 100_000;

/// The work left for one text and the texts nested in it.
#[derive(Debug, Clone)]
pub(crate) struct Budget(Rc<Cell<u64>>);

impl Budget {
    pub(crate) fn for_text(len: usize) -> Self {
        let len = u64::try_from(len).unwrap_or(u64::MAX);
        Budget(Rc::new(Cell::new(
            len.saturating_mul(STEPS_PER_BYTE)
                .saturating_add(BASE_STEPS),
        )))
    }

    /// Spends `steps`; false once the budget is used up.
    pub(crate) fn spend(&self, steps: usize) -> bool {
        let steps = u64::try_from(steps).unwrap_or(u64::MAX);
        let left = self.0.get();
        self.0.set(left.saturating_sub(steps));
        left >= steps
    }
}

/// Words that start a statement inside a routine or block body.
const BLOCK_WORDS: [&str; 5] = ["BEGIN", "THEN", "ELSE", "LOOP", "REPEAT"];

/// Positions computed once per text, so that every pass reads them in
/// constant time instead of scanning ahead (which made repeated
/// constructs quadratic). Positions are indexes of significant tokens,
/// except the `run_*` and `chunk_first` tables, which index all tokens.
struct Tables {
    /// For `(` and `[`: the position of the matching close, or the end.
    close: Vec<u32>,
    /// The next statement boundary after each token: `;`, or a block word
    /// (`BEGIN`, `THEN`, ...) at depth 0.
    stmt_end: Vec<u32>,
    /// The end of an assignment value starting at each token: the next
    /// `,`, `;`, `)` or `]` at the same depth, or the word `SET`.
    assign_end: Vec<u32>,
    /// The next `=` or `;` at or after each token.
    next_eq: Vec<u32>,
    /// For `(`: the number of top-level arguments of the call.
    arguments: Vec<u32>,
    /// Over all tokens: the first token of the glued chunk.
    chunk_first: Vec<u32>,
    /// Over all tokens: where an unquoted run starting there ends, up to
    /// whitespace or `;` (`plain`) or also `,` `(` `)` `=` (`punct`).
    run_plain: Vec<u32>,
    run_punct: Vec<u32>,
}

fn to_u32(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

fn from_u32(i: u32) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

impl Tables {
    fn new(src: &str, toks: &[Tok], sig: &[usize]) -> Self {
        let n = sig.len();
        let punct = |p: usize, c: &str| {
            let tok = &toks[sig[p]];
            tok.kind == Kind::Punct && &src[tok.start..tok.end] == c
        };
        let word_is = |p: usize, words: &[&str]| {
            let tok = &toks[sig[p]];
            tok.kind == Kind::Word
                && words
                    .iter()
                    .any(|w| src[tok.start..tok.end].eq_ignore_ascii_case(w))
        };
        let mut close = vec![to_u32(n); n];
        let mut depth = vec![0u32; n];
        let mut arguments = vec![0u32; n];
        // (open position, commas, any token inside)
        let mut stack: Vec<(usize, u32, bool)> = Vec::new();
        for (p, level) in depth.iter_mut().enumerate() {
            *level = to_u32(stack.len());
            let opens = punct(p, "(") || punct(p, "[");
            let closes = punct(p, ")") || punct(p, "]");
            if closes {
                if let Some((open, commas, inside)) = stack.pop() {
                    close[open] = to_u32(p);
                    arguments[open] = if inside { commas + 1 } else { 0 };
                }
                if let Some(top) = stack.last_mut() {
                    top.2 = true;
                }
                continue;
            }
            if let Some(top) = stack.last_mut() {
                if punct(p, ",") {
                    top.1 += 1;
                }
                top.2 = true;
            }
            if opens {
                stack.push((p, 0, false));
            }
        }
        for (open, commas, inside) in stack {
            arguments[open] = if inside { commas + 1 } else { 0 };
        }
        let mut stmt_end = vec![to_u32(n); n];
        let mut assign_end = vec![to_u32(n); n];
        let mut next_eq = vec![to_u32(n); n];
        let mut boundary = to_u32(n);
        for p in (0..n).rev() {
            stmt_end[p] = boundary;
            if punct(p, ";") || (depth[p] == 0 && word_is(p, &BLOCK_WORDS)) {
                boundary = to_u32(p);
            }
            next_eq[p] = if punct(p, "=") || punct(p, ";") {
                to_u32(p)
            } else {
                next_eq.get(p + 1).copied().unwrap_or(to_u32(n))
            };
            assign_end[p] = if punct(p, ",")
                || punct(p, ";")
                || punct(p, ")")
                || punct(p, "]")
                || word_is(p, &["SET"])
            {
                to_u32(p)
            } else if punct(p, "(") || punct(p, "[") {
                let after = from_u32(close[p]).saturating_add(1);
                assign_end.get(after).copied().unwrap_or(to_u32(n))
            } else {
                assign_end.get(p + 1).copied().unwrap_or(to_u32(n))
            };
        }
        let total = toks.len();
        let mut chunk_first = vec![0u32; total];
        for i in 0..total {
            chunk_first[i] = if i > 0 && !ends_chunk(&toks[i - 1], src) {
                chunk_first[i - 1]
            } else {
                to_u32(i)
            };
        }
        let semicolon = |t: &Tok| t.kind == Kind::Punct && &src[t.start..t.end] == ";";
        let mut run_plain = vec![0u32; total];
        let mut run_punct = vec![0u32; total];
        for i in (0..total).rev() {
            let own = to_u32(toks[i].end);
            let next = toks.get(i + 1);
            run_plain[i] = match next {
                Some(t) if t.kind != Kind::Space && !semicolon(t) => run_plain[i + 1],
                _ => own,
            };
            run_punct[i] = match next {
                Some(t) if !ends_chunk(t, src) => run_punct[i + 1],
                _ => own,
            };
        }
        Tables {
            close,
            stmt_end,
            assign_end,
            next_eq,
            arguments,
            chunk_first,
            run_plain,
            run_punct,
        }
    }
}

/// What a parenthesized group is, for the single forward pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    /// Statement level, or a group without special meaning.
    Plain,
    /// `CONCAT(` / `CONCAT_WS(`.
    Concat,
    /// `format(`.
    Format,
    /// `OPTIONS (`.
    Options,
    /// `jsonb_build_object(` / `JSON_OBJECT(` and the like: keys and
    /// values alternate.
    Json,
}

/// An open group in the single forward pass of [`Redactions::collect`].
#[derive(Debug, Clone)]
struct Frame {
    kind: FrameKind,
    /// Arguments of this call that hold a key or password.
    targets: &'static [usize],
    /// The current top-level argument.
    argument: usize,
    /// Every literal and number in the group is a secret (an enclosing
    /// key argument, a `PASSWORD(...)` group, ...).
    inherited: bool,
    /// A literal of the group builds an account statement or mentions a
    /// password (for `CONCAT`).
    mentions: bool,
    /// Where this group's literals start in the shared literal list (for
    /// `CONCAT`).
    literals_from: usize,
    /// The format string of `format(`, or an argument it splices in,
    /// builds an account statement: the later arguments are secrets.
    format_account: bool,
    /// A literal that builds an account statement was followed by `||` in
    /// the current argument: what follows is concatenated into it.
    chain: bool,
    /// The literals of the current `||` chain so far, each followed by
    /// `||`: when a later literal of the chain builds a credential
    /// statement, they are part of it.
    operands: Vec<usize>,
}

impl Frame {
    fn new(
        kind: FrameKind,
        targets: &'static [usize],
        inherited: bool,
        literals_from: usize,
    ) -> Self {
        Frame {
            kind,
            targets,
            argument: 0,
            inherited,
            mentions: false,
            literals_from,
            format_account: false,
            chain: false,
            operands: Vec::new(),
        }
    }

    /// True when a literal directly in the current argument is a secret.
    fn secret_literal(&self) -> bool {
        self.inherited
            || self.targets.contains(&self.argument)
            || (self.kind == FrameKind::Format && self.format_account && self.argument >= 1)
            || self.chain
    }
}

/// The indexes of the secret columns of a column list, looked up in
/// constant time for every value of every row.
#[derive(Debug, Default)]
struct SecretColumns {
    secret: Vec<bool>,
    /// Columns holding a time, count or policy about a secret
    /// (`password_changed_at`, `token_count`, `password_policy`): their
    /// value is kept only when it reads as one ([`metadata_text`]).
    metadata: Vec<Option<bool>>,
}

impl SecretColumns {
    fn insert(&mut self, index: usize) {
        if self.secret.len() <= index {
            self.secret.resize(index + 1, false);
        }
        self.secret[index] = true;
    }

    /// Marks a metadata column; `policy` for a policy name (`*_policy`),
    /// whose value may be a word (`'strong'`).
    fn insert_metadata(&mut self, index: usize, policy: bool) {
        if self.metadata.len() <= index {
            self.metadata.resize(index + 1, None);
        }
        self.metadata[index] = Some(policy);
    }

    fn contains(&self, index: usize) -> bool {
        self.secret.get(index).copied().unwrap_or(false)
    }

    /// `Some(policy)` for a metadata column.
    fn metadata(&self, index: usize) -> Option<bool> {
        self.metadata.get(index).copied().flatten()
    }

    fn is_empty(&self) -> bool {
        !self.secret.contains(&true) && !self.metadata.iter().any(Option::is_some)
    }
}

/// True for a policy name (`password_policy`, `password_hint`), whose
/// value may be a plain word.
fn policy_name(name: &str) -> bool {
    key_parts(name)
        .last()
        .is_some_and(|last| matches!(last.as_str(), "policy" | "hint" | "strength" | "rules"))
}

/// True when the text of a value given for a metadata column reads as a
/// time, date, number or null (`2024-01-01 10:00:00+00`, `3`, `\N`), or,
/// for a policy name, as one plain word (`strong`): never a password.
pub(crate) fn metadata_text(value: &str, policy: bool) -> bool {
    let value = value.trim();
    let date_or_number = value.bytes().any(|b| b.is_ascii_digit())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || b" -:.+/TZtz".contains(&b));
    let null = value.is_empty() || value == "\\N" || value.eq_ignore_ascii_case("null");
    let word =
        policy && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    date_or_number || null || word
}

/// How a routine body in another language than SQL is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptComments {
    /// `# ...` comments (PL/Python, PL/Perl, PL/Tcl, PL/R, PL/CoffeeScript,
    /// PL/LiveScript).
    Hash,
    /// `// ...` and `/* ... */` comments (PL/v8, JavaScript, Java).
    Slash,
    /// The body is a shell script (PL/sh): read whole as text.
    Shell,
}

impl ScriptComments {
    /// The style of a routine language name, or `None` for SQL languages
    /// (`sql`, `plpgsql`) and unknown ones, which are read as SQL.
    fn of_language(name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase();
        if name.starts_with("plsh") {
            Some(ScriptComments::Shell)
        } else if ["plpython", "plperl", "pltcl", "plr", "plcoffee", "plls"]
            .iter()
            .any(|l| name.starts_with(l))
        {
            Some(ScriptComments::Hash)
        } else if ["plv8", "javascript", "pljava", "js"]
            .iter()
            .any(|l| name.starts_with(l))
        {
            Some(ScriptComments::Slash)
        } else {
            None
        }
    }
}

/// Names of functions that run SQL given as a string in a script body
/// (`plpy.execute`, `plv8.execute`, `spi_exec_query`, ...).
const SCRIPT_SQL_CALLS: &[&str] = &[
    "execute",
    "prepare",
    "cursor",
    "spi_exec_query",
    "spi_exec",
    "spi_query",
    "spi_prepare",
    "spi_exec_prepared",
    "subtransaction",
];

/// What a script body holds for the redactor: the secret ranges its
/// comments and string literals show to the comment rules, and the string
/// literals given to a SQL-running call, which are read as SQL (offsets in
/// `body`).
struct ScriptParts {
    ranges: Vec<(usize, usize)>,
    sql: Vec<(usize, usize)>,
    /// Every string literal of the body (contents).
    strings: Vec<(usize, usize)>,
}

/// Reads a script body: comments (one block, mapped back), string
/// literals (Python triple quotes, JavaScript template strings and regular
/// expression literals included, so a quote in a regex hides nothing), and
/// the SQL strings of [`SCRIPT_SQL_CALLS`]. A shell body is read whole.
fn script_parts(body: &str, style: ScriptComments) -> ScriptParts {
    if style == ScriptComments::Shell {
        return ScriptParts {
            ranges: comment_secret_ranges(body),
            sql: Vec::new(),
            strings: Vec::new(),
        };
    }
    let bytes = body.as_bytes();
    let mut block = String::new();
    let mut map: Vec<usize> = Vec::new();
    let push = |block: &mut String, map: &mut Vec<usize>, from: usize, to: usize| {
        if !block.is_empty() {
            block.push('\n');
            map.push(from);
        }
        block.push_str(&body[from..to]);
        map.extend(from..to);
    };
    let mut strings: Vec<(usize, usize)> = Vec::new();
    let mut sql: Vec<(usize, usize)> = Vec::new();
    // The last significant byte before the current position (for regex
    // literals) and the end of the last word.
    let mut last_significant: Option<u8> = None;
    let mut last_word: (usize, usize) = (0, 0);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let line_comment = match style {
            ScriptComments::Hash => b == b'#',
            _ => b == b'/' && bytes.get(i + 1) == Some(&b'/'),
        };
        if line_comment {
            let end = body[i..].find('\n').map_or(bytes.len(), |p| i + p);
            push(&mut block, &mut map, i + 1, end);
            i = end;
            continue;
        }
        if style == ScriptComments::Slash && b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let end = body[i + 2..].find("*/").map_or(bytes.len(), |p| i + 2 + p);
            push(&mut block, &mut map, i + 2, end);
            i = (end + 2).min(bytes.len());
            continue;
        }
        if style == ScriptComments::Slash
            && b == b'/'
            && (last_significant.is_none_or(|c| b"(,=:[!&|?{};+-*%<>~^".contains(&c))
                || matches!(&body[last_word.0..last_word.1], "return" | "typeof"))
        {
            // A regular expression literal: to its closing `/` on the line.
            let mut j = i + 1;
            let mut class = false;
            while j < bytes.len() && bytes[j] != b'\n' {
                match bytes[j] {
                    b'\\' => j += 1,
                    b'[' => class = true,
                    b']' => class = false,
                    b'/' if !class => break,
                    _ => {}
                }
                j += 1;
            }
            i = (j + 1).min(bytes.len());
            last_significant = Some(b'/');
            continue;
        }
        if matches!(b, b'\'' | b'"' | b'`') {
            let triple = style == ScriptComments::Hash
                && bytes.get(i + 1) == Some(&b)
                && bytes.get(i + 2) == Some(&b);
            let open = if triple { i + 3 } else { i + 1 };
            let mut j = open;
            loop {
                match bytes.get(j) {
                    None => break,
                    Some(b'\\') => j += 2,
                    Some(&c)
                        if c == b
                            && (!triple
                                || (bytes.get(j + 1) == Some(&b)
                                    && bytes.get(j + 2) == Some(&b))) =>
                    {
                        break;
                    }
                    Some(b'\n') if !triple && b != b'`' => break,
                    Some(_) => j += 1,
                }
            }
            let close = j.min(bytes.len());
            strings.push((open, close));
            // `plpy.execute("...")`: the call right before the string.
            let before = body[..i].trim_end();
            if let Some(call) = before.strip_suffix('(') {
                let name = call
                    .trim_end()
                    .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or_default();
                if SCRIPT_SQL_CALLS
                    .iter()
                    .any(|call| call.eq_ignore_ascii_case(name))
                {
                    sql.push((open, close));
                }
            }
            i = (close + if triple { 3 } else { 1 }).min(bytes.len());
            last_significant = Some(b);
            continue;
        }
        if b.is_ascii_alphanumeric() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            last_word = (start, i);
            last_significant = Some(bytes[i - 1]);
            continue;
        }
        if !b.is_ascii_whitespace() {
            last_significant = Some(b);
        }
        i += 1;
    }
    map.push(bytes.len());
    let mut ranges: Vec<(usize, usize)> = comment_secret_ranges(&block)
        .into_iter()
        .filter_map(|(a, b)| {
            let start = *map.get(a)?;
            let end = map.get(b.saturating_sub(1)).map(|&e| e + 1)?;
            (end > start).then_some((start, end))
        })
        .collect();
    for &(start, end) in &strings {
        ranges.extend(
            comment_secret_ranges(&body[start..end])
                .into_iter()
                .map(|(a, b)| (start + a, start + b)),
        );
    }
    ScriptParts {
        ranges,
        sql,
        strings,
    }
}

/// Collects the byte ranges to replace with `[redacted]`.
struct Redactions<'a> {
    src: &'a str,
    toks: &'a [Tok],
    flavor: Flavor,
    /// The MySQL backslash mode the text was lexed with.
    backslash: bool,
    /// How many levels of SQL held in a string (a routine body, an
    /// `init_connect` value) surround this text.
    depth: usize,
    /// Text nested deeper than [`MAX_NESTING`] was found, or the work
    /// budget ran out: the whole statement is hidden instead of redacted.
    too_deep: bool,
    /// Indexes into `toks` of the tokens that are not space or comments.
    sig: Vec<usize>,
    tables: Tables,
    /// The work left, shared with the texts nested in this one.
    budget: Budget,
    /// Opening parentheses (positions) whose group holds a secret as a
    /// whole (`PASSWORD('x')`, the value of a secret column), applied when
    /// the forward pass reaches them.
    group_marks: HashSet<usize>,
    ranges: Vec<(usize, usize)>,
    /// The routine statement being read is written in a language other
    /// than SQL: its body is read for that language's comments only.
    body_language: Option<ScriptComments>,
}

impl<'a> Redactions<'a> {
    fn new(src: &'a str, toks: &'a [Tok], flavor: Flavor) -> Self {
        let sig = toks
            .iter()
            .enumerate()
            .filter(|(_, t)| !matches!(t.kind, Kind::Space | Kind::Comment | Kind::ExecMark))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        let tables = Tables::new(src, toks, &sig);
        Redactions {
            src,
            toks,
            flavor,
            backslash: true,
            depth: 0,
            too_deep: false,
            sig,
            tables,
            budget: Budget::for_text(src.len()),
            group_marks: HashSet::new(),
            ranges: Vec::new(),
            body_language: None,
        }
    }

    /// Spends `steps` of the budget; once it is used up the text is hidden.
    fn spend(&mut self, steps: usize) -> bool {
        if !self.budget.spend(steps) {
            self.too_deep = true;
        }
        !self.too_deep
    }

    /// The next statement boundary after the statement starting at `p`.
    fn statement_end(&self, p: usize) -> usize {
        self.tables
            .stmt_end
            .get(p)
            .map_or(self.sig.len(), |&e| from_u32(e))
    }

    /// The matching close of the group opening at `p`, or the end.
    fn close_of(&self, p: usize) -> usize {
        self.tables
            .close
            .get(p)
            .map_or(self.sig.len(), |&e| from_u32(e))
    }

    /// The secret patterns of SQL held in a string of this text (a routine
    /// body, an `init_connect` value), one level deeper. With `literals`,
    /// every string literal in it counts as a secret too. `None` when the
    /// nesting is too deep.
    fn nested_ranges(&mut self, text: &str, literals: bool) -> Option<Vec<(usize, usize)>> {
        if self.depth + 1 >= MAX_NESTING || !self.spend(text.len()) {
            self.too_deep = true;
            return None;
        }
        let toks = lex_mode(text, self.flavor, self.backslash);
        let mut inner = Redactions::new(text, &toks, self.flavor);
        inner.budget = self.budget.clone();
        inner.backslash = self.backslash;
        inner.depth = self.depth + 1;
        inner.collect();
        if inner.too_deep {
            self.too_deep = true;
            return None;
        }
        if literals {
            for tok in &toks {
                if tok.kind == Kind::Str && tok.content_end > tok.content_start {
                    inner.ranges.push((tok.content_start, tok.content_end));
                }
            }
        }
        Some(merge_ranges(inner.ranges))
    }

    fn tok(&self, p: usize) -> Option<&'a Tok> {
        self.sig.get(p).map(|&i| &self.toks[i])
    }

    fn text(&self, p: usize) -> Option<&'a str> {
        self.tok(p).map(|t| &self.src[t.start..t.end])
    }

    fn is_punct(&self, p: usize, c: &str) -> bool {
        self.tok(p).is_some_and(|t| t.kind == Kind::Punct) && self.text(p) == Some(c)
    }

    fn upper_word(&self, p: usize) -> Option<String> {
        let tok = self.tok(p)?;
        (tok.kind == Kind::Word).then(|| self.src[tok.start..tok.end].to_ascii_uppercase())
    }

    /// Redacts the literal at `p`. A literal that looks terminated early by
    /// a backslash (`'abc\'def'` in a PostgreSQL standard string, or a MySQL
    /// session with `NO_BACKSLASH_ESCAPES`) is redacted up to the end it has
    /// under either escaping rule, so no part of the password survives.
    fn redact_literal(&mut self, p: usize) {
        let Some(tok) = self.tok(p).copied() else {
            return;
        };
        let bytes = self.src.as_bytes();
        if tok.content_end > tok.content_start {
            self.ranges.push((tok.content_start, tok.content_end));
        }
        // Under the other rule the literal may run further. That part is
        // redacted too, but the closing quote this reading found is kept,
        // so the stored text still has balanced quotes (`E'x\''` keeps
        // `E'[redacted]'`).
        if tok.content_start > tok.start {
            let quote = bytes[tok.content_start - 1];
            if quote == b'\'' || quote == b'"' {
                let (alt_end, _) = scan_quoted(bytes, tok.content_start, quote, !tok.backslash);
                if alt_end > tok.end {
                    self.ranges.push((tok.end, alt_end));
                }
            }
        }
    }

    /// End of the unquoted run that starts with the token at `p`: every
    /// following token up to whitespace or `;`, comments included (a MySQL
    /// `#` glued to the value hides the rest of the password from the
    /// server, not from the log). With `stop_at_punct`, `,` `(` `)` and `=`
    /// end the run too.
    fn run_end(&self, p: usize, stop_at_punct: bool) -> usize {
        let Some(&first) = self.sig.get(p) else {
            return self.src.len();
        };
        let table = if stop_at_punct {
            &self.tables.run_punct
        } else {
            &self.tables.run_plain
        };
        from_u32(table[first])
    }

    /// `start`, or the position after a [`REDACTED`] marker there (text
    /// redacted before keeps its marker, so redacting twice changes
    /// nothing, while anything glued after the marker is still redacted).
    fn after_marker(&self, start: usize, end: usize) -> usize {
        if !self.src[start..].starts_with(REDACTED) {
            return start;
        }
        let after = (start + REDACTED.len()).min(end.max(start));
        // Only quotes and backslashes after it (`[redacted]\'`): the quoting
        // of the text around, no value.
        if self.src.as_bytes()[after..end.max(after)]
            .iter()
            .all(|b| matches!(b, b'\\' | b'\'' | b'"' | b'`'))
        {
            end.max(after)
        } else {
            after
        }
    }

    /// Redacts an unquoted value run starting at `p` (`p@ss!w0rd`,
    /// `Sup3r-Secr3t`, `123hunter`, `abc#hunter`).
    fn redact_run(&mut self, p: usize) {
        if let Some(tok) = self.tok(p) {
            let end = self.run_end(p, false);
            let start = self.after_marker(tok.start, end);
            if start < end {
                self.ranges.push((start, end));
            }
        }
    }

    /// Redacts the whole glued chunk around the unknown word at `p`, from
    /// the previous to the next whitespace or separator.
    fn redact_chunk(&mut self, p: usize) {
        let Some(&index) = self.sig.get(p) else {
            return;
        };
        let first = from_u32(self.tables.chunk_first[index]);
        let end = self.run_end(p, true);
        let start = self.after_marker(self.toks[first].start, end);
        if start < end {
            self.ranges.push((start, end));
        }
    }

    /// True when the token at `p` is glued to a following token that is
    /// not a separator (`123hunter`).
    fn glued_to_next(&self, p: usize) -> bool {
        self.sig
            .get(p)
            .and_then(|&i| self.toks.get(i + 1))
            .is_some_and(|next| !ends_chunk(next, self.src))
    }

    /// Marks the group opening at `p`: the forward pass redacts every
    /// literal and number inside it.
    fn redact_group(&mut self, p: usize) {
        self.group_marks.insert(p);
    }

    /// True for a PostgreSQL parameter such as `$1`.
    fn parameter(&self, text: &str) -> bool {
        self.flavor == Flavor::Postgres
            && text.len() > 1
            && text.starts_with('$')
            && text[1..].bytes().all(|b| b.is_ascii_digit())
    }

    /// True when the token at `p` is a keyword such as `NULL` standing on
    /// its own (not glued to more text).
    fn standalone_keyword(&self, p: usize) -> bool {
        self.upper_word(p)
            .is_some_and(|w| VALUE_KEYWORDS.contains(&w.as_str()))
            && !self.glued_to_next(p)
    }

    /// True when `name` is a password policy setting and the value at `p`
    /// (after `=`, `:=` or `TO`) is a plain number or a switch
    /// (`password_history = 5`, `PASSWORD_LOCK_TIME 2`,
    /// `password_encryption = 'scram-sha-256'`), or `name` is a count or
    /// time about a password and the value a plain number
    /// (`password_changed_at = 1700000000`): such a value is kept.
    fn policy_value(&self, name: &str, mut p: usize) -> bool {
        let last = name.rsplit('.').next().unwrap_or_default();
        let policy = PASSWORD_POLICY_SETTINGS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(last));
        let metadata = metadata_key(last);
        if !policy && !metadata {
            return false;
        }
        if self.is_punct(p, ":") && self.is_punct(p + 1, "=") {
            p += 2;
        } else if self.is_punct(p, "=") || self.upper_word(p).as_deref() == Some("TO") {
            p += 1;
        }
        let Some(tok) = self.tok(p) else {
            return false;
        };
        if self.glued_to_next(p) {
            return false;
        }
        let value = match tok.kind {
            Kind::Number | Kind::Word => &self.src[tok.start..tok.end],
            Kind::Str => &self.src[tok.content_start..tok.content_end],
            _ => return false,
        };
        plain_number(value)
            || (policy
                && POLICY_VALUES
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(value)))
    }

    /// Redacts the secret value that starts at `p`. An unquoted value is a
    /// secret after `=` or when `bare_word` is set; keywords such as `NULL`
    /// or `LOGIN` never are. A number always is.
    fn redact_value(&mut self, mut p: usize, bare_word: bool) {
        let mut assigned = false;
        let mut bare_word = bare_word;
        loop {
            if self.is_punct(p, "=") {
                assigned = true;
                p += 1;
            } else if self.is_punct(p, ":") && self.is_punct(p + 1, "=") {
                assigned = true;
                p += 2;
            } else if self.upper_word(p).as_deref() == Some("BINARY") && self.tok(p + 1).is_some() {
                // `= BINARY 'secret'`: the value follows the cast keyword.
                bare_word = true;
                p += 1;
            } else {
                break;
            }
        }
        let Some(tok) = self.tok(p).copied() else {
            return;
        };
        // Text redacted before (`IDENTIFIED BY [redacted]`): only what is
        // glued after the marker is redacted.
        if self.src[tok.start..].starts_with(REDACTED) {
            self.redact_run(p);
            return;
        }
        let text = &self.src[tok.start..tok.end];
        match tok.kind {
            Kind::Str | Kind::QuotedIdent => self.redact_literal(p),
            Kind::Punct if text == "(" => self.redact_group(p),
            Kind::Punct if matches!(text, ";" | "," | ")") => {}
            // A MySQL charset introducer: `BY _utf8mb4 'secret'`.
            Kind::Word
                if text.starts_with('_')
                    && self.tok(p + 1).is_some_and(|next| next.kind == Kind::Str) =>
            {
                self.redact_literal(p + 1);
            }
            // A PostgreSQL parameter (`$1`) is no value; in MySQL `$word` is.
            Kind::Word if self.parameter(text) || self.standalone_keyword(p) => {}
            Kind::Word if self.is_punct(p + 1, "(") => self.redact_group(p + 1),
            Kind::Number => self.redact_run(p),
            Kind::Word | Kind::Punct if assigned || bare_word => self.redact_run(p),
            _ => {}
        }
    }

    /// Like [`Self::redact_value`], but an unquoted value is a secret even
    /// without `=` (`IDENTIFIED BY secret`), except for keywords such as
    /// `RANDOM`.
    fn redact_value_after_identified(&mut self, p: usize) {
        self.redact_value(p, true);
    }

    /// The kind of account statement starting at `p`, and the position
    /// after its head (`CREATE ROLE`, `SET PASSWORD`).
    fn account_statement(&self, p: usize) -> Option<(Account, usize)> {
        let verb = self.upper_word(p)?;
        if verb == "SET" {
            let mut q = p + 1;
            loop {
                match self.upper_word(q).as_deref() {
                    Some("SESSION" | "GLOBAL" | "LOCAL" | "PERSIST" | "PERSIST_ONLY") => q += 1,
                    _ if self.is_punct(q, "@") || self.is_punct(q, ".") => q += 1,
                    _ => break,
                }
            }
            // The last part of a dotted name decides: `SET PASSWORD` and
            // `SET PASWORD` are, `SET validate_password.length` is not.
            let mut variable = self.upper_word(q)?;
            while self.is_punct(q + 1, ".")
                && let Some(next) = self.upper_word(q + 2)
            {
                variable = next;
                q += 2;
            }
            // A password policy setting (`SET GLOBAL password_history = 5`)
            // is no password; its value is checked as a setting.
            let policy = PASSWORD_POLICY_SETTINGS.contains(&variable.as_str());
            return (like_password(&variable) && !policy).then_some((Account::SetPassword, q + 1));
        }
        // MariaDB `CREATE OR REPLACE USER`, and `IF NOT EXISTS` written
        // before the object.
        let mut at = p + 1;
        for skip in ["OR", "REPLACE", "IF", "NOT", "EXISTS"] {
            if self.upper_word(at).as_deref() == Some(skip) {
                at += 1;
            }
        }
        let object = self.upper_word(at)?;
        let verb_ok = within(&verb, "CREATE", 1) || within(&verb, "ALTER", 1);
        let object_ok = object != "RULE"
            && ["USER", "ROLE", "GROUP"]
                .iter()
                .any(|known| within(&object, known, 1));
        let mapping = self.upper_word(at + 1).as_deref() == Some("MAPPING");
        (verb_ok && object_ok && !mapping).then_some((Account::Role, at + 1))
    }

    /// Position of the role name in a role statement starting at `p`.
    fn role_name(&self, p: usize) -> Option<usize> {
        let (Account::Role, mut name) = self.account_statement(p)? else {
            return None;
        };
        if self.upper_word(name).as_deref() == Some("IF") {
            name += 1;
            if self.upper_word(name).as_deref() == Some("NOT") {
                name += 1;
            }
            if self.upper_word(name).as_deref() == Some("EXISTS") {
                name += 1;
            }
        }
        Some(name)
    }

    /// Inside an account statement, keeps only keywords, role and user
    /// names, plugin names and `VALID UNTIL` timestamps: every other
    /// literal and every unknown word is redacted.
    fn walk_account(&mut self, start: usize, kind: Account, body: usize) {
        let mut expect_name = kind == Account::Role;
        let mut expect_param = false;
        // The value of a `SET param = value` whose name is no secret
        // (`search_path`): configuration, kept as written.
        let mut setting = false;
        let mut identified = false;
        let mut depth = 0usize;
        let mut q = body;
        let end = self.statement_end(start);
        while q < end
            && let Some(tok) = self.tok(q).copied()
        {
            if self.is_punct(q, ";") || !self.spend(1) {
                break;
            }
            if setting {
                q += 1;
                continue;
            }
            let upper = self.upper_word(q);
            let prev = if q > start {
                self.upper_word(q - 1)
            } else {
                None
            };
            if tok.kind == Kind::Punct {
                match self.text(q) {
                    Some("(") => depth += 1,
                    Some(")") => depth = depth.saturating_sub(1),
                    Some(",") if depth == 0 && kind == Account::Role => expect_name = true,
                    _ => {}
                }
                q += 1;
                continue;
            }
            let introducer = matches!(upper.as_deref(), Some("IF" | "NOT" | "EXISTS"));
            if expect_name
                && !introducer
                && matches!(tok.kind, Kind::Word | Kind::Str | Kind::QuotedIdent)
            {
                // A name, with an optional `@host` part.
                expect_name = false;
                q += 1;
                if self.is_punct(q, "@")
                    && self.tok(q + 1).is_some_and(|t| {
                        matches!(t.kind, Kind::Word | Kind::Str | Kind::QuotedIdent)
                    })
                {
                    q += 2;
                }
                continue;
            }
            let plugin = identified && matches!(prev.as_deref(), Some("WITH" | "VIA" | "OR"));
            match tok.kind {
                Kind::Word => {
                    let word = upper.unwrap_or_default();
                    if expect_param {
                        expect_param = false;
                        // The parameter name stays; the value of a secret
                        // parameter is redacted by the rules below.
                        let (secret, after) = self.secret_name_at(q);
                        if !secret {
                            setting = true;
                        }
                        q = after;
                        continue;
                    } else if word == "SET" || word == "RESET" {
                        expect_param = true;
                    } else if NAME_INTRODUCERS.contains(&word.as_str())
                        || (word == "TO" && prev.as_deref() == Some("RENAME"))
                    {
                        expect_name = true;
                    } else if word == "IDENTIFIED" {
                        identified = true;
                    } else if !(ACCOUNT_KEYWORDS.contains(&word.as_str())
                        || plugin
                        || word.starts_with('$'))
                    {
                        self.redact_chunk(q);
                    }
                }
                Kind::Str | Kind::QuotedIdent => {
                    // MySQL `COMMENT 'text'` and `ATTRIBUTE '{json}'` are
                    // descriptions (their credentials are still redacted
                    // as in any literal), unless the keyword itself sits
                    // where a password belongs (`IDENTIFIED BY COMMENT`).
                    let description = matches!(prev.as_deref(), Some("COMMENT" | "ATTRIBUTE"))
                        && !q.checked_sub(2).is_some_and(|before| {
                            matches!(
                                self.upper_word(before).as_deref(),
                                Some("BY" | "AS" | "USING" | "PASSWORD")
                            )
                        });
                    if !(plugin || description || prev.as_deref() == Some("UNTIL")) {
                        self.redact_literal(q);
                    }
                }
                Kind::Number if self.glued_to_next(q) => self.redact_chunk(q),
                _ => {}
            }
            q += 1;
        }
    }

    /// The column named by the token at `p` (a word or quoted identifier),
    /// when it holds secrets (`password`, `pw`, `api_token`, ...).
    fn secret_column(&self, p: usize) -> bool {
        let Some(tok) = self.tok(p) else {
            return false;
        };
        let name = match tok.kind {
            Kind::Word => &self.src[tok.start..tok.end],
            Kind::QuotedIdent => &self.src[tok.content_start..tok.content_end],
            _ => return false,
        };
        secret_word(name)
    }

    /// The name at `p` is a time, count or policy about a secret
    /// (`password_reset_at`, `token_count`).
    fn metadata_column(&self, p: usize) -> bool {
        let Some(tok) = self.tok(p) else {
            return false;
        };
        let name = match tok.kind {
            Kind::Word => &self.src[tok.start..tok.end],
            Kind::QuotedIdent => &self.src[tok.content_start..tok.content_end],
            _ => return false,
        };
        metadata_key(name) && about_secret(name)
    }

    /// [`Self::metadata_value`] after an optional `=`, `:=` or `TO` at `p`.
    fn metadata_value_after(&self, mut p: usize, policy: bool) -> bool {
        if self.is_punct(p, ":") && self.is_punct(p + 1, "=") {
            p += 2;
        } else if self.is_punct(p, "=") || self.upper_word(p).as_deref() == Some("TO") {
            p += 1;
        }
        self.metadata_value(p, policy)
    }

    /// The value at `p` reads as a time, date, number or keyword, never as
    /// a password: a number, a literal of digits and date punctuation
    /// (`'2024-01-01 10:00:00+00'`), `NULL`, `DEFAULT`, or a call
    /// (`now()`). A free string (`'hunter2'`) is no such value.
    fn metadata_value(&self, p: usize, policy: bool) -> bool {
        let Some(tok) = self.tok(p) else {
            return true;
        };
        match tok.kind {
            Kind::Number => true,
            Kind::Str => metadata_text(&self.src[tok.content_start..tok.content_end], policy),
            Kind::Word => {
                self.is_punct(p + 1, "(")
                    || matches!(
                        self.upper_word(p).as_deref(),
                        Some(
                            "NULL"
                                | "DEFAULT"
                                | "TRUE"
                                | "FALSE"
                                | "CURRENT_TIMESTAMP"
                                | "CURRENT_DATE"
                                | "LOCALTIMESTAMP"
                        )
                    )
            }
            Kind::Punct => !self.is_punct(p, "("),
            _ => false,
        }
    }

    /// The column list in the parenthesis opening at `open`: the indexes of
    /// its secret columns, and the position of its closing parenthesis.
    fn column_list(&mut self, open: usize) -> (SecretColumns, usize) {
        let mut secret = SecretColumns::default();
        let mut column = 0;
        let close = self.close_of(open);
        let mut q = open + 1;
        while q < close && self.spend(1) {
            if self.is_punct(q, "(") || self.is_punct(q, "[") {
                q = self.close_of(q) + 1;
                continue;
            }
            if self.is_punct(q, ",") {
                column += 1;
            } else if self.secret_column(q) {
                secret.insert(column);
            } else if self.metadata_column(q) {
                let policy = self.text(q).is_some_and(policy_name);
                secret.insert_metadata(column, policy);
            }
            q += 1;
        }
        (secret, close)
    }

    /// Redacts the value token at `p` given for a secret column: a literal,
    /// a number, or a bare word (an unquoted password the server takes for
    /// a column name).
    fn redact_column_value(&mut self, p: usize) {
        let Some(tok) = self.tok(p).copied() else {
            return;
        };
        // In a list, a value ends at `,` or `)`.
        let end = self.run_end(p, true);
        match tok.kind {
            Kind::Str | Kind::QuotedIdent => self.redact_literal(p),
            Kind::Number => self.ranges.push((tok.start, end)),
            Kind::Word
                if !self.is_punct(p + 1, "(")
                    && !self.standalone_keyword(p)
                    && !self.src[tok.start..tok.end].starts_with('$') =>
            {
                self.ranges.push((tok.start, end));
            }
            _ => {}
        }
    }

    /// Redacts, in each parenthesized row from `from` to `end` (`VALUES
    /// (...), (...)`), the values at the `secret` indexes. A nested group in
    /// such a value is marked whole. With `single`, only the first row.
    fn redact_rows(&mut self, from: usize, end: usize, secret: &SecretColumns, single: bool) {
        let mut q = from;
        while q < end && self.spend(1) {
            if self.is_punct(q, ";") {
                break;
            }
            if !self.is_punct(q, "(") {
                q += 1;
                continue;
            }
            let close = self.close_of(q).min(end);
            let mut index = 0;
            let mut r = q + 1;
            while r < close && self.spend(1) {
                if self.is_punct(r, "(") || self.is_punct(r, "[") {
                    if secret.contains(index) {
                        self.redact_group(r);
                    }
                    r = self.close_of(r) + 1;
                    continue;
                }
                if self.is_punct(r, ",") {
                    index += 1;
                } else if secret.contains(index)
                    || secret
                        .metadata(index)
                        .is_some_and(|policy| !self.metadata_value(r, policy))
                {
                    self.redact_column_value(r);
                }
                r += 1;
            }
            if single {
                return;
            }
            q = close + 1;
        }
    }

    /// Redacts, in the select list from `from` to `end` (`INSERT INTO t
    /// (a, pw) SELECT 'x', 'y'`), the items at the `secret` indexes.
    fn redact_select_items(&mut self, from: usize, end: usize, secret: &SecretColumns) {
        let mut index = 0;
        let mut q = from;
        while q < end && self.spend(1) {
            if self.is_punct(q, ";")
                || matches!(
                    self.upper_word(q).as_deref(),
                    Some("FROM" | "WHERE" | "UNION" | "ORDER" | "LIMIT" | "GROUP")
                )
            {
                break;
            }
            if self.is_punct(q, "(") || self.is_punct(q, "[") {
                if secret.contains(index) {
                    self.redact_group(q);
                }
                q = self.close_of(q) + 1;
                continue;
            }
            if self.is_punct(q, ",") {
                index += 1;
            } else if secret.contains(index)
                || secret
                    .metadata(index)
                    .is_some_and(|policy| !self.metadata_value(q, policy))
            {
                self.redact_column_value(q);
            }
            q += 1;
        }
    }

    /// `INSERT | REPLACE INTO t (a, password, ...) VALUES (...), (...)` and
    /// `... (a, password) SELECT ...`: redacts the values given for secret
    /// columns.
    fn redact_insert_values(&mut self, start: usize) {
        if !matches!(
            self.upper_word(start).as_deref(),
            Some("INSERT" | "REPLACE")
        ) {
            return;
        }
        // The column list: the first parenthesized group before VALUES.
        let end = self.statement_end(start);
        let mut q = start + 1;
        while q < end
            && !self.is_punct(q, "(")
            && !self.is_punct(q, ";")
            && !matches!(
                self.upper_word(q).as_deref(),
                Some("VALUES" | "VALUE" | "SELECT" | "SET")
            )
        {
            q += 1;
        }
        if !self.is_punct(q, "(") {
            return;
        }
        let (secret, close) = self.column_list(q);
        if secret.is_empty() {
            return;
        }
        // Words between the list and the rows are skipped: `OVERRIDING
        // SYSTEM VALUE VALUES (...)`, and a stray word in a statement the
        // server rejects (`(name, password)` then `copy VALUES (...)`),
        // which is still recorded.
        let mut k = close + 1;
        while k < end {
            match self.upper_word(k).as_deref() {
                Some("VALUES") => return self.redact_rows(k + 1, end, &secret, false),
                Some("VALUE") if self.is_punct(k + 1, "(") => {
                    return self.redact_rows(k + 1, end, &secret, false);
                }
                Some("SELECT") => return self.redact_select_items(k + 1, end, &secret),
                Some(_) => k += 1,
                None => return,
            }
        }
    }

    /// `UPDATE t SET (a, password) = ('x', 'y')`, also `= ROW('x', 'y')`
    /// and `= (SELECT ...)`.
    fn redact_row_assignment(&mut self, set: usize) {
        if !self.is_punct(set + 1, "(") {
            return;
        }
        let (secret, close) = self.column_list(set + 1);
        let end = self.statement_end(set);
        if secret.is_empty() || !self.is_punct(close + 1, "=") {
            return;
        }
        let mut value = close + 2;
        if self.upper_word(value).as_deref() == Some("ROW") && self.is_punct(value + 1, "(") {
            value += 1;
        }
        if self.upper_word(value).as_deref() == Some("SELECT") {
            self.redact_select_items(value + 1, end, &secret);
        } else if self.is_punct(value, "(") {
            let group_end = self.close_of(value).min(end);
            if self.upper_word(value + 1).as_deref() == Some("SELECT") {
                self.redact_select_items(value + 2, group_end, &secret);
            } else {
                self.redact_rows(value, end, &secret, true);
            }
        }
    }

    /// True for a dotted name (`app.password`, `myapp.api_token`) whose
    /// last part names a secret (`validate_password.length` does not), and
    /// the position after the name.
    fn secret_name_at(&self, mut p: usize) -> (bool, usize) {
        let mut secret = false;
        loop {
            let Some(tok) = self.tok(p) else {
                return (secret, p);
            };
            let name = match tok.kind {
                Kind::Word => &self.src[tok.start..tok.end],
                Kind::QuotedIdent => &self.src[tok.content_start..tok.content_end],
                _ => return (secret, p),
            };
            secret = secret_word(name);
            if self.is_punct(p + 1, ".") {
                p += 2;
            } else {
                return (secret, p + 1);
            }
        }
    }

    /// The last part of the name that ends right before `p` (a word or a
    /// quoted identifier), empty when there is none.
    fn name_before(&self, p: usize) -> String {
        let Some(tok) = p.checked_sub(1).and_then(|q| self.tok(q)) else {
            return String::new();
        };
        match tok.kind {
            Kind::Word => self.src[tok.start..tok.end].to_string(),
            Kind::QuotedIdent => self.src[tok.content_start..tok.content_end].to_string(),
            _ => String::new(),
        }
    }

    /// `SET [SESSION | LOCAL | GLOBAL] name TO | = value` (also inside
    /// `ALTER DATABASE | ROLE ... SET`) where the name looks like a secret.
    fn redact_secret_setting(&mut self, set: usize) {
        let mut q = set + 1;
        while matches!(
            self.upper_word(q).as_deref(),
            Some("SESSION" | "LOCAL" | "GLOBAL" | "PERSIST" | "PERSIST_ONLY")
        ) || self.is_punct(q, "@")
        {
            q += 1;
        }
        let (secret, after) = self.secret_name_at(q);
        if !secret || self.policy_value(&self.name_before(after), after) {
            return;
        }
        if self.upper_word(after).as_deref() == Some("TO") {
            self.redact_value(after + 1, true);
        } else if self.is_punct(after, "=") {
            self.redact_value(after, true);
        }
    }

    /// `COPY ... FROM | TO PROGRAM 'command'`: a shell command line often
    /// carries credentials (`curl -u user:pw`, `PGPASSWORD=...`), so the
    /// whole command is redacted.
    fn redact_copy_program(&mut self, start: usize) {
        let mut q = start;
        let end = self.statement_end(start);
        while q < end && !self.is_punct(q, ";") && self.spend(1) {
            if self.upper_word(q).as_deref() == Some("PROGRAM")
                && self.tok(q + 1).is_some_and(|t| t.kind == Kind::Str)
            {
                self.redact_literal(q + 1);
            }
            q += 1;
        }
    }

    /// `set_config('app.password', 'value', false)`.
    fn redact_set_config(&mut self, p: usize) {
        if !self.is_punct(p + 1, "(") {
            return;
        }
        let Some(name) = self.tok(p + 2).copied() else {
            return;
        };
        let setting = &self.src[name.content_start..name.content_end];
        if name.kind == Kind::Str
            && setting.rsplit('.').next().is_some_and(secret_word)
            && self.is_punct(p + 3, ",")
        {
            self.redact_value(p + 4, true);
        }
    }

    /// The comment style of the `LANGUAGE` of the routine statement at `p`,
    /// when that is no SQL language.
    fn routine_language(&mut self, p: usize) -> Option<ScriptComments> {
        let end = self.statement_end(p);
        let mut q = p;
        while q < end && self.spend(1) {
            if self.upper_word(q).as_deref() == Some("LANGUAGE") {
                let tok = self.tok(q + 1)?;
                let name = match tok.kind {
                    Kind::Str | Kind::QuotedIdent => &self.src[tok.content_start..tok.content_end],
                    _ => &self.src[tok.start..tok.end],
                };
                return ScriptComments::of_language(name);
            }
            q += 1;
        }
        None
    }

    /// True when the statement starting at `p` defines a routine whose
    /// body is a literal (`DO`, `CREATE [OR REPLACE] FUNCTION | PROCEDURE`).
    fn routine_statement(&self, p: usize) -> bool {
        match self.upper_word(p).as_deref() {
            Some("DO") => true,
            Some("CREATE") => (p + 1..p + 4).any(|q| {
                matches!(
                    self.upper_word(q).as_deref(),
                    Some("FUNCTION" | "PROCEDURE")
                )
            }),
            _ => false,
        }
    }

    /// Reads the routine body at `p` (or a literal holding SQL) as SQL and
    /// redacts what the rules find there; when it mentions a password,
    /// every string literal in it too.
    fn redact_body(&mut self, p: usize) {
        let Some(tok) = self.tok(p).copied() else {
            return;
        };
        let raw = &self.src[tok.content_start..tok.content_end];
        // Every body is read as SQL (its comments get the comment rules,
        // `-- mysql -p...` included); one that mentions a password also has
        // every literal redacted.
        let literals = mentions_account(raw);
        let opener = self.src.as_bytes()[tok.content_start.saturating_sub(1)];
        let script = self.body_language;
        if opener == b'$' {
            // A dollar-quoted body is the source text itself.
            let ranges = match script {
                Some(style) => match self.script_ranges(raw, style) {
                    Some(ranges) => ranges,
                    None => return,
                },
                None => match self.nested_ranges(raw, literals) {
                    Some(ranges) => ranges,
                    None => return,
                },
            };
            let base = tok.content_start;
            self.ranges
                .extend(ranges.into_iter().map(|(a, b)| (base + a, base + b)));
            return;
        }
        let mut body = Vec::with_capacity(raw.len());
        let mut map = SourceMap::default();
        decode_literal(
            raw,
            tok.content_start,
            opener,
            tok.backslash,
            &mut body,
            &mut map,
        );
        let Some(body) = self.decoded_text(body) else {
            return;
        };
        let ranges = match script {
            Some(style) => match self.script_ranges(&body, style) {
                Some(ranges) => ranges,
                None => return,
            },
            None => match self.nested_ranges(&body, literals) {
                Some(ranges) => ranges,
                None => return,
            },
        };
        if self.spend(body.len()) {
            push_mapped(&mut self.ranges, ranges, &map);
        }
    }

    /// The secret ranges of a script body (see [`script_parts`]); the SQL
    /// strings it hands to the database are read with the SQL rules. `None`
    /// when the work budget runs out.
    fn script_ranges(&mut self, body: &str, style: ScriptComments) -> Option<Vec<(usize, usize)>> {
        if !self.spend(body.len() * 8) {
            return None;
        }
        let parts = script_parts(body, style);
        let mut ranges = parts.ranges;
        for (start, end) in parts.sql {
            let inner = self.nested_ranges(&body[start..end], false)?;
            ranges.extend(inner.into_iter().map(|(a, b)| (start + a, start + b)));
        }
        Some(ranges)
    }

    /// The text decoded from literal bodies. Decoding only drops ASCII
    /// escape characters from valid UTF-8, so it stays valid; if it ever
    /// did not, the text is hidden rather than redacted in part.
    fn decoded_text(&mut self, bytes: Vec<u8>) -> Option<String> {
        match String::from_utf8(bytes) {
            Ok(text) => Some(text),
            Err(_) => {
                self.too_deep = true;
                None
            }
        }
    }

    /// Comments are kept as written at text level `full`: the secret
    /// patterns apply inside them too (`-- PASSWORD 'x'`). Comment text is
    /// read as plain text, not as SQL, so an apostrophe (`don't`) cannot
    /// hide what follows.
    fn redact_comments(&mut self) {
        // The comments of one statement, read as one block: a statement
        // commented out over several comments (line by line, or split
        // between a line and a block comment) is still recognized. A `;`
        // ends the block, so a rule never reaches into the comments of a
        // later statement.
        let mut block = String::new();
        let mut map = SourceMap::default();
        for tok in self.toks {
            if tok.kind == Kind::Punct && &self.src[tok.start..tok.end] == ";" {
                self.redact_comment_block(&mut block, &mut map);
                continue;
            }
            if tok.kind != Kind::Comment {
                continue;
            }
            let (start, end) = comment_body(self.src, *tok);
            if !block.is_empty() {
                block.push('\n');
                map.push_gap();
            }
            block.push_str(&self.src[start..end]);
            for at in start..end {
                map.push(at, at + 1);
            }
        }
        self.redact_comment_block(&mut block, &mut map);
    }

    /// Redacts the secrets of one block of comments and empties it. A
    /// range may run over several comments: it is split where the block
    /// joins them.
    fn redact_comment_block(&mut self, block: &mut String, map: &mut SourceMap) {
        if !block.is_empty() && self.spend(block.len() * 8) {
            let ranges = comment_secret_ranges(block);
            push_mapped(&mut self.ranges, ranges, map);
        }
        block.clear();
        map.spans.clear();
    }

    /// A string holding a secret key name followed by its value as the
    /// next argument or compared with it: `jsonb_build_object('api_key',
    /// 'x')`, `JSON_SET(c, '$.password', 'x')`, `cfg->>'api_key' = 'x'`.
    fn redact_value_of_named_key(&mut self, p: usize, json_key: bool) {
        let Some(tok) = self.tok(p).copied() else {
            return;
        };
        let content = &self.src[tok.content_start..tok.content_end];
        let key = content
            .trim()
            .trim_start_matches('$')
            .rsplit(['.', '/'])
            .next()
            .unwrap_or_default()
            .trim_matches(['"', '[', ']']);
        if key.is_empty() || !is_secret_key(key) || descriptive_key(key) {
            return;
        }
        // A key of several words (`'API Key'`, `'client secret'`) ends in
        // the secret word; longer text, or text whose secret word is not
        // last, is prose (`'password reset for %s'`), not a key. In a JSON
        // object call the position says it is a key: any words count
        // (`'Password Hash'`, `'the main db password'`).
        let words: Vec<&str> = key.split_whitespace().collect();
        if !json_key
            && (words.len() > 3
                || (words.len() > 1
                    && !words.last().is_some_and(|last| {
                        matches!(
                            last.to_ascii_lowercase().as_str(),
                            "password"
                                | "passwd"
                                | "passphrase"
                                | "pwd"
                                | "secret"
                                | "token"
                                | "key"
                                | "apikey"
                        )
                    })))
        {
            return;
        }
        if self.is_punct(p + 1, ",") {
            match self.tok(p + 2).map(|t| t.kind) {
                Some(Kind::Str) => self.redact_literal(p + 2),
                Some(Kind::QuotedIdent) if self.flavor == Flavor::Mysql => {
                    self.redact_literal(p + 2);
                }
                _ => {}
            }
        } else if self.is_punct(p + 1, "=") && !self.is_punct(p + 2, "=") {
            self.redact_value(p + 1, false);
        }
    }

    /// `SET [GLOBAL] init_connect = '...'` (also `:=`, a later assignment
    /// of the list, a backtick name, `CONCAT(...)` or an introducer): the
    /// value is SQL, so the secret and credential patterns apply to it. The
    /// literals of the value are read as one text, as the server
    /// concatenates them.
    fn redact_sql_variable(&mut self, set: usize) {
        let mut q = set + 1;
        loop {
            while matches!(
                self.upper_word(q).as_deref(),
                Some("GLOBAL" | "PERSIST" | "PERSIST_ONLY" | "SESSION" | "LOCAL")
            ) || self.is_punct(q, "@")
                || self.is_punct(q, ".")
            {
                q += 1;
            }
            let name = match self.tok(q) {
                Some(tok) if tok.kind == Kind::Word => {
                    self.src[tok.start..tok.end].to_ascii_uppercase()
                }
                Some(tok) if tok.kind == Kind::QuotedIdent => {
                    self.src[tok.content_start..tok.content_end].to_ascii_uppercase()
                }
                _ => return,
            };
            q += 1;
            if self.is_punct(q, ":") && self.is_punct(q + 1, "=") {
                q += 2;
            } else if self.is_punct(q, "=") {
                q += 1;
            } else {
                return;
            }
            // The value: up to the next top-level comma (or the next SET).
            let start = q;
            q = self
                .tables
                .assign_end
                .get(q)
                .map_or(self.sig.len(), |&e| from_u32(e))
                .max(start);
            if SQL_VARIABLES.contains(&name.as_str()) {
                self.redact_sql_value(start, q);
            }
            if !self.is_punct(q, ",") {
                return;
            }
            q += 1;
        }
    }

    /// Redacts the secrets of the SQL that the literals between positions
    /// `start` and `end` spell when concatenated. Mapping the ranges back
    /// is charged to the budget like the rest of the nested text.
    fn redact_sql_value(&mut self, start: usize, end: usize) {
        let mut text = Vec::new();
        let mut map = SourceMap::default();
        for q in start..end {
            let Some(tok) = self.tok(q).copied() else {
                break;
            };
            if !self.spend(1) {
                return;
            }
            if tok.kind != Kind::Str || tok.content_start <= tok.start {
                continue;
            }
            let raw = &self.src[tok.content_start..tok.content_end];
            let quote = self.src.as_bytes()[tok.content_start - 1];
            decode_literal(
                raw,
                tok.content_start,
                quote,
                tok.backslash,
                &mut text,
                &mut map,
            );
        }
        if text.is_empty() {
            return;
        }
        let Some(text) = self.decoded_text(text) else {
            return;
        };
        let Some(mut ranges) = self.nested_ranges(&text, false) else {
            return;
        };
        if !self.spend(text.len() * 8) {
            return;
        }
        ranges.extend(credential_ranges(&text));
        push_mapped(&mut self.ranges, ranges, &map);
    }

    /// True when `||` follows at `p`, after any casts (`::text`,
    /// `::varchar(20)`, `::text[]`, `::character varying`).
    fn concatenated_after(&self, mut p: usize) -> bool {
        while self.is_punct(p, ":") && self.is_punct(p + 1, ":") {
            p += 2;
            // A type name of up to four words (`timestamp with time zone`).
            for _ in 0..4 {
                if !self
                    .tok(p)
                    .is_some_and(|t| matches!(t.kind, Kind::Word | Kind::QuotedIdent))
                {
                    break;
                }
                p += 1;
            }
            while self.is_punct(p, "(") || self.is_punct(p, "[") {
                p = self.close_of(p) + 1;
            }
        }
        self.is_punct(p, "|") && self.is_punct(p + 1, "|")
    }

    /// The name of the function called by the `(` at `p`: the word or
    /// quoted identifier right before it, in upper case.
    fn function_name(&self, p: usize) -> Option<String> {
        let tok = self.tok(p.checked_sub(1)?)?;
        match tok.kind {
            Kind::Word => Some(self.src[tok.start..tok.end].to_ascii_uppercase()),
            Kind::QuotedIdent => {
                Some(self.src[tok.content_start..tok.content_end].to_ascii_uppercase())
            }
            _ => None,
        }
    }

    /// The single forward pass over the statement's tokens. Groups are
    /// tracked on a stack of frames, so a call's key argument, a `CONCAT`
    /// or `format` that builds an account statement, a `||` chain after
    /// one, and `OPTIONS (...)` are recognized without scanning ahead:
    /// every token is visited once here.
    fn collect(&mut self) {
        self.redact_comments();
        // `IDENTIFIED` was seen in this statement, or the statement creates
        // or alters a role: `BY`, `AS`, `USING` and `REPLACE` introduce a
        // password.
        let mut identified = false;
        // The statement creates or alters a role: a bare word after
        // `PASSWORD` is the password. Holds the position of the role name,
        // which is never a keyword even when it reads `password`.
        let mut role_name: Option<usize> = None;
        let mut routine = false;
        // `COPY ... WITH PASSWORD x`: a bare word after PASSWORD is the
        // password here too.
        let mut copy = false;
        let mut statement_start = true;
        let mut frames: Vec<Frame> = vec![Frame::new(FrameKind::Plain, &[], false, 0)];
        // The literals of the open `CONCAT` calls, in order.
        let mut concat_literals: Vec<usize> = Vec::new();
        let mut concat_open = 0usize;
        // How many `format(` calls are open.
        let mut format_open = 0usize;
        for p in 0..self.sig.len() {
            if !self.spend(1) {
                return;
            }
            if statement_start {
                statement_start = false;
                role_name = self.role_name(p);
                identified = role_name.is_some();
                routine = self.routine_statement(p);
                self.body_language = if routine {
                    self.routine_language(p)
                } else {
                    None
                };
                copy = self.upper_word(p).as_deref() == Some("COPY");
                if copy {
                    self.redact_copy_program(p);
                }
                self.redact_insert_values(p);
                if let Some((kind, body)) = self.account_statement(p) {
                    self.walk_account(p, kind, body);
                }
            }
            let Some(tok) = self.tok(p).copied() else {
                continue;
            };
            let text = &self.src[tok.start..tok.end];
            if tok.kind == Kind::Punct {
                match text {
                    "(" | "[" => {
                        let name = self.function_name(p);
                        let name = name.as_deref().unwrap_or_default();
                        let parent_secret = frames.last().is_some_and(Frame::secret_literal);
                        let marked = self.group_marks.remove(&p);
                        let kind = match name {
                            "CONCAT" | "CONCAT_WS" => FrameKind::Concat,
                            // `replace('ALTER ROLE r PASSWORD $p', '$p', 'x')`
                            // fills a template like `format`.
                            "FORMAT" | "REPLACE" | "REGEXP_REPLACE" => FrameKind::Format,
                            "OPTIONS" => FrameKind::Options,
                            "JSON_OBJECT" | "JSONB_BUILD_OBJECT" | "JSON_BUILD_OBJECT"
                            | "JSON_OBJECT_AGG" | "JSONB_OBJECT_AGG" => FrameKind::Json,
                            _ => FrameKind::Plain,
                        };
                        let arguments = self.tables.arguments.get(p).map_or(0, |&a| from_u32(a));
                        let targets = key_arguments(name, self.flavor, arguments);
                        // A setting read with `current_setting('app.key')`
                        // is a name, even inside a key argument.
                        let inherited = (parent_secret || marked) && name != "CURRENT_SETTING";
                        if kind == FrameKind::Concat {
                            concat_open += 1;
                        }
                        if kind == FrameKind::Format {
                            format_open += 1;
                        }
                        frames.push(Frame::new(kind, targets, inherited, concat_literals.len()));
                        continue;
                    }
                    ")" | "]" => {
                        if frames.len() > 1
                            && let Some(frame) = frames.pop()
                        {
                            if frame.kind == FrameKind::Format {
                                format_open = format_open.saturating_sub(1);
                            }
                            if frame.kind == FrameKind::Concat {
                                concat_open = concat_open.saturating_sub(1);
                                if frame.mentions {
                                    let done = concat_literals.split_off(frame.literals_from);
                                    for literal in done {
                                        self.redact_literal(literal);
                                    }
                                }
                            }
                            // The literals of an inner call are literals of
                            // the outer call too.
                            if frame.mentions
                                && let Some(parent) = frames.last_mut()
                            {
                                parent.mentions = true;
                                // `format(concat('ALTER ROLE %I ', ...), ...)`:
                                // the format string is built by the call.
                                if parent.kind == FrameKind::Format {
                                    parent.format_account = true;
                                }
                            }
                            if concat_open == 0 {
                                concat_literals.clear();
                            }
                        }
                        continue;
                    }
                    "," => {
                        if let Some(top) = frames.last_mut() {
                            top.argument += 1;
                            top.chain = false;
                            top.operands.clear();
                        }
                    }
                    ";" => {
                        frames.truncate(1);
                        frames[0] = Frame::new(FrameKind::Plain, &[], false, 0);
                        concat_literals.clear();
                        concat_open = 0;
                        format_open = 0;
                        statement_start = true;
                        continue;
                    }
                    _ => {}
                }
            }
            if tok.kind == Kind::Str {
                let content = &self.src[tok.content_start..tok.content_end];
                // `'...' || x`, also after a cast (`'...'::text || x`).
                let concatenated = self.concatenated_after(p + 1);
                let continues = p >= 2 && self.is_punct(p - 1, "|") && self.is_punct(p - 2, "|");
                // Only a literal that is concatenated, or inside a
                // `CONCAT` or `format` call, is read for a credential
                // statement (a routine body is read below).
                let spliced = concat_open > 0 || format_open > 0;
                let builds = (concatenated || continues || spliced)
                    && self.spend(content.len())
                    && builds_credential(content);
                let mut earlier: Vec<usize> = Vec::new();
                let secret = if let Some(top) = frames.last_mut() {
                    if !continues {
                        // A new expression: the chain before it is over.
                        top.chain = false;
                        top.operands.clear();
                    } else if builds && !top.chain {
                        // `'secret' || ' ... ALTER ROLE r PASSWORD'`: the
                        // literals already in the chain are part of the
                        // credential statement too.
                        earlier = std::mem::take(&mut top.operands);
                    }
                    let secret = top.secret_literal();
                    if builds {
                        top.mentions = true;
                        // The format string, or an argument it splices in
                        // (`format('%s %L', 'ALTER ROLE r PASSWORD', 'x')`):
                        // the arguments after it build the statement.
                        if top.kind == FrameKind::Format {
                            top.format_account = true;
                        }
                    }
                    if builds && concatenated {
                        top.chain = true;
                    }
                    if concatenated {
                        top.operands.push(p);
                    } else {
                        top.operands.clear();
                    }
                    secret
                } else {
                    false
                };
                for literal in earlier {
                    self.redact_literal(literal);
                }
                if secret {
                    self.redact_literal(p);
                }
                if concat_open > 0 {
                    concat_literals.push(p);
                }
                // In `jsonb_build_object(k, v, ...)` every even argument
                // is a key.
                let json_key = frames
                    .last()
                    .is_some_and(|f| f.kind == FrameKind::Json && f.argument % 2 == 0);
                self.redact_value_of_named_key(p, json_key);
                // Routine bodies, and any literal that holds an account
                // statement (`PREPARE s FROM 'ALTER USER ...'`).
                if routine
                    || self.src.as_bytes().get(tok.start) == Some(&b'$')
                    || holds_account_statement(content)
                {
                    self.redact_body(p);
                }
            }
            if tok.kind == Kind::Number && frames.last().is_some_and(Frame::secret_literal) {
                self.ranges.push((tok.start, tok.end));
            }
            // `OPTIONS ([ADD | SET | DROP] name 'value', ...)`: a value whose
            // option name looks like a secret, misspellings included.
            if frames.last().is_some_and(|f| f.kind == FrameKind::Options)
                && matches!(tok.kind, Kind::Word | Kind::QuotedIdent)
                && !self.is_punct(p + 1, ",")
            {
                let name = match tok.kind {
                    Kind::Word => text,
                    _ => &self.src[tok.content_start..tok.content_end],
                };
                if secret_word(name) {
                    self.redact_value(p + 1, true);
                }
            }
            // `pw = 'x'`, `"api_token" = 'x'`: a secret column assigned or
            // compared.
            // A time or count about a secret (`password_reset_at = ...`)
            // keeps a value that reads as one (a date, a number, `now()`).
            let metadata_column = self.metadata_column(p)
                && !self.metadata_value(p + 2, self.text(p).is_some_and(policy_name));
            if (self.secret_column(p) || metadata_column)
                && self.is_punct(p + 1, "=")
                && !self.is_punct(p + 2, "=")
                && role_name != Some(p)
                && !self.policy_value(&self.name_before(p + 1), p + 1)
            {
                self.redact_value(p + 1, false);
            }
            let Some(word) = self.upper_word(p) else {
                continue;
            };
            if role_name == Some(p) {
                continue;
            }
            match word.as_str() {
                "SET" => {
                    self.redact_secret_setting(p);
                    self.redact_row_assignment(p);
                    self.redact_sql_variable(p);
                }
                "SET_CONFIG" => self.redact_set_config(p),
                // Statements inside a procedure or block body.
                "BEGIN" | "THEN" | "ELSE" | "LOOP" | "REPEAT" if frames.len() == 1 => {
                    statement_start = true;
                }
                _ => {}
            }
            // A time, count or policy about a password
            // (`password_changed_at`) is no password.
            if (word.ends_with("PASSWORD")
                || word.ends_with("PASSWD")
                || word.split(['_', '.']).any(like_password))
                && !(metadata_key(&word) && self.metadata_value_after(p + 1, policy_name(&word)))
                && !flag_key(&key_parts(&word))
            {
                let mut q = p + 1;
                if self.upper_word(q).as_deref() == Some("FOR") {
                    // `SET PASSWORD FOR 'user'@'host' = '...'`
                    q = self
                        .tables
                        .next_eq
                        .get(q)
                        .map_or(self.sig.len(), |&e| from_u32(e));
                }
                let bare_word = (role_name.is_some() || copy) && word == "PASSWORD";
                if !self.policy_value(&word, q) {
                    self.redact_value(q, bare_word);
                }
            } else if within(&word, "IDENTIFIED", 2) {
                identified = true;
            } else if identified && matches!(word.as_str(), "BY" | "AS" | "USING" | "REPLACE") {
                let mut q = p + 1;
                if self.upper_word(q).as_deref() == Some("PASSWORD") {
                    q += 1;
                }
                self.redact_value_after_identified(q);
            }
        }
    }
}

/// The ranges the SQL secret rules find in `src` (password clauses of
/// account statements, secret columns, key arguments, comments), without
/// the credential patterns of the later stages; `None` when the text
/// needs more work than its budget or nests too deep.
pub(crate) fn statement_ranges(src: &str, flavor: Flavor) -> Option<Vec<(usize, usize)>> {
    let budget = Budget::for_text(src.len());
    let modes: &[bool] = match flavor {
        Flavor::Postgres => &[true],
        Flavor::Mysql => &[true, false],
    };
    let mut ranges = Vec::new();
    for &backslash in modes {
        let toks = lex_mode(src, flavor, backslash);
        let mut redactions = Redactions::new(src, &toks, flavor);
        redactions.budget = budget.clone();
        redactions.backslash = backslash;
        redactions.collect();
        if redactions.too_deep {
            return None;
        }
        ranges.extend(redactions.ranges);
    }
    Some(balance_ranges(src, &lex_mode(src, flavor, true), ranges))
}

/// Keeps the stored text's quoting as the primary reading of `src` (`toks`,
/// backslash escapes on) has it: a range never removes the quotes that
/// open or close a literal or quoted identifier, and never splits an
/// escape (`\'`, `''`) in two. Ranges from the other backslash mode, or
/// from SQL nested in a literal, can otherwise leave `'[redacted]` with
/// its closing quote gone. Linear in the text and the ranges.
fn balance_ranges(src: &str, toks: &[Tok], ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let bytes = src.as_bytes();
    // `escaped[i]`: byte `i` is the second byte of an escape in a literal.
    let mut escaped = vec![false; bytes.len() + 1];
    let mut markers: Vec<(usize, usize)> = Vec::new();
    for tok in toks {
        if !matches!(tok.kind, Kind::Str | Kind::QuotedIdent) || tok.content_start <= tok.start {
            continue;
        }
        let quote = bytes[tok.content_start - 1];
        let mut i = tok.content_start;
        while i < tok.content_end {
            if (tok.backslash && bytes[i] == b'\\')
                || (bytes[i] == quote && bytes.get(i + 1) == Some(&quote))
            {
                escaped[(i + 1).min(bytes.len())] = true;
                i += 2;
            } else {
                i += 1;
            }
        }
        markers.push((tok.start, tok.content_start));
        if tok.end > tok.content_end {
            markers.push((tok.content_end, tok.end));
        }
    }
    let mut out = Vec::new();
    let mut m = 0;
    for (start, end) in merge_ranges(ranges) {
        let start = if escaped[start] { start - 1 } else { start };
        let end = if end < bytes.len() && escaped[end] {
            end + 1
        } else {
            end
        };
        // Cut the quote markers out of the range.
        while m < markers.len() && markers[m].1 <= start {
            m += 1;
        }
        let mut from = start;
        let mut k = m;
        while k < markers.len() && markers[k].0 < end {
            let (a, b) = markers[k];
            if a > from {
                out.push((from, a));
            }
            from = from.max(b);
            k += 1;
        }
        if from < end {
            out.push((from, end));
        }
    }
    out
}

/// Removes passwords from a SQL script and appends every removed fragment
/// to `removed`. MySQL text is read in both backslash modes (with and
/// without `NO_BACKSLASH_ESCAPES`), so a literal ending in `\` cannot hide a
/// following password. `None` when SQL held in strings nests deeper than
/// [`MAX_NESTING`]: the caller hides the whole text.
pub(crate) fn redact(src: &str, flavor: Flavor, removed: &mut Vec<String>) -> Option<String> {
    // One budget for all passes and both modes: a text that needs more
    // work than its length allows is hidden, never redacted in part.
    redact_budgeted(src, flavor, removed, &Budget::for_text(src.len()))
}

fn redact_budgeted(
    src: &str,
    flavor: Flavor,
    removed: &mut Vec<String>,
    budget: &Budget,
) -> Option<String> {
    let modes: &[bool] = match flavor {
        Flavor::Postgres => &[true],
        Flavor::Mysql => &[true, false],
    };
    // 1. Password clauses of account statements, and secrets in comments.
    let mut ranges = Vec::new();
    for &backslash in modes {
        if !budget.spend(src.len()) {
            return None;
        }
        let toks = lex_mode(src, flavor, backslash);
        let mut redactions = Redactions::new(src, &toks, flavor);
        redactions.budget = budget.clone();
        redactions.backslash = backslash;
        redactions.collect();
        if redactions.too_deep {
            return None;
        }
        ranges.extend(redactions.ranges);
    }
    if !budget.spend(src.len() * 2) {
        return None;
    }
    let ranges = balance_ranges(src, &lex_mode(src, flavor, true), ranges);
    let stage1 = splice(src, ranges, removed).into_owned();

    // 2. Connection strings inside literals, quoted identifiers and comments.
    let mut ranges = Vec::new();
    for &backslash in modes {
        for tok in lex_mode(&stage1, flavor, backslash) {
            if !matches!(tok.kind, Kind::Str | Kind::QuotedIdent | Kind::Comment) {
                continue;
            }
            let content = &stage1[tok.content_start..tok.content_end];
            if !budget.spend(content.len() * 8) {
                return None;
            }
            ranges.extend(
                credential_ranges(content)
                    .into_iter()
                    .map(|(start, end)| (tok.content_start + start, tok.content_start + end)),
            );
        }
    }
    let stage2 = splice(&stage1, ranges, removed).into_owned();

    // 3. A connection URL pasted as code.
    let ranges = url_ranges(&stage2, UrlScope::Code);
    Some(splice(&stage2, ranges, removed).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str, flavor: Flavor) -> Vec<(Kind, String)> {
        lex(src, flavor)
            .into_iter()
            .filter(|t| t.kind != Kind::Space)
            .map(|t| (t.kind, src[t.start..t.end].to_string()))
            .collect()
    }

    #[test]
    fn lexes_postgres_literals() {
        let toks = kinds(
            "SELECT E'a\\'b', 'it''s', $$x$$, $tag$ y $tag$, $1, B'101', X'ff', U&'d', \"Col\"\"x\" -- c\n",
            Flavor::Postgres,
        );
        let strs: Vec<&str> = toks
            .iter()
            .filter(|(k, _)| *k == Kind::Str)
            .map(|(_, s)| s.as_str())
            .collect();
        assert_eq!(
            strs,
            [
                "E'a\\'b'",
                "'it''s'",
                "$$x$$",
                "$tag$ y $tag$",
                "B'101'",
                "X'ff'",
                "U&'d'"
            ]
        );
        assert!(toks.contains(&(Kind::Word, "$1".into())));
        assert!(toks.contains(&(Kind::QuotedIdent, "\"Col\"\"x\"".into())));
        assert!(toks.contains(&(Kind::Comment, "-- c".into())));
    }

    #[test]
    fn lexes_mysql_literals() {
        let toks = kinds(
            "SELECT 'a\\'b', \"q\\\"d\", `id``x`, x'ff', 1col, 0x1F, # c\n /*!50000 a */ --x",
            Flavor::Mysql,
        );
        assert!(toks.contains(&(Kind::Str, "'a\\'b'".into())));
        assert!(toks.contains(&(Kind::Str, "\"q\\\"d\"".into())));
        assert!(toks.contains(&(Kind::QuotedIdent, "`id``x`".into())));
        assert!(toks.contains(&(Kind::Str, "x'ff'".into())));
        assert!(toks.contains(&(Kind::Word, "1col".into())));
        assert!(toks.contains(&(Kind::Number, "0x1F".into())));
        assert!(toks.contains(&(Kind::Comment, "# c".into())));
        assert!(toks.contains(&(Kind::ExecMark, "/*!50000".into())));
        assert!(toks.contains(&(Kind::ExecMark, "*/".into())));
        // `--x` is not a comment in MySQL.
        assert!(!toks.iter().any(|(_, s)| s == "--x"));
    }

    #[test]
    fn postgres_block_comments_nest() {
        let toks = kinds("a /* x /* y */ z */ b", Flavor::Postgres);
        assert_eq!(
            toks,
            [
                (Kind::Word, "a".into()),
                (Kind::Comment, "/* x /* y */ z */".into()),
                (Kind::Word, "b".into())
            ]
        );
    }

    #[test]
    fn unterminated_literals_run_to_the_end() {
        let toks = kinds("SELECT 'abc", Flavor::Postgres);
        assert_eq!(toks.last(), Some(&(Kind::Str, "'abc".into())));
        let toks = kinds("SELECT $q$abc", Flavor::Postgres);
        assert_eq!(toks.last(), Some(&(Kind::Str, "$q$abc".into())));
    }

    #[test]
    fn a_text_over_its_work_budget_is_hidden_whole() {
        let text = "ALTER ROLE r PASSWORD 'Zq9wSecret'; SELECT 1";
        let tiny = Budget(Rc::new(Cell::new(10)));
        assert_eq!(
            redact_budgeted(text, Flavor::Postgres, &mut Vec::new(), &tiny),
            None
        );
        let ample = Budget::for_text(text.len());
        assert_eq!(
            redact_budgeted(text, Flavor::Postgres, &mut Vec::new(), &ample).as_deref(),
            Some("ALTER ROLE r PASSWORD '[redacted]'; SELECT 1")
        );
    }

    #[test]
    fn escapes_in_a_nested_value_are_redacted_with_the_secret() {
        // The decoded `S` came from `\S`: the escaped character itself must
        // be covered, not only its backslash.
        let text = r"SET GLOBAL init_connect = 'ALTER USER u IDENTIFIED BY ''Zq9w\Secret'''";
        assert_eq!(
            redact(text, Flavor::Mysql, &mut Vec::new()).as_deref(),
            Some("SET GLOBAL init_connect = 'ALTER USER u IDENTIFIED BY ''[redacted]'''")
        );
    }

    #[test]
    fn overlapping_nested_ranges_are_mapped_once() {
        // Every word of the glued chunk redacts the whole chunk: the
        // overlapping ranges are merged before they are mapped back.
        let chunk = "x.".repeat(20_000);
        let text = format!("SET GLOBAL init_connect = 'CREATE USER u {chunk}'");
        let start = std::time::Instant::now();
        let out = redact(&text, Flavor::Mysql, &mut Vec::new());
        let took = start.elapsed();
        assert_eq!(
            out.as_deref(),
            Some("SET GLOBAL init_connect = 'CREATE USER u [redacted]'")
        );
        let bound = if cfg!(debug_assertions) { 5_000 } else { 500 };
        assert!(took.as_millis() < bound, "took {took:?}");
    }

    #[test]
    fn merged_ranges_are_sorted_and_disjoint() {
        assert_eq!(
            merge_ranges(vec![(5, 9), (0, 2), (1, 3), (3, 4), (7, 8), (6, 6)]),
            vec![(0, 4), (5, 9)]
        );
    }

    #[test]
    fn lexer_handles_multibyte_text() {
        let src = "SELECT 'héllo', naïve, \"ü\" FROM t -- ünïcode";
        assert_eq!(
            super::super::sql_normalize::normalize(src, Flavor::Postgres),
            "SELECT ?, naïve, \"ü\" FROM t"
        );
    }
}
