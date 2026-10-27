//! Checks a raw SQL cell expression ([`super::CellValue::Expression`])
//! before it is placed, in parentheses, into a generated statement.
//!
//! The expression is the user's own SQL, so it is not quoted. What must
//! never happen is that it reaches outside its parentheses: closing them
//! and adding a `WHERE`, ending the statement, or commenting out the rest
//! of it (which could drop the key condition of an `UPDATE`). The scanner
//! therefore requires every quote and dollar quote to be closed,
//! parentheses to stay balanced and never go below zero, and refuses
//! comments and `;` outside literals. MySQL strings with a backslash are
//! refused because their extent depends on the `NO_BACKSLASH_ESCAPES` SQL
//! mode.

use varve_driver::Dialect;

use crate::model::{CatalogError, CatalogResult};

/// Checks that `text` is one self-contained expression for `dialect`.
///
/// # Errors
///
/// [`CatalogError::Invalid`] with what is wrong.
pub(crate) fn check_expression(dialect: Dialect, text: &str) -> CatalogResult<()> {
    let invalid = |message: &str| {
        Err(CatalogError::Invalid {
            message: format!("the SQL expression {message}"),
        })
    };
    if text.trim().is_empty() {
        return invalid("is empty");
    }
    if text.contains('\0') {
        return invalid("contains a NUL character");
    }
    let chars: Vec<char> = text.chars().collect();
    if dialect == Dialect::Mysql && has_backslash_in_string(&chars) {
        return invalid(
            "has a backslash inside a string, which MySQL reads differently depending on the \
             NO_BACKSLASH_ESCAPES SQL mode; use a hex literal (X'...') instead",
        );
    }
    let mut depth = 0usize;
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        let next = chars.get(i + 1).copied();
        match c {
            '\'' => {
                let escapes = dialect == Dialect::Postgres && escape_string_prefix(&chars, i);
                i = match closing_quote(&chars, i, '\'', escapes) {
                    Some(end) => end,
                    None => return invalid("has a string that is not closed"),
                };
                continue;
            }
            '"' => {
                i = match closing_quote(&chars, i, '"', false) {
                    Some(end) => end,
                    None => return invalid("has a quoted name or string that is not closed"),
                };
                continue;
            }
            '`' if dialect == Dialect::Mysql => {
                i = match closing_quote(&chars, i, '`', false) {
                    Some(end) => end,
                    None => return invalid("has a quoted name that is not closed"),
                };
                continue;
            }
            '$' if dialect == Dialect::Postgres => {
                if let Some(tag_end) = dollar_tag(&chars, i) {
                    let tag: String = chars[i..=tag_end].iter().collect();
                    match find(&chars, tag_end + 1, &tag) {
                        Some(close) => {
                            i = close + tag.chars().count();
                            continue;
                        }
                        None => return invalid("has a dollar-quoted string that is not closed"),
                    }
                }
            }
            '-' if next == Some('-') => return invalid("cannot contain a comment"),
            '/' if next == Some('*') => return invalid("cannot contain a comment"),
            '#' if dialect == Dialect::Mysql => return invalid("cannot contain a comment"),
            ';' => return invalid("cannot contain ';'"),
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return invalid("closes a parenthesis it did not open");
                }
                depth -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return invalid("has a parenthesis that is not closed");
    }
    Ok(())
}

/// True when the quote at `quote` starts a PostgreSQL `E'...'` string: it
/// directly follows `E` or `e`, which is not part of a longer word.
fn escape_string_prefix(chars: &[char], quote: usize) -> bool {
    let Some(prefix_at) = quote.checked_sub(1) else {
        return false;
    };
    if !matches!(chars.get(prefix_at), Some('e' | 'E')) {
        return false;
    }
    match prefix_at.checked_sub(1).and_then(|i| chars.get(i)) {
        None => true,
        Some(c) => !is_word_char(*c),
    }
}

/// Index after the closing quote of a quoted text that starts at `start`.
/// A doubled quote stands for itself; with `escapes`, a backslash escapes
/// the next character.
fn closing_quote(chars: &[char], start: usize, quote: char, escapes: bool) -> Option<usize> {
    let mut i = start + 1;
    while let Some(&c) = chars.get(i) {
        if escapes && c == '\\' {
            i += 2;
            continue;
        }
        if c == quote {
            if chars.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// When a PostgreSQL dollar quote (`$$` or `$tag$`) starts at `start`, the
/// index of its closing `$`. A `$` inside a word (`a$b`) or before a digit
/// (`$1`) does not start one.
fn dollar_tag(chars: &[char], start: usize) -> Option<usize> {
    if start
        .checked_sub(1)
        .and_then(|i| chars.get(i))
        .is_some_and(|c| is_word_char(*c))
    {
        return None;
    }
    let mut i = start + 1;
    match chars.get(i) {
        Some('$') => return Some(i),
        Some(c) if c.is_alphabetic() || *c == '_' => {}
        _ => return None,
    }
    while let Some(&c) = chars.get(i) {
        if c == '$' {
            return Some(i);
        }
        if !(c.is_alphanumeric() || c == '_') {
            return None;
        }
        i += 1;
    }
    None
}

/// Index of the first occurrence of `needle` at or after `from`.
fn find(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || from > chars.len() {
        return None;
    }
    chars[from..]
        .windows(needle.len())
        .position(|w| w == needle.as_slice())
        .map(|offset| from + offset)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// MySQL: true when a `'...'` or `"..."` string holds a backslash.
fn has_backslash_in_string(chars: &[char]) -> bool {
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        match c {
            '\'' | '"' | '`' => {
                let end = closing_quote(chars, i, c, false).unwrap_or(chars.len());
                if c != '`' && chars[i..end].contains(&'\\') {
                    return true;
                }
                i = end;
            }
            _ => i += 1,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use Dialect::{Mysql, Postgres};

    fn ok(dialect: Dialect, text: &str) -> bool {
        check_expression(dialect, text).is_ok()
    }

    #[test]
    fn plain_expressions_pass() {
        for text in [
            "now()",
            "upper('it''s')",
            "(SELECT max(id) FROM t) + 1",
            "'a;b' || \"weird;name\"",
            "'--not a comment'",
            "1 - -1",
        ] {
            assert!(ok(Postgres, text), "{text}");
            assert!(ok(Mysql, text), "{text}");
        }
        assert!(ok(Postgres, "$$a ') -- ;$$ || $tag$x$$y$tag$"));
        assert!(ok(Postgres, "E'it\\'s ) ;'"));
        assert!(ok(Postgres, "a$b + $1"));
        assert!(ok(Postgres, "2 # 3"));
        assert!(ok(Mysql, "`we;ird` + 1"));
        assert!(ok(Mysql, "X'00ff'"));
    }

    #[test]
    fn expressions_cannot_escape_their_parentheses() {
        for text in [
            "",
            "   ",
            "1) WHERE (true",
            "1); DROP TABLE t",
            "(1",
            "1)",
            "'open",
            "\"open",
            "1 -- rest",
            "1 /* rest",
            "1;",
        ] {
            assert!(!ok(Postgres, text), "{text}");
            assert!(!ok(Mysql, text), "{text}");
        }
        assert!(!ok(Postgres, "$$open"));
        assert!(!ok(Postgres, "$a$ x $b$"));
        assert!(
            !ok(Postgres, "'standard \\' ) ; '"),
            "backslash is literal in '...'"
        );
        assert!(!ok(Mysql, "1 # rest"));
        assert!(!ok(Mysql, "`open"));
        assert!(!ok(Mysql, "'a\\'b'"));
        assert!(!ok(Mysql, "\"a\\\" ) ; \""));
        assert!(!ok(Postgres, "a\0"));
    }
}
