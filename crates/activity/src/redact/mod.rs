//! Statement text privacy (design doc 9.3).
//!
//! [`prepare_statement_text`] is the only way statement text reaches the
//! activity log. The secret redactor always runs first, at every level:
//!
//! - SQL: `PASSWORD '...'` in `CREATE / ALTER USER | ROLE` (also `ENCRYPTED`,
//!   `WITH`, `VALID UNTIL`), `IDENTIFIED BY | AS | USING '...'`, `IDENTIFIED
//!   WITH plugin BY | AS '...'`, `SET PASSWORD [FOR ...] = '...'`,
//!   `PASSWORD('...')`, `REPLACE '...'`, replication `*_PASSWORD = '...'`.
//! - Redis: `AUTH [user] pass`, `AUTH2`, `HELLO ... AUTH user pass`,
//!   `ACL SETUSER name >pass <pass #hash !hash`, `CONFIG SET *pass* | *auth*
//!   value`, `SENTINEL SET ... auth-pass value`, `MIGRATE ... AUTH pass |
//!   AUTH2 user pass`.
//! - Everywhere: credentials in URLs (`postgres://user:pass@`, `mysql://`,
//!   `redis://:pass@`, ...) and in key/value strings (`password=...`).
//!
//! The secret is replaced with `[redacted]` and the rest is kept.
//!
//! Server error messages can echo statement text, so they go through
//! [`prepare_error_message`], which applies the same text levels.
//!
//! # Limits
//!
//! Text level `full` keeps comments and prose as written. There, an
//! unquoted value is taken for a password only where the syntax says so
//! (`PASSWORD x` in an account statement, also commented out, `IDENTIFIED
//! BY x`, `-p<x>` and `--password <x>` of a known client) or where it looks
//! like one: a digit, a symbol, mixed case, or 20 bytes or more. A short,
//! lowercase, digit-free word in prose is kept, as it cannot be told from
//! ordinary words: `-- the app password is hunter`, `-- password: hunter`,
//! `--password hunter` of an unknown tool, `COMMENT ... 'temp password
//! hunter'`. Quoted values, and any value at text level `normalized`, are
//! always removed.
//!
//! SQL with broken quoting (an unterminated literal, quoted identifier or
//! block comment, or a literal glued to a word, as in `'D'Angelo'`) is not
//! trusted from the statement holding the break on: that part is stored as
//! keywords only (`normalized`) or as [`REDACTED`] (`full`), and an error
//! message of the event is withheld.
//!
//! Routine bodies in other languages than SQL (PL/Python, PL/Perl,
//! PL/Tcl, PL/v8, PL/R, PL/sh) are read for their comments, their string
//! literals and the SQL strings passed to the database calls they make
//! (`plpy.execute("...")`). SQL built at run time is not seen (f-strings,
//! a query held in a variable, Tcl braces and Perl `q{}` / `qq{}` quoting,
//! PL/R's `pg.spi.exec`), nor secrets in code structures (a command line
//! given as a list of arguments, `os.environ['PGPASSWORD'] = ...`, bind
//! parameters). At text level `normalized`, an error message such a body
//! raises keeps the text it builds at run time (`"a" + "b"`, a template
//! with `${...}`) and strings in forms not read as literals (Perl `q{}`,
//! Tcl braces).
//!
//! Command lines in comments are recognised at the usual prompts and
//! positions; an unusual prompt (starship, oh-my-zsh, a zsh prompt showing
//! a directory name) or a numbered list keeps a short, lowercase,
//! digit-free `-p` value, and a line starting with an e-mail address or an
//! `@mention` may put the next word in command position (and redact a
//! `-p...` word after a client name there).
//!
//! The output of `COPY ... TO STDOUT` pasted after the statement is no
//! input to the server and is read as SQL: its values are kept at text
//! level `full`.

mod credentials;
mod fuzzy;
#[cfg(test)]
mod keyword_check;
mod keywords;
mod message;
mod message_words;
#[cfg(test)]
mod message_words_check;
mod redis;
mod sql;
mod sql_normalize;

use std::fmt;

use serde::{Deserialize, Serialize};
use varve_core::Engine;

pub(crate) use credentials::redact_credentials;
pub(crate) use message::prepare_error_message_with;
pub use message::{
    ERROR_MESSAGE_HIDDEN, ERROR_MESSAGE_OMITTED, ERROR_MESSAGE_WITHHELD, SYNTAX_ERROR_HIDDEN,
    prepare_error_message,
};

/// Replacement for a removed secret.
pub const REDACTED: &str = "[redacted]";

/// Lexical rules used to read statement text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlDialect {
    /// PostgreSQL.
    Postgres,
    /// MySQL and MariaDB.
    Mysql,
    /// Redis and Valkey commands.
    Redis,
}

impl SqlDialect {
    /// The dialect of an engine.
    #[must_use]
    pub fn for_engine(engine: Engine) -> Self {
        match engine {
            Engine::Postgres => SqlDialect::Postgres,
            Engine::Mysql | Engine::Mariadb => SqlDialect::Mysql,
            Engine::Redis => SqlDialect::Redis,
        }
    }

    /// Lowercase name, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SqlDialect::Postgres => "postgres",
            SqlDialect::Mysql => "mysql",
            SqlDialect::Redis => "redis",
        }
    }
}

impl From<Engine> for SqlDialect {
    fn from(engine: Engine) -> Self {
        SqlDialect::for_engine(engine)
    }
}

/// How much statement text the activity log keeps (design doc 9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextLevel {
    /// Statement kind, duration and row counts only.
    None,
    /// Text with literals replaced by `?` (Redis: arguments other than key
    /// names replaced). The default.
    #[default]
    Normalized,
    /// The full text, still passed through the secret redactor.
    Full,
}

impl TextLevel {
    /// Lowercase name, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TextLevel::None => "none",
            TextLevel::Normalized => "normalized",
            TextLevel::Full => "full",
        }
    }
}

impl fmt::Display for TextLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Removes passwords and credentials from `text`, keeping everything else.
///
/// Never panics into the caller: should the redactor fail on some input,
/// the whole text comes back as [`REDACTED`] (fail closed), and the failure
/// is logged without any of the text.
#[must_use]
pub fn redact_secrets(text: &str, dialect: SqlDialect) -> String {
    guarded("redact_secrets", || redact_secrets_tracked(text, dialect).0)
        .unwrap_or_else(|| REDACTED.to_string())
}

/// Runs a redactor so that a panic in it never reaches the caller: `None`
/// then, and the failure is logged with the name of the `part` only.
pub(crate) fn guarded<T>(part: &'static str, run: impl FnOnce() -> T) -> Option<T> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::recorder::injected_failure(part);
        run()
    }));
    match result {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::error!(part, "a redactor failed; none of its text is returned");
            None
        }
    }
}

/// Longest text the redactor reads, in bytes. Longer statement text is not
/// redacted in parts: only its kind and length are kept, at every level.
pub const MAX_REDACTED_BYTES: usize = 1 << 20;

/// Like [`redact_secrets`], and also returns the removed fragments, so the
/// same secrets can be removed from an error message that echoes them.
/// Text longer than [`MAX_REDACTED_BYTES`], or with SQL nested too deep in
/// strings, comes back as [`REDACTED`] as a whole.
pub(crate) fn redact_secrets_tracked(text: &str, dialect: SqlDialect) -> (String, Vec<String>) {
    let (redacted, removed) = redact_checked(text, dialect);
    (redacted.unwrap_or_else(|| REDACTED.to_string()), removed)
}

/// The redacted text, or `None` when the text is too long or nests too
/// deep to redact: it must then be hidden as a whole.
fn redact_checked(text: &str, dialect: SqlDialect) -> (Option<String>, Vec<String>) {
    if text.len() > MAX_REDACTED_BYTES {
        return (None, Vec::new());
    }
    let mut removed = Vec::new();
    let redacted = match dialect {
        SqlDialect::Postgres => sql::redact(text, sql::Flavor::Postgres, &mut removed),
        SqlDialect::Mysql => sql::redact(text, sql::Flavor::Mysql, &mut removed),
        SqlDialect::Redis => Some(redis::redact(text, &mut removed)),
    };
    (redacted, removed)
}

/// Replaces literals with `?`, strips comments and collapses whitespace
/// (SQL), or keeps only command names and key names (Redis). Does not
/// redact secrets on its own; use [`prepare_statement_text`].
fn normalize(text: &str, dialect: SqlDialect) -> String {
    match dialect {
        SqlDialect::Postgres => sql_normalize::normalize(text, sql::Flavor::Postgres),
        SqlDialect::Mysql => sql_normalize::normalize(text, sql::Flavor::Mysql),
        SqlDialect::Redis => redis::normalize(text),
    }
}

/// The statement text to store at `level`: `None` for [`TextLevel::None`],
/// otherwise the text after the secret redactor, normalized for
/// [`TextLevel::Normalized`]. Never panics into the caller: should a
/// redactor fail on some input, nothing of the text is returned (`None`),
/// and the failure is logged without any of the text.
#[must_use]
pub fn prepare_statement_text(text: &str, dialect: SqlDialect, level: TextLevel) -> Option<String> {
    guarded("prepare_statement_text", || {
        prepare_statement(text, dialect, level).text
    })
    .flatten()
}

/// What [`prepare_statement`] keeps of a statement, for the recorder.
#[derive(Debug, Default)]
pub(crate) struct PreparedStatement {
    /// The text to store, `None` at [`TextLevel::None`] or above the cap.
    pub(crate) text: Option<String>,
    /// The secrets the redactor removed, or `None` when (part of) the text
    /// could not be redacted and was hidden: an error message of the same
    /// event must then be hidden too, as nothing tells which of its words
    /// are secrets.
    pub(crate) secrets: Option<Vec<String>>,
}

/// True when a SQL statement raises its own error (`RAISE`, `SIGNAL`,
/// `RESIGNAL` as a statement, also inside a routine body, or an error call
/// of a script body such as `plpy.error`): the message is then its own
/// text, whatever SQLSTATE or error number it carries.
pub(crate) fn statement_raises(text: &str, dialect: SqlDialect) -> bool {
    if text.len() > MAX_REDACTED_BYTES {
        return false;
    }
    match dialect {
        SqlDialect::Postgres => sql::raises_own_error(text, sql::Flavor::Postgres),
        SqlDialect::Mysql => sql::raises_own_error(text, sql::Flavor::Mysql),
        SqlDialect::Redis => false,
    }
}

/// The literal values of a statement, which its normalized text shows as
/// `?` and an error message at [`TextLevel::Normalized`] must not show
/// either: SQL string and number literals (also in routine bodies), and
/// Redis arguments. `None` when they would cost too much to collect: the
/// message is then withheld.
pub(crate) fn statement_literals(text: &str, dialect: SqlDialect) -> Option<Vec<String>> {
    if text.len() > MAX_REDACTED_BYTES {
        return None;
    }
    match dialect {
        SqlDialect::Postgres => {
            // `COPY ... FROM stdin` data: every field is a value too.
            let mut literals = Vec::new();
            for section in sql::sections(text, sql::Flavor::Postgres) {
                match section {
                    sql::Section::Sql(range) => literals.extend(sql::statement_literals(
                        &text[range],
                        sql::Flavor::Postgres,
                    )?),
                    // The arguments of a meta-command (`\set name value`).
                    sql::Section::Meta { range, .. } => literals.extend(
                        text[range]
                            .split_whitespace()
                            .skip(1)
                            .map(|word| word.trim_matches(['\'', '"']).to_string()),
                    ),
                    sql::Section::CopyData { range, .. } => literals.extend(
                        text[range]
                            .split(['\t', '\n', ','])
                            .map(|field| field.trim_matches(['\r', '"']).to_string())
                            .filter(|field| !field.is_empty() && field != "\\N" && field != "\\."),
                    ),
                }
            }
            Some(literals)
        }
        SqlDialect::Mysql => sql::statement_literals(text, sql::Flavor::Mysql),
        SqlDialect::Redis => Some(redis::argument_values(text)),
    }
}

/// Like [`prepare_statement_text`], with what an error message of the same
/// event needs ([`PreparedStatement`]).
///
/// SQL whose quoting is broken (an unterminated literal, quoted identifier
/// or block comment, or a literal glued to a word as in `'D'Angelo'`) is
/// not trusted from the statement holding the break on: that part is
/// stored as keywords only at [`TextLevel::Normalized`] and as
/// [`REDACTED`] at [`TextLevel::Full`], since the lexer may read the rest
/// with data and code swapped.
pub(crate) fn prepare_statement(
    text: &str,
    dialect: SqlDialect,
    level: TextLevel,
) -> PreparedStatement {
    // Above the cap nothing is read: no text at any level.
    if text.len() > MAX_REDACTED_BYTES {
        return PreparedStatement::default();
    }
    if level == TextLevel::None {
        return PreparedStatement {
            secrets: Some(Vec::new()),
            ..PreparedStatement::default()
        };
    }
    let flavor = match dialect {
        SqlDialect::Postgres => Some(sql::Flavor::Postgres),
        SqlDialect::Mysql => Some(sql::Flavor::Mysql),
        SqlDialect::Redis => None,
    };
    let Some(flavor) = flavor else {
        return prepare_part(text, dialect, None, level);
    };
    let sections = sql::sections(text, flavor);
    if let [sql::Section::Sql(range)] = sections.as_slice()
        && range.len() == text.len()
    {
        return prepare_part(text, dialect, Some(flavor), level);
    }
    // A `pg_dump` script with `COPY ... FROM stdin` data: the SQL parts are
    // prepared one by one, the data rows are data.
    let mut stored = String::new();
    let mut secrets = Some(Vec::new());
    // The SQL part before was hidden from a broken statement on, `COPY`
    // head included: its data rows are hidden too, so the stored text
    // reads the same when it is redacted again.
    let mut head_hidden = false;
    // A meta-command line inside an open statement: psql sends the SQL
    // around it as one statement, so what the secret rules find in that
    // reading (`PASSWORD` on one line, the value after `\echo x` on the
    // next) is removed too, at the place it was found, before each part is
    // read on its own.
    let (joined, joined_hidden) = match level {
        TextLevel::Full => sql::joined_ranges(text, &sections, flavor),
        _ => (Vec::new(), Vec::new()),
    };
    // Both lists and the sections are walked in source order, each with
    // an index that only moves forward: linear in the script.
    // Merged as the splice would merge them, so their ends grow too.
    let joined = sql::merge_ranges(joined);
    let joined_hidden = sql::merge_ranges(joined_hidden);
    let mut next_joined = 0;
    let mut next_hidden = 0;
    if !joined_hidden.is_empty() {
        // Hidden from a break on: an error message is withheld.
        secrets = None;
    }
    for section in sections {
        let after_hidden = std::mem::take(&mut head_hidden);
        let part = match section {
            sql::Section::Sql(range) if text[range.clone()].trim().is_empty() => {
                // Only line breaks: kept as written at full.
                match level {
                    TextLevel::Normalized => String::new(),
                    _ => text[range].to_string(),
                }
            }
            sql::Section::Meta { range, .. } => {
                let line = &text[range];
                match level {
                    // The command word only: `\connect ?`.
                    TextLevel::Normalized => sql::meta_normalized(line),
                    _ => {
                        let mut removed = Vec::new();
                        let out = credentials::splice(line, sql::meta_ranges(line), &mut removed)
                            .into_owned();
                        if let Some(all) = &mut secrets {
                            all.extend(removed);
                        }
                        out
                    }
                }
            }
            sql::Section::Sql(range) => {
                let mut local = in_section(&joined, &mut next_joined, &range);
                let hidden = in_section(&joined_hidden, &mut next_hidden, &range);
                // Hidden up to its end, `COPY` head included: its data
                // rows are hidden too.
                let hidden_head = !hidden.is_empty();
                local.extend(hidden);
                let mut removed = Vec::new();
                let part = credentials::splice(&text[range], local, &mut removed);
                let prepared = prepare_part(&part, dialect, Some(flavor), level);
                if let Some(all) = &mut secrets {
                    all.extend(removed);
                }
                match (&mut secrets, prepared.secrets) {
                    (Some(all), Some(found)) => all.extend(found),
                    (_, found) => {
                        head_hidden = found.is_none();
                        secrets = None;
                    }
                }
                head_hidden |= hidden_head;
                prepared.text.unwrap_or_default()
            }
            sql::Section::CopyData {
                range,
                format,
                columns,
            } => {
                let data = &text[range];
                match level {
                    TextLevel::Normalized => "?".to_string(),
                    _ if after_hidden => {
                        // Hidden up to its `\.` line, which stays.
                        let body = ["\\.\r\n", "\\.\n", "\\."]
                            .iter()
                            .find_map(|end| {
                                data.strip_suffix(end)
                                    .filter(|body| body.is_empty() || body.ends_with('\n'))
                            })
                            .unwrap_or(data);
                        if body.trim().is_empty() {
                            data.to_string()
                        } else {
                            format!("{}{}", hidden_in_place(body), &data[body.len()..])
                        }
                    }
                    _ => {
                        // Every field is checked on its own and the ranges
                        // are spliced once, so the redaction markers never
                        // push the data past the size cap.
                        let mut removed = Vec::new();
                        let ranges = sql::copy_data_ranges(data, &format, &columns, flavor);
                        let out = credentials::splice(data, ranges, &mut removed).into_owned();
                        if let Some(all) = &mut secrets {
                            all.extend(removed);
                        }
                        out
                    }
                }
            }
        };
        if level == TextLevel::Normalized && !stored.is_empty() && !part.is_empty() {
            stored.push(' ');
        }
        stored.push_str(&part);
    }
    PreparedStatement {
        text: Some(stored),
        secrets,
    }
}

/// [`prepare_statement`] for one SQL part of a script (or the whole text,
/// or Redis text when `flavor` is `None`).
fn prepare_part(
    text: &str,
    dialect: SqlDialect,
    flavor: Option<sql::Flavor>,
    level: TextLevel,
) -> PreparedStatement {
    let cut = flavor
        .and_then(|flavor| sql::broken_quoting(text, flavor))
        .unwrap_or(text.len());
    let (head, rest) = text.split_at(cut);
    let (redacted, removed) = if head.trim().is_empty() {
        // Only white space: kept as written at full, so the line
        // structure (a `COPY` at the start of a line, a `\.` line) reads
        // the same when the stored text is redacted again.
        let kept = match level {
            TextLevel::Normalized => String::new(),
            _ => head.to_string(),
        };
        (Some(kept), Vec::new())
    } else {
        redact_checked(head, dialect)
    };
    let Some(redacted) = redacted else {
        // Too deep to redact: keywords only, or nothing of the text.
        let hidden = match (level, flavor) {
            (TextLevel::Normalized, Some(flavor)) => {
                sql_normalize::keywords_only_text(text, flavor)
            }
            (TextLevel::Normalized, None) => redis::normalize(text),
            _ => hidden_in_place(text),
        };
        return PreparedStatement {
            text: Some(hidden),
            ..PreparedStatement::default()
        };
    };
    let mut stored = match level {
        TextLevel::Normalized => normalize(&redacted, dialect),
        _ => redacted,
    };
    if rest.trim().is_empty() {
        return PreparedStatement {
            text: Some(stored),
            secrets: Some(removed),
        };
    }
    // Broken quoting from `cut` on: that part is hidden.
    let hidden = match (level, flavor) {
        (TextLevel::Normalized, Some(flavor)) => sql_normalize::keywords_only_text(rest, flavor),
        _ => hidden_in_place(rest),
    };
    if !stored.is_empty()
        && !stored.ends_with(char::is_whitespace)
        && !hidden.starts_with(char::is_whitespace)
    {
        stored.push(' ');
    }
    stored.push_str(&hidden);
    PreparedStatement {
        text: Some(stored),
        secrets: None,
    }
}

/// The parts of `ranges` (sorted by start) inside `section`, in its own
/// offsets. `next` is the first range that may still reach this section or
/// a later one; it only moves forward, as the sections come in source
/// order and the ranges never reach back.
fn in_section(
    ranges: &[(usize, usize)],
    next: &mut usize,
    section: &std::ops::Range<usize>,
) -> Vec<(usize, usize)> {
    while ranges
        .get(*next)
        .is_some_and(|&(_, end)| end <= section.start)
    {
        *next += 1;
    }
    ranges[*next..]
        .iter()
        .take_while(|&&(start, _)| start < section.end)
        .filter_map(|&(a, b)| {
            let (a, b) = (a.max(section.start), b.min(section.end));
            (a < b).then(|| (a - section.start, b - section.start))
        })
        .collect()
}

/// [`REDACTED`] in place of `part`, with the white space `part` starts and
/// ends with kept: the line breaks around a hidden part decide how a
/// script splits into SQL, meta-command lines and `COPY` data, so a second
/// pass over the stored text must see them where the first one did.
fn hidden_in_place(part: &str) -> String {
    let lead = &part[..part.len() - part.trim_start().len()];
    let trail = if lead.len() == part.len() {
        ""
    } else {
        &part[part.trim_end().len()..]
    };
    format!("{lead}{REDACTED}{trail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEVELS: [TextLevel; 3] = [TextLevel::None, TextLevel::Normalized, TextLevel::Full];

    /// Asserts that none of `secrets` survives at any level, that the full
    /// text shows `[redacted]`, and that redaction is idempotent.
    #[track_caller]
    fn assert_hidden(text: &str, dialect: SqlDialect, secrets: &[&str]) -> String {
        for level in LEVELS {
            if let Some(out) = prepare_statement_text(text, dialect, level) {
                for secret in secrets {
                    assert!(
                        !out.contains(secret),
                        "secret `{secret}` survived at {level} in {out:?} (input {text:?})"
                    );
                }
            }
        }
        let full = redact_secrets(text, dialect);
        assert!(full.contains(REDACTED), "no redaction in {full:?}");
        assert_eq!(redact_secrets(&full, dialect), full, "not idempotent");
        full
    }

    /// Runs `assert_hidden` on the text as written, in lower case and in
    /// upper case (the secret is matched case-insensitively then).
    #[track_caller]
    fn assert_hidden_any_case(text: &str, dialect: SqlDialect, secret: &str) {
        assert_hidden(text, dialect, &[secret]);
        assert_hidden(
            &text.to_lowercase(),
            dialect,
            &[secret.to_lowercase().as_str()],
        );
        assert_hidden(
            &text.to_uppercase(),
            dialect,
            &[secret.to_uppercase().as_str()],
        );
    }

    // ---- PostgreSQL ----------------------------------------------------

    #[test]
    fn postgres_role_passwords() {
        let pg = SqlDialect::Postgres;
        for text in [
            "CREATE USER alice PASSWORD 'Tr0ub4dor'",
            "CREATE USER alice WITH PASSWORD 'Tr0ub4dor'",
            "CREATE ROLE alice WITH LOGIN PASSWORD 'Tr0ub4dor' VALID UNTIL '2030-01-01'",
            "ALTER ROLE alice WITH PASSWORD 'Tr0ub4dor' VALID UNTIL 'infinity'",
            "ALTER USER alice ENCRYPTED PASSWORD 'Tr0ub4dor'",
            "ALTER USER alice WITH UNENCRYPTED PASSWORD 'Tr0ub4dor'",
            "CREATE ROLE alice LOGIN PASSWORD E'Tr0ub4dor'",
            "CREATE ROLE alice LOGIN PASSWORD $$Tr0ub4dor$$",
            "CREATE ROLE alice LOGIN PASSWORD $pw$Tr0ub4dor$pw$",
            "CREATE ROLE alice LOGIN PASSWORD U&'Tr0ub4dor'",
            "alter role \"Alice\" password 'Tr0ub4dor'",
            "ALTER ROLE alice PASSWORD 'Tr0ub4dor';",
            "ALTER ROLE alice PASSWORD /* new */ 'Tr0ub4dor'",
            "ALTER ROLE alice PASSWORD\n  'Tr0ub4dor'",
            // Invalid but logged as a failed query: still redacted.
            "ALTER ROLE alice PASSWORD \"Tr0ub4dor\"",
            "ALTER ROLE alice PASSWORD = 'Tr0ub4dor'",
            "ALTER ROLE alice PASSWORD = Tr0ub4dor",
            "ALTER ROLE alice PASSWORD 'Tr0ub4dor",
            // Unquoted: a syntax error, logged as a failed query.
            "ALTER ROLE alice PASSWORD Tr0ub4dor",
            "CREATE USER alice PASSWORD Tr0ub4dor LOGIN",
            "CREATE ROLE alice WITH LOGIN ENCRYPTED PASSWORD Tr0ub4dor",
            "ALTER USER IF EXISTS alice PASSWORD Tr0ub4dor",
            "SELECT 1; ALTER ROLE alice PASSWORD Tr0ub4dor",
            "ALTER ROLE password PASSWORD Tr0ub4dor",
        ] {
            assert_hidden_any_case(text, pg, "Tr0ub4dor");
        }
    }

    #[test]
    fn misaligned_config_set_and_mysql_dollar_words() {
        let r = SqlDialect::Redis;
        assert_eq!(
            redact_secrets("CONFIG SET \"requirepass hunter2xyz\"", r),
            "CONFIG SET \"[redacted]\""
        );
        assert_eq!(
            redact_secrets("CONFIG SET requirepass hunter2 xyz9Secret", r),
            "CONFIG SET requirepass [redacted] [redacted]"
        );
        let my = SqlDialect::Mysql;
        for text in [
            "SET PASSWORD = $ecretXyz9",
            "ALTER USER u IDENTIFIED BY $ecretXyz9",
            "GRANT ALL ON *.* TO u IDENTIFED BY P@ssw0rdXy",
        ] {
            let out = redact_secrets(text, my);
            assert!(
                !out.contains("ecretXyz9") && !out.contains("ssw0rdXy"),
                "{out}"
            );
        }
        assert_eq!(
            redact_secrets("SELECT $1::int", SqlDialect::Postgres),
            "SELECT $1::int"
        );
    }

    #[test]
    fn round_ten_patterns() {
        let pg = SqlDialect::Postgres;
        let my = SqlDialect::Mysql;
        // Only the value is redacted: the escaped line break and the next
        // key stay.
        assert_eq!(
            redact_secrets("SELECT E'password=Zq9w\\nhost=h'", pg),
            "SELECT E'password=[redacted]\\nhost=h'"
        );
        // Redacting under both escaping rules keeps the quotes balanced.
        assert_eq!(
            redact_secrets("ALTER ROLE r PASSWORD E'Zq9w\\''", pg),
            "ALTER ROLE r PASSWORD E'[redacted]'"
        );
        assert_eq!(
            redact_secrets("ALTER ROLE r PASSWORD 'Zq9w\\' , 'next'", pg),
            "ALTER ROLE r PASSWORD '[redacted]'[redacted]'next'"
        );
        // Comments: PASSWORD('x') and REPLACE 'current'.
        assert_eq!(
            redact_secrets(
                "SELECT 1 -- SET PASSWORD FOR app = PASSWORD('zqsecretword')",
                my
            ),
            "SELECT 1 -- SET PASSWORD FOR app = PASSWORD('[redacted]')"
        );
        assert_eq!(
            redact_secrets(
                "SELECT 1 -- ALTER USER app IDENTIFIED BY 'n' REPLACE 'zqsecretword'",
                my
            ),
            "SELECT 1 -- ALTER USER app IDENTIFIED BY '[redacted]' REPLACE '[redacted]'"
        );
        // Command lines: options of every form before `-p`.
        assert_eq!(
            redact_secrets(
                "SELECT 1 -- mysql -u 'my user' -h db --port 3306 appdb < x.sql -pzqsecretword",
                my
            ),
            "SELECT 1 -- mysql -u 'my user' -h db --port 3306 appdb < x.sql -p[redacted]"
        );
        assert_eq!(
            redact_secrets("SELECT 1 -- psql --password prompts for it", pg),
            "SELECT 1 -- psql --password prompts for it"
        );
        // Obfuscating escapes before a key are undone.
        for text in [
            "SELECT E'host=h\\x0apwd=zqsecretword'",
            "SELECT E'host=h\\012pwd=zqsecretword'",
            "SELECT E'host=h\\u000apwd=zqsecretword'",
            "SELECT '{\"\\u0070assword\": \"zqsecretword\"}'",
        ] {
            assert!(!redact_secrets(text, pg).contains("zqsecretword"), "{text}");
        }
        for text in [
            "SELECT 'host=h\\0pwd=zqsecretword'",
            "SELECT 'host=h\\Zpwd=zqsecretword'",
        ] {
            assert!(!redact_secrets(text, my).contains("zqsecretword"), "{text}");
        }
        // JSON keys of several words in a key position.
        for text in [
            "SELECT jsonb_build_object('the main db password', 'zqsecretword')",
            "SELECT '{\"Password Hash\": \"zqsecretword\"}'",
        ] {
            assert!(!redact_secrets(text, pg).contains("zqsecretword"), "{text}");
        }
    }

    #[test]
    fn round_seven_patterns() {
        let pg = SqlDialect::Postgres;
        let my = SqlDialect::Mysql;
        for (text, dialect) in [
            // M2: SET PASSWORD FOR in a comment, and a password split over
            // comments.
            ("SELECT 1 -- SET PASSWORD FOR u = 'xyzzysecret'", my),
            (
                "SELECT 1\n-- ALTER ROLE r PASSWORD\n/* 'xyzzysecret' */",
                pg,
            ),
            (
                "SELECT 1 /* ALTER ROLE r PASSWORD */ /* 'xyzzysecret' */",
                pg,
            ),
            ("SELECT 1 -- PASSWORD\n, 2 -- 'xyzzysecret'", pg),
            // L1: statements built with format() or ||.
            (
                "SELECT format('ALTER ROLE %I PASSWORD %L', 'r', 'xyzzysecret')",
                pg,
            ),
            (
                "SELECT 'ALTER ROLE r PASSWORD ' || quote_literal('xyzzysecret')",
                pg,
            ),
            // L2: quoted YAML values and unquoted values after an account
            // statement head or IDENTIFIED BY.
            (
                "INSERT INTO cfg VALUES (E'db:\\n  password: \"xyzzysecret\"\\n')",
                pg,
            ),
            ("SELECT 1 -- ALTER ROLE bob PASSWORD xyzzysecret", pg),
            ("SELECT 1 /* ALTER USER u IDENTIFIED BY xyzzysecret */", my),
            // L5: command lines and bare bearer tokens.
            ("SELECT 1 -- mysql -uroot -pxyzzysecret", my),
            ("SELECT 1 -- redis-cli -a xyzzysecret", pg),
            // `psql --password` takes no value (it asks for one); the
            // MySQL client takes the next word.
            ("SELECT 1 -- mysql -u root --password xyzzysecret", my),
            ("SELECT 'Bearer xyzzysecret.Zq9w'", pg),
        ] {
            let out = redact_secrets(text, dialect);
            assert!(!out.contains("xyzzysecret"), "{text} => {out}");
        }
        // L4: ordinary SQL stays as written.
        for (text, dialect) in [
            ("ALTER ROLE reporting SET search_path = app, public", pg),
            ("SET GLOBAL validate_password.length = 12", my),
            ("GET session:token:42", SqlDialect::Redis),
            ("SELECT 1 -- the bearer token is set by the proxy", pg),
        ] {
            assert_eq!(redact_secrets(text, dialect), text, "{text}");
        }
        // A secret setting is still redacted.
        assert_eq!(
            redact_secrets("ALTER ROLE alice SET app.password = 'Zq9wSecret'", pg),
            "ALTER ROLE alice SET app.password = '[redacted]'"
        );
    }

    #[test]
    fn text_over_the_cap_is_never_read() {
        let text = format!(
            "ALTER ROLE r PASSWORD 'Zq9wSecret'; SELECT '{}'",
            "x".repeat(MAX_REDACTED_BYTES)
        );
        for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Redis] {
            for level in LEVELS {
                assert_eq!(prepare_statement_text(&text, dialect, level), None);
            }
            assert_eq!(redact_secrets(&text, dialect), REDACTED);
        }
        let message = format!("near 'PASSWORD Zq9wSecret' {}", "x".repeat(70_000));
        assert_eq!(
            prepare_error_message(&message, None, Some(SqlDialect::Mysql), TextLevel::Full),
            ERROR_MESSAGE_OMITTED
        );
    }

    #[test]
    fn both_mysql_backslash_modes_are_read() {
        for text in [
            "SELECT AES_DECRYPT(x, 'a\\'), AES_DECRYPT(y, 'Zq9wSecret')",
            "SELECT 'C:\\', PASSWORD('Zq9wSecret')",
            "SELECT 'C:\\'; ALTER USER u IDENTIFIED BY 'Zq9wSecret'",
        ] {
            let out = redact_secrets(text, SqlDialect::Mysql);
            assert!(!out.contains("Zq9wSecret"), "{text} => {out}");
        }
    }

    #[test]
    fn comments_are_plain_text_and_line_comments_one_block() {
        let pg = SqlDialect::Postgres;
        for text in [
            "SELECT 1 -- don't use PASSWORD 'Zq9wSecret'",
            "/* it's: ALTER USER u IDENTIFIED BY 'Zq9wSecret' */ SELECT 1",
            "-- ALTER USER u IDENTIFIED BY\n-- 'Zq9wSecret'\nSELECT 1",
            "SELECT 1 -- password: Zq9wSecret",
            "SELECT 1 -- password Zq9wSecret",
            "/* SELECT pgp_sym_encrypt('d', E'Zq9w\\'Secret') */ SELECT 1",
        ] {
            let out = redact_secrets(text, pg);
            assert!(!out.contains("Zq9w"), "{text} => {out}");
        }
        for text in [
            "SELECT 1 -- don't forget the index",
            "-- password policy: min 12 chars\nSELECT 1",
            "-- ALTER USER docs: see wiki\nSELECT 1",
            "/* compute password reset stats */ SELECT 1",
        ] {
            assert_eq!(redact_secrets(text, pg), text);
        }
    }

    #[test]
    fn secret_names_are_whole_tokens() {
        let pg = SqlDialect::Postgres;
        for text in [
            "UPDATE posts SET is_secret = 1 WHERE id = 3",
            "UPDATE trips SET compass = 'N' WHERE id = 1",
            "UPDATE students SET pass = 1 WHERE score >= 50",
            "UPDATE models SET max_token = 4096 WHERE id = 1",
            "UPDATE flags SET bypass = false WHERE id = 1",
            "SELECT * FROM t WHERE note = 'Authorization: required for admins'",
            "SELECT pgp_sym_decrypt(data, current_setting('app.key')) FROM t",
            "SELECT DECODE(status, 1, 'on', 'off') FROM t",
        ] {
            assert_eq!(redact_secrets(text, pg), text, "{text}");
        }
        assert_eq!(
            redact_secrets("SELECT ENCRYPT('abc', 'ab')", SqlDialect::Mysql),
            "SELECT ENCRYPT('abc', 'ab')"
        );
        assert_eq!(
            redact_secrets(
                "INSERT INTO t VALUES ('{\"Authorization\": \"Bearer Zq9wSecret\"}')",
                pg
            ),
            "INSERT INTO t VALUES ('{\"Authorization\": \"Bearer [redacted]\"}')"
        );
    }

    #[test]
    fn nesting_beyond_the_limit_hides_the_text() {
        let depth = 40;
        let text = format!(
            "DO {}PASSWORD 'Zq9wSecret'{}",
            (0..depth).map(|i| format!("$t{i}$ ")).collect::<String>(),
            (0..depth)
                .rev()
                .map(|i| format!(" $t{i}$"))
                .collect::<String>()
        );
        let pg = SqlDialect::Postgres;
        assert_eq!(
            prepare_statement_text(&text, pg, TextLevel::Full).as_deref(),
            Some(REDACTED)
        );
        let normalized =
            prepare_statement_text(&text, pg, TextLevel::Normalized).unwrap_or_default();
        assert!(normalized.starts_with("DO ?"), "{normalized}");
        assert!(!normalized.contains("Zq9w"));
    }

    #[test]
    fn api_keys_and_headers_at_every_level() {
        let cases = [
            (
                "INSERT INTO hooks (url) VALUES ('https://h/x?api_key=k3yZq9w')",
                SqlDialect::Postgres,
                "INSERT INTO hooks (url) VALUES ('https://h/x?api_key=[redacted]')",
            ),
            (
                "INSERT INTO cfg VALUES ('{\"api_key\": \"k3yZq9w\"}')",
                SqlDialect::Mysql,
                "INSERT INTO cfg VALUES ('{\"api_key\": \"[redacted]\"}')",
            ),
            (
                "SELECT http_get('https://h', 'Authorization: Bearer k3yZq9w')",
                SqlDialect::Postgres,
                "SELECT http_get('https://h', 'Authorization: Bearer [redacted]')",
            ),
            (
                "SET hook \"X-Api-Key: k3yZq9w\"",
                SqlDialect::Redis,
                "SET hook \"X-Api-Key: [redacted]\"",
            ),
        ];
        for (text, dialect, full) in cases {
            assert_eq!(redact_secrets(text, dialect), full, "{text}");
            assert_eq!(
                prepare_statement_text(text, dialect, TextLevel::Full).as_deref(),
                Some(full)
            );
            assert_eq!(prepare_statement_text(text, dialect, TextLevel::None), None);
            let normalized =
                prepare_statement_text(text, dialect, TextLevel::Normalized).unwrap_or_default();
            assert!(!normalized.contains("k3yZq9w"), "{normalized}");
            assert!(!normalized.contains(REDACTED), "{normalized}");
        }
    }

    #[test]
    fn encryption_key_arguments() {
        let my = SqlDialect::Mysql;
        let pg = SqlDialect::Postgres;
        for (text, dialect, expected) in [
            (
                "SELECT AES_ENCRYPT(card, 'k3yZq9wSecret') FROM t",
                my,
                "SELECT AES_ENCRYPT(card, '[redacted]') FROM t",
            ),
            (
                "SELECT aes_decrypt(c, UNHEX('6b33795a71397753'), 'iv') FROM t",
                my,
                "SELECT aes_decrypt(c, UNHEX('[redacted]'), 'iv') FROM t",
            ),
            (
                "SELECT DES_ENCRYPT('x', \"k3yZq9wSecret\")",
                my,
                "SELECT DES_ENCRYPT('x', \"[redacted]\")",
            ),
            (
                "SELECT DECODE(c, 'k3yZq9wSecret') FROM t",
                my,
                "SELECT DECODE(c, '[redacted]') FROM t",
            ),
            (
                "SELECT pgp_sym_encrypt(note, 'k3yZq9wSecret', 'cipher-algo=aes256')",
                pg,
                "SELECT pgp_sym_encrypt(note, '[redacted]', 'cipher-algo=aes256')",
            ),
            (
                "SELECT pgp_sym_decrypt(note::bytea, 'k3yZq9wSecret') FROM t",
                pg,
                "SELECT pgp_sym_decrypt(note::bytea, '[redacted]') FROM t",
            ),
            (
                "SELECT encrypt('data', 'k3yZq9wSecret', 'aes')",
                pg,
                "SELECT encrypt('data', '[redacted]', 'aes')",
            ),
            (
                "SELECT decrypt_iv(d, 'k3yZq9wSecret', 'iv', 'aes')",
                pg,
                "SELECT decrypt_iv(d, '[redacted]', 'iv', 'aes')",
            ),
        ] {
            assert_eq!(redact_secrets(text, dialect), expected, "{text}");
        }
        // PostgreSQL decode(text, format) takes no key.
        assert_eq!(
            redact_secrets("SELECT decode('aGk=', 'base64')", pg),
            "SELECT decode('aGk=', 'base64')"
        );
    }

    #[test]
    fn sql_held_in_init_connect() {
        let my = SqlDialect::Mysql;
        for text in [
            "SET GLOBAL init_connect = 'ALTER USER CURRENT_USER() IDENTIFIED BY ''k3yZq9wSecret'''",
            "SET GLOBAL init_connect = 'SELECT 1 FROM t WHERE url = ''mysql://u:k3yZq9wSecret@h/db'''",
            "SET PERSIST init_replica = 'SET @x = ''host=h password=k3yZq9wSecret'''",
            "SET @@GLOBAL.init_slave = 'SET PASSWORD = ''k3yZq9wSecret'''",
        ] {
            let out = redact_secrets(text, my);
            assert!(!out.contains("k3yZq9wSecret"), "{text} => {out}");
            assert!(out.contains(REDACTED), "{out}");
        }
        assert_eq!(
            redact_secrets("SET GLOBAL init_connect = 'SET NAMES utf8mb4'", my),
            "SET GLOBAL init_connect = 'SET NAMES utf8mb4'"
        );
    }

    #[test]
    fn secret_patterns_apply_inside_comments() {
        for (text, dialect) in [
            (
                "SELECT 1 -- ALTER ROLE app PASSWORD 'k3yZq9wSecret'",
                SqlDialect::Postgres,
            ),
            (
                "SELECT 1 /* CREATE USER u IDENTIFIED BY 'k3yZq9wSecret' */",
                SqlDialect::Mysql,
            ),
            (
                "SELECT 1 # SET PASSWORD = 'k3yZq9wSecret'",
                SqlDialect::Mysql,
            ),
        ] {
            let out = redact_secrets(text, dialect);
            assert!(!out.contains("k3yZq9wSecret"), "{text} => {out}");
        }
        assert_eq!(
            redact_secrets(
                "SELECT 1 -- ALTER ROLE app PASSWORD 'k3yZq9wSecret'",
                SqlDialect::Postgres
            ),
            "SELECT 1 -- ALTER ROLE app PASSWORD '[redacted]'"
        );
        // Ordinary comments stay as written.
        assert_eq!(
            redact_secrets("SELECT 1 -- reset the password later", SqlDialect::Postgres),
            "SELECT 1 -- reset the password later"
        );
    }

    #[test]
    fn copy_programs_passphrase_commands_and_null_bulk_strings() {
        assert_eq!(
            redact_secrets(
                "COPY t FROM PROGRAM 'curl -u u:hunter2xyz http://x'",
                SqlDialect::Postgres
            ),
            "COPY t FROM PROGRAM '[redacted]'"
        );
        assert_eq!(
            redact_secrets(
                "ALTER SYSTEM SET ssl_passphrase_command = 'echo hunter2xyz'",
                SqlDialect::Postgres
            ),
            "ALTER SYSTEM SET ssl_passphrase_command = '[redacted]'"
        );
        let resp = "*3\r\n$4\r\nAUTH\r\n$-1\r\n$10\r\nhunter2xyz\r\n";
        assert!(!redact_secrets(resp, SqlDialect::Redis).contains("hunter2xyz"));
    }

    #[test]
    fn binary_casts_and_copy_passwords() {
        assert_eq!(
            redact_secrets(
                "SELECT * FROM users WHERE password = BINARY 'Zq9wSecret'",
                SqlDialect::Mysql
            ),
            "SELECT * FROM users WHERE password = BINARY '[redacted]'"
        );
        assert_eq!(
            redact_secrets(
                "COPY t FROM STDIN WITH PASSWORD hunter2xyz",
                SqlDialect::Postgres
            ),
            "COPY t FROM STDIN WITH PASSWORD [redacted]"
        );
    }

    #[test]
    fn unquoted_role_passwords_keep_the_rest() {
        let pg = SqlDialect::Postgres;
        assert_eq!(
            redact_secrets("CREATE USER alice PASSWORD hunter2 LOGIN", pg),
            "CREATE USER alice PASSWORD [redacted] LOGIN"
        );
        for kept in [
            "ALTER ROLE alice PASSWORD NULL",
            "ALTER ROLE alice NOLOGIN PASSWORD NULL VALID UNTIL 'infinity'",
            // A role called `password` is a name, not the keyword.
            "ALTER ROLE password RENAME TO pw",
            "CREATE ROLE password LOGIN",
            // Outside role statements a bare word after `password` is code.
            "SELECT password FROM users",
            "SELECT password AS pw FROM users WHERE password IS NULL",
            "UPDATE users SET password = NULL",
        ] {
            assert_eq!(redact_secrets(kept, pg), kept);
        }
        // MySQL role statements read the same way. A misspelled keyword is
        // an unknown word, so it is redacted along with the password.
        let my = SqlDialect::Mysql;
        assert_eq!(
            redact_secrets("CREATE USER u IDENTIFED BY 'hunter2'", my),
            "CREATE USER u [redacted] BY '[redacted]'"
        );
        for kept in [
            "ALTER USER bob PASSWORD EXPIRE NEVER",
            "ALTER USER bob PASSWORD HISTORY DEFAULT PASSWORD REUSE INTERVAL 30 DAY",
            "ALTER USER bob PASSWORD REQUIRE CURRENT OPTIONAL",
            "CREATE USER bob IDENTIFIED WITH caching_sha2_password",
            "SELECT a FROM t ORDER BY 'x'",
        ] {
            assert_eq!(redact_secrets(kept, my), kept);
        }
    }

    #[test]
    fn doubled_quotes_inside_a_connection_string_literal() {
        let full = assert_hidden(
            "CREATE SUBSCRIPTION s CONNECTION 'host=h password=ab''cdef' PUBLICATION p",
            SqlDialect::Postgres,
            &["ab", "cdef"],
        );
        assert_eq!(
            full,
            "CREATE SUBSCRIPTION s CONNECTION 'host=h password=[redacted]' PUBLICATION p"
        );
        let full = assert_hidden(
            "SELECT dblink_connect('host=h password=ab''cdef dbname=x')",
            SqlDialect::Postgres,
            &["cdef"],
        );
        assert_eq!(
            full,
            "SELECT dblink_connect('host=h password=[redacted] dbname=x')"
        );
        // MySQL: doubled and backslash-escaped quotes.
        for text in [
            "SELECT 'host=h password=ab''cdef'",
            "SELECT 'host=h password=ab\\'cdef'",
            "SELECT \"host=h password=ab\"\"cdef\"",
        ] {
            assert_hidden(text, SqlDialect::Mysql, &["cdef"]);
        }
        // A libpq-quoted value inside the literal.
        assert_hidden(
            "SELECT dblink_connect('host=h password=''ab\\''cdef'' dbname=x')",
            SqlDialect::Postgres,
            &["cdef"],
        );
    }

    #[test]
    fn postgres_escaped_quotes_in_passwords() {
        let pg = SqlDialect::Postgres;
        assert_hidden(
            "ALTER ROLE a PASSWORD 'it''s-Tr0ub4dor''x'",
            pg,
            &["Tr0ub4dor", "it", "''x"],
        );
        assert_hidden(
            "ALTER ROLE a PASSWORD E'it\\'s-Tr0ub4dor\\'x'",
            pg,
            &["Tr0ub4dor", "\\'x"],
        );
        // With standard_conforming_strings off, `\'` escapes: the whole
        // password must still disappear.
        assert_hidden(
            "ALTER ROLE a PASSWORD 'ab\\'Tr0ub4dor' LOGIN",
            pg,
            &["Tr0ub4dor"],
        );
        assert_hidden(
            "ALTER ROLE a PASSWORD $q$it's $$ Tr0ub4dor$q$",
            pg,
            &["Tr0ub4dor", "it's"],
        );
    }

    #[test]
    fn postgres_keeps_everything_but_the_password() {
        let full = redact_secrets(
            "ALTER ROLE alice WITH PASSWORD 'Tr0ub4dor' VALID UNTIL '2030-01-01'",
            SqlDialect::Postgres,
        );
        assert_eq!(
            full,
            "ALTER ROLE alice WITH PASSWORD '[redacted]' VALID UNTIL '2030-01-01'"
        );
        assert_eq!(
            redact_secrets("ALTER ROLE alice PASSWORD NULL", SqlDialect::Postgres),
            "ALTER ROLE alice PASSWORD NULL"
        );
        assert_eq!(
            redact_secrets(
                "SELECT password, email FROM users WHERE id = 1",
                SqlDialect::Postgres
            ),
            "SELECT password, email FROM users WHERE id = 1"
        );
        assert_eq!(
            redact_secrets("SELECT password FROM users", SqlDialect::Postgres),
            "SELECT password FROM users"
        );
    }

    #[test]
    fn postgres_password_functions_and_options() {
        let pg = SqlDialect::Postgres;
        assert_hidden(
            "UPDATE users SET password = crypt('Tr0ub4dor', gen_salt('bf'))",
            pg,
            &["Tr0ub4dor"],
        );
        assert_hidden(
            "CREATE USER MAPPING FOR bob SERVER s OPTIONS (user 'bob', password 'Tr0ub4dor')",
            pg,
            &["Tr0ub4dor"],
        );
        let full = assert_hidden(
            "CREATE SUBSCRIPTION s CONNECTION 'host=db user=rep password=Tr0ub4dor dbname=app' PUBLICATION p",
            pg,
            &["Tr0ub4dor"],
        );
        assert_eq!(
            full,
            "CREATE SUBSCRIPTION s CONNECTION 'host=db user=rep password=[redacted] dbname=app' PUBLICATION p"
        );
        assert_hidden(
            "SELECT dblink_connect('host=db password=''Tr0ub 4dor'' dbname=app')",
            pg,
            &["Tr0ub", "4dor"],
        );
        assert_hidden(
            "SELECT dblink_connect('postgresql://rep:Tr0ub4dor@db:5432/app')",
            pg,
            &["Tr0ub4dor"],
        );
    }

    // ---- MySQL / MariaDB -----------------------------------------------

    #[test]
    fn mysql_account_passwords() {
        let my = SqlDialect::Mysql;
        for text in [
            "CREATE USER 'bob'@'%' IDENTIFIED BY 'Tr0ub4dor'",
            "CREATE USER bob@localhost IDENTIFIED BY \"Tr0ub4dor\"",
            "ALTER USER 'bob'@'%' IDENTIFIED BY 'Tr0ub4dor'",
            "ALTER USER bob IDENTIFIED WITH caching_sha2_password BY 'Tr0ub4dor'",
            "ALTER USER bob IDENTIFIED WITH 'mysql_native_password' BY 'Tr0ub4dor'",
            "CREATE USER bob IDENTIFIED WITH mysql_native_password AS '*Tr0ub4dor'",
            "CREATE USER bob IDENTIFIED BY PASSWORD '*Tr0ub4dor'",
            "ALTER USER bob IDENTIFIED BY 'x' REPLACE 'Tr0ub4dor'",
            "GRANT ALL ON db.* TO bob@'%' IDENTIFIED BY 'Tr0ub4dor'",
            "SET PASSWORD = 'Tr0ub4dor'",
            "SET PASSWORD FOR 'bob'@'%' = 'Tr0ub4dor'",
            "SET PASSWORD FOR bob = PASSWORD('Tr0ub4dor')",
            "SET PASSWORD = PASSWORD(\"Tr0ub4dor\")",
            "SELECT PASSWORD('Tr0ub4dor')",
            "CREATE USER bob IDENTIFIED VIA ed25519 USING PASSWORD('Tr0ub4dor')",
            "CREATE USER bob IDENTIFIED VIA mysql_native_password USING '*Tr0ub4dor' OR unix_socket",
            "CREATE USER bob IDENTIFIED BY 'a' AND IDENTIFIED BY 'Tr0ub4dor'",
            "CHANGE REPLICATION SOURCE TO SOURCE_USER='rep', SOURCE_PASSWORD='Tr0ub4dor'",
            "CHANGE MASTER TO MASTER_PASSWORD = 'Tr0ub4dor'",
            "/*!80000 ALTER USER bob IDENTIFIED BY 'Tr0ub4dor' */",
            "ALTER USER bob IDENTIFIED BY Tr0ub4dor",
            "ALTER USER bob IDENTIFIED BY 'Tr0ub4dor",
        ] {
            assert_hidden_any_case(text, my, "Tr0ub4dor");
        }
    }

    #[test]
    fn mysql_escaped_quotes_in_passwords() {
        let my = SqlDialect::Mysql;
        assert_hidden(
            "ALTER USER bob IDENTIFIED BY 'it\\'s-Tr0ub4dor\\'x'",
            my,
            &["Tr0ub4dor", "\\'x"],
        );
        assert_hidden(
            "ALTER USER bob IDENTIFIED BY 'it''s-Tr0ub4dor''x'",
            my,
            &["Tr0ub4dor", "''x"],
        );
        assert_hidden(
            "ALTER USER bob IDENTIFIED BY \"say \\\"Tr0ub4dor\\\" \"\"x\"\"\"",
            my,
            &["Tr0ub4dor", "\"\"x"],
        );
        // A trailing backslash: under NO_BACKSLASH_ESCAPES the literal ends
        // at the first quote, otherwise later. Both ends are covered.
        assert_hidden(
            "ALTER USER bob IDENTIFIED BY 'ab\\' , Tr0ub4dor'",
            my,
            &["Tr0ub4dor"],
        );
    }

    #[test]
    fn mysql_keeps_everything_but_the_password() {
        let my = SqlDialect::Mysql;
        assert_eq!(
            redact_secrets(
                "ALTER USER 'bob'@'%' IDENTIFIED WITH caching_sha2_password BY 'Tr0ub4dor' PASSWORD EXPIRE INTERVAL 90 DAY",
                my
            ),
            "ALTER USER 'bob'@'%' IDENTIFIED WITH caching_sha2_password BY '[redacted]' PASSWORD EXPIRE INTERVAL 90 DAY"
        );
        assert_eq!(
            redact_secrets(
                "CREATE USER bob IDENTIFIED BY RANDOM PASSWORD; SELECT 'x'",
                my
            ),
            "CREATE USER bob IDENTIFIED BY RANDOM PASSWORD; SELECT 'x'"
        );
        assert_eq!(
            redact_secrets("SET PASSWORD FOR 'bob'@'%' = 'Tr0ub4dor'", my),
            "SET PASSWORD FOR 'bob'@'%' = '[redacted]'"
        );
        // `BY` after the statement that had IDENTIFIED is not affected.
        assert_eq!(
            redact_secrets(
                "CREATE USER b IDENTIFIED BY 'p'; SELECT a FROM t GROUP BY 'k'",
                my
            ),
            "CREATE USER b IDENTIFIED BY '[redacted]'; SELECT a FROM t GROUP BY 'k'"
        );
    }

    // ---- Connection strings ----------------------------------------------

    #[test]
    fn connection_urls_in_any_dialect() {
        for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Redis] {
            for text in [
                "postgres://alice:Tr0ub4dor@db.example.com:5432/app",
                "postgresql://alice:Tr0ub4dor@db/app?sslmode=verify-full",
                "mysql://root:Tr0ub4dor@127.0.0.1:3306/shop",
                "mariadb://root:Tr0ub4dor@localhost/shop",
                "redis://:Tr0ub4dor@cache:6379/0",
                "rediss://default:Tr0ub4dor@cache:6380",
                "host=db port=5432 user=alice password=Tr0ub4dor dbname=app",
                "host=db password='Tr0ub 4dor' dbname=app",
            ] {
                let secret = if text.contains("Tr0ub 4dor") {
                    "4dor"
                } else {
                    "Tr0ub4dor"
                };
                assert_hidden_any_case(text, dialect, secret);
            }
        }
    }

    #[test]
    fn connection_urls_inside_literals_and_comments() {
        assert_hidden(
            "SELECT * FROM dblink('postgres://u:Tr0ub4dor@h/db', 'select 1') AS t(a int)",
            SqlDialect::Postgres,
            &["Tr0ub4dor"],
        );
        assert_hidden(
            "SELECT 1 -- copied from mysql://root:Tr0ub4dor@db/shop",
            SqlDialect::Mysql,
            &["Tr0ub4dor"],
        );
        assert_hidden(
            "INSERT INTO cfg VALUES ('{\"url\":\"redis://:Tr0ub4dor@c:6379\"}')",
            SqlDialect::Postgres,
            &["Tr0ub4dor"],
        );
        assert_hidden(
            "SET k \"redis://default:Tr0ub4dor@cache:6379\"",
            SqlDialect::Redis,
            &["Tr0ub4dor"],
        );
        assert_eq!(
            redact_secrets("postgres://u:Tr0ub4dor@h/db", SqlDialect::Postgres),
            "postgres://u:[redacted]@h/db"
        );
    }

    // ---- Redis -----------------------------------------------------------

    #[test]
    fn redis_auth() {
        let r = SqlDialect::Redis;
        for text in [
            "AUTH Tr0ub4dor",
            "AUTH alice Tr0ub4dor",
            "AUTH \"Tr0ub4dor\"",
            "AUTH 'Tr0ub4dor'",
            "AUTH alice \"Tr0ub \\\"4dor\\\"\"",
            "AUTH alice 'it\\'s Tr0ub4dor'",
            "AUTH2 alice Tr0ub4dor",
            "HELLO 3 AUTH alice Tr0ub4dor",
            "HELLO 3 AUTH alice Tr0ub4dor SETNAME console",
            "HELLO 3 AUTH Tr0ub4dor SETNAME console",
            "PING\nAUTH Tr0ub4dor\nGET k",
            "AUTH \"Tr0ub4dor",
        ] {
            assert_hidden_any_case(text, r, "Tr0ub");
        }
        assert_eq!(
            redact_secrets("AUTH alice Tr0ub4dor", r),
            "AUTH alice [redacted]"
        );
        assert_eq!(redact_secrets("AUTH\0Tr0ub4dor", r), "AUTH\0[redacted]");
        assert_eq!(
            redact_secrets("hello 3 auth alice \"Tr0ub4dor\" setname c", r),
            "hello 3 auth alice \"[redacted]\" setname c"
        );
    }

    /// The redactor reads lines and arguments exactly as the driver does.
    #[test]
    fn redis_splits_like_the_driver() {
        let r = SqlDialect::Redis;
        for text in [
            // `\0` separates arguments in `redis-cli` and in the driver.
            "AUTH\0Tr0ub4dor",
            "AUTH\0alice\0Tr0ub4dor",
            "ACL\0SETUSER alice >Tr0ub4dor",
            "CONFIG SET\0requirepass\0Tr0ub4dor",
            // A quote never runs past the end of its line.
            "SET k \"unterminated\nAUTH Tr0ub4dor",
            "SET k 'unterminated\nAUTH alice Tr0ub4dor",
            // Quotes that start in the middle of an argument.
            "AUTH alice Tr0ub\"4 dor\"",
            "AUTH alice ab\"Tr0ub 4dor\"x",
            "AUTH alice \"Tr0ub\"4dor",
            "AUTH \"Tr0ub\"'4dor'",
            "ACL SETUSER alice on >Tr0ub\"4 dor\"",
            // Windows line endings.
            "PING\r\nAUTH Tr0ub4dor\r\n",
        ] {
            let secrets: &[&str] = if text.contains("4 dor") {
                &["Tr0ub", "dor"]
            } else {
                &["Tr0ub", "4dor"]
            };
            assert_hidden(text, r, secrets);
        }
        assert_eq!(
            redact_secrets("SET k \"unterminated\nAUTH Tr0ub4dor", r),
            "SET k \"unterminated\nAUTH [redacted]"
        );
        assert_eq!(
            redact_secrets("AUTH alice ab\"Tr0ub 4dor\"x KEEP", r),
            "AUTH [redacted] [redacted] [redacted]"
        );
    }

    #[test]
    fn redis_comment_lines_are_redacted_and_not_normalized() {
        let r = SqlDialect::Redis;
        for text in [
            "# AUTH Tr0ub4dor\nGET k",
            "  #AUTH alice Tr0ub4dor",
            "#CONFIG SET requirepass Tr0ub4dor",
            "# old: redis://:Tr0ub4dor@cache:6379",
        ] {
            assert_hidden(text, r, &["Tr0ub4dor"]);
        }
        assert_eq!(
            redact_secrets("# AUTH Tr0ub4dor\nGET k", r),
            "# AUTH [redacted]\nGET k"
        );
        assert_eq!(
            prepare_statement_text("# AUTH Tr0ub4dor\nGET k", r, TextLevel::Normalized).as_deref(),
            Some("GET k")
        );
    }

    #[test]
    fn redis_acl_setuser() {
        let r = SqlDialect::Redis;
        for text in [
            "ACL SETUSER alice on >Tr0ub4dor ~cached:* +get",
            "ACL SETUSER alice on \">Tr0ub 4dor\" +@all",
            "ACL SETUSER alice <Tr0ub4dor",
            "ACL SETUSER alice #Tr0ub4dor0123456789abcdef",
            "ACL SETUSER alice !Tr0ub4dor0123456789abcdef",
            "acl setuser alice resetpass >Tr0ub4dor >Tr0ub4dor2",
        ] {
            assert_hidden_any_case(text, r, "Tr0ub");
        }
        assert_eq!(
            redact_secrets("ACL SETUSER alice on >Tr0ub4dor ~cached:* +get", r),
            "ACL SETUSER alice on >[redacted] ~cached:* +get"
        );
    }

    #[test]
    fn redis_config_set() {
        let r = SqlDialect::Redis;
        for text in [
            "CONFIG SET requirepass Tr0ub4dor",
            "CONFIG SET masterauth Tr0ub4dor",
            "CONFIG SET masteruser rep masterauth Tr0ub4dor",
            "CONFIG SET tls-key-file-pass Tr0ub4dor",
            "CONFIG SET requirepass \"Tr0ub 4dor\"",
            "SENTINEL SET mymaster auth-pass Tr0ub4dor",
            "SENTINEL CONFIG SET sentinel-pass Tr0ub4dor",
        ] {
            assert_hidden_any_case(text, r, "Tr0ub");
        }
        assert_eq!(
            redact_secrets("CONFIG SET maxmemory 1gb requirepass Tr0ub4dor", r),
            "CONFIG SET maxmemory 1gb requirepass [redacted]"
        );
    }

    #[test]
    fn redis_migrate() {
        let r = SqlDialect::Redis;
        for text in [
            "MIGRATE 10.0.0.2 6379 key 0 5000 AUTH Tr0ub4dor",
            "MIGRATE 10.0.0.2 6379 \"\" 0 5000 COPY REPLACE AUTH Tr0ub4dor KEYS a b",
            "MIGRATE 10.0.0.2 6379 key 0 5000 AUTH2 alice Tr0ub4dor",
        ] {
            assert_hidden_any_case(text, r, "Tr0ub");
        }
        assert_eq!(
            redact_secrets("MIGRATE h 6379 key 0 5000 AUTH2 alice Tr0ub4dor", r),
            "MIGRATE h 6379 key 0 5000 AUTH2 alice [redacted]"
        );
    }

    // ---- Levels ----------------------------------------------------------

    #[test]
    fn level_none_returns_nothing() {
        assert_eq!(
            prepare_statement_text("SELECT 1", SqlDialect::Postgres, TextLevel::None),
            None
        );
    }

    #[test]
    fn level_full_keeps_text_and_comments() {
        let text = "SELECT a, 'x' /* why */ FROM t WHERE id = 42";
        assert_eq!(
            prepare_statement_text(text, SqlDialect::Postgres, TextLevel::Full).as_deref(),
            Some(text)
        );
    }

    // ---- Normalization ---------------------------------------------------

    fn normalized(text: &str, dialect: SqlDialect) -> String {
        prepare_statement_text(text, dialect, TextLevel::Normalized).unwrap_or_default()
    }

    #[test]
    fn normalizes_postgres() {
        let pg = SqlDialect::Postgres;
        let cases = [
            (
                "SELECT *   FROM \"Users\" u\n WHERE u.id = 42 AND name = 'O''Brien' -- who\n",
                "SELECT * FROM \"Users\" u WHERE u.id = ? AND name = ?",
            ),
            (
                "INSERT INTO t (a, b, c) VALUES (1.5e3, E'x\\'y', $$z$$)",
                "INSERT INTO t (a, b, c) VALUES (?, ?, ?)",
            ),
            (
                "SELECT B'1010', X'FF', 0x1F, .5, 1_000, U&'d\\0061t'",
                "SELECT ?, ?, ?, ?, ?, ?",
            ),
            (
                "SELECT $1::int, a::text FROM t /* c /* nested */ */ WHERE b > -3",
                "SELECT $1::INT, a::TEXT FROM t WHERE b > -?",
            ),
            (
                "SELECT t1.col2 FROM t1; SELECT $f$ body $f$",
                "SELECT t1.col2 FROM t1; SELECT ?",
            ),
            (
                "ALTER ROLE r PASSWORD 'secret' VALID UNTIL '2030-01-01'",
                "ALTER ROLE ? PASSWORD ? VALID UNTIL ?",
            ),
        ];
        let wrong: Vec<String> = cases
            .iter()
            .filter(|(input, expected)| normalized(input, pg) != *expected)
            .map(|(input, _)| format!("{input:?} => {:?}", normalized(input, pg)))
            .collect();
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn normalizes_mysql() {
        let my = SqlDialect::Mysql;
        let cases = [
            (
                "SELECT `order`, \"str\" FROM t # comment\nWHERE a = 'it\\'s' AND b IN (1, 2)",
                "SELECT `order`, ? FROM t WHERE a = ? AND b IN (?, ?)",
            ),
            (
                "UPDATE t SET b = x'ff', c = b'01', d = 0x1F, e = _utf8mb4'x' WHERE id = ?",
                "UPDATE t SET b = ?, c = ?, d = ?, e = _utf8mb4 ? WHERE id = ?",
            ),
            ("SELECT 1col FROM t -- c\n", "SELECT ? FROM ?"),
            ("/*!40101 SET NAMES utf8mb4 */", "/*!40101 SET NAMES ? */"),
            (
                "SET PASSWORD FOR 'bob'@'%' = 'x'",
                "SET PASSWORD FOR ?@? = ?",
            ),
        ];
        let wrong: Vec<String> = cases
            .iter()
            .filter(|(input, expected)| normalized(input, my) != *expected)
            .map(|(input, _)| format!("{input:?} => {:?}", normalized(input, my)))
            .collect();
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn normalizes_redis() {
        let r = SqlDialect::Redis;
        let cases = [
            ("GET user:1", "GET user:1"),
            ("SET user:1 \"Alice\" EX 60 NX", "SET user:1 ? EX ? NX"),
            ("set k v px 100", "set k ? px ?"),
            ("HSET user:1 name Alice age 30", "HSET user:1 ? ? ? ?"),
            ("HGET user:1 name", "HGET user:1 ?"),
            ("LPUSH queue a b c", "LPUSH queue ? ? ?"),
            ("MGET a b c", "MGET a b c"),
            ("DEL a b", "DEL a b"),
            ("EXISTS a b", "EXISTS a b"),
            ("MSET a 1 b 2", "MSET a ? b ?"),
            ("BLPOP q1 q2 5", "BLPOP q1 q2 ?"),
            ("LMOVE src dst LEFT RIGHT", "LMOVE src dst LEFT RIGHT"),
            ("ZADD board 10 alice", "ZADD board ? ?"),
            (
                "ZRANGE board 0 -1 WITHSCORES",
                "ZRANGE board ? ? WITHSCORES",
            ),
            (
                "ZUNIONSTORE out 2 a b WEIGHTS 1 2",
                "ZUNIONSTORE out ? a b WEIGHTS ? ?",
            ),
            (
                "EVAL \"return redis.call('get', KEYS[1])\" 1 k argv1",
                "EVAL ? ? ? ?",
            ),
            (
                "XREAD COUNT 10 BLOCK 0 STREAMS s1 s2 0 $",
                "XREAD COUNT ? BLOCK ? STREAMS s1 s2 ? ?",
            ),
            ("CONFIG GET maxmemory", "CONFIG GET maxmemory"),
            (
                "CONFIG SET maxmemory 1gb requirepass secret",
                "CONFIG SET maxmemory ? requirepass ?",
            ),
            ("OBJECT ENCODING user:1", "OBJECT ENCODING user:1"),
            ("CLIENT KILL ID 7", "CLIENT KILL ? ?"),
            ("ACL SETUSER alice on >secret", "ACL SETUSER alice ? ?"),
            ("AUTH alice secret", "AUTH ? ?"),
            ("PING hello", "PING ?"),
            ("SCAN 0 MATCH user:* COUNT 100", "SCAN ? MATCH ? COUNT ?"),
            ("FLUSHALL ASYNC", "FLUSHALL ASYNC"),
            (
                "MIGRATE h 6379 \"\" 0 5000 COPY AUTH pw KEYS a b",
                "MIGRATE ? ? ? ? ? COPY AUTH ? KEYS a b",
            ),
            ("  GET   a  \n\n  INCR   b ", "GET a\nINCR b"),
            ("GET\0a", "GET a"),
            ("SET\0k\0v", "SET k ?"),
            ("SET k \"open\nGET k", "SET k ?\nGET k"),
            ("SET k a\"b c\"d", "SET k ?"),
            ("# SET k v\nGET k", "GET k"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalized(input, r), expected, "{input}");
        }
    }

    #[test]
    fn unknown_redis_commands_keep_no_arguments() {
        let r = SqlDialect::Redis;
        for (input, expected) in [
            ("AUHT hunter2", "? ?"),
            ("auht alice hunter2", "? ? ?"),
            ("AUHT\0hunter2", "? ?"),
            ("FOO.BAR key value", "? ? ?"),
            ("HSETT user:1 name hunter2", "? ? ? ?"),
            // Known commands keep their keys.
            ("GET user:1", "GET user:1"),
            ("HSET user:1 name Alice", "HSET user:1 ? ?"),
            ("XADD stream * f v", "XADD stream ? ? ?"),
        ] {
            assert_eq!(normalized(input, r), expected, "{input}");
        }
    }

    #[test]
    fn unknown_redis_subcommands_become_placeholders() {
        let r = SqlDialect::Redis;
        for (input, expected) in [
            ("CONFIG SETT requirepass hunter2", "CONFIG ? ? ?"),
            ("ACL SETUSR u >hunter2", "ACL ? ? ?"),
            ("OBJECT ENCODINGG hunter2", "OBJECT ? ?"),
            ("XINFO STREAMZ hunter2", "XINFO ? ?"),
            ("XGROUP CREATEE s g hunter2", "XGROUP ? ? ? ?"),
            ("MEMORY USAGEE hunter2", "MEMORY ? ?"),
            // Known subcommands keep their keys and parameter names.
            ("OBJECT ENCODING user:1", "OBJECT ENCODING user:1"),
            ("XGROUP CREATE s g $", "XGROUP CREATE s ? ?"),
            ("CONFIG SET maxmemory 1gb", "CONFIG SET maxmemory ?"),
            ("ACL SETUSER u >pw", "ACL SETUSER u ?"),
        ] {
            assert_eq!(normalized(input, r), expected, "{input}");
        }
    }

    #[test]
    fn dialect_for_engine() {
        assert_eq!(SqlDialect::from(Engine::Mariadb), SqlDialect::Mysql);
        assert_eq!(SqlDialect::from(Engine::Postgres), SqlDialect::Postgres);
        assert_eq!(SqlDialect::from(Engine::Redis), SqlDialect::Redis);
        assert_eq!(SqlDialect::Mysql.as_str(), "mysql");
    }
}
