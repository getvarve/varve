//! An open MySQL or MariaDB session: execute, cancel, ping, close.

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Error as MyError, Opts, SessionStateChange};
use tokio::sync::{Mutex, Notify, OwnedRwLockReadGuard, RwLock, oneshot};
use varve_core::Engine;
use varve_driver::{
    CancelHandle, Dialect, DriverError, Durability, ErrorKind, ExecOptions, ExecOutput, LexKind,
    MysqlMode, MysqlServer, Notice, PartialOutput, RanStatement, ServerInfo, Session,
    StatementKind, StatementResult, StatementSpan, TransactionEffect, TransactionTracker, Value,
    lex_mysql, split_statements, split_statements_mysql, split_statements_strict,
    transaction_effect_mysql,
};

use crate::connect::{self, Opened};
use crate::error::{ER_NO_SUCH_THREAD, session_error};
use crate::guard;
use crate::overrides::{self, SessionOverrides};
use crate::tracking::{self, ClientCharset};
use crate::types::{self, ColumnMeta, TimestampZone};

/// An open connection to a MySQL or MariaDB server.
///
/// Calls are serialized: a second [`Session::execute`] waits until the
/// first one is done. [`Session::cancel_handle`] works while a call runs.
///
/// Scripts run as one multi-statement request over the text protocol, so
/// every result set comes back in order. The server keeps warning details
/// only for the last statement, so `SHOW WARNINGS` details are returned for
/// the last statement of a script and earlier statements report only how
/// many warnings they raised.
///
/// # Client character set
///
/// Varve talks utf8mb4 to the server in every session, and its lexer reads
/// scripts as UTF-8. The session setup ends with `SET NAMES utf8mb4`, after
/// the server's `init_connect` and the profile's `init_sql`, and checks the
/// result, so a session never runs in another character set.
///
/// Scripts that set the client character set to anything but utf8mb4 are
/// refused with [`ErrorKind::Unsupported`]: `SET NAMES`, `SET CHARACTER
/// SET`, `SET CHARSET`, and assignments to `character_set_client`,
/// `character_set_connection`, `character_set_results` or
/// `collation_connection` (also `SET STATEMENT ... FOR`). Only the literal
/// utf8mb4 (or a `utf8mb4_*` collation) is allowed; a variable as the value
/// is refused. The check reads the script every way the server might: with
/// and without backslash escapes and `ANSI_QUOTES`, and executable comments
/// both as code (after five or six version digits) and as comments. Dumps
/// first go through [`crate::normalize_dump_boilerplate`], which removes
/// their restore lines.
///
/// A stored routine that changes the character set is seen through session
/// tracking or the check after the call: a read-only session restores
/// utf8mb4 and stops the script, a normal session restores it and adds a
/// warning. Without session tracking a normal session checks the character
/// set and time zone after every call that has an executable comment, a
/// `CALL`, an `EXECUTE` or a statement the classifier does not know.
/// Replaying DDL with its original settings goes through
/// [`MysqlSession::execute_with_session`].
///
/// # Failed scripts
///
/// When a statement fails, the error carries what ran before it
/// ([`varve_driver::PartialOutput`]). In a multi-statement request the
/// server does not say which statement a result belongs to: every statement
/// returns one result, a `CALL` (or MariaDB `BEGIN NOT ATOMIC` block) its
/// result sets and then one OK packet. Results are matched to the
/// statements on that basis; when that is not certain (an `EXECUTE` that
/// returned rows, a MariaDB compound statement outside a routine, boundaries
/// that depend on `NO_BACKSLASH_ESCAPES`) the failing statement is not
/// named. Whether a transaction was open after each statement comes from
/// the status flags of the OK packet that ended its last result.
///
/// # Calls
///
/// Every call runs in a task of its own. A caller that stops waiting (its
/// future is dropped) does not cut the call short: it finishes, restores
/// what it changed, and the next call waits for it. A call whose task
/// panicked leaves the session closed.
///
/// # Read-only sessions
///
/// Read-only mode is `SET SESSION TRANSACTION READ ONLY`, enforced by the
/// server. Before a script runs, the session refuses it (with
/// [`ErrorKind::ReadOnly`], nothing run) when it contains:
///
/// - a statement that would switch read-only mode off (see
///   [`varve_driver::read_only_violation`]);
/// - dynamic SQL whose text cannot be checked: `PREPARE name FROM @var`,
///   MariaDB `EXECUTE IMMEDIATE <expression>`, or a literal source with
///   backslashes. A `PREPARE` (or `EXECUTE IMMEDIATE`) from plain string
///   literals is unescaped and checked like a script;
/// - statement boundaries that depend on `NO_BACKSLASH_ESCAPES`, and
///   executable `/*! */` comments;
/// - writes the access mode does not cover: `INTO OUTFILE`/`INTO DUMPFILE`,
///   `SET GLOBAL`/`SET PERSIST`, XA transactions, and server administration
///   such as `FLUSH`, `RESET`, `PURGE`, `KILL` or replication control;
/// - a `SET` of the `session_track_*` variables the session relies on.
///
/// The script then runs one statement per request (split with
/// [`varve_driver::split_statements_strict`]), with nothing of the driver's
/// in between, so within a call `FOUND_ROWS()`, `ROW_COUNT()`,
/// `SHOW WARNINGS`, `SHOW ERRORS` and `@@warning_count` behave as in a
/// normal session. After each statement the session reads the state changes
/// the server reported in its OK packets (session state tracking, no query):
/// when the access mode went off, a `READ WRITE` transaction started, the
/// tracking variables or the client character set changed (also inside a
/// stored function or procedure), the session rolls back, restores read-only
/// mode, tracking and utf8mb4, and stops the script before the next
/// statement with [`ErrorKind::ReadOnly`]. Each statement is sent after as
/// many newlines as precede it, so server errors name the same line as for
/// the whole script. MariaDB anonymous blocks (`BEGIN NOT ATOMIC ... END`)
/// are split at their inner semicolons and fail with a syntax error.
///
/// After every call one `SELECT` reads the tracking configuration, the access
/// mode and the client character set. When a stored routine narrowed or
/// switched off tracking (which the server then no longer reports), or
/// anything else is off, the state is restored and the call returns
/// [`ErrorKind::ReadOnly`].
///
/// Differences from a normal session, all across calls: because of that
/// `SELECT`, `FOUND_ROWS()`, `ROW_COUNT()`, `SHOW WARNINGS`, `SHOW ERRORS`,
/// `@@warning_count` and `@@error_count` at the start of the *next* call
/// describe the driver's check, not the previous script (the previous
/// script's warnings are already in its output). A cancel that interrupts
/// no statement with an error (MySQL ends a killed `SLEEP()` normally) stops
/// the script before its next statement: the call returns the results so
/// far with a note naming the last statement that ran.
///
/// When restoring fails (any of these statements returns an error), the
/// session closes its connection, and every later call returns
/// [`ErrorKind::ConnectionLost`]: a session never stays open in an unknown
/// state.
///
/// What remains, and needs a database account without write privileges: a
/// stored routine that writes in its own body after switching read-only
/// mode off (autocommitted before control returns), and one that narrows
/// tracking and then writes within the same call, are only found after the
/// call. Creating routines is refused in read-only mode.
///
/// # Time zone
///
/// The session starts in UTC, so `TIMESTAMP` values are
/// [`varve_driver::Value::TimestampTz`]. The server reports every change of
/// `time_zone` (also from a stored routine) in the OK packet of the
/// statement that made it, and result sets after it are labelled with the
/// new zone: UTC gives `TimestampTz`, anything else
/// [`varve_driver::Value::Timestamp`] without a zone.
///
/// When the server reports that tracking changed, results after that point
/// have no zone. A normal session whose script may change tracking without
/// a report (a `CALL`, `EXECUTE`, an unknown statement, or text naming the
/// `session_track_*` variables) checks tracking after the call; when it was
/// narrowed, tracking is restored, `TIMESTAMP` values of the call lose their
/// zone, and a warning is added. A stored function in a plain `SELECT` that
/// narrows tracking is not checked in a normal session; it only affects the
/// labels.
///
/// Without tracking (a server or proxy that does not report session state),
/// `TIMESTAMP` values from the first statement that may change the zone (a
/// `SET` that names `time_zone`, a `CALL`, or a statement the classifier does
/// not know) on have no zone, and the session reads `@@session.time_zone`
/// after the script.
pub struct MysqlSession {
    core: Arc<Core>,
}

/// The state of a session. Calls run in a task of their own that holds
/// it, so a caller that stops waiting (its future is dropped) never
/// leaves the connection halfway through a call: the call finishes,
/// restores what it changed, and the next call waits for it.
struct Core {
    conn: Mutex<Connection>,
    /// Whether a transaction is open, from the status flags of the last OK
    /// packet: 0 unknown, 1 no, 2 yes. Readable while a call runs.
    in_transaction: AtomicU8,
    info: ServerInfo,
    connection_id: u64,
    read_only_variable: Option<&'static str>,
    tls: bool,
    cancel: Arc<MysqlCancelHandle>,
    /// The statements the last call ran, when known exactly (see
    /// [`Session::last_statements`]).
    last: std::sync::Mutex<Option<Vec<RanStatement>>>,
}

/// The connection and what the session knows about its state.
struct Connection {
    /// `None` once the session was closed after a failed restore.
    conn: Option<Conn>,
    /// How `TIMESTAMP` values are labelled, from the session time zone.
    zone: TimestampZone,
    /// The statements that set up session state tracking, while the
    /// session can rely on it; `None` falls back to reading the time zone.
    tracking: Option<Vec<String>>,
    /// The variables the server accepted for tracking at connect.
    tracked_names: Vec<String>,
    /// The client character set Varve keeps (utf8mb4).
    charset: ClientCharset,
    /// The session's `sql_mode`, from the setup and every change the
    /// server reported since (it decides where statements end).
    sql_mode: String,
    /// Statements that put the session back in order, set while a call
    /// changes session state and cleared when it is done. A call whose
    /// future was dropped leaves them for the next call to run first.
    pending: Option<Vec<String>>,
}

/// The error of every call after the session was closed.
fn closed() -> DriverError {
    DriverError::new(
        ErrorKind::ConnectionLost,
        "this session was closed because restoring its state after a script failed; open a \
         new connection",
    )
}

/// Hand-written: it must never print the connection options (`Opts` holds
/// the decrypted TLS client key), so it names safe fields only.
impl std::fmt::Debug for MysqlSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlSession")
            .field("info", &self.core.info)
            .field("connection_id", &self.core.connection_id)
            .field("read_only", &self.core.read_only_variable.is_some())
            .field("tls", &self.core.tls)
            .finish_non_exhaustive()
    }
}

impl MysqlSession {
    pub(crate) fn new(opened: Opened, connect_timeout: Duration) -> Self {
        Self {
            core: Arc::new(Core::new(opened, connect_timeout)),
        }
    }

    /// The server's id for this connection, read with `CONNECTION_ID()`
    /// after connecting.
    #[must_use]
    pub fn connection_id(&self) -> u64 {
        self.core.connection_id
    }

    /// True when the connection runs over TLS. False for
    /// [`varve_driver::TlsMode::Prefer`] against a server without TLS.
    #[must_use]
    pub fn uses_tls(&self) -> bool {
        self.core.tls
    }

    /// True for read-only sessions.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.core.read_only_variable.is_some()
    }

    /// Runs `sql` with session settings that apply to this call only, for
    /// replaying DDL as it was created (a stored routine keeps the
    /// `character_set_client`, collation and `sql_mode` it was created
    /// with).
    ///
    /// Under the session lock: the overrides are checked (character sets and
    /// collations against the server, `sql_mode` names, the time zone),
    /// applied, the SQL runs with the same guards as [`Session::execute`],
    /// and the session's values are always restored afterwards (utf8mb4,
    /// the `sql_mode` and time zone from before the call). When restoring
    /// fails the session is closed, and every later call returns
    /// [`ErrorKind::ConnectionLost`]. Results stay utf8mb4.
    ///
    /// The SQL is UTF-8. Under a single-byte client character set (latin1
    /// and the like) the server transcodes it first, and a character the
    /// character set cannot represent is refused. Under utf8mb3 characters
    /// outside the BMP are refused. Under other multibyte character sets
    /// only ASCII SQL is accepted.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`] for an override the server does not know
    /// (`utf8` on servers that call it `utf8mb3`), for a client character
    /// set the lexer cannot read (`gbk`, `big5`, `sjis`, `cp932`,
    /// `gb18030`, `swe7`, and the UCS and UTF-16/32 sets), and for SQL the
    /// client character set cannot carry. Otherwise the errors of
    /// [`Session::execute`].
    pub async fn execute_with_session(
        &self,
        settings: &SessionOverrides,
        sql: &str,
        options: &ExecOptions,
    ) -> Result<ExecOutput, DriverError> {
        let core = Arc::clone(&self.core);
        let settings = settings.clone();
        let sql = sql.to_owned();
        let options = *options;
        joined(
            tokio::spawn(async move { core.execute_with_session(&settings, &sql, &options).await })
                .await,
        )
    }
}

/// Whether the last OK packet says a transaction is open
/// (`SERVER_STATUS_IN_TRANS`); `None` when the last reply was no OK.
fn transaction_status(conn: &Conn) -> Option<bool> {
    conn.last_ok_packet().map(|ok| {
        ok.status_flags()
            .contains(mysql_async::consts::StatusFlags::SERVER_STATUS_IN_TRANS)
    })
}

/// The stored form of a transaction status.
fn transaction_code(status: Option<bool>) -> u8 {
    match status {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }
}

/// The outcome of a call's task.
fn joined(
    outcome: Result<Result<ExecOutput, DriverError>, tokio::task::JoinError>,
) -> Result<ExecOutput, DriverError> {
    outcome.unwrap_or_else(|error| {
        Err(DriverError::new(
            ErrorKind::Protocol,
            format!("the call ended abnormally: {error}"),
        ))
    })
}

impl Core {
    fn new(opened: Opened, connect_timeout: Duration) -> Self {
        let connection_id = opened.connection_id;
        let in_transaction = AtomicU8::new(transaction_code(transaction_status(&opened.conn)));
        Self {
            in_transaction,
            cancel: Arc::new(MysqlCancelHandle {
                target: Arc::new(KillTarget {
                    opts: opened.opts,
                    connection_id,
                    connect_timeout,
                }),
                state: Arc::new(CancelState::default()),
            }),
            conn: Mutex::new(Connection {
                conn: Some(opened.conn),
                // The session setup ends with `SET time_zone = '+00:00'`.
                zone: TimestampZone::Utc,
                tracking: opened.tracking,
                tracked_names: opened.tracked_names,
                charset: opened.charset,
                sql_mode: opened.sql_mode,
                pending: None,
            }),
            info: opened.info,
            connection_id,
            read_only_variable: opened.read_only_variable,
            tls: opened.tls,
            last: std::sync::Mutex::new(None),
        }
    }

    /// How this session reads SQL text now: its `sql_mode` and server.
    fn mode(&self, connection: &Connection) -> MysqlMode {
        let server = self.info.patch.map(|patch| {
            MysqlServer::new(
                self.info.engine == Engine::Mariadb,
                self.info.major,
                self.info.minor,
                patch,
            )
        });
        MysqlMode::from_sql_mode(&connection.sql_mode, server)
    }

    /// Records the statements of the last call (`None` when not exact).
    fn set_last(&self, ran: Option<Vec<RanStatement>>) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = ran;
    }

    /// See [`MysqlSession::execute_with_session`].
    async fn execute_with_session(
        &self,
        settings: &SessionOverrides,
        sql: &str,
        options: &ExecOptions,
    ) -> Result<ExecOutput, DriverError> {
        let checked = overrides::check(settings)?;
        self.check_script(sql)?;
        let mut locked = self.conn.lock().await;
        let connection = &mut *locked;
        let result = async {
            self.recover(connection).await?;
            let Some(apply) = checked.apply_statement() else {
                let expected = connection.charset.clone();
                return self
                    .run_locked(connection, sql, None, options, &expected)
                    .await;
            };
            let conn = connection.conn.as_mut().ok_or_else(closed)?;
            let client = verify_overrides(conn, &checked).await?;
            let encoded = encode_for(conn, client.as_ref(), sql).await?;
            let before: Option<(Option<String>, Option<String>)> = conn
                .query_first("SELECT @@session.sql_mode, @@session.time_zone")
                .await
                .map_err(|error| session_error(&error))?;
            let (sql_mode, time_zone) = before.unwrap_or_default();
            let mut restore = vec![connection.charset.restore_statement()];
            if let Some(mode) = sql_mode {
                restore.push(format!("SET sql_mode = '{}'", quote(&mode)));
            }
            if let Some(zone) = time_zone {
                restore.push(format!("SET time_zone = '{}'", quote(&zone)));
            }
            // Set before anything changes, so a dropped call is put right by
            // the next one.
            let mut pending = self
                .restore_plan(connection, &connection.charset)
                .statements();
            pending.extend(restore.iter().cloned());
            connection.pending = Some(pending);

            let conn = connection.conn.as_mut().ok_or_else(closed)?;
            let outcome = match conn.query_drop(apply.as_str()).await {
                Ok(()) => {
                    // The zone and sql_mode first: the next query clears
                    // the reported changes.
                    let observed = tracking::observe(conn.session_state_changes());
                    if let Some(mode) = observed.sql_mode {
                        connection.sql_mode = mode;
                    }
                    if let Some(zone) = observed.zone {
                        connection.zone = zone;
                    } else if checked.time_zone.is_some() {
                        connection.zone = read_zone(conn).await.unwrap_or(TimestampZone::Unknown);
                    }
                    match connect::client_charset(conn).await {
                        Ok(expected) => {
                            self.run_locked(connection, sql, encoded.as_deref(), options, &expected)
                                .await
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(session_error(&error)),
            };

            // Always back to the session's own values.
            let Some(conn) = connection.conn.as_mut() else {
                return outcome;
            };
            let mut failed = None;
            for statement in &restore {
                if let Err(error) = conn.query_drop(statement.as_str()).await {
                    failed = Some(session_error(&error).message);
                    break;
                }
                if let Some(mode) = tracking::observe(conn.session_state_changes()).sql_mode {
                    connection.sql_mode = mode;
                }
            }
            if let Some(message) = failed {
                let message =
                    format!("restoring the session settings after the call failed: {message}");
                return Err(poison(connection, message).await);
            }
            if let Some(zone) = tracking::latest_zone(conn.session_state_changes()) {
                connection.zone = zone;
            } else if connection.tracking.is_none() {
                connection.zone = read_zone(conn).await.unwrap_or(TimestampZone::Unknown);
            }
            connection.pending = None;
            outcome
        }
        .await;
        self.refresh_transaction(connection).await;
        result
    }

    /// A call that did not finish (its task panicked) leaves the session in
    /// an unknown state, also on the wire: the session is closed.
    async fn recover(&self, connection: &mut Connection) -> Result<(), DriverError> {
        if connection.pending.take().is_none() {
            return Ok(());
        }
        Err(poison(
            connection,
            "the previous call on this session ended abnormally".to_owned(),
        )
        .await)
    }

    /// What restores the session state in this session.
    fn restore_plan(&self, connection: &Connection, expected: &ClientCharset) -> RestorePlan {
        RestorePlan {
            read_only: self.read_only_variable.is_some(),
            tracking: connection.tracking.clone().unwrap_or_default(),
            charset: expected.restore_statement(),
        }
    }

    /// See [`Session::execute`].
    async fn execute(&self, sql: &str, options: &ExecOptions) -> Result<ExecOutput, DriverError> {
        self.check_script(sql)?;
        let mut locked = self.conn.lock().await;
        let result = async {
            self.recover(&mut locked).await?;
            let expected = locked.charset.clone();
            self.run_locked(&mut locked, sql, None, options, &expected)
                .await
        }
        .await;
        self.refresh_transaction(&mut locked).await;
        result
    }

    /// Records whether a transaction is open, from the last reply. An
    /// error reply carries no status, so after one a `COM_PING` (no SQL, no
    /// privilege, no change to the session) brings the current flags.
    async fn refresh_transaction(&self, connection: &mut Connection) {
        let status = match connection.conn.as_mut() {
            None => None,
            Some(conn) => {
                if conn.last_ok_packet().is_none() && conn.ping().await.is_err() {
                    None
                } else {
                    transaction_status(conn)
                }
            }
        };
        self.in_transaction
            .store(transaction_code(status), Ordering::SeqCst);
    }

    /// The checks every script passes before it runs.
    fn check_script(&self, sql: &str) -> Result<(), DriverError> {
        guard::check_charset(sql, "the script")?;
        if self.read_only_variable.is_some() {
            guard::check(sql, "the script")?;
        }
        Ok(())
    }

    /// Runs a script on the locked connection; `expected` is the client
    /// character set the call must keep.
    async fn run_locked(
        &self,
        connection: &mut Connection,
        sql: &str,
        encoded: Option<&[u8]>,
        options: &ExecOptions,
        expected: &ClientCharset,
    ) -> Result<ExecOutput, DriverError> {
        let read_only = self.read_only_variable.is_some();
        let tracked = connection.tracking.is_some();
        self.set_last(None);
        let mode = self.mode(connection);
        // Scripts that may change privileges run one statement per request,
        // so the statements that ran are known exactly. That needs the
        // server to report sql_mode changes (tracking) and certain
        // boundaries; otherwise the server splits the script.
        let tracks_mode = tracked
            && connection
                .tracked_names
                .iter()
                .any(|name| name == "*" || name.eq_ignore_ascii_case("sql_mode"));
        let one_by_one = !read_only
            && tracks_mode
            && mode.server.is_some()
            && changes_privileges(sql, &mode)
            && split_statements_mysql(sql, &mode).is_ok();
        let zone_change = if tracked {
            None
        } else {
            first_zone_change(sql)
        };
        let plan = self.restore_plan(connection, expected);
        let owns_pending = connection.pending.is_none();
        if owns_pending {
            connection.pending = Some(plan.statements());
        }
        let Some(conn) = connection.conn.as_mut() else {
            return Err(closed());
        };
        let started = Instant::now();
        let state = &self.cancel.state;
        // Waits until no kill of an earlier call is in flight.
        let run = state.begin().await;
        let _end = RunEnd {
            state,
            run: Arc::clone(&run),
        };

        let mut zones = ZoneTracker {
            zone: connection.zone,
            tracked,
            blind: false,
            heuristic: ResultZones {
                session: connection.zone,
                changes_at: zone_change,
            },
            cursor: 0,
        };
        let mut read = ScriptRead {
            mode,
            ..ScriptRead::default()
        };
        let read_outcome = if read_only && tracked {
            read_statements(
                conn, sql, encoded, options, &mut zones, &run, &plan, expected, &mut read,
            )
            .await
        } else if one_by_one {
            read_one_by_one(conn, sql, encoded, options, &mut zones, &run, &mut read)
                .await
                .map_err(Failure::Error)
        } else {
            let outcome = read_results(
                conn,
                encoded.unwrap_or(sql.as_bytes()),
                options,
                &mut read,
                &mut zones,
            )
            .await
            .map_err(Failure::Error);
            zones.apply(conn.session_state_changes());
            read.changes = conn.session_state_changes().to_vec();
            read.mode_changed = !same_split_in_every_mode(sql, mode, &read.changes);
            outcome
        };
        // The transaction status right after the script, before any query
        // of the driver's own.
        let open_after_script = match &read_outcome {
            Ok(()) => transaction_status(conn),
            Err(_) => None,
        };
        // From here on no new kill starts; wait for the ones in flight so
        // none of them can hit the statements below or a later call.
        run.finish_script();
        state.wait_for_kills().await;
        // What the script reported, also across the requests of a script
        // run one statement per request.
        let all = tracking::observe(&read.changes);
        if let Some(sql_mode) = &all.sql_mode {
            connection.sql_mode.clone_from(sql_mode);
        }
        let reported = if read_only {
            tracking::Observed::default()
        } else {
            all
        };
        let stopped = read_outcome.is_ok() && read.stopped_after.is_some();
        let connection_lost = matches!(&read_outcome, Err(Failure::Error(error)) if error.kind == ErrorKind::ConnectionLost);

        // Cancelled is reported only when the server interrupted a
        // statement: it then fails with error 1317 (or the connection is
        // gone). A kill that found the script already finished on the
        // server changes nothing, so the results are complete.
        let mut outcome = match read_outcome {
            Ok(()) => finish_output(conn, &mut read).await.map_err(Failure::Error),
            Err(failure) => Err(failure),
        };
        // Results of a script that ran to its end and then failed a check:
        // kept for the error's partial output.
        let mut finished: Vec<StatementResult> = Vec::new();
        let mut zone = if zones.blind {
            TimestampZone::Unknown
        } else {
            zones.zone
        };
        let check_needed = read_only
            || may_change_session(sql)
            || reported.tracking_changed
            || reported.charset_differs(expected);
        if !connection_lost && !matches!(outcome, Err(Failure::Poison(_))) {
            if check_needed {
                let names: Vec<&str> = connection
                    .tracked_names
                    .iter()
                    .map(String::as_str)
                    .collect();
                let wanted = tracked.then_some(names.as_slice());
                match check_session(conn, self.read_only_variable, wanted, expected).await {
                    Ok(found) if found.anything() => match restore_session(conn, &plan).await {
                        Err(message) => {
                            fail(&mut outcome, &mut finished, Failure::Poison(message));
                        }
                        Ok(()) if read_only => {
                            if !matches!(&outcome, Err(Failure::Error(error)) if error.kind == ErrorKind::ReadOnly)
                            {
                                fail(&mut outcome, &mut finished, Failure::Error(found.error()));
                            }
                        }
                        Ok(()) => {
                            if found.tracking {
                                strip_zones(&mut outcome);
                                zone = read_zone(conn).await.unwrap_or(TimestampZone::Unknown);
                                add_notice(&mut outcome, TRACKING_RESTORED);
                            }
                            if found.charset {
                                add_notice(&mut outcome, CHARSET_RESTORED);
                            }
                        }
                    },
                    Ok(_) => {}
                    Err(error) => fail(&mut outcome, &mut finished, Failure::Error(error)),
                }
                // Without tracking, or after it was blinded, read the zone.
                if (zones.blind || !tracked) && !read_only {
                    zone = read_zone(conn).await.unwrap_or(TimestampZone::Unknown);
                }
            } else if zone_change.is_some() {
                match read_zone(conn).await {
                    Ok(read) => zone = read,
                    Err(error) => {
                        // The zone is not known any more.
                        zone = TimestampZone::Unknown;
                        fail(&mut outcome, &mut finished, Failure::Error(error));
                    }
                }
            }
        }
        connection.zone = zone;

        let (results, mut notices) = match outcome {
            Ok(output) => output,
            Err(failure) => {
                // What ran before the failure: the results read so far (all
                // of them when the script ended and a check failed), and
                // what became of each completed statement.
                if !finished.is_empty() {
                    read.results = finished;
                }
                let open_after = match connection.conn.as_mut() {
                    Some(conn) if !matches!(failure, Failure::Poison(_)) => {
                        status_after_failure(conn).await
                    }
                    _ => None,
                };
                let partial = partial_output(
                    &read,
                    sql,
                    open_after,
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                );
                self.set_last(read.exact_statements(open_after));
                let mut error = match failure {
                    Failure::Error(error) => {
                        if owns_pending {
                            connection.pending = None;
                        }
                        error
                    }
                    Failure::Poison(message) => poison(connection, message).await,
                };
                if error.position.is_none()
                    && !read.one_by_one
                    && error
                        .code
                        .as_deref()
                        .is_some_and(|code| code.starts_with("1064") || code.starts_with("1065"))
                    && let Some(completed) = partial.failed_statement
                {
                    error.position = empty_statement_position(sql, &mode, completed);
                }
                return Err(error.with_partial(partial));
            }
        };
        if owns_pending {
            connection.pending = None;
        }
        self.set_last(read.exact_statements(open_after_script));
        if run.requested() && !run.kill_failed() && !stopped {
            notices.push(Notice {
                severity: "Note".into(),
                message: "The cancel came too late to stop the script: no statement was \
                          interrupted with an error, so it ran to its end and these are its \
                          results. (A cancelled SLEEP() ends early without an error.)"
                    .into(),
            });
        }
        Ok(ExecOutput {
            results,
            notices,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }
}

/// Quotes a value for a `'...'` literal.
fn quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "''")
}

/// Closes the session after a failed restore; every later call returns
/// [`closed`].
async fn poison(connection: &mut Connection, message: String) -> DriverError {
    tracing::warn!(%message, "closing the session");
    if let Some(conn) = connection.conn.take() {
        connect::disconnect_quietly(conn).await;
    }
    DriverError::new(
        ErrorKind::ConnectionLost,
        format!(
            "{message}. The session was closed so it cannot run in an unknown state; open a new connection."
        ),
    )
}

/// The client character set of an override, as the server knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientSet {
    name: String,
    /// Bytes per character at most (`MAXLEN`).
    max_bytes: i64,
}

/// Checks the overrides against the server; returns the client character
/// set when one is overridden.
async fn verify_overrides(
    conn: &mut Conn,
    checked: &overrides::Checked,
) -> Result<Option<ClientSet>, DriverError> {
    let mut client = None;
    if let Some(name) = &checked.character_set_client {
        let max_bytes: Option<Option<i64>> = conn
            .query_first(format!(
                "SELECT MAXLEN FROM information_schema.CHARACTER_SETS \
                 WHERE CHARACTER_SET_NAME = '{name}'"
            ))
            .await
            .map_err(|error| session_error(&error))?;
        let Some(max_bytes) = max_bytes.flatten() else {
            let hint = if name == "utf8" {
                " (this server lists it as utf8mb3; name that instead)"
            } else {
                ""
            };
            return Err(DriverError::new(
                ErrorKind::Unsupported,
                format!("the server has no character set {name}{hint}"),
            ));
        };
        client = Some(ClientSet {
            name: name.clone(),
            max_bytes,
        });
    }
    let mut lookups = Vec::new();
    if let Some(collation) = &checked.collation_connection {
        lookups.push((
            format!(
                "SELECT COUNT(*) FROM information_schema.COLLATIONS \
                 WHERE COLLATION_NAME = '{collation}'"
            ),
            format!("the server has no collation {collation}"),
        ));
    }
    if let Some(zone) = &checked.time_zone {
        lookups.push((
            format!(
                "SELECT CONVERT_TZ('2000-01-01 00:00:00', '+00:00', '{}') IS NOT NULL",
                quote(zone)
            ),
            format!("the server does not know the time zone {zone}"),
        ));
    }
    for (query, missing) in lookups {
        let found: Option<Option<i64>> = conn
            .query_first(query)
            .await
            .map_err(|error| session_error(&error))?;
        if found.flatten().unwrap_or(0) < 1 {
            return Err(DriverError::new(ErrorKind::Unsupported, missing));
        }
    }
    Ok(client)
}

/// The bytes to send `sql` as under the client character set, when they
/// differ from its UTF-8 text.
///
/// ASCII SQL is the same in every allowed character set. Under utf8mb3 the
/// UTF-8 text is sent as is (characters outside the BMP are refused).
/// Under a single-byte character set the server transcodes the text (so
/// its own mapping is used) and the result must convert back unchanged,
/// otherwise a character cannot be represented and the SQL is refused.
/// Non-ASCII SQL under other multibyte character sets is refused.
async fn encode_for(
    conn: &mut Conn,
    client: Option<&ClientSet>,
    sql: &str,
) -> Result<Option<Vec<u8>>, DriverError> {
    let Some(client) = client else {
        return Ok(None);
    };
    let name = client.name.as_str();
    if sql.is_ascii() || name == "utf8mb4" {
        return Ok(None);
    }
    let refuse = |message: String| Err(DriverError::new(ErrorKind::Unsupported, message));
    if matches!(name, "utf8" | "utf8mb3") {
        if sql.chars().any(|c| u32::from(c) > 0xFFFF) {
            return refuse(format!(
                "the SQL contains characters outside the Basic Multilingual Plane, which the \
                 client character set {name} cannot carry"
            ));
        }
        return Ok(None);
    }
    if client.max_bytes != 1 {
        return refuse(format!(
            "the SQL contains non-ASCII text, which Varve can send only as UTF-8 or in a \
             single-byte character set; {name} is a multibyte character set"
        ));
    }
    let hex: String = sql.bytes().map(|byte| format!("{byte:02X}")).collect();
    let row: Option<(Option<String>, Option<i64>)> = conn
        .query_first(format!(
            "SELECT HEX(CONVERT(_utf8mb4 X'{hex}' USING {name})), \
             CONVERT(CONVERT(_utf8mb4 X'{hex}' USING {name}) USING utf8mb4) \
             = _utf8mb4 X'{hex}' COLLATE utf8mb4_bin"
        ))
        .await
        .map_err(|error| session_error(&error))?;
    let (encoded, round_trip) = row.unwrap_or_default();
    if round_trip != Some(1) {
        return refuse(format!(
            "the SQL contains characters the client character set {name} cannot represent"
        ));
    }
    let encoded = encoded.unwrap_or_default();
    let bytes: Option<Vec<u8>> = (0..encoded.len())
        .step_by(2)
        .map(|at| {
            encoded
                .get(at..at + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect();
    match bytes {
        Some(bytes) if bytes.len() == sql.chars().count() => Ok(Some(bytes)),
        _ => refuse(format!(
            "the server's {name} text for the SQL does not have one byte per character"
        )),
    }
}

#[async_trait::async_trait]
impl Session for MysqlSession {
    fn server_info(&self) -> &ServerInfo {
        &self.core.info
    }

    fn tls_in_use(&self) -> Option<bool> {
        Some(self.uses_tls())
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.core.cancel.clone()
    }

    async fn execute(&self, sql: &str, options: &ExecOptions) -> Result<ExecOutput, DriverError> {
        let core = Arc::clone(&self.core);
        let sql = sql.to_owned();
        let options = *options;
        joined(tokio::spawn(async move { core.execute(&sql, &options).await }).await)
    }

    async fn ping(&self) -> Result<(), DriverError> {
        let mut connection = self.core.conn.lock().await;
        self.core.cancel.state.wait_for_kills().await;
        let result = match connection.conn.as_mut() {
            Some(conn) => conn.ping().await.map_err(|error| session_error(&error)),
            None => Err(closed()),
        };
        self.core.refresh_transaction(&mut connection).await;
        result
    }

    fn last_statements(&self) -> Option<Vec<RanStatement>> {
        self.core
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn in_transaction(&self) -> Option<bool> {
        match self.core.in_transaction.load(Ordering::SeqCst) {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        }
    }

    async fn close(self: Box<Self>) -> Result<(), DriverError> {
        let mut connection = self.core.conn.lock().await;
        self.core.cancel.state.wait_for_kills().await;
        self.core.in_transaction.store(0, Ordering::SeqCst);
        match connection.conn.take() {
            Some(conn) => conn
                .disconnect()
                .await
                .map_err(|error| session_error(&error)),
            None => Ok(()),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

const TRACKING_RESTORED: &str = "The script changed session state tracking (for example \
     through a stored routine); it was switched back on. TIMESTAMP values of this call are \
     shown without a time zone.";

const CHARSET_RESTORED: &str = "The script changed the client character set (for example \
     through a stored routine); it was set back to utf8mb4. Text in results after that change \
     may be garbled.";

/// Adds a warning to a successful outcome, or to the error's detail.
fn add_notice(outcome: &mut Result<ScriptOutput, Failure>, message: &str) {
    match outcome {
        Ok((_, notices)) => notices.push(Notice {
            severity: "Warning".into(),
            message: message.into(),
        }),
        Err(Failure::Error(error)) => {
            error.detail = Some(match error.detail.take() {
                Some(detail) => format!("{detail}\n{message}"),
                None => message.into(),
            });
        }
        Err(Failure::Poison(_)) => {}
    }
}

/// `TIMESTAMP` values of a call whose zone is not known: no zone.
fn strip_zones(outcome: &mut Result<ScriptOutput, Failure>) {
    let Ok((results, _)) = outcome else {
        return;
    };
    for result in results {
        if let StatementResult::Rows { rows, .. } = result {
            for value in rows.iter_mut().flatten() {
                if let Value::TimestampTz(text) = value {
                    *value = Value::Timestamp(text.trim_end_matches('Z').to_owned());
                }
            }
        }
    }
}

/// How a call failed.
#[derive(Debug)]
enum Failure {
    /// An error for the caller; the session stays usable.
    Error(DriverError),
    /// Restoring the session state failed: the session must be closed.
    Poison(String),
}

/// True when a normal session's script may change session state the
/// driver relies on (tracking, the client character set) without the
/// server reporting it: routines (`CALL`, `EXECUTE`), statements the
/// classifier does not know, and text that names the tracking variables.
fn may_change_session(sql: &str) -> bool {
    sql.to_ascii_lowercase().contains("session_track")
        || sql.contains("/*!")
        || sql.contains("/*M!")
        || split_statements(sql, Dialect::Mysql)
            .iter()
            .any(|statement| matches!(statement.kind, StatementKind::Call | StatementKind::Other))
}

/// True when a statement may change the session time zone.
fn may_change_zone(kind: StatementKind, text: &str) -> bool {
    match kind {
        StatementKind::Session => text.to_ascii_lowercase().contains("time_zone"),
        // Procedures and prepared statements can run `SET time_zone`, and
        // unknown statements are not trusted.
        StatementKind::Call | StatementKind::Other => true,
        _ => false,
    }
}

/// Index of the first statement that may change the session time zone.
///
/// Every statement before it is a plain statement that returns exactly one
/// result (only `CALL` returns more, and `CALL` counts as a change), so the
/// index is also the index of the first result it affects.
fn first_zone_change(sql: &str) -> Option<usize> {
    split_statements(sql, Dialect::Mysql)
        .iter()
        .position(|statement| may_change_zone(statement.kind, statement.text))
}

/// How the results of one script label `TIMESTAMP` values.
#[derive(Debug, Clone, Copy)]
struct ResultZones {
    /// The zone when the script started.
    session: TimestampZone,
    /// Results from this index on may come from another zone.
    changes_at: Option<usize>,
}

impl ResultZones {
    fn for_result(self, index: usize) -> TimestampZone {
        match self.changes_at {
            Some(change) if index >= change => TimestampZone::Unknown,
            _ => self.session,
        }
    }
}

/// True when a session time zone is UTC.
///
/// Offsets (`+00:00`) and the tz database names of UTC count. `SYSTEM`
/// does not, because the server's zone is not known here.
pub(crate) fn is_utc_zone(zone: &str) -> bool {
    const UTC_NAMES: [&str; 16] = [
        "utc",
        "etc/utc",
        "uct",
        "etc/uct",
        "universal",
        "etc/universal",
        "zulu",
        "etc/zulu",
        "gmt",
        "etc/gmt",
        "gmt0",
        "etc/gmt0",
        "etc/gmt+0",
        "etc/gmt-0",
        "greenwich",
        "etc/greenwich",
    ];
    let zone = zone.trim();
    if UTC_NAMES.contains(&zone.to_ascii_lowercase().as_str()) {
        return true;
    }
    let Some(offset) = zone.strip_prefix(['+', '-']) else {
        return false;
    };
    let mut parts = offset.split(':');
    let (Some(hours), Some(minutes), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let zero =
        |part: &str| !part.is_empty() && part.len() <= 2 && part.bytes().all(|byte| byte == b'0');
    zero(hours) && zero(minutes)
}

/// Reads the session time zone (sessions without tracking).
async fn read_zone(conn: &mut Conn) -> Result<TimestampZone, DriverError> {
    let zone: Option<Option<String>> = conn
        .query_first("SELECT @@session.time_zone")
        .await
        .map_err(|error| session_error(&error))?;
    Ok(match zone.flatten().as_deref() {
        Some(zone) if is_utc_zone(zone) => TimestampZone::Utc,
        _ => TimestampZone::Unknown,
    })
}

/// The labelling of `TIMESTAMP` values while a script runs.
#[derive(Debug)]
struct ZoneTracker {
    /// The zone as last reported (tracked sessions).
    zone: TimestampZone,
    /// The server reports time zone changes.
    tracked: bool,
    /// Tracking was changed during the script: later results have no zone.
    blind: bool,
    /// For sessions without tracking: the zone from the script's text.
    heuristic: ResultZones,
    /// How many reported changes of the current request were applied.
    cursor: usize,
}

impl ZoneTracker {
    /// A new request: its list of reported changes starts empty.
    fn new_request(&mut self) {
        self.cursor = 0;
    }

    /// Applies the changes reported since the last call.
    fn apply(&mut self, changes: &[SessionStateChange<'static>]) {
        if self.tracked
            && let Some(new) = changes.get(self.cursor..)
        {
            let observed = tracking::observe(new);
            if let Some(zone) = observed.zone {
                self.zone = zone;
            }
            // Tracking may no longer report the zone.
            self.blind |= observed.tracking_changed;
        }
        self.cursor = changes.len();
    }

    fn for_result(&self, index: usize) -> TimestampZone {
        if self.blind {
            TimestampZone::Unknown
        } else if self.tracked {
            self.zone
        } else {
            self.heuristic.for_result(index)
        }
    }
}

/// What to run to restore the session state.
#[derive(Debug)]
struct RestorePlan {
    read_only: bool,
    tracking: Vec<String>,
    charset: String,
}

/// Restores the session state: in read-only sessions it rolls back any open
/// transaction (it may be a `READ WRITE` one) and switches read-only mode
/// back on; in every session it sets up tracking and the client character
/// set again. Returns the failure message when a statement fails.
impl RestorePlan {
    /// The statements, in order.
    fn statements(&self) -> Vec<String> {
        let mut statements: Vec<String> = Vec::new();
        if self.read_only {
            statements.extend(
                ["ROLLBACK", connect::READ_ONLY, "SET TRANSACTION READ ONLY"].map(str::to_owned),
            );
        }
        statements.extend(self.tracking.iter().cloned());
        statements.push(self.charset.clone());
        statements
    }
}

async fn restore_session(conn: &mut Conn, plan: &RestorePlan) -> Result<(), String> {
    for statement in plan.statements() {
        if let Err(error) = conn.query_drop(statement.as_str()).await {
            return Err(format!(
                "restoring the session state failed ({statement}: {})",
                session_error(&error).message
            ));
        }
    }
    Ok(())
}

/// What [`check_session`] found wrong.
#[derive(Debug, Default)]
struct Found {
    tracking: bool,
    read_only_off: bool,
    charset: bool,
}

impl Found {
    fn anything(&self) -> bool {
        self.tracking || self.read_only_off || self.charset
    }

    /// The error of a read-only call after which this was found.
    fn error(&self) -> DriverError {
        let mut what = Vec::new();
        if self.tracking {
            what.push("session state tracking was narrowed or switched off");
        }
        if self.read_only_off {
            what.push("read-only mode was off");
        }
        if self.charset {
            what.push("the client character set was changed");
        }
        DriverError::new(
            ErrorKind::ReadOnly,
            format!(
                "After the script, {} (through a stored routine, which the session could not \
                 watch). The session state was restored and any open transaction rolled back; \
                 statements of this call after that change may have run unobserved.",
                what.join(" and ")
            ),
        )
    }
}

/// After a call: reads the tracking configuration, the access mode and the
/// client character set in one statement. `wanted` are the tracked
/// variables (`None` when the session has no tracking).
async fn check_session(
    conn: &mut Conn,
    read_only_variable: Option<&str>,
    wanted: Option<&[&str]>,
    expected: &ClientCharset,
) -> Result<Found, DriverError> {
    type Row = (
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let mode = read_only_variable.map_or_else(|| "NULL".to_owned(), |v| format!("@@session.{v}"));
    let row: Option<Row> = conn
        .query_first(format!(
            "SELECT @@session.session_track_system_variables, \
             @@session.session_track_transaction_info, {mode}, \
             @@session.character_set_client, @@session.character_set_connection, \
             @@session.character_set_results, @@session.collation_connection"
        ))
        .await
        .map_err(|error| session_error(&error))?;
    let (variables, transaction, read_only, client, connection, results, collation) =
        row.unwrap_or_default();
    let mut found = Found::default();
    if let Some(wanted) = wanted {
        found.tracking = tracking::tracking_list(&variables.unwrap_or_default(), wanted).is_some();
        if read_only_variable.is_some() {
            found.tracking |= !transaction
                .unwrap_or_default()
                .eq_ignore_ascii_case("CHARACTERISTICS");
        }
    }
    found.read_only_off = read_only_variable.is_some() && read_only != Some(1);
    let actual = ClientCharset {
        client: client.unwrap_or_default(),
        connection: connection.unwrap_or_default(),
        results: results.unwrap_or_default(),
        collation: collation.unwrap_or_default(),
    };
    found.charset = !actual.same_as(expected);
    Ok(found)
}

/// The error for a statement after which the session left read-only mode.
fn left_read_only_error(statement: usize, what: &str) -> DriverError {
    DriverError::new(
        ErrorKind::ReadOnly,
        format!(
            "Statement {statement} {what}. Read-only mode was switched back on, any open \
             transaction was rolled back, and the rest of the script was not run."
        ),
    )
}

/// How often a cancelled script that keeps running is killed again.
const KILL_REPEAT_INTERVAL: Duration = Duration::from_millis(200);

type ScriptOutput = (Vec<StatementResult>, Vec<Notice>);

/// The results of a script before its warnings are read.
///
/// A normal session runs a script as one multi-statement request and reads
/// every result set in order. Nothing of the driver's own runs between the
/// user's statements, so `FOUND_ROWS()`, `ROW_COUNT()`, `SHOW WARNINGS` and
/// `@@warning_count` behave as in any client. A cancel does not stop the
/// reading: killed statements end with an error, and when nothing was
/// interrupted (the cancel came too late) the results are complete.
#[derive(Debug, Default)]
struct ScriptRead {
    results: Vec<StatementResult>,
    /// Warning counts of each result, where the protocol reports them.
    warning_counts: Vec<u16>,
    /// Whether a transaction was open after each result, from the status
    /// flags of the OK packet that ended it (`None` when none was seen).
    result_open: Vec<Option<bool>>,
    /// Scripts run one statement per request: whether a transaction was
    /// open after each statement that completed.
    statements: Option<Vec<Option<bool>>>,
    /// A script run one statement per request stopped by a cancel after
    /// this many statements.
    stopped_after: Option<usize>,
    /// The session state changes the server reported during the script.
    changes: Vec<SessionStateChange<'static>>,
    /// How the session read SQL text when the script started.
    mode: MysqlMode,
    /// The `sql_mode` changed during a multi-statement request so that the
    /// script splits differently: where its later statements end is not
    /// known from the text.
    mode_changed: bool,
    /// A normal session's script run one statement per request: the
    /// statements that completed, in order.
    ran: Vec<Ran>,
    /// The statements in `ran` are exactly the server's statements: every
    /// request gave the results its statement gives.
    exact: bool,
    /// The script ran one statement per request in a normal session.
    one_by_one: bool,
}

/// A statement of a normal session's script that completed as a request of
/// its own.
#[derive(Debug, Clone)]
struct Ran {
    offset: usize,
    len: usize,
    kind: StatementKind,
    effect: TransactionEffect,
}

impl ScriptRead {
    /// The exact statements of a script run one statement per request, with
    /// what became of them; `open_after` is the transaction status when the
    /// call returned. `None` unless every request matched its statement.
    fn exact_statements(&self, open_after: Option<bool>) -> Option<Vec<RanStatement>> {
        let statuses = self.statements.as_ref()?;
        if !self.exact || statuses.len() != self.ran.len() {
            return None;
        }
        let mut tracker = TransactionTracker::new();
        for (ran, open) in self.ran.iter().zip(statuses) {
            tracker.completed(ran.effect.clone(), *open);
        }
        let durabilities = tracker.finish(open_after, Durability::Unknown);
        Some(
            self.ran
                .iter()
                .zip(durabilities)
                .map(|(ran, durability)| RanStatement {
                    offset: ran.offset,
                    len: ran.len,
                    kind: ran.kind,
                    durability,
                })
                .collect(),
        )
    }
}

/// True for statement kinds that never return a result set: their result
/// must be an OK packet.
fn returns_no_rows(kind: StatementKind) -> bool {
    matches!(
        kind,
        StatementKind::Insert
            | StatementKind::Update
            | StatementKind::UpdateAll
            | StatementKind::Delete
            | StatementKind::DeleteAll
            | StatementKind::Merge
            | StatementKind::Ddl
            | StatementKind::AlterDrop
            | StatementKind::Drop
            | StatementKind::Truncate
            | StatementKind::Grant
            | StatementKind::Transaction
            | StatementKind::Session
            | StatementKind::Copy
    )
}

/// True when `results` are what a statement of `kind` with `shape` gives.
fn results_fit(kind: StatementKind, shape: Shape, results: &[StatementResult]) -> bool {
    let is_rows = |r: &StatementResult| matches!(r, StatementResult::Rows { .. });
    let procedure_like = |results: &[StatementResult]| match results.split_last() {
        Some((last, rows)) => !is_rows(last) && rows.iter().all(is_rows),
        None => false,
    };
    match shape {
        Shape::One => matches!(results, [only] if !(returns_no_rows(kind) && is_rows(only))),
        Shape::Procedure => procedure_like(results),
        Shape::Execute => results.len() == 1 || procedure_like(results),
        Shape::Compound => false,
    }
}

/// True when a script may change privileges: it has a `GRANT`, `REVOKE`,
/// `SET PASSWORD`, `DEFAULT ROLE` (`SET DEFAULT ROLE`) or a
/// `CREATE [OR REPLACE]`/`ALTER`/`DROP`/`RENAME` of a `USER` or `ROLE` anywhere outside literals and comments (also inside routine
/// bodies and executable comments, which only costs a request per
/// statement).
fn changes_privileges(sql: &str, mode: &MysqlMode) -> bool {
    let words: Vec<String> = lex_mysql(sql, mode)
        .into_iter()
        .filter(|l| l.kind == LexKind::Word)
        .map(|l| {
            sql.get(l.start..l.end)
                .unwrap_or_default()
                .to_ascii_uppercase()
        })
        .collect();
    words.iter().any(|w| w == "GRANT" || w == "REVOKE")
        || words.windows(2).any(|pair| {
            (matches!(pair[0].as_str(), "CREATE" | "ALTER" | "DROP" | "RENAME")
                && matches!(pair[1].as_str(), "USER" | "ROLE"))
                || (pair[0] == "SET" && pair[1] == "PASSWORD")
                || (pair[0] == "DEFAULT" && pair[1] == "ROLE")
        })
        || words.windows(4).any(|w| {
            w[0] == "CREATE"
                && w[1] == "OR"
                && w[2] == "REPLACE"
                && matches!(w[3].as_str(), "USER" | "ROLE")
        })
}

/// The bytes of a part of `sql` starting at byte `start`: as UTF-8, or the
/// transcoded bytes of a single-byte client character set (one byte per
/// character).
fn bytes_of(sql: &str, encoded: Option<&[u8]>, start: usize, text: &str) -> Vec<u8> {
    match encoded {
        Some(encoded) => {
            let from = sql[..start].chars().count();
            let to = from + text.chars().count();
            encoded.get(from..to).unwrap_or_default().to_vec()
        }
        None => text.as_bytes().to_vec(),
    }
}

/// One request for the statement at byte `offset`: as many newlines as
/// precede it in the script (so server errors name the same line), then
/// the statement.
fn request_for(sql: &str, encoded: Option<&[u8]>, offset: usize, text: &str) -> Vec<u8> {
    let line = sql[..offset].matches('\n').count();
    let mut request = vec![b'\n'; line];
    request.extend(bytes_of(sql, encoded, offset, text));
    request
}

/// `(offset, length, kind)` of the statements of `sql[from..]` read in
/// `mode`, with offsets into `sql`.
fn statements_from(
    sql: &str,
    from: usize,
    mode: &MysqlMode,
) -> Result<Vec<(usize, usize, StatementKind)>, varve_driver::AmbiguousSplit> {
    Ok(split_statements_mysql(&sql[from..], mode)?
        .into_iter()
        .map(|s| (from + s.offset, s.text.len(), s.kind))
        .collect())
}

/// A statement of a [`Piece::Group`]: its text at `offset` (`len` bytes),
/// and `end`, just after its `;` (or after its text when it has none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stmt {
    offset: usize,
    len: usize,
    end: usize,
    kind: StatementKind,
    /// It defines a stored body (routine, trigger, event).
    definition: bool,
}

/// What a script run one statement per request sends, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// Statements sent in one request: `sql[statements[0].offset..end]`.
    ///
    /// Normally one statement, up to and including its `;`, so the server
    /// reads exactly what it reads in the whole script. A definition goes
    /// together with the statement after it: the server keeps the `;` of a
    /// single-statement body only when more text follows, and the next
    /// statement then sees the definition's `ROW_COUNT()`, warnings and the
    /// rest exactly as in the whole script. The last statement goes with
    /// the rest of the script (comments, a trailing `;`, an empty
    /// statement), which the server answers as in the whole script (a
    /// comment with OK, an empty statement with an error).
    Group { statements: Vec<Stmt>, end: usize },
    /// From `at` on the server meets an empty statement (`;;`, a leading
    /// `;`, a comment followed by `;`) before a statement, or text without
    /// statements: the rest goes as it is, in one request, so the server
    /// answers as it does to the whole script (it rejects an empty
    /// statement followed by a statement and runs nothing after it).
    /// `statements`: the rest holds statements, which then ran (or not)
    /// as the server split them.
    Rest { at: usize, statements: bool },
}

/// The pieces of `sql[from..]` read in `mode` (see [`Piece`]).
fn pieces(
    sql: &str,
    from: usize,
    mode: &MysqlMode,
) -> Result<Vec<Piece>, varve_driver::AmbiguousSplit> {
    let statements = statements_from(sql, from, mode)?;
    let semicolons: Vec<usize> = lex_mysql(&sql[from..], mode)
        .into_iter()
        .filter(|l| l.kind == LexKind::Punct(';'))
        .map(|l| from + l.start)
        .collect();
    let semicolon_in = |start: usize, end: usize| {
        semicolons
            .iter()
            .copied()
            .find(|&at| at >= start && at < end)
    };
    let mut stmts = Vec::new();
    let mut cursor = from;
    let mut empty_before_statement = false;
    for (index, &(offset, len, kind)) in statements.iter().enumerate() {
        if semicolon_in(cursor, offset).is_some() {
            empty_before_statement = true;
            break;
        }
        let next = statements.get(index + 1).map_or(sql.len(), |s| s.0);
        let end = semicolon_in(offset + len, next).map_or(offset + len, |at| at + 1);
        stmts.push(Stmt {
            offset,
            len,
            end,
            kind,
            definition: varve_driver::mysql_defines_body(&sql[offset..offset + len], mode),
        });
        cursor = end;
    }
    let mut out = Vec::new();
    let mut index = 0;
    while index < stmts.len() {
        let mut group = vec![stmts[index]];
        index += 1;
        while group.last().is_some_and(|s| s.definition) && index < stmts.len() {
            group.push(stmts[index]);
            index += 1;
        }
        let own_end = group.last().map_or(cursor, |s| s.end);
        // The last group takes the rest of the script with it.
        let end = if index == stmts.len() && !sql[own_end..].trim().is_empty() {
            sql.len()
        } else {
            own_end
        };
        out.push(Piece::Group {
            statements: group,
            end,
        });
    }
    if stmts.is_empty() && !sql[from..].trim().is_empty() {
        out.push(Piece::Rest {
            at: from,
            statements: empty_before_statement,
        });
    }
    Ok(out)
}

/// The statements of a group whose results `results` (with the transaction
/// status after each, `open`) are, in order: each with the status after
/// its last result. Stops at the statement whose results ran out (it
/// failed when the request failed). The flag is false when the results do
/// not certainly belong to the statements: a statement got results it does
/// not give, an EXECUTE returned rows, a compound statement, results left
/// over that are not the OK of a trailing comment, or a statement without
/// results although the request did not fail.
fn assign(
    sql: &str,
    statements: &[Stmt],
    results: &[StatementResult],
    open: &[Option<bool>],
    failed: bool,
    trailing: bool,
) -> (Vec<(Stmt, Option<bool>)>, bool) {
    let is_rows = |j: usize| matches!(results.get(j), Some(StatementResult::Rows { .. }));
    let mut done = Vec::new();
    let mut next = 0;
    for stmt in statements {
        let text = &sql[stmt.offset..stmt.offset + stmt.len];
        let shape = shape(text);
        let start = next;
        match shape {
            Shape::One => {
                if next >= results.len() {
                    break;
                }
                next += 1;
            }
            Shape::Procedure => {
                while is_rows(next) {
                    next += 1;
                }
                if next >= results.len() {
                    break;
                }
                next += 1;
            }
            Shape::Execute => {
                if next >= results.len() {
                    break;
                }
                if is_rows(next) {
                    return (done, false);
                }
                next += 1;
            }
            Shape::Compound => return (done, false),
        }
        let produced = results.get(start..next).unwrap_or_default();
        if !results_fit(stmt.kind, shape, produced) {
            return (done, false);
        }
        done.push((*stmt, open.get(next - 1).copied().flatten()));
    }
    let leftover = results.get(next..).unwrap_or_default();
    let fits = if done.len() == statements.len() {
        leftover.is_empty()
            || (trailing
                && leftover
                    .iter()
                    .all(|r| matches!(r, StatementResult::Command { .. })))
    } else {
        failed && leftover.is_empty()
    };
    (done, fits)
}

/// Runs a normal session's script one statement per request (definitions
/// with the statement after them, see [`Piece`]). The server then runs
/// exactly the statements Varve split (a part of a statement fails as a
/// syntax error instead of running as something else), and the transaction
/// status after each statement is known. Scripts that may change privileges
/// run this way, so the activity log names exactly the grants that ran.
/// Statements are split with the session's `sql_mode`, again after a
/// statement changed it. Nothing of the driver's own runs in between.
#[allow(clippy::too_many_arguments)]
async fn read_one_by_one(
    conn: &mut Conn,
    sql: &str,
    encoded: Option<&[u8]>,
    options: &ExecOptions,
    zones: &mut ZoneTracker,
    run: &Run,
    read: &mut ScriptRead,
) -> Result<(), DriverError> {
    let mut mode = read.mode;
    let not_certain = || {
        DriverError::new(
            ErrorKind::Unsupported,
            "where the statements of this script end is not certain, so it was not run",
        )
    };
    let mut pieces = pieces(sql, 0, &mode).map_err(|_| not_certain())?;
    read.statements = Some(Vec::new());
    read.exact = true;
    read.one_by_one = true;
    let mut next = 0;
    while let Some(piece) = pieces.get(next).cloned() {
        if next > 0 && run.requested() {
            // A cancel that interrupted nothing: the statements so far ran,
            // the rest does not.
            read.stopped_after = Some(read.ran.len());
            return Ok(());
        }
        let (statements, end) = match piece {
            Piece::Group { statements, end } => (statements, end),
            Piece::Rest { at, statements } => {
                // The server rejects the empty statement at the start of
                // the rest (and runs nothing), as in the whole script.
                let result = read_results(
                    conn,
                    &request_for(sql, encoded, at, &sql[at..]),
                    options,
                    read,
                    zones,
                )
                .await;
                zones.apply(conn.session_state_changes());
                read.changes
                    .extend(conn.session_state_changes().iter().cloned());
                result?;
                // It ran statements of its own split: they are not known.
                if statements {
                    read.exact = false;
                }
                return Ok(());
            }
        };
        let Some(first) = statements.first().copied() else {
            next += 1;
            continue;
        };
        let last_end = statements.last().map_or(end, |s| s.end);
        let before = read.results.len();
        let result = read_results(
            conn,
            &request_for(sql, encoded, first.offset, &sql[first.offset..end]),
            options,
            read,
            zones,
        )
        .await;
        zones.apply(conn.session_state_changes());
        let changes = conn.session_state_changes().to_vec();
        read.changes.extend(changes.iter().cloned());
        let (done, fits) = assign(
            sql,
            &statements,
            read.results.get(before..).unwrap_or_default(),
            read.result_open.get(before..).unwrap_or_default(),
            result.is_err(),
            end > last_end,
        );
        if !fits {
            read.exact = false;
        }
        for (stmt, open) in done {
            if let Some(completed) = read.statements.as_mut() {
                completed.push(open);
            }
            read.ran.push(Ran {
                offset: stmt.offset,
                len: stmt.len,
                kind: stmt.kind,
                effect: transaction_effect_mysql(&sql[stmt.offset..stmt.offset + stmt.len], &mode),
            });
        }
        result?;
        next += 1;
        if let Some(sql_mode) = tracking::observe(&changes).sql_mode {
            let changed = MysqlMode::from_sql_mode(&sql_mode, mode.server);
            if changed != mode {
                // Where the rest ends depends on the new mode.
                mode = changed;
                let done = read.ran.len();
                let rest = self::pieces(sql, end, &mode).map_err(|_| {
                    DriverError::new(
                        ErrorKind::Unsupported,
                        format!(
                            "statement {done} changed sql_mode, and where the statements after \
                             it end is not certain under the new mode; they were not run"
                        ),
                    )
                })?;
                pieces.truncate(next);
                pieces.extend(rest);
            }
        }
    }
    Ok(())
}

/// Whether status flags say a transaction is open.
fn in_trans(flags: mysql_async::consts::StatusFlags) -> bool {
    flags.contains(mysql_async::consts::StatusFlags::SERVER_STATUS_IN_TRANS)
}

/// Replaces a successful outcome with `failure`, keeping its results in
/// `finished` for the error's partial output.
fn fail(
    outcome: &mut Result<ScriptOutput, Failure>,
    finished: &mut Vec<StatementResult>,
    failure: Failure,
) {
    if let Ok((results, _)) = std::mem::replace(outcome, Err(failure)) {
        *finished = results;
    }
}

/// Whether a transaction is open after a failed call. An error reply
/// carries no status, so after one a `COM_PING` brings the current flags
/// (see [`Core::refresh_transaction`]).
async fn status_after_failure(conn: &mut Conn) -> Option<bool> {
    if conn.last_ok_packet().is_none() && conn.ping().await.is_err() {
        return None;
    }
    transaction_status(conn)
}

/// How many results a statement returns, as far as its text tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Exactly one: an OK packet or a result set.
    One,
    /// A procedure call: any number of result sets, then one OK packet.
    Procedure,
    /// `EXECUTE` (and MariaDB `EXECUTE IMMEDIATE`): one result, or a
    /// procedure's results when the prepared statement is a `CALL`.
    Execute,
    /// A MariaDB compound statement outside a routine (`IF`, `WHILE`,
    /// `LOOP`, `REPEAT`, `FOR`, `CASE`, a labelled block): the server runs
    /// it as one statement, the splitter cuts it at its inner semicolons.
    Compound,
}

fn shape(statement: &str) -> Shape {
    let words: Vec<String> = varve_driver::lex(statement, Dialect::Mysql, true)
        .into_iter()
        .filter(|l| {
            !matches!(
                l.kind,
                LexKind::Comment | LexKind::ExecutableCommentStart | LexKind::ExecutableCommentEnd
            )
        })
        .take(3)
        .map(|l| {
            if l.kind == LexKind::Punct(':') {
                ":".to_owned()
            } else {
                statement
                    .get(l.start..l.end)
                    .unwrap_or_default()
                    .to_ascii_uppercase()
            }
        })
        .collect();
    let word = |i: usize| words.get(i).map(String::as_str);
    match (word(0), word(1), word(2)) {
        (_, Some(":"), _) | (Some("BEGIN"), Some("NOT"), Some("ATOMIC")) => Shape::Compound,
        (Some("CALL"), _, _) => Shape::Procedure,
        (Some("EXECUTE"), _, _) => Shape::Execute,
        (Some("IF" | "WHILE" | "LOOP" | "REPEAT" | "FOR" | "CASE" | "END"), _, _) => {
            Shape::Compound
        }
        _ => Shape::One,
    }
}

/// Which statements of a multi-statement request completed, and whether a
/// transaction was open after each, from the results the server sent
/// (`open[i]` belongs to `results[i]`). `None` when the results cannot be
/// attributed to statements with certainty.
fn attribute(
    statements: &[StatementSpan<'_>],
    results: &[StatementResult],
    open: &[Option<bool>],
) -> Option<Vec<Option<bool>>> {
    let shapes: Vec<Shape> = statements.iter().map(|s| shape(s.text)).collect();
    // Compound statements outside routines: how many results they return
    // is not relied on.
    if shapes.contains(&Shape::Compound) {
        return None;
    }
    let is_rows = |j: usize| matches!(results.get(j), Some(StatementResult::Rows { .. }));
    let mut completed = Vec::new();
    let mut next = 0;
    for (shape, statement) in shapes.into_iter().zip(statements) {
        if next >= results.len() {
            break;
        }
        match shape {
            // A statement that never returns rows got a result set: the
            // results do not belong to the statements Varve split.
            Shape::One if returns_no_rows(statement.kind) && is_rows(next) => return None,
            Shape::One | Shape::Compound => {}
            Shape::Procedure => {
                while is_rows(next) {
                    next += 1;
                }
                if next >= results.len() {
                    // The failing statement: a procedure that sent result
                    // sets and then failed.
                    return Some(completed);
                }
            }
            // A result set may be the prepared statement's own (then the
            // statement ended) or a procedure's (then more follow).
            Shape::Execute if is_rows(next) => return None,
            Shape::Execute => {}
        }
        completed.push(open.get(next).copied().flatten());
        next += 1;
    }
    // Results left over that no statement accounts for: the split does not
    // match the server's.
    (next == results.len()).then_some(completed)
}

/// True when `sql` splits the same way in `mode` and in every `sql_mode`
/// the server reported in `changes` (a script whose statements changed the
/// mode is split the same whichever applied).
fn same_split_in_every_mode(
    sql: &str,
    mode: MysqlMode,
    changes: &[SessionStateChange<'static>],
) -> bool {
    let bounds = |mode: &MysqlMode| {
        split_statements_mysql(sql, mode).ok().map(|spans| {
            spans
                .iter()
                .map(|s| (s.offset, s.text.len()))
                .collect::<Vec<_>>()
        })
    };
    let reference = bounds(&mode);
    reference.is_some()
        && changes
            .iter()
            .filter_map(|change| tracking::observe(std::slice::from_ref(change)).sql_mode)
            .map(|reported| MysqlMode::from_sql_mode(&reported, mode.server))
            .all(|other| other == mode || bounds(&other) == reference)
}

/// What a failed script produced, and what became of the statements that
/// completed. `open_after` is the transaction status after the failure.
/// Statements are named only when the results certainly belong to them.
fn partial_output(
    read: &ScriptRead,
    sql: &str,
    open_after: Option<bool>,
    elapsed_ms: u64,
) -> PartialOutput {
    let mode = read.mode;
    let effects_of = |statements: &[StatementSpan<'_>]| -> Vec<TransactionEffect> {
        statements
            .iter()
            .map(|s| transaction_effect_mysql(s.text, &mode))
            .collect()
    };
    let reported: Option<(Vec<TransactionEffect>, Vec<Option<bool>>)> = if read.one_by_one {
        // One request per statement: exact unless a request gave results
        // its statement does not give.
        read.statements
            .clone()
            .filter(|_| read.exact)
            .map(|statuses| {
                (
                    read.ran.iter().map(|r| r.effect.clone()).collect(),
                    statuses,
                )
            })
    } else if let Some(statuses) = &read.statements {
        // A read-only session: one request per statement of the strict split.
        split_statements_strict(sql, Dialect::Mysql)
            .ok()
            .map(|statements| (effects_of(&statements), statuses.clone()))
    } else if read.mode_changed {
        None
    } else {
        // One multi-statement request: the server's results matched with
        // the split of the session's sql_mode.
        split_statements_mysql(sql, &mode)
            .ok()
            .and_then(|statements| {
                attribute(&statements, &read.results, &read.result_open)
                    .map(|statuses| (effects_of(&statements), statuses))
            })
    };
    let (failed_statement, completed) = match reported {
        Some((effects, statuses)) if statuses.len() <= effects.len() => {
            let mut tracker = TransactionTracker::new();
            for (effect, open) in effects.into_iter().zip(&statuses) {
                tracker.completed(effect, *open);
            }
            // A transaction that ended during the failed statement may have
            // been rolled back (a deadlock) or committed (a failing DDL
            // statement commits first).
            (
                u32::try_from(statuses.len()).ok(),
                tracker.finish(open_after, Durability::Unknown),
            )
        }
        _ => (None, Vec::new()),
    };
    PartialOutput {
        results: read.results.clone(),
        notices: warning_notes(&read.warning_counts),
        elapsed_ms,
        failed_statement,
        completed,
    }
}

/// Where the empty statement the server rejected is (MySQL and MariaDB
/// refuse `;;` in a multi-statement request): the 1-based character
/// position of its `;` between statement `completed - 1` and statement
/// `completed`, if there is one.
fn empty_statement_position(sql: &str, mode: &MysqlMode, completed: u32) -> Option<u32> {
    let statements = split_statements_mysql(sql, mode).ok()?;
    let completed = usize::try_from(completed).ok()?;
    let from = match completed.checked_sub(1) {
        Some(last) => statements.get(last).map(|s| s.offset + s.text.len())?,
        None => 0,
    };
    let to = statements.get(completed).map_or(sql.len(), |s| s.offset);
    let gap = sql.get(from..to)?;
    // After a statement, its own `;` comes first; another one is empty.
    let skip = usize::from(completed > 0);
    let semicolon = lex_mysql(gap, mode)
        .into_iter()
        .filter(|l| l.kind == LexKind::Punct(';'))
        .nth(skip)?;
    u32::try_from(sql[..from + semicolon.start].chars().count() + 1).ok()
}

/// Notes for results that raised warnings: the server keeps details only
/// for the last statement of a script.
fn warning_notes(counts: &[u16]) -> Vec<Notice> {
    counts
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(index, count)| Notice {
            severity: "Warning".into(),
            message: format!(
                "Statement {} raised {count} warning(s). The server keeps details only for \
                 the last statement of a script.",
                index + 1
            ),
        })
        .collect()
}

/// Runs a script in a read-only session: one statement per request, with
/// nothing of the driver's in between, and after each statement the
/// session state changes the server reported. When a statement left
/// read-only mode (directly or through a stored routine), the mode is
/// restored and the script stops before the next statement.
///
/// Each statement is sent after as many newlines as precede it in the
/// script, so server errors name the same line as for the whole script.
#[allow(clippy::too_many_arguments)]
async fn read_statements(
    conn: &mut Conn,
    sql: &str,
    encoded: Option<&[u8]>,
    options: &ExecOptions,
    zones: &mut ZoneTracker,
    run: &Run,
    plan: &RestorePlan,
    expected: &ClientCharset,
    read: &mut ScriptRead,
) -> Result<(), Failure> {
    // The guard already refused scripts whose boundaries are ambiguous.
    // Empty statements and a script of comments go to the server as the
    // rest of the script, so it answers as to the whole script (see
    // [`Piece`]).
    let pieces = split_statements_strict(sql, Dialect::Mysql)
        .and_then(|_| pieces(sql, 0, &read.mode))
        .map_err(|error| {
            Failure::Error(DriverError::new(ErrorKind::ReadOnly, error.to_string()))
        })?;
    read.statements = Some(Vec::new());
    for (index, piece) in pieces.iter().enumerate() {
        if index > 0 && run.requested() {
            // A cancel that interrupted nothing: the statements so far ran,
            // the rest does not.
            read.stopped_after = Some(read.statements.as_ref().map_or(0, Vec::len));
            return Ok(());
        }
        let (request, group) = match piece {
            Piece::Group { statements, end } => match statements.first() {
                Some(first) => (
                    request_for(sql, encoded, first.offset, &sql[first.offset..*end]),
                    Some((statements, *end)),
                ),
                None => continue,
            },
            Piece::Rest { at, .. } => (request_for(sql, encoded, *at, &sql[*at..]), None),
        };
        let before = read.results.len();
        let result = read_results(conn, &request, options, read, zones).await;
        if let Some((statements, end)) = group {
            let last_end = statements.last().map_or(end, |s| s.end);
            let (done, _) = assign(
                sql,
                statements,
                read.results.get(before..).unwrap_or_default(),
                read.result_open.get(before..).unwrap_or_default(),
                result.is_err(),
                end > last_end,
            );
            if let Some(completed) = read.statements.as_mut() {
                completed.extend(done.into_iter().map(|(_, open)| open));
            }
        }
        if matches!(&result, Err(error) if error.kind == ErrorKind::ConnectionLost) {
            return result.map_err(Failure::Error);
        }
        zones.apply(conn.session_state_changes());
        read.changes
            .extend(conn.session_state_changes().iter().cloned());
        let observed = tracking::observe(conn.session_state_changes());
        let charset = observed.charset_differs(expected);
        if observed.left_read_only() || charset {
            tracing::warn!("a statement left read-only mode, restoring it");
            restore_session(conn, plan).await.map_err(Failure::Poison)?;
            let what = if observed.left_read_only() {
                observed.describe()
            } else {
                "changed the client character set, which Varve keeps at utf8mb4"
            };
            return Err(Failure::Error(left_read_only_error(
                read.statements.as_ref().map_or(0, Vec::len),
                what,
            )));
        }
        result.map_err(Failure::Error)?;
    }
    Ok(())
}

/// Sends one request and appends every result set it returns.
async fn read_results(
    conn: &mut Conn,
    sql: &[u8],
    options: &ExecOptions,
    read: &mut ScriptRead,
    zones: &mut ZoneTracker,
) -> Result<(), DriverError> {
    zones.new_request();
    let mut query = conn
        .query_iter(sql)
        .await
        .map_err(|error| session_error(&error))?;
    loop {
        let Some(columns) = query.columns() else {
            // Either the end, or a later statement failed: `next`
            // returns that error.
            query.next().await.map_err(|error| session_error(&error))?;
            break;
        };
        if columns.is_empty() {
            let rows_affected = query.affected_rows();
            let last_insert_id = query.last_insert_id().filter(|id| *id != 0);
            read.warning_counts.push(query.warnings());
            read.result_open
                .push(query.ok_status_flags().last().copied().map(in_trans));
            read.results.push(StatementResult::Command {
                tag: command_tag(rows_affected, last_insert_id),
                rows_affected: Some(rows_affected),
            });
            // Moves on to the next result set.
            query.next().await.map_err(|error| session_error(&error))?;
        } else {
            // Changes reported by the statements before this result set.
            zones.apply(query.session_state_changes());
            let zone = zones.for_result(read.results.len());
            let metas: Vec<ColumnMeta> = columns.iter().map(ColumnMeta::of).collect();
            let ui_columns = columns.iter().map(types::column).collect();
            let mut rows = Vec::new();
            let mut truncated = false;
            // The OK packet that ends this result set comes next.
            let terminator = query.ok_status_flags().len();
            let failure = loop {
                match query.next().await {
                    Ok(Some(row)) => {
                        if rows.len() >= options.max_rows {
                            truncated = true;
                            continue;
                        }
                        let values = row
                            .unwrap()
                            .into_iter()
                            .zip(&metas)
                            .map(|(value, meta)| types::value(meta, value, zone))
                            .collect();
                        rows.push(values);
                    }
                    Ok(None) => break None,
                    Err(error) => break Some(session_error(&error)),
                }
            };
            // The result set is complete once its terminator arrived; an
            // error after that belongs to a later statement.
            let ended = query.ok_status_flags().get(terminator).copied();
            if failure.is_none() || ended.is_some() {
                // Reading the next result set header replaced the count, so
                // it is only known when this was the last result.
                read.warning_counts.push(0);
                read.result_open.push(ended.map(in_trans));
                read.results.push(StatementResult::Rows {
                    columns: ui_columns,
                    rows,
                    truncated,
                });
            }
            if let Some(error) = failure {
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Adds the warnings of a finished script.
async fn finish_output(
    conn: &mut Conn,
    read: &mut ScriptRead,
) -> Result<ScriptOutput, DriverError> {
    let mut notices = Vec::new();
    if let Some(count) = read.stopped_after {
        notices.push(Notice {
            severity: "Note".into(),
            message: format!(
                "The script was cancelled after statement {count}; the statements after it \
                 were not run. (The cancel interrupted no statement with an error.)"
            ),
        });
    }
    let last_warnings = conn.get_warnings();
    let earlier = read.warning_counts.len().saturating_sub(1);
    notices.extend(warning_notes(
        read.warning_counts.get(..earlier).unwrap_or_default(),
    ));
    if last_warnings > 0 {
        notices.extend(show_warnings(conn).await?);
    }
    Ok((std::mem::take(&mut read.results), notices))
}

/// `OK, 3 rows affected` plus the last insert id when there is one.
pub(crate) fn command_tag(rows_affected: u64, last_insert_id: Option<u64>) -> String {
    let rows = if rows_affected == 1 { "row" } else { "rows" };
    match last_insert_id {
        Some(id) => format!("OK, {rows_affected} {rows} affected, last insert id {id}"),
        None => format!("OK, {rows_affected} {rows} affected"),
    }
}

async fn show_warnings(conn: &mut Conn) -> Result<Vec<Notice>, DriverError> {
    let mut query = conn
        .query_iter("SHOW WARNINGS")
        .await
        .map_err(|error| session_error(&error))?;
    let warnings: Vec<(String, u32, String)> = query
        .collect::<(String, u32, String)>()
        .await
        .map_err(|error| session_error(&error))?;
    query
        .drop_result()
        .await
        .map_err(|error| session_error(&error))?;
    Ok(warnings
        .into_iter()
        .map(|(level, code, message)| Notice {
            severity: level,
            message: format!("{code}: {message}"),
        })
        .collect())
}

/// One call of [`Session::execute`], as seen by cancel.
#[derive(Debug, Default)]
struct Run {
    /// A cancel was asked for during the call.
    requested: AtomicBool,
    /// The script has ended (its last result was read, or reading failed).
    finished: AtomicBool,
    /// Wakes the killer when the script ends.
    finished_notify: Notify,
    /// A kill attempt failed (its error went to the caller of `cancel`).
    kill_failed: AtomicBool,
    /// A killer task is running for this call.
    killer_active: AtomicBool,
}

impl Run {
    fn finish_script(&self) {
        self.finished.store(true, Ordering::SeqCst);
        self.finished_notify.notify_waiters();
    }

    fn finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    fn kill_failed(&self) -> bool {
        self.kill_failed.load(Ordering::SeqCst)
    }
}

/// Cancel bookkeeping shared by a session and its cancel handle.
///
/// Every kill holds a read guard of `kills` until its `KILL QUERY` has been
/// answered, and a call starts (and ends) only after taking the write
/// guard. So a kill can never reach the server after the call it was meant
/// for: not even a repeated kill, or one whose `cancel` future was dropped.
#[derive(Debug, Default)]
struct CancelState {
    kills: Arc<RwLock<()>>,
    current: std::sync::Mutex<Option<Arc<Run>>>,
}

impl CancelState {
    /// Starts a call once no kill is in flight.
    async fn begin(&self) -> Arc<Run> {
        let _no_kills = self.kills.write().await;
        let run = Arc::new(Run::default());
        *self.lock_current() = Some(Arc::clone(&run));
        run
    }

    /// Waits until no kill is in flight.
    async fn wait_for_kills(&self) {
        drop(self.kills.write().await);
    }

    fn current(&self) -> Option<Arc<Run>> {
        self.lock_current().clone()
    }

    fn lock_current(&self) -> std::sync::MutexGuard<'_, Option<Arc<Run>>> {
        // The guarded value is a plain `Option`, valid even after a panic.
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Ends a call, also when its future is dropped: stops its killer.
struct RunEnd<'a> {
    state: &'a CancelState,
    run: Arc<Run>,
}

impl Drop for RunEnd<'_> {
    fn drop(&mut self) {
        self.run.finish_script();
        let mut current = self.state.lock_current();
        if current
            .as_ref()
            .is_some_and(|run| Arc::ptr_eq(run, &self.run))
        {
            *current = None;
        }
    }
}

assert_not_debug!(KillTarget, Connection, Core);

/// Where `KILL QUERY` goes. Never `Debug`: it holds `Opts`, which prints
/// the client key (see `connect::client_identity`).
struct KillTarget {
    opts: Opts,
    connection_id: u64,
    connect_timeout: Duration,
}

impl KillTarget {
    /// Runs `KILL QUERY` once, opening the kill connection when needed.
    async fn kill_once(&self, conn: &mut Option<Conn>) -> Result<(), DriverError> {
        let kill = match conn {
            Some(kill) => kill,
            None => conn.insert(connect::reconnect(self.opts.clone(), self.connect_timeout).await?),
        };
        match kill
            .query_drop(format!("KILL QUERY {}", self.connection_id))
            .await
        {
            // The session's connection is gone: nothing left to cancel.
            Ok(()) => Ok(()),
            Err(MyError::Server(error)) if error.code == ER_NO_SUCH_THREAD => Ok(()),
            Err(error) => {
                if let Some(broken) = conn.take() {
                    connect::disconnect_quietly(broken).await;
                }
                Err(session_error(&error))
            }
        }
    }
}

/// Cancels the running statement with `KILL QUERY` on a short-lived second
/// connection to the same server.
struct MysqlCancelHandle {
    target: Arc<KillTarget>,
    state: Arc<CancelState>,
}

#[async_trait::async_trait]
impl CancelHandle for MysqlCancelHandle {
    async fn cancel(&self) -> Result<(), DriverError> {
        let guard = Arc::clone(&self.state.kills).read_owned().await;
        let Some(run) = self.state.current() else {
            // Nothing runs, so there is nothing to kill.
            return Ok(());
        };
        run.requested.store(true, Ordering::SeqCst);
        if run.finished() || run.killer_active.swap(true, Ordering::SeqCst) {
            // Too late to kill anything, or a killer is already at work.
            return Ok(());
        }
        let (first, first_result) = oneshot::channel();
        // A task of its own, so a dropped `cancel` future never leaves a
        // kill half sent.
        tokio::spawn(run_killer(Arc::clone(&self.target), run, guard, first));
        first_result.await.unwrap_or(Ok(()))
    }
}

/// Kills the running script and keeps killing it every
/// [`KILL_REPEAT_INTERVAL`] until it ends.
///
/// MySQL ends a killed `SLEEP()` normally and then runs the next statement
/// of the script, so one kill is not always enough. The result of the
/// first kill goes to `first`. A failed kill stops the killer; cancelling
/// again starts a new one.
async fn run_killer(
    target: Arc<KillTarget>,
    run: Arc<Run>,
    _in_flight: OwnedRwLockReadGuard<()>,
    first: oneshot::Sender<Result<(), DriverError>>,
) {
    let mut first = Some(first);
    let mut conn = None;
    loop {
        // Registered before the check, so an end in between is not missed.
        let ended = run.finished_notify.notified();
        tokio::pin!(ended);
        ended.as_mut().enable();
        if run.finished() {
            break;
        }
        let result = target.kill_once(&mut conn).await;
        let failed = result.is_err();
        if failed {
            run.kill_failed.store(true, Ordering::SeqCst);
        }
        match first.take() {
            Some(first) => {
                // The caller may have stopped waiting; the kill was still sent.
                let _ = first.send(result);
            }
            None => {
                if let Err(error) = result {
                    tracing::debug!(%error, "repeating KILL QUERY failed");
                }
            }
        }
        if failed {
            break;
        }
        tokio::select! {
            () = tokio::time::sleep(KILL_REPEAT_INTERVAL) => {}
            () = &mut ended => break,
        }
    }
    run.killer_active.store(false, Ordering::SeqCst);
    if let Some(conn) = conn {
        connect::disconnect_quietly(conn).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_role_statements_run_one_by_one() {
        let mode = MysqlMode::default();
        for sql in [
            "SET DEFAULT ROLE r TO u",
            "set default role all to u",
            "GRANT SELECT ON d.* TO u",
            "SET PASSWORD FOR u = 'x'",
            "CREATE USER u",
        ] {
            assert!(changes_privileges(sql, &mode), "{sql}");
        }
        for sql in ["SELECT 'SET DEFAULT ROLE r TO u'", "SET @x = 1"] {
            assert!(!changes_privileges(sql, &mode), "{sql}");
        }
    }

    /// Audit round 6, Low 6: MariaDB CREATE OR REPLACE USER / ROLE run one
    /// statement per request, so they are reported exactly.
    #[test]
    fn or_replace_account_statements_run_one_by_one() {
        let mode = MysqlMode::default();
        for sql in [
            "CREATE OR REPLACE USER u",
            "SELECT 1; create or replace role r; SELECT 2",
        ] {
            assert!(changes_privileges(sql, &mode), "{sql}");
        }
        for sql in [
            "REPLACE user VALUES (1)",
            "CREATE OR REPLACE TABLE user (a int)",
        ] {
            assert!(!changes_privileges(sql, &mode), "{sql}");
        }
    }

    #[test]
    fn command_tags() {
        assert_eq!(command_tag(0, None), "OK, 0 rows affected");
        assert_eq!(command_tag(1, None), "OK, 1 row affected");
        assert_eq!(command_tag(3, None), "OK, 3 rows affected");
        assert_eq!(
            command_tag(1, Some(42)),
            "OK, 1 row affected, last insert id 42"
        );
    }

    #[test]
    fn utc_zones() {
        for zone in [
            "+00:00", "-00:00", "+0:00", "UTC", "utc", "Etc/UTC", "GMT", "Zulu",
        ] {
            assert!(is_utc_zone(zone), "{zone}");
        }
        for zone in [
            "SYSTEM",
            "+02:00",
            "-00:30",
            "Europe/Amsterdam",
            "",
            "+00",
            "+00:00:00",
            "+:00",
        ] {
            assert!(!is_utc_zone(zone), "{zone}");
        }
    }

    #[test]
    fn finds_the_first_statement_that_may_change_the_zone() {
        assert_eq!(first_zone_change("SELECT 1; SELECT 2"), None);
        assert_eq!(first_zone_change("SET @a = 1; USE varve"), None);
        assert_eq!(
            first_zone_change("SELECT 1; SET time_zone = '+02:00'; SELECT 2"),
            Some(1)
        );
        assert_eq!(
            first_zone_change("select 1; set @@SESSION.TIME_ZONE = 'UTC'"),
            Some(1)
        );
        assert_eq!(first_zone_change("SELECT 1; CALL p(); SELECT 2"), Some(1));
        assert_eq!(first_zone_change("EXECUTE stmt"), Some(0));
        // Comments are not statements.
        assert_eq!(
            first_zone_change("-- SET time_zone = '+02:00'\nSELECT 1"),
            None
        );
    }

    #[test]
    fn result_zones_follow_the_change() {
        let zones = ResultZones {
            session: TimestampZone::Utc,
            changes_at: Some(2),
        };
        assert_eq!(zones.for_result(0), TimestampZone::Utc);
        assert_eq!(zones.for_result(1), TimestampZone::Utc);
        assert_eq!(zones.for_result(2), TimestampZone::Unknown);
        assert_eq!(zones.for_result(5), TimestampZone::Unknown);
        let unchanged = ResultZones {
            session: TimestampZone::Unknown,
            changes_at: None,
        };
        assert_eq!(unchanged.for_result(0), TimestampZone::Unknown);
    }

    #[test]
    fn notices_reach_results_and_errors() {
        let mut ok: Result<ScriptOutput, Failure> = Ok((Vec::new(), Vec::new()));
        add_notice(&mut ok, CHARSET_RESTORED);
        assert!(matches!(&ok, Ok((_, notices)) if notices.len() == 1
            && notices[0].severity == "Warning"));
        let mut failed: Result<ScriptOutput, Failure> =
            Err(Failure::Error(DriverError::new(ErrorKind::Query, "boom")));
        add_notice(&mut failed, TRACKING_RESTORED);
        assert!(
            matches!(&failed, Err(Failure::Error(error)) if error.detail.as_deref() == Some(TRACKING_RESTORED))
        );
    }

    #[test]
    fn stripping_zones_keeps_the_local_time() {
        let mut outcome: Result<ScriptOutput, Failure> = Ok((
            vec![StatementResult::Rows {
                columns: Vec::new(),
                rows: vec![vec![
                    Value::TimestampTz("2026-10-19T11:45:00Z".into()),
                    Value::Int(1),
                ]],
                truncated: false,
            }],
            Vec::new(),
        ));
        strip_zones(&mut outcome);
        assert!(matches!(&outcome, Ok((results, _)) if matches!(&results[0],
            StatementResult::Rows { rows, .. }
                if rows[0] == vec![Value::Timestamp("2026-10-19T11:45:00".into()), Value::Int(1)])));
    }

    #[test]
    fn pieces_mirror_what_the_server_reads() -> Result<(), varve_driver::AmbiguousSplit> {
        let mode = MysqlMode::default();
        let stmt =
            |offset: usize, len: usize, end: usize, kind: StatementKind, definition: bool| Stmt {
                offset,
                len,
                end,
                kind,
                definition,
            };
        let group = |statements: Vec<Stmt>, end: usize| Piece::Group { statements, end };
        // A definition goes with the statement after it (audit round 3,
        // Medium 1): its `;` and the next statement's diagnostics stay.
        let sql = "CREATE PROCEDURE p() SELECT 1; GRANT SELECT ON d.* TO u; SELECT 2";
        assert_eq!(
            pieces(sql, 0, &mode)?,
            [
                group(
                    vec![
                        stmt(0, 29, 30, StatementKind::Ddl, true),
                        stmt(31, 24, 56, StatementKind::Grant, false)
                    ],
                    56
                ),
                group(vec![stmt(57, 8, 65, StatementKind::Select, false)], 65)
            ]
        );
        // A table named `event` is no definition.
        let sql = "CREATE TABLE event (x int); SELECT ROW_COUNT()";
        assert!(
            matches!(&pieces(sql, 0, &mode)?[0], Piece::Group { statements, .. } if statements.len() == 1)
        );
        // Empty statements: the rest goes to the server (R2-1).
        let sql = "GRANT SELECT ON d.* TO u;; GRANT INSERT ON d.* TO u";
        assert_eq!(
            pieces(sql, 0, &mode)?,
            [group(
                vec![stmt(0, 24, 25, StatementKind::Grant, false)],
                sql.len()
            )]
        );
        assert_eq!(
            pieces("; GRANT SELECT ON d.* TO u", 0, &mode)?,
            [Piece::Rest {
                at: 0,
                statements: true
            }]
        );
        // The last statement takes trailing comments and `;` with it
        // (audit round 3, Low 2); whitespace alone is not sent.
        assert_eq!(
            pieces("SELECT 1; -- c", 0, &mode)?,
            [group(vec![stmt(0, 8, 9, StatementKind::Select, false)], 14)]
        );
        assert_eq!(
            pieces("SELECT 1;  \n", 0, &mode)?,
            [group(vec![stmt(0, 8, 9, StatementKind::Select, false)], 9)]
        );
        // An executable comment that does not run on the server is a
        // comment, so the statement after it is empty.
        let mysql = MysqlMode {
            server: Some(MysqlServer::new(false, 8, 0, 46)),
            ..MysqlMode::default()
        };
        let sql = "/*!99999 GRANT SELECT ON d.* TO u */; GRANT INSERT ON d.* TO u";
        assert_eq!(
            pieces(sql, 0, &mysql)?,
            [Piece::Rest {
                at: 0,
                statements: true
            }]
        );
        // Only comments: the whole script goes as it is.
        assert_eq!(
            pieces("-- nothing", 0, &mode)?,
            [Piece::Rest {
                at: 0,
                statements: false
            }]
        );
        Ok(())
    }

    #[test]
    fn found_problems_name_themselves() {
        let found = Found {
            tracking: true,
            read_only_off: true,
            charset: false,
        };
        assert!(found.anything());
        let error = found.error();
        assert_eq!(error.kind, ErrorKind::ReadOnly);
        assert!(error.message.contains("tracking was narrowed"));
        assert!(error.message.contains("read-only mode was off"));
        assert!(!Found::default().anything());
    }

    #[tokio::test]
    async fn calls_wait_for_kills_in_flight() -> Result<(), Box<dyn std::error::Error>> {
        let state = Arc::new(CancelState::default());
        let kill = Arc::clone(&state.kills).read_owned().await;
        let waiting = Arc::clone(&state);
        let call = tokio::spawn(async move { waiting.begin().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !call.is_finished(),
            "a call started while a kill was in flight"
        );
        assert!(state.current().is_none());
        drop(kill);
        let run = tokio::time::timeout(Duration::from_secs(1), call).await??;
        assert!(
            state
                .current()
                .is_some_and(|current| Arc::ptr_eq(&current, &run))
        );
        Ok(())
    }

    #[tokio::test]
    async fn ending_a_call_clears_it() {
        let state = CancelState::default();
        let run = state.begin().await;
        {
            let _end = RunEnd {
                state: &state,
                run: Arc::clone(&run),
            };
            assert!(!run.finished());
        }
        assert!(run.finished());
        assert!(state.current().is_none());
    }
}
