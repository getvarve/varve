//! Lexing of MySQL and MariaDB SQL text, for reading and rewriting the
//! output of `SHOW CREATE ...` statements.
//!
//! The lexer knows just enough of the grammar to never mistake the inside
//! of a string, a quoted identifier or a comment for structure: words,
//! quoted identifiers, strings, comments, executable comments
//! (`/*!50100 ... */`, MariaDB `/*M!100601 ... */`) and single-character
//! symbols. How `"` and `\` are read depends on the SQL mode the text was
//! written in, see [`Lex`].

/// How the text is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lex {
    /// `"` quotes identifiers (`ANSI_QUOTES`, also part of MariaDB's
    /// `ORACLE` mode) instead of strings.
    pub ansi_quotes: bool,
    /// `\` escapes the next character in strings (off with
    /// `NO_BACKSLASH_ESCAPES`).
    pub backslash_escapes: bool,
    /// MariaDB: executable comments may carry six-digit versions.
    pub mariadb: bool,
}

impl Lex {
    /// The reading rules for text written under `sql_mode`.
    pub(crate) fn for_sql_mode(sql_mode: &str, mariadb: bool) -> Self {
        let has = |flag: &str| {
            sql_mode
                .split(',')
                .any(|mode| mode.trim().eq_ignore_ascii_case(flag))
        };
        Self {
            ansi_quotes: has("ANSI_QUOTES") || has("ORACLE") || has("ANSI"),
            backslash_escapes: !has("NO_BACKSLASH_ESCAPES"),
            mariadb,
        }
    }
}

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Whitespace.
    Space,
    /// A comment that is never executed.
    Comment,
    /// The start of an executable comment, with its minimum version.
    ExecStart(Option<u32>),
    /// The `*/` that ends an executable comment.
    ExecEnd,
    /// A keyword, unquoted identifier or number.
    Word,
    /// A quoted identifier.
    Ident,
    /// A string literal.
    Str,
    /// Any other single character.
    Sym,
}

/// One token: its kind and byte range in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tok {
    /// What it is.
    pub kind: Kind,
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset after the last character.
    pub end: usize,
}

impl Tok {
    /// True for tokens that carry meaning (not space or comments).
    pub(crate) fn significant(&self) -> bool {
        matches!(self.kind, Kind::Word | Kind::Ident | Kind::Str | Kind::Sym)
    }

    /// The token's text.
    pub(crate) fn text<'a>(&self, sql: &'a str) -> &'a str {
        sql.get(self.start..self.end).unwrap_or("")
    }

    /// True for the word `word`, ignoring case.
    pub(crate) fn is_word(&self, sql: &str, word: &str) -> bool {
        self.kind == Kind::Word && self.text(sql).eq_ignore_ascii_case(word)
    }

    /// True for the symbol `c`.
    pub(crate) fn is_sym(&self, sql: &str, c: char) -> bool {
        self.kind == Kind::Sym && self.text(sql).starts_with(c)
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// Splits `sql` into tokens. Never fails: unterminated strings, quotes and
/// comments run to the end of the text.
pub(crate) fn tokens(sql: &str, lex: Lex) -> Vec<Tok> {
    let chars: Vec<(usize, char)> = sql.char_indices().collect();
    let at = |i: usize| chars.get(i).map(|(_, c)| *c);
    let offset = |i: usize| chars.get(i).map_or(sql.len(), |(o, _)| *o);
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_exec = false;
    while let Some(c) = at(i) {
        let start = i;
        let kind = if c.is_whitespace() {
            while at(i).is_some_and(char::is_whitespace) {
                i += 1;
            }
            Kind::Space
        } else if c == '#'
            || (c == '-'
                && at(i + 1) == Some('-')
                && at(i + 2).is_none_or(|n| n.is_whitespace() || n.is_control()))
        {
            while at(i).is_some_and(|n| n != '\n') {
                i += 1;
            }
            Kind::Comment
        } else if c == '/' && at(i + 1) == Some('*') {
            let exec = if at(i + 2) == Some('!') {
                Some(3)
            } else if at(i + 2) == Some('M') && at(i + 3) == Some('!') {
                Some(4)
            } else {
                None
            };
            match exec {
                Some(skip) if !in_exec => {
                    i += skip;
                    let max_digits = if lex.mariadb { 6 } else { 5 };
                    let mut version = String::new();
                    while version.len() < max_digits && at(i).is_some_and(|d| d.is_ascii_digit()) {
                        if let Some(d) = at(i) {
                            version.push(d);
                        }
                        i += 1;
                    }
                    in_exec = true;
                    Kind::ExecStart(version.parse().ok())
                }
                _ => {
                    i += 2;
                    while at(i).is_some() && !(at(i) == Some('*') && at(i + 1) == Some('/')) {
                        i += 1;
                    }
                    i = (i + 2).min(chars.len());
                    Kind::Comment
                }
            }
        } else if c == '*' && at(i + 1) == Some('/') && in_exec {
            i += 2;
            in_exec = false;
            Kind::ExecEnd
        } else if c == '`' || (c == '"' && lex.ansi_quotes) {
            i = quoted_end(&chars, i, c, false);
            Kind::Ident
        } else if c == '\'' || c == '"' {
            i = quoted_end(&chars, i, c, lex.backslash_escapes);
            Kind::Str
        } else if is_word_char(c) {
            while at(i).is_some_and(is_word_char) {
                i += 1;
            }
            Kind::Word
        } else {
            i += 1;
            Kind::Sym
        };
        out.push(Tok {
            kind,
            start: offset(start),
            end: offset(i),
        });
    }
    out
}

/// Index after the closing quote of a quoted token starting at `i`.
fn quoted_end(chars: &[(usize, char)], mut i: usize, quote: char, backslash: bool) -> usize {
    i += 1;
    while let Some((_, c)) = chars.get(i) {
        if backslash && *c == '\\' {
            i += 2;
            continue;
        }
        if *c == quote {
            if chars.get(i + 1).map(|(_, n)| *n) == Some(quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    chars.len()
}

/// The name a word or quoted identifier stands for.
pub(crate) fn name_of(sql: &str, tok: &Tok) -> Option<String> {
    let text = tok.text(sql);
    match tok.kind {
        Kind::Word => Some(text.to_owned()),
        Kind::Ident => {
            let mut chars = text.chars();
            let quote = chars.next()?;
            let inner = text.get(quote.len_utf8()..text.len().checked_sub(quote.len_utf8())?)?;
            let doubled: String = [quote, quote].iter().collect();
            Some(inner.replace(&doubled, &quote.to_string()))
        }
        _ => None,
    }
}

/// The value of a string literal token (quotes removed, escapes read).
pub(crate) fn string_value(sql: &str, tok: &Tok, lex: Lex) -> Option<String> {
    if tok.kind != Kind::Str {
        return None;
    }
    let text = tok.text(sql);
    let quote = text.chars().next()?;
    let inner = text.get(1..text.len().checked_sub(1)?)?;
    Some(unescape(inner, quote, lex.backslash_escapes))
}

/// Reads the inside of a quoted string: doubled quotes and, when enabled,
/// backslash escapes.
pub(crate) fn unescape(inner: &str, quote: char, backslash: bool) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if backslash && c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some('b') => out.push('\u{8}'),
                Some('Z') => out.push('\u{1a}'),
                // `\%` and `\_` keep their backslash (LIKE patterns).
                Some(other @ ('%' | '_')) => {
                    out.push('\\');
                    out.push(other);
                }
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else if c == quote && chars.peek() == Some(&quote) {
            chars.next();
            out.push(quote);
        } else {
            out.push(c);
        }
    }
    out
}

/// Removes the markers of executable comments (`/*!50100 `, `*/`) whose
/// version is at most `version` (`major * 10000 + minor * 100 + patch`),
/// keeping their content, so server output reads as plain SQL. Comments for
/// newer versions are kept as they are.
pub(crate) fn unwrap_exec_comments(sql: &str, lex: Lex, version: u32) -> String {
    let toks = tokens(sql, lex);
    let mut out = String::with_capacity(sql.len());
    let mut unwrapping = false;
    let mut skip_space = false;
    for tok in &toks {
        match tok.kind {
            Kind::ExecStart(v) if v.is_none_or(|v| v <= version) => {
                unwrapping = true;
                skip_space = true;
                continue;
            }
            Kind::ExecEnd if unwrapping => {
                unwrapping = false;
                let trimmed = out.trim_end_matches(' ').len();
                out.truncate(trimmed);
                continue;
            }
            Kind::Space if skip_space => {
                skip_space = false;
                let text = tok.text(sql);
                if let Some(rest) = text.strip_prefix(' ') {
                    out.push_str(rest);
                } else {
                    out.push_str(text);
                }
                continue;
            }
            _ => {}
        }
        skip_space = false;
        out.push_str(tok.text(sql));
    }
    out
}

/// The parts of a `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableParts {
    /// Column, index, constraint and period definitions, in order, trimmed.
    pub items: Vec<String>,
    /// Everything after the closing parenthesis: table options and the
    /// partitioning clause.
    pub tail: String,
}

/// Splits the definitions of a `CREATE TABLE` statement at their
/// top-level commas.
pub(crate) fn table_parts(sql: &str, lex: Lex) -> Option<TableParts> {
    let toks = tokens(sql, lex);
    let open = toks.iter().position(|t| t.is_sym(sql, '('))?;
    let (items, close) = split_group(sql, &toks, open)?;
    Some(TableParts {
        items,
        tail: sql.get(close..).unwrap_or("").to_owned(),
    })
}

/// Splits the parenthesised group opening at token `open` at its
/// top-level commas. Returns the trimmed parts and the byte offset after
/// the closing parenthesis.
fn split_group(sql: &str, toks: &[Tok], open: usize) -> Option<(Vec<String>, usize)> {
    let mut depth = 0_usize;
    let mut items = Vec::new();
    let mut item_start = toks.get(open)?.end;
    for tok in toks.iter().skip(open) {
        if tok.is_sym(sql, '(') {
            depth += 1;
        } else if tok.is_sym(sql, ')') {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                let item = sql.get(item_start..tok.start)?.trim();
                if !item.is_empty() {
                    items.push(item.to_owned());
                }
                return Some((items, tok.end));
            }
        } else if depth == 1 && tok.is_sym(sql, ',') {
            items.push(sql.get(item_start..tok.start)?.trim().to_owned());
            item_start = tok.end;
        }
    }
    None
}

/// What a `CREATE TABLE` definition defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TableItem {
    /// A column.
    Column(String),
    /// The primary key.
    PrimaryKey,
    /// A plain, unique, fulltext or spatial index.
    Index(String),
    /// A foreign key.
    ForeignKey(String),
    /// A table-level check constraint.
    Check(String),
    /// A period (`SYSTEM_TIME` or an application-time period) with its
    /// start and end columns.
    Period {
        /// Period name (`SYSTEM_TIME` for system versioning).
        name: String,
        /// Start column.
        start: String,
        /// End column.
        end: String,
    },
    /// Something not recognised.
    Other,
}

/// Significant tokens of `sql`.
fn significant(sql: &str, lex: Lex) -> Vec<Tok> {
    tokens(sql, lex)
        .into_iter()
        .filter(Tok::significant)
        .collect()
}

/// Classifies one definition of a `CREATE TABLE` statement.
pub(crate) fn table_item(item: &str, lex: Lex) -> TableItem {
    let toks = significant(item, lex);
    let word = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(item, w));
    let name = |i: usize| toks.get(i).and_then(|t| name_of(item, t));
    let Some(first) = toks.first() else {
        return TableItem::Other;
    };
    if first.kind == Kind::Ident {
        return name(0).map_or(TableItem::Other, TableItem::Column);
    }
    if word(0, "PRIMARY") && word(1, "KEY") {
        return TableItem::PrimaryKey;
    }
    if word(0, "PERIOD") && word(1, "FOR") {
        // PERIOD FOR name ( start , end )
        let Some(open) = toks.iter().position(|t| t.is_sym(item, '(')) else {
            return TableItem::Other;
        };
        return match (name(2), name(open + 1), name(open + 3)) {
            (Some(name), Some(start), Some(end)) => TableItem::Period { name, start, end },
            _ => TableItem::Other,
        };
    }
    if word(0, "CONSTRAINT") {
        let has_name = toks.get(1).is_some_and(|t| {
            t.kind == Kind::Ident
                || (t.kind == Kind::Word
                    && !["FOREIGN", "CHECK", "PRIMARY", "UNIQUE"]
                        .iter()
                        .any(|w| t.is_word(item, w)))
        });
        let (constraint, next) = if has_name {
            (name(1).unwrap_or_default(), 2)
        } else {
            (String::new(), 1)
        };
        if word(next, "FOREIGN") {
            return TableItem::ForeignKey(constraint);
        }
        if word(next, "CHECK") {
            return TableItem::Check(constraint);
        }
        if word(next, "PRIMARY") {
            return TableItem::PrimaryKey;
        }
        if word(next, "UNIQUE") {
            return index_name(item, &toks, next + 1).map_or(TableItem::Other, TableItem::Index);
        }
        return TableItem::Other;
    }
    for prefix in ["UNIQUE", "FULLTEXT", "SPATIAL"] {
        if word(0, prefix) {
            return index_name(item, &toks, 1).map_or(TableItem::Other, TableItem::Index);
        }
    }
    if word(0, "KEY") || word(0, "INDEX") {
        return index_name(item, &toks, 0).map_or(TableItem::Other, TableItem::Index);
    }
    if first.kind == Kind::Word && !word(1, "FOR") {
        return name(0).map_or(TableItem::Other, TableItem::Column);
    }
    TableItem::Other
}

/// The index name after an optional `KEY`/`INDEX` at token `i`.
fn index_name(sql: &str, toks: &[Tok], mut i: usize) -> Option<String> {
    if toks
        .get(i)
        .is_some_and(|t| t.is_word(sql, "KEY") || t.is_word(sql, "INDEX"))
    {
        i += 1;
    }
    let tok = toks.get(i)?;
    if tok.is_sym(sql, '(') {
        return None;
    }
    name_of(sql, tok)
}

/// Byte offset just after the `KEY`/`INDEX` keyword of an index
/// definition, or after `FOREIGN KEY` of a foreign key: where MariaDB
/// takes `IF NOT EXISTS`.
pub(crate) fn key_keyword_end(item: &str, lex: Lex) -> Option<usize> {
    let toks = significant(item, lex);
    toks.iter()
        .find(|t| t.is_word(item, "KEY") || t.is_word(item, "INDEX"))
        .map(|t| t.end)
}

/// The partition definitions of a table's options tail: the items of the
/// `(PARTITION ..., PARTITION ...)` list, or `None` when the partitioning
/// clause has no list (`PARTITIONS n`) or the table is not partitioned.
pub(crate) fn partition_items(tail: &str, lex: Lex) -> Option<Vec<String>> {
    let toks: Vec<Tok> = significant(tail, lex);
    let by = toks
        .windows(2)
        .position(|w| w[0].is_word(tail, "PARTITION") && w[1].is_word(tail, "BY"))?;
    let mut depth = 0_usize;
    for (i, tok) in toks.iter().enumerate().skip(by) {
        if tok.is_sym(tail, '(') {
            if depth == 0
                && toks
                    .get(i + 1)
                    .is_some_and(|n| n.is_word(tail, "PARTITION"))
            {
                return split_group(tail, &toks, i).map(|(items, _)| items);
            }
            depth += 1;
        } else if tok.is_sym(tail, ')') {
            depth = depth.saturating_sub(1);
        }
    }
    None
}

/// True when a table's options tail says `WITH SYSTEM VERSIONING`.
pub(crate) fn has_system_versioning(tail: &str, lex: Lex) -> bool {
    let toks = significant(tail, lex);
    toks.windows(3).any(|w| {
        w[0].is_word(tail, "WITH")
            && w[1].is_word(tail, "SYSTEM")
            && w[2].is_word(tail, "VERSIONING")
    })
}

/// The name of a `PARTITION name ...` or `SUBPARTITION name ...`
/// definition.
pub(crate) fn partition_item_name(item: &str, lex: Lex) -> Option<String> {
    let toks = significant(item, lex);
    let first = toks.first()?;
    if !(first.is_word(item, "PARTITION") || first.is_word(item, "SUBPARTITION")) {
        return None;
    }
    name_of(item, toks.get(1)?)
}

/// A name in a statement, possibly qualified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NameSpan {
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset after the last character.
    pub end: usize,
    /// The (last part of the) name.
    pub name: String,
    /// True when written as `db.name`.
    pub qualified: bool,
    /// Index of the last significant token of the name.
    last: usize,
}

/// Where the parts of a `CREATE ...` statement are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreateHead {
    /// Byte offset after the `CREATE` keyword.
    pub create_end: usize,
    /// True when `OR REPLACE` follows `CREATE`.
    pub or_replace: bool,
    /// Byte offset after the object kind keywords (`TABLE`, `PACKAGE BODY`).
    pub kind_end: usize,
    /// True when `IF NOT EXISTS` follows the kind.
    pub if_not_exists: bool,
    /// The object name.
    pub name: NameSpan,
}

/// Finds the parts of a `CREATE [...] <kind> [IF NOT EXISTS] name`
/// statement; `kind` holds the kind keywords, for example
/// `["PACKAGE", "BODY"]`.
pub(crate) fn create_head(sql: &str, lex: Lex, kind: &[&str]) -> Option<CreateHead> {
    let toks = significant(sql, lex);
    let create = toks.iter().position(|t| t.is_word(sql, "CREATE"))?;
    let or_replace = toks.get(create + 1).is_some_and(|t| t.is_word(sql, "OR"))
        && toks
            .get(create + 2)
            .is_some_and(|t| t.is_word(sql, "REPLACE"));
    let first_kind = kind.first()?;
    let kind_at = (create + 1..toks.len()).find(|&i| {
        toks.get(i).is_some_and(|t| t.is_word(sql, first_kind))
            && kind
                .iter()
                .enumerate()
                .all(|(k, w)| toks.get(i + k).is_some_and(|t| t.is_word(sql, w)))
    })?;
    let mut next = kind_at + kind.len();
    let kind_end = toks.get(next - 1)?.end;
    let if_not_exists = ["IF", "NOT", "EXISTS"]
        .iter()
        .enumerate()
        .all(|(k, w)| toks.get(next + k).is_some_and(|t| t.is_word(sql, w)));
    if if_not_exists {
        next += 3;
    }
    let name = name_span(sql, &toks, next)?;
    Some(CreateHead {
        create_end: toks.get(create)?.end,
        or_replace,
        kind_end,
        if_not_exists,
        name,
    })
}

/// The possibly qualified name starting at significant token `i`.
fn name_span(sql: &str, toks: &[Tok], i: usize) -> Option<NameSpan> {
    let first = toks.get(i)?;
    let first_name = name_of(sql, first)?;
    let dotted = toks.get(i + 1).is_some_and(|t| t.is_sym(sql, '.'))
        && toks.get(i + 1).map(|t| t.end) == toks.get(i + 2).map(|t| t.start)
        && toks.get(i + 2).and_then(|t| name_of(sql, t)).is_some();
    if dotted {
        let last = toks.get(i + 2)?;
        Some(NameSpan {
            start: first.start,
            end: last.end,
            name: name_of(sql, last)?,
            qualified: true,
            last: i + 2,
        })
    } else {
        Some(NameSpan {
            start: first.start,
            end: first.end,
            name: first_name,
            qualified: false,
            last: i,
        })
    }
}

/// Index of the significant token that ends at byte offset `end`.
fn index_ending_at(toks: &[Tok], end: usize) -> Option<usize> {
    toks.iter().position(|t| t.end == end)
}

/// In a `CREATE TRIGGER` statement: the table after `ON`, and the byte
/// offset after `FOR EACH ROW` together with whether `FOLLOWS`/`PRECEDES`
/// already follows it.
pub(crate) fn trigger_parts(
    sql: &str,
    lex: Lex,
    head: &CreateHead,
) -> Option<(NameSpan, usize, bool)> {
    let toks = significant(sql, lex);
    let after_name = head.name.last + 1;
    let on =
        (after_name..toks.len()).find(|&i| toks.get(i).is_some_and(|t| t.is_word(sql, "ON")))?;
    let table = name_span(sql, &toks, on + 1)?;
    let row = (table.last + 1..toks.len()).find(|&i| {
        toks.get(i).is_some_and(|t| t.is_word(sql, "FOR"))
            && toks.get(i + 1).is_some_and(|t| t.is_word(sql, "EACH"))
            && toks.get(i + 2).is_some_and(|t| t.is_word(sql, "ROW"))
    })?;
    let row_end = toks.get(row + 2)?.end;
    let ordered = toks
        .get(row + 3)
        .is_some_and(|t| t.is_word(sql, "FOLLOWS") || t.is_word(sql, "PRECEDES"));
    Some((table, row_end, ordered))
}

/// In a `CREATE VIEW` statement: the byte offset after the `AS` that
/// starts the query (after the name and an optional column list).
pub(crate) fn view_as_end(sql: &str, lex: Lex, head: &CreateHead) -> Option<usize> {
    let toks = significant(sql, lex);
    let mut i = index_ending_at(&toks, head.name.end)? + 1;
    if toks.get(i).is_some_and(|t| t.is_sym(sql, '(')) {
        let mut depth = 0_usize;
        while let Some(tok) = toks.get(i) {
            if tok.is_sym(sql, '(') {
                depth += 1;
            } else if tok.is_sym(sql, ')') {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            i += 1;
        }
        i += 1;
    }
    let tok = toks.get(i)?;
    tok.is_word(sql, "AS").then_some(tok.end)
}

/// The value of `ALGORITHM=...` in a `CREATE VIEW` header.
pub(crate) fn view_algorithm(sql: &str, lex: Lex) -> Option<String> {
    let toks = significant(sql, lex);
    let i = toks.iter().position(|t| t.is_word(sql, "ALGORITHM"))?;
    let value = if toks.get(i + 1).is_some_and(|t| t.is_sym(sql, '=')) {
        toks.get(i + 2)?
    } else {
        toks.get(i + 1)?
    };
    (value.kind == Kind::Word).then(|| value.text(sql).to_ascii_uppercase())
}

/// Adds MariaDB `REF_SYSTEM_ID=n` after the type of the geometry columns
/// named in `srids` (column name and SRID) in a `CREATE TABLE` statement.
/// MariaDB keeps the SRID but leaves it out of `SHOW CREATE TABLE`.
pub(crate) fn add_ref_system_ids(sql: &str, lex: Lex, srids: &[(String, u32)]) -> String {
    if srids.is_empty() {
        return sql.to_owned();
    }
    let toks = significant(sql, lex);
    let Some(open) = toks.iter().position(|t| t.is_sym(sql, '(')) else {
        return sql.to_owned();
    };
    let mut edits = Vec::new();
    let mut depth = 0_usize;
    let mut item_start = Some(open + 1);
    let mut i = open;
    while let Some(tok) = toks.get(i) {
        if tok.is_sym(sql, '(') {
            depth += 1;
        } else if tok.is_sym(sql, ')') {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                break;
            }
        } else if depth == 1 && tok.is_sym(sql, ',') {
            item_start = Some(i + 1);
        }
        if let Some(start) = item_start.take() {
            let name = toks
                .get(start)
                .filter(|t| t.kind == Kind::Ident)
                .and_then(|t| name_of(sql, t));
            let srid = name.and_then(|n| srids.iter().find(|(c, _)| *c == n).map(|(_, s)| *s));
            if let (Some(srid), Some(ty)) = (srid, toks.get(start + 1)) {
                let mut j = start + 1;
                let mut nested = 0_usize;
                let mut present = false;
                while let Some(t) = toks.get(j) {
                    if t.is_sym(sql, '(') {
                        nested += 1;
                    } else if t.is_sym(sql, ')') {
                        if nested == 0 {
                            break;
                        }
                        nested -= 1;
                    } else if nested == 0 && t.is_sym(sql, ',') {
                        break;
                    } else if t.is_word(sql, "REF_SYSTEM_ID") {
                        present = true;
                    }
                    j += 1;
                }
                if !present && ty.kind == Kind::Word {
                    edits.push((ty.end, ty.end, format!(" REF_SYSTEM_ID={srid}")));
                }
            }
        }
        i += 1;
    }
    apply_edits(sql, edits)
}

/// Replaces the `COMMENT '...'` literal of a `CREATE` statement whose
/// value is exactly `broken` (the double encoded comment `SHOW CREATE`
/// printed) with `literal`. Leaves the text alone when there is no such
/// literal.
pub(crate) fn repair_comment(sql: &str, lex: Lex, broken: &str, literal: &str) -> String {
    let toks = significant(sql, lex);
    let found = toks.windows(2).find(|w| {
        w[0].is_word(sql, "COMMENT") && string_value(sql, &w[1], lex).as_deref() == Some(broken)
    });
    match found {
        Some(w) => apply_edits(sql, vec![(w[1].start, w[1].end, literal.to_owned())]),
        None => sql.to_owned(),
    }
}

/// Applies non-overlapping replacements `(start, end, text)` to `sql`.
pub(crate) fn apply_edits(sql: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    edits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut out = sql.to_owned();
    for (start, end, text) in edits {
        if start <= end
            && end <= out.len()
            && out.is_char_boundary(start)
            && out.is_char_boundary(end)
        {
            out.replace_range(start..end, &text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEX: Lex = Lex {
        ansi_quotes: false,
        backslash_escapes: true,
        mariadb: false,
    };

    #[test]
    fn strings_comments_and_quotes_are_single_tokens() {
        let sql = "a 'x''y\\'z' `q``r` \"s\" -- c\n# h\n/* m */ b";
        let kinds: Vec<Kind> = tokens(sql, LEX)
            .into_iter()
            .filter(|t| t.kind != Kind::Space)
            .map(|t| t.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                Kind::Word,
                Kind::Str,
                Kind::Ident,
                Kind::Str,
                Kind::Comment,
                Kind::Comment,
                Kind::Comment,
                Kind::Word
            ]
        );
        let ansi = Lex::for_sql_mode("ANSI_QUOTES,NO_BACKSLASH_ESCAPES", false);
        let toks: Vec<Tok> = significant("\"a\"\"b\" 'c\\'", ansi);
        assert_eq!(toks.len(), 2);
        assert_eq!(
            name_of("\"a\"\"b\" 'c\\'", &toks[0]).as_deref(),
            Some("a\"b")
        );
        assert_eq!(
            string_value("\"a\"\"b\" 'c\\'", &toks[1], ansi).as_deref(),
            Some("c\\")
        );
        assert_eq!(
            name_of("`a``b`", &significant("`a``b`", LEX)[0]).as_deref(),
            Some("a`b")
        );
    }

    #[test]
    fn executable_comments_unwrap_by_version() {
        let sql = "`i` int /*!80023 INVISIBLE */,\n x /*!99999 FUTURE */ 'a/*!1 b*/'\n/*!50100 PARTITION BY HASH (`a`)\nPARTITIONS 2 */";
        assert_eq!(
            unwrap_exec_comments(sql, LEX, 80046),
            "`i` int INVISIBLE,\n x /*!99999 FUTURE */ 'a/*!1 b*/'\nPARTITION BY HASH (`a`)\nPARTITIONS 2"
        );
        let maria = Lex {
            mariadb: true,
            ..LEX
        };
        assert_eq!(
            unwrap_exec_comments("a /*M!100601 b */ c /*!40100 d*/", maria, 120303),
            "a b c d"
        );
    }

    #[test]
    fn table_items_are_classified() {
        let sql = "CREATE TABLE `t` (\n  `id` int NOT NULL,\n  `c` varchar(10) DEFAULT 'a,(b',\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `u` (`c`),\n  KEY `k` (`c`(5) DESC,`id`),\n  FULLTEXT KEY `f` (`c`),\n  CONSTRAINT `fk` FOREIGN KEY (`id`) REFERENCES `p` (`id`),\n  CONSTRAINT `ck` CHECK ((`id` > 0)),\n  PERIOD FOR `valid` (`s`, `e`),\n  PERIOD FOR SYSTEM_TIME (`rs`, `re`)\n) ENGINE=InnoDB\nPARTITION BY RANGE (`id`)\n(PARTITION p0 VALUES LESS THAN (10) ENGINE = InnoDB,\n PARTITION `p 1` VALUES LESS THAN MAXVALUE ENGINE = InnoDB)";
        let parts = table_parts(sql, LEX);
        let Some(parts) = parts else {
            panic!("no parts");
        };
        let kinds: Vec<TableItem> = parts.items.iter().map(|i| table_item(i, LEX)).collect();
        assert_eq!(
            kinds,
            [
                TableItem::Column("id".into()),
                TableItem::Column("c".into()),
                TableItem::PrimaryKey,
                TableItem::Index("u".into()),
                TableItem::Index("k".into()),
                TableItem::Index("f".into()),
                TableItem::ForeignKey("fk".into()),
                TableItem::Check("ck".into()),
                TableItem::Period {
                    name: "valid".into(),
                    start: "s".into(),
                    end: "e".into()
                },
                TableItem::Period {
                    name: "SYSTEM_TIME".into(),
                    start: "rs".into(),
                    end: "re".into()
                },
            ]
        );
        assert_eq!(parts.items[1], "`c` varchar(10) DEFAULT 'a,(b'");
        let partitions = partition_items(&parts.tail, LEX).unwrap_or_default();
        assert_eq!(partitions.len(), 2);
        let names: Vec<Option<String>> = partitions
            .iter()
            .map(|p| partition_item_name(p, LEX))
            .collect();
        assert_eq!(names, [Some("p0".into()), Some("p 1".into())]);
        assert_eq!(
            partition_items(") ENGINE=InnoDB PARTITION BY HASH (`a`) PARTITIONS 4", LEX),
            None
        );
        assert_eq!(key_keyword_end("UNIQUE KEY `u` (`c`)", LEX), Some(10));
        assert!(has_system_versioning(
            ") ENGINE=InnoDB WITH SYSTEM VERSIONING",
            LEX
        ));
        assert!(!has_system_versioning(
            ") COMMENT='WITH SYSTEM VERSIONING'",
            LEX
        ));
    }

    #[test]
    fn create_heads_are_found() -> Result<(), String> {
        let sql = "CREATE DEFINER=`root`@`%` TRIGGER `tr` BEFORE INSERT ON `t` FOR EACH ROW set new.a = 1";
        let head = create_head(sql, LEX, &["TRIGGER"]).ok_or("no head")?;
        assert_eq!(head.name.name, "tr");
        assert!(!head.name.qualified);
        let (table, row_end, ordered) = trigger_parts(sql, LEX, &head).ok_or("no parts")?;
        assert_eq!(table.name, "t");
        assert_eq!(
            sql.get(..row_end),
            Some("CREATE DEFINER=`root`@`%` TRIGGER `tr` BEFORE INSERT ON `t` FOR EACH ROW")
        );
        assert!(!ordered);

        let sql = "CREATE DEFINER=\"root\"@\"localhost\" PACKAGE BODY \"pk\" as begin null; end";
        let ansi = Lex::for_sql_mode("PIPES_AS_CONCAT,ANSI_QUOTES,ORACLE", true);
        let head = create_head(sql, ansi, &["PACKAGE", "BODY"]).ok_or("no head")?;
        assert_eq!(head.name.name, "pk");

        let sql = "CREATE trigger db.tr before insert on db.t for each row follows x set a = 1";
        let head = create_head(sql, LEX, &["TRIGGER"]).ok_or("no head")?;
        assert!(head.name.qualified);
        assert_eq!(head.name.name, "tr");
        let (table, _, ordered) = trigger_parts(sql, LEX, &head).ok_or("no parts")?;
        assert!(table.qualified);
        assert!(ordered);

        let sql = "CREATE ALGORITHM=MERGE DEFINER=`a`@`b` SQL SECURITY INVOKER VIEW `db`.`v` (`x`, `y`) AS select 1 AS `x`, 2 AS `y`";
        let head = create_head(sql, LEX, &["VIEW"]).ok_or("no head")?;
        assert!(head.name.qualified);
        let end = view_as_end(sql, LEX, &head).ok_or("no AS")?;
        assert!(sql.get(..end).is_some_and(|s| s.ends_with("(`x`, `y`) AS")));
        assert_eq!(view_algorithm(sql, LEX).as_deref(), Some("MERGE"));

        let sql = "CREATE TABLE IF NOT EXISTS `t` (a int)";
        let head = create_head(sql, LEX, &["TABLE"]).ok_or("no head")?;
        assert!(head.if_not_exists);
        assert_eq!(head.name.name, "t");
        assert_eq!(
            apply_edits(
                sql,
                vec![
                    (head.name.start, head.name.end, "`d`.`t`".into()),
                    (head.create_end, head.create_end, " X".into())
                ]
            ),
            "CREATE X TABLE IF NOT EXISTS `d`.`t` (a int)"
        );
        Ok(())
    }

    #[test]
    fn ref_system_ids_are_added() {
        let sql = "CREATE TABLE `t` (\n  `id` int,\n  `p` point NOT NULL,\n  `q` point REF_SYSTEM_ID=1\n)";
        let srids = vec![("p".to_owned(), 4326), ("q".to_owned(), 1)];
        assert_eq!(
            add_ref_system_ids(sql, LEX, &srids),
            "CREATE TABLE `t` (\n  `id` int,\n  `p` point REF_SYSTEM_ID=4326 NOT NULL,\n  `q` point REF_SYSTEM_ID=1\n)"
        );
    }

    #[test]
    fn double_encoded_comments_are_repaired() {
        let sql = "CREATE PROCEDURE `p`()\n    COMMENT 'caf\u{c3}\u{a9}'\nselect 'gr\u{fc}\u{df}e'";
        assert_eq!(
            repair_comment(sql, LEX, "caf\u{c3}\u{a9}", "'caf\u{e9}'"),
            "CREATE PROCEDURE `p`()\n    COMMENT 'caf\u{e9}'\nselect 'gr\u{fc}\u{df}e'"
        );
        assert_eq!(repair_comment(sql, LEX, "other", "'x'"), sql);
    }

    #[test]
    fn unescapes_strings() {
        assert_eq!(unescape("a''b\\\\c\\n\\%", '\'', true), "a'b\\c\n\\%");
        assert_eq!(unescape("a\\b", '\'', false), "a\\b");
    }
}
