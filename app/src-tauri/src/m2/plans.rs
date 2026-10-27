//! Server-held plans.
//!
//! Every `*_plan` command stores what it planned here and returns a plan id
//! with a view of the plan for the preview; the matching `*_apply` command
//! takes the id and runs exactly the stored plan. The UI never sends
//! statements back, so what runs, its destructive flag and the text in the
//! activity log are the ones the planner produced.
//!
//! Plans are bound to the session they were made for, expire after
//! [`PLAN_TTL`], are used once, and at most [`MAX_PLANS_PER_SESSION`] are
//! kept per session (the oldest go first). A session's plans are dropped
//! when it closes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use varve_catalog::EditPlan;
use varve_catalog::data_edit::{RowEditPlan, TableTarget};
use varve_live_stats::redis::CommandPlan;
use varve_live_stats::sql::EditScope;

use crate::commands::CmdError;

/// How long a plan can be applied after it was made.
pub const PLAN_TTL: Duration = Duration::from_secs(30 * 60);

/// Plans kept per session.
pub const MAX_PLANS_PER_SESSION: usize = 32;

/// A plan and its id, as the `*_plan` commands return them.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Planned<T> {
    /// Pass this to the matching `*_apply` command.
    pub plan_id: String,
    /// What will run, for the preview.
    pub plan: T,
}

/// What a stored plan is.
#[derive(Debug, Clone)]
pub enum StoredPlan {
    /// An object editor plan.
    Edit(EditPlan),
    /// An object editor plan that drops or renames a PostgreSQL database:
    /// the app's own pooled sessions on that database close before it runs
    /// (PostgreSQL refuses while any session is connected to it).
    EditDatabase {
        /// The SQL.
        plan: EditPlan,
        /// The database dropped or renamed.
        database: String,
    },
    /// Grid changes, bound to their table.
    Rows {
        /// The table the plan was made for.
        target: TableTarget,
        /// The SQL.
        plan: RowEditPlan,
    },
    /// A server variable change.
    Variable {
        /// The SQL.
        plan: EditPlan,
        /// Where the change applies: session changes run on the user's
        /// session, the others on the admin session.
        scope: EditScope,
    },
    /// Redis commands.
    Redis(CommandPlan),
    /// B1: a Redis value editor or consumer group change, with its checks.
    RedisEdit(Box<varve_driver_redis::EditProgram>),
}

impl StoredPlan {
    fn kind(&self) -> &'static str {
        match self {
            Self::Edit(_) | Self::EditDatabase { .. } => "editor",
            Self::Rows { .. } => "grid",
            Self::Variable { .. } => "variable",
            Self::Redis(_) => "Redis",
            Self::RedisEdit(_) => "Redis edit",
        }
    }
}

struct Entry {
    session_id: String,
    created: Instant,
    plan: StoredPlan,
}

/// The plans of every open session.
#[derive(Default)]
pub struct PlanStore {
    map: Mutex<HashMap<String, Entry>>,
    /// When each taken plan was made, so a restored plan keeps its age.
    taken: Mutex<HashMap<String, Instant>>,
}

impl std::fmt::Debug for PlanStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.map.lock().map(|m| m.len()).unwrap_or_default();
        f.debug_struct("PlanStore").field("plans", &count).finish()
    }
}

fn unavailable() -> CmdError {
    CmdError::NotFound {
        message: "This plan is no longer available: it was applied, expired, or made for \
                  another session. Plan the change again."
            .into(),
    }
}

impl PlanStore {
    /// Stores a plan for a session and returns its id.
    pub fn put(&self, session_id: &str, plan: StoredPlan) -> String {
        self.put_at(session_id, plan, Instant::now())
    }

    fn put_at(&self, session_id: &str, plan: StoredPlan, now: Instant) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        self.insert_at(&id, session_id, plan, now, now);
        id
    }

    /// Inserts under `id` with its creation time, dropping expired plans
    /// and the session's oldest beyond the cap.
    fn insert_at(
        &self,
        id: &str,
        session_id: &str,
        plan: StoredPlan,
        created: Instant,
        now: Instant,
    ) {
        if now.saturating_duration_since(created) >= PLAN_TTL {
            return;
        }
        if let Ok(mut taken) = self.taken.lock() {
            taken.retain(|_, made| now.saturating_duration_since(*made) < PLAN_TTL);
        }
        if let Ok(mut map) = self.map.lock() {
            map.retain(|_, e| now.saturating_duration_since(e.created) < PLAN_TTL);
            let mut own: Vec<(String, Instant)> = map
                .iter()
                .filter(|(_, e)| e.session_id == session_id)
                .map(|(k, e)| (k.clone(), e.created))
                .collect();
            if own.len() >= MAX_PLANS_PER_SESSION {
                own.sort_by_key(|(_, created)| *created);
                for (old, _) in own.iter().take(own.len() + 1 - MAX_PLANS_PER_SESSION) {
                    map.remove(old);
                }
            }
            map.insert(
                id.to_string(),
                Entry {
                    session_id: session_id.to_string(),
                    created,
                    plan,
                },
            );
        }
    }

    /// A copy of a plan, for the checks before it runs.
    ///
    /// # Errors
    /// [`CmdError::NotFound`] when the id is unknown, expired, used, or
    /// belongs to another session.
    pub fn peek(&self, session_id: &str, plan_id: &str) -> Result<StoredPlan, CmdError> {
        self.peek_at(session_id, plan_id, Instant::now())
    }

    fn peek_at(
        &self,
        session_id: &str,
        plan_id: &str,
        now: Instant,
    ) -> Result<StoredPlan, CmdError> {
        let map = self.map.lock().map_err(|_| unavailable())?;
        match map.get(plan_id) {
            Some(e)
                if e.session_id == session_id
                    && now.saturating_duration_since(e.created) < PLAN_TTL =>
            {
                Ok(e.plan.clone())
            }
            _ => Err(unavailable()),
        }
    }

    /// Removes a plan to run it. Only one caller gets it.
    ///
    /// # Errors
    /// As [`Self::peek`].
    pub fn take(&self, session_id: &str, plan_id: &str) -> Result<StoredPlan, CmdError> {
        self.take_at(session_id, plan_id, Instant::now())
    }

    fn take_at(
        &self,
        session_id: &str,
        plan_id: &str,
        now: Instant,
    ) -> Result<StoredPlan, CmdError> {
        let mut map = self.map.lock().map_err(|_| unavailable())?;
        let fresh = map.get(plan_id).is_some_and(|e| {
            e.session_id == session_id && now.saturating_duration_since(e.created) < PLAN_TTL
        });
        if !fresh {
            return Err(unavailable());
        }
        let entry = map.remove(plan_id).ok_or_else(unavailable)?;
        if let Ok(mut taken) = self.taken.lock() {
            taken.insert(plan_id.to_string(), entry.created);
        }
        Ok(entry.plan)
    }

    /// Puts a taken plan back under its id, when nothing of it ran (the
    /// session refused it, for example because of an open transaction).
    /// It keeps the time it was made (so it still expires on time) and
    /// counts against the session's cap.
    pub fn restore(&self, session_id: &str, plan_id: &str, plan: StoredPlan) {
        let created = self
            .taken
            .lock()
            .ok()
            .and_then(|mut taken| taken.remove(plan_id));
        if let Some(created) = created {
            self.insert_at(plan_id, session_id, plan, created, Instant::now());
        }
    }

    /// Drops the plans of a closed session.
    pub fn forget_session(&self, session_id: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.retain(|_, e| e.session_id != session_id);
        }
    }
}

impl crate::commands::Services {
    /// Puts a plan back after a refused apply, unless its session closed
    /// meanwhile (a dead session's plan could never be applied).
    pub(crate) fn restore_plan(&self, session_id: &str, plan_id: &str, plan: StoredPlan) {
        if self.sessions.info(session_id).is_ok() {
            self.m2.plans.restore(session_id, plan_id, plan);
        }
    }
}

/// The error for a plan id of another kind.
pub(crate) fn wrong_kind(expected: &str, found: &StoredPlan) -> CmdError {
    CmdError::Invalid {
        message: format!(
            "This is a {} plan; apply it with the {} command.",
            found.kind(),
            expected
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit() -> StoredPlan {
        StoredPlan::Edit(EditPlan {
            statements: vec!["CREATE TABLE t (a int)".into()],
            transactional: true,
            destructive: false,
            warnings: vec![],
        })
    }

    #[test]
    fn plans_are_single_use_and_bound_to_their_session() {
        let store = PlanStore::default();
        let id = store.put("s1", edit());
        assert!(store.peek("s2", &id).is_err(), "other session");
        assert!(store.peek("s1", "not-a-plan").is_err(), "tampered id");
        assert!(store.peek("s1", &id).is_ok());
        assert!(store.take("s2", &id).is_err());
        assert!(store.take("s1", &id).is_ok());
        assert!(
            matches!(store.take("s1", &id), Err(CmdError::NotFound { .. })),
            "used twice"
        );
        let restored = store.put("s1", edit());
        let plan = store.take("s1", &restored);
        assert!(plan.is_ok());
        if let Ok(plan) = plan {
            store.restore("s1", &restored, plan);
        }
        assert!(store.take("s1", &restored).is_ok());
        // A plan that was never taken cannot be planted by restore.
        store.restore("s1", "planted", edit());
        assert!(store.peek("s1", "planted").is_err());
    }

    #[test]
    fn restored_plans_keep_their_age() {
        let store = PlanStore::default();
        let start = Instant::now();
        let id = store.put_at("s1", edit(), start);
        let plan = store.take_at("s1", &id, start);
        assert!(plan.is_ok());
        if let Ok(plan) = plan {
            store.restore("s1", &id, plan);
        }
        let late = start + PLAN_TTL + Duration::from_secs(1);
        assert!(store.peek_at("s1", &id, late).is_err(), "expires as made");
        assert!(store.peek_at("s1", &id, start).is_ok());
    }

    #[test]
    fn plans_expire_and_are_bounded() {
        let store = PlanStore::default();
        let start = Instant::now();
        let id = store.put_at("s1", edit(), start);
        let late = start + PLAN_TTL + Duration::from_secs(1);
        assert!(store.peek_at("s1", &id, late).is_err(), "expired");
        assert!(store.take_at("s1", &id, late).is_err());

        let first = store.put_at("s1", edit(), start);
        for i in 0..MAX_PLANS_PER_SESSION {
            store.put_at("s1", edit(), start + Duration::from_millis(i as u64 + 1));
        }
        assert!(
            store.peek_at("s1", &first, start).is_err(),
            "oldest evicted"
        );
        let other = store.put_at("s2", edit(), start);
        store.forget_session("s1");
        assert!(store.peek_at("s2", &other, start).is_ok());
        assert!(matches!(
            wrong_kind(
                "edit_apply",
                &store.peek_at("s2", &other, start).unwrap_or(edit())
            ),
            CmdError::Invalid { .. }
        ));
    }
}
