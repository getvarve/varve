//! `EXPLAIN` and `EXPLAIN ANALYZE` (design doc 4.2): the options a server
//! offers and the parsed plan tree.

use std::time::Instant;

use varve_activity::{ActivityEvent, RowCounts, SqlDialect};
use varve_catalog::CatalogError;
use varve_catalog::explain::{
    AnalyzeSafety, ExplainCapabilities, ExplainOptions, Plan, analyze_safety, explain_capabilities,
    explain_with_cancel,
};
use varve_core::Engine;
use varve_driver::{ErrorKind, split_statements};

use super::{elapsed_ms, environment_label, error_details, sql_dialect};
use crate::commands::{CmdError, Services};

type CmdResult<T> = Result<T, CmdError>;

const SQL_ENGINES: [Engine; 3] = [Engine::Postgres, Engine::Mysql, Engine::Mariadb];

impl Services {
    /// The `EXPLAIN` options this session's server offers.
    ///
    /// # Errors
    /// Unknown session, or a Redis session.
    pub fn explain_capabilities(&self, session_id: &str) -> CmdResult<ExplainCapabilities> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "EXPLAIN")?;
        Ok(explain_capabilities(&info.server))
    }

    /// Explains one statement and returns the plan tree.
    ///
    /// Without `options.analyze` nothing runs. With it, the catalog first
    /// tries to prove the statement free of side effects and then runs it
    /// in a read-only transaction that is rolled back (no writable session
    /// needed). Anything it cannot prove (a change of data, functions a
    /// rollback may not undo, a run inside the session's own transaction,
    /// ...) comes back as a typed "needs confirmation": the app then asks
    /// (`confirmationRequired` listing every reason) unless `confirmed`,
    /// requires a writable session, and runs it confirmed inside a
    /// transaction (or savepoint) that is rolled back. An up-front
    /// `confirmed` never turns a provable read into a writable run.
    /// `options.confirmed` from the UI is ignored: only `confirmed` counts,
    /// and only after the catalog asked. Statements `ANALYZE` must not run
    /// are refused. Every run is logged as a query of kind `explain` or
    /// `explain_analyze`, a confirmed one also as `destructive.confirmed`.
    /// A read-only session refuses what needs confirmation with the
    /// catalog's reasons (inside an open transaction: that the run cannot
    /// be made read-only there). The tab's Cancel (`cancel`) stops the
    /// whole explain, every step of it, including one still waiting for
    /// the session; it then fails as cancelled and is logged as
    /// `query.cancelled`.
    ///
    /// # Errors
    /// Unknown session, read-only session (only for statements that need
    /// confirmation), confirmation needed, catalog errors (`invalid` for
    /// refused statements, `unsupported` options).
    pub async fn explain(
        &self,
        session_id: &str,
        statement: &str,
        options: &ExplainOptions,
        confirmed: bool,
    ) -> CmdResult<Plan> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "EXPLAIN")?;
        let kind = if options.analyze {
            "explain_analyze"
        } else {
            "explain"
        };
        let dialect = SqlDialect::from(info.engine);
        let unconfirmed = ExplainOptions {
            confirmed: false,
            ..*options
        };
        // Registered before waiting for the session: the tab's Cancel stops
        // the whole explain (every step), not only the statement that runs.
        let run = self.m2.start_explain(session_id);
        let lock = self.lock(session_id).await?;
        let session = Self::locked(&lock, session_id)?;
        let started = Instant::now();
        // One call: the catalog checks (and, when proven safe, runs
        // read-only); only a typed "needs confirmation" leads to a second,
        // confirmed call.
        let mut result =
            explain_with_cancel(session, &info.server, statement, &unconfirmed, &run.cancel).await;
        let mut confirmed_kind = None;
        if let Err(CatalogError::NeedsConfirmation { warnings, .. }) = &result {
            // A read-only session cannot resolve the statement's names on
            // the server (no temporary view in a read-only transaction);
            // when that is the only reason, the user confirms the read-only
            // run instead of being refused.
            let only_connection = warnings
                .iter()
                .all(|w| w == varve_catalog::explain::READ_ONLY_CONNECTION_REASON);
            if info.read_only && !only_connection {
                return Err(CmdError::ReadOnly {
                    message: read_only_refusal(
                        &analyze_safety(&info.server, statement),
                        warnings,
                        session.in_transaction(),
                    ),
                });
            }
            let statement_kind = sql_dialect(info.engine)
                .and_then(|d| split_statements(statement, d).first().map(|s| s.kind))
                .map_or_else(|| "Statement".to_string(), |k| format!("{k:?}"));
            // The dialog lists every reason the catalog gave (data changes,
            // functions a rollback may not undo, the session's open
            // transaction, ...).
            let mut labels = vec![format!(
                "EXPLAIN ANALYZE runs this {statement_kind} statement for real, inside a \
                 transaction that is rolled back afterwards."
            )];
            labels.extend(warnings.iter().cloned());
            self.require_confirmation(&info, true, confirmed, labels)?;
            let agreed = ExplainOptions {
                confirmed: true,
                ..*options
            };
            result =
                explain_with_cancel(session, &info.server, statement, &agreed, &run.cancel).await;
            confirmed_kind = Some(statement_kind);
        }
        drop(lock);
        drop(run);
        let duration_ms = elapsed_ms(started);
        match &result {
            Ok(_) => self.record(
                &info,
                ActivityEvent::query_executed(
                    statement,
                    dialect,
                    kind,
                    RowCounts::default(),
                    duration_ms,
                ),
            ),
            Err(CatalogError::Driver { error }) if error.kind == ErrorKind::Cancelled => self
                .record(
                    &info,
                    ActivityEvent::query_cancelled(statement, dialect, kind, duration_ms),
                ),
            Err(CatalogError::Driver { error }) => self.record(
                &info,
                ActivityEvent::query_failed(
                    statement,
                    dialect,
                    kind,
                    &error_details(error),
                    duration_ms,
                ),
            ),
            Err(other) => self.record(
                &info,
                ActivityEvent::query_failed(
                    statement,
                    dialect,
                    kind,
                    &varve_activity::ErrorDetails {
                        code: None,
                        message: other.to_string(),
                    },
                    duration_ms,
                ),
            ),
        }
        // A confirmed run is logged like every other confirmed destructive
        // run (design doc 9.2), once it ran.
        if let (Some(kind), Ok(_)) = (confirmed_kind, &result) {
            self.record(
                &info,
                ActivityEvent::destructive_confirmed(
                    format!("ExplainAnalyze {kind}"),
                    &environment_label(&info),
                ),
            );
        }
        Ok(result?)
    }
}

/// Why a read-only session cannot run a statement the catalog could not
/// prove free of side effects: what the text check found (a change of
/// data, functions a rollback may not undo) and the catalog's reasons.
/// The advice to end the session's open transaction is given only when
/// the text check found a plain read, so the transaction is what stands in
/// the way; for anything else ending it would not help on a read-only
/// session.
fn read_only_refusal(
    text_check: &AnalyzeSafety,
    warnings: &[String],
    in_transaction: Option<bool>,
) -> String {
    let mut reasons: Vec<String> = Vec::new();
    match text_check {
        AnalyzeSafety::RolledBack { warnings: found } => {
            reasons.push(
                "The statement changes data, so it cannot run in a read-only transaction."
                    .to_owned(),
            );
            reasons.extend(found.iter().cloned());
        }
        AnalyzeSafety::NeedsConfirmation { warnings: found } => {
            reasons.extend(found.iter().cloned());
        }
        AnalyzeSafety::ReadOnly | AnalyzeSafety::Refused { .. } => {}
    }
    for warning in warnings {
        if !reasons.contains(warning) {
            reasons.push(warning.clone());
        }
    }
    if reasons.is_empty() {
        reasons.push("The statement may have side effects.".to_owned());
    }
    let mut message = format!(
        "This session is read-only, so EXPLAIN ANALYZE cannot run here: {}",
        reasons.join(" ")
    );
    if matches!(text_check, AnalyzeSafety::ReadOnly) && in_transaction == Some(true) {
        message.push_str(
            " Commit or roll back the session's open transaction, then run EXPLAIN ANALYZE again.",
        );
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    const IN_TRANSACTION: &str =
        "The session is inside a transaction, where the run cannot be made read-only.";

    #[test]
    fn transaction_advice_only_for_reads_the_transaction_blocks() {
        let read = read_only_refusal(
            &AnalyzeSafety::ReadOnly,
            &[IN_TRANSACTION.to_owned()],
            Some(true),
        );
        assert!(read.contains(IN_TRANSACTION), "{read}");
        assert!(read.contains("Commit or roll back"), "{read}");

        let insert = read_only_refusal(
            &AnalyzeSafety::RolledBack { warnings: vec![] },
            &[IN_TRANSACTION.to_owned()],
            Some(true),
        );
        assert!(insert.contains("changes data"), "{insert}");
        assert!(!insert.contains("Commit or roll back"), "{insert}");

        let function = read_only_refusal(
            &AnalyzeSafety::NeedsConfirmation {
                warnings: vec!["calls pg_advisory_lock".to_owned()],
            },
            &[IN_TRANSACTION.to_owned()],
            Some(true),
        );
        assert!(function.contains("pg_advisory_lock"), "{function}");
        assert!(!function.contains("Commit or roll back"), "{function}");

        let outside = read_only_refusal(
            &AnalyzeSafety::ReadOnly,
            &["calls a user function".to_owned()],
            Some(false),
        );
        assert!(!outside.contains("Commit or roll back"), "{outside}");
    }
}
