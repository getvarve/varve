//! Helpers shared by the relations loaders and DDL builders: quoting
//! shortcuts, typed reads of catalog rows, the schema-qualifying search
//! path guard and small parsers for server output.

use serde::de::DeserializeOwned;
use varve_driver::{Dialect, Session, Value};

use crate::model::{CatalogError, CatalogResult};
use crate::postgres::programs::{fetch_pinned, fetch_pinned_with};
use crate::rows::{Row, Rows};
use crate::sql;

use super::types::{PgOption, PgQualifiedName};

/// A quoted identifier.
pub(super) fn ident(name: &str) -> CatalogResult<String> {
    sql::ident(Dialect::Postgres, name)
}

/// A quoted `schema.name`.
pub(super) fn qname(schema: &str, name: &str) -> CatalogResult<String> {
    sql::qualified(Dialect::Postgres, &[Some(schema), Some(name)])
}

/// A quoted qualified name from a [`PgQualifiedName`].
pub(super) fn qualified(name: &PgQualifiedName) -> CatalogResult<String> {
    qname(&name.schema, &name.name)
}

/// A quoted string literal.
pub(super) fn lit(text: &str) -> CatalogResult<String> {
    sql::string(Dialect::Postgres, text)
}

/// A text column that must be present.
pub(super) fn text(row: &Row<'_>, column: &str) -> CatalogResult<String> {
    row.req_text(column)
}

/// A nullable text column.
pub(super) fn opt_text(row: &Row<'_>, column: &str) -> CatalogResult<Option<String>> {
    row.text(column)
}

/// A nullable text column where an empty string also means "none".
pub(super) fn non_empty(row: &Row<'_>, column: &str) -> CatalogResult<Option<String>> {
    Ok(row.text(column)?.filter(|s| !s.is_empty()))
}

/// A boolean column; `NULL` reads as `false`.
pub(super) fn flag(row: &Row<'_>, column: &str) -> CatalogResult<bool> {
    Ok(row.bool(column)?.unwrap_or(false))
}

/// An integer column that must be present.
pub(super) fn int(row: &Row<'_>, column: &str) -> CatalogResult<i64> {
    row.int(column)?.ok_or_else(|| CatalogError::Unexpected {
        message: format!("column {column} is NULL"),
    })
}

/// A nullable integer column.
pub(super) fn opt_int(row: &Row<'_>, column: &str) -> CatalogResult<Option<i64>> {
    row.int(column)
}

/// A text array column; `NULL` reads as empty.
pub(super) fn text_list(row: &Row<'_>, column: &str) -> CatalogResult<Vec<String>> {
    match row.value(column)? {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => Ok(items.iter().filter_map(Value::to_plain_text).collect()),
        other => Err(CatalogError::Unexpected {
            message: format!("column {column} is not an array: {other:?}"),
        }),
    }
}

/// A JSON column decoded into `T`; `NULL` reads as `T::default()`.
pub(super) fn json<T: DeserializeOwned + Default>(row: &Row<'_>, column: &str) -> CatalogResult<T> {
    let text = match row.value(column)? {
        Value::Null => return Ok(T::default()),
        Value::Json(json) => json.as_str().to_owned(),
        Value::Text(text) | Value::Other(text) => text.clone(),
        other => {
            return Err(CatalogError::Unexpected {
                message: format!("column {column} is not JSON: {other:?}"),
            });
        }
    };
    serde_json::from_str(&text).map_err(|error| CatalogError::Unexpected {
        message: format!("column {column} is not the expected JSON: {error}"),
    })
}

/// SQL that is true when the relation `c` in namespace `n` belongs to the
/// system or an extension.
pub(super) fn system_relation(c: &str, n: &str) -> String {
    format!(
        "({n}.nspname OPERATOR(pg_catalog.=) ANY (ARRAY['pg_catalog', 'information_schema']::pg_catalog.name[]) \
         OR {n}.nspname OPERATOR(pg_catalog.~~) 'pg\\_toast%' OR {n}.nspname OPERATOR(pg_catalog.~~) 'pg\\_temp\\_%' \
         OR EXISTS (SELECT 1 FROM pg_catalog.pg_depend xd \
                    WHERE xd.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass \
                      AND xd.objid OPERATOR(pg_catalog.=) {c}.oid AND xd.deptype OPERATOR(pg_catalog.=) 'e'))"
    )
}

/// SQL that is true when the schema `n` belongs to the system or an
/// extension.
pub(super) fn system_schema(n: &str) -> String {
    format!(
        "({n}.nspname OPERATOR(pg_catalog.=) ANY (ARRAY['pg_catalog', 'information_schema']::pg_catalog.name[]) \
         OR {n}.nspname OPERATOR(pg_catalog.~~) 'pg\\_toast%' OR {n}.nspname OPERATOR(pg_catalog.~~) 'pg\\_temp\\_%' \
         OR EXISTS (SELECT 1 FROM pg_catalog.pg_depend xd \
                    WHERE xd.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_namespace'::pg_catalog.regclass \
                      AND xd.objid OPERATOR(pg_catalog.=) {n}.oid AND xd.deptype OPERATOR(pg_catalog.=) 'e'))"
    )
}

/// SQL producing a JSON array of `{name, value}` objects from an option
/// array expression, with an optional name prefix, in stored order (so DDL
/// built from it writes the options as the server does).
pub(super) fn options_json(array: &str, prefix: &str) -> String {
    format!(
        "(SELECT pg_catalog.json_agg(pg_catalog.json_build_object('name', '{prefix}' OPERATOR(pg_catalog.||) o.option_name, \
         'value', o.option_value) ORDER BY o.ord) \
         FROM pg_catalog.pg_options_to_table({array}) WITH ORDINALITY o(option_name, option_value, ord))"
    )
}

/// `WITH (name = 'value', ...)` style list of options; names are quoted
/// per dotted part.
pub(super) fn option_list(options: &[PgOption], separator: &str) -> CatalogResult<String> {
    let mut parts = Vec::with_capacity(options.len());
    for option in options {
        let name = option
            .name
            .split('.')
            .map(ident)
            .collect::<CatalogResult<Vec<_>>>()?
            .join(".");
        parts.push(format!("{name}{separator}{}", lit(&option.value)?));
    }
    Ok(parts.join(", "))
}

/// How a catalog statement runs (see [`fetch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pin {
    /// Every list, count, detail and DDL query: the programs catalog's
    /// shared pin ([`fetch_pinned`]), so both modules behave the same.
    Catalog,
    /// Exact sizes: as [`Pin::Catalog`] with a much shorter lock timeout;
    /// a lock timeout here means "size unavailable", not a failure.
    Sizes,
}

/// Lock timeout of the exact size statement.
const SIZE_LOCK_TIMEOUT: &str = "200ms";

/// SQLSTATE of `lock_timeout`.
pub(super) const LOCK_NOT_AVAILABLE: &str = "55P03";

/// True for a lock timeout error.
pub(super) fn is_lock_timeout(error: &CatalogError) -> bool {
    matches!(error, CatalogError::Driver { error } if error.code.as_deref() == Some(LOCK_NOT_AVAILABLE))
}

/// Runs one catalog query. `order` sorts the output by output columns
/// (`q.<column>`).
///
/// Every query runs through the programs catalog's
/// [`fetch_pinned`]: schema-qualified names, a statement-local lock
/// timeout, and a refusal inside a transaction block (the app reads the
/// catalog on a metadata session that is never in a user transaction).
/// [`Pin::Sizes`] only shortens the lock timeout.
///
/// # Errors
///
/// Driver errors; a lock wait as a [`varve_driver::ErrorKind::Timeout`] driver error
/// with SQLSTATE `55P03`; [`CatalogError::Invalid`] inside a transaction
/// block.
pub(super) async fn fetch(
    session: &dyn Session,
    pin: Pin,
    sql: &str,
    order: &str,
) -> CatalogResult<Rows> {
    let statements = [(sql.to_owned(), order.to_owned())];
    let sets = match pin {
        Pin::Catalog => fetch_pinned(session, &statements).await?,
        Pin::Sizes => fetch_pinned_with(session, &statements, SIZE_LOCK_TIMEOUT).await?,
    };
    sets.into_iter()
        .next()
        .ok_or_else(|| CatalogError::Unexpected {
            message: "the catalog query returned no result set".into(),
        })
}

/// Strips whitespace and one trailing semicolon from server-printed SQL.
pub(super) fn strip_semicolon(sql: &str) -> String {
    let trimmed = sql.trim();
    trimmed
        .strip_suffix(';')
        .unwrap_or(trimmed)
        .trim_end()
        .to_owned()
}

/// The `WHEN (...)` condition of a `pg_get_triggerdef` result, without the
/// outer parentheses. Quoted identifiers and string literals are skipped,
/// so text inside them never matches.
pub(super) fn trigger_condition(definition: &str) -> Option<String> {
    const MARKER: &str = " WHEN (";
    let mut quote: Option<char> = None;
    let mut start = None;
    for (i, c) in definition.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if definition[i..].starts_with(MARKER) => {
                start = Some(i + MARKER.len() - 1);
                break;
            }
            None => {}
        }
    }
    let open = start?;
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (offset, c) in definition[open..].char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let inner = &definition[open + 1..open + offset];
                        return Some(inner.trim().to_owned());
                    }
                }
                _ => {}
            },
        }
    }
    None
}

/// Configuration parameters whose stored value is a list of separately
/// quoted elements (the server's `GUC_LIST_QUOTE` parameters).
const LIST_QUOTED: &[&str] = &[
    "local_preload_libraries",
    "search_path",
    "session_preload_libraries",
    "shared_preload_libraries",
    "temp_tablespaces",
    "unix_socket_directories",
];

/// The value part of `SET name TO value` for a stored setting.
///
/// List parameters are stored as the server flattened them (`"$user",
/// public`); each element is split out and quoted as its own literal, as
/// `pg_dump` does. Every other value becomes one string literal.
pub(super) fn setting_value(name: &str, value: &str) -> CatalogResult<String> {
    if !LIST_QUOTED.contains(&name.to_ascii_lowercase().as_str()) {
        return lit(value);
    }
    let elements = split_guc_list(value);
    if elements.is_empty() {
        return lit("");
    }
    Ok(elements
        .iter()
        .map(|e| lit(e))
        .collect::<CatalogResult<Vec<_>>>()?
        .join(", "))
}

/// Splits a flattened list setting into its elements: comma separated,
/// surrounding whitespace dropped, double-quoted elements unquoted with
/// `""` read as `"`.
fn split_guc_list(value: &str) -> Vec<String> {
    let mut elements = Vec::new();
    let mut chars = value.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut element = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        element.push('"');
                    } else {
                        break;
                    }
                } else {
                    element.push(c);
                }
            }
            while chars.peek().is_some_and(|c| *c != ',') {
                chars.next();
            }
        } else {
            while let Some(c) = chars.peek() {
                if *c == ',' {
                    break;
                }
                element.push(*c);
                chars.next();
            }
            element = element.trim_end().to_owned();
        }
        elements.push(element);
        if chars.next().is_none() {
            break;
        }
    }
    elements
}

/// Byte count in the units `pg_size_pretty` uses.
pub(super) fn pretty_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if bytes.unsigned_abs() < 10 * 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes;
    let mut unit = UNITS[0];
    for (i, name) in UNITS.iter().enumerate() {
        // Halve-round like the server: divide by 1024 with rounding.
        value = (value + value.signum() * 512) / 1024;
        unit = name;
        if value.unsigned_abs() < 10 * 1024 || i == UNITS.len() - 1 {
            break;
        }
    }
    format!("{value} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_trigger_condition_outside_quotes() {
        let def = "CREATE TRIGGER \"x WHEN (\" BEFORE UPDATE ON s.t FOR EACH ROW \
                   WHEN ((old.b IS DISTINCT FROM new.b) AND ('a ) b'::text <> 'y')) \
                   EXECUTE FUNCTION s.f('WHEN (')";
        assert_eq!(
            trigger_condition(def).as_deref(),
            Some("(old.b IS DISTINCT FROM new.b) AND ('a ) b'::text <> 'y')")
        );
        assert_eq!(
            trigger_condition("CREATE TRIGGER t AFTER INSERT ON s.t EXECUTE FUNCTION f()"),
            None
        );
    }

    #[test]
    fn list_settings_quote_each_element() -> CatalogResult<()> {
        assert_eq!(
            setting_value("search_path", "\"$user\", public, \"a\"\"b\"")?,
            "'$user', 'public', 'a\"b'"
        );
        assert_eq!(setting_value("work_mem", "8MB")?, "'8MB'");
        assert_eq!(setting_value("search_path", "")?, "''");
        Ok(())
    }

    #[test]
    fn option_lists_quote_names_and_values() -> CatalogResult<()> {
        let options = vec![
            PgOption {
                name: "fillfactor".into(),
                value: "70".into(),
            },
            PgOption {
                name: "toast.autovacuum_enabled".into(),
                value: "it's".into(),
            },
        ];
        assert_eq!(
            option_list(&options, "=")?,
            "\"fillfactor\"='70', \"toast\".\"autovacuum_enabled\"='it''s'"
        );
        assert_eq!(option_list(&options[..1], " ")?, "\"fillfactor\" '70'");
        Ok(())
    }

    #[test]
    fn sizes_read_like_pg_size_pretty() {
        assert_eq!(pretty_bytes(8192), "8192 bytes");
        assert_eq!(pretty_bytes(16384), "16 kB");
        assert_eq!(pretty_bytes(10 * 1024 * 1024), "10 MB");
        assert_eq!(pretty_bytes(-20_000), "-20 kB");
    }

    #[test]
    fn strips_one_trailing_semicolon() {
        assert_eq!(strip_semicolon(" SELECT 1;\n"), "SELECT 1");
        assert_eq!(strip_semicolon("SELECT 1"), "SELECT 1");
    }
}
