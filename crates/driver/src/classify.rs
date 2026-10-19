//! Statement classification for SQL engines.
//!
//! Used for the destructive-statement confirmation on production connections
//! (design doc 3.2) and for the statement kind in activity events (9.2). The
//! lexer understands `--`, `#` and `/* */` comments, single and double quoted
//! strings, backtick identifiers and PostgreSQL dollar quoting, so keywords
//! inside literals and comments never count.

use serde::{Deserialize, Serialize};

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

/// Classifies every statement in `sql`, in order. Empty statements are skipped.
#[must_use]
pub fn classify(sql: &str) -> Vec<StatementKind> {
    split_tokens(sql)
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| classify_tokens(s))
        .collect()
}

/// True when any statement in `sql` is destructive.
#[must_use]
pub fn is_destructive(sql: &str) -> bool {
    classify(sql).into_iter().any(StatementKind::is_destructive)
}

/// A keyword-level token with its parenthesis depth.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    word: String,
    depth: u32,
}

/// Splits `sql` into statements of uppercase word tokens. Literals, quoted
/// identifiers and comments produce no tokens.
fn split_tokens(sql: &str) -> Vec<Vec<Token>> {
    let chars: Vec<char> = sql.chars().collect();
    let mut statements = vec![Vec::new()];
    let mut depth: u32 = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '-' if next == Some('-') => i = skip_line(&chars, i),
            '#' => i = skip_line(&chars, i),
            '/' if next == Some('*') => i = skip_block_comment(&chars, i + 2),
            '\'' | '"' | '`' => i = skip_quoted(&chars, i + 1, c),
            '$' => match dollar_tag(&chars, i) {
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
                statements.push(Vec::new());
                i += 1;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i]
                    .iter()
                    .collect::<String>()
                    .to_ascii_uppercase();
                if let Some(statement) = statements.last_mut() {
                    statement.push(Token { word, depth });
                }
            }
            _ => i += 1,
        }
    }
    statements
}

fn skip_line(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(chars: &[char], mut i: usize) -> usize {
    let mut nesting = 1;
    while i < chars.len() && nesting > 0 {
        if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
            nesting += 1;
            i += 2;
        } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
            nesting -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

fn skip_quoted(chars: &[char], mut i: usize, quote: char) -> usize {
    while i < chars.len() {
        if chars[i] == '\\' && quote != '`' {
            i += 2;
        } else if chars[i] == quote {
            if chars.get(i + 1) == Some(&quote) {
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

/// Length of a dollar-quote opening tag (`$$` or `$tag$`) starting at `i`.
fn dollar_tag(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if chars.get(j) == Some(&'$') && !chars.get(i + 1).is_some_and(char::is_ascii_digit) {
        Some(j - i + 1)
    } else {
        None
    }
}

fn skip_dollar_quoted(chars: &[char], i: usize, tag_len: usize) -> usize {
    let tag = &chars[i..i + tag_len];
    let mut j = i + tag_len;
    while j + tag_len <= chars.len() {
        if &chars[j..j + tag_len] == tag {
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
            let main = tokens.iter().skip(1).find(|t| {
                t.depth == 0
                    && matches!(
                        t.word.as_str(),
                        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE"
                    )
            });
            match main {
                Some(t) => {
                    let position = tokens.iter().position(|x| std::ptr::eq(x, t)).unwrap_or(0);
                    classify_tokens(&tokens[position..])
                }
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
    use super::StatementKind::*;
    use super::*;

    #[test]
    fn basic_kinds() {
        assert_eq!(classify("select 1"), vec![Select]);
        assert_eq!(classify("INSERT INTO t VALUES (1)"), vec![Insert]);
        assert_eq!(classify("update t set a = 1 where id = 2"), vec![Update]);
        assert_eq!(classify("update t set a = 1"), vec![UpdateAll]);
        assert_eq!(classify("delete from t where id = 1"), vec![Delete]);
        assert_eq!(classify("DELETE FROM t"), vec![DeleteAll]);
        assert_eq!(classify("drop table t"), vec![Drop]);
        assert_eq!(classify("truncate t"), vec![Truncate]);
        assert_eq!(classify("alter table t add column a int"), vec![Ddl]);
        assert_eq!(classify("alter table t drop column a"), vec![AlterDrop]);
        assert_eq!(
            classify("refresh materialized view concurrently mv"),
            vec![Maintenance]
        );
        assert_eq!(classify("explain analyze select 1"), vec![Explain]);
    }

    #[test]
    fn scripts_return_every_statement() {
        assert_eq!(
            classify("begin; delete from t where id = 1; commit;"),
            vec![Transaction, Delete, Transaction]
        );
        assert!(is_destructive("select 1; drop table t"));
        assert!(!is_destructive("select 1; delete from t where id = 1"));
    }

    #[test]
    fn where_inside_a_subquery_does_not_count() {
        assert_eq!(
            classify("delete from t using (select id from u where x = 1) s"),
            vec![DeleteAll]
        );
        assert_eq!(
            classify("update t set a = (select max(b) from u where u.id = 1)"),
            vec![UpdateAll]
        );
    }

    #[test]
    fn keywords_in_literals_and_comments_do_not_count() {
        assert_eq!(
            classify("select 'drop table t; delete from x'"),
            vec![Select]
        );
        assert_eq!(classify("-- drop table t\nselect 1"), vec![Select]);
        assert_eq!(classify("# drop table t\nselect 1"), vec![Select]);
        assert_eq!(
            classify("/* drop /* nested */ table */ select 1"),
            vec![Select]
        );
        assert_eq!(
            classify("select \"where\" from t; select `drop` from u"),
            vec![Select, Select]
        );
        assert_eq!(
            classify("delete from t where note = 'it''s; fine'"),
            vec![Delete]
        );
    }

    #[test]
    fn dollar_quoted_bodies_are_skipped() {
        let sql = "create function f() returns void as $body$ begin delete from t; drop table x; end $body$ language plpgsql";
        assert_eq!(classify(sql), vec![Ddl]);
        assert_eq!(classify("do $$ begin truncate t; end $$"), vec![Call]);
        assert_eq!(classify("select $1, $2 from t where a = $3"), vec![Select]);
    }

    #[test]
    fn ctes_classify_by_their_main_statement() {
        assert_eq!(
            classify("with x as (select 1) select * from x"),
            vec![Select]
        );
        assert_eq!(
            classify("with x as (select id from t) delete from t"),
            vec![DeleteAll]
        );
        assert_eq!(
            classify("with x as (select id from t) delete from t where id in (select id from x)"),
            vec![Delete]
        );
    }

    #[test]
    fn empty_and_whitespace_statements_are_skipped() {
        assert_eq!(
            classify(" ;; \n -- only a comment\n ; "),
            Vec::<StatementKind>::new()
        );
    }

    #[test]
    fn writes_flag() {
        assert!(!Select.writes());
        assert!(Insert.writes());
        assert!(Drop.writes());
        assert!(!Explain.writes());
    }
}
