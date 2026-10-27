//! The process list: what every connection is doing now, with cancel query
//! and terminate connection.
//!
//! - PostgreSQL: `pg_stat_activity` with `pg_blocking_pids`, cancel with
//!   `pg_cancel_backend` and terminate with `pg_terminate_backend` (their
//!   boolean result is reported).
//! - MySQL: `performance_schema.processlist` on 8.0.22 and newer, else
//!   `information_schema.PROCESSLIST`. A user without the `PROCESS`
//!   privilege may see no rows at all in the performance schema table (MySQL
//!   8.0 filters it differently), so a result without the own connection
//!   falls back to the information schema table. `KILL QUERY` and
//!   `KILL CONNECTION`.
//! - MariaDB: `information_schema.PROCESSLIST`, with its progress columns.
//!   `KILL QUERY` and `KILL CONNECTION`.
//!
//! The session's own connection is marked and can never be killed from
//! here. Statement text of every connection goes through
//! [`varve_activity::redact_secrets`], so passwords in other sessions'
//! statements (`CREATE USER ... PASSWORD`, `ALTER ROLE ...`) are never shown.

use serde::{Deserialize, Serialize};
use varve_activity::{SqlDialect, redact_secrets};
use varve_catalog::rows::{self, Row, Rows};
use varve_driver::{ServerInfo, Session};

use super::{Family, LiveError, LiveResult, at_least_patch, float, mysql_error_number, non_empty};

/// `ER_NO_SUCH_THREAD`: the connection to kill does not exist (any more).
const ER_NO_SUCH_THREAD: u32 = 1094;

/// One server connection (PostgreSQL backend, MySQL or MariaDB thread).
///
/// Fields an engine does not report are `None` (or empty).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    /// Backend process id (PostgreSQL) or connection id (MySQL, MariaDB):
    /// what cancel and terminate take.
    pub id: i64,
    /// True for the connection this panel runs on.
    pub own: bool,
    /// User name.
    pub user: Option<String>,
    /// Database (PostgreSQL) or default schema (MySQL, MariaDB).
    pub database: Option<String>,
    /// Client application name (PostgreSQL `application_name`), with
    /// secrets redacted (some clients put connection URIs in it).
    pub application: Option<String>,
    /// Client address: PostgreSQL client IP, MySQL and MariaDB `host:port`
    /// as the server reports it.
    pub client_address: Option<String>,
    /// Client host name (PostgreSQL, when `log_hostname` is on).
    pub client_hostname: Option<String>,
    /// Client TCP port (PostgreSQL; `-1` for a Unix socket).
    pub client_port: Option<i64>,
    /// PostgreSQL backend type (`client backend`, `autovacuum worker`, ...).
    pub backend_type: Option<String>,
    /// MySQL and MariaDB command (`Query`, `Sleep`, `Binlog Dump`, ...).
    pub command: Option<String>,
    /// State: PostgreSQL `active`, `idle`, `idle in transaction`, ...;
    /// MySQL and MariaDB thread state (`executing`, `User sleep`, ...).
    pub state: Option<String>,
    /// PostgreSQL wait event type (`Lock`, `IO`, `Client`, ...).
    pub wait_event_type: Option<String>,
    /// PostgreSQL wait event.
    pub wait_event: Option<String>,
    /// When the connection started (PostgreSQL).
    pub backend_start: Option<String>,
    /// When the current transaction started (PostgreSQL).
    pub xact_start: Option<String>,
    /// When the current or last query started (PostgreSQL).
    pub query_start: Option<String>,
    /// How long the connection has been in its current state, in
    /// milliseconds: for an active PostgreSQL backend since its query
    /// started, otherwise since its state last changed; MySQL `TIME` (whole
    /// seconds) and MariaDB `TIME_MS`.
    pub duration_ms: Option<i64>,
    /// Current (or for an idle PostgreSQL backend, last) statement text,
    /// with passwords and other credentials removed by the activity log's
    /// secret redactor (the raw text is never returned).
    pub query: Option<String>,
    /// Backends blocking this one (PostgreSQL `pg_blocking_pids`).
    pub blocked_by: Vec<i64>,
    /// Leader of a parallel worker (PostgreSQL).
    pub leader_id: Option<i64>,
    /// Query identifier (PostgreSQL 14 and newer with `compute_query_id`,
    /// MariaDB `QUERY_ID`). Decimal text in JSON: PostgreSQL's is a 64-bit
    /// hash, which a JavaScript number would round so it no longer matches
    /// `pg_stat_statements.queryid`.
    #[serde(with = "query_id_text")]
    pub query_id: Option<i64>,
    /// Progress of a long statement in percent (MariaDB `PROGRESS`, 0 when
    /// the statement reports none).
    pub progress: Option<f64>,
    /// Current stage of a long statement (MariaDB `STAGE`).
    pub stage: Option<i64>,
    /// Number of stages (MariaDB `MAX_STAGE`).
    pub max_stage: Option<i64>,
    /// Memory used by the connection in bytes (MariaDB `MEMORY_USED`).
    pub memory_bytes: Option<i64>,
    /// Rows examined by the current statement (MariaDB `EXAMINED_ROWS`).
    pub rows_examined: Option<i64>,
}

/// `Option<i64>` as decimal text, read from text or a JSON integer.
mod query_id_text {
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrText {
        Int(i64),
        Text(String),
    }

    #[allow(clippy::ref_option)]
    pub(super) fn serialize<S: Serializer>(
        value: &Option<i64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => serializer.serialize_str(&v.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<i64>, D::Error> {
        match Option::<IntOrText>::deserialize(deserializer)? {
            None => Ok(None),
            Some(IntOrText::Int(v)) => Ok(Some(v)),
            Some(IntOrText::Text(t)) => t
                .trim()
                .parse()
                .map(Some)
                .map_err(|_| serde::de::Error::custom(format!("{t:?} is not a 64-bit integer"))),
        }
    }
}

/// The process list of a server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessList {
    /// The connection id of the session that read the list.
    pub own_id: Option<i64>,
    /// Where the list came from (`pg_catalog.pg_stat_activity`,
    /// `performance_schema.processlist`, `information_schema.PROCESSLIST`).
    pub source: String,
    /// The connections, ordered by id.
    pub processes: Vec<Process>,
}

/// What to kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KillTarget {
    /// Cancel the running statement and keep the connection.
    Query,
    /// Close the connection (rolling back its open transaction).
    Connection,
}

/// What a kill request did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KillOutcome {
    /// The connection the request was for.
    pub id: i64,
    /// What was killed.
    pub target: KillTarget,
    /// The statement that ran.
    pub statement: String,
    /// True when the server accepted the request: PostgreSQL
    /// `pg_cancel_backend` / `pg_terminate_backend` returned true, MySQL or
    /// MariaDB `KILL` succeeded. False when the process does not exist (any
    /// more) or is not a backend that can be signalled.
    pub signalled: bool,
    /// The server's explanation when not signalled.
    pub message: Option<String>,
}

const PG_SOURCE: &str = "pg_catalog.pg_stat_activity";
const PS_SOURCE: &str = "performance_schema.processlist";
const IS_SOURCE: &str = "information_schema.PROCESSLIST";

/// The `pg_stat_activity` query; `query_id` exists from PostgreSQL 14.
fn pg_query(server: &ServerInfo) -> String {
    let query_id = if server.at_least(14, 0) {
        "a.query_id"
    } else {
        "NULL::bigint"
    };
    // Every name is qualified: the statement is parsed under the user's
    // search_path, where an unqualified function or operator could resolve
    // to an object planted in another schema.
    format!(
        "SELECT a.pid, a.pid OPERATOR(pg_catalog.=) pg_catalog.pg_backend_pid() AS own, \
         a.usename AS user_name, a.datname AS database_name, a.application_name, \
         pg_catalog.host(a.client_addr) AS client_address, \
         a.client_hostname, a.client_port, a.backend_type, a.state, a.wait_event_type, \
         a.wait_event, a.backend_start, a.xact_start, a.query_start, \
         (pg_catalog.date_part('epoch', pg_catalog.clock_timestamp() OPERATOR(pg_catalog.-) \
         CASE WHEN a.state OPERATOR(pg_catalog.=) 'active' \
         THEN a.query_start ELSE a.state_change END) OPERATOR(pg_catalog.*) 1000)::bigint \
         AS duration_ms, \
         a.query, pg_catalog.array_to_string(pg_catalog.pg_blocking_pids(a.pid), ',') \
         AS blocked_by, a.leader_pid, {query_id} AS query_id \
         FROM pg_catalog.pg_stat_activity a ORDER BY a.pid"
    )
}

/// The MySQL process list query on one of the two tables.
fn mysql_query(table: &str) -> String {
    format!(
        "SELECT ID AS id, ID = CONNECTION_ID() AS own, USER AS user_name, HOST AS host, \
         DB AS db, COMMAND AS command, TIME AS time_s, STATE AS state, INFO AS info \
         FROM {table} ORDER BY ID"
    )
}

/// The MariaDB process list query.
const MARIADB_QUERY: &str = "SELECT ID AS id, ID = CONNECTION_ID() AS own, USER AS user_name, \
     HOST AS host, DB AS db, COMMAND AS command, TIME AS time_s, TIME_MS AS time_ms, \
     STATE AS state, INFO AS info, PROGRESS AS progress, STAGE AS stage, \
     MAX_STAGE AS max_stage, MEMORY_USED AS memory_used, EXAMINED_ROWS AS examined_rows, \
     QUERY_ID AS query_id FROM information_schema.PROCESSLIST ORDER BY ID";

/// Reads the process list.
///
/// # Errors
///
/// [`LiveError::Unsupported`] for Redis, [`LiveError::PermissionDenied`]
/// when the user may not read the list, other server errors as
/// [`LiveError::Driver`].
pub async fn process_list(session: &dyn Session, server: &ServerInfo) -> LiveResult<ProcessList> {
    match Family::of(server)? {
        Family::Postgres => {
            let rows = rows::query(session, &pg_query(server)).await?;
            build_list(PG_SOURCE, &rows, pg_process)
        }
        Family::Mariadb => {
            let rows = rows::query(session, MARIADB_QUERY).await?;
            build_list(IS_SOURCE, &rows, mariadb_process)
        }
        Family::Mysql => {
            if at_least_patch(server, 8, 0, 22) {
                match rows::query(session, &mysql_query(PS_SOURCE))
                    .await
                    .map_err(LiveError::from)
                {
                    Ok(rows) => {
                        let list = build_list(PS_SOURCE, &rows, mysql_process)?;
                        if list.own_id.is_some() {
                            return Ok(list);
                        }
                    }
                    // Without SELECT on the performance schema the
                    // information schema table still shows what the user
                    // may see.
                    Err(LiveError::PermissionDenied { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
            let rows = rows::query(session, &mysql_query(IS_SOURCE)).await?;
            build_list(IS_SOURCE, &rows, mysql_process)
        }
    }
}

fn build_list(
    source: &str,
    rows: &Rows,
    map: fn(&Row<'_>) -> LiveResult<Process>,
) -> LiveResult<ProcessList> {
    let processes = rows
        .iter()
        .map(|row| map(&row))
        .collect::<LiveResult<Vec<_>>>()?;
    let own_id = processes.iter().find(|p| p.own).map(|p| p.id);
    Ok(ProcessList {
        own_id,
        source: source.into(),
        processes,
    })
}

fn required_id(row: &Row<'_>, column: &str) -> LiveResult<i64> {
    row.int(column)?.ok_or_else(|| LiveError::Invalid {
        message: format!("column {column} is NULL"),
    })
}

/// A comma separated list of ids (`array_to_string` of an `int[]`).
fn id_list(text: Option<String>) -> LiveResult<Vec<i64>> {
    let Some(text) = text else {
        return Ok(Vec::new());
    };
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse().map_err(|_| LiveError::Invalid {
                message: format!("{part} is not a process id"),
            })
        })
        .collect()
}

fn pg_process(row: &Row<'_>) -> LiveResult<Process> {
    Ok(Process {
        id: required_id(row, "pid")?,
        own: row.bool("own")?.unwrap_or(false),
        user: row.text("user_name")?,
        database: row.text("database_name")?,
        application: non_empty(row, "application_name")?
            .map(|a| redact_secrets(&a, SqlDialect::Postgres)),
        client_address: row.text("client_address")?,
        client_hostname: row.text("client_hostname")?,
        client_port: row.int("client_port")?,
        backend_type: row.text("backend_type")?,
        command: None,
        state: row.text("state")?,
        wait_event_type: row.text("wait_event_type")?,
        wait_event: row.text("wait_event")?,
        backend_start: row.text("backend_start")?,
        xact_start: row.text("xact_start")?,
        query_start: row.text("query_start")?,
        duration_ms: row.int("duration_ms")?,
        query: non_empty(row, "query")?.map(|q| redact_secrets(&q, SqlDialect::Postgres)),
        blocked_by: id_list(row.text("blocked_by")?)?,
        leader_id: row.int("leader_pid")?,
        query_id: row.int("query_id")?,
        progress: None,
        stage: None,
        max_stage: None,
        memory_bytes: None,
        rows_examined: None,
    })
}

fn mysql_process(row: &Row<'_>) -> LiveResult<Process> {
    Ok(Process {
        id: required_id(row, "id")?,
        own: row.bool("own")?.unwrap_or(false),
        user: row.text("user_name")?,
        database: row.text("db")?,
        application: None,
        client_address: non_empty(row, "host")?,
        client_hostname: None,
        client_port: None,
        backend_type: None,
        command: row.text("command")?,
        state: non_empty(row, "state")?,
        wait_event_type: None,
        wait_event: None,
        backend_start: None,
        xact_start: None,
        query_start: None,
        duration_ms: row.int("time_s")?.map(|s| s.saturating_mul(1000)),
        query: non_empty(row, "info")?.map(|q| redact_secrets(&q, SqlDialect::Mysql)),
        blocked_by: Vec::new(),
        leader_id: None,
        query_id: None,
        progress: None,
        stage: None,
        max_stage: None,
        memory_bytes: None,
        rows_examined: None,
    })
}

fn mariadb_process(row: &Row<'_>) -> LiveResult<Process> {
    let mut process = mysql_process(row)?;
    // TIME_MS is a DECIMAL with three fraction digits.
    if let Some(ms) = float(row, "time_ms")? {
        process.duration_ms = Some(ms.round() as i64);
    }
    process.progress = float(row, "progress")?;
    process.stage = row.int("stage")?;
    process.max_stage = row.int("max_stage")?;
    process.memory_bytes = row.int("memory_used")?;
    process.rows_examined = row.int("examined_rows")?;
    process.query_id = row.int("query_id")?;
    Ok(process)
}

/// The id of the session's own connection: PostgreSQL `pg_backend_pid()`,
/// MySQL and MariaDB `CONNECTION_ID()`.
///
/// # Errors
///
/// [`LiveError::Unsupported`] for Redis and server errors.
pub async fn own_connection_id(session: &dyn Session, server: &ServerInfo) -> LiveResult<i64> {
    let sql = match Family::of(server)? {
        Family::Postgres => "SELECT pg_catalog.pg_backend_pid() AS id",
        Family::Mysql | Family::Mariadb => "SELECT CONNECTION_ID() AS id",
    };
    let rows = rows::query(session, sql).await?;
    required_id(&rows.single("the own connection id")?, "id")
}

/// The statement that kills `id`. Refuses the own connection.
///
/// # Errors
///
/// [`LiveError::OwnConnection`] when `id` is `own_id`,
/// [`LiveError::Invalid`] for a non-positive id, [`LiveError::Unsupported`]
/// for Redis.
pub fn kill_statement(
    server: &ServerInfo,
    own_id: i64,
    id: i64,
    target: KillTarget,
) -> LiveResult<String> {
    let family = Family::of(server)?;
    if id <= 0 {
        return Err(LiveError::Invalid {
            message: format!("{id} is not a connection id"),
        });
    }
    if id == own_id {
        return Err(LiveError::OwnConnection { id });
    }
    Ok(match (family, target) {
        (Family::Postgres, KillTarget::Query) => {
            format!("SELECT pg_catalog.pg_cancel_backend({id}) AS signalled")
        }
        (Family::Postgres, KillTarget::Connection) => {
            format!("SELECT pg_catalog.pg_terminate_backend({id}) AS signalled")
        }
        (Family::Mysql | Family::Mariadb, KillTarget::Query) => format!("KILL QUERY {id}"),
        (Family::Mysql | Family::Mariadb, KillTarget::Connection) => {
            format!("KILL CONNECTION {id}")
        }
    })
}

/// Cancels the running statement of a connection or terminates it.
///
/// The own connection is looked up first and refused. PostgreSQL reports
/// whether the backend was signalled (with the server's warning when not);
/// MySQL and MariaDB report an unknown connection id as not signalled.
///
/// # Errors
///
/// [`LiveError::OwnConnection`] for the own connection,
/// [`LiveError::PermissionDenied`] when the user may not kill that
/// connection (another user's backend without the privilege, a superuser
/// backend), other server errors as [`LiveError::Driver`].
pub async fn kill(
    session: &dyn Session,
    server: &ServerInfo,
    id: i64,
    target: KillTarget,
) -> LiveResult<KillOutcome> {
    let family = Family::of(server)?;
    let own_id = own_connection_id(session, server).await?;
    let statement = kill_statement(server, own_id, id, target)?;
    let outcome = |signalled: bool, message: Option<String>| KillOutcome {
        id,
        target,
        statement: statement.clone(),
        signalled,
        message,
    };
    match family {
        Family::Postgres => {
            let output = rows::run(session, &statement).await?;
            let message = output
                .notices
                .iter()
                .map(|notice| notice.message.clone())
                .reduce(|a, b| format!("{a}\n{b}"));
            let rows = Rows::first(output)?;
            let signalled = rows
                .single("the kill result")?
                .bool("signalled")?
                .unwrap_or(false);
            let message = if signalled {
                None
            } else {
                Some(message.unwrap_or_else(|| format!("process {id} was not signalled")))
            };
            Ok(outcome(signalled, message))
        }
        Family::Mysql | Family::Mariadb => match rows::run(session, &statement).await {
            Ok(_) => Ok(outcome(true, None)),
            Err(error) => {
                let error = LiveError::from(error);
                match &error {
                    LiveError::Driver { error: driver }
                        if driver
                            .code
                            .as_deref()
                            .and_then(mysql_error_number)
                            .is_some_and(|n| n == ER_NO_SUCH_THREAD) =>
                    {
                        Ok(outcome(false, Some(driver.message.clone())))
                    }
                    _ => Err(error),
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_core::Engine;
    use varve_driver::{Column, ExecOutput, StatementResult, Value};

    fn server(engine: Engine, major: u32, minor: u32, patch: Option<u32>) -> ServerInfo {
        ServerInfo {
            engine,
            version: format!("{major}.{minor}"),
            major,
            minor,
            patch,
        }
    }

    fn rows(columns: &[&str], values: Vec<Vec<Value>>) -> LiveResult<Rows> {
        Ok(Rows::first(ExecOutput {
            results: vec![StatementResult::Rows {
                columns: columns
                    .iter()
                    .map(|name| Column {
                        name: (*name).into(),
                        type_name: "text".into(),
                    })
                    .collect(),
                rows: values,
                truncated: false,
            }],
            notices: vec![],
            elapsed_ms: 0,
        })?)
    }

    #[test]
    fn pg_query_gates_query_id() {
        assert!(
            pg_query(&server(Engine::Postgres, 13, 0, None)).contains("NULL::bigint AS query_id")
        );
        assert!(
            pg_query(&server(Engine::Postgres, 18, 0, None)).contains("a.query_id AS query_id")
        );
    }

    #[test]
    fn maps_pg_rows() -> LiveResult<()> {
        let columns = [
            "pid",
            "own",
            "user_name",
            "database_name",
            "application_name",
            "client_address",
            "client_hostname",
            "client_port",
            "backend_type",
            "state",
            "wait_event_type",
            "wait_event",
            "backend_start",
            "xact_start",
            "query_start",
            "duration_ms",
            "query",
            "blocked_by",
            "leader_pid",
            "query_id",
        ];
        let set = rows(
            &columns,
            vec![
                vec![
                    Value::Int(10),
                    Value::Bool(true),
                    Value::Text("varve".into()),
                    Value::Text("varve".into()),
                    Value::Text("".into()),
                    Value::Text("172.18.0.1".into()),
                    Value::Null,
                    Value::Int(50000),
                    Value::Text("client backend".into()),
                    Value::Text("active".into()),
                    Value::Null,
                    Value::Null,
                    Value::TimestampTz("2026-10-19T10:00:00Z".into()),
                    Value::TimestampTz("2026-10-19T10:00:01Z".into()),
                    Value::TimestampTz("2026-10-19T10:00:01Z".into()),
                    Value::Int(12),
                    Value::Text("SELECT 1".into()),
                    Value::Text("".into()),
                    Value::Null,
                    Value::Null,
                ],
                vec![
                    Value::Int(11),
                    Value::Bool(false),
                    Value::Null,
                    Value::Null,
                    Value::Text("".into()),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Text("autovacuum launcher".into()),
                    Value::Null,
                    Value::Text("Activity".into()),
                    Value::Text("AutovacuumMain".into()),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Text("".into()),
                    Value::Text("12,13".into()),
                    Value::Int(10),
                    Value::Int(-4_611_686_018_427_387_903),
                ],
            ],
        )?;
        let list = build_list(PG_SOURCE, &set, pg_process)?;
        assert_eq!(list.own_id, Some(10));
        let own = &list.processes[0];
        assert!(own.own);
        assert_eq!(own.application, None);
        assert_eq!(own.client_address.as_deref(), Some("172.18.0.1"));
        assert_eq!(own.duration_ms, Some(12));
        assert_eq!(own.query.as_deref(), Some("SELECT 1"));
        assert!(own.blocked_by.is_empty());
        let other = &list.processes[1];
        assert_eq!(other.blocked_by, vec![12, 13]);
        assert_eq!(other.leader_id, Some(10));
        assert_eq!(other.query_id, Some(-4_611_686_018_427_387_903));
        // A 64-bit query id travels as exact decimal text, so it still
        // matches pg_stat_statements.queryid in the UI.
        let json = serde_json::to_value(other).map_err(|e| LiveError::Invalid {
            message: e.to_string(),
        })?;
        assert_eq!(json["queryId"], "-4611686018427387903");
        assert_eq!(
            serde_json::to_value(own).map_err(|e| LiveError::Invalid {
                message: e.to_string(),
            })?["queryId"],
            serde_json::Value::Null
        );
        let back: Process = serde_json::from_value(json).map_err(|e| LiveError::Invalid {
            message: e.to_string(),
        })?;
        assert_eq!(back.query_id, other.query_id);
        assert_eq!(other.query, None);
        assert_eq!(other.wait_event.as_deref(), Some("AutovacuumMain"));
        Ok(())
    }

    #[test]
    fn maps_mariadb_rows() -> LiveResult<()> {
        let columns = [
            "id",
            "own",
            "user_name",
            "host",
            "db",
            "command",
            "time_s",
            "time_ms",
            "state",
            "info",
            "progress",
            "stage",
            "max_stage",
            "memory_used",
            "examined_rows",
            "query_id",
        ];
        let set = rows(
            &columns,
            vec![vec![
                Value::Int(5),
                Value::Int(0),
                Value::Text("root".into()),
                Value::Text("172.18.0.1:5000".into()),
                Value::Null,
                Value::Text("Query".into()),
                Value::Int(3),
                Value::Decimal("3456.789".into()),
                Value::Text("User sleep".into()),
                Value::Text("SELECT SLEEP(30)".into()),
                Value::Decimal("12.500".into()),
                Value::Int(1),
                Value::Int(2),
                Value::Int(81920),
                Value::Int(0),
                Value::Int(99),
            ]],
        )?;
        let list = build_list(IS_SOURCE, &set, mariadb_process)?;
        assert_eq!(list.own_id, None);
        let p = &list.processes[0];
        assert_eq!(p.duration_ms, Some(3457));
        assert_eq!(p.progress, Some(12.5));
        assert_eq!(p.max_stage, Some(2));
        assert_eq!(p.memory_bytes, Some(81920));
        assert_eq!(p.query_id, Some(99));
        assert_eq!(p.command.as_deref(), Some("Query"));
        Ok(())
    }

    #[test]
    fn mysql_rows_use_whole_seconds() -> LiveResult<()> {
        let columns = [
            "id",
            "own",
            "user_name",
            "host",
            "db",
            "command",
            "time_s",
            "state",
            "info",
        ];
        let set = rows(
            &columns,
            vec![vec![
                Value::UInt(7),
                Value::Int(1),
                Value::Text("event_scheduler".into()),
                Value::Text("localhost".into()),
                Value::Null,
                Value::Text("Daemon".into()),
                Value::Int(4),
                Value::Text("".into()),
                Value::Null,
            ]],
        )?;
        let list = build_list(PS_SOURCE, &set, mysql_process)?;
        assert_eq!(list.own_id, Some(7));
        assert_eq!(list.processes[0].duration_ms, Some(4000));
        assert_eq!(list.processes[0].state, None);
        Ok(())
    }

    #[test]
    fn statement_text_is_redacted() -> LiveResult<()> {
        let pg_columns = [
            "pid",
            "own",
            "user_name",
            "database_name",
            "application_name",
            "client_address",
            "client_hostname",
            "client_port",
            "backend_type",
            "state",
            "wait_event_type",
            "wait_event",
            "backend_start",
            "xact_start",
            "query_start",
            "duration_ms",
            "query",
            "blocked_by",
            "leader_pid",
            "query_id",
        ];
        let mut values = vec![Value::Null; pg_columns.len()];
        values[0] = Value::Int(3);
        values[16] = Value::Text("ALTER ROLE app PASSWORD 'hunter2'".into());
        values[4] = Value::Text("postgresql://app:hunter3@db/x".into());
        let list = build_list(PG_SOURCE, &rows(&pg_columns, vec![values])?, pg_process)?;
        let query = list.processes[0].query.clone().unwrap_or_default();
        assert!(query.starts_with("ALTER ROLE app PASSWORD"), "{query}");
        assert!(!query.contains("hunter2"), "{query}");
        let application = list.processes[0].application.clone().unwrap_or_default();
        assert!(
            application.starts_with("postgresql://app:"),
            "{application}"
        );
        assert!(!application.contains("hunter3"), "{application}");
        let my_columns = [
            "id",
            "own",
            "user_name",
            "host",
            "db",
            "command",
            "time_s",
            "state",
            "info",
        ];
        let mut values = vec![Value::Null; my_columns.len()];
        values[0] = Value::Int(4);
        values[8] = Value::Text("CREATE USER u IDENTIFIED BY 'hunter2'".into());
        let list = build_list(PS_SOURCE, &rows(&my_columns, vec![values])?, mysql_process)?;
        let query = list.processes[0].query.clone().unwrap_or_default();
        assert!(query.starts_with("CREATE USER u IDENTIFIED BY"), "{query}");
        assert!(!query.contains("hunter2"), "{query}");
        Ok(())
    }

    #[test]
    fn id_lists() -> LiveResult<()> {
        assert_eq!(id_list(None)?, Vec::<i64>::new());
        assert_eq!(id_list(Some("".into()))?, Vec::<i64>::new());
        assert_eq!(id_list(Some("1, 2".into()))?, vec![1, 2]);
        assert!(id_list(Some("x".into())).is_err());
        Ok(())
    }

    #[test]
    fn kill_statements() -> LiveResult<()> {
        let pg = server(Engine::Postgres, 18, 0, None);
        assert_eq!(
            kill_statement(&pg, 1, 42, KillTarget::Query)?,
            "SELECT pg_catalog.pg_cancel_backend(42) AS signalled"
        );
        assert_eq!(
            kill_statement(&pg, 1, 42, KillTarget::Connection)?,
            "SELECT pg_catalog.pg_terminate_backend(42) AS signalled"
        );
        let my = server(Engine::Mariadb, 12, 3, None);
        assert_eq!(
            kill_statement(&my, 1, 42, KillTarget::Query)?,
            "KILL QUERY 42"
        );
        assert_eq!(
            kill_statement(&my, 1, 42, KillTarget::Connection)?,
            "KILL CONNECTION 42"
        );
        assert!(matches!(
            kill_statement(&my, 42, 42, KillTarget::Query),
            Err(LiveError::OwnConnection { id: 42 })
        ));
        assert!(matches!(
            kill_statement(&pg, 1, 0, KillTarget::Query),
            Err(LiveError::Invalid { .. })
        ));
        assert!(matches!(
            kill_statement(&server(Engine::Redis, 7, 2, None), 1, 2, KillTarget::Query),
            Err(LiveError::Unsupported { .. })
        ));
        Ok(())
    }

    #[test]
    fn types_serialize_camel_case() -> Result<(), serde_json::Error> {
        let outcome = KillOutcome {
            id: 3,
            target: KillTarget::Connection,
            statement: "KILL CONNECTION 3".into(),
            signalled: true,
            message: None,
        };
        let json = serde_json::to_value(&outcome)?;
        assert_eq!(json["target"], "connection");
        assert_eq!(json["signalled"], true);
        Ok(())
    }
}
