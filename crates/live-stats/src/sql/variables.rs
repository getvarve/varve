//! Server variables: browse every setting with its metadata, and turn an
//! edit into an [`EditPlan`] the UI shows before it runs.
//!
//! - PostgreSQL: `pg_settings`. Session changes use `SET` / `RESET`;
//!   server-wide changes use `ALTER SYSTEM SET` / `ALTER SYSTEM RESET`
//!   followed by `SELECT pg_catalog.pg_reload_conf()`, with a restart
//!   warning for `postmaster` settings.
//! - MySQL: `performance_schema.global_variables` and `session_variables`
//!   joined with `variables_info`, `persisted_variables` and (when the server
//!   has it) `variables_metadata` for scope, type and documentation. Without
//!   `variables_metadata` (MySQL 8.0 and early 9.x) the session scope comes
//!   from `variables_by_thread` when readable and the type is inferred from
//!   the value and range ([`ServerVariable::type_inferred`]). Changes use
//!   `SET SESSION`, `SET GLOBAL`, `SET PERSIST`, `SET PERSIST_ONLY` and
//!   `RESET PERSIST`.
//! - MariaDB: `information_schema.SYSTEM_VARIABLES`. Changes use
//!   `SET SESSION` and `SET GLOBAL`; MariaDB has no `SET PERSIST`.
//!
//! Values are validated against the variable's type before any SQL is
//! built: booleans, integers (PostgreSQL with units such as `64MB` or
//! `5min`), reals, enums and sets against their allowed values, and strings
//! quoted through [`varve_catalog::sql::string`].

use serde::{Deserialize, Serialize};
use varve_activity::{REDACTED, SqlDialect, redact_secrets};
use varve_catalog::EditPlan;
use varve_catalog::rows::{self, Row};
use varve_catalog::sql;
use varve_driver::{Dialect, ServerInfo, Session};

use super::{Family, LiveError, LiveResult, non_empty};

/// The type of a variable's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VariableType {
    /// On or off.
    Boolean,
    /// Whole number (PostgreSQL: optionally with a unit).
    Integer,
    /// Floating point number.
    Real,
    /// One of [`ServerVariable::enum_values`].
    Enum,
    /// A comma separated subset of [`ServerVariable::enum_values`]
    /// (MySQL and MariaDB `SET` variables such as `sql_mode`).
    Set,
    /// Free text.
    String,
}

/// Where a change applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EditScope {
    /// This session only (`SET`, `SET SESSION`).
    Session,
    /// The whole server: PostgreSQL `ALTER SYSTEM` plus a configuration
    /// reload, MySQL and MariaDB `SET GLOBAL` (lost at restart).
    Global,
    /// MySQL `SET PERSIST`: the running value and `mysqld-auto.cnf`.
    Persist,
    /// MySQL `SET PERSIST_ONLY`: `mysqld-auto.cnf` only, used at the next
    /// restart (also for read-only variables).
    PersistOnly,
}

impl EditScope {
    fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Global => "global",
            Self::Persist => "persist",
            Self::PersistOnly => "persist only",
        }
    }
}

/// One server variable with what the server tells about it.
///
/// Fields an engine does not report are `None` (or empty).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerVariable {
    /// Name as the server lists it (MariaDB names lowercased, as `SHOW
    /// VARIABLES` shows them).
    pub name: String,
    /// The value in effect for this session (the global value for global
    /// only variables).
    pub value: Option<String>,
    /// Session value, for variables with a session scope.
    pub session_value: Option<String>,
    /// Global value (MySQL, MariaDB).
    pub global_value: Option<String>,
    /// Built-in default (PostgreSQL `boot_val`, MariaDB `DEFAULT_VALUE`).
    pub default_value: Option<String>,
    /// The value `RESET` returns to in this session (PostgreSQL
    /// `reset_val`).
    pub reset_value: Option<String>,
    /// Unit of the value (PostgreSQL: `kB`, `8kB`, `ms`, `s`, ...).
    pub unit: Option<String>,
    /// Category (PostgreSQL).
    pub category: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Value type.
    pub var_type: VariableType,
    /// True when the type was inferred from the value and range because the
    /// server does not report it (MySQL without `variables_metadata`).
    pub type_inferred: bool,
    /// The server's own scope or context text: PostgreSQL `context`
    /// (`user`, `superuser`, `sighup`, `postmaster`, ...), MySQL and MariaDB
    /// `VARIABLE_SCOPE` (`GLOBAL`, `SESSION`, `SESSION_ONLY`).
    pub context: Option<String>,
    /// The scopes this variable can be changed in, which the UI offers.
    pub settable_scopes: Vec<EditScope>,
    /// True when it can change without a restart; `None` when the server
    /// does not say (MySQL).
    pub dynamic: Option<bool>,
    /// True when it cannot be changed at all while running (PostgreSQL
    /// `internal`, MariaDB `READ_ONLY`).
    pub read_only: bool,
    /// True when a server-wide change only takes effect after a restart
    /// (PostgreSQL `postmaster`).
    pub restart_required: bool,
    /// True when the configuration file was changed and a restart is
    /// pending (PostgreSQL `pending_restart`).
    pub pending_restart: bool,
    /// Smallest allowed value, for numeric variables.
    pub min_value: Option<String>,
    /// Largest allowed value, for numeric variables.
    pub max_value: Option<String>,
    /// Allowed values of enums and sets (MariaDB also lists the flags of
    /// `FLAGSET` variables such as `optimizer_switch`).
    pub enum_values: Vec<String>,
    /// Where the value came from: PostgreSQL `source` (`default`,
    /// `configuration file`, `session`, ...), MySQL `VARIABLE_SOURCE`,
    /// MariaDB `GLOBAL_VALUE_ORIGIN`.
    pub source: Option<String>,
    /// The file that set it: PostgreSQL `sourcefile:sourceline`, MySQL
    /// `VARIABLE_PATH`, MariaDB `GLOBAL_VALUE_PATH`.
    pub source_file: Option<String>,
    /// Who last set it at runtime (MySQL `SET_USER@SET_HOST`).
    pub set_by: Option<String>,
    /// When it was last set at runtime (MySQL `SET_TIME`).
    pub set_time: Option<String>,
    /// The value in `mysqld-auto.cnf` (MySQL `persisted_variables`).
    pub persisted_value: Option<String>,
    /// True when the values were masked because they hold a secret (a
    /// password in a connection string, a passphrase command, an SST
    /// credential, ...): the value fields then show [`HIDDEN`] or the
    /// connection string without its password. The UI must not prefill an
    /// editor with a masked value.
    pub masked: bool,
}

/// A change to one variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum VariableChange {
    /// Set a new value.
    Set {
        /// Where it applies.
        scope: EditScope,
        /// The value as the user typed it.
        value: String,
    },
    /// Return to the default: PostgreSQL `RESET` / `ALTER SYSTEM RESET`,
    /// MySQL and MariaDB `SET ... = DEFAULT`, MySQL `RESET PERSIST` for the
    /// persist scopes (which leaves the running value as it is).
    Reset {
        /// Where it applies.
        scope: EditScope,
    },
}

/// Reads every server variable, ordered by name.
///
/// # Errors
///
/// [`LiveError::Unsupported`] for Redis, [`LiveError::PermissionDenied`]
/// when the user may not read the tables, other server errors.
pub async fn server_variables(
    session: &dyn Session,
    server: &ServerInfo,
) -> LiveResult<Vec<ServerVariable>> {
    load(session, server, None).await
}

/// Reads one server variable (names compare case-insensitively).
///
/// # Errors
///
/// [`LiveError::NotFound`] for an unknown name, and as
/// [`server_variables`].
pub async fn server_variable(
    session: &dyn Session,
    server: &ServerInfo,
    name: &str,
) -> LiveResult<ServerVariable> {
    load(session, server, Some(name))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| LiveError::NotFound {
            message: format!("there is no server variable named {name}"),
        })
}

/// Reads the variable fresh from the server and plans the change with
/// [`variable_edit_plan`].
///
/// # Errors
///
/// As [`server_variable`] and [`variable_edit_plan`].
pub async fn plan_variable_change(
    session: &dyn Session,
    server: &ServerInfo,
    name: &str,
    change: &VariableChange,
) -> LiveResult<EditPlan> {
    let variable = server_variable(session, server, name).await?;
    variable_edit_plan(server, &variable, change)
}

async fn load(
    session: &dyn Session,
    server: &ServerInfo,
    name: Option<&str>,
) -> LiveResult<Vec<ServerVariable>> {
    let family = Family::of(server)?;
    let dialect = family.dialect();
    let filter = name.map(|n| sql::string(dialect, n)).transpose()?;
    match family {
        Family::Postgres => {
            let sql = pg_query(filter.as_deref());
            let rows = rows::query(session, &sql).await?;
            rows.iter()
                .map(|row| pg_variable(&row).map(|v| mask(family, v)))
                .collect()
        }
        Family::Mariadb => {
            let sql = mariadb_query(filter.as_deref());
            let rows = rows::query(session, &sql).await?;
            rows.iter()
                .map(|row| mariadb_variable(&row).map(|v| mask(family, v)))
                .collect()
        }
        Family::Mysql => {
            let has_metadata = !rows::query(session, MYSQL_METADATA_PROBE).await?.is_empty();
            let session_names = if has_metadata {
                None
            } else {
                mysql_session_names(session, filter.as_deref()).await?
            };
            let sql = mysql_query(has_metadata, filter.as_deref());
            let rows = rows::query(session, &sql).await?;
            rows.iter()
                .map(|row| {
                    mysql_variable(server, &row, has_metadata, session_names.as_deref())
                        .map(|v| mask(family, v))
                })
                .collect()
        }
    }
}

// PostgreSQL --------------------------------------------------------------

/// The `pg_settings` query. Every name is qualified: the statement is
/// parsed under the user's search_path, where an unqualified function,
/// operator or relation could resolve to an object planted in another
/// schema.
fn pg_query(filter: Option<&str>) -> String {
    let filter = filter
        .map(|literal| {
            format!(
                " WHERE pg_catalog.lower(name) OPERATOR(pg_catalog.=) \
                 pg_catalog.lower({literal}::pg_catalog.text)"
            )
        })
        .unwrap_or_default();
    format!(
        "SELECT name, setting, unit, category, short_desc, extra_desc, context, vartype, \
         source, min_val, max_val, pg_catalog.array_to_json(enumvals)::pg_catalog.text \
         AS enum_values, boot_val, reset_val, sourcefile, sourceline, pending_restart \
         FROM pg_catalog.pg_settings{filter} ORDER BY name"
    )
}

fn json_list(text: Option<String>) -> LiveResult<Vec<String>> {
    match text {
        None => Ok(Vec::new()),
        Some(text) => serde_json::from_str(&text).map_err(|error| LiveError::Unexpected {
            message: format!("enum values are not a JSON list: {error}"),
        }),
    }
}

fn pg_variable(row: &Row<'_>) -> LiveResult<ServerVariable> {
    let context = row.req_text("context")?;
    let var_type = match row.req_text("vartype")?.as_str() {
        "bool" => VariableType::Boolean,
        "integer" => VariableType::Integer,
        "real" => VariableType::Real,
        "enum" => VariableType::Enum,
        _ => VariableType::String,
    };
    let settable_scopes = match context.as_str() {
        "user" | "superuser" => vec![EditScope::Session, EditScope::Global],
        "internal" => Vec::new(),
        // sighup, postmaster, backend and superuser-backend settings cannot
        // be changed with SET once the session runs.
        _ => vec![EditScope::Global],
    };
    let description = match (row.text("short_desc")?, non_empty(row, "extra_desc")?) {
        (Some(short), Some(extra)) => Some(format!("{short} {extra}")),
        (short, extra) => short.or(extra),
    };
    let source_file = match (row.text("sourcefile")?, row.int("sourceline")?) {
        (Some(file), Some(line)) => Some(format!("{file}:{line}")),
        (file, _) => file,
    };
    let setting = row.text("setting")?;
    Ok(ServerVariable {
        name: row.req_text("name")?,
        value: setting.clone(),
        session_value: setting,
        global_value: None,
        default_value: row.text("boot_val")?,
        reset_value: row.text("reset_val")?,
        unit: row.text("unit")?,
        category: row.text("category")?,
        description,
        var_type,
        type_inferred: false,
        settable_scopes,
        dynamic: Some(!matches!(context.as_str(), "postmaster" | "internal")),
        read_only: context == "internal",
        restart_required: context == "postmaster",
        pending_restart: row.bool("pending_restart")?.unwrap_or(false),
        min_value: row.text("min_val")?,
        max_value: row.text("max_val")?,
        enum_values: json_list(row.text("enum_values")?)?,
        source: row.text("source")?,
        source_file,
        set_by: None,
        set_time: None,
        persisted_value: None,
        masked: false,
        context: Some(context),
    })
}

// MySQL -------------------------------------------------------------------

/// Whether the server has `performance_schema.variables_metadata`.
const MYSQL_METADATA_PROBE: &str = "SELECT TABLE_NAME FROM information_schema.TABLES \
     WHERE TABLE_SCHEMA = 'performance_schema' AND TABLE_NAME = 'variables_metadata'";

fn mysql_query(has_metadata: bool, filter: Option<&str>) -> String {
    let (metadata_columns, metadata_join) = if has_metadata {
        (
            "m.VARIABLE_NAME IS NOT NULL AS has_metadata, m.VARIABLE_SCOPE AS scope, \
             m.DATA_TYPE AS data_type, m.DOCUMENTATION AS description",
            " LEFT JOIN performance_schema.variables_metadata m \
             ON m.VARIABLE_NAME = i.VARIABLE_NAME",
        )
    } else {
        (
            "FALSE AS has_metadata, NULL AS scope, NULL AS data_type, NULL AS description",
            "",
        )
    };
    let filter = filter
        .map(|literal| format!(" WHERE LOWER(i.VARIABLE_NAME) = LOWER({literal})"))
        .unwrap_or_default();
    format!(
        "SELECT i.VARIABLE_NAME AS name, s.VARIABLE_VALUE AS session_value, \
         g.VARIABLE_NAME IS NOT NULL AS has_global, g.VARIABLE_VALUE AS global_value, \
         i.VARIABLE_SOURCE AS source, i.VARIABLE_PATH AS path, i.MIN_VALUE AS min_value, \
         i.MAX_VALUE AS max_value, i.SET_TIME AS set_time, i.SET_USER AS set_user, \
         i.SET_HOST AS set_host, p.VARIABLE_VALUE AS persisted_value, {metadata_columns} \
         FROM performance_schema.variables_info i \
         LEFT JOIN performance_schema.session_variables s ON s.VARIABLE_NAME = i.VARIABLE_NAME \
         LEFT JOIN performance_schema.global_variables g ON g.VARIABLE_NAME = i.VARIABLE_NAME \
         LEFT JOIN performance_schema.persisted_variables p ON p.VARIABLE_NAME = i.VARIABLE_NAME\
         {metadata_join}{filter} ORDER BY i.VARIABLE_NAME"
    )
}

/// The variables with a session scope, from `variables_by_thread` of this
/// session's thread. `None` when the user may not read that table.
async fn mysql_session_names(
    session: &dyn Session,
    filter: Option<&str>,
) -> LiveResult<Option<Vec<String>>> {
    let filter = filter
        .map(|literal| format!(" AND LOWER(VARIABLE_NAME) = LOWER({literal})"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT VARIABLE_NAME AS name FROM performance_schema.variables_by_thread \
         WHERE THREAD_ID = PS_CURRENT_THREAD_ID(){filter}"
    );
    match rows::query(session, &sql).await.map_err(LiveError::from) {
        Ok(rows) => Ok(Some(
            rows.iter()
                .map(|row| row.req_text("name").map(|n| n.to_ascii_lowercase()))
                .collect::<Result<_, _>>()?,
        )),
        Err(LiveError::PermissionDenied { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_decimal_number(text: &str) -> bool {
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    let mut parts = body.splitn(2, '.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

/// The type of a MySQL variable without `variables_metadata`: numeric
/// variables have a non-zero maximum in `variables_info`; others are
/// booleans when their value is `ON` or `OFF`, reals when the value is a
/// decimal fraction (`timestamp`), and strings otherwise.
fn mysql_inferred_type(max_value: Option<&str>, value: Option<&str>) -> VariableType {
    let numeric_range = max_value.is_some_and(|max| !max.is_empty() && max != "0");
    let value = value.unwrap_or_default();
    if numeric_range {
        if value.contains('.') {
            VariableType::Real
        } else {
            VariableType::Integer
        }
    } else if value.eq_ignore_ascii_case("ON") || value.eq_ignore_ascii_case("OFF") {
        VariableType::Boolean
    } else if value.contains('.') && is_decimal_number(value) {
        VariableType::Real
    } else {
        VariableType::String
    }
}

fn mysql_metadata_type(data_type: &str) -> VariableType {
    match data_type {
        "Integer" => VariableType::Integer,
        "Numeric" => VariableType::Real,
        "Boolean" => VariableType::Boolean,
        "Enumeration" => VariableType::Enum,
        "Set" => VariableType::Set,
        _ => VariableType::String,
    }
}

const MYSQL_ALL_SCOPES: [EditScope; 4] = [
    EditScope::Session,
    EditScope::Global,
    EditScope::Persist,
    EditScope::PersistOnly,
];
const MYSQL_GLOBAL_SCOPES: [EditScope; 3] = [
    EditScope::Global,
    EditScope::Persist,
    EditScope::PersistOnly,
];

/// The scopes the generated table (`mysql_readonly.rs`) says MySQL accepts
/// for a variable, from the table of the nearest probed version: the last
/// one at or below the server's version, else the oldest. `None` for a
/// variable the table does not know.
fn mysql_probed_scopes(server: &ServerInfo, name: &str) -> Option<Vec<EditScope>> {
    use super::mysql_readonly::{GLOBAL, PERSIST, PERSIST_ONLY, SESSION, TABLES};
    let version = (server.major, server.minor);
    let table = TABLES
        .iter()
        .rev()
        .find(|table| table.0 <= version)
        .or_else(|| TABLES.first())?;
    let lower = name.to_ascii_lowercase();
    let mask = table
        .2
        .binary_search_by(|(entry, _)| (*entry).cmp(lower.as_str()))
        .ok()
        .and_then(|index| table.2.get(index))
        .map(|(_, mask)| *mask)?;
    Some(
        [
            (SESSION, EditScope::Session),
            (GLOBAL, EditScope::Global),
            (PERSIST, EditScope::Persist),
            (PERSIST_ONLY, EditScope::PersistOnly),
        ]
        .into_iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, scope)| scope)
        .collect(),
    )
}

fn mysql_variable(
    server: &ServerInfo,
    row: &Row<'_>,
    has_metadata_table: bool,
    session_names: Option<&[String]>,
) -> LiveResult<ServerVariable> {
    let name = row.req_text("name")?;
    let has_global = row.bool("has_global")?.unwrap_or(false);
    let has_metadata = has_metadata_table && row.bool("has_metadata")?.unwrap_or(false);
    let global_value = if has_global {
        row.text("global_value")?
    } else {
        None
    };
    let reported_session = row.text("session_value")?;
    let min_value = row.text("min_value")?;
    let max_value = row.text("max_value")?;
    // Scope: from the metadata, else from this thread's variables, else
    // only "global or not".
    let scope = if has_metadata {
        row.text("scope")?
    } else {
        let has_session =
            session_names.map(|names| names.iter().any(|n| n.eq_ignore_ascii_case(&name)));
        match (has_global, has_session) {
            (false, _) => Some("SESSION_ONLY".to_string()),
            (true, Some(true)) => Some("SESSION".to_string()),
            (true, Some(false)) => Some("GLOBAL".to_string()),
            (true, None) => None,
        }
    };
    let (mut settable_scopes, session_value) = match scope.as_deref() {
        Some("GLOBAL") => (MYSQL_GLOBAL_SCOPES.to_vec(), None),
        Some("SESSION_ONLY") => (vec![EditScope::Session], reported_session),
        _ => (MYSQL_ALL_SCOPES.to_vec(), reported_session),
    };
    // MySQL publishes no read-only flag: the generated table says which
    // scopes the server accepted for this variable. Unknown variables keep
    // every scope their scope allows and rely on the server's error.
    let probed = mysql_probed_scopes(server, &name);
    if let Some(allowed) = &probed {
        settable_scopes.retain(|scope| allowed.contains(scope));
    }
    let read_only = probed.as_ref().is_some_and(|allowed| {
        !allowed.iter().any(|s| {
            matches!(
                s,
                EditScope::Session | EditScope::Global | EditScope::Persist
            )
        })
    });
    let restart_required = read_only && settable_scopes.contains(&EditScope::PersistOnly);
    let value = session_value.clone().or_else(|| global_value.clone());
    let (var_type, type_inferred) = match (has_metadata, row.text("data_type")?) {
        (true, Some(data_type)) => (mysql_metadata_type(&data_type), false),
        _ => (
            mysql_inferred_type(max_value.as_deref(), value.as_deref()),
            true,
        ),
    };
    let numeric = matches!(var_type, VariableType::Integer | VariableType::Real);
    let set_by = match (non_empty(row, "set_user")?, non_empty(row, "set_host")?) {
        (Some(user), Some(host)) => Some(format!("{user}@{host}")),
        (user, _) => user,
    };
    Ok(ServerVariable {
        name,
        value,
        session_value,
        global_value,
        default_value: None,
        reset_value: None,
        unit: None,
        category: None,
        description: non_empty(row, "description")?,
        var_type,
        type_inferred,
        context: scope,
        settable_scopes,
        dynamic: probed.as_ref().map(|_| !read_only),
        read_only,
        restart_required,
        pending_restart: false,
        min_value: if numeric { min_value } else { None },
        max_value: if numeric { max_value } else { None },
        enum_values: Vec::new(),
        source: row.text("source")?,
        source_file: non_empty(row, "path")?,
        set_by,
        set_time: row.text("set_time")?,
        persisted_value: row.text("persisted_value")?,
        masked: false,
    })
}

// MariaDB -----------------------------------------------------------------

fn mariadb_query(filter: Option<&str>) -> String {
    let filter = filter
        .map(|literal| format!(" WHERE VARIABLE_NAME = UPPER({literal})"))
        .unwrap_or_default();
    format!(
        "SELECT LOWER(VARIABLE_NAME) AS name, SESSION_VALUE AS session_value, \
         GLOBAL_VALUE AS global_value, GLOBAL_VALUE_ORIGIN AS source, \
         DEFAULT_VALUE AS default_value, VARIABLE_SCOPE AS scope, VARIABLE_TYPE AS data_type, \
         VARIABLE_COMMENT AS description, NUMERIC_MIN_VALUE AS min_value, \
         NUMERIC_MAX_VALUE AS max_value, ENUM_VALUE_LIST AS enum_values, \
         READ_ONLY AS read_only, GLOBAL_VALUE_PATH AS path \
         FROM information_schema.SYSTEM_VARIABLES{filter} ORDER BY VARIABLE_NAME"
    )
}

fn mariadb_type(data_type: &str) -> VariableType {
    let upper = data_type.to_ascii_uppercase();
    match upper.as_str() {
        "BOOLEAN" => VariableType::Boolean,
        "ENUM" => VariableType::Enum,
        "SET" => VariableType::Set,
        "DOUBLE" => VariableType::Real,
        _ if upper.contains("INT") => VariableType::Integer,
        // VARCHAR, FLAGSET (optimizer_switch style key=value lists), ...
        _ => VariableType::String,
    }
}

fn mariadb_variable(row: &Row<'_>) -> LiveResult<ServerVariable> {
    let scope = row.req_text("scope")?;
    let read_only = row.bool("read_only")?.unwrap_or(false);
    let settable_scopes = if read_only {
        Vec::new()
    } else {
        match scope.as_str() {
            "GLOBAL" => vec![EditScope::Global],
            "SESSION ONLY" => vec![EditScope::Session],
            _ => vec![EditScope::Session, EditScope::Global],
        }
    };
    let session_value = row.text("session_value")?;
    let global_value = row.text("global_value")?;
    let enum_values = row
        .text("enum_values")?
        .map(|list| {
            list.split(',')
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(ServerVariable {
        name: row.req_text("name")?,
        value: session_value.clone().or_else(|| global_value.clone()),
        session_value,
        global_value,
        default_value: row.text("default_value")?,
        reset_value: None,
        unit: None,
        category: None,
        description: non_empty(row, "description")?,
        var_type: mariadb_type(&row.req_text("data_type")?),
        type_inferred: false,
        context: Some(scope),
        settable_scopes,
        dynamic: Some(!read_only),
        read_only,
        restart_required: false,
        pending_restart: false,
        min_value: row.text("min_value")?,
        max_value: row.text("max_value")?,
        enum_values,
        source: row.text("source")?,
        source_file: non_empty(row, "path")?,
        set_by: None,
        set_time: None,
        persisted_value: None,
        masked: false,
    })
}

// Secrets ------------------------------------------------------------------

/// What a masked secret value shows instead.
pub const HIDDEN: &str = "(hidden)";

/// Variables whose whole value is a secret, by exact (lowercase) name.
const SECRET_NAMES: [&str; 3] = [
    "ssl_passphrase_command",
    "wsrep_sst_auth",
    "report_password",
];

/// Name parts that mark a string variable as a secret. Paths and file names
/// (`*_path`, `*_file`, `*_dir`) are not secrets, and numeric, boolean and
/// enum variables (`password_history`, `default_password_lifetime`) cannot
/// hold one.
const SECRET_PARTS: [&str; 6] = [
    "password",
    "secret",
    "passphrase",
    "token",
    "pwd",
    "filekey",
];

fn is_secret_name(variable: &ServerVariable) -> bool {
    let name = variable.name.to_ascii_lowercase();
    if SECRET_NAMES.contains(&name.as_str()) {
        return true;
    }
    let path_like = ["_path", "_file", "_dir"]
        .iter()
        .any(|suffix| name.ends_with(suffix));
    variable.var_type == VariableType::String
        && !path_like
        && SECRET_PARTS.iter().any(|part| name.contains(part))
}

/// True when a value may be a libpq connection string carrying a secret: a
/// `postgres://` URI, or `keyword=value` text naming a secret keyword. Other
/// secret-bearing values are handled by the redactor in [`clean_value`].
fn may_hold_conninfo_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("postgresql://")
        || lower.starts_with("postgres://")
        || (lower.contains('=')
            && [
                "password",
                "oauth_client_secret",
                "scram_client_key",
                "scram_server_key",
            ]
            .iter()
            .any(|k| lower.contains(k)))
}

/// The PostgreSQL value with connection string secrets removed: the
/// cleaned connection string, or [`HIDDEN`] when it does not parse (it
/// might hold a password where the parser did not expect one).
fn pg_clean_value(name: &str, value: &str) -> Option<String> {
    let conninfo_setting = name.to_ascii_lowercase().contains("conninfo");
    if value.is_empty() || !(conninfo_setting || may_hold_conninfo_secret(value)) {
        return None;
    }
    Some(
        varve_catalog::postgres::programs::strip_passwords(value)
            .unwrap_or_else(|| HIDDEN.to_string()),
    )
}

/// True when text holds a URI whose user info has a password
/// (`scheme://user:password@host`).
fn has_uri_password(text: &str) -> bool {
    text.match_indices("://").any(|(at, sep)| {
        let rest = &text[at + sep.len()..];
        let authority = rest
            .split(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace())
            .next()
            .unwrap_or_default();
        authority
            .rsplit_once('@')
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
    })
}

/// The value to show instead of `value`, when it holds a secret: a
/// PostgreSQL connection string without its secrets, otherwise the text
/// with secrets redacted by the activity log's redactor (URI credentials,
/// `key=value` secrets, `IDENTIFIED BY` and `PASSWORD` clauses in SQL such
/// as `init_connect`). A URI password the redactor cannot remove hides the
/// whole value. `None` when nothing needs masking.
fn clean_value(family: Family, name: &str, value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    if family == Family::Postgres
        && let Some(clean) = pg_clean_value(name, value)
    {
        return (clean != value).then_some(clean);
    }
    let dialect = match family {
        Family::Postgres => SqlDialect::Postgres,
        Family::Mysql | Family::Mariadb => SqlDialect::Mysql,
    };
    let redacted = redact_secrets(value, dialect);
    if redacted != value {
        Some(redacted)
    } else if has_uri_password(value) {
        Some(HIDDEN.to_string())
    } else {
        None
    }
}

/// Masks secret values in a variable read from the server.
fn mask(family: Family, mut variable: ServerVariable) -> ServerVariable {
    let secret = is_secret_name(&variable);
    let name = variable.name.clone();
    let mut masked = false;
    for field in [
        &mut variable.value,
        &mut variable.session_value,
        &mut variable.global_value,
        &mut variable.default_value,
        &mut variable.reset_value,
        &mut variable.persisted_value,
    ] {
        let Some(value) = field.as_mut() else {
            continue;
        };
        if secret {
            if !value.is_empty() {
                *value = HIDDEN.to_string();
                masked = true;
            }
        } else if let Some(clean) = clean_value(family, &name, value) {
            *value = clean;
            masked = true;
        }
    }
    variable.masked = masked;
    variable
}

/// Refuses a new value that is a masked placeholder: [`HIDDEN`], text with
/// the redactor's marker, or the masked text currently shown, which would
/// replace a secret (a connection string password) with its masked form.
fn check_not_masked(variable: &ServerVariable, value: &str) -> LiveResult<()> {
    let trimmed = value.trim();
    let shown = [
        &variable.value,
        &variable.session_value,
        &variable.global_value,
        &variable.reset_value,
        &variable.persisted_value,
    ];
    let is_placeholder = trimmed == HIDDEN
        || value.contains(REDACTED)
        || (variable.masked
            && shown
                .iter()
                .any(|field| field.as_deref().is_some_and(|f| f.trim() == trimmed)));
    if is_placeholder {
        return Err(LiveError::Invalid {
            message: format!(
                "{} holds a secret that is masked here; type the complete new value, including any password",
                variable.name
            ),
        });
    }
    Ok(())
}

// Edit plans --------------------------------------------------------------

/// PostgreSQL settings marked `GUC_LIST_QUOTE`: `SET` and `ALTER SYSTEM`
/// quote each string argument as an identifier, so a list must be passed
/// as one literal per element. PostgreSQL does not expose the flag in
/// `pg_settings`; this mirrors its `guc_tables.c` (13 to 18).
const PG_LIST_QUOTE: [&str; 6] = [
    "search_path",
    "temp_tablespaces",
    "session_preload_libraries",
    "shared_preload_libraries",
    "local_preload_libraries",
    "oauth_validator_libraries",
];

/// Plans a change to a variable.
///
/// The variable comes from [`server_variables`] or [`server_variable`];
/// the scope must be one of its [`ServerVariable::settable_scopes`] and the
/// value must fit its type. Plans are never transactional: PostgreSQL
/// rejects `ALTER SYSTEM` in a transaction, and MySQL `SET` needs none.
/// Run them with [`super::run_edit_plan`].
///
/// # Errors
///
/// [`LiveError::Unsupported`] for `SET PERSIST` scopes outside MySQL (with
/// a clear message on MariaDB) and for Redis, [`LiveError::Invalid`] for a
/// scope the variable does not have or a value that does not fit its type.
pub fn variable_edit_plan(
    server: &ServerInfo,
    variable: &ServerVariable,
    change: &VariableChange,
) -> LiveResult<EditPlan> {
    let family = Family::of(server)?;
    let scope = match change {
        VariableChange::Set { scope, .. } | VariableChange::Reset { scope } => *scope,
    };
    if matches!(scope, EditScope::Persist | EditScope::PersistOnly) && family != Family::Mysql {
        let what = if family == Family::Mariadb {
            "MariaDB has no SET PERSIST; set the variable globally and add it to an option file"
        } else {
            "SET PERSIST is a MySQL feature; use the global scope (ALTER SYSTEM)"
        };
        return Err(LiveError::Unsupported {
            message: what.into(),
        });
    }
    if !variable.settable_scopes.contains(&scope) {
        let allowed: Vec<&str> = variable.settable_scopes.iter().map(|s| s.label()).collect();
        let allowed = if allowed.is_empty() {
            "it is read-only".to_string()
        } else {
            format!("it can be changed in: {}", allowed.join(", "))
        };
        return Err(LiveError::Invalid {
            message: format!(
                "{} cannot be changed in the {} scope; {allowed}",
                variable.name,
                scope.label()
            ),
        });
    }
    if let VariableChange::Set { value, .. } = change {
        check_not_masked(variable, value)?;
    }
    match family {
        Family::Postgres => pg_plan(variable, change),
        Family::Mysql | Family::Mariadb => mysql_plan(family, variable, change),
    }
}

fn plan(statements: Vec<String>, warnings: Vec<String>) -> EditPlan {
    EditPlan {
        statements,
        transactional: false,
        destructive: false,
        warnings,
    }
}

fn pg_plan(variable: &ServerVariable, change: &VariableChange) -> LiveResult<EditPlan> {
    let name = sql::ident(Dialect::Postgres, &variable.name)?;
    let context = variable.context.as_deref().unwrap_or_default();
    let reload = "SELECT pg_catalog.pg_reload_conf()".to_string();
    let mut warnings = Vec::new();
    let global_warning = |warnings: &mut Vec<String>| {
        warnings.push(match context {
            "postmaster" => format!("{} only takes effect after a server restart", variable.name),
            "backend" | "superuser-backend" => format!(
                "the reload applies {} to new connections only",
                variable.name
            ),
            _ => format!(
                "the reload applies {} to every session that has not set it itself",
                variable.name
            ),
        });
        warnings.push("ALTER SYSTEM writes postgresql.auto.conf".into());
    };
    let statements = match change {
        VariableChange::Set {
            scope: EditScope::Session,
            value,
        } => vec![format!("SET {name} = {}", pg_value(variable, value)?)],
        VariableChange::Reset {
            scope: EditScope::Session,
        } => vec![format!("RESET {name}")],
        VariableChange::Set { value, .. } => {
            global_warning(&mut warnings);
            vec![
                format!("ALTER SYSTEM SET {name} = {}", pg_value(variable, value)?),
                reload,
            ]
        }
        VariableChange::Reset { .. } => {
            global_warning(&mut warnings);
            vec![format!("ALTER SYSTEM RESET {name}"), reload]
        }
    };
    Ok(plan(statements, warnings))
}

fn mysql_name(name: &str) -> LiveResult<String> {
    // Component and structured variables (`validate_password.length`) are
    // written part by part.
    Ok(name
        .split('.')
        .map(|part| sql::ident(Dialect::Mysql, part))
        .collect::<Result<Vec<_>, _>>()?
        .join("."))
}

fn mysql_plan(
    family: Family,
    variable: &ServerVariable,
    change: &VariableChange,
) -> LiveResult<EditPlan> {
    let name = mysql_name(&variable.name)?;
    let mut warnings = Vec::new();
    let keyword = |scope: EditScope| match scope {
        EditScope::Session => "SESSION",
        EditScope::Global => "GLOBAL",
        EditScope::Persist => "PERSIST",
        EditScope::PersistOnly => "PERSIST_ONLY",
    };
    let statement = match change {
        VariableChange::Set { scope, value } => {
            let value = mysql_value(variable, value)?;
            scope_warning(family, *scope, &mut warnings);
            format!("SET {} {name} = {value}", keyword(*scope))
        }
        VariableChange::Reset {
            scope: scope @ (EditScope::Session | EditScope::Global),
        } => {
            scope_warning(family, *scope, &mut warnings);
            if *scope == EditScope::Session {
                warnings.push("the session takes the global value".into());
            } else {
                warnings.push("the global value returns to the compiled-in default".into());
            }
            format!("SET {} {name} = DEFAULT", keyword(*scope))
        }
        VariableChange::Reset { .. } => {
            warnings.push(
                "RESET PERSIST removes the value from mysqld-auto.cnf; the running value does not change"
                    .into(),
            );
            format!("RESET PERSIST IF EXISTS {name}")
        }
    };
    Ok(plan(vec![statement], warnings))
}

fn scope_warning(family: Family, scope: EditScope, warnings: &mut Vec<String>) {
    match (family, scope) {
        (_, EditScope::Session) => {}
        (Family::Mariadb, _) => warnings.push(
            "SET GLOBAL applies to the server and new sessions; it is lost at restart unless also set in an option file"
                .into(),
        ),
        (_, EditScope::Global) => warnings.push(
            "SET GLOBAL applies to the server and new sessions; it is lost at restart".into(),
        ),
        (_, EditScope::Persist) => warnings.push(
            "SET PERSIST changes the running value and writes mysqld-auto.cnf, so it survives a restart"
                .into(),
        ),
        (_, EditScope::PersistOnly) => warnings.push(
            "SET PERSIST_ONLY only writes mysqld-auto.cnf: it takes effect at the next restart"
                .into(),
        ),
    }
}

// Values ------------------------------------------------------------------

fn invalid(message: String) -> LiveError {
    LiveError::Invalid { message }
}

fn parse_bool(name: &str, value: &str) -> LiveResult<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" | "t" | "y" => Ok(true),
        "off" | "false" | "no" | "0" | "f" | "n" => Ok(false),
        _ => Err(invalid(format!("{name} takes on or off, not {value:?}"))),
    }
}

/// A plain number: sign, digits, optional fraction and exponent (the
/// fraction and exponent only when `real`).
fn is_number(text: &str, real: bool) -> bool {
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(at) if real => (&body[..at], Some(&body[at + 1..])),
        Some(_) => return false,
        None => (body, None),
    };
    let mut parts = mantissa.splitn(2, '.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if fraction.is_some() && !real {
        return false;
    }
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = digits(whole)
        && fraction.is_none_or(digits)
        && !(whole.is_empty() && fraction.is_none_or(str::is_empty));
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['-', '+']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    mantissa_ok && exponent_ok
}

/// Checks a number (in the variable's base unit) against its range.
fn check_range(variable: &ServerVariable, number: f64) -> LiveResult<()> {
    let bound = |text: &Option<String>| text.as_deref().and_then(|t| t.trim().parse::<f64>().ok());
    if let Some(min) = bound(&variable.min_value)
        && number < min
    {
        return Err(invalid(format!(
            "{} must be at least {}",
            variable.name,
            variable.min_value.as_deref().unwrap_or_default()
        )));
    }
    if let Some(max) = bound(&variable.max_value)
        && number > max
    {
        return Err(invalid(format!(
            "{} must be at most {}",
            variable.name,
            variable.max_value.as_deref().unwrap_or_default()
        )));
    }
    Ok(())
}

fn enum_match(variable: &ServerVariable, value: &str) -> LiveResult<String> {
    let value = value.trim();
    if variable.enum_values.is_empty() {
        if value.is_empty() {
            return Err(invalid(format!("{} needs a value", variable.name)));
        }
        return Ok(value.to_string());
    }
    variable
        .enum_values
        .iter()
        .find(|allowed| allowed.eq_ignore_ascii_case(value))
        .cloned()
        .ok_or_else(|| {
            invalid(format!(
                "{} takes one of {}, not {value:?}",
                variable.name,
                variable.enum_values.join(", ")
            ))
        })
}

const PG_MEMORY_UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
const PG_TIME_UNITS: [&str; 6] = ["us", "ms", "s", "min", "h", "d"];

/// Validates a PostgreSQL number with an optional unit (`64MB`, `5min`).
fn pg_number(variable: &ServerVariable, value: &str) -> LiveResult<String> {
    let text = value.trim();
    let split = text
        .find(|c: char| c.is_ascii_alphabetic() && c != 'e' && c != 'E')
        .unwrap_or(text.len());
    let (number, unit) = (text[..split].trim(), text[split..].trim());
    // PostgreSQL 12 and newer round fractional values of integer settings,
    // so both types take a fraction; an exponent only makes sense for reals.
    let real = variable.var_type == VariableType::Real;
    if !is_number(number, true) || (!real && number.contains(['e', 'E'])) {
        return Err(invalid(format!(
            "{} takes a number{}, not {value:?}",
            variable.name,
            variable
                .unit
                .as_deref()
                .map(|u| format!(" (in {u} unless a unit is given)"))
                .unwrap_or_default()
        )));
    }
    if unit.is_empty() {
        let parsed: f64 = number
            .parse()
            .map_err(|_| invalid(format!("{number} is not a number")))?;
        check_range(variable, parsed)?;
    } else {
        let base = variable.unit.as_deref().unwrap_or_default();
        let allowed: &[&str] = if base.ends_with('B') {
            &PG_MEMORY_UNITS
        } else if PG_TIME_UNITS.contains(&base) {
            &PG_TIME_UNITS
        } else {
            &[]
        };
        if !allowed.contains(&unit) {
            return Err(invalid(if allowed.is_empty() {
                format!("{} takes no unit", variable.name)
            } else {
                format!(
                    "{} takes the units {} (case-sensitive), not {unit:?}",
                    variable.name,
                    allowed.join(", ")
                )
            }));
        }
    }
    Ok(format!("{number}{unit}"))
}

/// The list settings PostgreSQL reads with `SplitIdentifierString`
/// (unquoted elements lowercased); the library lists are read with
/// `SplitDirectoriesString`, which keeps their case and inner spaces.
const PG_IDENTIFIER_LISTS: [&str; 2] = ["search_path", "temp_tablespaces"];

/// Splits a list the way PostgreSQL reads list settings: comma separated,
/// `"double quoted"` elements kept as written (with `""` for a quote).
/// Unquoted elements are lowercased and end at whitespace for identifier
/// lists (`SplitIdentifierString`); for library lists (`directories`,
/// `SplitDirectoriesString`) they keep their case and run to the next
/// comma, with surrounding whitespace trimmed.
fn split_identifier_list(name: &str, text: &str, directories: bool) -> LiveResult<Vec<String>> {
    let mut items = Vec::new();
    let mut chars = text.chars().peekable();
    let skip_ws = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
    };
    skip_ws(&mut chars);
    if chars.peek().is_none() {
        return Ok(items);
    }
    loop {
        skip_ws(&mut chars);
        let mut item = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        item.push('"');
                    }
                    Some('"') => break,
                    Some(c) => item.push(c),
                    None => {
                        return Err(invalid(format!(
                            "{name}: a double quoted element is not closed"
                        )));
                    }
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' || (!directories && c.is_whitespace()) {
                    break;
                }
                item.push(if directories {
                    c
                } else {
                    c.to_ascii_lowercase()
                });
                chars.next();
            }
            if directories {
                item.truncate(item.trim_end().len());
            }
        }
        if item.is_empty() {
            return Err(invalid(format!("{name}: the list has an empty element")));
        }
        items.push(item);
        skip_ws(&mut chars);
        match chars.next() {
            None => return Ok(items),
            Some(',') => {}
            Some(c) => {
                return Err(invalid(format!(
                    "{name}: expected a comma between list elements, found {c:?}"
                )));
            }
        }
    }
}

/// The SQL for a PostgreSQL value (one or more string literals).
fn pg_value(variable: &ServerVariable, value: &str) -> LiveResult<String> {
    let literal = |text: &str| -> LiveResult<String> { Ok(sql::string(Dialect::Postgres, text)?) };
    match variable.var_type {
        VariableType::Boolean => literal(if parse_bool(&variable.name, value)? {
            "on"
        } else {
            "off"
        }),
        VariableType::Integer | VariableType::Real => literal(&pg_number(variable, value)?),
        VariableType::Enum => literal(&enum_match(variable, value)?),
        VariableType::Set | VariableType::String => {
            let lower = variable.name.to_ascii_lowercase();
            if PG_LIST_QUOTE.contains(&lower.as_str()) {
                let directories = !PG_IDENTIFIER_LISTS.contains(&lower.as_str());
                let items = split_identifier_list(&variable.name, value, directories)?;
                if items.is_empty() {
                    return literal("");
                }
                Ok(items
                    .iter()
                    .map(|item| literal(item))
                    .collect::<LiveResult<Vec<_>>>()?
                    .join(", "))
            } else {
                literal(value)
            }
        }
    }
}

/// The SQL for a MySQL or MariaDB value.
fn mysql_value(variable: &ServerVariable, value: &str) -> LiveResult<String> {
    let trimmed = value.trim();
    match variable.var_type {
        VariableType::Boolean => Ok(if parse_bool(&variable.name, value)? {
            "ON"
        } else {
            "OFF"
        }
        .into()),
        VariableType::Integer => {
            if !is_number(trimmed, false) {
                return Err(invalid(format!(
                    "{} takes a whole number, not {value:?}",
                    variable.name
                )));
            }
            let parsed: i128 = trimmed
                .parse()
                .map_err(|_| invalid(format!("{trimmed} is out of range")))?;
            let bound =
                |text: &Option<String>| text.as_deref().and_then(|t| t.trim().parse::<i128>().ok());
            if bound(&variable.min_value).is_some_and(|min| parsed < min)
                || bound(&variable.max_value).is_some_and(|max| parsed > max)
            {
                return Err(invalid(format!(
                    "{} must be between {} and {}",
                    variable.name,
                    variable.min_value.as_deref().unwrap_or("?"),
                    variable.max_value.as_deref().unwrap_or("?")
                )));
            }
            Ok(parsed.to_string())
        }
        VariableType::Real => {
            if !is_number(trimmed, true) {
                return Err(invalid(format!(
                    "{} takes a number, not {value:?}",
                    variable.name
                )));
            }
            let parsed: f64 = trimmed
                .parse()
                .map_err(|_| invalid(format!("{trimmed} is not a number")))?;
            check_range(variable, parsed)?;
            Ok(trimmed.to_string())
        }
        VariableType::Enum => Ok(sql::string(Dialect::Mysql, &enum_match(variable, value)?)?),
        VariableType::Set => {
            let mut items = Vec::new();
            for item in trimmed.split(',').map(str::trim).filter(|i| !i.is_empty()) {
                items.push(if variable.enum_values.is_empty() {
                    item.to_string()
                } else {
                    enum_match(variable, item)?
                });
            }
            Ok(sql::string(Dialect::Mysql, &items.join(","))?)
        }
        VariableType::String => Ok(sql::string(Dialect::Mysql, value)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_core::Engine;

    fn server(engine: Engine, major: u32) -> ServerInfo {
        ServerInfo {
            engine,
            version: format!("{major}.0"),
            major,
            minor: 0,
            patch: None,
        }
    }

    fn variable(name: &str, var_type: VariableType, scopes: &[EditScope]) -> ServerVariable {
        ServerVariable {
            name: name.into(),
            value: None,
            session_value: None,
            global_value: None,
            default_value: None,
            reset_value: None,
            unit: None,
            category: None,
            description: None,
            var_type,
            type_inferred: false,
            context: None,
            settable_scopes: scopes.to_vec(),
            dynamic: None,
            read_only: false,
            restart_required: false,
            pending_restart: false,
            min_value: None,
            max_value: None,
            enum_values: Vec::new(),
            source: None,
            source_file: None,
            set_by: None,
            set_time: None,
            persisted_value: None,
            masked: false,
        }
    }

    fn set(scope: EditScope, value: &str) -> VariableChange {
        VariableChange::Set {
            scope,
            value: value.into(),
        }
    }

    const PG_BOTH: [EditScope; 2] = [EditScope::Session, EditScope::Global];

    #[test]
    fn pg_session_and_system_plans() -> LiveResult<()> {
        let pg = server(Engine::Postgres, 18);
        let mut work_mem = variable("work_mem", VariableType::Integer, &PG_BOTH);
        work_mem.unit = Some("kB".into());
        work_mem.min_value = Some("64".into());
        work_mem.max_value = Some("2147483647".into());
        work_mem.context = Some("user".into());
        let session = variable_edit_plan(&pg, &work_mem, &set(EditScope::Session, " 64MB "))?;
        assert_eq!(session.statements, vec![r#"SET "work_mem" = '64MB'"#]);
        assert!(!session.transactional);
        let global = variable_edit_plan(&pg, &work_mem, &set(EditScope::Global, "4096"))?;
        assert_eq!(
            global.statements,
            vec![
                r#"ALTER SYSTEM SET "work_mem" = '4096'"#,
                "SELECT pg_catalog.pg_reload_conf()"
            ]
        );
        let reset = variable_edit_plan(
            &pg,
            &work_mem,
            &VariableChange::Reset {
                scope: EditScope::Global,
            },
        )?;
        assert_eq!(
            reset.statements,
            vec![
                r#"ALTER SYSTEM RESET "work_mem""#,
                "SELECT pg_catalog.pg_reload_conf()"
            ]
        );
        let session_reset = variable_edit_plan(
            &pg,
            &work_mem,
            &VariableChange::Reset {
                scope: EditScope::Session,
            },
        )?;
        assert_eq!(session_reset.statements, vec![r#"RESET "work_mem""#]);
        // Units are case-sensitive and must fit the setting.
        assert!(variable_edit_plan(&pg, &work_mem, &set(EditScope::Session, "64mb")).is_err());
        assert!(variable_edit_plan(&pg, &work_mem, &set(EditScope::Session, "5min")).is_err());
        assert!(variable_edit_plan(&pg, &work_mem, &set(EditScope::Session, "12")).is_err());
        assert!(variable_edit_plan(&pg, &work_mem, &set(EditScope::Session, "lots")).is_err());
        assert!(variable_edit_plan(&pg, &work_mem, &set(EditScope::Session, "1e3")).is_err());
        assert!(variable_edit_plan(&pg, &work_mem, &set(EditScope::Session, "1.5MB")).is_ok());
        Ok(())
    }

    #[test]
    fn pg_postmaster_settings_warn_about_restarts() -> LiveResult<()> {
        let pg = server(Engine::Postgres, 13);
        let mut max_connections = variable(
            "max_connections",
            VariableType::Integer,
            &[EditScope::Global],
        );
        max_connections.context = Some("postmaster".into());
        let plan = variable_edit_plan(&pg, &max_connections, &set(EditScope::Global, "200"))?;
        assert!(plan.warnings.iter().any(|w| w.contains("restart")));
        let refused = variable_edit_plan(&pg, &max_connections, &set(EditScope::Session, "200"));
        assert!(matches!(refused, Err(LiveError::Invalid { .. })));
        let persist = variable_edit_plan(&pg, &max_connections, &set(EditScope::Persist, "200"));
        assert!(matches!(persist, Err(LiveError::Unsupported { .. })));
        let internal = variable("block_size", VariableType::Integer, &[]);
        let refused = variable_edit_plan(&pg, &internal, &set(EditScope::Global, "8192"));
        assert!(
            matches!(refused, Err(LiveError::Invalid { message }) if message.contains("read-only"))
        );
        Ok(())
    }

    #[test]
    fn pg_values_by_type() -> LiveResult<()> {
        let mut bool_var = variable("enable_seqscan", VariableType::Boolean, &PG_BOTH);
        assert_eq!(pg_value(&bool_var, "TRUE")?, "'on'");
        assert_eq!(pg_value(&bool_var, "off")?, "'off'");
        assert!(pg_value(&bool_var, "maybe").is_err());
        bool_var.var_type = VariableType::Enum;
        bool_var.enum_values = vec!["debug5".into(), "notice".into(), "warning".into()];
        assert_eq!(pg_value(&bool_var, "NOTICE")?, "'notice'");
        assert!(pg_value(&bool_var, "loud").is_err());
        let mut real = variable("random_page_cost", VariableType::Real, &PG_BOTH);
        real.min_value = Some("0".into());
        real.max_value = Some("1.79769e+308".into());
        assert_eq!(pg_value(&real, "1.1")?, "'1.1'");
        assert_eq!(pg_value(&real, "2e1")?, "'2e1'");
        assert!(pg_value(&real, "-1").is_err());
        let mut delay = variable("vacuum_cost_delay", VariableType::Real, &PG_BOTH);
        delay.unit = Some("ms".into());
        assert_eq!(pg_value(&delay, "2ms")?, "'2ms'");
        assert!(pg_value(&delay, "2MB").is_err());
        let text = variable("application_name", VariableType::String, &PG_BOTH);
        assert_eq!(pg_value(&text, "it's")?, "'it''s'");
        Ok(())
    }

    #[test]
    fn pg_list_settings_pass_one_literal_per_element() -> LiveResult<()> {
        let path = variable("search_path", VariableType::String, &PG_BOTH);
        assert_eq!(
            pg_value(&path, r#""$user", Public, "My ""S""""#)?,
            r#"'$user', 'public', 'My "S"'"#
        );
        assert_eq!(pg_value(&path, "  ")?, "''");
        assert!(pg_value(&path, "a,,b").is_err());
        assert!(pg_value(&path, "\"open").is_err());
        assert!(pg_value(&path, "a b").is_err());
        Ok(())
    }

    #[test]
    fn pg_library_lists_keep_case_and_spaces() -> LiveResult<()> {
        let libs = variable(
            "shared_preload_libraries",
            VariableType::String,
            &[EditScope::Global],
        );
        assert_eq!(
            pg_value(&libs, " $libdir/My Ext , pg_stat_statements,\"Q,R\"")?,
            "'$libdir/My Ext', 'pg_stat_statements', 'Q,R'"
        );
        assert!(pg_value(&libs, "a,,b").is_err());
        Ok(())
    }

    #[test]
    fn secret_bearing_values_are_redacted() {
        let string = |name: &str, value: &str| {
            let mut v = variable(name, VariableType::String, &[EditScope::Global]);
            v.global_value = Some(value.into());
            v.value = Some(value.into());
            v
        };
        // SQL and DSNs in MySQL and MariaDB variables.
        let init = mask(
            Family::Mariadb,
            string("init_connect", "SET @dsn='mysql://app:ic_SECRET@db/x'"),
        );
        assert!(init.masked);
        let shown = init.global_value.as_deref().unwrap_or_default();
        assert!(
            !shown.contains("ic_SECRET") && shown.contains("mysql://app:"),
            "{shown}"
        );
        let replica = mask(
            Family::Mysql,
            string("init_replica", "CREATE USER u IDENTIFIED BY 'pw1'"),
        );
        assert!(
            !replica
                .global_value
                .as_deref()
                .unwrap_or_default()
                .contains("pw1")
        );
        // URIs with user info in any scheme, also in PostgreSQL settings.
        for uri in [
            "redis://:pw2@h:6379",
            "https://u:pw2@example.com/x",
            "mysql://a:pw2@h/db",
        ] {
            let v = mask(Family::Postgres, string("application_name", uri));
            assert!(v.masked, "{uri}");
            assert!(
                !v.value.as_deref().unwrap_or_default().contains("pw2"),
                "{uri}"
            );
        }
        // key=value secrets the redactor knows.
        let v = mask(Family::Postgres, string("cluster_name", "app token=abc123"));
        assert!(!v.value.as_deref().unwrap_or_default().contains("abc123"));
        // A URI password the redactor leaves alone hides the whole value.
        assert!(has_uri_password("x ftp://u:p@h"));
        assert!(!has_uri_password("https://example.com/a@b"));
        assert!(!has_uri_password("mailto user@example.com"));
        // More secret names.
        for name in [
            "hashicorp_key_management_token",
            "file_key_management_filekey",
            "authentication_ldap_sasl_bind_root_pwd",
        ] {
            let v = mask(Family::Mariadb, string(name, "s3cret"));
            assert_eq!(v.global_value.as_deref(), Some(HIDDEN), "{name}");
        }
        let plain = mask(Family::Mysql, string("init_connect", "SET NAMES utf8mb4"));
        assert!(!plain.masked);
    }

    #[test]
    fn masked_values_are_not_accepted_back() -> LiveResult<()> {
        let pg = server(Engine::Postgres, 18);
        let mut conninfo = variable(
            "primary_conninfo",
            VariableType::String,
            &[EditScope::Global],
        );
        conninfo.value = Some("host=primary user=rep password=pw".into());
        conninfo.reset_value = conninfo.value.clone();
        let conninfo = mask(Family::Postgres, conninfo);
        let stripped = conninfo.value.clone().unwrap_or_default();
        for value in [
            stripped.as_str(),
            HIDDEN,
            " (hidden) ",
            "host=x password=[redacted]",
        ] {
            let refused = variable_edit_plan(&pg, &conninfo, &set(EditScope::Global, value));
            assert!(
                matches!(refused, Err(LiveError::Invalid { .. })),
                "{value}: {refused:?}"
            );
        }
        // A complete new value is accepted.
        let plan = variable_edit_plan(
            &pg,
            &conninfo,
            &set(EditScope::Global, "host=primary user=rep password=new"),
        )?;
        assert_eq!(plan.statements.len(), 2);
        // Unmasked variables only refuse the placeholders.
        let app = variable("application_name", VariableType::String, &PG_BOTH);
        assert!(variable_edit_plan(&pg, &app, &set(EditScope::Session, HIDDEN)).is_err());
        assert!(variable_edit_plan(&pg, &app, &set(EditScope::Session, "psql")).is_ok());
        Ok(())
    }

    #[test]
    fn secrets_are_masked() {
        let mut conninfo = variable(
            "primary_conninfo",
            VariableType::String,
            &[EditScope::Global],
        );
        conninfo.value =
            Some("host=primary user=rep password='s3 cret' application_name=r1".into());
        conninfo.session_value = conninfo.value.clone();
        conninfo.reset_value = conninfo.value.clone();
        let masked = mask(Family::Postgres, conninfo);
        assert!(masked.masked);
        for value in [&masked.value, &masked.session_value, &masked.reset_value] {
            let value = value.as_deref().unwrap_or_default();
            assert!(!value.contains("s3 cret"), "{value}");
            assert!(value.contains("host=primary"), "{value}");
        }
        let mut uri = variable(
            "primary_conninfo",
            VariableType::String,
            &[EditScope::Global],
        );
        uri.value = Some("postgresql://rep:pw@primary/db".into());
        let masked = mask(Family::Postgres, uri);
        assert!(!masked.value.as_deref().unwrap_or_default().contains("pw@"));
        let mut broken = variable(
            "primary_conninfo",
            VariableType::String,
            &[EditScope::Global],
        );
        broken.value = Some("host=x password='unterminated".into());
        assert_eq!(
            mask(Family::Postgres, broken).value.as_deref(),
            Some(HIDDEN)
        );
        // Any setting whose value is a connection string with a password.
        let mut other = variable("my.dsn", VariableType::String, &PG_BOTH);
        other.value = Some("host=h password=pw".into());
        let other = mask(Family::Postgres, other);
        assert!(other.masked && !other.value.as_deref().unwrap_or_default().contains("pw"));
        let mut plain = variable(
            "primary_conninfo",
            VariableType::String,
            &[EditScope::Global],
        );
        plain.value = Some(String::new());
        assert!(!mask(Family::Postgres, plain).masked);
        let mut command = variable(
            "ssl_passphrase_command",
            VariableType::String,
            &[EditScope::Global],
        );
        command.value = Some("echo hunter2".into());
        assert_eq!(
            mask(Family::Postgres, command).value.as_deref(),
            Some(HIDDEN)
        );
        let mut sst = variable("wsrep_sst_auth", VariableType::String, &[EditScope::Global]);
        sst.global_value = Some("root:pw".into());
        let sst = mask(Family::Mariadb, sst);
        assert!(sst.masked);
        assert_eq!(sst.global_value.as_deref(), Some(HIDDEN));
        let mut report = variable(
            "report_password",
            VariableType::String,
            &[EditScope::Global],
        );
        report.global_value = Some("pw".into());
        assert_eq!(
            mask(Family::Mysql, report).global_value.as_deref(),
            Some(HIDDEN)
        );
        // Policy numbers and key paths are not secrets.
        let mut history = variable(
            "password_history",
            VariableType::Integer,
            &[EditScope::Global],
        );
        history.global_value = Some("5".into());
        assert!(!mask(Family::Mysql, history).masked);
        let mut path = variable(
            "sha256_password_private_key_path",
            VariableType::String,
            &[EditScope::Global],
        );
        path.global_value = Some("private_key.pem".into());
        assert!(!mask(Family::Mysql, path).masked);
        let mut app = variable("application_name", VariableType::String, &PG_BOTH);
        app.value = Some("a=b".into());
        assert!(!mask(Family::Postgres, app).masked);
    }

    #[test]
    fn mysql_plans_for_every_scope() -> LiveResult<()> {
        let my = server(Engine::Mysql, 9);
        let mut v = variable(
            "max_connect_errors",
            VariableType::Integer,
            &MYSQL_GLOBAL_SCOPES,
        );
        v.min_value = Some("1".into());
        v.max_value = Some("18446744073709551615".into());
        assert_eq!(
            variable_edit_plan(&my, &v, &set(EditScope::Global, " 200 "))?.statements,
            vec!["SET GLOBAL `max_connect_errors` = 200"]
        );
        assert_eq!(
            variable_edit_plan(&my, &v, &set(EditScope::Persist, "200"))?.statements,
            vec!["SET PERSIST `max_connect_errors` = 200"]
        );
        assert_eq!(
            variable_edit_plan(&my, &v, &set(EditScope::PersistOnly, "200"))?.statements,
            vec!["SET PERSIST_ONLY `max_connect_errors` = 200"]
        );
        assert_eq!(
            variable_edit_plan(
                &my,
                &v,
                &VariableChange::Reset {
                    scope: EditScope::Persist
                }
            )?
            .statements,
            vec!["RESET PERSIST IF EXISTS `max_connect_errors`"]
        );
        assert_eq!(
            variable_edit_plan(
                &my,
                &v,
                &VariableChange::Reset {
                    scope: EditScope::Global
                }
            )?
            .statements,
            vec!["SET GLOBAL `max_connect_errors` = DEFAULT"]
        );
        assert!(variable_edit_plan(&my, &v, &set(EditScope::Session, "1")).is_err());
        assert!(variable_edit_plan(&my, &v, &set(EditScope::Global, "0")).is_err());
        assert!(variable_edit_plan(&my, &v, &set(EditScope::Global, "1.5")).is_err());
        assert!(variable_edit_plan(&my, &v, &set(EditScope::Global, "1; DROP")).is_err());
        let dotted = variable(
            "validate_password.length",
            VariableType::Integer,
            &MYSQL_ALL_SCOPES,
        );
        assert_eq!(
            variable_edit_plan(&my, &dotted, &set(EditScope::Global, "8"))?.statements,
            vec!["SET GLOBAL `validate_password`.`length` = 8"]
        );
        Ok(())
    }

    #[test]
    fn mysql_values_by_type() -> LiveResult<()> {
        let flag = variable("autocommit", VariableType::Boolean, &MYSQL_ALL_SCOPES);
        assert_eq!(mysql_value(&flag, "true")?, "ON");
        assert_eq!(mysql_value(&flag, "0")?, "OFF");
        let mut mode = variable("sql_mode", VariableType::Set, &MYSQL_ALL_SCOPES);
        assert_eq!(
            mysql_value(&mode, "ansi, traditional")?,
            "'ansi,traditional'"
        );
        mode.enum_values = vec!["ANSI".into(), "TRADITIONAL".into()];
        assert_eq!(
            mysql_value(&mode, "ansi, traditional")?,
            "'ANSI,TRADITIONAL'"
        );
        assert_eq!(mysql_value(&mode, "")?, "''");
        assert!(mysql_value(&mode, "ansi,bogus").is_err());
        let mut format = variable("binlog_format", VariableType::Enum, &MYSQL_ALL_SCOPES);
        assert_eq!(mysql_value(&format, "ROW")?, "'ROW'");
        format.enum_values = vec!["MIXED".into(), "STATEMENT".into(), "ROW".into()];
        assert_eq!(mysql_value(&format, "row")?, "'ROW'");
        assert!(mysql_value(&format, "rows").is_err());
        let mut time = variable("long_query_time", VariableType::Real, &MYSQL_ALL_SCOPES);
        time.min_value = Some("0".into());
        time.max_value = Some("31536000".into());
        assert_eq!(mysql_value(&time, "2.5")?, "2.5");
        assert!(mysql_value(&time, "-1").is_err());
        assert!(mysql_value(&time, "2.5s").is_err());
        let text = variable("init_connect", VariableType::String, &MYSQL_GLOBAL_SCOPES);
        assert_eq!(
            mysql_value(&text, "SET x='a\\b'")?,
            "_utf8mb4 X'53455420783d27615c6227'"
        );
        Ok(())
    }

    #[test]
    fn mariadb_refuses_persist() -> LiveResult<()> {
        let maria = server(Engine::Mariadb, 11);
        let v = variable(
            "max_connections",
            VariableType::Integer,
            &[EditScope::Global],
        );
        assert!(matches!(
            variable_edit_plan(&maria, &v, &set(EditScope::Persist, "200")),
            Err(LiveError::Unsupported { message }) if message.contains("MariaDB")
        ));
        assert!(matches!(
            variable_edit_plan(
                &maria,
                &v,
                &VariableChange::Reset {
                    scope: EditScope::PersistOnly
                }
            ),
            Err(LiveError::Unsupported { .. })
        ));
        let plan = variable_edit_plan(&maria, &v, &set(EditScope::Global, "200"))?;
        assert_eq!(plan.statements, vec!["SET GLOBAL `max_connections` = 200"]);
        assert!(plan.warnings.iter().any(|w| w.contains("option file")));
        Ok(())
    }

    #[test]
    fn mysql_type_inference() {
        assert_eq!(
            mysql_inferred_type(Some("100000"), Some("151")),
            VariableType::Integer
        );
        assert_eq!(
            mysql_inferred_type(Some("4719230590769954816"), Some("10.000000")),
            VariableType::Real
        );
        assert_eq!(
            mysql_inferred_type(Some("0"), Some("ON")),
            VariableType::Boolean
        );
        assert_eq!(
            mysql_inferred_type(Some("0"), Some("1792378759.411519")),
            VariableType::Real
        );
        assert_eq!(
            mysql_inferred_type(Some("0"), Some("ROW")),
            VariableType::String
        );
        assert_eq!(
            mysql_inferred_type(Some("0"), Some("8.0.46")),
            VariableType::String
        );
        assert_eq!(mysql_inferred_type(None, None), VariableType::String);
        assert_eq!(mysql_metadata_type("Numeric"), VariableType::Real);
        assert_eq!(mysql_metadata_type("Enumeration"), VariableType::Enum);
        assert_eq!(mariadb_type("BIGINT UNSIGNED"), VariableType::Integer);
        assert_eq!(mariadb_type("FLAGSET"), VariableType::String);
        assert_eq!(mariadb_type("DOUBLE"), VariableType::Real);
    }

    #[test]
    fn mysql_probed_scope_table() {
        let at = |major, minor| ServerInfo {
            engine: Engine::Mysql,
            version: format!("{major}.{minor}"),
            major,
            minor,
            patch: None,
        };
        assert_eq!(mysql_probed_scopes(&at(9, 7), "version"), Some(vec![]));
        assert_eq!(
            mysql_probed_scopes(&at(8, 0), "innodb_buffer_pool_instances"),
            Some(vec![EditScope::PersistOnly])
        );
        assert_eq!(
            mysql_probed_scopes(&at(8, 4), "MAX_CONNECTIONS"),
            Some(MYSQL_GLOBAL_SCOPES.to_vec())
        );
        assert_eq!(
            mysql_probed_scopes(&at(9, 9), "pseudo_thread_id"),
            Some(vec![EditScope::Session])
        );
        // Older than every table: the oldest table; newer: the newest.
        assert!(mysql_probed_scopes(&at(7, 9), "autocommit").is_some());
        assert!(mysql_probed_scopes(&at(10, 0), "autocommit").is_some());
        assert_eq!(
            mysql_probed_scopes(&at(9, 7), "m2_live_no_such_variable"),
            None
        );
    }

    #[test]
    fn numbers() {
        assert!(is_number("12", false));
        assert!(is_number("-12", false));
        assert!(!is_number("1.5", false));
        assert!(is_number("1.5", true));
        assert!(is_number(".5", true));
        assert!(is_number("1e-3", true));
        assert!(!is_number("1e", true));
        assert!(!is_number("", true));
        assert!(!is_number(".", true));
        assert!(!is_number("0x10", false));
    }

    #[test]
    fn queries_quote_the_name_filter() -> LiveResult<()> {
        let literal = sql::string(Dialect::Postgres, "work'mem")?;
        assert!(pg_query(Some(&literal)).contains(
            "pg_catalog.lower(name) OPERATOR(pg_catalog.=) \
             pg_catalog.lower('work''mem'::pg_catalog.text)"
        ));
        assert!(mysql_query(true, None).contains("variables_metadata"));
        assert!(!mysql_query(false, None).contains("variables_metadata"));
        assert!(mariadb_query(Some("'x'")).contains("VARIABLE_NAME = UPPER('x')"));
        Ok(())
    }

    #[test]
    fn changes_serialize_for_the_ui() -> Result<(), serde_json::Error> {
        let change: VariableChange = serde_json::from_value(serde_json::json!({
            "action": "set", "scope": "persistOnly", "value": "1"
        }))?;
        assert_eq!(change, set(EditScope::PersistOnly, "1"));
        let v = variable("x", VariableType::Set, &[EditScope::Session]);
        let json = serde_json::to_value(&v)?;
        assert_eq!(json["varType"], "set");
        assert_eq!(json["settableScopes"][0], "session");
        Ok(())
    }
}
