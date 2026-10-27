//! PostgreSQL, MySQL and MariaDB live server panels (design doc 4.2): the
//! process list with kill, server variables with editing, the server status
//! summary and the MariaDB Galera panel. They run on the profile's admin
//! session ([`super::metadata`]), never under the user's session lock.

use std::time::Instant;

use varve_activity::{ActivityEvent, Category, ErrorDetails, Outcome as EventOutcome};
use varve_catalog::EditPlan;
use varve_core::Engine;
use varve_live_stats::sql::{
    self as live, EditScope, GaleraStatus, KillOutcome, KillTarget, LiveError, ProcessList,
    ServerStatus, ServerVariable, VariableChange,
};

use super::editors::{confirmation_labels, plan_kinds};
use super::plans::{Planned, StoredPlan, wrong_kind};
use super::{elapsed_ms, sql_dialect};
use crate::commands::{CmdError, Services};
use crate::sessions::SessionError;

type CmdResult<T> = Result<T, CmdError>;

const SQL_ENGINES: [Engine; 3] = [Engine::Postgres, Engine::Mysql, Engine::Mariadb];

impl Services {
    /// The process list, read on the profile's admin session (its own
    /// connection is `ownId`; the user's session is a normal entry, so a
    /// running query can be killed from the same tab).
    ///
    /// # Errors
    /// Unknown session, a Redis session, panel errors (`live`).
    pub async fn process_list(&self, session_id: &str) -> CmdResult<ProcessList> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "The process list")?;
        let server = info.server.clone();
        self.with_admin(session_id, true, |session| {
            let server = server.clone();
            Box::pin(async move { Ok(live::process_list(session, &server).await?) })
        })
        .await
    }

    /// Cancels the running statement of a connection or closes it, from the
    /// admin session. Refused on read-only sessions and for the admin
    /// session's own connection; logged as `process.killed`.
    ///
    /// # Errors
    /// As described, and panel errors (`live`, `permissionDenied`).
    pub async fn kill_process(
        &self,
        session_id: &str,
        id: i64,
        target: KillTarget,
    ) -> CmdResult<KillOutcome> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "Killing processes")?;
        Self::require_writable(&info, "killing processes")?;
        let started = Instant::now();
        let server = info.server.clone();
        let result = self
            .with_admin(session_id, false, |session| {
                let server = server.clone();
                Box::pin(async move { Ok(live::kill(session, &server, id, target).await?) })
            })
            .await;
        let target_label = match target {
            KillTarget::Query => "query",
            KillTarget::Connection => "connection",
        };
        let mut event = ActivityEvent::builder(Category::Query, "process.killed")
            .target(id.to_string())
            .detail("process_id", id)
            .detail("kill", target_label)
            .duration_ms(elapsed_ms(started));
        event = match &result {
            Ok(outcome) => event.detail("signalled", outcome.signalled),
            Err(error) => event.outcome(EventOutcome::Failure).error(
                &ErrorDetails {
                    code: cmd_code(error),
                    message: error.to_string(),
                },
                None,
            ),
        };
        self.record(&info, event);
        result
    }

    /// Every server variable, read on the admin session.
    ///
    /// # Errors
    /// Unknown session, a Redis session, panel errors.
    pub async fn server_variables(&self, session_id: &str) -> CmdResult<Vec<ServerVariable>> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "Server variables")?;
        let server = info.server.clone();
        self.with_admin(session_id, true, |session| {
            let server = server.clone();
            Box::pin(async move { Ok(live::server_variables(session, &server).await?) })
        })
        .await
    }

    /// Plans a variable change from the variable as the server reports it
    /// now, and keeps the plan on the server.
    ///
    /// # Errors
    /// Unknown session, a Redis session, an unknown variable, a scope or
    /// value that does not fit (`live`).
    pub async fn variable_change_plan(
        &self,
        session_id: &str,
        name: &str,
        change: &VariableChange,
    ) -> CmdResult<Planned<EditPlan>> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "Server variables")?;
        let server = info.server.clone();
        let plan = self
            .with_admin(session_id, true, |session| {
                let server = server.clone();
                let name = name.to_string();
                let change = change.clone();
                Box::pin(async move {
                    Ok(live::plan_variable_change(session, &server, &name, &change).await?)
                })
            })
            .await?;
        if plan.transactional {
            return Err(CmdError::Invalid {
                message: "Variable changes do not run in a transaction.".into(),
            });
        }
        let scope = match change {
            VariableChange::Set { scope, .. } | VariableChange::Reset { scope } => *scope,
        };
        let plan_id = self.m2.plans.put(
            session_id,
            StoredPlan::Variable {
                plan: plan.clone(),
                scope,
            },
        );
        Ok(Planned { plan_id, plan })
    }

    /// Runs a stored variable change plan statement by statement through
    /// the session manager (logged as queries): session-scope changes on
    /// the user's session, the others on the admin session. Refused on
    /// read-only sessions; a destructive plan needs `confirmed`. A missing
    /// privilege comes back as `live` / `permissionDenied`.
    ///
    /// # Errors
    /// As described, an unknown, used or expired plan id, and driver errors
    /// of the failing statement.
    pub async fn variable_change_apply(
        &self,
        session_id: &str,
        plan_id: &str,
        confirmed: bool,
    ) -> CmdResult<()> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "Server variables")?;
        Self::require_writable(&info, "changing server variables")?;
        let stored = self.m2.plans.peek(session_id, plan_id)?;
        let StoredPlan::Variable { plan, .. } = &stored else {
            return Err(wrong_kind("variable_change_apply", &stored));
        };
        let dialect = sql_dialect(info.engine).ok_or_else(|| CmdError::Invalid {
            message: "Server variables are a SQL panel.".into(),
        })?;
        let kinds = plan_kinds(&plan.statements, dialect)?;
        let classified = kinds.iter().any(|k| k.is_destructive());
        let labels = confirmation_labels(&kinds);
        self.require_confirmation(
            &info,
            plan.destructive || (info.confirm_destructive && classified),
            confirmed,
            labels.clone(),
        )?;
        let stored = self.m2.plans.take(session_id, plan_id)?;
        let StoredPlan::Variable { plan, scope } = &stored else {
            return Err(wrong_kind("variable_change_apply", &stored));
        };
        let runner = if *scope == EditScope::Session {
            info.clone()
        } else {
            let admin = self.admin_session(session_id).await?;
            self.session_info(&admin)?
        };
        let forced = plan.destructive.then_some(labels);
        match self
            .run_statements(&runner, &plan.statements, confirmed, forced, None)
            .await
            .map_err(|stopped| *stopped)
        {
            Ok(_) => Ok(()),
            Err((SessionError::Driver { error }, _)) => Err(LiveError::from(error).into()),
            Err((other, _)) => Err(other.into()),
        }
    }

    /// The live server status summary, read on the admin session.
    ///
    /// # Errors
    /// Unknown session, a Redis session, panel errors.
    pub async fn server_status(&self, session_id: &str) -> CmdResult<ServerStatus> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "Server status")?;
        let server = info.server.clone();
        self.with_admin(session_id, true, |session| {
            let server = server.clone();
            Box::pin(async move { Ok(live::server_status(session, &server).await?) })
        })
        .await
    }

    /// The MariaDB Galera status, read on the admin session.
    ///
    /// # Errors
    /// Unknown session, other engines (`live` / `unsupported`), panel
    /// errors.
    pub async fn galera_status(&self, session_id: &str) -> CmdResult<GaleraStatus> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &SQL_ENGINES, "The Galera panel")?;
        let server = info.server.clone();
        self.with_admin(session_id, true, |session| {
            let server = server.clone();
            Box::pin(async move { Ok(live::galera_status(session, &server).await?) })
        })
        .await
    }
}

/// The engine error code of a failed panel call, for the activity log.
fn cmd_code(error: &CmdError) -> Option<String> {
    match error {
        CmdError::Live { error } => live_code(error),
        CmdError::Session {
            error: SessionError::Driver { error },
        } => error.code.clone(),
        _ => None,
    }
}

/// The engine error code of a panel error, for the activity log.
fn live_code(error: &LiveError) -> Option<String> {
    match error {
        LiveError::PermissionDenied { code, .. } => code.clone(),
        LiveError::Driver { error } => error.code.clone(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn panel_arguments_deserialize_from_the_ui() -> Result<(), serde_json::Error> {
        let target: KillTarget = serde_json::from_value(json!("connection"))?;
        assert_eq!(target, KillTarget::Connection);
        let change: VariableChange = serde_json::from_value(json!({
            "action": "set", "scope": "global", "value": "100"
        }))?;
        assert!(matches!(change, VariableChange::Set { .. }));
        Ok(())
    }

    #[test]
    fn live_codes_are_kept() {
        let denied = LiveError::PermissionDenied {
            message: "no".into(),
            code: Some("42501".into()),
        };
        assert_eq!(live_code(&denied).as_deref(), Some("42501"));
        assert_eq!(live_code(&LiveError::OwnConnection { id: 1 }), None);
    }
}
