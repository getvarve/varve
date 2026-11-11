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
use varve_catalog::postgres::edit::{
    DatabaseChange, DatabaseOperation, MaterializedViewChange, PgChange,
};
use varve_core::Engine;
#[cfg(test)]
use varve_driver::classify;
use varve_driver::{
    Dialect, DriverError, ExecOptions, ExecOutput, StatementKind, split_statements,
};

use super::plans::{EditPlanView, Planned, StoredPlan, wrong_kind};
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
    /// Statements of the plan that ran, counted as the user reads them
    /// ([`EditPlan::shown`]).
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

/// The statement kinds of a plan as the user reads it
/// ([`EditPlan::shown`]), one per shown statement: what the destructive
/// check, the confirmation labels and the activity log go by. What runs is
/// checked with [`plan_kinds`].
///
/// A shown statement is classified by its first statement only. The
/// readable form of a statement built at run time quotes its texts with
/// doubled quotes, so a text with a backslash can read as an open string
/// and a `;` in a later text as a statement end; the kind comes from the
/// leading `ALTER` or `CREATE`, which that cannot change.
///
/// # Errors
/// [`CmdError::Invalid`] for a shown statement that holds no statement.
pub fn shown_kinds(shown: &[String], dialect: Dialect) -> Result<Vec<StatementKind>, CmdError> {
    shown
        .iter()
        .enumerate()
        .map(|(index, statement)| {
            split_statements(statement, dialect)
                .first()
                .map(|span| span.kind)
                .ok_or_else(|| CmdError::Invalid {
                    message: format!("Statement {} of the plan is empty.", index + 1),
                })
        })
        .collect()
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

/// Adds where a non-transactional plan stopped to a driver error. `index`
/// is the failed statement of `plan.statements`; the note counts the
/// statements the user reads ([`EditPlan::shown`]).
fn stopped_at(mut error: DriverError, plan: &EditPlan, index: usize) -> DriverError {
    let total = plan.shown().len();
    let (index, ran) = plan.shown_position(index);
    let note = if ran {
        // The clean-up after a statement built at run time failed: the
        // statement itself ran.
        format!(
            "Statement {} of {total} ran, but cleaning up after it failed; the {} statement(s) \
             up to it ran and are not rolled back.",
            index + 1,
            index + 1
        )
    } else if index == 0 {
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

/// The materialized view an editor change refreshes, as `schema.name`
/// (the form other activity targets take), or `name` when the change named
/// no schema.
fn refresh_target(change: &EngineChange) -> Option<String> {
    let EngineChange::Postgres(change) = change else {
        return None;
    };
    let PgChange::MaterializedView(MaterializedViewChange::Refresh { name, .. }) = change.as_ref()
    else {
        return None;
    };
    Some(match &name.schema {
        Some(schema) => format!("{schema}.{}", name.name),
        None => name.name.clone(),
    })
}

/// The parts of a stored editor plan.
struct EditParts<'a> {
    /// What runs.
    plan: &'a EditPlan,
    /// The PostgreSQL database it drops or renames.
    database: Option<&'a str>,
    /// The materialized view it refreshes, for the activity event.
    target: Option<&'a str>,
}

/// The plan of a stored editor plan, the database it drops or renames and
/// the view it refreshes.
fn edit_parts(stored: &StoredPlan) -> CmdResult<EditParts<'_>> {
    let parts = |plan, database, target| EditParts {
        plan,
        database,
        target,
    };
    match stored {
        StoredPlan::Edit(plan) => Ok(parts(plan, None, None)),
        StoredPlan::EditDatabase { plan, database } => {
            Ok(parts(plan, Some(database.as_str()), None))
        }
        StoredPlan::EditRefresh { plan, target } => Ok(parts(plan, None, Some(target.as_str()))),
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
    ) -> CmdResult<Planned<EditPlanView>> {
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
            shown_kinds(&plan.shown(), dialect)?;
        }
        let stored = match (database_target(change), refresh_target(change)) {
            (Some(database), _) => StoredPlan::EditDatabase {
                plan: plan.clone(),
                database,
            },
            (None, Some(target)) => StoredPlan::EditRefresh {
                plan: plan.clone(),
                target,
            },
            (None, None) => StoredPlan::Edit(plan.clone()),
        };
        let plan_id = self.m2.plans.put(session_id, stored);
        Ok(Planned {
            plan_id,
            plan: plan.into(),
        })
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
    ///   failure, whose detail says how many statements ran (counted as
    ///   the user reads them). After a failure, what a MySQL statement
    ///   built at run time leaves in the session is removed.
    /// - The destructive check, the confirmation labels and the activity
    ///   event go by the statements as the user reads them
    ///   ([`EditPlan::shown`]); what runs is the plan's statements.
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
        let EditParts { plan, database, .. } = edit_parts(&stored)?;
        let Some(dialect) = sql_dialect(info.engine) else {
            return Err(CmdError::Invalid {
                message: "Editor plans run on PostgreSQL, MySQL and MariaDB sessions.".into(),
            });
        };
        Self::require_writable(&info, "changing objects")?;
        // What runs is checked as it runs; what it does is read from the
        // statements the user sees.
        plan_kinds(&plan.statements, dialect)?;
        let shown = plan.shown();
        let kinds = shown_kinds(&shown, dialect)?;
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
        let EditParts {
            plan,
            database,
            target,
        } = edit_parts(&stored)?;
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
            let ran = self
                .run_statements(&info, &plan.statements, confirmed, forced, Some(&effect))
                .await
                .map_err(|stopped| *stopped);
            if let Err((SessionError::Driver { .. }, _)) = &ran {
                self.clean_up_after_failure(&info, plan).await;
            }
            ran
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
                    error: stopped_at(error, plan, index),
                }))
            }
            Err((other, _)) => Err(CmdError::from(other)),
        };

        let mut event = ActivityEvent::builder(Category::Schema, schema_action(&shown, &kinds))
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
        if let Some(target) = target {
            event = event.target(target);
        }
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
            statements: shown.len(),
            transactional: plan.transactional,
            outputs,
            duration_ms,
        })
    }

    /// Removes what a failed statement of `plan` can leave in the session
    /// ([`EditPlan::cleanup_after_failure`]: the prepared statement and the
    /// user variable of a MySQL statement built at run time). Nothing runs
    /// for a plan without such a statement. Errors are ignored: each
    /// statement fails when there is nothing to remove, and a session that
    /// is gone holds nothing.
    async fn clean_up_after_failure(&self, info: &SessionInfo, plan: &EditPlan) {
        for statement in plan.cleanup_after_failure() {
            let _ignored = self
                .sessions
                .execute_with(
                    &info.id,
                    &statement,
                    false,
                    &ExecOptions::default(),
                    &ExecSpec::default(),
                )
                .await;
        }
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

    /// A refresh names its view as `schema.name`; other changes name none.
    #[test]
    fn refreshes_name_their_view() {
        use varve_catalog::postgres::edit::QualifiedName;

        let refresh = |name: QualifiedName| {
            EngineChange::Postgres(Box::new(PgChange::MaterializedView(
                MaterializedViewChange::Refresh {
                    name,
                    concurrently: false,
                    has_unique_index: false,
                    with_data: true,
                },
            )))
        };
        assert_eq!(
            refresh_target(&refresh(QualifiedName::new("sales", "daily totals"))),
            Some("sales.daily totals".to_owned())
        );
        assert_eq!(
            refresh_target(&refresh(QualifiedName {
                schema: None,
                name: "m".into(),
            })),
            Some("m".to_owned())
        );
        let drop = EngineChange::Postgres(Box::new(PgChange::Database(DatabaseChange::Drop {
            name: "d".into(),
            if_exists: false,
            force: false,
        })));
        assert_eq!(refresh_target(&drop), None);
        let plan = EditPlan {
            statements: vec!["REFRESH MATERIALIZED VIEW \"sales\".\"m\"".into()],
            transactional: true,
            destructive: false,
            warnings: vec![],
        };
        let stored = StoredPlan::EditRefresh {
            plan,
            target: "sales.m".into(),
        };
        assert!(matches!(
            edit_parts(&stored),
            Ok(EditParts {
                target: Some("sales.m"),
                database: None,
                ..
            })
        ));
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

    fn plan_of(statements: Vec<String>) -> EditPlan {
        EditPlan {
            statements,
            transactional: false,
            destructive: false,
            warnings: vec![],
        }
    }

    /// The five statements MySQL runs for a statement built at run time,
    /// shown as `shown`.
    fn prepared(shown: &str) -> Vec<String> {
        vec![
            "SET @varve_ddl = CONCAT(X'414C544552')".into(),
            "PREPARE varve_ddl FROM @varve_ddl".into(),
            "SET @varve_ddl = NULL".into(),
            format!("EXECUTE varve_ddl /* {shown} */"),
            "DEALLOCATE PREPARE varve_ddl".into(),
        ]
    }

    fn stopped(plan: &EditPlan, index: usize) -> String {
        stopped_at(
            DriverError::new(varve_driver::ErrorKind::Query, "boom"),
            plan,
            index,
        )
        .detail
        .unwrap_or_default()
    }

    #[test]
    fn stopped_plans_say_how_far_they_got() {
        let plain = plan_of(vec![
            "CREATE TABLE a (x int)".into(),
            "CREATE TABLE b (x int)".into(),
            "CREATE TABLE c (x int)".into(),
        ]);
        assert!(stopped(&plain, 0).contains("Statement 1 of 3 failed; nothing ran."));
        assert!(stopped(&plain, 2).contains("Statement 3 of 3 failed; the 2 statement(s) before"));
    }

    /// A statement built at run time is one statement for the user: the
    /// note counts shown statements, whichever of its steps failed.
    #[test]
    fn stopped_plans_count_the_statements_the_user_reads() {
        let mut statements = vec!["ALTER TABLE `t` ADD COLUMN `a` int".to_string()];
        statements.extend(prepared("ALTER TABLE `t` COMMENT='a\\b'"));
        statements.push("DROP TABLE `old`".into());
        let plan = plan_of(statements);
        // SET, PREPARE, the clearing SET and EXECUTE: statement 2 failed.
        for index in 1..=4 {
            let note = stopped(&plan, index);
            assert!(
                note.contains("Statement 2 of 3 failed; the 1 statement(s) before"),
                "{index}: {note}"
            );
        }
        // DEALLOCATE PREPARE: statement 2 ran.
        let note = stopped(&plan, 5);
        assert!(
            note.contains("Statement 2 of 3 ran, but cleaning up after it failed; the 2"),
            "{note}"
        );
        let note = stopped(&plan, 6);
        assert!(
            note.contains("Statement 3 of 3 failed; the 2 statement(s) before"),
            "{note}"
        );
        let first = plan_of(prepared("CREATE TABLE `t` (`a` int COMMENT 'a\\b')"));
        assert!(stopped(&first, 3).contains("Statement 1 of 1 failed; nothing ran."));
    }

    /// What a plan does is read from the statements the user sees; what
    /// runs is still checked statement by statement.
    #[test]
    fn run_time_statements_are_classified_as_shown() -> Result<(), CmdError> {
        let mut statements = prepared("ALTER TABLE `t` DROP COLUMN `a`, COMMENT='a\\b'");
        statements.push("DROP TABLE `old`".into());
        let plan = plan_of(statements);
        assert_eq!(plan_kinds(&plan.statements, Dialect::Mysql)?.len(), 6);
        let shown = plan.shown();
        let kinds = shown_kinds(&shown, Dialect::Mysql)?;
        assert_eq!(
            kinds,
            classify(
                "ALTER TABLE `t` DROP COLUMN `a`; DROP TABLE `old`",
                Dialect::Mysql
            )
        );
        assert!(kinds.iter().all(|kind| kind.is_destructive()));
        assert_eq!(schema_action(&shown, &kinds), "schema.changed");
        // The internal statements alone name no destructive kind.
        let internal = plan_kinds(&plan.statements[..5], Dialect::Mysql)?;
        assert!(!internal.iter().any(|kind| kind.is_destructive()));

        // A readable form whose quoting reads as several statements (a
        // backslash before the closing quote, a `;` in a later text) keeps
        // the kind of its leading keyword.
        let odd = plan_of(prepared(
            "ALTER TABLE `t` MODIFY `a` int COMMENT 'x\\', MODIFY `b` int COMMENT 'y;z'",
        ));
        assert_eq!(
            shown_kinds(&odd.shown(), Dialect::Mysql)?,
            [StatementKind::Ddl]
        );
        assert!(shown_kinds(&[String::new()], Dialect::Mysql).is_err());
        Ok(())
    }

    /// The view the UI gets holds the readable statements next to the ones
    /// that run, always.
    #[test]
    fn planned_edits_carry_the_shown_statements() -> Result<(), serde_json::Error> {
        let mut statements = prepared("ALTER TABLE `t` COMMENT='a\\b'");
        statements.push("DROP TABLE `old`".into());
        let view = EditPlanView::from(plan_of(statements));
        let json = serde_json::to_value(&view)?;
        assert_eq!(json["statements"].as_array().map(Vec::len), Some(6));
        assert_eq!(
            json["shown"],
            json!(["ALTER TABLE `t` COMMENT='a\\b'", "DROP TABLE `old`"])
        );
        assert_eq!(json["transactional"], false);
        let plain =
            serde_json::to_value(EditPlanView::from(plan_of(vec!["DROP TABLE `old`".into()])))?;
        assert_eq!(plain["shown"], plain["statements"]);
        Ok(())
    }
}
