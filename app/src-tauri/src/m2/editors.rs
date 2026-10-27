//! Object editors (design doc 4.2): a typed change becomes an [`EditPlan`]
//! that the server keeps ([`super::plans`]) and the UI shows; applying it
//! by id runs exactly that plan through the session manager, so the
//! destructive statement confirmation, read-only sessions, open
//! transactions and the query log apply as in the SQL editor.

use std::time::Instant;

use serde::{Deserialize, Serialize};
use varve_activity::{ActivityEvent, Category, Outcome as EventOutcome, SqlDialect};
use varve_catalog::EditPlan;
use varve_catalog::mysql::edit::MyChange;
use varve_catalog::postgres::edit::{DatabaseChange, DatabaseOperation, PgChange};
use varve_core::Engine;
use varve_driver::{
    Dialect, DriverError, ExecOptions, ExecOutput, StatementKind, split_statements,
};

use super::plans::{Planned, StoredPlan, wrong_kind};
use super::{elapsed_ms, error_details, sql_dialect};
use crate::commands::{CmdError, Services};
use crate::sessions::{ExecSpec, SessionError, SessionInfo};

type CmdResult<T> = Result<T, CmdError>;

/// A change from an object editor, tagged by engine:
/// `{ engine: 'postgres', change: PgChange }` or
/// `{ engine: 'mysql', change: MyChange }` (MySQL and MariaDB).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "engine", content = "change", rename_all = "camelCase")]
pub enum EngineChange {
    /// A PostgreSQL change.
    Postgres(Box<PgChange>),
    /// A MySQL or MariaDB change.
    Mysql(Box<MyChange>),
}

/// What [`Services::edit_apply`] ran.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditApplied {
    /// Statements of the plan that ran.
    pub statements: usize,
    /// True when they ran in one transaction that was committed.
    pub transactional: bool,
    /// Server output: one entry per statement, or one for the whole
    /// transaction.
    pub outputs: Vec<ExecOutput>,
    /// Wall time.
    pub duration_ms: u64,
}

/// The statement kinds of a plan, one per statement, after checking that
/// every entry is exactly one statement and none controls transactions
/// (which would break the transaction the runner opens).
///
/// # Errors
/// [`CmdError::Invalid`] for an empty plan, an entry with no or several
/// statements, or a transaction control statement.
pub fn plan_kinds(statements: &[String], dialect: Dialect) -> Result<Vec<StatementKind>, CmdError> {
    if statements.is_empty() {
        return Err(CmdError::Invalid {
            message: "The plan has no statements to run.".into(),
        });
    }
    let mut kinds = Vec::with_capacity(statements.len());
    for (index, statement) in statements.iter().enumerate() {
        let spans = split_statements(statement, dialect);
        let [span] = spans.as_slice() else {
            return Err(CmdError::Invalid {
                message: format!(
                    "Statement {} of the plan must be exactly one statement.",
                    index + 1
                ),
            });
        };
        if span.kind == StatementKind::Transaction {
            return Err(CmdError::Invalid {
                message: format!(
                    "Statement {} of the plan controls the transaction; plans may not.",
                    index + 1
                ),
            });
        }
        kinds.push(span.kind);
    }
    Ok(kinds)
}

/// The labels the confirmation dialog shows: the destructive kinds, or
/// every kind when the plan is destructive in a way the classifier cannot
/// see (a column type change that loses data).
pub(crate) fn confirmation_labels(kinds: &[StatementKind]) -> Vec<String> {
    let destructive: Vec<String> = kinds
        .iter()
        .filter(|k| k.is_destructive())
        .map(|k| format!("{k:?}"))
        .collect();
    if destructive.is_empty() {
        let mut all: Vec<String> = kinds.iter().map(|k| format!("{k:?}")).collect();
        all.dedup();
        all
    } else {
        destructive
    }
}

/// The script of a transactional plan. Statements are separated by a line
/// with only `;`, so a statement ending in a line comment stays closed.
pub(crate) fn transaction_script(statements: &[String]) -> String {
    let mut script = String::from("BEGIN\n;\n");
    for statement in statements {
        script.push_str(statement);
        script.push_str("\n;\n");
    }
    script.push_str("COMMIT");
    script
}

/// The activity action for a plan: `matview.refreshed` for refreshes,
/// `object.dropped` when every statement drops, else `schema.changed`.
pub(crate) fn schema_action(statements: &[String], kinds: &[StatementKind]) -> &'static str {
    let refresh = statements.iter().all(|s| {
        s.split_whitespace()
            .take(3)
            .map(str::to_ascii_uppercase)
            .eq(["REFRESH", "MATERIALIZED", "VIEW"])
    });
    if refresh {
        "matview.refreshed"
    } else if kinds.iter().all(|k| *k == StatementKind::Drop) {
        "object.dropped"
    } else {
        "schema.changed"
    }
}

/// Adds where a non-transactional plan stopped to a driver error.
fn stopped_at(mut error: DriverError, index: usize, total: usize) -> DriverError {
    let note = if index == 0 {
        format!("Statement 1 of {total} failed; nothing ran.")
    } else {
        format!(
            "Statement {} of {total} failed; the {index} statement(s) before it ran and are not \
             rolled back.",
            index + 1
        )
    };
    error.detail = Some(match error.detail.take() {
        Some(detail) => format!("{detail}\n{note}"),
        None => note,
    });
    error
}

/// What running a plan inside an open transaction would do, by engine.
pub(crate) fn transaction_effect(engine: Engine) -> String {
    match engine {
        Engine::Postgres => {
            "the change would become part of it and only last if it is committed.".into()
        }
        _ => "MySQL and MariaDB would commit it implicitly before the DDL.".into(),
    }
}

/// The PostgreSQL database an editor change drops or renames: the app's
/// own sessions on it have to close before it runs.
fn database_target(change: &EngineChange) -> Option<String> {
    let EngineChange::Postgres(change) = change else {
        return None;
    };
    let PgChange::Database(change) = change.as_ref() else {
        return None;
    };
    match change {
        DatabaseChange::Drop { name, .. } => Some(name.clone()),
        DatabaseChange::Alter { name, operations }
            if operations.iter().any(|op| {
                matches!(
                    op,
                    DatabaseOperation::Rename { .. } | DatabaseOperation::Tablespace { .. }
                )
            }) =>
        {
            Some(name.clone())
        }
        _ => None,
    }
}

/// The plan of a stored editor plan, and the database it drops or renames.
fn edit_parts(stored: &StoredPlan) -> CmdResult<(&EditPlan, Option<&str>)> {
    match stored {
        StoredPlan::Edit(plan) => Ok((plan, None)),
        StoredPlan::EditDatabase { plan, database } => Ok((plan, Some(database.as_str()))),
        other => Err(wrong_kind("edit_apply", other)),
    }
}

impl Services {
    /// Plans an editor change on this session's server and keeps the plan
    /// on the server; apply it with [`Self::edit_apply`] and the returned
    /// id.
    ///
    /// # Errors
    /// Unknown session, a change for another engine (`invalid`), or the
    /// editor's own errors (`catalog`).
    pub fn edit_plan(
        &self,
        session_id: &str,
        change: &EngineChange,
    ) -> CmdResult<Planned<EditPlan>> {
        let info = self.session_info(session_id)?;
        let plan = match (change, info.engine) {
            (EngineChange::Postgres(change), Engine::Postgres) => {
                varve_catalog::postgres::edit::plan(&info.server, change)?
            }
            (EngineChange::Mysql(change), Engine::Mysql | Engine::Mariadb) => {
                varve_catalog::mysql::edit::plan(&info.server, change)?
            }
            (_, engine) => {
                return Err(CmdError::Invalid {
                    message: format!("This change is not for a {engine:?} session."),
                });
            }
        };
        if let Some(dialect) = sql_dialect(info.engine) {
            plan_kinds(&plan.statements, dialect)?;
        }
        let stored = match database_target(change) {
            Some(database) => StoredPlan::EditDatabase {
                plan: plan.clone(),
                database,
            },
            None => StoredPlan::Edit(plan.clone()),
        };
        let plan_id = self.m2.plans.put(session_id, stored);
        Ok(Planned { plan_id, plan })
    }

    /// Runs a stored editor plan, exactly as planned.
    ///
    /// - Read-only sessions refuse it.
    /// - It needs `confirmed` when the plan is destructive (always), or when
    ///   a statement is destructive and the session confirms destructive
    ///   statements; the check covers every statement before any runs.
    /// - A session with an open transaction refuses it
    ///   (`session` / `openTransaction`); the plan stays available.
    /// - Transactional plans (PostgreSQL only) run as one `BEGIN ... COMMIT`
    ///   script, checked and rolled back on failure under one session lock.
    /// - Other plans run statement by statement and stop at the first
    ///   failure, whose detail says how many statements ran.
    /// - Every statement is logged as a query, and the plan as
    ///   `schema.changed`, `object.dropped` or `matview.refreshed`.
    ///
    /// # Errors
    /// As described, an unknown, used or expired plan id (`notFound`),
    /// plus driver errors.
    pub async fn edit_apply(
        &self,
        session_id: &str,
        plan_id: &str,
        confirmed: bool,
    ) -> CmdResult<EditApplied> {
        let info = self.session_info(session_id)?;
        let stored = self.m2.plans.peek(session_id, plan_id)?;
        let (plan, database) = edit_parts(&stored)?;
        let Some(dialect) = sql_dialect(info.engine) else {
            return Err(CmdError::Invalid {
                message: "Editor plans run on PostgreSQL, MySQL and MariaDB sessions.".into(),
            });
        };
        Self::require_writable(&info, "changing objects")?;
        let kinds = plan_kinds(&plan.statements, dialect)?;
        if plan.transactional && info.engine != Engine::Postgres {
            return Err(CmdError::Invalid {
                message: "MySQL and MariaDB commit DDL implicitly; the plan cannot run in a \
                          transaction."
                    .into(),
            });
        }
        if let Some(database) = database
            && info.database.as_deref() == Some(database)
        {
            return Err(CmdError::Invalid {
                message: format!(
                    "This session is connected to database \"{database}\". PostgreSQL cannot \
                     drop or rename the database a session uses: run it from a session on \
                     another database."
                ),
            });
        }
        let classified = kinds.iter().any(|k| k.is_destructive());
        let labels = confirmation_labels(&kinds);
        self.require_confirmation(
            &info,
            plan.destructive || (info.confirm_destructive && classified),
            confirmed,
            labels.clone(),
        )?;
        let stored = self.m2.plans.take(session_id, plan_id)?;
        let (plan, database) = edit_parts(&stored)?;
        if let Some(database) = database {
            // The browser's own sessions on the database would make
            // PostgreSQL refuse; they reopen on their next use.
            self.release_database_sessions(&info.connection_id, database)
                .await;
        }

        let started = Instant::now();
        let forced = plan.destructive.then(|| labels.clone());
        let effect = transaction_effect(info.engine);
        let result = if plan.transactional {
            let spec = ExecSpec {
                destructive: forced,
                outside_transaction: Some(effect),
                rollback_on_error: true,
                overrides: None,
            };
            self.sessions
                .execute_with(
                    &info.id,
                    &transaction_script(&plan.statements),
                    confirmed,
                    &ExecOptions::default(),
                    &spec,
                )
                .await
                .map(|output| vec![output])
                .map_err(|error| (error, 0))
        } else {
            self.run_statements(&info, &plan.statements, confirmed, forced, Some(&effect))
                .await
                .map_err(|stopped| *stopped)
        };
        let duration_ms = elapsed_ms(started);
        let result = match result {
            Ok(outputs) => Ok(outputs),
            Err((error @ SessionError::OpenTransaction { .. }, 0)) => {
                // Nothing ran: the user can commit and apply the same plan.
                self.restore_plan(session_id, plan_id, stored.clone());
                return Err(error.into());
            }
            Err((SessionError::Driver { error }, index)) if !plan.transactional => {
                Err(CmdError::from(SessionError::Driver {
                    error: stopped_at(error, index, plan.statements.len()),
                }))
            }
            Err((other, _)) => Err(CmdError::from(other)),
        };

        let mut event =
            ActivityEvent::builder(Category::Schema, schema_action(&plan.statements, &kinds))
                .detail("statements", plan.statements.len())
                .detail(
                    "kinds",
                    kinds
                        .iter()
                        .map(|k| format!("{k:?}").to_ascii_lowercase())
                        .collect::<Vec<_>>(),
                )
                .detail("transactional", plan.transactional)
                .detail("destructive", plan.destructive)
                .duration_ms(duration_ms)
                .statement(plan.statements.join(";\n"), SqlDialect::from(info.engine));
        if let Err(CmdError::Session {
            error: SessionError::Driver { error },
        }) = &result
        {
            event = event
                .outcome(EventOutcome::Failure)
                .error(&error_details(error), Some(SqlDialect::from(info.engine)));
        } else if result.is_err() {
            event = event.outcome(EventOutcome::Failure);
        }
        self.record(&info, event);
        let outputs = result?;
        Ok(EditApplied {
            statements: plan.statements.len(),
            transactional: plan.transactional,
            outputs,
            duration_ms,
        })
    }

    /// Runs statements one at a time through the session manager, each
    /// refused inside an open transaction when `effect` says why. `forced` labels make the first
    /// statement count as confirmed-destructive (the caller checked the
    /// confirmation for the whole list). Returns the failing index with
    /// the error (boxed: a driver error can carry a partial output).
    pub(crate) async fn run_statements(
        &self,
        info: &SessionInfo,
        statements: &[String],
        confirmed: bool,
        forced: Option<Vec<String>>,
        effect: Option<&str>,
    ) -> Result<Vec<ExecOutput>, Box<(SessionError, usize)>> {
        let mut outputs = Vec::with_capacity(statements.len());
        for (index, statement) in statements.iter().enumerate() {
            let spec = ExecSpec {
                destructive: if index == 0 { forced.clone() } else { None },
                outside_transaction: effect.map(str::to_string),
                rollback_on_error: false,
                overrides: None,
            };
            match self
                .sessions
                .execute_with(
                    &info.id,
                    statement,
                    confirmed,
                    &ExecOptions::default(),
                    &spec,
                )
                .await
            {
                Ok(output) => outputs.push(output),
                Err(error) => return Err(Box::new((error, index))),
            }
        }
        Ok(outputs)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn changes_are_tagged_by_engine() -> Result<(), serde_json::Error> {
        let change: EngineChange = serde_json::from_value(json!({
            "engine": "mysql",
            "change": { "change": "dropDatabase", "name": "m2_ipc_x", "ifExists": true }
        }))?;
        assert!(matches!(change, EngineChange::Mysql(_)));
        let back = serde_json::to_value(&change)?;
        assert_eq!(back["engine"], "mysql");
        assert_eq!(back["change"]["change"], "dropDatabase");
        assert!(
            serde_json::from_value::<EngineChange>(json!({ "engine": "redis", "change": {} }))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn plans_are_checked_statement_by_statement() {
        let ok = plan_kinds(
            &["CREATE TABLE t (a int)".into(), "DROP TABLE u".into()],
            Dialect::Postgres,
        );
        assert!(matches!(ok, Ok(ref k) if k == &[StatementKind::Ddl, StatementKind::Drop]));
        assert!(plan_kinds(&[], Dialect::Postgres).is_err());
        assert!(plan_kinds(&["SELECT 1; SELECT 2".into()], Dialect::Postgres).is_err());
        assert!(plan_kinds(&["COMMIT".into()], Dialect::Mysql).is_err());
        let body = "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END".to_string();
        assert!(plan_kinds(&[body], Dialect::Mysql).is_ok());
    }

    #[test]
    fn transaction_scripts_survive_trailing_comments() {
        let script = transaction_script(&["CREATE VIEW v AS SELECT 1 -- note".into()]);
        assert_eq!(
            script,
            "BEGIN\n;\nCREATE VIEW v AS SELECT 1 -- note\n;\nCOMMIT"
        );
        assert_eq!(split_statements(&script, Dialect::Postgres).len(), 3);
    }

    #[test]
    fn actions_and_labels() {
        let refresh = vec!["refresh materialized view concurrently s.m".to_string()];
        assert_eq!(
            schema_action(&refresh, &[StatementKind::Other]),
            "matview.refreshed"
        );
        let drops = vec!["DROP TABLE a".to_string()];
        assert_eq!(
            schema_action(&drops, &[StatementKind::Drop]),
            "object.dropped"
        );
        let alter = vec!["ALTER TABLE a ADD b int".to_string()];
        assert_eq!(
            schema_action(&alter, &[StatementKind::Ddl]),
            "schema.changed"
        );
        assert_eq!(
            confirmation_labels(&[StatementKind::Ddl, StatementKind::Drop]),
            ["Drop"]
        );
        assert_eq!(confirmation_labels(&[StatementKind::Ddl]), ["Ddl"]);
    }

    #[test]
    fn stopped_plans_say_how_far_they_got() {
        let error = stopped_at(
            DriverError::new(varve_driver::ErrorKind::Query, "boom"),
            2,
            3,
        );
        assert!(
            error
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("Statement 3 of 3"))
        );
    }
}
