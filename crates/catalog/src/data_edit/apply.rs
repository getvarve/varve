//! Runs a [`RowEditPlan`] in one transaction ([`apply`]).

use varve_core::Engine;
use varve_driver::{DriverError, ErrorKind, ServerInfo, Session, StatementResult, Value};

use super::model::{
    AppliedChange, ApplyOutcome, ChangeOp, ConflictReason, PlannedChange, RowEditPlan,
};
use crate::model::{CatalogError, CatalogResult};
use crate::rows::run;

/// Runs every statement of `plan` in one transaction and commits only when
/// every `UPDATE` and `DELETE` affected exactly one row and the server
/// accepted every statement.
///
/// - A change that matched no row (deleted, key changed, or with original
///   checks a compared value changed by someone else) or more than one row
///   (the key is not unique) rolls everything back and returns
///   [`ApplyOutcome::Conflict`] naming the change.
/// - A statement the server rejects (constraint, type, trigger) rolls
///   everything back and returns [`ApplyOutcome::Failed`] naming the change.
/// - On success the written rows come back as stored (defaults, generated
///   and identity values filled in), one entry per change.
///
/// The session must not be inside a transaction (MySQL's and MariaDB's
/// `START TRANSACTION` would commit it implicitly, PostgreSQL would nest
/// the grid's work in it): an open transaction is refused with
/// [`CatalogError::Invalid`], and so is a MySQL session whose transaction
/// state cannot be read. The future must run to completion: dropping it
/// midway leaves the transaction open on the session.
///
/// # Errors
///
/// [`CatalogError::Invalid`] when the plan was made for another engine or
/// the session has (or may have) an open transaction; [`CatalogError::Driver`] for a
/// read-only session (the driver's or server's refusal, after rolling
/// back), a lost connection, a cancel or timeout, and for failures of the
/// transaction statements themselves; [`CatalogError::Unexpected`] when
/// reading a written row back finds more than one row. A row the read back
/// does not find is returned without values (`row: None`).
pub async fn apply(
    session: &dyn Session,
    server: &ServerInfo,
    plan: &RowEditPlan,
) -> CatalogResult<ApplyOutcome> {
    if plan.engine != server.engine || session.server_info().engine != server.engine {
        return Err(CatalogError::Invalid {
            message: "the edit plan was made for another engine than this session's".into(),
        });
    }
    if plan.engine == Engine::Redis {
        return Err(CatalogError::Unsupported {
            message: "Redis values are edited with the key browser".into(),
        });
    }
    if plan.changes.is_empty() {
        return Ok(ApplyOutcome::Applied { rows: Vec::new() });
    }
    refuse_open_transaction(session, plan.engine).await?;
    let begin = match plan.engine {
        Engine::Postgres => "BEGIN",
        _ => "START TRANSACTION",
    };
    run(session, begin).await?;

    let mut applied = Vec::with_capacity(plan.changes.len());
    for change in &plan.changes {
        match run_change(session, change).await {
            Ok(Step::Done(row)) => applied.push(AppliedChange {
                change: change.change,
                op: change.op,
                row,
            }),
            Ok(Step::Conflict(rows_matched)) => {
                rollback(session, change.change).await?;
                return Ok(conflict(change, rows_matched));
            }
            Err(StepError::Statement(error)) => {
                rollback(session, change.change).await?;
                return if fatal(&error) {
                    Err(CatalogError::Driver { error })
                } else {
                    Ok(ApplyOutcome::Failed {
                        change: change.change,
                        error,
                    })
                };
            }
            Err(StepError::Catalog(error)) => {
                rollback(session, change.change).await?;
                return Err(error);
            }
        }
    }
    if let Err(error) = run(session, "COMMIT").await {
        // A failed COMMIT (a deferred constraint, serialization failure)
        // has already ended the transaction on the server; the rollback
        // only makes sure nothing stays open.
        let _ = run(session, "ROLLBACK").await;
        return Err(error);
    }
    Ok(ApplyOutcome::Applied { rows: applied })
}

/// Errors that are about the session, not about one change: the whole
/// apply fails with them.
fn fatal(error: &DriverError) -> bool {
    matches!(
        error.kind,
        ErrorKind::ReadOnly
            | ErrorKind::ConnectionLost
            | ErrorKind::Cancelled
            | ErrorKind::Timeout
            | ErrorKind::Connect
            | ErrorKind::Protocol
    )
}

async fn refuse_open_transaction(session: &dyn Session, engine: Engine) -> CatalogResult<()> {
    // The server's own status flag from the last reply (PostgreSQL
    // ReadyForQuery, MySQL and MariaDB SERVER_STATUS_IN_TRANS): no round
    // trip and no privilege. The queries are only the fallback.
    let open = match session.in_transaction() {
        Some(open) => Some(open),
        None => query_open_transaction(session, engine).await?,
    };
    match open {
        Some(false) => Ok(()),
        Some(true) => Err(CatalogError::Invalid {
            message: "the session has an open transaction; commit or roll it back before saving \
                      grid changes"
                .into(),
        }),
        // Only MySQL ends up here. Its START TRANSACTION would commit an
        // open transaction without a word, so an unknown state is refused.
        None => Err(CatalogError::Invalid {
            message: "cannot tell whether the session has an open transaction, and saving grid \
                      changes would commit it; commit or roll back, then try again on a new \
                      session"
                .into(),
        }),
    }
}

/// Asks the server whether the session is inside a transaction. `None`
/// when no query could tell (MySQL without access to `INNODB_TRX` and no
/// transaction in the Performance Schema).
async fn query_open_transaction(
    session: &dyn Session,
    engine: Engine,
) -> CatalogResult<Option<bool>> {
    // Each query with whether its "no" settles the question.
    let queries: &[(&str, bool)] = match engine {
        // Outside a transaction block each statement is its own
        // transaction, so its start time is the statement's start time.
        Engine::Postgres => &[(
            "SELECT pg_catalog.transaction_timestamp() OPERATOR(pg_catalog.<>) \
             pg_catalog.statement_timestamp() AS open",
            true,
        )],
        Engine::Mariadb => &[("SELECT @@in_transaction AS open", true)],
        // MySQL has no session variable for it. The Performance Schema
        // shows the thread's explicit transaction at once, but its table is
        // also empty when the Performance Schema is off or does not record
        // transactions (the default on MySQL 5.7), so only its "yes"
        // counts. INNODB_TRX (needs PROCESS) is refreshed only every 0.1 s,
        // so it comes second; its answer is final.
        Engine::Mysql => &[
            (
                "SELECT COUNT(*) > 0 AS open FROM performance_schema.events_transactions_current t \
                 JOIN performance_schema.threads th ON th.THREAD_ID = t.THREAD_ID \
                 WHERE th.PROCESSLIST_ID = CONNECTION_ID() AND t.STATE = 'ACTIVE' \
                 AND t.AUTOCOMMIT = 'NO'",
                false,
            ),
            (
                "SELECT COUNT(*) > 0 AS open FROM information_schema.INNODB_TRX \
                 WHERE trx_mysql_thread_id = CONNECTION_ID()",
                true,
            ),
        ],
        // Redis plans are refused before this.
        Engine::Redis => return Ok(None),
    };
    for (sql, no_settles) in queries {
        match crate::rows::query(session, sql).await {
            Ok(rows) => match rows.single("the transaction state")?.bool("open")? {
                Some(false) if !no_settles => {}
                answer => return Ok(answer),
            },
            // A missing table or privilege on MySQL: try the next query.
            Err(CatalogError::Driver { error })
                if engine == Engine::Mysql && error.kind != ErrorKind::ConnectionLost => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

async fn rollback(session: &dyn Session, change: usize) -> CatalogResult<()> {
    run(session, "ROLLBACK")
        .await
        .map(|_| ())
        .map_err(|e| match e {
            CatalogError::Driver { mut error } => {
                error.message = format!(
                    "change {} failed and rolling back failed too: {}",
                    change + 1,
                    error.message
                );
                CatalogError::Driver { error }
            }
            other => other,
        })
}

fn conflict(change: &PlannedChange, rows_matched: u64) -> ApplyOutcome {
    let number = change.change + 1;
    let (reason, message) = if rows_matched == 0 {
        let what = if change.checks_original {
            "someone else changed or deleted the row"
        } else {
            "someone else deleted the row or changed its key"
        };
        (
            ConflictReason::RowChanged,
            format!("change {number}: {what}; reload the grid and try again"),
        )
    } else {
        (
            ConflictReason::KeyNotUnique,
            format!(
                "change {number}: the key matched more than one row, so it does not identify a row"
            ),
        )
    };
    ApplyOutcome::Conflict {
        change: change.change,
        reason,
        rows_matched,
        message,
    }
}

enum Step {
    Done(Option<Vec<(String, Value)>>),
    Conflict(u64),
}

enum StepError {
    /// The server rejected a statement of the change.
    Statement(DriverError),
    /// Anything else.
    Catalog(CatalogError),
}

impl From<CatalogError> for StepError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::Driver { error } => Self::Statement(error),
            other => Self::Catalog(other),
        }
    }
}

/// What one statement returned: its rows, or the affected row count.
enum Answer {
    Rows(Vec<(String, Value)>, u64),
    Affected(Option<u64>),
}

async fn statement(session: &dyn Session, sql: &str) -> Result<Answer, StepError> {
    let output = run(session, sql).await?;
    match output.results.into_iter().next() {
        Some(StatementResult::Rows {
            columns,
            rows,
            truncated,
        }) => {
            let count = u64::try_from(rows.len()).unwrap_or(u64::MAX) + u64::from(truncated);
            let first = rows
                .into_iter()
                .next()
                .map(|values| {
                    columns
                        .into_iter()
                        .map(|c| c.name)
                        .zip(values)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Ok(Answer::Rows(first, count))
        }
        Some(StatementResult::Command { rows_affected, .. }) => Ok(Answer::Affected(rows_affected)),
        None => Err(StepError::Catalog(CatalogError::Unexpected {
            message: "the statement returned no result".into(),
        })),
    }
}

fn unexpected(change: &PlannedChange, what: &str) -> StepError {
    StepError::Catalog(CatalogError::Unexpected {
        message: format!("change {}: {what}", change.change + 1),
    })
}

async fn run_change(session: &dyn Session, change: &PlannedChange) -> Result<Step, StepError> {
    if let Some(lock) = &change.lock {
        match statement(session, lock).await? {
            Answer::Rows(_, 1) => {}
            Answer::Rows(_, n) => return Ok(Step::Conflict(n)),
            Answer::Affected(_) => return Err(unexpected(change, "the row lock returned no rows")),
        }
    }
    let answer = statement(session, &change.statement).await?;
    let mut row = match answer {
        Answer::Rows(row, 1) => Some(row),
        Answer::Rows(_, n) => {
            return match change.op {
                ChangeOp::Insert => Err(unexpected(
                    change,
                    &format!("the insert returned {n} rows instead of one"),
                )),
                _ => Ok(Step::Conflict(n)),
            };
        }
        Answer::Affected(affected) => {
            let affected = affected.ok_or_else(|| {
                unexpected(change, "the server did not report the affected row count")
            })?;
            match change.op {
                // Locked before: MySQL reports 0 when the values did not
                // change, which is fine; more than one cannot happen.
                ChangeOp::Update if change.lock.is_some() => {
                    if affected > 1 {
                        return Ok(Step::Conflict(affected));
                    }
                }
                ChangeOp::Insert if affected != 1 => {
                    return Err(unexpected(
                        change,
                        &format!("the insert affected {affected} rows instead of one"),
                    ));
                }
                ChangeOp::Insert => {}
                ChangeOp::Update | ChangeOp::Delete => {
                    if affected != 1 {
                        return Ok(Step::Conflict(affected));
                    }
                }
            }
            None
        }
    };
    if let Some(refresh) = &change.refresh {
        match statement(session, refresh).await? {
            Answer::Rows(found, 1) => row = Some(found),
            // The server accepted the write but stored a key other than the
            // literal (fractional seconds cut, a DECIMAL rounded, a trigger
            // that rewrote the key, trailing spaces in a PAD SPACE key): the
            // row is written, it just cannot be found by what was typed.
            Answer::Rows(_, 0) => row = None,
            Answer::Rows(_, n) => {
                return Err(unexpected(
                    change,
                    &format!("reading the written row back found {n} rows instead of one"),
                ));
            }
            Answer::Affected(_) => {
                return Err(unexpected(
                    change,
                    "reading the written row back returned no rows",
                ));
            }
        }
    }
    if change.op == ChangeOp::Delete {
        row = None;
    }
    Ok(Step::Done(row))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use varve_driver::{CancelHandle, Column, ExecOptions, ExecOutput};

    use super::*;

    /// A session that answers each statement with the next scripted reply
    /// and records what it ran.
    struct Scripted {
        server: ServerInfo,
        in_transaction: Option<bool>,
        replies: Mutex<Vec<Result<StatementResult, DriverError>>>,
        ran: Mutex<Vec<String>>,
    }

    struct NoCancel;

    #[async_trait::async_trait]
    impl CancelHandle for NoCancel {
        async fn cancel(&self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Session for Scripted {
        fn server_info(&self) -> &ServerInfo {
            &self.server
        }

        fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
            Arc::new(NoCancel)
        }

        async fn execute(
            &self,
            sql: &str,
            _options: &ExecOptions,
        ) -> Result<ExecOutput, DriverError> {
            self.ran
                .lock()
                .map_err(|_| poisoned())?
                .push(sql.to_owned());
            let mut replies = self.replies.lock().map_err(|_| poisoned())?;
            if replies.is_empty() {
                return Err(DriverError::new(ErrorKind::Query, "no scripted reply"));
            }
            replies.remove(0).map(|result| ExecOutput {
                results: vec![result],
                notices: vec![],
                elapsed_ms: 0,
            })
        }

        fn in_transaction(&self) -> Option<bool> {
            self.in_transaction
        }

        async fn ping(&self) -> Result<(), DriverError> {
            Ok(())
        }

        async fn close(self: Box<Self>) -> Result<(), DriverError> {
            Ok(())
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn poisoned() -> DriverError {
        DriverError::new(ErrorKind::Protocol, "poisoned")
    }

    fn mysql_session(
        in_transaction: Option<bool>,
        replies: Vec<Result<StatementResult, DriverError>>,
    ) -> Scripted {
        Scripted {
            server: ServerInfo {
                engine: Engine::Mysql,
                version: "8.0.40".into(),
                major: 8,
                minor: 0,
                patch: Some(40),
            },
            in_transaction,
            replies: Mutex::new(replies),
            ran: Mutex::new(Vec::new()),
        }
    }

    fn rows(columns: &[&str], data: Vec<Vec<Value>>) -> Result<StatementResult, DriverError> {
        Ok(StatementResult::Rows {
            columns: columns
                .iter()
                .map(|c| Column {
                    name: (*c).into(),
                    type_name: "bigint".into(),
                })
                .collect(),
            rows: data,
            truncated: false,
        })
    }

    fn ok(rows_affected: u64) -> Result<StatementResult, DriverError> {
        Ok(StatementResult::Command {
            tag: "OK".into(),
            rows_affected: Some(rows_affected),
        })
    }

    fn denied() -> Result<StatementResult, DriverError> {
        Err(DriverError::new(ErrorKind::Query, "access denied").with_code("1142"))
    }

    fn insert_plan() -> RowEditPlan {
        RowEditPlan {
            engine: Engine::Mysql,
            changes: vec![PlannedChange {
                change: 0,
                op: ChangeOp::Insert,
                lock: None,
                statement: "INSERT INTO `d`.`t` (`at`) VALUES ('2026-01-01 10:00:00.123456')"
                    .into(),
                returns_row: false,
                refresh: Some(
                    "SELECT * FROM `d`.`t` WHERE `at` = '2026-01-01 10:00:00.123456'".into(),
                ),
                checks_original: false,
            }],
            warnings: Vec::new(),
            has_expressions: false,
        }
    }

    fn ran(session: &Scripted) -> Vec<String> {
        session.ran.lock().map(|r| r.clone()).unwrap_or_default()
    }

    #[tokio::test]
    async fn mysql_refuses_an_open_transaction_from_the_status_flag() {
        let session = mysql_session(Some(true), vec![]);
        let result = apply(&session, &session.server.clone(), &insert_plan()).await;
        assert!(
            matches!(result, Err(CatalogError::Invalid { .. })),
            "{result:?}"
        );
        assert!(ran(&session).is_empty());
    }

    #[tokio::test]
    async fn mysql_without_the_flag_asks_the_server_and_refuses_an_open_transaction() {
        // The Performance Schema is not readable; INNODB_TRX shows the
        // transaction.
        let session = mysql_session(
            None,
            vec![denied(), rows(&["open"], vec![vec![Value::Int(1)]])],
        );
        let result = apply(&session, &session.server.clone(), &insert_plan()).await;
        assert!(
            matches!(result, Err(CatalogError::Invalid { .. })),
            "{result:?}"
        );
        let ran = ran(&session);
        assert_eq!(ran.len(), 2);
        assert!(ran.iter().all(|sql| !sql.contains("START TRANSACTION")));
    }

    #[tokio::test]
    async fn an_empty_performance_schema_does_not_prove_there_is_no_transaction()
    -> CatalogResult<()> {
        let none = || rows(&["open"], vec![vec![Value::Int(0)]]);
        // The Performance Schema records no transactions; INNODB_TRX shows one.
        let session = mysql_session(
            None,
            vec![none(), rows(&["open"], vec![vec![Value::Int(1)]])],
        );
        let result = apply(&session, &session.server.clone(), &insert_plan()).await;
        assert!(
            matches!(result, Err(CatalogError::Invalid { .. })),
            "{result:?}"
        );
        assert_eq!(ran(&session).len(), 2);
        // INNODB_TRX is not readable either: the state is unknown.
        let session = mysql_session(None, vec![none(), denied()]);
        let result = apply(&session, &session.server.clone(), &insert_plan()).await;
        assert!(
            matches!(result, Err(CatalogError::Invalid { .. })),
            "{result:?}"
        );
        assert_eq!(ran(&session).len(), 2);
        // Both say there is none: the save goes ahead.
        let session = mysql_session(
            None,
            vec![
                none(),
                none(),
                ok(0),
                ok(1),
                rows(&["id", "at"], vec![]),
                ok(0),
            ],
        );
        apply(&session, &session.server.clone(), &insert_plan()).await?;
        let ran = ran(&session);
        assert!(ran.iter().any(|sql| sql.contains("START TRANSACTION")));
        assert_eq!(ran.last().map(String::as_str), Some("COMMIT"));
        Ok(())
    }

    #[tokio::test]
    async fn mysql_refuses_when_the_transaction_state_is_unknown() {
        let session = mysql_session(None, vec![denied(), denied()]);
        let result = apply(&session, &session.server.clone(), &insert_plan()).await;
        assert!(
            matches!(result, Err(CatalogError::Invalid { .. })),
            "{result:?}"
        );
        assert!(
            ran(&session)
                .iter()
                .all(|sql| !sql.contains("START TRANSACTION"))
        );
    }

    #[tokio::test]
    async fn a_row_the_read_back_does_not_find_is_saved_without_values() -> CatalogResult<()> {
        // The column keeps fewer fractional digits than were typed, so the
        // read back by the typed value finds nothing.
        let session = mysql_session(
            Some(false),
            vec![ok(0), ok(1), rows(&["id", "at"], vec![]), ok(0)],
        );
        let outcome = apply(&session, &session.server.clone(), &insert_plan()).await?;
        let ApplyOutcome::Applied { rows } = outcome else {
            return Err(CatalogError::Unexpected {
                message: format!("expected applied, got {outcome:?}"),
            });
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows.first().and_then(|r| r.row.clone()), None);
        assert_eq!(ran(&session).last().map(String::as_str), Some("COMMIT"));
        Ok(())
    }

    #[tokio::test]
    async fn more_than_one_row_read_back_still_rolls_back() {
        let session = mysql_session(
            Some(false),
            vec![
                ok(0),
                ok(1),
                rows(&["id"], vec![vec![Value::Int(1)], vec![Value::Int(2)]]),
                ok(0),
            ],
        );
        let result = apply(&session, &session.server.clone(), &insert_plan()).await;
        assert!(
            matches!(result, Err(CatalogError::Unexpected { .. })),
            "{result:?}"
        );
        assert_eq!(ran(&session).last().map(String::as_str), Some("ROLLBACK"));
    }
}
