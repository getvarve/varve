//! Bulk time to live changes in the key browser (design doc 4.2, "TTL:
//! E"): set or clear the time to live of the keys that match the browser's
//! filter, as a background job with progress and Stop.
//!
//! - [`Services::redis_bulk_ttl_preview`] counts the matching keys (within
//!   [`PREVIEW_MAX_KEYS`] and [`PREVIEW_TIME`]: the count is then a lower
//!   bound) and says which database the count is for.
//! - [`Services::redis_bulk_ttl_start`] refuses read-only sessions, asks
//!   for confirmation when the session confirms destructive statements
//!   (production or the profile option), and starts the job on its own
//!   connection ([`varve_driver_redis::BulkTtl`]): `SCAN` steps of at most
//!   [`varve_driver_redis::BULK_BATCH`] keys, one pipeline each, so the
//!   console stays free. It is refused when the tab switched database
//!   since the preview.
//! - [`Services::redis_bulk_ttl_status`] reports progress; the UI polls it.
//! - [`Services::redis_bulk_ttl_stop`] stops the job before its next step
//!   (keys changed so far stay changed). Closing the tab's session stops
//!   its jobs too.
//!
//! A finished run is logged as `data.edited` (operation `bulkExpire` or
//! `bulkPersist`, the filter, the keys changed, and `status` `complete`,
//! `stopped` or `failed`), plus `destructive.confirmed` when it was
//! confirmed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use varve_activity::Recorder;
use varve_activity::{ActivityEvent, ActivityEventBuilder, Category, ErrorDetails, Outcome};
use varve_core::Engine;
use varve_driver::DriverError;
use varve_driver_redis::{
    BulkCount, BulkCounts, BulkEnd, BulkFilter, BulkProgress, BulkTtl, BulkTtlAction,
};

use super::redis_edit::driver_err;
use super::{elapsed_ms, environment_label};
use crate::commands::{CmdError, Services};
use crate::sessions::SessionInfo;

type CmdResult<T> = Result<T, CmdError>;

/// Most keys a preview counts.
pub const PREVIEW_MAX_KEYS: u64 = 1_000_000;

/// Longest a preview counts.
pub const PREVIEW_TIME: Duration = Duration::from_secs(3);

/// Finished jobs kept for their status (as long as a plan).
pub const JOB_KEPT: Duration = Duration::from_secs(30 * 60);

/// Most jobs kept (running ones are never dropped).
pub const MAX_JOBS: usize = 32;

/// Most jobs running at once over all tabs (one per tab).
pub const MAX_RUNNING: usize = 4;

/// A bulk change to start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkTtlRequest {
    /// The browser's filter.
    pub filter: BulkFilter,
    /// Set or clear.
    pub action: BulkTtlAction,
    /// The database of the preview; the start is refused when the tab is
    /// on another one now.
    pub db: u16,
}

/// A started job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkTtlJob {
    /// Pass to the status and stop calls.
    pub job_id: String,
}

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BulkTtlState {
    /// Still running.
    Running,
    /// Every matching key was walked.
    Complete,
    /// Stopped by the user or because the session closed.
    Stopped,
    /// Failed; `error` says why.
    Failed,
}

/// The status of a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkTtlStatus {
    /// Job id.
    pub job_id: String,
    /// Where it is.
    pub state: BulkTtlState,
    /// What it did so far.
    pub counts: BulkCounts,
    /// Why it failed.
    pub error: Option<DriverError>,
    /// Milliseconds since it started (until it ended, when it ended).
    pub elapsed_ms: u64,
    /// The database it runs on.
    pub db: u16,
    /// What it does.
    pub action: BulkTtlAction,
    /// Which keys.
    pub filter: BulkFilter,
}

#[derive(Debug)]
struct Job {
    session_id: String,
    db: u16,
    filter: BulkFilter,
    action: BulkTtlAction,
    started: Instant,
    stop: AtomicBool,
    progress: BulkProgress,
    outcome: Mutex<Option<(Result<BulkEnd, DriverError>, u64)>>,
}

impl Job {
    fn status(&self, id: &str) -> BulkTtlStatus {
        let outcome = self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (state, error, elapsed) = match outcome {
            None => (BulkTtlState::Running, None, elapsed_ms(self.started)),
            Some((Ok(BulkEnd::Complete), ms)) => (BulkTtlState::Complete, None, ms),
            Some((Ok(BulkEnd::Stopped), ms)) => (BulkTtlState::Stopped, None, ms),
            Some((Err(error), ms)) => (BulkTtlState::Failed, Some(error), ms),
        };
        BulkTtlStatus {
            job_id: id.to_string(),
            state,
            counts: self.progress.snapshot(),
            error,
            elapsed_ms: elapsed,
            db: self.db,
            action: self.action,
            filter: self.filter.clone(),
        }
    }

    fn finished(&self) -> bool {
        self.outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
}

/// The bulk TTL jobs of all tabs, by job id.
#[derive(Debug, Default)]
pub struct BulkTtlJobs {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}

impl BulkTtlJobs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Job>>> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn get(&self, session_id: &str, id: &str) -> CmdResult<Arc<Job>> {
        self.lock()
            .get(id)
            .filter(|j| j.session_id == session_id)
            .cloned()
            .ok_or_else(|| CmdError::Invalid {
                message: "This bulk change is no longer known (it finished long ago, or the tab \
                          was closed)"
                    .into(),
            })
    }

    /// Stops every job of a closed session (before its next step) and
    /// forgets them.
    pub fn forget_session(&self, session_id: &str) {
        self.lock().retain(|_, job| {
            let keep = job.session_id != session_id;
            if !keep {
                job.stop.store(true, Ordering::SeqCst);
            }
            keep
        });
    }

    /// Adds a job unless the tab already runs one or [`MAX_RUNNING`] run
    /// in all (each holds its own connection).
    ///
    /// # Errors
    /// `invalid` when the limit is reached.
    fn admit(&self, session_id: &str, id: String, job: Arc<Job>) -> CmdResult<()> {
        let mut jobs = self.lock();
        let running: Vec<&Arc<Job>> = jobs.values().filter(|j| !j.finished()).collect();
        if running.iter().any(|j| j.session_id == session_id) {
            return Err(CmdError::Invalid {
                message: "A bulk TTL change is already running in this tab; stop it or wait \
                          until it ends"
                    .into(),
            });
        }
        if running.len() >= MAX_RUNNING {
            return Err(CmdError::Invalid {
                message: format!(
                    "{MAX_RUNNING} bulk TTL changes are running already; wait until one ends"
                ),
            });
        }
        jobs.insert(id, job);
        Ok(())
    }

    /// The jobs of a tab, newest first.
    fn of_session(&self, session_id: &str) -> Vec<(String, Arc<Job>)> {
        let mut list: Vec<(String, Arc<Job>)> = self
            .lock()
            .iter()
            .filter(|(_, j)| j.session_id == session_id)
            .map(|(id, j)| (id.clone(), Arc::clone(j)))
            .collect();
        list.sort_by_key(|(_, j)| std::cmp::Reverse(j.started));
        list
    }

    /// Drops finished jobs older than [`JOB_KEPT`] and the oldest finished
    /// ones beyond [`MAX_JOBS`].
    fn prune(&self) {
        let mut jobs = self.lock();
        jobs.retain(|_, j| !j.finished() || j.started.elapsed() < JOB_KEPT);
        while jobs.len() >= MAX_JOBS {
            let Some(oldest) = jobs
                .iter()
                .filter(|(_, j)| j.finished())
                .min_by_key(|(_, j)| j.started)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            jobs.remove(&oldest);
        }
    }
}

/// Records an event the way [`Services::record`] does, from a task.
fn record(activity: &Recorder, info: &SessionInfo, builder: ActivityEventBuilder) {
    let builder = if info.connection_id == "unsaved" {
        builder
    } else {
        builder.connection(info.connection_id.clone())
    };
    match builder.build() {
        Ok(event) => {
            if let Err(error) = activity.record(event) {
                tracing::warn!(%error, "could not record an activity event");
            }
        }
        Err(error) => tracing::warn!(%error, "invalid activity event"),
    }
}

/// The `data.edited` event of a finished job.
fn finished_event(job: &Job, result: &Result<BulkEnd, DriverError>) -> ActivityEventBuilder {
    let counts = job.progress.snapshot();
    let operation = match job.action {
        BulkTtlAction::Expire { .. } => "bulkExpire",
        BulkTtlAction::Persist => "bulkPersist",
    };
    let pattern: String = job.filter.pattern.chars().take(200).collect();
    let mut event = ActivityEvent::builder(Category::Data, "data.edited")
        .target(format!("db{}:{}", job.db, pattern))
        .detail("engine", "redis")
        .detail("db", u64::from(job.db))
        .detail("operations", serde_json::json!([operation]))
        .detail("pattern", pattern)
        .detail_opt(
            "key_type",
            job.filter
                .key_type
                .as_ref()
                .map(|t| t.type_name().to_string()),
        )
        .detail("keys_matched", counts.matched)
        .detail("rows_updated", counts.changed)
        .duration_ms(elapsed_ms(job.started));
    if let BulkTtlAction::Expire { ttl_ms } = job.action {
        event = event.detail("ttl_ms", ttl_ms);
    }
    match result {
        Ok(BulkEnd::Complete) => event.detail("status", "complete"),
        Ok(BulkEnd::Stopped) => event.detail("status", "stopped"),
        Err(error) => event
            .outcome(Outcome::Failure)
            .detail("status", "failed")
            .error(
                &ErrorDetails {
                    code: error.code.clone(),
                    message: format!(
                        "the bulk change stopped after {} changed keys",
                        counts.changed
                    ),
                },
                None,
            ),
    }
}

impl Services {
    /// A bulk TTL runner of the tab's session, on its database now.
    async fn bulk_runner(&self, session_id: &str) -> CmdResult<BulkTtl> {
        let (_, lock) = self.redis_session_lock(session_id).await?;
        Self::redis_of(&lock, session_id)?
            .bulk_ttl()
            .await
            .map_err(driver_err)
    }

    /// Counts the keys that match `filter` on the tab's database (a lower
    /// bound when the count was cut off; `complete` says which).
    ///
    /// # Errors
    /// Unknown or non-Redis session, read-only sessions (`readOnly`: they
    /// cannot run the change), driver errors.
    pub async fn redis_bulk_ttl_preview(
        &self,
        session_id: &str,
        filter: &BulkFilter,
    ) -> CmdResult<BulkCount> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "Bulk time to live changes")?;
        Self::require_writable(&info, "changing time to live values")?;
        let runner = self.bulk_runner(session_id).await?;
        runner
            .count(filter, PREVIEW_MAX_KEYS, PREVIEW_TIME)
            .await
            .map_err(driver_err)
    }

    /// Starts a bulk change (see the module documentation).
    ///
    /// # Errors
    /// Read-only sessions (`readOnly`), a time to live of 0 or another
    /// database than the preview's (`invalid`), `confirmationRequired`
    /// until `confirmed` on sessions that confirm destructive statements,
    /// driver errors.
    pub async fn redis_bulk_ttl_start(
        &self,
        session_id: &str,
        request: &BulkTtlRequest,
        confirmed: bool,
    ) -> CmdResult<BulkTtlJob> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "Bulk time to live changes")?;
        Self::require_writable(&info, "changing time to live values")?;
        request
            .action
            .validate()
            .map_err(|e| CmdError::Invalid { message: e.message })?;
        let runner = self.bulk_runner(session_id).await?;
        if runner.db() != request.db {
            return Err(CmdError::Invalid {
                message: format!(
                    "The keys were counted on database {}, but this tab is on database {} now; \
                     count them again",
                    request.db,
                    runner.db()
                ),
            });
        }
        let needs = info.confirm_destructive;
        let statement = format!(
            "{} on {}",
            request.action.command(),
            request.filter.describe()
        );
        self.require_confirmation(&info, needs, confirmed, vec![statement.clone()])?;
        self.m2.bulk_ttl.prune();
        let id = uuid::Uuid::now_v7().to_string();
        let session_id = session_id.to_string();
        let job = Arc::new(Job {
            session_id: session_id.clone(),
            db: runner.db(),
            filter: request.filter.clone(),
            action: request.action,
            started: Instant::now(),
            stop: AtomicBool::new(false),
            progress: BulkProgress::default(),
            outcome: Mutex::new(None),
        });
        self.m2
            .bulk_ttl
            .admit(&session_id, id.clone(), Arc::clone(&job))?;
        // Confirmed now, whatever the run does later (it can change keys
        // and then fail).
        if needs && confirmed {
            self.record(
                &info,
                ActivityEvent::destructive_confirmed(statement, &environment_label(&info)),
            );
        }
        let activity = Arc::clone(&self.activity);
        tokio::spawn(async move {
            let result = runner
                .apply(&job.filter, job.action, &job.stop, &job.progress)
                .await;
            record(&activity, &info, finished_event(&job, &result));
            *job.outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((result, elapsed_ms(job.started)));
        });
        Ok(BulkTtlJob { job_id: id })
    }

    /// The bulk jobs of the tab, newest first, so a reopened dialog finds
    /// a job that still runs (or how the last one ended).
    #[must_use]
    pub fn redis_bulk_ttl_jobs(&self, session_id: &str) -> Vec<BulkTtlStatus> {
        self.m2
            .bulk_ttl
            .of_session(session_id)
            .into_iter()
            .map(|(id, job)| job.status(&id))
            .collect()
    }

    /// The status of a job of the tab.
    ///
    /// # Errors
    /// An unknown job (`invalid`).
    pub fn redis_bulk_ttl_status(
        &self,
        session_id: &str,
        job_id: &str,
    ) -> CmdResult<BulkTtlStatus> {
        Ok(self.m2.bulk_ttl.get(session_id, job_id)?.status(job_id))
    }

    /// Stops a job before its next step and returns its status.
    ///
    /// # Errors
    /// An unknown job (`invalid`).
    pub fn redis_bulk_ttl_stop(&self, session_id: &str, job_id: &str) -> CmdResult<BulkTtlStatus> {
        let job = self.m2.bulk_ttl.get(session_id, job_id)?;
        job.stop.store(true, Ordering::SeqCst);
        Ok(job.status(job_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(session: &str, finished: bool) -> Arc<Job> {
        Arc::new(Job {
            session_id: session.into(),
            db: 0,
            filter: BulkFilter {
                pattern: "*".into(),
                key_type: None,
            },
            action: BulkTtlAction::Persist,
            started: Instant::now(),
            stop: AtomicBool::new(false),
            progress: BulkProgress::default(),
            outcome: Mutex::new(finished.then_some((Ok(BulkEnd::Complete), 1))),
        })
    }

    #[test]
    fn running_jobs_are_capped() {
        let jobs = BulkTtlJobs::default();
        assert!(jobs.admit("s1", "a".into(), job("s1", false)).is_ok());
        // One running job per tab.
        assert!(matches!(
            jobs.admit("s1", "b".into(), job("s1", false)),
            Err(CmdError::Invalid { .. })
        ));
        // A finished one does not count.
        assert!(jobs.admit("s9", "f".into(), job("s9", true)).is_ok());
        assert!(jobs.admit("s9", "g".into(), job("s9", false)).is_ok());
        for s in ["s2", "s3"] {
            assert!(jobs.admit(s, s.into(), job(s, false)).is_ok());
        }
        assert!(matches!(
            jobs.admit("s4", "h".into(), job("s4", false)),
            Err(CmdError::Invalid { message }) if message.contains("running already")
        ));
        let mine = jobs.of_session("s9");
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].0, "g");
    }
}
