//! Quoting of identifiers and literals. Every editor, DDL generator and
//! grid edit builds SQL through these functions; nothing formats user text
//! into SQL any other way.

use varve_driver::{Dialect, Value};

use crate::model::{CatalogError, CatalogResult};

/// Quotes an identifier: `"name"` for PostgreSQL, `` `name` `` for MySQL
/// and MariaDB, with the quote character doubled inside.
///
/// # Errors
///
/// [`CatalogError::Invalid`] for an empty name or one containing a NUL
/// character, which no engine accepts.
pub fn ident(dialect: Dialect, name: &str) -> CatalogResult<String> {
    if name.is_empty() || name.contains('\0') {
        return Err(CatalogError::Invalid {
            message: "a name cannot be empty or contain a NUL character".into(),
        });
    }
    let quote = match dialect {
        Dialect::Postgres => '"',
        Dialect::Mysql => '`',
    };
    let mut out = String::with_capacity(name.len() + 2);
    out.push(quote);
    for c in name.chars() {
        if c == quote {
            out.push(quote);
        }
        out.push(c);
    }
    out.push(quote);
    Ok(out)
}

/// Quotes a possibly qualified name: `schema.name` (PostgreSQL) or
/// `database.name` (MySQL, MariaDB). `None` parts are left out.
///
/// # Errors
///
/// As [`ident`].
pub fn qualified(dialect: Dialect, parts: &[Option<&str>]) -> CatalogResult<String> {
    let quoted: Vec<String> = parts
        .iter()
        .flatten()
        .map(|part| ident(dialect, part))
        .collect::<CatalogResult<_>>()?;
    if quoted.is_empty() {
        return Err(CatalogError::Invalid {
            message: "a qualified name needs at least one part".into(),
        });
    }
    Ok(quoted.join("."))
}

/// Quotes a string literal.
///
/// PostgreSQL: `'...'` with `'` doubled (standard conforming strings, the
/// default since 9.1 and required by Varve's sessions).
///
/// MySQL and MariaDB: `'...'` with `'` doubled when the text has no
/// backslash; otherwise `_utf8mb4 X'..'`, because a backslash means
/// different things with and without the `NO_BACKSLASH_ESCAPES` SQL mode
/// and the hex form is read the same way in both.
///
/// # Errors
///
/// [`CatalogError::Invalid`] for a NUL character in PostgreSQL text, which
/// PostgreSQL cannot store.
pub fn string(dialect: Dialect, text: &str) -> CatalogResult<String> {
    match dialect {
        Dialect::Postgres => {
            if text.contains('\0') {
                return Err(CatalogError::Invalid {
                    message: "PostgreSQL text cannot contain a NUL character".into(),
                });
            }
            Ok(format!("'{}'", text.replace('\'', "''")))
        }
        Dialect::Mysql => {
            if text.contains('\\') || text.contains('\0') {
                Ok(format!("_utf8mb4 X'{}'", hex(text.as_bytes())))
            } else {
                Ok(format!("'{}'", text.replace('\'', "''")))
            }
        }
    }
}

/// Quotes PostgreSQL text as a dollar-quoted string (`$$...$$`), which
/// keeps routine bodies readable in previews: nothing inside is escaped.
/// The tag is chosen so it does not occur in `text`: `$$`, then `$body$`,
/// `$body1$`, `$body2$`, and so on.
///
/// # Errors
///
/// [`CatalogError::Invalid`] for a NUL character, which PostgreSQL cannot
/// store.
pub fn dollar_quote(text: &str) -> CatalogResult<String> {
    if text.contains('\0') {
        return Err(CatalogError::Invalid {
            message: "PostgreSQL text cannot contain a NUL character".into(),
        });
    }
    let mut tag = String::from("$$");
    let mut n = 0u32;
    // The closing tag must be the first occurrence of the tag after the
    // opening one: not inside the text, and not formed by the end of the
    // text plus the start of the tag (`...$` before `$$`).
    while format!("{text}{tag}").find(&tag) != Some(text.len()) {
        tag = if n == 0 {
            "$body$".to_string()
        } else {
            format!("$body{n}$")
        };
        n += 1;
    }
    Ok(format!("{tag}{text}{tag}"))
}

/// The SQL literal for a value, for `INSERT`, `UPDATE` and `WHERE` clauses.
/// Typed values (dates, UUIDs, JSON, intervals, ...) become string literals
/// that the server converts to the column's type.
///
/// # Errors
///
/// [`CatalogError::Invalid`] for values the dialect cannot express: a
/// non-finite float in MySQL, a decimal that is not a number, or an array
/// in MySQL.
pub fn literal(dialect: Dialect, value: &Value) -> CatalogResult<String> {
    Ok(match value {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if f.is_finite() => format!("{f:?}"),
        Value::Float(f) => match dialect {
            Dialect::Postgres => {
                let text = if f.is_nan() {
                    "NaN"
                } else if *f > 0.0 {
                    "Infinity"
                } else {
                    "-Infinity"
                };
                format!("'{text}'")
            }
            Dialect::Mysql => {
                return Err(CatalogError::Invalid {
                    message: "MySQL and MariaDB cannot store NaN or infinity".into(),
                });
            }
        },
        Value::Decimal(d) => {
            if !is_decimal(d) {
                return Err(CatalogError::Invalid {
                    message: format!("{d} is not a decimal number"),
                });
            }
            d.clone()
        }
        Value::Bytes(bytes) => match dialect {
            Dialect::Postgres => format!("'\\x{}'::pg_catalog.bytea", hex(bytes)),
            Dialect::Mysql => format!("X'{}'", hex(bytes)),
        },
        Value::Json(json) => string(dialect, json.as_str())?,
        Value::Array(items) => match dialect {
            Dialect::Postgres => string(dialect, &pg_array_text(items)?)?,
            Dialect::Mysql => {
                return Err(CatalogError::Invalid {
                    message: "MySQL and MariaDB have no array values".into(),
                });
            }
        },
        Value::Text(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::TimestampTz(s)
        | Value::Interval(s)
        | Value::Uuid(s)
        | Value::Other(s) => string(dialect, s)?,
    })
}

/// PostgreSQL array text form (`{a,"b c",NULL}`), elements quoted when
/// needed.
fn pg_array_text(items: &[Value]) -> CatalogResult<String> {
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        parts.push(match item {
            Value::Null => "NULL".to_string(),
            Value::Array(inner) => pg_array_text(inner)?,
            other => {
                let text = other.to_plain_text().unwrap_or_default();
                if text.contains('\0') {
                    return Err(CatalogError::Invalid {
                        message: "PostgreSQL text cannot contain a NUL character".into(),
                    });
                }
                let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
                format!("\"{escaped}\"")
            }
        });
    }
    Ok(format!("{{{}}}", parts.join(",")))
}

fn is_decimal(text: &str) -> bool {
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    let mut digits = 0;
    let mut dots = 0;
    for c in body.chars() {
        match c {
            '0'..='9' => digits += 1,
            '.' => dots += 1,
            _ => return false,
        }
    }
    digits > 0 && dots <= 1
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Dialect::{Mysql, Postgres};

    #[test]
    fn identifiers_double_their_quote() -> CatalogResult<()> {
        assert_eq!(ident(Postgres, r#"my "t""#)?, r#""my ""t""""#);
        assert_eq!(ident(Mysql, "a`b")?, "`a``b`");
        assert!(ident(Postgres, "").is_err());
        assert!(ident(Mysql, "a\0").is_err());
        assert_eq!(
            qualified(Postgres, &[Some("public"), Some("t")])?,
            r#""public"."t""#
        );
        assert_eq!(qualified(Mysql, &[None, Some("t")])?, "`t`");
        assert!(qualified(Mysql, &[None]).is_err());
        Ok(())
    }

    #[test]
    fn strings_are_safe_in_every_mode() -> CatalogResult<()> {
        assert_eq!(string(Postgres, "it's \\n")?, "'it''s \\n'");
        assert!(string(Postgres, "a\0b").is_err());
        assert_eq!(string(Mysql, "it's")?, "'it''s'");
        assert_eq!(string(Mysql, "a\\b")?, "_utf8mb4 X'615c62'");
        assert_eq!(string(Mysql, "a\0")?, "_utf8mb4 X'6100'");
        Ok(())
    }

    #[test]
    fn dollar_quotes_pick_a_free_tag() -> CatalogResult<()> {
        assert_eq!(dollar_quote("select 'a'")?, "$$select 'a'$$");
        assert_eq!(dollar_quote("a $$ b")?, "$body$a $$ b$body$");
        assert_eq!(dollar_quote("x $$ $body$")?, "$body1$x $$ $body$$body1$");
        assert_eq!(dollar_quote("ends with $")?, "$body$ends with $$body$");
        assert!(dollar_quote("a\0").is_err());
        Ok(())
    }

    #[test]
    fn literals() -> CatalogResult<()> {
        assert_eq!(literal(Postgres, &Value::Null)?, "NULL");
        assert_eq!(literal(Mysql, &Value::Bool(true))?, "TRUE");
        assert_eq!(literal(Postgres, &Value::Float(1.5))?, "1.5");
        assert_eq!(literal(Postgres, &Value::Float(f64::NAN))?, "'NaN'");
        assert!(literal(Mysql, &Value::Float(f64::INFINITY)).is_err());
        assert_eq!(literal(Mysql, &Value::Decimal("-12.50".into()))?, "-12.50");
        assert!(literal(Mysql, &Value::Decimal("1; drop".into())).is_err());
        assert_eq!(
            literal(Postgres, &Value::Bytes(vec![0, 255]))?,
            "'\\x00ff'::pg_catalog.bytea"
        );
        assert_eq!(literal(Mysql, &Value::Bytes(vec![0, 255]))?, "X'00ff'");
        assert_eq!(literal(Postgres, &Value::Uuid("u".into()))?, "'u'");
        assert_eq!(
            literal(
                Postgres,
                &Value::Array(vec![Value::Text("a\"b".into()), Value::Null, Value::Int(3)])
            )?,
            r#"'{"a\"b",NULL,"3"}'"#
        );
        assert!(literal(Mysql, &Value::Array(vec![])).is_err());
        assert_eq!(
            literal(Postgres, &Value::json(r#"{"b":1,"a":2}"#))?,
            r#"'{"b":1,"a":2}'"#
        );
        Ok(())
    }
}
