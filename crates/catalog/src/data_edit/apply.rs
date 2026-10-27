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
/// The session must not be inside a transaction: on PostgreSQL and MariaDB
/// an open transaction is detected and refused with
/// [`CatalogError::Invalid`]; MySQL has no way to tell, and its
/// `START TRANSACTION` commits an open transaction implicitly. The future
/// must run to completion: dropping it midway leaves the transaction open
/// on the session.
///
/// # Errors
///
/// [`CatalogError::Invalid`] when the plan was made for another engine or
/// the session has an open transaction; [`CatalogError::Driver`] for a
/// read-only session (the driver's or server's refusal, after rolling
/// back), a lost connection, a cancel or timeout, and for failures of the
/// transaction statements themselves; [`CatalogError::Unexpected`] when a
/// written row cannot be read back.
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
    // PostgreSQL: outside a transaction block each statement is its own
    // transaction, so its start time is the statement's start time.
    let check = match engine {
        Engine::Postgres => {
            "SELECT pg_catalog.transaction_timestamp() OPERATOR(pg_catalog.<>) \
             pg_catalog.statement_timestamp() AS open"
        }
        Engine::Mariadb => "SELECT @@in_transaction AS open",
        Engine::Mysql | Engine::Redis => return Ok(()),
    };
    let rows = crate::rows::query(session, check).await?;
    if rows.single("the transaction state")?.bool("open")? == Some(true) {
        return Err(CatalogError::Invalid {
            message: "the session has an open transaction; commit or roll it back before saving \
                      grid changes"
                .into(),
        });
    }
    Ok(())
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
