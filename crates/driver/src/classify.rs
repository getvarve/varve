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
    scan(sql, dialect)
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

/// A keyword-level token with its parenthesis depth.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    word: String,
    depth: u32,
}

/// A statement found by the scanner: byte range and its word tokens.
struct Scanned {
    start: usize,
    end: usize,
    tokens: Vec<Token>,
}

/// Walks the script once. Literals, quoted identifiers and comments produce
/// no tokens; `;` at parenthesis depth 0 ends a statement.
fn scan(sql: &str, dialect: Dialect) -> Vec<Scanned> {
    let chars: Vec<(usize, char)> = sql.char_indices().collect();
    let byte_at = |i: usize| chars.get(i).map_or(sql.len(), |(b, _)| *b);
    let mysql = dialect == Dialect::Mysql;

    let mut out = Vec::new();
    let mut current = Scanned {
        start: 0,
        end: 0,
        tokens: Vec::new(),
    };
    let mut depth: u32 = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        let next = chars.get(i + 1).map(|(_, c)| *c);
        match c {
            '-' if next == Some('-') => i = skip_line(&chars, i),
            '#' if mysql => i = skip_line(&chars, i),
            '/' if next == Some('*') => i = skip_block_comment(&chars, i + 2, !mysql),
            '\'' => {
                let escapes = mysql || escape_string_prefix(&chars, i);
                i = skip_quoted(&chars, i + 1, '\'', escapes);
            }
            '"' => i = skip_quoted(&chars, i + 1, '"', mysql),
            '`' if mysql => i = skip_quoted(&chars, i + 1, '`', false),
            '$' if !mysql => match dollar_tag(&chars, i) {
                Some(tag_len) => i = skip_dollar_quoted(&chars, i, tag_len),
                None => i += 1,
            },
            '(' => {
                depth += 1;
                i += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            ';' if depth == 0 => {
                current.end = byte_at(i);
                out.push(current);
                i += 1;
                current = Scanned {
                    start: byte_at(i),
                    end: 0,
                    tokens: Vec::new(),
                };
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].1.is_alphanumeric()
                        || chars[i].1 == '_'
                        || (!mysql && chars[i].1 == '$'))
                {
                    i += 1;
                }
                let word: String = chars[start..i]
                    .iter()
                    .map(|(_, c)| *c)
                    .collect::<String>()
                    .to_ascii_uppercase();
                current.tokens.push(Token { word, depth });
            }
            _ => i += 1,
        }
    }
    current.end = sql.len();
    out.push(current);
    out
}

/// True for PostgreSQL escape strings: `E'...'` (not part of a longer word).
fn escape_string_prefix(chars: &[(usize, char)], quote: usize) -> bool {
    quote >= 1
        && matches!(chars[quote - 1].1, 'e' | 'E')
        && (quote < 2 || !(chars[quote - 2].1.is_alphanumeric() || chars[quote - 2].1 == '_'))
}

fn skip_line(chars: &[(usize, char)], mut i: usize) -> usize {
    while i < chars.len() && chars[i].1 != '\n' {
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
    while j < chars.len() && (chars[j].1.is_alphanumeric() || chars[j].1 == '_') {
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
}
