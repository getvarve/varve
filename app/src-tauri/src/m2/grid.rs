//! Grid data editing (design doc 4.2): the key that makes a table
//! editable, the SQL for a set of grid changes, and applying it in one
//! transaction.

use std::time::Instant;

use varve_activity::{ActivityEvent, Category, ErrorDetails, Outcome as EventOutcome, RowChanges};
use varve_catalog::data_edit::{
    ApplyOutcome, ChangeOp, KeyInfo, PlanOptions, RowChange, RowEditPlan, TableTarget,
};
use varve_core::Engine;
use varve_driver::{Dialect, StatementKind, split_statements};

use super::editors::transaction_effect;
use super::plans::{Planned, StoredPlan, wrong_kind};
use super::{elapsed_ms, environment_label, sql_dialect};
use crate::commands::{CmdError, Services};
use crate::sessions::{SessionError, open_transaction};

type CmdResult<T> = Result<T, CmdError>;

const SQL_ENGINES: [Engine; 3] = [Engine::Postgres, Engine::Mysql, Engine::Mariadb];

/// The kind of the one statement in `sql`, or `None` when it is not
/// exactly one statement.
fn single_kind(sql: &str, dialect: Dialect) -> Option<StatementKind> {
    match split_statements(sql, dialect).as_slice() {
        [span] => Some(span.kind),
        _ => None,
    }
}

/// Checks that a plan coming back from the UI only holds what the planner
/// writes: one `INSERT`, `UPDATE` or `DELETE` per change matching its
/// operation (with a `WHERE` for updates and deletes), and `SELECT`s for
/// row locks and read-backs. Anything else would bypass the confirmation
/// and logging rules of the SQL editor.
///
/// # Errors
/// [`CmdError::Invalid`] naming the first change that does not fit.
pub fn validate_row_plan(plan: &RowEditPlan, engine: Engine) -> CmdResult<()> {
    let Some(dialect) = sql_dialect(engine) else {
        return Err(CmdError::Invalid {
            message: "Grid editing runs on PostgreSQL, MySQL and MariaDB sessions.".into(),
        });
    };
    if plan.engine != engine {
        return Err(CmdError::Invalid {
            message: "The grid changes were planned for another engine than this session's.".into(),
        });
    }
    for (index, change) in plan.changes.iter().enumerate() {
        let invalid = || CmdError::Invalid {
            message: format!(
                "Change {} of the plan is not a grid change; plan the changes again.",
                index + 1
            ),
        };
        let expected = match change.op {
            ChangeOp::Insert => StatementKind::Insert,
            ChangeOp::Update => StatementKind::Update,
            ChangeOp::Delete => StatementKind::Delete,
        };
        if single_kind(&change.statement, dialect) != Some(expected) {
            return Err(invalid());
        }
        for select in change.lock.iter().chain(change.refresh.iter()) {
            if single_kind(select, dialect) != Some(StatementKind::Select) {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

/// Rows inserted, updated and deleted by a plan.
pub(crate) fn planned_counts(plan: &RowEditPlan) -> RowChanges {
    let mut counts = RowChanges::default();
    for change in &plan.changes {
        match change.op {
            ChangeOp::Insert => counts.inserted += 1,
            ChangeOp::Update => counts.updated += 1,
            ChangeOp::Delete => counts.deleted += 1,
        }
    }
    counts
}

/// The table as logged: `database.schema.table` or `database.table`.
pub(crate) fn table_label(target: &TableTarget) -> String {
    match &target.schema {
        Some(schema) => format!("{}.{schema}.{}", target.database, target.table),
        None => format!("{}.{}", target.database, target.table),
    }
}

impl Services {
    /// The key that makes a table editable and its columns; `None` keeps
    /// the grid read only.
    ///
    /// # Errors
    /// Unknown session, a Redis session, catalog errors.
    pub async fn editable_key(
        &self,
        session_id: &str,
        target: &TableTarget,
    ) -> CmdResult<Option<KeyInfo>> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "Grid editing")?;
        let lock = self.lock(session_id).await?;
        let session = Self::locked(&lock, session_id)?;
        Ok(varve_catalog::data_edit::editable_key(session, &info.server, target).await?)
    }

    /// The SQL for a set of grid changes, kept on the server with its
    /// table; the preview shows it and [`Self::row_edit_apply`] runs it by
    /// id.
    ///
    /// # Errors
    /// Unknown session, a Redis session, planner errors (`catalog`).
    pub fn row_edit_plan(
        &self,
        session_id: &str,
        target: &TableTarget,
        changes: &[RowChange],
        options: &PlanOptions,
    ) -> CmdResult<Planned<RowEditPlan>> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "Grid editing")?;
        let plan = varve_catalog::data_edit::plan_edits(&info.server, target, changes, options)?;
        validate_row_plan(&plan, info.engine)?;
        let plan_id = self.m2.plans.put(
            session_id,
            StoredPlan::Rows {
                target: target.clone(),
                plan: plan.clone(),
            },
        );
        Ok(Planned { plan_id, plan })
    }

    /// Applies grid changes in one transaction.
    ///
    /// Read-only sessions refuse it; deleting rows needs `confirmed` when
    /// the session confirms destructive statements (production, design doc
    /// 3.2). The commit is logged as `data.edited` with the table and the
    /// row counts, never cell values or statement text; a conflict or a
    /// rejected change is logged as a failed `data.edited` with nothing
    /// changed.
    ///
    /// # Errors
    /// Unknown session, read-only session, confirmation needed, a plan that
    /// is not a grid plan (`invalid`), catalog errors (an open transaction,
    /// a lost connection).
    pub async fn row_edit_apply(
        &self,
        session_id: &str,
        plan_id: &str,
        confirmed: bool,
    ) -> CmdResult<ApplyOutcome> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "Grid editing")?;
        Self::require_writable(&info, "editing rows")?;
        let stored = self.m2.plans.peek(session_id, plan_id)?;
        let StoredPlan::Rows { plan, .. } = &stored else {
            return Err(wrong_kind("row_edit_apply", &stored));
        };
        validate_row_plan(plan, info.engine)?;
        let counts = planned_counts(plan);
        self.require_confirmation(
            &info,
            info.confirm_destructive && counts.deleted > 0,
            confirmed,
            vec!["Delete".into()],
        )?;
        // Lock first, then take the plan: a lost session (no lock) or a
        // failed check below leaves the plan in place, so nothing is spent
        // unless the changes actually run. A second apply of the same plan
        // waits for the lock and then finds the plan gone.
        let lock = self.lock(session_id).await?;
        let session = Self::locked(&lock, session_id)?;
        let stored = self.m2.plans.take(session_id, plan_id)?;
        let StoredPlan::Rows { target, plan } = &stored else {
            let error = wrong_kind("row_edit_apply", &stored);
            drop(lock);
            self.restore_plan(session_id, plan_id, stored.clone());
            return Err(error);
        };

        // The grid commits its own transaction: inside the user's open one
        // (or an aborted PostgreSQL one) MySQL would commit the user's work
        // implicitly and PostgreSQL would refuse. Checked under the lock.
        let open = match open_transaction(info.engine, session).await {
            Ok(open) => open,
            Err(error) => {
                drop(lock);
                self.restore_plan(session_id, plan_id, stored.clone());
                return Err(SessionError::Driver { error }.into());
            }
        };
        if open == Some(true) {
            drop(lock);
            self.restore_plan(session_id, plan_id, stored.clone());
            self.record(
                &info,
                ActivityEvent::builder(Category::Safety, "policy.blocked")
                    .outcome(EventOutcome::Cancelled)
                    .detail("policy", "open_transaction")
                    .detail("kind", "grid"),
            );
            return Err(SessionError::OpenTransaction {
                message: format!(
                    "The session has an open transaction; saving grid changes commits their own \
                     transaction, so {} Commit or roll it back first.",
                    transaction_effect(info.engine)
                ),
            }
            .into());
        }
        let started = Instant::now();
        let result = varve_catalog::data_edit::apply(session, &info.server, plan).await;
        drop(lock);
        let duration_ms = elapsed_ms(started);
        if let Err(varve_catalog::CatalogError::Invalid { .. }) = &result {
            // Refused before anything ran (an open transaction): the same
            // plan can be applied once the user committed.
            self.restore_plan(session_id, plan_id, stored.clone());
        }

        let table = table_label(target);
        let event = match &result {
            Ok(ApplyOutcome::Applied { .. }) => {
                ActivityEvent::data_edited(&table, counts).duration_ms(duration_ms)
            }
            Ok(ApplyOutcome::Conflict { change, reason, .. }) => {
                ActivityEvent::data_edited(&table, RowChanges::default())
                    .outcome(EventOutcome::Failure)
                    .duration_ms(duration_ms)
                    .detail("status", "conflict")
                    .detail("change", *change)
                    .detail(
                        "reason",
                        serde_json::to_value(reason).unwrap_or(serde_json::Value::Null),
                    )
            }
            Ok(ApplyOutcome::Failed { change, error }) => {
                // The server message can quote a cell value; keep the code.
                ActivityEvent::data_edited(&table, RowChanges::default())
                    .outcome(EventOutcome::Failure)
                    .duration_ms(duration_ms)
                    .detail("status", "failed")
                    .detail("change", *change)
                    .error(
                        &ErrorDetails {
                            code: error.code.clone(),
                            message: format!("the server rejected change {}", change + 1),
                        },
                        None,
                    )
            }
            Err(error) => ActivityEvent::data_edited(&table, RowChanges::default())
                .outcome(EventOutcome::Failure)
                .duration_ms(duration_ms)
                .detail("status", "error")
                .error(
                    &ErrorDetails {
                        code: match error {
                            varve_catalog::CatalogError::Driver { error } => error.code.clone(),
                            _ => None,
                        },
                        message: "the grid changes were not applied".into(),
                    },
                    None,
                ),
        };
        self.record(&info, event);
        if confirmed && counts.deleted > 0 && matches!(result, Ok(ApplyOutcome::Applied { .. })) {
            self.record(
                &info,
                ActivityEvent::destructive_confirmed("Delete", &environment_label(&info)),
            );
        }
        Ok(result?)
    }
}

#[cfg(test)]
mod tests {
    use varve_catalog::data_edit::PlannedChange;

    use super::*;

    fn change(op: ChangeOp, statement: &str, lock: Option<&str>) -> PlannedChange {
        PlannedChange {
            change: 0,
            op,
            lock: lock.map(Into::into),
            statement: statement.into(),
            returns_row: false,
            refresh: None,
            checks_original: false,
        }
    }

    fn plan(engine: Engine, changes: Vec<PlannedChange>) -> RowEditPlan {
        RowEditPlan {
            engine,
            changes,
            warnings: vec![],
            has_expressions: false,
        }
    }

    #[test]
    fn grid_plans_hold_only_grid_statements() {
        let good = plan(
            Engine::Postgres,
            vec![
                change(
                    ChangeOp::Insert,
                    "INSERT INTO s.t (a) VALUES (1) RETURNING *",
                    None,
                ),
                change(ChangeOp::Delete, "DELETE FROM s.t WHERE id = 1", None),
            ],
        );
        assert!(validate_row_plan(&good, Engine::Postgres).is_ok());
        assert_eq!(
            planned_counts(&good),
            RowChanges {
                inserted: 1,
                updated: 0,
                deleted: 1
            }
        );
        assert!(validate_row_plan(&good, Engine::Mysql).is_err());

        let wrong_op = plan(
            Engine::Postgres,
            vec![change(ChangeOp::Update, "DROP TABLE s.t", None)],
        );
        assert!(validate_row_plan(&wrong_op, Engine::Postgres).is_err());
        let delete_all = plan(
            Engine::Postgres,
            vec![change(ChangeOp::Delete, "DELETE FROM s.t", None)],
        );
        assert!(validate_row_plan(&delete_all, Engine::Postgres).is_err());
        let two = plan(
            Engine::Mysql,
            vec![change(
                ChangeOp::Update,
                "UPDATE t SET a = 1 WHERE id = 1; DROP TABLE t",
                None,
            )],
        );
        assert!(validate_row_plan(&two, Engine::Mysql).is_err());
        let bad_lock = plan(
            Engine::Mysql,
            vec![change(
                ChangeOp::Update,
                "UPDATE t SET a = 1 WHERE id = 1",
                Some("DELETE FROM t WHERE id = 2"),
            )],
        );
        assert!(validate_row_plan(&bad_lock, Engine::Mysql).is_err());
        let locked = plan(
            Engine::Mysql,
            vec![change(
                ChangeOp::Update,
                "UPDATE t SET a = 1 WHERE id = 1",
                Some("SELECT * FROM t WHERE id = 1 FOR UPDATE"),
            )],
        );
        assert!(validate_row_plan(&locked, Engine::Mysql).is_ok());
    }

    #[test]
    fn tables_are_labelled_with_their_schema() {
        let target = TableTarget {
            database: "d".into(),
            schema: Some("s".into()),
            table: "t".into(),
        };
        assert_eq!(table_label(&target), "d.s.t");
    }
}
