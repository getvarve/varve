//! Helpers shared by the program catalog modules (and, re-exported, by
//! other catalog modules): running catalog queries with a pinned
//! `search_path`, quoting, reading values, system object
//! detection and generic options.

use serde::{Deserialize, Serialize};
use varve_driver::{Dialect, DriverError, ErrorKind, Session, Value};

use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef, ObjectSummary};
use crate::rows::{self, Row, Rows};
use crate::sql;

/// The dialect of everything in this module.
pub(super) const D: Dialect = Dialect::Postgres;

/// The `search_path` catalog queries run with: `format_type`, `regproc`,
/// `pg_get_functiondef` and `pg_get_expr` then qualify every name outside
/// `pg_catalog`, so details and generated DDL do not depend on the user's
/// `search_path`. `pg_temp` is listed last so a temporary object cannot
/// shadow a catalog.
const PINNED_SEARCH_PATH: &str = "pg_catalog, pg_temp";

/// How long a catalog statement waits for a lock before it gives up:
/// deparsing (`pg_get_expr`, `pg_get_*def`) and size functions take locks
/// on relations, and a session holding an `ACCESS EXCLUSIVE` lock would
/// otherwise make the object browser hang.
pub const LOCK_TIMEOUT: &str = "2s";

/// True when the statement runs in its own transaction (autocommit):
/// `statement_timestamp()` equals `transaction_timestamp()` only then.
const AUTOCOMMIT: &str =
    "pg_catalog.statement_timestamp() OPERATOR(pg_catalog.=) pg_catalog.transaction_timestamp()";

/// The refusal of every catalog call inside a transaction block, also
/// when that transaction has already failed (`25P02`).
pub const IN_TRANSACTION: &str =
    "the catalog cannot be read inside an open transaction: finish the transaction first";

/// SQLSTATE of a lock wait that ran into `lock_timeout`.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// SQLSTATE of a statement sent inside a transaction that already failed.
const IN_FAILED_TRANSACTION: &str = "25P02";

/// SQL that pins `search_path` and `lock_timeout` for the current
/// (implicit) transaction when the statement runs in autocommit, and is
/// `NULL` without setting anything inside a transaction block.
fn pin_sql(lock_timeout: &str) -> String {
    format!(
        "CASE WHEN {AUTOCOMMIT} \
         THEN pg_catalog.set_config('search_path', '{PINNED_SEARCH_PATH}', true) \
         OPERATOR(pg_catalog.||) pg_catalog.set_config('lock_timeout', '{lock_timeout}', true) END"
    )
}

/// Wraps one `SELECT` so it runs with `search_path = pg_catalog, pg_temp` and
/// [`LOCK_TIMEOUT`] without changing the session: the settings are made
/// with `set_config(..., true)`
/// in the statement itself, so it ends with the statement's own implicit
/// transaction, which the server ends by itself even when the caller stops
/// waiting (a dropped future). The pin is computed in a subquery the main
/// query depends on laterally (and `OFFSET 0` keeps both from being
/// flattened), so it is in place before any of the query is evaluated.
///
/// The pin cannot affect how the statement itself is parsed: names are
/// resolved before it runs. Every function, operator, type and relation in
/// catalog SQL is therefore written schema-qualified (`pg_catalog.f(...)`,
/// `OPERATOR(pg_catalog.=)`, `::pg_catalog.text`), so nothing a user
/// created in another schema can take its place.
///
/// Inside a transaction block a local setting would last until the user's
/// transaction ends, so there nothing is set and the statement returns no
/// rows (without an error, which would abort the user's transaction);
/// [`fetch_pinned`] checks for that case first. `order` sorts the result
/// and refers to columns as `q.<column>`.
#[must_use]
pub fn pinned_statement(sql: &str, order: &str) -> String {
    pinned_statement_with(sql, order, LOCK_TIMEOUT)
}

/// As [`pinned_statement`] with another lock timeout.
fn pinned_statement_with(sql: &str, order: &str, lock_timeout: &str) -> String {
    format!(
        "SELECT q.* FROM (SELECT {} AS pinned OFFSET 0) pin \
         CROSS JOIN LATERAL (SELECT * FROM ({sql}) i WHERE pin.pinned IS NOT NULL OFFSET 0) q \
         ORDER BY {order}",
        pin_sql(lock_timeout)
    )
}

/// Runs `(sql, order)` statements, each wrapped by [`pinned_statement`],
/// in one call and returns one result set per statement. The script starts
/// with a read-only check whether the session is inside a transaction
/// block; it then fails without having changed anything, and without
/// aborting the user's transaction. Each statement of the script runs in
/// its own implicit transaction and pins `search_path` and `lock_timeout`
/// for itself (see [`pinned_statement`]); nothing carries over from one
/// statement to the next.
///
/// # Errors
///
/// Driver errors; [`CatalogError::Invalid`] with [`IN_TRANSACTION`] when
/// the session is inside a transaction block (also an already failed
/// one); [`CatalogError::Driver`] with [`ErrorKind::Timeout`] and code
/// `55P03` when an object stayed locked by another session for
/// [`LOCK_TIMEOUT`].
pub async fn fetch_pinned(
    session: &dyn Session,
    statements: &[(String, String)],
) -> CatalogResult<Vec<Rows>> {
    fetch_pinned_with(session, statements, LOCK_TIMEOUT).await
}

/// As [`fetch_pinned`] with another lock timeout (a duration such as
/// `200ms`, a code constant).
///
/// # Errors
///
/// As [`fetch_pinned`].
pub(crate) async fn fetch_pinned_with(
    session: &dyn Session,
    statements: &[(String, String)],
    lock_timeout: &str,
) -> CatalogResult<Vec<Rows>> {
    let mut script = vec![format!("SELECT {AUTOCOMMIT} AS autocommit")];
    script.extend(
        statements
            .iter()
            .map(|(sql, order)| pinned_statement_with(sql, order, lock_timeout)),
    );
    let output = rows::run(session, &script.join(";\n"))
        .await
        .map_err(|error| match error {
            // A lock wait is an ordinary, retryable condition: a timeout
            // with the server's code, so the UI can offer a retry.
            CatalogError::Driver { error: driver }
                if driver.code.as_deref() == Some(LOCK_NOT_AVAILABLE) =>
            {
                CatalogError::Driver {
                    error: DriverError {
                        kind: ErrorKind::Timeout,
                        message: format!(
                            "an object is locked by another session (waited {lock_timeout}): \
                             try again when that session has finished"
                        ),
                        ..driver
                    },
                }
            }
            // The session is in a transaction that already failed: the same
            // refusal as in any other transaction block.
            CatalogError::Driver { error: driver }
                if driver.code.as_deref() == Some(IN_FAILED_TRANSACTION) =>
            {
                CatalogError::Invalid {
                    message: IN_TRANSACTION.into(),
                }
            }
            other => other,
        })?;
    let mut sets = Rows::all(output)?;
    if sets.len() != statements.len() + 1 {
        return Err(CatalogError::Unexpected {
            message: "the catalog query returned the wrong number of result sets".into(),
        });
    }
    let check = sets.remove(0);
    if !flag(&check.single("the transaction check")?, "autocommit")? {
        return Err(CatalogError::Invalid {
            message: IN_TRANSACTION.into(),
        });
    }
    Ok(sets)
}

/// As [`fetch_pinned`] for one statement.
pub(super) async fn fetch_one(
    session: &dyn Session,
    sql: &str,
    order: &str,
) -> CatalogResult<Rows> {
    fetch_pinned(session, &[(sql.to_owned(), order.to_owned())])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| CatalogError::Unexpected {
            message: "the catalog query returned no result set".into(),
        })
}

/// True for a "permission denied" (`42501`) server error.
pub(super) fn is_permission_denied(error: &CatalogError) -> bool {
    matches!(error, CatalogError::Driver { error } if error.code.as_deref() == Some("42501"))
}

/// A quoted identifier.
pub(super) fn ident(name: &str) -> CatalogResult<String> {
    sql::ident(D, name)
}

/// A quoted `schema.name`.
pub(super) fn qname(schema: &str, name: &str) -> CatalogResult<String> {
    sql::qualified(D, &[Some(schema), Some(name)])
}

/// A quoted string literal.
pub(super) fn lit(text: &str) -> CatalogResult<String> {
    sql::string(D, text)
}

/// The schema of a schema-scoped object reference.
pub(super) fn schema_of(object: &ObjectRef) -> CatalogResult<&str> {
    object
        .schema
        .as_deref()
        .ok_or_else(|| CatalogError::Invalid {
            message: format!("a {:?} reference needs a schema", object.kind),
        })
}

/// Text array column; `NULL` reads as empty.
pub(super) fn texts(row: &Row<'_>, column: &str) -> CatalogResult<Vec<String>> {
    match row.value(column)? {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.to_plain_text()
                    .ok_or_else(|| CatalogError::Unexpected {
                        message: format!("column {column} has a NULL element"),
                    })
            })
            .collect(),
        _ => Err(CatalogError::Unexpected {
            message: format!("column {column} is not an array"),
        }),
    }
}

/// Floating point column; `None` for `NULL`.
pub(super) fn float(row: &Row<'_>, column: &str) -> CatalogResult<Option<f64>> {
    let not_float = || CatalogError::Unexpected {
        message: format!("column {column} is not a number"),
    };
    match row.value(column)? {
        Value::Null => Ok(None),
        Value::Float(f) => Ok(Some(*f)),
        #[allow(clippy::cast_precision_loss)]
        Value::Int(i) => Ok(Some(*i as f64)),
        Value::Decimal(s) | Value::Text(s) | Value::Other(s) => {
            s.trim().parse().map(Some).map_err(|_| not_float())
        }
        _ => Err(not_float()),
    }
}

/// Boolean column that must not be `NULL`.
pub(super) fn flag(row: &Row<'_>, column: &str) -> CatalogResult<bool> {
    row.bool(column)?.ok_or_else(|| CatalogError::Unexpected {
        message: format!("column {column} is NULL"),
    })
}

/// Integer column that must not be `NULL`.
pub(super) fn int(row: &Row<'_>, column: &str) -> CatalogResult<i64> {
    row.int(column)?.ok_or_else(|| CatalogError::Unexpected {
        message: format!("column {column} is NULL"),
    })
}

/// An unexpected single-character catalog code.
pub(super) fn bad_code(column: &str, code: &str) -> CatalogError {
    CatalogError::Unexpected {
        message: format!("column {column} has the unknown code {code:?}"),
    }
}

/// Owner, comment and system status, shared by every object.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgMeta {
    /// Owning role; `None` for objects without an owner (casts).
    pub owner: Option<String>,
    /// Comment on the object.
    pub comment: Option<String>,
    /// The extension the object belongs to, when it is an extension member.
    pub extension: Option<String>,
    /// True for built-in objects, objects in system schemas and extension
    /// members.
    pub system: bool,
}

/// Reads the `owner`, `comment`, `extension` and `system` columns.
pub(super) fn meta(row: &Row<'_>) -> CatalogResult<PgMeta> {
    Ok(PgMeta {
        owner: row.text("owner")?,
        comment: row.text("comment")?,
        extension: row.text("extension")?,
        system: flag(row, "system")?,
    })
}

/// SQL for the name of the extension that owns `oid_expr`, an object of
/// system catalog `catalog`, written schema-qualified (`pg_catalog.pg_proc`,
/// `pg_catalog.pg_type`, ...) so the `regclass` input does not look it up
/// in `search_path`; `NULL` when it is not an extension member. Both
/// arguments are code constants.
pub(super) fn extension_sql(catalog: &str, oid_expr: &str) -> String {
    format!(
        "(SELECT ext.extname FROM pg_catalog.pg_depend dep JOIN pg_catalog.pg_extension ext ON ext.oid OPERATOR(pg_catalog.=) dep.refobjid \
         WHERE dep.classid OPERATOR(pg_catalog.=) '{catalog}'::pg_catalog.regclass AND dep.objid OPERATOR(pg_catalog.=) {oid_expr} \
         AND dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass AND dep.deptype OPERATOR(pg_catalog.=) 'e' LIMIT 1)"
    )
}

/// SQL that is true when the schema name `nspname_expr` is a system schema.
pub(super) fn system_schema_sql(nspname_expr: &str) -> String {
    format!(
        "({nspname_expr} OPERATOR(pg_catalog.=) ANY (ARRAY['pg_catalog', 'information_schema']::pg_catalog.name[]) \
         OR {nspname_expr} OPERATOR(pg_catalog.~~) 'pg\\_toast%' OR {nspname_expr} OPERATOR(pg_catalog.~~) 'pg\\_temp\\_%')"
    )
}

/// The `system` expression over an inner query aliased `q` that has the
/// columns `oid`, `extension` and `system_schema` (true for objects in a
/// system schema, and for objects that exist only as part of another one).
/// Objects created by `initdb` have OIDs below 16384 (`FirstNormalObjectId`).
pub(super) const SYSTEM_EXPR: &str = "(q.oid OPERATOR(pg_catalog.<) '16384'::pg_catalog.oid OR q.extension IS NOT NULL OR q.system_schema)";

/// Wraps an inner query (see [`SYSTEM_EXPR`]) with a `system` column and
/// the system filter.
pub(super) fn with_system(inner: &str, include_system: bool) -> String {
    let filter = if include_system {
        String::new()
    } else {
        format!(" WHERE NOT {SYSTEM_EXPR}")
    };
    format!("SELECT q.*, {SYSTEM_EXPR} AS system FROM ({inner}) q{filter}")
}

/// Builds a tree entry from a row with the columns `db`, `name`, `comment`,
/// `system` and, for schema-scoped kinds, `schema`.
pub(super) fn summary(
    kind: ObjectKind,
    row: &Row<'_>,
    schema_scoped: bool,
    signature: Option<String>,
    detail: Option<String>,
    flags: Vec<String>,
) -> CatalogResult<ObjectSummary> {
    Ok(ObjectSummary {
        object: ObjectRef {
            kind,
            database: row.text("db")?,
            schema: if schema_scoped {
                Some(row.req_text("schema")?)
            } else {
                None
            },
            parent: None,
            name: row.req_text("name")?,
            signature,
        },
        comment: row.text("comment")?,
        detail,
        flags,
        has_children: false,
        system: flag(row, "system")?,
        row_estimate: None,
        size_bytes: None,
    })
}

/// One generic option of a foreign data wrapper, server, user mapping,
/// foreign table or foreign column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgOption {
    /// Option name.
    pub name: String,
    /// Option value; `None` when it is hidden because it may hold a secret
    /// (see [`redact_option`]).
    pub value: Option<String>,
}

/// Placeholder generated DDL writes for a hidden option value.
pub const REDACTED: &str = "<redacted>";

/// libpq settings without secrets: the server options of postgres_fdw and
/// dblink_fdw.
const LIBPQ_OPTIONS: &[&str] = &[
    "host",
    "hostaddr",
    "port",
    "dbname",
    "application_name",
    "connect_timeout",
    "client_encoding",
    "sslmode",
    "sslnegotiation",
    "sslcert",
    "sslkey",
    "sslrootcert",
    "sslcrl",
    "sslcrldir",
    "sslsni",
    "sslcompression",
    "ssl_min_protocol_version",
    "ssl_max_protocol_version",
    "gssencmode",
    "krbsrvname",
    "channel_binding",
    "require_auth",
    "target_session_attrs",
    "load_balance_hosts",
    "keepalives",
    "keepalives_idle",
    "keepalives_interval",
    "keepalives_count",
    "tcp_user_timeout",
];

/// postgres_fdw options of its own (wrapper, server, table, column level).
const POSTGRES_FDW_OPTIONS: &[&str] = &[
    "use_remote_estimate",
    "fdw_startup_cost",
    "fdw_tuple_cost",
    "extensions",
    "fetch_size",
    "batch_size",
    "updatable",
    "truncatable",
    "async_capable",
    "parallel_commit",
    "parallel_abort",
    "keep_connections",
    "analyze_sampling",
    "schema_name",
    "table_name",
    "column_name",
];

const FILE_FDW_OPTIONS: &[&str] = &[
    "filename",
    "format",
    "header",
    "delimiter",
    "quote",
    "escape",
    "null",
    "encoding",
    "force_not_null",
    "force_null",
    "on_error",
    "log_verbosity",
    "reject_limit",
];

const MYSQL_FDW_OPTIONS: &[&str] = &[
    "host",
    "port",
    "dbname",
    "table_name",
    "secure_auth",
    "use_remote_estimate",
    "reconnect",
    "character_set",
    "sql_mode",
    "ssl_key",
    "ssl_cert",
    "ssl_ca",
    "ssl_capath",
    "ssl_cipher",
    "mysql_default_file",
    "fetch_size",
    "max_blob_size",
    "enable_order_by_pushdown",
    "enable_aggregate_pushdown",
    "enable_join_pushdown",
];

const ORACLE_FDW_OPTIONS: &[&str] = &[
    "dbserver",
    "isolation_level",
    "nchar",
    "schema",
    "table",
    "max_long",
    "readonly",
    "sample_percent",
    "prefetch",
    "lob_prefetch",
    "key",
];

const TDS_FDW_OPTIONS: &[&str] = &[
    "servername",
    "port",
    "database",
    "dbuse",
    "language",
    "character_set",
    "tds_version",
    "msg_handler",
    "fdw_startup_cost",
    "fdw_tuple_cost",
    "sqlserver_ansi_mode",
    "row_estimate_method",
    "local_tuple_estimate",
    "match_column_names",
    "use_remote_estimate",
    "schema_name",
    "table_name",
    "table",
    "column_name",
];

const MONGO_FDW_OPTIONS: &[&str] = &[
    "address",
    "port",
    "authentication_database",
    "replica_set",
    "read_preference",
    "ssl",
    "pem_file",
    "ca_file",
    "ca_dir",
    "crl_file",
    "weak_cert_validation",
    "enable_join_pushdown",
    "enable_aggregate_pushdown",
    "enable_order_by_pushdown",
    "database",
    "collection",
    "use_remote_estimate",
];

const REDIS_FDW_OPTIONS: &[&str] = &[
    "address",
    "port",
    "database",
    "tabletype",
    "tablekeyprefix",
    "tablekeyset",
    "singleton_key",
];

/// The options known to be harmless for the wrapper implemented by
/// `extension` (see [`fdw_extension_sql`]); empty for every other wrapper.
fn harmless_options(extension: &str) -> &'static [&'static [&'static str]] {
    match extension {
        "postgres_fdw" => &[LIBPQ_OPTIONS, POSTGRES_FDW_OPTIONS],
        "dblink" => &[LIBPQ_OPTIONS],
        "file_fdw" => &[FILE_FDW_OPTIONS],
        "mysql_fdw" => &[MYSQL_FDW_OPTIONS],
        "oracle_fdw" => &[ORACLE_FDW_OPTIONS],
        "tds_fdw" => &[TDS_FDW_OPTIONS],
        "mongo_fdw" => &[MONGO_FDW_OPTIONS],
        "redis_fdw" => &[REDIS_FDW_OPTIONS],
        _ => &[],
    }
}

/// SQL for the extension that implements the foreign data wrapper
/// `wrapper_oid_expr` (a code constant): the extension that owns the
/// wrapper's handler or validator function, when that function carries the
/// extension's own name (`postgres_fdw_handler`, `dblink_fdw_validator`,
/// ...). `NULL` for wrappers built on anything else; their option values
/// are all hidden. The user-chosen wrapper name plays no part.
#[must_use]
pub fn fdw_extension_sql(wrapper_oid_expr: &str) -> String {
    format!(
        "(SELECT fext.extname::pg_catalog.text FROM pg_catalog.pg_foreign_data_wrapper fw \
         JOIN pg_catalog.pg_proc fp ON (fp.oid OPERATOR(pg_catalog.=) fw.fdwhandler OR fp.oid OPERATOR(pg_catalog.=) fw.fdwvalidator) \
         JOIN pg_catalog.pg_depend fdep ON fdep.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass \
         AND fdep.objid OPERATOR(pg_catalog.=) fp.oid AND fdep.deptype OPERATOR(pg_catalog.=) 'e' \
         AND fdep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass \
         JOIN pg_catalog.pg_extension fext ON fext.oid OPERATOR(pg_catalog.=) fdep.refobjid \
         WHERE fw.oid OPERATOR(pg_catalog.=) {wrapper_oid_expr} AND fp.proname::pg_catalog.text OPERATOR(pg_catalog.=) ANY (ARRAY[\
         fext.extname::pg_catalog.text OPERATOR(pg_catalog.||) '_handler', \
         fext.extname::pg_catalog.text OPERATOR(pg_catalog.||) '_validator', \
         fext.extname::pg_catalog.text OPERATOR(pg_catalog.||) '_fdw_validator']) \
         ORDER BY 1 LIMIT 1)"
    )
}

/// The value of a foreign data option when it is safe to show, `None` when
/// it must be hidden. `wrapper_extension` is the extension implementing the
/// option's wrapper, from [`fdw_extension_sql`]. Only option names on that
/// wrapper's allowlist keep their value (every value of an unknown wrapper
/// is hidden), and even then not when the value looks like it carries
/// credentials (`scheme://user:secret@host`, `password=...`, `Pwd = ...`,
/// ODBC `Uid=...;Pwd=...`). Used for wrapper, server, foreign table and
/// foreign column options; user mapping values are never shown at all.
#[must_use]
pub fn redact_option(wrapper_extension: Option<&str>, name: &str, value: &str) -> Option<String> {
    let known = wrapper_extension.is_some_and(|extension| {
        harmless_options(extension)
            .iter()
            .flat_map(|list| list.iter())
            .any(|harmless| harmless.eq_ignore_ascii_case(name))
    });
    (known && !carries_credentials(value)).then(|| value.to_owned())
}

/// True for values that look like they hold credentials. Case and white
/// space are ignored, so `Pwd = x` reads as `pwd=x`.
///
/// Any `@` with a `:` somewhere before it counts as `user:secret@host`
/// userinfo. That covers every URL in the value (not only the first), a
/// password that holds `/`, `?` or `#` (URL parsers end the authority
/// there, libpq does not) or `@` (the userinfo then ends at the last `@`),
/// and userinfo without a scheme; it errs toward hiding values such as
/// `https://host/a@b`.
fn carries_credentials(value: &str) -> bool {
    let compact: String = value
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    let userinfo = compact
        .find(':')
        .is_some_and(|colon| compact[colon..].contains('@'));
    userinfo
        || [
            "password", "passwd", "pwd=", "pwd:", "secret", "token", "apikey", "api_key",
        ]
        .iter()
        .any(|word| compact.contains(word))
}

/// Parses a `name=value` option array column through [`redact_option`],
/// for a wrapper implemented by `wrapper_extension`.
pub(super) fn options(
    row: &Row<'_>,
    column: &str,
    wrapper_extension: Option<&str>,
) -> CatalogResult<Vec<PgOption>> {
    Ok(texts(row, column)?
        .into_iter()
        .map(|entry| {
            let (name, value) = entry.split_once('=').unwrap_or((entry.as_str(), ""));
            PgOption {
                name: name.to_owned(),
                value: redact_option(wrapper_extension, name, value),
            }
        })
        .collect())
}

/// Parses a `name=value` option array column keeping only the names (user
/// mappings: every value may be a credential).
pub(super) fn option_names(row: &Row<'_>, column: &str) -> CatalogResult<Vec<PgOption>> {
    Ok(texts(row, column)?
        .into_iter()
        .map(|entry| PgOption {
            name: entry
                .split_once('=')
                .map_or(entry.as_str(), |(n, _)| n)
                .to_owned(),
            value: None,
        })
        .collect())
}

/// ` OPTIONS (name 'value', ...)`, or an empty string without options.
/// Hidden values are written as [`REDACTED`]; the flag says whether any
/// was, so the DDL can say so.
pub(super) fn options_clause(options: &[PgOption]) -> CatalogResult<(String, bool)> {
    let mut parts = Vec::new();
    let mut hidden = false;
    for option in options {
        let value = match &option.value {
            Some(value) => value.as_str(),
            None => {
                hidden = true;
                REDACTED
            }
        };
        parts.push(format!("{} {}", ident(&option.name)?, lit(value)?));
    }
    let clause = if parts.is_empty() {
        String::new()
    } else {
        format!(" OPTIONS ({})", parts.join(", "))
    };
    Ok((clause, hidden))
}

/// The comment generated DDL starts with when it holds [`REDACTED`] values.
pub(super) const REDACTED_NOTE: &str = "-- Option values that may hold secrets are written as '<redacted>': \
     replace them before running this.";

/// `COMMENT ON <target> IS '...'` when there is a comment.
pub(super) fn comment_on(target: &str, comment: Option<&str>) -> CatalogResult<Option<String>> {
    comment
        .map(|text| Ok(format!("COMMENT ON {target} IS {}", lit(text)?)))
        .transpose()
}

/// `ALTER <target> OWNER TO <role>` when the owner is known.
pub(super) fn owner_to(target: &str, owner: Option<&str>) -> CatalogResult<Option<String>> {
    owner
        .map(|role| Ok(format!("ALTER {target} OWNER TO {}", ident(role)?)))
        .transpose()
}

/// Joins statements into a script, each ended by `;`. Leading lines that
/// start with `--` are kept as comments.
pub(super) fn script(statements: Vec<String>) -> String {
    let mut out = String::new();
    for statement in statements {
        let statement = statement.trim_end();
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(statement);
        if !statement
            .lines()
            .all(|line| line.trim_start().starts_with("--"))
        {
            out.push(';');
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_options_harmless_for_their_wrapper_keep_their_values() {
        let pg = Some("postgres_fdw");
        assert_eq!(redact_option(pg, "host", "db").as_deref(), Some("db"));
        assert_eq!(redact_option(pg, "PORT", "5432").as_deref(), Some("5432"));
        for secret in [
            "password",
            "apikey",
            "api_key",
            "endpoint",
            "credentials",
            "private_key",
        ] {
            assert_eq!(redact_option(pg, secret, "x"), None, "{secret}");
        }
        // `key` is harmless for oracle_fdw only.
        assert_eq!(
            redact_option(Some("oracle_fdw"), "key", "true").as_deref(),
            Some("true")
        );
        assert_eq!(redact_option(pg, "key", "SECRET"), None);
        assert_eq!(redact_option(Some("mongo_fdw"), "key", "SECRET"), None);
        // Unknown wrappers show no values at all.
        assert_eq!(redact_option(None, "host", "db"), None);
        assert_eq!(redact_option(Some("my_fdw"), "host", "db"), None);
        let mongo = Some("mongo_fdw");
        assert_eq!(redact_option(mongo, "address", "mongodb://u:pw@h/db"), None);
        assert_eq!(
            redact_option(mongo, "address", "mongodb://h:27017/db").as_deref(),
            Some("mongodb://h:27017/db")
        );
        let tds = Some("tds_fdw");
        for value in [
            "a;Pwd = x",
            "PWD=x",
            "pwd : x",
            "Uid=sa;Pwd=x",
            "PassWord=x",
        ] {
            assert_eq!(redact_option(tds, "servername", value), None, "{value}");
        }
        assert_eq!(
            redact_option(tds, "servername", "sql01").as_deref(),
            Some("sql01")
        );
    }

    #[test]
    fn userinfo_is_found_in_every_url_and_without_a_scheme() {
        let pg = Some("postgres_fdw");
        for value in [
            "postgresql://u:XYZZY?x@h",
            "postgresql://u:XYZZY#x@h",
            "postgresql://u:XYZ/ZY@h/db",
            "postgresql://u:XY@ZZY@h/db",
            "a://h/ b://u:XYZZY@h",
            "a://h/,b://u:XYZZY@h",
            "u:XYZZY@h",
            "host=h user=u:XYZZY@h",
        ] {
            assert_eq!(redact_option(pg, "host", value), None, "{value}");
        }
        for value in [
            "db.example.com",
            "postgresql://h:5432/db",
            "a@b.example",
            "[::1]",
        ] {
            assert_eq!(
                redact_option(pg, "host", value).as_deref(),
                Some(value),
                "{value}"
            );
        }
    }

    #[test]
    fn option_clauses_write_placeholders() -> CatalogResult<()> {
        let options = vec![
            PgOption {
                name: "user".into(),
                value: Some("o'k".into()),
            },
            PgOption {
                name: "password".into(),
                value: None,
            },
        ];
        assert_eq!(
            options_clause(&options)?,
            (
                r#" OPTIONS ("user" 'o''k', "password" '<redacted>')"#.to_owned(),
                true
            )
        );
        assert_eq!(options_clause(&[])?, (String::new(), false));
        Ok(())
    }

    #[test]
    fn scripts_end_statements_but_not_comments() {
        assert_eq!(
            script(vec![
                "-- note".into(),
                "CREATE X\n".into(),
                "COMMENT ON X IS 'a'".into()
            ]),
            "-- note\n\nCREATE X;\n\nCOMMENT ON X IS 'a';\n"
        );
    }
}
