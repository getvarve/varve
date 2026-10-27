//! Statement splitting and classification for SQL engines.
//!
//! Used for the destructive-statement confirmation on production connections
//! (design doc 3.2), for the statement kind in activity events (9.2), and by
//! drivers that run scripts one statement at a time.
//!
//! The lexer follows the rules of each [`Dialect`], so keywords inside
//! literals, quoted identifiers and comments never count:
//!
//! | | PostgreSQL | MySQL, MariaDB |
//! |---|---|---|
//! | `--` and `/* */` comments | yes (`/* */` nests) | yes |
//! | `#` comments | no, `#` is an operator | yes |
//! | backslash escapes in `'...'` | only in `E'...'` | yes |
//! | `$tag$ ... $tag$` quoting | yes | no |
//! | `` `identifier` `` | no | yes |

use serde::{Deserialize, Serialize};

/// Lexical rules of a SQL engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    /// PostgreSQL.
    Postgres,
    /// MySQL and MariaDB.
    Mysql,
}

/// What a statement does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    /// `SELECT`, `VALUES`, `TABLE`, or a `WITH` that ends in a select.
    Select,
    /// `INSERT`, `REPLACE`.
    Insert,
    /// `UPDATE` with a `WHERE` clause.
    Update,
    /// `UPDATE` without a `WHERE` clause.
    UpdateAll,
    /// `DELETE` with a `WHERE` clause.
    Delete,
    /// `DELETE` without a `WHERE` clause.
    DeleteAll,
    /// `MERGE`.
    Merge,
    /// `CREATE`, `ALTER` (without `DROP`), `COMMENT`, `RENAME`.
    Ddl,
    /// `ALTER ... DROP ...`.
    AlterDrop,
    /// `DROP`.
    Drop,
    /// `TRUNCATE`.
    Truncate,
    /// `GRANT`, `REVOKE`.
    Grant,
    /// `BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`.
    Transaction,
    /// `EXPLAIN`, `SHOW`, `DESCRIBE`.
    Explain,
    /// `SET`, `USE`, `RESET`.
    Session,
    /// `CALL`, `DO`, `EXECUTE`.
    Call,
    /// `COPY`, `LOAD DATA`.
    Copy,
    /// `VACUUM`, `ANALYZE`, `OPTIMIZE`, `REINDEX`, `CLUSTER`, `REFRESH`.
    Maintenance,
    /// Anything else.
    Other,
}

impl StatementKind {
    /// True when the statement can remove data or objects without an
    /// explicit filter (design doc 3.2).
    #[must_use]
    pub fn is_destructive(self) -> bool {
        matches!(
            self,
            StatementKind::Drop
                | StatementKind::Truncate
                | StatementKind::DeleteAll
                | StatementKind::UpdateAll
                | StatementKind::AlterDrop
        )
    }

    /// True when the statement changes data or schema.
    #[must_use]
    pub fn writes(self) -> bool {
        !matches!(
            self,
            StatementKind::Select
                | StatementKind::Explain
                | StatementKind::Session
                | StatementKind::Transaction
        )
    }
}

/// One statement of a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementSpan<'a> {
    /// Statement text, trimmed, without the terminating `;`.
    pub text: &'a str,
    /// Byte offset of `text` in the script, for mapping server error
    /// positions back to the editor.
    pub offset: usize,
    /// What the statement does.
    pub kind: StatementKind,
}

/// Splits a script into statements. Statements that contain only
/// whitespace and comments are dropped.
#[must_use]
pub fn split_statements(sql: &str, dialect: Dialect) -> Vec<StatementSpan<'_>> {
    spans(sql, scan(sql, dialect))
}

/// The statement boundaries of a MySQL script cannot be known from the text
/// alone: they depend on the session's `sql_mode` (`NO_BACKSLASH_ESCAPES`,
/// `ANSI_QUOTES`), or the script holds a construct the splitter does not
/// model with certainty (a MariaDB package, an unterminated or mismatched
/// compound statement, `sql_mode = ORACLE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the statement boundaries of this script cannot be known with certainty")]
pub struct AmbiguousSplit;

/// The server a MySQL or MariaDB script is read for. Executable comments
/// (`/*!NNNNN ... */`, `/*M!NNNNN ... */`) are code or comments depending
/// on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MysqlServer {
    /// MariaDB (runs `/*M! */` comments) rather than MySQL.
    pub mariadb: bool,
    /// Version as the comments write it: `major * 10000 + minor * 100 +
    /// patch` (80036 for 8.0.36, 100611 for 10.6.11).
    pub version: u32,
}

impl MysqlServer {
    /// The server of `major.minor.patch`.
    #[must_use]
    pub fn new(mariadb: bool, major: u32, minor: u32, patch: u32) -> Self {
        Self {
            mariadb,
            version: major
                .saturating_mul(10_000)
                .saturating_add(minor.saturating_mul(100))
                .saturating_add(patch),
        }
    }

    /// How this server reads an executable comment opened with `/*!`
    /// (`mariadb_only` false) or `/*M!`, followed by `digits` (all the
    /// digits right after the marker): how many of them are the version,
    /// and whether the comment's contents run. Measured on MySQL 8.0, 8.4,
    /// 9.7 and MariaDB 10.6, 12.3:
    ///
    /// - MySQL reads 5 digits (fewer: no version, the digits are code); from
    ///   8.4 on it reads exactly 6 when 6 follow, and 5 when 7 or more do.
    ///   `/*M!` is an ordinary comment.
    /// - MariaDB reads up to 6 digits. A `/*!` version from 50700 to 99999
    ///   is MySQL's and never runs; `/*M!` has no such range.
    fn executable_comment(self, mariadb_only: bool, digits: &str) -> (usize, bool) {
        let number = |len: usize| digits.get(..len).and_then(|d| d.parse::<u32>().ok());
        if self.mariadb {
            let len = digits.len().min(6);
            return match number(len).filter(|_| len > 0) {
                None => (0, true),
                Some(version) => {
                    let mysql_only = !mariadb_only && (50_700..=99_999).contains(&version);
                    (len, !mysql_only && version <= self.version)
                }
            };
        }
        if mariadb_only {
            return (0, false);
        }
        let len = match digits.len() {
            6 if self.version >= 80_400 => 6,
            n if n >= 5 => 5,
            _ => 0,
        };
        match number(len).filter(|_| len > 0) {
            None => (0, true),
            Some(version) => (len, version <= self.version),
        }
    }
}

/// How a MySQL or MariaDB session reads SQL text, from its `sql_mode` and
/// server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MysqlMode {
    /// Backslashes escape quotes in strings (no `NO_BACKSLASH_ESCAPES`).
    pub backslash_escapes: bool,
    /// `"..."` quotes an identifier, in which a backslash is an ordinary
    /// character (`ANSI_QUOTES`).
    pub ansi_quotes: bool,
    /// MariaDB `sql_mode=ORACLE`: PL/SQL syntax the splitter does not
    /// model.
    pub oracle: bool,
    /// The server, for executable comments. `None` reads every executable
    /// comment as code.
    pub server: Option<MysqlServer>,
}

impl Default for MysqlMode {
    fn default() -> Self {
        Self {
            backslash_escapes: true,
            ansi_quotes: false,
            oracle: false,
            server: None,
        }
    }
}

impl MysqlMode {
    /// The mode of a session whose `@@sql_mode` is `sql_mode` (the
    /// comma separated list the server reports).
    #[must_use]
    pub fn from_sql_mode(sql_mode: &str, server: Option<MysqlServer>) -> Self {
        let has = |name: &str| {
            sql_mode
                .split(',')
                .any(|m| m.trim().eq_ignore_ascii_case(name))
        };
        Self {
            backslash_escapes: !has("NO_BACKSLASH_ESCAPES"),
            ansi_quotes: has("ANSI_QUOTES") || has("ANSI"),
            oracle: has("ORACLE"),
            server,
        }
    }
}

/// Splits a MySQL or MariaDB script exactly as a session in `mode` does:
/// strings and quoted names follow the `sql_mode`, executable comments the
/// server version, and compound statements written without `DELIMITER`
/// (routine bodies, `IF`, `CASE`, loops, labels, MariaDB `BEGIN NOT
/// ATOMIC` and top-level compound statements) stay one statement.
///
/// # Errors
///
/// [`AmbiguousSplit`] when the boundaries are not certain: a construct the
/// splitter does not model (MariaDB packages, `sql_mode=ORACLE`), or a
/// compound statement that is not closed or closed with the wrong `END`.
pub fn split_statements_mysql<'a>(
    sql: &'a str,
    mode: &MysqlMode,
) -> Result<Vec<StatementSpan<'a>>, AmbiguousSplit> {
    if mode.oracle {
        return Err(AmbiguousSplit);
    }
    let (scanned, uncertain) = scan_with(sql, Dialect::Mysql, mode);
    if uncertain {
        return Err(AmbiguousSplit);
    }
    Ok(spans(sql, scanned))
}

/// True when a MySQL or MariaDB statement defines a stored body: `CREATE
/// [OR REPLACE] [DEFINER = ...] [AGGREGATE] PROCEDURE|FUNCTION|TRIGGER|EVENT`
/// with a body, or `ALTER EVENT ... DO`. The words are read in header
/// position only, so a table named `event` or `procedure` is no definition,
/// nor is a loadable function (`CREATE FUNCTION f RETURNS STRING SONAME`).
#[must_use]
pub fn mysql_defines_body(statement: &str, mode: &MysqlMode) -> bool {
    let mut items = Vec::new();
    let mut after_dot = false;
    for lexeme in lex_mode(statement, Dialect::Mysql, mode) {
        let text = &statement[lexeme.start..lexeme.end];
        let item = match lexeme.kind {
            LexKind::Comment | LexKind::ExecutableCommentStart | LexKind::ExecutableCommentEnd => {
                continue;
            }
            // A name after `.` is never a keyword.
            LexKind::Word if after_dot => Item::Word(String::new()),
            LexKind::Word => Item::Word(text.to_ascii_uppercase()),
            LexKind::QuotedIdent | LexKind::DoubleQuoted => Item::Quoted,
            LexKind::String | LexKind::DollarString | LexKind::Number | LexKind::Parameter => {
                Item::Literal
            }
            LexKind::Punct(c) => Item::Punct(c),
        };
        after_dot = item == Item::Punct('.');
        items.push(item);
    }
    matches!(header_end(&items), HeaderEnd::BodyAt(_))
}

fn spans(sql: &str, scanned: Vec<Scanned>) -> Vec<StatementSpan<'_>> {
    scanned
        .into_iter()
        .filter(|s| !s.tokens.is_empty())
        .map(|s| {
            let raw = &sql[s.start..s.end];
            let leading = raw.len() - raw.trim_start().len();
            StatementSpan {
                text: raw.trim(),
                offset: s.start + leading,
                kind: classify_tokens(&s.tokens),
            }
        })
        .collect()
}

/// Like [`split_statements`], but refuses MySQL text whose statement
/// boundaries differ between `sql_mode`s (with and without
/// `NO_BACKSLASH_ESCAPES` and `ANSI_QUOTES`: a backslash before a quote in
/// a string) or are otherwise uncertain (see [`split_statements_mysql`]).
/// Use it wherever the number of statements is a safety decision (EXPLAIN,
/// read-only guards).
///
/// # Errors
///
/// [`AmbiguousSplit`] for such MySQL text.
pub fn split_statements_strict(
    sql: &str,
    dialect: Dialect,
) -> Result<Vec<StatementSpan<'_>>, AmbiguousSplit> {
    if dialect == Dialect::Mysql {
        let bounds = |backslash_escapes: bool, ansi_quotes: bool| {
            let mode = MysqlMode {
                backslash_escapes,
                ansi_quotes,
                ..MysqlMode::default()
            };
            let (scanned, uncertain) = scan_with(sql, dialect, &mode);
            let bounds: Vec<(usize, usize)> = scanned
                .into_iter()
                .filter(|s| !s.tokens.is_empty())
                .map(|s| (s.start, s.end))
                .collect();
            (bounds, uncertain)
        };
        let reference = bounds(true, false);
        if reference.1 {
            return Err(AmbiguousSplit);
        }
        for (backslash, ansi) in [(false, false), (true, true), (false, true)] {
            if bounds(backslash, ansi) != reference {
                return Err(AmbiguousSplit);
            }
        }
    }
    Ok(split_statements(sql, dialect))
}

/// Classifies every statement in `sql`, in order.
#[must_use]
pub fn classify(sql: &str, dialect: Dialect) -> Vec<StatementKind> {
    split_statements(sql, dialect)
        .into_iter()
        .map(|s| s.kind)
        .collect()
}

/// True when any statement in `sql` is destructive.
#[must_use]
pub fn is_destructive(sql: &str, dialect: Dialect) -> bool {
    classify(sql, dialect)
        .into_iter()
        .any(StatementKind::is_destructive)
}

/// Returns the first statement that would turn off a read-only session
/// (design doc 3.2), or `None`.
///
/// Drivers set read-only mode when the session opens; these statements
/// would switch it off again: `SET`/`RESET` of `default_transaction_read_only`,
/// `transaction_read_only` or `tx_read_only` (any value, since a read-only
/// session has no reason to touch them), `RESET ALL`, `DISCARD ALL`, any `READ WRITE`
/// transaction mode (`BEGIN`, `START TRANSACTION`, `SET [SESSION] TRANSACTION`,
/// `SET SESSION CHARACTERISTICS`), and PostgreSQL `set_config(...)`, whose
/// arguments are literals the lexer does not look into.
///
/// A PostgreSQL `DO` block runs as one statement, so a check after each
/// statement comes too late for a block that switches the mode off and then
/// commits inside itself. Its body (string literals included, since
/// `EXECUTE` takes its SQL as a string) is therefore searched word by word
/// for `COMMIT`, `ROLLBACK`, `set_config`, the mode variables and
/// `READ WRITE`. Procedures that already exist on the server and do the same
/// cannot be seen from the statement text; only a read-only database role
/// prevents those.
#[must_use]
pub fn read_only_violation(sql: &str, dialect: Dialect) -> Option<&str> {
    const MODE_VARIABLES: [&str; 3] = [
        "DEFAULT_TRANSACTION_READ_ONLY",
        "TRANSACTION_READ_ONLY",
        "TX_READ_ONLY",
    ];
    for statement in scan(sql, dialect) {
        let words: Vec<&str> = statement.tokens.iter().map(|t| t.word.as_str()).collect();
        let Some(first) = words.first() else { continue };
        let sets_mode = matches!(*first, "SET" | "RESET")
            && (words.iter().any(|w| MODE_VARIABLES.contains(w))
                || statement
                    .quoted
                    .iter()
                    .any(|q| MODE_VARIABLES.iter().any(|v| q.contains(v))));
        let reset_all = matches!(*first, "RESET" | "DISCARD") && words.get(1) == Some(&"ALL");
        let read_write = statement
            .tokens
            .windows(2)
            .any(|pair| pair[0].word == "READ" && pair[1].word == "WRITE" && !pair[1].detached);
        let text = &sql[statement.start..statement.end];
        let set_config = dialect == Dialect::Postgres
            && words.contains(&"SET_CONFIG")
            && set_config_may_switch_mode(text);
        let unsafe_block =
            dialect == Dialect::Postgres && *first == "DO" && block_can_switch_mode(text);
        if sets_mode || reset_all || read_write || set_config || unsafe_block {
            return Some(sql[statement.start..statement.end].trim());
        }
    }
    None
}

/// True unless every `set_config(...)` call in a PostgreSQL statement names
/// its setting with a plain string literal that is not one of the read-only
/// mode variables (`set_config('search_path', ...)` is harmless). A name
/// built at run time (an expression, a parameter, an `E''` string) cannot be
/// checked and counts as a switch.
fn set_config_may_switch_mode(text: &str) -> bool {
    let lexemes: Vec<Lexeme> = lex(text, Dialect::Postgres, true)
        .into_iter()
        .filter(|l| l.kind != LexKind::Comment)
        .collect();
    for (at, lexeme) in lexemes.iter().enumerate() {
        let is_call = lexeme.kind == LexKind::Word
            && text[lexeme.start..lexeme.end].eq_ignore_ascii_case("set_config");
        if !is_call {
            continue;
        }
        let open = lexemes.get(at + 1).map(|l| l.kind);
        let name = lexemes.get(at + 2);
        let after = lexemes.get(at + 3).map(|l| l.kind);
        let literal = match (open, name, after) {
            (Some(LexKind::Punct('(')), Some(name), Some(LexKind::Punct(',')))
                if name.kind == LexKind::String =>
            {
                &text[name.start..name.end]
            }
            _ => return true,
        };
        let setting = literal
            .trim_matches('\'')
            .replace("''", "'")
            .trim()
            .to_ascii_lowercase();
        let plain = setting
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        if !plain
            || matches!(
                setting.as_str(),
                "default_transaction_read_only" | "transaction_read_only"
            )
        {
            return true;
        }
    }
    false
}

/// True when a `DO` statement could switch a read-only session to
/// read-write and make that stick inside the block.
///
/// The body is lexed as PL/pgSQL and only its code counts: words and
/// quoted names, not string literals (nested dollar quotes included) or
/// comments, so `SELECT 'commit'` in a block is harmless. Refused: COMMIT
/// and ROLLBACK, `READ WRITE`, `RESET ALL`, the mode variables, and a
/// `set_config` whose setting is not a plain literal outside the mode
/// variables (`set_config('varve.x', ...)` is allowed). Dynamic SQL is
/// allowed in two forms whose statement text is known: `EXECUTE '<sql>'`
/// and `EXECUTE format('<sql>', ...)` with only `%L` (literal) and `%%`
/// placeholders; the SQL is checked by the same rules. Any other `EXECUTE`
/// is refused (its text is built at run time). Fail-closed: a body in
/// another language, an escape string, two bodies, or text the lexer leaves
/// unterminated is searched as raw text, literals and comments included.
fn block_can_switch_mode(text: &str) -> bool {
    match do_body(text) {
        Some(body) => body_can_switch_mode(&body),
        None => raw_text_can_switch_mode(text),
    }
}

/// True when any word of `words` (upper-cased) could switch read-only mode
/// off: transaction control, `set_config`, the mode variables or a `READ
/// WRITE` pair.
fn words_can_switch_mode(words: &[String]) -> bool {
    words.iter().any(|w| {
        matches!(
            w.as_str(),
            "COMMIT"
                | "ROLLBACK"
                | "SET_CONFIG"
                | "DEFAULT_TRANSACTION_READ_ONLY"
                | "TRANSACTION_READ_ONLY"
        )
    }) || words
        .windows(2)
        .any(|pair| pair[0] == "READ" && pair[1] == "WRITE")
}

/// The raw-text check: every word, also inside literals and comments.
fn raw_text_can_switch_mode(text: &str) -> bool {
    let words: Vec<String> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    words_can_switch_mode(&words)
}

/// The PL/pgSQL body of a `DO [LANGUAGE plpgsql] <string>` statement, or
/// `None` when it is not certain what runs (another language, an escape
/// string, more than one body, anything else in the statement).
fn do_body(text: &str) -> Option<String> {
    let lexemes: Vec<Lexeme> = lex(text, Dialect::Postgres, true)
        .into_iter()
        .filter(|l| l.kind != LexKind::Comment)
        .collect();
    let piece = |l: &Lexeme| text.get(l.start..l.end).unwrap_or_default();
    let mut rest = lexemes.iter();
    if !rest
        .next()
        .is_some_and(|l| piece(l).eq_ignore_ascii_case("DO"))
    {
        return None;
    }
    let mut body = None;
    while let Some(lexeme) = rest.next() {
        match lexeme.kind {
            LexKind::Word if piece(lexeme).eq_ignore_ascii_case("LANGUAGE") => {
                let language = rest.next()?;
                let name = match language.kind {
                    LexKind::Word => piece(language).to_ascii_lowercase(),
                    LexKind::String | LexKind::QuotedIdent => {
                        quoted_inner(piece(language)).to_owned()
                    }
                    _ => return None,
                };
                if name != "plpgsql" {
                    return None;
                }
            }
            LexKind::DollarString if body.is_none() => {
                let raw = piece(lexeme);
                let tag_end = raw[1..].find('$')? + 2;
                let tag = &raw[..tag_end];
                let inner = raw.strip_prefix(tag)?.strip_suffix(tag)?;
                if raw.len() < 2 * tag.len() {
                    return None;
                }
                body = Some(inner.to_owned());
            }
            LexKind::String if body.is_none() => {
                let raw = piece(lexeme);
                // An escape string (E'...') would need its escapes decoded.
                if text[..lexeme.start].ends_with(['E', 'e'])
                    || raw.len() < 2
                    || !raw.ends_with('\'')
                {
                    return None;
                }
                body = Some(quoted_inner(raw).replace("''", "'"));
            }
            _ => return None,
        }
    }
    body
}

/// True when the code of a PL/pgSQL body (or of the SQL an `EXECUTE` runs)
/// could switch read-only mode off. See [`block_can_switch_mode`].
fn body_can_switch_mode(body: &str) -> bool {
    let lexemes = lex(body, Dialect::Postgres, true);
    // Text the lexer ran to the end with (an unterminated literal, quoted
    // name or comment) is not certain: search it all.
    if !lexemes.iter().all(|l| terminated(body, l)) {
        return raw_text_can_switch_mode(body);
    }
    let code: Vec<&Lexeme> = lexemes
        .iter()
        .filter(|l| l.kind != LexKind::Comment)
        .collect();
    let mut words = Vec::new();
    let mut quoted_set_config = false;
    for lexeme in &code {
        match lexeme.kind {
            LexKind::Word => words.push(lexeme_text(body, lexeme).to_ascii_uppercase()),
            // A quoted name still names the function or variable.
            LexKind::QuotedIdent => {
                let name = quoted_inner(lexeme_text(body, lexeme)).to_ascii_uppercase();
                quoted_set_config |= name == "SET_CONFIG";
                words.push(name);
            }
            _ => {}
        }
    }
    let resets_all = words
        .windows(2)
        .any(|pair| pair[0] == "RESET" && pair[1] == "ALL");
    let mode_words = words.iter().any(|w| {
        matches!(
            w.as_str(),
            "COMMIT" | "ROLLBACK" | "DEFAULT_TRANSACTION_READ_ONLY" | "TRANSACTION_READ_ONLY"
        )
    }) || words
        .windows(2)
        .any(|pair| pair[0] == "READ" && pair[1] == "WRITE");
    let set_config = words.iter().any(|w| w == "SET_CONFIG")
        && (quoted_set_config || set_config_may_switch_mode(body));
    if resets_all || mode_words || set_config {
        return true;
    }
    // Every EXECUTE must run text that is known and passes these rules.
    code.iter().enumerate().any(|(at, lexeme)| {
        lexeme.kind == LexKind::Word
            && lexeme_text(body, lexeme).eq_ignore_ascii_case("EXECUTE")
            && dynamic_sql(body, &code[at + 1..]).is_none_or(|sql| body_can_switch_mode(&sql))
    })
}

fn lexeme_text<'a>(text: &'a str, lexeme: &Lexeme) -> &'a str {
    text.get(lexeme.start..lexeme.end).unwrap_or_default()
}

/// True when a literal, quoted name or block comment is closed.
fn terminated(text: &str, lexeme: &Lexeme) -> bool {
    let piece = lexeme_text(text, lexeme);
    match lexeme.kind {
        LexKind::String => piece.len() >= 2 && piece.ends_with('\''),
        LexKind::QuotedIdent => piece.len() >= 2 && piece.ends_with('"'),
        LexKind::DollarString => {
            piece
                .get(1..)
                .and_then(|rest| rest.find('$'))
                .is_some_and(|end| {
                    let tag = &piece[..end + 2];
                    piece.len() >= 2 * tag.len() && piece.ends_with(tag)
                })
        }
        LexKind::Comment => !piece.starts_with("/*") || piece.ends_with("*/"),
        _ => true,
    }
}

/// The text of a string literal lexeme: `'...'` (not an escape string) or
/// a dollar-quoted string.
fn literal_text(text: &str, lexeme: &Lexeme, previous: Option<&&Lexeme>) -> Option<String> {
    let piece = lexeme_text(text, lexeme);
    match lexeme.kind {
        LexKind::String => {
            // E'...' needs its escapes decoded: not checked.
            let escape = previous.is_some_and(|p| {
                p.kind == LexKind::Word
                    && p.end == lexeme.start
                    && lexeme_text(text, p).eq_ignore_ascii_case("e")
            });
            (!escape).then(|| quoted_inner(piece).replace("''", "'"))
        }
        LexKind::DollarString => {
            let end = piece.get(1..)?.find('$')? + 2;
            let tag = &piece[..end];
            Some(piece.strip_prefix(tag)?.strip_suffix(tag)?.to_owned())
        }
        _ => None,
    }
}

/// The SQL an `EXECUTE` runs, when its text is known: `EXECUTE '<sql>'` or
/// `EXECUTE [pg_catalog.]format('<template>', args...)` where the template
/// has only `%L` and `%%` placeholders (each `%L` becomes `''`). `after`
/// are the code lexemes after `EXECUTE`; the expression must end at `;`,
/// `USING`, `INTO` or the end. `None` when the text is built otherwise.
fn dynamic_sql(text: &str, after: &[&Lexeme]) -> Option<String> {
    let word = |at: usize| {
        after
            .get(at)
            .filter(|l| l.kind == LexKind::Word)
            .map(|l| lexeme_text(text, l).to_ascii_uppercase())
    };
    let ends_at = |at: usize| match after.get(at) {
        None => true,
        Some(l) if l.kind == LexKind::Punct(';') => true,
        Some(_) => matches!(word(at).as_deref(), Some("USING" | "INTO")),
    };
    // EXECUTE '<sql>'
    if let Some(sql) = after
        .first()
        .and_then(|l| literal_text(text, l, None))
        .filter(|_| ends_at(1))
    {
        return Some(sql);
    }
    // EXECUTE [pg_catalog.]format('<template>', ...)
    let mut at = 0;
    if word(0).as_deref() == Some("PG_CATALOG")
        && after.get(1).is_some_and(|l| l.kind == LexKind::Punct('.'))
    {
        at = 2;
    }
    if word(at).as_deref() != Some("FORMAT")
        || !after
            .get(at + 1)
            .is_some_and(|l| l.kind == LexKind::Punct('('))
    {
        return None;
    }
    let template = literal_text(text, after.get(at + 2)?, after.get(at + 1))?;
    // The closing parenthesis of the call, then the end of the expression.
    let mut depth = 0usize;
    let mut close = None;
    for (index, lexeme) in after.iter().enumerate().skip(at + 1) {
        match lexeme.kind {
            LexKind::Punct('(') => depth += 1,
            LexKind::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    if !ends_at(close? + 1) {
        return None;
    }
    // Only literal placeholders: %L (also %1$L) and %%.
    let mut sql = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            sql.push(c);
            continue;
        }
        let mut spec = String::new();
        while let Some(&next) = chars.peek() {
            spec.push(next);
            chars.next();
            if next.is_ascii_alphabetic() || next == '%' {
                break;
            }
        }
        match spec.chars().last() {
            Some('%') if spec == "%" => sql.push('%'),
            Some('L')
                if spec[..spec.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '$') =>
            {
                sql.push_str("''");
            }
            _ => return None,
        }
    }
    Some(sql)
}

/// PostgreSQL `BEGIN ATOMIC ... END` bodies of `CREATE FUNCTION` and
/// `CREATE PROCEDURE` (14+), whose inner `;` do not end the statement.
///
/// Inside a body, a bare `END` closes the innermost open construct (the
/// body or a `CASE` expression). Because the word after `END` decides, an
/// `END` is resolved at the next word or `;`.
#[derive(Debug, Default)]
struct PgBlocks {
    /// Open constructs, innermost last: `true` for a body, `false` for a
    /// `CASE` inside a body.
    open: Vec<bool>,
    /// An `END` whose meaning depends on the next word.
    pending_end: bool,
}

impl PgBlocks {
    /// Called for every word (`token`) before it is added to `previous`.
    fn word(&mut self, token: &Token, previous: &[Token]) {
        if self.pending_end {
            self.pending_end = false;
            self.open.pop();
        }
        let word = token.word.as_str();
        // `BEGIN ATOMIC` as two adjacent top-level words in a routine
        // definition (`CREATE TABLE begin (atomic int)` is not one).
        let opens = word == "ATOMIC"
            && !token.detached
            && token.depth == 0
            && previous
                .last()
                .is_some_and(|t| t.word == "BEGIN" && t.depth == 0)
            && previous.first().is_some_and(|t| t.word == "CREATE")
            && previous
                .iter()
                .any(|t| t.depth == 0 && matches!(t.word.as_str(), "FUNCTION" | "PROCEDURE"));
        if opens {
            self.open.push(true);
            return;
        }
        if !self.open.is_empty() {
            match word {
                "CASE" => self.open.push(false),
                "END" => self.pending_end = true,
                _ => {}
            }
        }
    }

    /// Called for a word in a name position: it resolves a pending `END`.
    fn name(&mut self) {
        if self.pending_end {
            self.pending_end = false;
            self.open.pop();
        }
    }

    /// Called at a top-level `;`. True when the `;` is inside a body.
    fn semicolon(&mut self) -> bool {
        self.name();
        !self.open.is_empty()
    }
}

/// A compound construct of MySQL and MariaDB stored programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Construct {
    /// `BEGIN ... END` (also MariaDB `BEGIN NOT ATOMIC`).
    Begin,
    /// `IF ... END IF`.
    If,
    /// A `CASE` statement, closed by `END CASE`.
    CaseStatement,
    /// A `CASE` expression, closed by `END`.
    CaseExpression,
    /// `LOOP ... END LOOP`.
    Loop,
    /// `WHILE ... DO ... END WHILE`.
    While,
    /// `REPEAT ... UNTIL ... END REPEAT`.
    Repeat,
    /// MariaDB `FOR ... DO ... END FOR`.
    For,
}

/// An item of a statement header (what decides where a routine body
/// starts).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Word(String),
    Quoted,
    Literal,
    Punct(char),
}

/// Where the header of a MySQL `CREATE`/`ALTER` statement ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderEnd {
    /// More items are needed.
    Incomplete,
    /// The item at this index is the first item of the body.
    BodyAt(usize),
    /// The statement has no body (a table, a view, a loadable function).
    NoBody,
    /// A construct the splitter does not model.
    Unknown,
}

/// Words of routine characteristics, which come between the parameter
/// list (or the return type) and the body.
const CHARACTERISTICS: [&str; 13] = [
    "COMMENT",
    "LANGUAGE",
    "SQL",
    "NOT",
    "DETERMINISTIC",
    "CONTAINS",
    "NO",
    "READS",
    "MODIFIES",
    "DATA",
    "SECURITY",
    "DEFINER",
    "INVOKER",
];

/// Words that may follow the first word of a function's return type
/// (`DOUBLE PRECISION`, `LONG VARCHAR`, `INT UNSIGNED`, ...).
const TYPE_WORDS: [&str; 14] = [
    "UNSIGNED",
    "SIGNED",
    "ZEROFILL",
    "BINARY",
    "ASCII",
    "UNICODE",
    "BYTE",
    "PRECISION",
    "VARYING",
    "VARCHAR",
    "CHAR",
    "VARBINARY",
    "NCHAR",
    "LONG",
];

/// Finds where the body of a routine, trigger or event definition starts.
/// `items` are the statement's items so far, from its first word.
fn header_end(items: &[Item]) -> HeaderEnd {
    let word = |i: usize| match items.get(i) {
        Some(Item::Word(w)) => Some(w.as_str()),
        _ => None,
    };
    if !matches!(word(0), Some("CREATE" | "ALTER")) {
        return HeaderEnd::NoBody;
    }
    // The object kind: the first word that names one (DEFINER clauses and
    // OR REPLACE come before it).
    let mut i = 1;
    let kind = loop {
        match items.get(i) {
            None => return HeaderEnd::Incomplete,
            Some(Item::Word(w)) => match w.as_str() {
                "PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT" | "PACKAGE" => break w.as_str(),
                "OR" | "REPLACE" | "DEFINER" | "AGGREGATE" | "CURRENT_USER" => {}
                // A user name in a DEFINER clause (`DEFINER = admin@host`).
                _ if matches!(items.get(i.wrapping_sub(1)), Some(Item::Punct('=' | '@'))) => {}
                _ => return HeaderEnd::NoBody,
            },
            Some(_) => {}
        }
        i += 1;
    };
    i += 1;
    match kind {
        "PACKAGE" => HeaderEnd::Unknown,
        "EVENT" => {
            // The body follows the first DO.
            match items[i.min(items.len())..]
                .iter()
                .position(|item| *item == Item::Word("DO".into()))
            {
                Some(at) if i + at + 1 < items.len() => HeaderEnd::BodyAt(i + at + 1),
                _ => HeaderEnd::Incomplete,
            }
        }
        "TRIGGER" => {
            let Some(row) = items
                .windows(3)
                .position(|w| {
                    w == [
                        Item::Word("FOR".into()),
                        Item::Word("EACH".into()),
                        Item::Word("ROW".into()),
                    ]
                })
                .map(|at| at + 3)
            else {
                return HeaderEnd::Incomplete;
            };
            match word(row) {
                Some("FOLLOWS" | "PRECEDES") => {
                    if row + 2 < items.len() {
                        HeaderEnd::BodyAt(row + 2)
                    } else {
                        HeaderEnd::Incomplete
                    }
                }
                _ if row < items.len() => HeaderEnd::BodyAt(row),
                _ => HeaderEnd::Incomplete,
            }
        }
        _ => routine_header_end(items, i, kind == "FUNCTION", word(0) == Some("ALTER")),
    }
}

/// The header end of a procedure or function from the item after
/// `PROCEDURE`/`FUNCTION`.
fn routine_header_end(items: &[Item], mut i: usize, function: bool, alter: bool) -> HeaderEnd {
    let word = |i: usize| match items.get(i) {
        Some(Item::Word(w)) => Some(w.as_str()),
        _ => None,
    };
    if alter {
        // ALTER PROCEDURE / FUNCTION only changes characteristics.
        return HeaderEnd::NoBody;
    }
    if word(i) == Some("IF") {
        i += 3;
    }
    // The name, maybe qualified.
    i += 1;
    while items.get(i) == Some(&Item::Punct('.')) {
        i += 2;
    }
    match items.get(i) {
        None => return HeaderEnd::Incomplete,
        Some(Item::Punct('(')) => {}
        // A loadable function: `CREATE FUNCTION f RETURNS STRING SONAME ...`.
        Some(_) => return HeaderEnd::NoBody,
    }
    // The parameter list.
    let mut depth = 0usize;
    loop {
        match items.get(i) {
            None => return HeaderEnd::Incomplete,
            Some(Item::Punct('(')) => depth += 1,
            Some(Item::Punct(')')) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Some(_) => {}
        }
        i += 1;
    }
    i += 1;
    if function {
        if word(i) != Some("RETURNS") {
            return if i < items.len() {
                HeaderEnd::Unknown
            } else {
                HeaderEnd::Incomplete
            };
        }
        // MariaDB anchored types: `TYPE OF t.c`, `ROW TYPE OF t`.
        let anchored = match (word(i + 1), word(i + 2), word(i + 3)) {
            (Some("TYPE"), Some("OF"), _) => Some(i + 3),
            (Some("ROW"), Some("TYPE"), Some("OF")) => Some(i + 4),
            // Not decided until the words after TYPE / ROW TYPE are read.
            (Some("TYPE"), None, _) | (Some("ROW"), Some("TYPE"), None) if items.len() <= i + 3 => {
                return HeaderEnd::Incomplete;
            }
            (Some("ROW"), None, _) if items.len() <= i + 2 => return HeaderEnd::Incomplete,
            _ => None,
        };
        if let Some(name) = anchored {
            // The anchor: a name, maybe qualified.
            i = name + 1;
            while items.get(i) == Some(&Item::Punct('.')) {
                i += 2;
            }
            return characteristics_end(items, i);
        }
        // The type: its first word, more type words, arguments, character
        // set and collation.
        i += 2;
        loop {
            match items.get(i) {
                None => return HeaderEnd::Incomplete,
                Some(Item::Punct('(')) => {
                    let mut depth = 0usize;
                    loop {
                        match items.get(i) {
                            None => return HeaderEnd::Incomplete,
                            Some(Item::Punct('(')) => depth += 1,
                            Some(Item::Punct(')')) => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            Some(_) => {}
                        }
                        i += 1;
                    }
                    i += 1;
                }
                Some(Item::Word(w)) if TYPE_WORDS.contains(&w.as_str()) => i += 1,
                Some(Item::Word(w)) if w == "CHARSET" || w == "COLLATE" => i += 2,
                Some(Item::Word(w)) if w == "CHARACTER" => i += 3,
                Some(_) => break,
            }
        }
    }
    characteristics_end(items, i)
}

/// Skips routine characteristics from item `i`: the body starts after them.
fn characteristics_end(items: &[Item], mut i: usize) -> HeaderEnd {
    loop {
        match items.get(i) {
            None => return HeaderEnd::Incomplete,
            Some(Item::Word(w)) if w == "COMMENT" => i += 2,
            Some(Item::Word(w)) if CHARACTERISTICS.contains(&w.as_str()) => i += 1,
            Some(_) => return HeaderEnd::BodyAt(i),
        }
    }
}

/// Words after which an operand follows: an `END` right after one of them
/// is a name (`THEN end`), not the end of a `CASE` expression.
const OPERAND_FOLLOWS: [&str; 26] = [
    "THEN", "ELSE", "WHEN", "CASE", "AND", "OR", "NOT", "XOR", "IS", "IN", "LIKE", "BETWEEN",
    "DIV", "MOD", "REGEXP", "RLIKE", "SELECT", "RETURN", "SET", "DO", "UNTIL", "IF", "ELSEIF",
    "WHILE", "ESCAPE", "DISTINCT",
];

/// The state of a `DECLARE ... HANDLER FOR <conditions> <statement>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Handler {
    /// Not in a handler declaration.
    #[default]
    None,
    /// `DECLARE` seen: waiting for `HANDLER`.
    Declare,
    /// `HANDLER` seen: waiting for `FOR`.
    AwaitFor,
    /// A condition value is expected.
    ExpectValue,
    /// `SQLSTATE` seen: `VALUE` or the string follows.
    AfterSqlstate,
    /// `NOT` seen: `FOUND` follows.
    AfterNot,
    /// A condition value ended: a comma, or the handler's statement.
    AfterValue,
}

/// Compound statements of MySQL and MariaDB written without `DELIMITER`:
/// routine, trigger and event bodies that are a compound statement
/// (`BEGIN`, `IF`, `CASE`, `LOOP`, `WHILE`, `REPEAT`, MariaDB `FOR`, with
/// labels), MariaDB `BEGIN NOT ATOMIC` and top-level compound statements.
/// A `;` inside an open construct does not end the statement.
///
/// Openers count only where a statement starts: at the start of a
/// top-level statement, of a routine body, after a `;` inside a construct,
/// after `BEGIN`, `THEN`, `ELSE`, `LOOP`, `REPEAT`, the `DO` of `WHILE` and
/// `FOR`, a label, and the condition list of a handler. So `IF(...)` and
/// `REPEAT(...)` functions, `CASE` expressions and `begin` used as a name
/// are told apart from statements.
#[derive(Debug, Default)]
struct MysqlBlocks {
    open: Vec<Construct>,
    /// An `END` whose meaning depends on the next word.
    pending_end: bool,
    /// The next word starts a statement.
    at_start: bool,
    /// The last item was a word at a statement start, which a `:` makes a
    /// label.
    label_candidate: bool,
    /// The condition of a `WHILE` or `FOR` is read: its `DO` starts the body.
    awaiting_do: bool,
    handler: Handler,
    /// The items of a `CREATE`/`ALTER` statement until its body starts.
    header: Option<Vec<Item>>,
    /// A top-level statement starting with `BEGIN`: 1 after `BEGIN`, 2
    /// after `BEGIN NOT`.
    top_begin: u8,
    /// A `BEGIN` inside a body just opened: 1 after `BEGIN`, 2 after
    /// `BEGIN NOT` (MariaDB `BEGIN NOT ATOMIC`).
    after_begin: u8,
    /// The body of a definition has started: statements in it may open
    /// constructs.
    body: bool,
    /// The last item ends an operand (a name, a literal, `)`): an `END`
    /// after it can end a `CASE` expression.
    prev_value: bool,
    /// The `UNTIL` of the innermost `REPEAT` was read.
    until: bool,
    /// An `END` after an `UNTIL` condition: it ends the `REPEAT` only when
    /// `REPEAT` follows; otherwise it was a name.
    pending_until: bool,
    /// Something was not certain (see [`AmbiguousSplit`]).
    uncertain: bool,
}

impl MysqlBlocks {
    /// A new top-level statement starts.
    fn statement_start(&mut self) {
        self.at_start = true;
        self.label_candidate = false;
        self.awaiting_do = false;
        self.handler = Handler::None;
        self.header = None;
        self.top_begin = 0;
        self.after_begin = 0;
        self.body = false;
    }

    fn top(&self) -> Option<Construct> {
        self.open.last().copied()
    }

    /// Resolves a pending `END` by `next` (the word after it, `None` for
    /// anything else). Returns true when `next` belonged to the `END`.
    fn resolve_end(&mut self, next: Option<&str>) -> bool {
        self.pending_end = false;
        let closes = match next {
            Some("IF") => Some(Construct::If),
            Some("CASE") => Some(Construct::CaseStatement),
            Some("LOOP") => Some(Construct::Loop),
            Some("WHILE") => Some(Construct::While),
            Some("REPEAT") => Some(Construct::Repeat),
            Some("FOR") => Some(Construct::For),
            _ => None,
        };
        match closes {
            Some(construct) => {
                if self.open.pop() != Some(construct) {
                    self.uncertain = true;
                }
                if construct == Construct::Repeat {
                    self.until = false;
                }
                true
            }
            None => {
                if !matches!(
                    self.open.pop(),
                    Some(Construct::Begin | Construct::CaseExpression)
                ) {
                    self.uncertain = true;
                }
                false
            }
        }
    }

    /// Opens `construct` for a word at a statement start. Returns true when
    /// `word` opens one.
    fn open_at_start(&mut self, word: &str) -> bool {
        let construct = match word {
            "BEGIN" => Construct::Begin,
            "IF" => Construct::If,
            "CASE" => Construct::CaseStatement,
            "LOOP" => Construct::Loop,
            "WHILE" => Construct::While,
            "REPEAT" => Construct::Repeat,
            "FOR" => Construct::For,
            _ => return false,
        };
        self.open.push(construct);
        if construct == Construct::Repeat {
            self.until = false;
        }
        self.after_begin = u8::from(construct == Construct::Begin);
        self.at_start = matches!(
            construct,
            Construct::Begin | Construct::Loop | Construct::Repeat
        );
        self.awaiting_do = matches!(construct, Construct::While | Construct::For);
        true
    }

    /// Reads the header of a definition; returns true when `item` (just
    /// added) starts the body.
    fn header_item(&mut self, item: Item) -> bool {
        let Some(items) = self.header.as_mut() else {
            return false;
        };
        items.push(item);
        match header_end(items) {
            HeaderEnd::Incomplete => false,
            HeaderEnd::NoBody => {
                self.header = None;
                false
            }
            HeaderEnd::Unknown => {
                self.uncertain = true;
                self.header = None;
                false
            }
            HeaderEnd::BodyAt(at) => {
                let starts = at + 1 == items.len();
                self.header = None;
                self.body = true;
                // A body that started at an item already passed is not a
                // compound statement.
                starts
            }
        }
    }

    /// Called for every word that is not a name (`t.end`, `AS case`).
    /// `first` is true for the first word of a top-level statement.
    fn word(&mut self, word: &str, first: bool) {
        if self.pending_until {
            self.pending_until = false;
            if word == "REPEAT" {
                if self.open.pop() != Some(Construct::Repeat) {
                    self.uncertain = true;
                }
                self.until = false;
                self.at_start = false;
                self.label_candidate = false;
                self.prev_value = false;
                return;
            }
            // The END was a name in the condition.
        }
        self.word_inner(word, first);
        self.prev_value = !OPERAND_FOLLOWS.contains(&word);
    }

    fn word_inner(&mut self, word: &str, first: bool) {
        if self.pending_end && self.resolve_end(Some(word)) {
            self.at_start = false;
            self.label_candidate = false;
            return;
        }
        match (self.after_begin, word) {
            (1, "NOT") => {
                self.after_begin = 2;
                return;
            }
            (2, "ATOMIC") => {
                self.after_begin = 0;
                self.at_start = true;
                return;
            }
            _ => self.after_begin = 0,
        }
        if first && self.open.is_empty() && matches!(word, "CREATE" | "ALTER") {
            self.header = Some(Vec::new());
        }
        let body_starts = self.header_item(Item::Word(word.to_owned()));
        if word == "END" {
            // END ends a construct where a statement starts, after the
            // operand that ends a CASE expression, or after a REPEAT's
            // condition when REPEAT follows. Anywhere else it is a name
            // (`SELECT end FROM t`).
            let closes = if self.at_start && !self.open.is_empty() {
                true
            } else if self.top() == Some(Construct::CaseExpression) {
                self.prev_value
            } else {
                if self.top() == Some(Construct::Repeat) && self.until && self.prev_value {
                    self.pending_until = true;
                    self.at_start = false;
                    self.label_candidate = false;
                    return;
                }
                false
            };
            if closes {
                self.pending_end = true;
                self.at_start = false;
                self.label_candidate = false;
                self.handler = Handler::None;
                return;
            }
            if self.at_start && self.open.is_empty() && self.header.is_none() && !self.body {
                // A top-level statement that starts with END: no construct
                // is open that it could end.
                self.uncertain = true;
            }
        }
        // The statement of a handler starts after its condition list.
        let handler_starts = match self.handler {
            Handler::Declare if word == "HANDLER" => {
                self.handler = Handler::AwaitFor;
                false
            }
            Handler::AwaitFor if word == "FOR" => {
                self.handler = Handler::ExpectValue;
                return;
            }
            Handler::ExpectValue => {
                self.handler = match word {
                    "SQLSTATE" => Handler::AfterSqlstate,
                    "NOT" => Handler::AfterNot,
                    _ => Handler::AfterValue,
                };
                return;
            }
            Handler::AfterSqlstate if word == "VALUE" => return,
            Handler::AfterNot => {
                self.handler = Handler::AfterValue;
                return;
            }
            Handler::AfterValue => {
                self.handler = Handler::None;
                true
            }
            _ => false,
        };
        // Branch keywords of IF and CASE statements stand where a statement
        // could: ELSE starts one, the others a condition.
        if matches!(self.top(), Some(Construct::If | Construct::CaseStatement)) {
            match word {
                "ELSE" => {
                    self.at_start = true;
                    self.label_candidate = false;
                    return;
                }
                "ELSEIF" | "WHEN" => {
                    self.at_start = false;
                    self.label_candidate = false;
                    return;
                }
                _ => {}
            }
        }
        if word == "UNTIL" && self.top() == Some(Construct::Repeat) {
            self.until = true;
            self.at_start = false;
            self.label_candidate = false;
            return;
        }
        let at_start = self.at_start || body_starts || handler_starts;
        self.label_candidate = false;
        if at_start {
            self.at_start = false;
            let inside = !self.open.is_empty() || self.body || handler_starts;
            if inside && self.open_at_start(word) {
                return;
            }
            if !inside {
                // A top-level statement: only compound statements open a
                // construct (MariaDB runs them outside routines; MySQL
                // rejects them). A bare BEGIN starts a transaction.
                match word {
                    "BEGIN" => self.top_begin = 1,
                    "IF" | "CASE" | "LOOP" | "WHILE" | "REPEAT" | "FOR" => {
                        self.open_at_start(word);
                        return;
                    }
                    _ => {}
                }
            }
            if word == "DECLARE" {
                self.handler = Handler::Declare;
            }
            self.label_candidate = true;
            return;
        }
        match self.top_begin {
            1 if word == "NOT" => {
                self.top_begin = 2;
                return;
            }
            2 if word == "ATOMIC" => {
                self.top_begin = 0;
                self.open.push(Construct::Begin);
                self.at_start = true;
                return;
            }
            _ => self.top_begin = 0,
        }
        if self.open.is_empty() {
            return;
        }
        match word {
            "CASE" => self.open.push(Construct::CaseExpression),
            "THEN" | "ELSE"
                if matches!(self.top(), Some(Construct::If | Construct::CaseStatement)) =>
            {
                self.at_start = true;
            }
            "DO" if self.awaiting_do => {
                self.awaiting_do = false;
                self.at_start = true;
            }
            _ => {}
        }
    }

    /// Called for a word in a name position (`t.end`, `AS case`).
    fn name(&mut self) {
        self.pending_until = false;
        self.prev_value = true;
        if self.pending_end {
            self.resolve_end(None);
        }
        let _ = self.header_item(Item::Word(String::new()));
        self.at_start = false;
        self.label_candidate = false;
        self.top_begin = 0;
        self.after_begin = 0;
    }

    /// Called for anything but words, comments and a top-level `;`.
    fn other(&mut self, item: Item) {
        self.pending_until = false;
        self.prev_value = matches!(item, Item::Literal | Item::Punct(')'));
        if self.pending_end {
            self.resolve_end(None);
        }
        let is_colon = item == Item::Punct(':');
        let is_comma = item == Item::Punct(',');
        let is_literal = matches!(item, Item::Literal);
        let body_starts = self.header_item(item);
        match self.handler {
            Handler::ExpectValue if is_literal => self.handler = Handler::AfterValue,
            Handler::AfterSqlstate if is_literal => self.handler = Handler::AfterValue,
            Handler::AfterValue if is_comma => self.handler = Handler::ExpectValue,
            Handler::AfterValue => self.handler = Handler::None,
            _ => {}
        }
        if is_colon && self.label_candidate {
            // A label: the statement starts after it. A labelled statement
            // at the top level or at a body start is a compound statement.
            self.label_candidate = false;
            self.at_start = true;
            return;
        }
        self.label_candidate = body_starts;
        self.at_start = false;
        self.top_begin = 0;
        self.after_begin = 0;
    }

    /// Called for a quoted name (a label may be quoted).
    fn quoted(&mut self) {
        self.pending_until = false;
        self.prev_value = true;
        if self.pending_end {
            self.resolve_end(None);
        }
        let body_starts = self.header_item(Item::Quoted);
        let starts = self.at_start || body_starts;
        self.at_start = false;
        self.top_begin = 0;
        self.after_begin = 0;
        self.label_candidate = starts;
    }

    /// Called at a top-level `;`. True when the `;` is inside a construct
    /// and does not end the statement.
    fn semicolon(&mut self) -> bool {
        self.pending_until = false;
        self.prev_value = false;
        if self.pending_end {
            self.resolve_end(None);
        }
        if self.open.is_empty() {
            return false;
        }
        self.at_start = true;
        self.label_candidate = false;
        self.awaiting_do = false;
        self.handler = Handler::None;
        true
    }

    /// At the end of the text: a construct still open is not certain.
    fn finish(&mut self) {
        if self.pending_end {
            self.resolve_end(None);
        }
        if !self.open.is_empty() {
            self.uncertain = true;
        }
    }
}

/// A keyword-level token with its parenthesis depth.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    word: String,
    depth: u32,
    /// Punctuation (a comma, an operator, a literal) stands between this
    /// token and the previous one, so the two are not one phrase.
    detached: bool,
}

/// A statement found by the scanner: byte range and its word tokens.
struct Scanned {
    start: usize,
    end: usize,
    tokens: Vec<Token>,
    /// Contents of quoted identifiers (and, for MySQL, double-quoted
    /// strings, which are identifiers under `ANSI_QUOTES`), upper-cased.
    /// They are not keywords, but a quoted variable name still names the
    /// variable.
    quoted: Vec<String>,
}

/// Walks the script once. Literals, quoted identifiers and comments produce
/// no tokens; `;` at parenthesis depth 0 ends a statement.
fn scan(sql: &str, dialect: Dialect) -> Vec<Scanned> {
    scan_with(sql, dialect, &MysqlMode::default()).0
}

/// Like [`scan`], reading MySQL text as a session in `mode` does. Built on
/// the lexer, so splitting, classification and every other consumer of the
/// lexer agree on where literals, comments and statements end. The flag is
/// true when the boundaries are not certain (see [`MysqlBlocks`]).
fn scan_with(sql: &str, dialect: Dialect, mode: &MysqlMode) -> (Vec<Scanned>, bool) {
    let mut out = Vec::new();
    let mut current = Scanned {
        start: 0,
        end: 0,
        tokens: Vec::new(),
        quoted: Vec::new(),
    };
    let mut depth: u32 = 0;
    let mut detached = false;
    let mut pg = PgBlocks::default();
    let mut my = MysqlBlocks::default();
    my.statement_start();
    let mysql = dialect == Dialect::Mysql;
    // A word right after `.` or `AS` is a name (`t.end`, `AS case`), never a
    // block keyword.
    let mut after_dot = false;
    for lexeme in lex_mode(sql, dialect, mode) {
        let was_after_dot = after_dot;
        after_dot = matches!(lexeme.kind, LexKind::Punct('.'));
        let text = &sql[lexeme.start..lexeme.end];
        match lexeme.kind {
            // Comments separate words but keep them one phrase; the markers
            // of executable comments that run are transparent.
            LexKind::Comment | LexKind::ExecutableCommentStart | LexKind::ExecutableCommentEnd => {}
            LexKind::QuotedIdent | LexKind::DoubleQuoted => {
                current.quoted.push(quoted_inner(text).to_uppercase());
                detached = true;
                if mysql {
                    my.quoted();
                }
            }
            LexKind::Punct('(') => {
                depth += 1;
                detached = true;
                if mysql {
                    my.other(Item::Punct('('));
                }
            }
            LexKind::Punct(')') => {
                depth = depth.saturating_sub(1);
                detached = true;
                if mysql {
                    my.other(Item::Punct(')'));
                }
            }
            LexKind::Punct(';') if depth == 0 => {
                let inside = if mysql {
                    my.semicolon()
                } else {
                    pg.semicolon()
                };
                if inside {
                    detached = true;
                } else {
                    current.end = lexeme.start;
                    out.push(current);
                    current = Scanned {
                        start: lexeme.end,
                        end: 0,
                        tokens: Vec::new(),
                        quoted: Vec::new(),
                    };
                    detached = false;
                    if mysql {
                        my.statement_start();
                    }
                }
            }
            LexKind::Word => {
                let word = text.to_ascii_uppercase();
                let named = was_after_dot
                    || current
                        .tokens
                        .last()
                        .is_some_and(|t| t.word == "AS" && !detached);
                let token = Token {
                    word,
                    depth,
                    detached,
                };
                if mysql {
                    if named {
                        my.name();
                    } else {
                        my.word(&token.word, current.tokens.is_empty());
                    }
                } else if named {
                    pg.name();
                } else {
                    pg.word(&token, &current.tokens);
                }
                current.tokens.push(token);
                detached = false;
            }
            LexKind::String | LexKind::DollarString | LexKind::Number | LexKind::Parameter => {
                detached = true;
                if mysql {
                    my.other(Item::Literal);
                }
            }
            LexKind::Punct(c) => {
                detached = true;
                if mysql {
                    my.other(Item::Punct(c));
                }
            }
        }
    }
    current.end = sql.len();
    out.push(current);
    let uncertain = if mysql {
        my.finish();
        my.uncertain
    } else {
        false
    };
    (out, uncertain)
}

/// What a [`Lexeme`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexKind {
    /// A keyword or unquoted identifier.
    Word,
    /// A quoted identifier: `"name"` (PostgreSQL) or `` `name` `` (MySQL).
    QuotedIdent,
    /// A MySQL `"..."`: a string by default, an identifier under
    /// `ANSI_QUOTES`.
    DoubleQuoted,
    /// A `'...'` string literal (PostgreSQL `E'...'` included, prefix
    /// excluded).
    String,
    /// A PostgreSQL dollar-quoted string (`$$...$$`, `$tag$...$tag$`).
    DollarString,
    /// A numeric literal.
    Number,
    /// A PostgreSQL positional parameter (`$1`).
    Parameter,
    /// A line or block comment (not an executable one).
    Comment,
    /// The `/*!` or `/*M!` (with optional version) that opens a MySQL or
    /// MariaDB executable comment; what follows is code.
    ExecutableCommentStart,
    /// The `*/` that closes an executable comment.
    ExecutableCommentEnd,
    /// Any other single character: operators and punctuation.
    Punct(char),
}

/// One token of SQL text, with byte offsets into the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lexeme {
    /// What it is.
    pub kind: LexKind,
    /// Byte offset of its first character.
    pub start: usize,
    /// Byte offset just past it. An unterminated literal or comment runs to
    /// the end of the text.
    pub end: usize,
}

/// True for characters that may continue an unquoted identifier. Both
/// engines accept every non-ASCII character in identifiers (PostgreSQL any
/// byte from 0x80, MySQL U+0080 and up), and both accept `$` after the first
/// character.
fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// True for characters that may start an unquoted identifier. MySQL also
/// lets `$` start one; in PostgreSQL a leading `$` is a parameter or a
/// dollar quote.
fn ident_start(c: char, dialect: Dialect) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii() || (dialect == Dialect::Mysql && c == '$')
}

/// Splits SQL text into tokens with the lexical rules of the engine:
/// comments (MySQL `--` only when whitespace or a control character follows;
/// a line comment ends at `\n` or `\r`; PostgreSQL block comments nest),
/// MySQL executable comments, quoted identifiers, strings (backslash escapes
/// in PostgreSQL `E'...'` strings, and in MySQL strings when
/// `mysql_backslash_escapes` is true, the default `sql_mode`), PostgreSQL
/// dollar quotes and parameters, numbers, words and punctuation. Whitespace
/// is skipped. Text is read as Unicode; the drivers always talk UTF-8 to the
/// server, so the server reads the same characters.
#[must_use]
pub fn lex(sql: &str, dialect: Dialect, mysql_backslash_escapes: bool) -> Vec<Lexeme> {
    lex_mode(
        sql,
        dialect,
        &MysqlMode {
            backslash_escapes: mysql_backslash_escapes,
            ..MysqlMode::default()
        },
    )
}

/// Like [`lex`] for MySQL and MariaDB text read as a session in `mode`
/// does: `ANSI_QUOTES` makes `"..."` an identifier without backslash
/// escapes, and an executable comment that does not run on the server
/// (`/*M! */` on MySQL, `/*!NNNNN */` on an older server) is a comment.
#[must_use]
pub fn lex_mysql(sql: &str, mode: &MysqlMode) -> Vec<Lexeme> {
    lex_mode(sql, Dialect::Mysql, mode)
}

fn lex_mode(sql: &str, dialect: Dialect, mode: &MysqlMode) -> Vec<Lexeme> {
    let mysql_backslash_escapes = mode.backslash_escapes;
    let chars: Vec<(usize, char)> = sql.char_indices().collect();
    let byte_at = |i: usize| chars.get(i).map_or(sql.len(), |(b, _)| *b);
    let char_at = |i: usize| chars.get(i).map(|(_, c)| *c);
    let mysql = dialect == Dialect::Mysql;
    let mut executable = false;
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        let next = char_at(i + 1);
        let start = i;
        let kind = match c {
            '-' if next == Some('-')
                && (!mysql
                    || char_at(i + 2)
                        .is_none_or(|c| c.is_ascii_whitespace() || c.is_control())) =>
            {
                i = skip_line(&chars, i);
                LexKind::Comment
            }
            '#' if mysql => {
                i = skip_line(&chars, i);
                LexKind::Comment
            }
            '/' if next == Some('*')
                && mysql
                && !executable
                && (char_at(i + 2) == Some('!')
                    || (matches!(char_at(i + 2), Some('M' | 'm'))
                        && char_at(i + 3) == Some('!'))) =>
            {
                let mariadb_only = char_at(i + 2) != Some('!');
                let marker = i;
                i += if mariadb_only { 4 } else { 3 };
                let digits_from = i;
                let mut digits_to = i;
                while char_at(digits_to).is_some_and(|c| c.is_ascii_digit()) {
                    digits_to += 1;
                }
                let digits: String = chars[digits_from..digits_to]
                    .iter()
                    .map(|(_, c)| *c)
                    .collect();
                // Without a server every digit is the version and the
                // contents are code.
                let (version_len, runs) = match mode.server {
                    Some(server) => server.executable_comment(mariadb_only, &digits),
                    None => (digits.len(), true),
                };
                i = digits_from + version_len;
                if runs {
                    executable = true;
                    LexKind::ExecutableCommentStart
                } else {
                    i = skip_block_comment(&chars, marker + 2, false);
                    LexKind::Comment
                }
            }
            '*' if executable && next == Some('/') => {
                executable = false;
                i += 2;
                LexKind::ExecutableCommentEnd
            }
            '/' if next == Some('*') => {
                i = skip_block_comment(&chars, i + 2, !mysql);
                LexKind::Comment
            }
            c if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            '\'' => {
                if mysql {
                    i = skip_quoted(&chars, i + 1, '\'', mysql_backslash_escapes);
                } else {
                    // An escape string is a quote directly after a word that
                    // is exactly `e`: `E'..'`, and also `1e'..'` or `$1e'..'`
                    // where the number stops before the `e` (PostgreSQL 13
                    // and 14 read those as escape strings; 15+ rejects them).
                    // A longer word such as `abc1e` is a name, not a prefix.
                    let escapes = out.last().is_some_and(|prev: &Lexeme| {
                        prev.kind == LexKind::Word
                            && prev.end == byte_at(i)
                            && sql[prev.start..prev.end].eq_ignore_ascii_case("e")
                    });
                    i = skip_pg_string(&chars, i, escapes);
                }
                LexKind::String
            }
            '"' => {
                i = skip_quoted(
                    &chars,
                    i + 1,
                    '"',
                    mysql && mysql_backslash_escapes && !mode.ansi_quotes,
                );
                if mysql {
                    LexKind::DoubleQuoted
                } else {
                    LexKind::QuotedIdent
                }
            }
            '`' if mysql => {
                i = skip_quoted(&chars, i + 1, '`', false);
                LexKind::QuotedIdent
            }
            '$' if !mysql && next.is_some_and(|n| n.is_ascii_digit()) => {
                i += 1;
                while char_at(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
                LexKind::Parameter
            }
            '$' if !mysql => match dollar_tag(&chars, i) {
                Some(tag_len) => {
                    i = skip_dollar_quoted(&chars, i, tag_len);
                    LexKind::DollarString
                }
                None => {
                    i += 1;
                    LexKind::Punct('$')
                }
            },
            c if c.is_ascii_digit() || (c == '.' && next.is_some_and(|n| n.is_ascii_digit())) => {
                while char_at(i).is_some_and(|c| c.is_ascii_digit() || c == '.') {
                    i += 1;
                }
                if matches!(char_at(i), Some('e' | 'E')) {
                    let sign = usize::from(matches!(char_at(i + 1), Some('+' | '-')));
                    if char_at(i + 1 + sign).is_some_and(|c| c.is_ascii_digit()) {
                        i += 1 + sign;
                        while char_at(i).is_some_and(|c| c.is_ascii_digit()) {
                            i += 1;
                        }
                    }
                }
                LexKind::Number
            }
            c if ident_start(c, dialect) => {
                while char_at(i).is_some_and(ident_char) {
                    i += 1;
                }
                LexKind::Word
            }
            c => {
                i += 1;
                LexKind::Punct(c)
            }
        };
        out.push(Lexeme {
            kind,
            start: byte_at(start),
            end: byte_at(i.min(chars.len())),
        });
    }
    out
}

/// The text between the quotes of a quoted lexeme (to the end when it is
/// unterminated).
fn quoted_inner(text: &str) -> &str {
    let mut inner = text.chars();
    let open = inner.next();
    let body = inner.as_str();
    match open {
        Some(q) if body.ends_with(q) && !body.is_empty() => &body[..body.len() - q.len_utf8()],
        _ => body,
    }
}

fn skip_line(chars: &[(usize, char)], mut i: usize) -> usize {
    while i < chars.len() && !matches!(chars[i].1, '\n' | '\r') {
        i += 1;
    }
    i
}

fn skip_block_comment(chars: &[(usize, char)], mut i: usize, nests: bool) -> usize {
    let mut nesting = 1;
    while i < chars.len() && nesting > 0 {
        let next = chars.get(i + 1).map(|(_, c)| *c);
        if nests && chars[i].1 == '/' && next == Some('*') {
            nesting += 1;
            i += 2;
        } else if chars[i].1 == '*' && next == Some('/') {
            nesting -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

/// Skips a PostgreSQL string starting at the quote `i`, including
/// continuations: a following string separated only by whitespace that
/// contains a newline (and `--` comments that end at a newline) is the same
/// literal and keeps the escape mode of the first part.
fn skip_pg_string(chars: &[(usize, char)], i: usize, escapes: bool) -> usize {
    let mut end = skip_quoted(chars, i + 1, '\'', escapes);
    loop {
        let Some(next_quote) = continuation_quote(chars, end) else {
            return end;
        };
        end = skip_quoted(chars, next_quote + 1, '\'', escapes);
    }
}

/// When the text after a closed string at `from` is whitespace with at
/// least one newline (`--` comments allowed anywhere in it, as in
/// PostgreSQL's `whitespace_with_newline`) followed by a quote, returns that
/// quote's index.
fn continuation_quote(chars: &[(usize, char)], from: usize) -> Option<usize> {
    let at = |i: usize| chars.get(i).map(|(_, c)| *c);
    let mut i = from;
    let mut newline = false;
    // Horizontal whitespace (or a `--` comment, which runs to the newline)
    // before the first newline.
    while let Some(c) = at(i) {
        match c {
            ' ' | '\t' | '\u{c}' => i += 1,
            '-' if at(i + 1) == Some('-') => {
                while at(i).is_some_and(|c| c != '\n' && c != '\r') {
                    i += 1;
                }
            }
            '\n' | '\r' => {
                newline = true;
                i += 1;
                break;
            }
            _ => return None,
        }
    }
    if !newline {
        return None;
    }
    loop {
        match at(i) {
            Some(' ' | '\t' | '\n' | '\r' | '\u{c}') => i += 1,
            Some('-') if at(i + 1) == Some('-') => {
                while at(i).is_some_and(|c| c != '\n' && c != '\r') {
                    i += 1;
                }
                // The comment must end at a newline to continue.
                at(i)?;
            }
            Some('\'') => return Some(i),
            _ => return None,
        }
    }
}

fn skip_quoted(
    chars: &[(usize, char)],
    mut i: usize,
    quote: char,
    backslash_escapes: bool,
) -> usize {
    while i < chars.len() {
        let c = chars[i].1;
        if backslash_escapes && c == '\\' {
            i += 2;
        } else if c == quote {
            if chars.get(i + 1).map(|(_, c)| *c) == Some(quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    i
}

/// Length in chars of a dollar-quote opening tag (`$$` or `$tag$`) at `i`.
fn dollar_tag(chars: &[(usize, char)], i: usize) -> Option<usize> {
    if chars.get(i + 1).is_some_and(|(_, c)| c.is_ascii_digit()) {
        return None; // `$1` is a parameter, not a quote.
    }
    let mut j = i + 1;
    while j < chars.len() && chars[j].1 != '$' && ident_char(chars[j].1) {
        j += 1;
    }
    (chars.get(j).map(|(_, c)| *c) == Some('$')).then_some(j - i + 1)
}

fn skip_dollar_quoted(chars: &[(usize, char)], i: usize, tag_len: usize) -> usize {
    let tag: Vec<char> = chars[i..i + tag_len].iter().map(|(_, c)| *c).collect();
    let mut j = i + tag_len;
    while j + tag_len <= chars.len() {
        if chars[j..j + tag_len]
            .iter()
            .map(|(_, c)| *c)
            .eq(tag.iter().copied())
        {
            return j + tag_len;
        }
        j += 1;
    }
    chars.len()
}

fn has_top_level(tokens: &[Token], word: &str) -> bool {
    tokens.iter().any(|t| t.depth == 0 && t.word == word)
}

fn classify_tokens(tokens: &[Token]) -> StatementKind {
    let Some(first) = tokens.first() else {
        return StatementKind::Other;
    };
    match first.word.as_str() {
        "WITH" => {
            // The main statement follows the CTE list at depth 0.
            let main = tokens.iter().enumerate().skip(1).find(|(_, t)| {
                t.depth == 0
                    && matches!(
                        t.word.as_str(),
                        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE"
                    )
            });
            match main {
                Some((position, _)) => classify_tokens(&tokens[position..]),
                None => StatementKind::Select,
            }
        }
        "SELECT" | "VALUES" | "TABLE" => StatementKind::Select,
        "INSERT" | "REPLACE" => StatementKind::Insert,
        "UPDATE" => {
            if has_top_level(tokens, "WHERE") {
                StatementKind::Update
            } else {
                StatementKind::UpdateAll
            }
        }
        "DELETE" => {
            if has_top_level(tokens, "WHERE") {
                StatementKind::Delete
            } else {
                StatementKind::DeleteAll
            }
        }
        "MERGE" => StatementKind::Merge,
        "ALTER" => {
            if has_top_level(tokens, "DROP") {
                StatementKind::AlterDrop
            } else {
                StatementKind::Ddl
            }
        }
        "CREATE" | "COMMENT" | "RENAME" => StatementKind::Ddl,
        "DROP" => StatementKind::Drop,
        "TRUNCATE" => StatementKind::Truncate,
        "GRANT" | "REVOKE" => StatementKind::Grant,
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "END" | "ABORT" => {
            StatementKind::Transaction
        }
        "EXPLAIN" | "SHOW" | "DESCRIBE" | "DESC" => StatementKind::Explain,
        "SET" | "USE" | "RESET" => StatementKind::Session,
        "CALL" | "DO" | "EXECUTE" => StatementKind::Call,
        "COPY" | "LOAD" => StatementKind::Copy,
        "VACUUM" | "ANALYZE" | "OPTIMIZE" | "REINDEX" | "CLUSTER" | "REFRESH" | "CHECKPOINT" => {
            StatementKind::Maintenance
        }
        _ => StatementKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::Dialect::{Mysql, Postgres};
    use super::StatementKind::*;
    use super::*;

    #[test]
    fn basic_kinds() {
        for d in [Postgres, Mysql] {
            assert_eq!(classify("select 1", d), vec![Select]);
            assert_eq!(classify("INSERT INTO t VALUES (1)", d), vec![Insert]);
            assert_eq!(classify("update t set a = 1 where id = 2", d), vec![Update]);
            assert_eq!(classify("update t set a = 1", d), vec![UpdateAll]);
            assert_eq!(classify("delete from t where id = 1", d), vec![Delete]);
            assert_eq!(classify("DELETE FROM t", d), vec![DeleteAll]);
            assert_eq!(classify("drop table t", d), vec![Drop]);
            assert_eq!(classify("truncate t", d), vec![Truncate]);
            assert_eq!(classify("alter table t add column a int", d), vec![Ddl]);
            assert_eq!(classify("alter table t drop column a", d), vec![AlterDrop]);
            assert_eq!(classify("explain analyze select 1", d), vec![Explain]);
        }
        assert_eq!(
            classify("refresh materialized view concurrently mv", Postgres),
            vec![Maintenance]
        );
    }

    #[test]
    fn scripts_return_every_statement() {
        assert_eq!(
            classify("begin; delete from t where id = 1; commit;", Postgres),
            vec![Transaction, Delete, Transaction]
        );
        assert!(is_destructive("select 1; drop table t", Postgres));
        assert!(!is_destructive(
            "select 1; delete from t where id = 1",
            Mysql
        ));
    }

    #[test]
    fn hash_is_an_operator_in_postgres_and_a_comment_in_mysql() {
        // In PostgreSQL `#` is bitwise XOR: the DROP after it must be seen.
        assert_eq!(
            classify("select 1 # 2; drop table t", Postgres),
            vec![Select, Drop]
        );
        assert!(is_destructive("select 1 # 2; drop table t", Postgres));
        // In MySQL the rest of the line is a comment.
        assert_eq!(classify("select 1 # 2; drop table t", Mysql), vec![Select]);
    }

    #[test]
    fn backslash_escapes_follow_the_dialect() {
        // MySQL: \' escapes the quote, so the string continues past it.
        assert_eq!(
            classify(r"select 'it\'s; drop table t'", Mysql),
            vec![Select]
        );
        // PostgreSQL standard strings: \ is literal, the string ends at the second quote.
        assert_eq!(
            classify(r"select 'a\'; drop table t; select 'b'", Postgres),
            vec![Select, Drop, Select]
        );
        // PostgreSQL E'' strings do process backslash escapes.
        assert_eq!(
            classify(r"select E'it\'s; drop table t'", Postgres),
            vec![Select]
        );
    }

    #[test]
    fn where_inside_a_subquery_does_not_count() {
        assert_eq!(
            classify(
                "delete from t using (select id from u where x = 1) s",
                Postgres
            ),
            vec![DeleteAll]
        );
        assert_eq!(
            classify(
                "update t set a = (select max(b) from u where u.id = 1)",
                Mysql
            ),
            vec![UpdateAll]
        );
    }

    #[test]
    fn keywords_in_literals_and_comments_do_not_count() {
        for d in [Postgres, Mysql] {
            assert_eq!(
                classify("select 'drop table t; delete from x'", d),
                vec![Select]
            );
            assert_eq!(classify("-- drop table t\nselect 1", d), vec![Select]);
            assert_eq!(classify("/* drop table */ select 1", d), vec![Select]);
            assert_eq!(
                classify("delete from t where note = 'it''s; fine'", d),
                vec![Delete]
            );
        }
        assert_eq!(classify("# drop table t\nselect 1", Mysql), vec![Select]);
        assert_eq!(
            classify("/* drop /* nested */ table */ select 1", Postgres),
            vec![Select]
        );
        assert_eq!(
            classify("select \"where\" from t; select `drop` from u", Mysql),
            vec![Select, Select]
        );
    }

    #[test]
    fn dollar_quoted_bodies_are_skipped_in_postgres() {
        let sql = "create function f() returns void as $body$ begin delete from t; drop table x; end $body$ language plpgsql";
        assert_eq!(classify(sql, Postgres), vec![Ddl]);
        assert_eq!(
            classify("do $$ begin truncate t; end $$", Postgres),
            vec![Call]
        );
        assert_eq!(
            classify("select $1, $2 from t where a = $3", Postgres),
            vec![Select]
        );
    }

    #[test]
    fn ctes_classify_by_their_main_statement() {
        assert_eq!(
            classify("with x as (select 1) select * from x", Postgres),
            vec![Select]
        );
        assert_eq!(
            classify("with x as (select id from t) delete from t", Postgres),
            vec![DeleteAll]
        );
        assert_eq!(
            classify(
                "with x as (select id from t) delete from t where id in (select id from x)",
                Postgres
            ),
            vec![Delete]
        );
    }

    #[test]
    fn split_returns_trimmed_text_and_offsets() {
        let sql = "  select 1;\n-- note\n delete from t where id = 2 ;\n ; ";
        let spans = split_statements(sql, Postgres);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "select 1");
        assert_eq!(spans[0].offset, 2);
        assert_eq!(spans[1].text, "-- note\n delete from t where id = 2");
        assert_eq!(&sql[spans[1].offset..spans[1].offset + 7], "-- note");
        assert_eq!(spans[1].kind, Delete);
    }

    #[test]
    fn split_keeps_function_bodies_whole() {
        let sql = "create function f() returns int as $$ select 1; $$ language sql; select f();";
        let spans = split_statements(sql, Postgres);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].text.ends_with("language sql"));
        assert_eq!(spans[1].text, "select f()");
    }

    #[test]
    fn split_handles_multibyte_text() {
        let sql = "select 'café ☕'; select 'ü'";
        let spans = split_statements(sql, Mysql);
        assert_eq!(
            spans.iter().map(|s| s.text).collect::<Vec<_>>(),
            vec!["select 'café ☕'", "select 'ü'"]
        );
        assert_eq!(&sql[spans[1].offset..], "select 'ü'");
    }

    #[test]
    fn empty_and_comment_only_statements_are_dropped() {
        assert!(classify(" ;; \n -- only a comment\n ; ", Postgres).is_empty());
    }

    #[test]
    fn writes_flag() {
        assert!(!Select.writes());
        assert!(Insert.writes());
        assert!(Drop.writes());
        assert!(!Explain.writes());
    }

    #[test]
    fn read_only_violations_are_found() {
        for (sql, d) in [
            ("SET default_transaction_read_only = off", Postgres),
            (
                "set session default_transaction_read_only to 'false'",
                Postgres,
            ),
            ("RESET default_transaction_read_only", Postgres),
            ("RESET ALL", Postgres),
            ("discard all", Postgres),
            ("BEGIN READ WRITE", Postgres),
            (
                "start transaction isolation level serializable, read write",
                Postgres,
            ),
            (
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
                Postgres,
            ),
            (
                "select set_config('default_transaction_read_only', 'off', false)",
                Postgres,
            ),
            ("SET SESSION TRANSACTION READ WRITE", Mysql),
            ("START TRANSACTION READ WRITE", Mysql),
            ("SET @@SESSION.transaction_read_only = 0", Mysql),
            ("set tx_read_only = OFF", Mysql),
            ("select 1; SET TRANSACTION READ WRITE", Mysql),
            (
                "DO $$ BEGIN PERFORM set_config('default_transaction_read_only','off',false); COMMIT; INSERT INTO t VALUES (1); END $$",
                Postgres,
            ),
            (
                "do $body$ begin execute 'set default_' || 'transaction_read_only = off'; commit; end $body$",
                Postgres,
            ),
            (
                "DO 'BEGIN EXECUTE ''SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE''; END'",
                Postgres,
            ),
            ("DO LANGUAGE plpgsql $$ BEGIN ROLLBACK; END $$", Postgres),
            ("SET @@session.`transaction_read_only` = 0", Mysql),
            ("SET `tx_read_only` = OFF", Mysql),
            ("SET \"default_transaction_read_only\" = off", Postgres),
            ("SET SESSION \"transaction_read_only\" TO off", Postgres),
        ] {
            assert!(read_only_violation(sql, d).is_some(), "{sql}");
        }
    }

    #[test]
    fn harmless_statements_pass_the_read_only_check() {
        for (sql, d) in [
            ("select 'read write'", Postgres),
            ("BEGIN READ ONLY", Postgres),
            ("SET search_path = app", Postgres),
            ("SHOW transaction_read_only", Postgres),
            ("select read, write from t", Mysql),
            ("START TRANSACTION READ ONLY", Mysql),
            (
                "-- SET default_transaction_read_only = off\nselect 1",
                Postgres,
            ),
        ] {
            assert_eq!(read_only_violation(sql, d), None, "{sql}");
        }
    }

    #[test]
    fn violation_returns_the_offending_statement() {
        assert_eq!(
            read_only_violation("select 1; begin read write; select 2", Postgres),
            Some("begin read write")
        );
    }

    #[test]
    fn atomic_and_compound_bodies_stay_one_statement() {
        let pg = "CREATE PROCEDURE s.p(a int) LANGUAGE sql BEGIN ATOMIC INSERT INTO s.log VALUES (a); \
                  SELECT CASE WHEN a > 1 THEN 1 ELSE 2 END; INSERT INTO s.log VALUES (a + 1); END; select 1";
        let spans = split_statements(pg, Postgres);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert!(spans[0].text.ends_with("END"));
        assert_eq!(spans[1].text, "select 1");

        let pg_fn =
            "create function f() returns int language sql begin atomic select 1; end;\nselect 2";
        assert_eq!(split_statements(pg_fn, Postgres).len(), 2);

        let my = "CREATE PROCEDURE p() BEGIN\n  DECLARE x INT DEFAULT 0;\n  lbl: LOOP\n    SET x = x + 1;\n    IF x > 3 THEN LEAVE lbl; END IF;\n  END LOOP lbl;\n  CASE x WHEN 4 THEN SELECT 'four'; ELSE BEGIN SELECT CASE WHEN x THEN 1 END; END; END CASE;\n  WHILE x > 0 DO SET x = x - 1; END WHILE;\nEND;\nSELECT 1";
        let spans = split_statements(my, Mysql);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[1].text, "SELECT 1");

        let trigger = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = 1; SET NEW.b = 2; END; SELECT 2";
        assert_eq!(split_statements(trigger, Mysql).len(), 2);
        let event = "CREATE EVENT e ON SCHEDULE EVERY 1 HOUR DO BEGIN DELETE FROM t; INSERT INTO t VALUES (1); END; SELECT 3";
        assert_eq!(split_statements(event, Mysql).len(), 2);
        let anonymous = "BEGIN NOT ATOMIC DECLARE a INT; SET a = 1; END; SELECT 4";
        assert_eq!(split_statements(anonymous, Mysql).len(), 2);
    }

    #[test]
    fn transactions_are_not_bodies() {
        assert_eq!(
            split_statements("BEGIN; INSERT INTO t VALUES (1); COMMIT", Mysql).len(),
            3
        );
        assert_eq!(
            split_statements("BEGIN WORK; SELECT 1; END", Postgres).len(),
            3
        );
        assert_eq!(
            split_statements("begin; select case when true then 1 end; end;", Postgres).len(),
            3
        );
        assert_eq!(
            split_statements("CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END $$ LANGUAGE plpgsql; SELECT 1", Postgres).len(),
            2
        );
    }

    #[test]
    fn mysql_comment_rules_match_the_server() {
        // `--1` is not a comment in MySQL: the `;` ends a statement.
        assert_eq!(
            split_statements("SELECT 1 --1; DELETE FROM t", Mysql).len(),
            2
        );
        assert_eq!(
            split_statements("SELECT 1 -- 1; DELETE FROM t", Mysql).len(),
            1
        );
        assert_eq!(
            split_statements("SELECT 1 --\n; DELETE FROM t", Mysql).len(),
            2
        );
        assert_eq!(
            split_statements("SELECT 1 --1; DELETE FROM t", Postgres).len(),
            1
        );
        // Executable comments are code.
        let spans = split_statements("SELECT 1 /*! ; DELETE FROM t */", Mysql);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[1].kind, StatementKind::DeleteAll);
        assert_eq!(
            split_statements("SELECT 1 /*!50700 ; DROP TABLE t */", Mysql).len(),
            2
        );
        assert_eq!(
            split_statements("SELECT 1 /*M!100100 ; DROP TABLE t */", Mysql).len(),
            2
        );
        assert_eq!(
            split_statements("SELECT 1 /* ; DROP TABLE t */", Mysql).len(),
            1
        );
        assert!(is_destructive("SELECT 1 /*! ; DROP TABLE t */", Mysql));
    }

    #[test]
    fn strict_split_refuses_backslash_dependent_text() {
        let text = r"SELECT 'a\'; DELETE FROM t; SELECT '";
        assert!(split_statements_strict(text, Mysql).is_err());
        assert!(split_statements_strict(text, Postgres).is_ok());
        assert_eq!(
            split_statements_strict("SELECT 'it''s'; SELECT 2", Mysql).map(|s| s.len()),
            Ok(2)
        );
        assert_eq!(
            split_statements_strict(r"SELECT 'a\\b'", Mysql).map(|s| s.len()),
            Ok(1)
        );
    }

    #[test]
    fn lexer_follows_postgres_rules() {
        // `\r` ends a line comment, so the `;` ends the statement.
        assert_eq!(
            split_statements("SELECT 'x' --\r; DROP TABLE t", Postgres).len(),
            2
        );
        // Non-ASCII dollar tags quote.
        assert_eq!(
            split_statements(
                "SELECT $\u{20ac}$'$\u{20ac}$ AS a; DROP TABLE t; --'",
                Postgres
            )
            .len(),
            2
        );
        // Non-ASCII characters continue an identifier: `€$x$` is one word.
        let spans = split_statements("SELECT 1 AS \u{20ac}$x$; DROP TABLE t; --$x$", Postgres);
        assert_eq!(spans.len(), 2, "{spans:?}");
        // `€e'...'` is a name followed by a standard string, not an E-string.
        assert_eq!(
            split_statements("SELECT s.\u{20ac}e'\\'; DROP TABLE t; --'", Postgres).len(),
            2
        );
        let kinds: Vec<LexKind> = lex("SELECT $1, $$a$$, 1.5e3, \"q\"", Postgres, true)
            .iter()
            .map(|l| l.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                LexKind::Word,
                LexKind::Parameter,
                LexKind::Punct(','),
                LexKind::DollarString,
                LexKind::Punct(','),
                LexKind::Number,
                LexKind::Punct(','),
                LexKind::QuotedIdent
            ]
        );
    }

    #[test]
    fn lexer_offsets_cover_the_text() {
        let sql = "SELECT `a``b`, \"s\", 'it''s' /* c */ # x\n FROM t";
        let lexemes = lex(sql, Mysql, true);
        assert_eq!(&sql[lexemes[1].start..lexemes[1].end], "`a``b`");
        assert_eq!(lexemes[1].kind, LexKind::QuotedIdent);
        assert_eq!(lexemes[3].kind, LexKind::DoubleQuoted);
        assert_eq!(&sql[lexemes[5].start..lexemes[5].end], "'it''s'");
        assert_eq!(lexemes[6].kind, LexKind::Comment);
        assert_eq!(lexemes[7].kind, LexKind::Comment);
        assert_eq!(&sql[lexemes[8].start..lexemes[8].end], "FROM");
    }

    #[test]
    fn postgres_string_continuation_keeps_the_escape_mode() {
        // `E'a'` continued on the next line: the second part keeps escapes,
        // so `\'` does not end it and the `;` stays inside the literal.
        let text = "SELECT E'a'\n'b\\'; c'";
        assert_eq!(split_statements(text, Postgres).len(), 1);
        let text = "SELECT E'a' -- note\n  -- more\n'b\\'; c'";
        assert_eq!(split_statements(text, Postgres).len(), 1);
        // Without a newline it is not a continuation.
        assert_eq!(
            split_statements("SELECT 'a' 'b'; SELECT 2", Postgres).len(),
            2
        );
        // A standard string continued keeps standard rules.
        assert_eq!(
            split_statements("SELECT 'a'\n'b\\'; SELECT 2", Postgres).len(),
            2
        );
        // A block comment between parts ends the continuation.
        assert_eq!(
            split_statements("SELECT E'a'\n/* x */'b\\'; SELECT 2", Postgres).len(),
            2
        );
    }

    #[test]
    fn e_prefix_after_numbers_and_parameters() {
        // `1e'..'` and `$1e'..'` are escape strings on PostgreSQL 13 and 14.
        assert_eq!(split_statements("SELECT 1e'\\'; x'", Postgres).len(), 1);
        assert_eq!(split_statements("SELECT $1e'\\'; x'", Postgres).len(), 1);
        // `abc1e` is a name, so the string after it is standard.
        assert_eq!(
            split_statements("SELECT abc1e'\\'; SELECT 2", Postgres).len(),
            2
        );
        assert_eq!(
            split_statements("SELECT E'\\''; SELECT 2", Postgres).len(),
            2
        );
    }

    #[test]
    fn names_inside_atomic_bodies_are_not_block_words() {
        let text = "CREATE PROCEDURE p() LANGUAGE sql BEGIN ATOMIC SELECT t.end FROM s.t t; SELECT 1 AS case; END; SELECT 2";
        let spans = split_statements(text, Postgres);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[1].text, "SELECT 2");
    }

    fn texts(spans: &[StatementSpan<'_>]) -> Vec<String> {
        spans.iter().map(|s| s.text.to_owned()).collect()
    }

    fn mysql_split(sql: &str) -> Result<Vec<String>, AmbiguousSplit> {
        split_statements_mysql(sql, &MysqlMode::default()).map(|s| texts(&s))
    }

    #[test]
    fn routine_bodies_without_begin_stay_one_statement() -> Result<(), AmbiguousSplit> {
        // Audit probe P-M1: the body is a single IF statement.
        let spans = split_statements_mysql(
            "CREATE PROCEDURE pc_p() IF FALSE THEN DO 0; GRANT SELECT ON varve.* TO pc_u; END IF",
            &MysqlMode::default(),
        )?;
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(spans[0].kind, Ddl);
        // P-M2: the statements after it are counted right.
        assert_eq!(
            mysql_split(
                "CREATE PROCEDURE pc_p2() IF FALSE THEN DO 0; GRANT SELECT ON varve.* TO pc_u; \
                 END IF; SELECT 1; SELECT * FROM pc_nonexist"
            )?
            .len(),
            3
        );
        for (sql, count) in [
            (
                "CREATE FUNCTION f(a INT) RETURNS DOUBLE PRECISION DETERMINISTIC \
                 IF a > 0 THEN RETURN 1; ELSEIF a < 0 THEN RETURN -1; ELSE RETURN 0; END IF; SELECT 1",
                2,
            ),
            (
                "CREATE FUNCTION f() RETURNS VARCHAR(10) CHARSET utf8mb4 COLLATE utf8mb4_bin \
                 RETURN IF(1, 'a', 'b'); SELECT 1",
                2,
            ),
            (
                "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW FOLLOWS other \
                 IF NEW.a > 1 THEN SET NEW.b = 1; END IF; SELECT 1",
                2,
            ),
            (
                "CREATE EVENT e ON SCHEDULE EVERY 1 DAY DO \
                 WHILE @a > 0 DO SET @a = @a - 1; END WHILE; SELECT 1",
                2,
            ),
            (
                "ALTER EVENT e DO lbl: LOOP SET @a = 1; LEAVE lbl; END LOOP lbl; SELECT 1",
                2,
            ),
            (
                "CREATE PROCEDURE p() BEGIN \
                 DECLARE CONTINUE HANDLER FOR SQLSTATE VALUE '23000', NOT FOUND, 1062 \
                 BEGIN SET @x = 1; END; \
                 DECLARE EXIT HANDLER FOR SQLEXCEPTION IF @x THEN SET @y = 1; END IF; \
                 REPEAT SET @a = REPEAT('x', 2); UNTIL 1 END REPEAT; \
                 CASE WHEN @a THEN SET @b = CASE WHEN 1 THEN 2 ELSE 3 END; ELSE BEGIN END; END CASE; \
                 END; SELECT 1",
                2,
            ),
            (
                "CREATE DEFINER = 'u'@'%' PROCEDURE p() COMMENT 'x;y' SQL SECURITY INVOKER \
                 BEGIN SELECT 1; END; SELECT 2",
                2,
            ),
            (
                "CREATE DEFINER = CURRENT_USER() PROCEDURE IF NOT EXISTS db.p() \
                 `lbl`: LOOP LEAVE lbl; END LOOP lbl; SELECT 2",
                2,
            ),
            // A simple body ends at its semicolon.
            ("CREATE PROCEDURE p() SELECT 1; SELECT 2", 2),
            (
                "CREATE FUNCTION f RETURNS STRING SONAME 'x.so'; SELECT 2",
                2,
            ),
            // Names that are keywords elsewhere.
            (
                "CREATE TABLE begin (atomic int, `if` int, loop1 int); SELECT t.end FROM t",
                2,
            ),
            ("SELECT CASE WHEN 1 THEN 2 END; SELECT 3", 2),
            ("BEGIN; INSERT INTO t VALUES (1); COMMIT", 3),
        ] {
            assert_eq!(mysql_split(sql)?.len(), count, "{sql}");
        }
        Ok(())
    }

    #[test]
    fn end_and_start_are_names_outside_statement_position() -> Result<(), AmbiguousSplit> {
        // Audit round 2, R2-2.
        for (sql, count) in [
            (
                "CREATE PROCEDURE p14() BEGIN SELECT end FROM t7; \
                 GRANT INSERT ON db_a.* TO test_user_a; END; GRANT SELECT ON db_a.* TO test_user_a",
                2,
            ),
            (
                "CREATE PROCEDURE p() BEGIN UPDATE t SET end = 1, start = 2 WHERE end > 0; \
                 SELECT CASE WHEN end THEN end ELSE start END AS e FROM t ORDER BY end; END; SELECT 1",
                2,
            ),
            (
                "CREATE PROCEDURE p() BEGIN REPEAT SET @a = 1; UNTIL end > 1 END REPEAT; END; SELECT 1",
                2,
            ),
            (
                "CREATE PROCEDURE p() IF (SELECT end FROM t) THEN SELECT 1; END IF; SELECT 2",
                2,
            ),
            ("CREATE PROCEDURE p() BEGIN END; SELECT 2", 2),
        ] {
            assert_eq!(mysql_split(sql)?.len(), count, "{sql}");
        }
        // An END that no construct can be closed by.
        assert_eq!(mysql_split("END; SELECT 1"), Err(AmbiguousSplit));
        Ok(())
    }

    #[test]
    fn mariadb_anchored_return_types_are_headers() -> Result<(), AmbiguousSplit> {
        // Audit round 2, R2-3.
        for sql in [
            "CREATE FUNCTION f1() RETURNS TYPE OF tt.c BEGIN DECLARE x INT; RETURN x; END; SELECT 1",
            "CREATE FUNCTION f2() RETURNS TYPE OF db.tt.c IF 1 THEN RETURN 1; ELSE RETURN 2; END IF; SELECT 1",
            "CREATE FUNCTION f3() RETURNS ROW TYPE OF tt DETERMINISTIC BEGIN RETURN NULL; END; SELECT 1",
        ] {
            let spans = mysql_split(sql).inspect_err(|_| eprintln!("refused: {sql}"))?;
            assert_eq!(spans.len(), 2, "{sql}");
        }
        Ok(())
    }

    #[test]
    fn mariadb_compound_statements_stay_one_statement() -> Result<(), AmbiguousSplit> {
        // Audit probe P-M4.
        assert_eq!(
            mysql_split("IF 0 THEN DO 0; GRANT SELECT ON varve.* TO pc_u; END IF; SELECT 1")?,
            [
                "IF 0 THEN DO 0; GRANT SELECT ON varve.* TO pc_u; END IF",
                "SELECT 1"
            ]
        );
        for (sql, count) in [
            ("lbl: BEGIN NOT ATOMIC SELECT 1; END lbl; SELECT 2", 2),
            (
                "BEGIN NOT ATOMIC BEGIN NOT ATOMIC SELECT 1; END; END; SELECT 2",
                2,
            ),
            ("FOR i IN 1..3 DO SELECT i; END FOR; SELECT 2", 2),
            (
                "CASE WHEN 1 THEN SELECT 1; ELSE SELECT 2; END CASE; SELECT 3",
                2,
            ),
            (
                "WHILE 0 DO SELECT 1; END WHILE; REPEAT SELECT 1; UNTIL 1 END REPEAT; SELECT 3",
                3,
            ),
        ] {
            assert_eq!(mysql_split(sql)?.len(), count, "{sql}");
        }
        Ok(())
    }

    #[test]
    fn uncertain_mysql_boundaries_are_refused() {
        for sql in [
            "CREATE PACKAGE pkg PROCEDURE p(); END",
            "CREATE PROCEDURE p() BEGIN SELECT 1;",
            "CREATE PROCEDURE p() IF 1 THEN SELECT 1; END LOOP; SELECT 2",
        ] {
            assert_eq!(mysql_split(sql), Err(AmbiguousSplit), "{sql}");
            assert_eq!(
                split_statements_strict(sql, Mysql).map(|s| s.len()),
                Err(AmbiguousSplit)
            );
        }
        let oracle = MysqlMode::from_sql_mode("ORACLE,PIPES_AS_CONCAT", None);
        assert_eq!(
            split_statements_mysql("SELECT 1", &oracle).map(|s| s.len()),
            Err(AmbiguousSplit)
        );
    }

    #[test]
    fn the_session_sql_mode_decides_string_boundaries() -> Result<(), AmbiguousSplit> {
        // Audit probe P-N1.
        let sql = "SELECT 'a\\'' AS c; GRANT SELECT ON varve.* TO pc_u; SELECT '";
        let plain = MysqlMode::from_sql_mode("STRICT_TRANS_TABLES", None);
        let nbe = MysqlMode::from_sql_mode("STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES", None);
        assert!(plain.backslash_escapes && !nbe.backslash_escapes);
        assert_eq!(split_statements_mysql(sql, &plain)?.len(), 3);
        assert_eq!(split_statements_mysql(sql, &nbe)?.len(), 1);
        assert_eq!(
            split_statements_strict(sql, Mysql).map(|s| s.len()),
            Err(AmbiguousSplit)
        );
        // ANSI_QUOTES: a backslash is no escape in a quoted name.
        let sql = "SELECT \"a\\\"; GRANT SELECT ON varve.* TO pc_u; SELECT \"";
        let ansi = MysqlMode::from_sql_mode("ANSI", None);
        assert!(ansi.ansi_quotes);
        assert_eq!(split_statements_mysql(sql, &plain)?.len(), 1);
        assert_eq!(split_statements_mysql(sql, &ansi)?.len(), 3);
        assert_eq!(
            split_statements_strict(sql, Mysql).map(|s| s.len()),
            Err(AmbiguousSplit)
        );
        Ok(())
    }

    #[test]
    fn executable_comments_follow_the_server() -> Result<(), AmbiguousSplit> {
        let sql = "SELECT 1 /*!99999 ; GRANT SELECT ON t TO u */; SELECT 2";
        let mysql80 = MysqlMode {
            server: Some(MysqlServer::new(false, 8, 0, 36)),
            ..MysqlMode::default()
        };
        assert_eq!(MysqlServer::new(false, 8, 0, 36).version, 80036);
        assert_eq!(split_statements_mysql(sql, &mysql80)?.len(), 2);
        assert_eq!(split_statements_mysql(sql, &MysqlMode::default())?.len(), 3);
        let sql = "SELECT 1 /*M! ; GRANT SELECT ON t TO u */; SELECT 2";
        let mariadb = MysqlMode {
            server: Some(MysqlServer::new(true, 10, 6, 11)),
            ..MysqlMode::default()
        };
        assert_eq!(split_statements_mysql(sql, &mysql80)?.len(), 2);
        assert_eq!(split_statements_mysql(sql, &mariadb)?.len(), 3);
        let sql = "SELECT 1 /*!50003 ; SELECT 3 */; SELECT 2";
        assert_eq!(split_statements_mysql(sql, &mysql80)?.len(), 3);
        Ok(())
    }

    #[test]
    fn do_blocks_are_checked_by_their_code_only() {
        let refused = |sql: &str| read_only_violation(sql, Postgres).is_some();
        // Words in string literals, nested dollar quotes, quoted names of
        // other things and comments are harmless (catalog round 7, L2).
        for sql in [
            "DO $$ BEGIN PERFORM 'commit'; END $$",
            "DO $x$ BEGIN PERFORM $y$ rollback; set transaction_read_only = off $y$; END $x$",
            "DO $$ BEGIN -- commit later\n PERFORM 1; /* rollback */ END $$",
            "DO 'BEGIN PERFORM ''read write''; END'",
            "DO LANGUAGE plpgsql $$ BEGIN RAISE NOTICE 'COMMIT'; END $$",
            "DO $$ BEGIN PERFORM 1; END $$ LANGUAGE plpgsql",
            // Dynamic SQL whose text is known and harmless, and settings
            // that are not the read-only mode (the catalog's EXPLAIN store).
            "DO $$ BEGIN EXECUTE format('SET varve.explain_output = %L', 'SELECT ''commit'''); END $$",
            "DO $$ BEGIN EXECUTE pg_catalog.format('SET varve.x = %1$L', 'rollback') USING 1; END $$",
            "DO $$ BEGIN EXECUTE 'SELECT 1'; END $$",
            "DO $$ BEGIN PERFORM pg_catalog.set_config('varve.explain_output', 'read write', false); END $$",
            "DO $$ BEGIN SET LOCAL varve.x = 'commit'; END $$",
        ] {
            assert!(!refused(sql), "{sql}");
        }
        // Code that switches the mode, dynamic SQL, quoted function names,
        // other languages and what the lexer cannot close stay refused.
        for sql in [
            "DO $$ BEGIN COMMIT; END $$",
            "DO $$ BEGIN PERFORM set_config('default_transaction_read_only', 'off', false); END $$",
            "DO $$ BEGIN PERFORM \"set_config\"('transaction_read_only', 'off', false); END $$",
            "DO $$ BEGIN SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE; END $$",
            "DO $$ BEGIN EXECUTE 'COMMIT'; END $$",
            "DO $$ BEGIN EXECUTE $q$ROLLBACK$q$; END $$",
            "DO $$ BEGIN EXECUTE 'SET default_transaction_read_only = off'; END $$",
            "DO $$ BEGIN EXECUTE 'SET ' || 'x = 1'; END $$",
            "DO $$ DECLARE q text := 'COMMIT'; BEGIN EXECUTE q; END $$",
            "DO $$ BEGIN EXECUTE format('SET %I = %L', 'default_transaction_read_only', 'off'); END $$",
            "DO $$ BEGIN EXECUTE format('SET %s', 'transaction_read_only = off'); END $$",
            "DO $$ BEGIN EXECUTE format('SET transaction_read_only = %L', 'off'); END $$",
            "DO $$ BEGIN PERFORM set_config(name, 'off', false); END $$",
            "DO $$ BEGIN RESET ALL; END $$",
            "DO LANGUAGE plperl $$ spi_exec_query('commit') $$",
            "DO E'BEGIN PERFORM ''commit''; END'",
            "DO $$ BEGIN PERFORM 'unterminated; COMMIT; END $$",
        ] {
            assert!(refused(sql), "{sql}");
        }
    }

    #[test]
    fn executable_comment_versions_are_read_like_each_server() {
        // Measured with `SELECT /*!<digits> +1 */ 5` (audit round 2, R2-5):
        // `runs` is true where the server ran `+1` (a syntax error).
        let servers = [
            ("MySQL 8.0.46", MysqlServer::new(false, 8, 0, 46)),
            ("MySQL 8.4.11", MysqlServer::new(false, 8, 4, 11)),
            ("MySQL 9.7.2", MysqlServer::new(false, 9, 7, 2)),
            ("MariaDB 10.6.28", MysqlServer::new(true, 10, 6, 28)),
            ("MariaDB 12.3.3", MysqlServer::new(true, 12, 3, 3)),
        ];
        // (marker, digits, runs on each server in the order above)
        let cases: [(bool, &str, [bool; 5]); 21] = [
            (false, "100000", [true, false, false, true, true]),
            (false, "8000001", [true, true, true, false, false]),
            (false, "80000", [true, true, true, false, false]),
            (false, "50699", [true, true, true, true, true]),
            (false, "50700", [true, true, true, false, false]),
            (false, "57000", [true, true, true, false, false]),
            (false, "99999", [false, false, false, false, false]),
            (false, "1234", [true, true, true, true, true]),
            (false, "12345678", [true, true, true, false, false]),
            (false, "", [true, true, true, true, true]),
            (false, "100629", [true, false, false, false, true]),
            (false, "120303", [true, false, false, false, true]),
            (false, "120304", [true, false, false, false, false]),
            (false, "080411", [true, true, true, false, false]),
            (false, "080412", [true, false, true, false, false]),
            (true, "100000", [false, false, false, true, true]),
            (true, "50000", [false, false, false, true, true]),
            (true, "80000", [false, false, false, true, true]),
            (true, "999999", [false, false, false, false, false]),
            (true, "1000001", [false, false, false, true, true]),
            (true, "", [false, false, false, true, true]),
        ];
        for (mariadb_only, digits, runs) in cases {
            for ((name, server), expected) in servers.iter().zip(runs) {
                let marker = if mariadb_only { "/*M!" } else { "/*!" };
                let sql = format!("SELECT {marker}{digits} +1 */ 5");
                let mode = MysqlMode {
                    server: Some(*server),
                    ..MysqlMode::default()
                };
                let ran = lex_mysql(&sql, &mode)
                    .iter()
                    .any(|l| l.kind == LexKind::ExecutableCommentStart);
                assert_eq!(ran, expected, "{name}: {sql}");
            }
        }
    }

    #[test]
    fn begin_and_atomic_as_names_open_no_postgres_body() {
        // Audit probe P-A1.
        let spans = split_statements(
            "CREATE TEMP TABLE begin (atomic int); ROLLBACK; END; SELECT 1",
            Postgres,
        );
        assert_eq!(spans.len(), 4, "{spans:?}");
        let spans = split_statements(
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; END; SELECT 2",
            Postgres,
        );
        assert_eq!(spans.len(), 2, "{spans:?}");
    }
}
