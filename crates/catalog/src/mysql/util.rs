//! Shared helpers of the MySQL and MariaDB catalog: quoting, reading
//! catalog values, version checks and error mapping.

use varve_core::Engine;
use varve_driver::{Dialect, ServerInfo, Session, Value};

use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef};
use crate::rows::{Row, Rows};

/// Databases owned by the server.
pub(crate) const SYSTEM_DATABASES: &[&str] =
    &["mysql", "sys", "information_schema", "performance_schema"];

/// True for a system database. The comparison is exact: the server names
/// its databases in lower case, and with `lower_case_table_names = 0`
/// (Linux) a user database `MySQL` is not the system one. With 1 or 2 the
/// server never reports another spelling of these names.
pub(crate) fn is_system_db(name: &str) -> bool {
    SYSTEM_DATABASES.contains(&name)
}

/// True for MariaDB.
pub(crate) fn is_mariadb(server: &ServerInfo) -> bool {
    server.engine == Engine::Mariadb
}

/// True when the server is at least `major.minor.patch` (a missing patch
/// counts as 0).
pub(crate) fn at_least(server: &ServerInfo, major: u32, minor: u32, patch: u32) -> bool {
    (server.major, server.minor, server.patch.unwrap_or(0)) >= (major, minor, patch)
}

/// The version as executable comments write it:
/// `major * 10000 + minor * 100 + patch`.
pub(crate) fn version_number(server: &ServerInfo) -> u32 {
    server
        .major
        .saturating_mul(10_000)
        .saturating_add(server.minor.saturating_mul(100))
        .saturating_add(server.patch.unwrap_or(0))
}

/// A quoted identifier.
pub(crate) fn id(name: &str) -> CatalogResult<String> {
    crate::sql::ident(Dialect::Mysql, name)
}

/// A quoted `db.name`.
pub(crate) fn qname(database: &str, name: &str) -> CatalogResult<String> {
    crate::sql::qualified(Dialect::Mysql, &[Some(database), Some(name)])
}

/// A string literal.
pub(crate) fn lit(text: &str) -> CatalogResult<String> {
    crate::sql::string(Dialect::Mysql, text)
}

/// A column value as text. Binary values (some catalog columns are
/// `varbinary`) are read as UTF-8.
pub(crate) fn text(row: &Row<'_>, column: &str) -> CatalogResult<Option<String>> {
    Ok(match row.value(column)? {
        Value::Null => None,
        Value::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        other => other.to_plain_text(),
    })
}

/// A column value as text, `None` for `NULL` and empty text.
pub(crate) fn text_nonempty(row: &Row<'_>, column: &str) -> CatalogResult<Option<String>> {
    Ok(text(row, column)?.filter(|t| !t.is_empty()))
}

/// A column value as text that must not be `NULL`.
pub(crate) fn req(row: &Row<'_>, column: &str) -> CatalogResult<String> {
    text(row, column)?.ok_or_else(|| CatalogError::Unexpected {
        message: format!("column {column} is NULL"),
    })
}

/// A column value as an unsigned integer.
pub(crate) fn uint(row: &Row<'_>, column: &str) -> CatalogResult<Option<u64>> {
    let bad = || CatalogError::Unexpected {
        message: format!("column {column} is not an unsigned integer"),
    };
    match row.value(column)? {
        Value::Null => Ok(None),
        Value::UInt(u) => Ok(Some(*u)),
        Value::Int(i) => u64::try_from(*i).map(Some).map_err(|_| bad()),
        Value::Bool(b) => Ok(Some(u64::from(*b))),
        Value::Float(f) if f.is_finite() && *f >= 0.0 && f.fract() == 0.0 => {
            format!("{f:.0}").parse().map(Some).map_err(|_| bad())
        }
        Value::Decimal(s) | Value::Text(s) | Value::Other(s) => {
            let s = s.trim();
            let whole = s.split_once('.').map_or(s, |(whole, _)| whole);
            whole.parse().map(Some).map_err(|_| bad())
        }
        Value::Bytes(b) => String::from_utf8_lossy(b)
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| bad()),
        _ => Err(bad()),
    }
}

/// A column value as a `u32`.
pub(crate) fn uint32(row: &Row<'_>, column: &str) -> CatalogResult<Option<u32>> {
    uint(row, column)?
        .map(|v| {
            u32::try_from(v).map_err(|_| CatalogError::Unexpected {
                message: format!("column {column} is out of range"),
            })
        })
        .transpose()
}

/// True when a `YES`/`NO` (or boolean) column says yes.
pub(crate) fn yes(row: &Row<'_>, column: &str) -> CatalogResult<bool> {
    Ok(match row.value(column)? {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::UInt(u) => *u != 0,
        _ => text(row, column)?.is_some_and(|t| {
            let t = t.trim();
            t.eq_ignore_ascii_case("YES") || t == "1" || t.eq_ignore_ascii_case("TRUE")
        }),
    })
}

/// The MySQL error number of a driver error (`1146 (42S02)` gives 1146).
pub(crate) fn error_number(error: &CatalogError) -> Option<u32> {
    let CatalogError::Driver { error } = error else {
        return None;
    };
    let code = error.code.as_deref()?;
    code.split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|n| n.parse().ok())
}

/// Server errors that mean "this object does not exist".
const MISSING_ERRORS: &[u32] = &[
    1049, // ER_BAD_DB_ERROR
    1146, // ER_NO_SUCH_TABLE
    1305, // ER_SP_DOES_NOT_EXIST (routines, packages)
    1347, // ER_WRONG_OBJECT ("is not VIEW" / "is not SEQUENCE")
    1360, // ER_TRG_DOES_NOT_EXIST
    1539, // ER_EVENT_DOES_NOT_EXIST
    4089, // MariaDB ER_NOT_SEQUENCE ("is not a SEQUENCE")
    4090, // MariaDB ER_NOT_SEQUENCE2
    4091, // MariaDB ER_UNKNOWN_SEQUENCES
];

/// Turns "does not exist" server errors into [`CatalogError::NotFound`].
pub(crate) fn missing_as_not_found(error: CatalogError, what: &str) -> CatalogError {
    match error_number(&error) {
        Some(code) if MISSING_ERRORS.contains(&code) => CatalogError::NotFound {
            message: format!("{what} was not found"),
        },
        _ => error,
    }
}

/// Runs catalog SQL and returns every result set, mapping "does not exist"
/// errors to [`CatalogError::NotFound`].
pub(crate) async fn query_all(
    session: &dyn Session,
    sql: &str,
    what: &str,
) -> CatalogResult<Vec<Rows>> {
    let output = crate::rows::run(session, sql)
        .await
        .map_err(|e| missing_as_not_found(e, what))?;
    Rows::all(output)
}

/// Runs one catalog query.
pub(crate) async fn query_one(session: &dyn Session, sql: &str, what: &str) -> CatalogResult<Rows> {
    query_all(session, sql, what)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| CatalogError::Unexpected {
            message: "the query returned no result set".into(),
        })
}

/// Takes result set `index` out of a batch.
pub(crate) fn take(sets: &mut [Rows], index: usize) -> CatalogResult<Rows> {
    sets.get_mut(index)
        .map(std::mem::take)
        .ok_or_else(|| CatalogError::Unexpected {
            message: format!("result set {index} is missing"),
        })
}

/// The database of a reference.
pub(crate) fn database_of(object: &ObjectRef) -> CatalogResult<&str> {
    object
        .database
        .as_deref()
        .ok_or_else(|| CatalogError::Invalid {
            message: format!("a {:?} needs a database", object.kind),
        })
}

/// The parent table of a reference.
pub(crate) fn parent_of(object: &ObjectRef) -> CatalogResult<&str> {
    object
        .parent
        .as_deref()
        .ok_or_else(|| CatalogError::Invalid {
            message: format!("a {:?} needs its table as parent", object.kind),
        })
}

/// A reference to an object in a database.
pub(crate) fn object_ref(
    kind: ObjectKind,
    database: &str,
    parent: Option<&str>,
    name: &str,
) -> ObjectRef {
    ObjectRef {
        kind,
        database: Some(database.to_owned()),
        schema: None,
        parent: parent.map(str::to_owned),
        name: name.to_owned(),
        signature: None,
    }
}

/// The character set of a collation name (`utf8mb4_0900_ai_ci` gives
/// `utf8mb4`; every collation name starts with its character set).
pub(crate) fn charset_of(collation: &str) -> String {
    collation
        .split_once('_')
        .map_or(collation, |(charset, _)| charset)
        .to_owned()
}

/// Human-readable byte size.
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS.get(unit).copied().unwrap_or("B"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_driver::{DriverError, ErrorKind};

    #[test]
    fn helpers() {
        assert!(is_system_db("mysql"));
        assert!(!is_system_db("MySQL"));
        assert!(!is_system_db("app"));
        assert_eq!(charset_of("utf8mb4_0900_ai_ci"), "utf8mb4");
        assert_eq!(charset_of("binary"), "binary");
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(16_384), "16.0 KB");
        let error = CatalogError::from(
            DriverError::new(ErrorKind::Query, "no table").with_code("1146 (42S02)"),
        );
        assert_eq!(error_number(&error), Some(1146));
        assert!(matches!(
            missing_as_not_found(error, "t"),
            CatalogError::NotFound { .. }
        ));
    }
}
