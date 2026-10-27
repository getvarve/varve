//! Redis value editors, sorted set ranges and stream consumer groups
//! (design doc 4.2). They run on the tab's own session (the key browser's
//! session and database): reads on its side connection, writes as
//! server-held plans ([`super::plans`]) that the driver applies on a
//! short-lived side connection with optimistic checks
//! ([`varve_driver_redis::RedisSession::apply_edit`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use varve_activity::{ActivityEvent, Category, ErrorDetails, Outcome as EventOutcome};
use varve_core::Engine;
use varve_driver::{DriverError, ErrorKind};
use varve_driver_redis::{
    CommandRisk, EditError, EditOutcome, EditProgram, KeyEdit, KeyExpect, KeyVersion, PendingPage,
    PendingQuery, PendingSummary, RedisEditPlan, RedisSession, StreamConsumer, StreamGroup,
    StreamGroupChange, StreamInfo, VersionReader, ZsetRange, ZsetRangePage, key_from_display,
};
use varve_driver_redis::{DigestMissing, FullDigest};

use super::plans::{Planned, StoredPlan, wrong_kind};
use super::{elapsed_ms, environment_label};
use crate::commands::{CmdError, Services};
use crate::sessions::{SessionError, SessionInfo, SessionLock};

type CmdResult<T> = Result<T, CmdError>;

/// Most characters of a key name in an activity event.
const LOGGED_KEY_CHARS: usize = 200;

/// A value editor request from the UI.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedisEditRequest {
    /// The key, display text.
    pub key: String,
    /// What the UI read about the key.
    pub expect: KeyExpect,
    /// The changes.
    pub edits: Vec<KeyEdit>,
}

/// A driver error as the UI sees it: read-only refusals as `readOnly`,
/// malformed ranges as `invalid`, everything else as a session error.
pub(crate) fn driver_err(error: DriverError) -> CmdError {
    match (error.kind, error.code.as_deref()) {
        (ErrorKind::ReadOnly, _) => CmdError::ReadOnly {
            message: error.message,
        },
        (_, Some("INVALID_RANGE")) => CmdError::Invalid {
            message: error.message,
        },
        _ => CmdError::Session {
            error: SessionError::Driver { error },
        },
    }
}

fn edit_err(error: EditError) -> CmdError {
    match error {
        EditError::Invalid(message) => CmdError::Invalid { message },
        EditError::Driver(error) => driver_err(error),
    }
}

/// Key, group or channel bytes from display text.
pub(crate) fn name_bytes(text: &str) -> CmdResult<Vec<u8>> {
    key_from_display(text).map_err(|e| CmdError::Invalid { message: e.message })
}

fn not_redis() -> CmdError {
    super::unsupported("this session is not a Redis connection")
}

/// `db<N>:<key>`, the key shortened, for the activity log.
fn key_label(view: &RedisEditPlan) -> String {
    let key: String = view.key.chars().take(LOGGED_KEY_CHARS).collect();
    format!("db{}:{key}", view.db)
}

/// Background digest reads kept at most (the oldest go first).
pub const MAX_DIGEST_READS: usize = 64;

/// How long a finished background digest read is kept (as long as a plan).
pub const DIGEST_READ_KEPT: Duration = Duration::from_secs(30 * 60);

/// Where a background digest read is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadState {
    /// Still reading.
    Running,
    /// Read: a digest, or why there is none, or how the key changed.
    Done(FullDigest),
    /// The read failed (the text says why).
    Failed(String),
}

#[derive(Debug)]
struct DigestRead {
    session_id: String,
    key: Vec<u8>,
    db: u16,
    started: Instant,
    state: ReadState,
    task: Option<tokio::task::AbortHandle>,
}

/// B1: the background reads of the full digest of keys opened in the value
/// viewer (too large to digest within the open budget), by read id. Each
/// runs on its own connection without the session lock, within the digest
/// budget; it is cancelled when the viewer closes or the session closes.
#[derive(Debug, Default)]
pub struct DigestReads {
    reads: Mutex<HashMap<String, DigestRead>>,
}

fn lock_reads(
    reads: &Mutex<HashMap<String, DigestRead>>,
) -> std::sync::MutexGuard<'_, HashMap<String, DigestRead>> {
    reads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl DigestReads {
    /// Starts reading the digest of `key` (opened as `opened`) in the
    /// background and returns the read id.
    fn start(
        m2: &Arc<super::M2State>,
        session_id: &str,
        key: Vec<u8>,
        reader: VersionReader,
        opened: KeyVersion,
    ) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        let db = reader.db();
        {
            let mut reads = lock_reads(&m2.digests.reads);
            reads.retain(|_, r| {
                let keep = r.started.elapsed() < DIGEST_READ_KEPT;
                if !keep && let Some(task) = &r.task {
                    task.abort();
                }
                keep
            });
            while reads.len() >= MAX_DIGEST_READS {
                let Some(oldest) = reads
                    .iter()
                    .min_by_key(|(_, r)| r.started)
                    .map(|(id, _)| id.clone())
                else {
                    break;
                };
                if let Some(task) = reads.remove(&oldest).and_then(|r| r.task) {
                    task.abort();
                }
            }
            reads.insert(
                id.clone(),
                DigestRead {
                    session_id: session_id.to_string(),
                    key: key.clone(),
                    db,
                    started: Instant::now(),
                    state: ReadState::Running,
                    task: None,
                },
            );
        }
        let state = Arc::clone(m2);
        let read_id = id.clone();
        let task = tokio::spawn(async move {
            let limit = reader.full_read_limit();
            let outcome = match reader.read_full(&key, &opened, limit).await {
                Ok(found) => ReadState::Done(found),
                Err(error) => ReadState::Failed(error.message),
            };
            if let Some(read) = lock_reads(&state.digests.reads).get_mut(&read_id) {
                read.state = outcome;
                read.task = None;
            }
        });
        if let Some(read) = lock_reads(&m2.digests.reads).get_mut(&id)
            && read.state == ReadState::Running
        {
            read.task = Some(task.abort_handle());
        }
        id
    }

    /// Stops and forgets read `id` of `session_id`.
    pub fn cancel(&self, session_id: &str, id: &str) {
        let mut reads = lock_reads(&self.reads);
        if reads.get(id).is_some_and(|r| r.session_id == session_id)
            && let Some(task) = reads.remove(id).and_then(|r| r.task)
        {
            task.abort();
        }
    }

    /// Stops and forgets the reads of a closed session.
    pub fn forget_session(&self, session_id: &str) {
        lock_reads(&self.reads).retain(|_, r| {
            let keep = r.session_id != session_id;
            if !keep && let Some(task) = &r.task {
                task.abort();
            }
            keep
        });
    }

    /// Key, database and state of read `id` of `session_id`.
    fn state(&self, session_id: &str, id: &str) -> Option<(Vec<u8>, u16, ReadState)> {
        lock_reads(&self.reads)
            .get(id)
            .filter(|r| r.session_id == session_id)
            .map(|r| (r.key.clone(), r.db, r.state.clone()))
    }

    /// `expect` with the background digest filled in: a version still
    /// [`DigestMissing::Deferred`] whose read finished gets its digest (or
    /// the reason there is none). Otherwise it stays deferred, so a
    /// whole-key plan is unchecked and asks first, with the exact reason
    /// (`unchecked_reason`): no read started, read no longer kept, another
    /// session's or another key's read, another database, still running,
    /// failed.
    ///
    /// # Errors
    /// `invalid` when the read found that the key changed since it was
    /// opened (read it again).
    fn resolve(
        &self,
        session_id: &str,
        key: &[u8],
        db: u16,
        expect: &KeyExpect,
    ) -> CmdResult<KeyExpect> {
        let KeyExpect::Present {
            key_type,
            version: Some(version),
        } = expect
        else {
            return Ok(expect.clone());
        };
        if version.digest.is_some() || version.digest_missing != Some(DigestMissing::Deferred) {
            return Ok(expect.clone());
        }
        let Some(id) = version.digest_read.as_deref() else {
            let mut version = version.clone();
            version.unchecked_reason = Some(
                "the key was too large to read when it was opened, and no background read of \
                 its content was started"
                    .into(),
            );
            return Ok(KeyExpect::Present {
                key_type: key_type.clone(),
                version: Some(version),
            });
        };
        let mut version = version.clone();
        let read = lock_reads(&self.reads)
            .get(id)
            .map(|r| (r.session_id.clone(), r.key.clone(), r.db, r.state.clone()));
        let reason = match read {
            None => Some(
                "the background read of its content is no longer kept (it was cancelled, it \
                 expired, or newer reads replaced it); read the key again"
                    .to_string(),
            ),
            Some((owner, ..)) if owner != session_id => Some(
                "the background read of its content belongs to another tab's session; read the \
                 key again"
                    .to_string(),
            ),
            Some((_, read_key, ..)) if read_key != key => Some(
                "the background read of its content was of another key; read this key again"
                    .to_string(),
            ),
            Some((_, _, read_db, _)) if read_db != db => Some(format!(
                "its content was read in the background on database {read_db}, but the tab is on \
                 database {db} now"
            )),
            Some((.., ReadState::Running)) => Some(
                "the key is too large to read when it is opened, and reading it in the \
                 background had not finished when this plan was made"
                    .to_string(),
            ),
            Some((.., ReadState::Failed(why))) => Some(format!(
                "reading its content in the background failed: {why}"
            )),
            Some((.., ReadState::Done(found))) => {
                match found {
                    FullDigest::Done(digest) => {
                        version.digest = Some(digest);
                        version.digest_missing = None;
                    }
                    // The driver words these (too large, stalled, not
                    // permitted).
                    FullDigest::Missing(missing) => version.digest_missing = Some(missing),
                    FullDigest::Changed(how) => {
                        return Err(CmdError::Invalid {
                            message: format!("{how} since it was opened; read the key again"),
                        });
                    }
                }
                None
            }
        };
        version.unchecked_reason = reason;
        Ok(KeyExpect::Present {
            key_type: key_type.clone(),
            version: Some(version),
        })
    }
}

impl Services {
    /// Locks a Redis user session.
    pub(crate) async fn redis_session_lock(
        &self,
        session_id: &str,
    ) -> CmdResult<(SessionInfo, SessionLock)> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "This Redis view")?;
        let lock = self.lock(session_id).await?;
        let is_redis = Self::locked(&lock, session_id)?
            .as_any()
            .downcast_ref::<RedisSession>()
            .is_some();
        if !is_redis {
            return Err(not_redis());
        }
        Ok((info, lock))
    }

    /// The Redis session behind a lock from [`Self::redis_session_lock`].
    pub(crate) fn redis_of<'a>(
        lock: &'a SessionLock,
        session_id: &str,
    ) -> CmdResult<&'a RedisSession> {
        Self::locked(lock, session_id)?
            .as_any()
            .downcast_ref::<RedisSession>()
            .ok_or_else(not_redis)
    }

    /// The version of a key, for whole-key changes; `None` when it does
    /// not exist. Bounded (it runs on every key open) and read on its own
    /// connection, opened after the session lock is released: the content
    /// digest only of a small key. A larger key comes back
    /// `digestMissing: deferred` with a `digestRead` id, and its digest is
    /// read right away in the background (also without the session), for
    /// the plans of whole-key changes ([`Self::redis_edit_plan`]). Cancel it
    /// with [`Self::redis_key_digest_cancel`] when the viewer closes.
    ///
    /// # Errors
    /// Unknown or non-Redis session, a malformed key, driver errors.
    pub async fn redis_key_version(
        &self,
        session_id: &str,
        key: &str,
    ) -> CmdResult<Option<KeyVersion>> {
        let key = name_bytes(key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        let reader = Self::redis_of(&lock, session_id)?.version_reader().await;
        drop(lock);
        let Some(mut version) = reader.read_open(&key).await.map_err(driver_err)? else {
            return Ok(None);
        };
        if version.digest_missing == Some(DigestMissing::Deferred) {
            version.digest_read = Some(DigestReads::start(
                &self.m2,
                session_id,
                key,
                reader,
                version.clone(),
            ));
        }
        Ok(Some(version))
    }

    /// Stops the background digest read `read_id` (from
    /// [`Self::redis_key_version`]) and forgets it; unknown ids are fine.
    pub fn redis_key_digest_cancel(&self, session_id: &str, read_id: &str) {
        self.m2.digests.cancel(session_id, read_id);
    }

    /// True when the background digest read `read_id` has finished (with a
    /// digest or a reason why there is none); false while it runs or when
    /// it is unknown. For diagnostics and tests; plans do not wait for it.
    #[must_use]
    pub fn redis_key_digest_finished(&self, session_id: &str, read_id: &str) -> bool {
        matches!(
            self.m2.digests.state(session_id, read_id),
            Some((_, _, ReadState::Done(_) | ReadState::Failed(_)))
        )
    }

    /// Plans value edits of one key and keeps the plan on the server.
    ///
    /// # Errors
    /// Read-only sessions (`readOnly`), a request against the rules
    /// (`invalid`), driver errors.
    pub async fn redis_edit_plan(
        &self,
        session_id: &str,
        request: &RedisEditRequest,
    ) -> CmdResult<Planned<RedisEditPlan>> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "Redis value editing")?;
        Self::require_writable(&info, "changing keys")?;
        let key = name_bytes(&request.key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        let redis = Self::redis_of(&lock, session_id)?;
        let db = redis.current_db().await;
        let expect = self
            .m2
            .digests
            .resolve(session_id, &key, db, &request.expect)?;
        let program = redis
            .plan_edit(&key, &expect, &request.edits)
            .await
            .map_err(edit_err)?;
        drop(lock);
        Ok(self.store_redis_edit(session_id, program))
    }

    /// Plans a stream consumer group change; applied with
    /// [`Self::redis_edit_apply`].
    ///
    /// # Errors
    /// As [`Self::redis_edit_plan`].
    pub async fn redis_stream_group_plan(
        &self,
        session_id: &str,
        key: &str,
        change: &StreamGroupChange,
    ) -> CmdResult<Planned<RedisEditPlan>> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "Stream consumer groups")?;
        Self::require_writable(&info, "changing consumer groups")?;
        let key = name_bytes(key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        let program = Self::redis_of(&lock, session_id)?
            .plan_stream_group(&key, change)
            .await
            .map_err(edit_err)?;
        drop(lock);
        Ok(self.store_redis_edit(session_id, program))
    }

    fn store_redis_edit(&self, session_id: &str, program: EditProgram) -> Planned<RedisEditPlan> {
        let plan = program.view().clone();
        let plan_id = self
            .m2
            .plans
            .put(session_id, StoredPlan::RedisEdit(Box::new(program)));
        Planned { plan_id, plan }
    }

    /// Applies a stored value editor or consumer group plan with its
    /// checks. Refused on read-only sessions; needs `confirmed` when the
    /// server classifies a command as destructive, when a whole-key change
    /// cannot be checked for concurrent changes (`unchecked`), or when the
    /// plan deletes data on a session that confirms destructive statements.
    ///
    /// # Errors
    /// As described, an unknown, used or expired plan id, driver errors.
    pub async fn redis_edit_apply(
        &self,
        session_id: &str,
        plan_id: &str,
        confirmed: bool,
    ) -> CmdResult<EditOutcome> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "Redis value editing")?;
        Self::require_writable(&info, "changing keys")?;
        let stored = self.m2.plans.peek(session_id, plan_id)?;
        let StoredPlan::RedisEdit(program) = &stored else {
            return Err(wrong_kind("redis_edit_apply", &stored));
        };
        let view = program.view();
        // A whole-key change that cannot be checked for concurrent changes
        // always asks, in every environment, and says why.
        let needs = view.risk == CommandRisk::Destructive
            || view.unchecked
            || (view.destructive && info.confirm_destructive);
        let mut statements = program.command_names();
        if view.unchecked {
            statements.extend(view.warnings.iter().cloned());
        }
        self.require_confirmation(&info, needs, confirmed, statements)?;
        // The plan is spent only once the session that runs it is ours.
        let (_, lock) = self.redis_session_lock(session_id).await?;
        let stored = self.m2.plans.take(session_id, plan_id)?;
        let StoredPlan::RedisEdit(program) = stored else {
            return Err(wrong_kind("redis_edit_apply", &stored));
        };
        let started = Instant::now();
        let result = Self::redis_of(&lock, session_id)?
            .apply_edit(&program)
            .await;
        drop(lock);
        self.record_redis_edit(&info, &program, &result, elapsed_ms(started));
        if needs && confirmed && matches!(result, Ok(EditOutcome::Applied { .. })) {
            self.record(
                &info,
                ActivityEvent::destructive_confirmed(
                    program.command_names().join(","),
                    &environment_label(&info),
                ),
            );
        }
        result.map_err(driver_err)
    }

    /// Records `data.edited` for an applied (or refused) Redis edit. Never
    /// values: the key, the operations and the counts.
    fn record_redis_edit(
        &self,
        info: &SessionInfo,
        program: &EditProgram,
        result: &Result<EditOutcome, DriverError>,
        duration_ms: u64,
    ) {
        let view = program.view();
        let counts = program.counts();
        let applied = matches!(result, Ok(EditOutcome::Applied { .. }));
        let (added, changed, removed) = if applied {
            (counts.added, counts.changed, counts.removed)
        } else {
            (0, 0, 0)
        };
        let key: String = view.key.chars().take(LOGGED_KEY_CHARS).collect();
        let mut event = ActivityEvent::builder(Category::Data, "data.edited")
            .target(key_label(view))
            .detail("engine", "redis")
            .detail("db", u64::from(view.db))
            .detail("key", key)
            .detail(
                "operations",
                serde_json::Value::from(
                    program
                        .operations()
                        .iter()
                        .map(|op| serde_json::Value::from(*op))
                        .collect::<Vec<_>>(),
                ),
            )
            .detail("rows_inserted", added)
            .detail("rows_updated", changed)
            .detail("rows_deleted", removed)
            .duration_ms(duration_ms);
        match result {
            Ok(EditOutcome::Applied { .. }) => {}
            Ok(EditOutcome::Conflict { reason, .. }) => {
                event = event
                    .outcome(EventOutcome::Failure)
                    .detail("status", "conflict")
                    .detail(
                        "reason",
                        serde_json::to_value(reason).unwrap_or(serde_json::Value::Null),
                    );
            }
            Ok(EditOutcome::Failed { command, error }) => {
                // The server message can quote a value; keep the code.
                event = event
                    .outcome(EventOutcome::Failure)
                    .detail("status", "failed")
                    .detail("command", *command)
                    .error(
                        &ErrorDetails {
                            code: error.code.clone(),
                            message: format!("the server rejected command {}", command + 1),
                        },
                        None,
                    );
            }
            Err(error) => {
                event = event
                    .outcome(EventOutcome::Failure)
                    .detail("status", "error")
                    .error(
                        &ErrorDetails {
                            code: error.code.clone(),
                            message: "the Redis change did not complete".into(),
                        },
                        None,
                    );
            }
        }
        self.record(info, event);
    }

    /// A page of a sorted set range by score or lex.
    ///
    /// # Errors
    /// Unknown or non-Redis session, malformed bounds (`invalid`), another
    /// type (`WRONGTYPE`), driver errors.
    pub async fn redis_zset_range(
        &self,
        session_id: &str,
        key: &str,
        range: &ZsetRange,
    ) -> CmdResult<Option<ZsetRangePage>> {
        let key = name_bytes(key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        Self::redis_of(&lock, session_id)?
            .zset_range(&key, range)
            .await
            .map_err(driver_err)
    }

    /// `XINFO STREAM` without entry fields.
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_stream_info(
        &self,
        session_id: &str,
        key: &str,
    ) -> CmdResult<Option<StreamInfo>> {
        let key = name_bytes(key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        Self::redis_of(&lock, session_id)?
            .stream_info(&key)
            .await
            .map_err(driver_err)
    }

    /// `XINFO GROUPS`.
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_stream_groups(
        &self,
        session_id: &str,
        key: &str,
    ) -> CmdResult<Vec<StreamGroup>> {
        let key = name_bytes(key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        Self::redis_of(&lock, session_id)?
            .stream_groups(&key)
            .await
            .map_err(driver_err)
    }

    /// `XINFO CONSUMERS`.
    ///
    /// # Errors
    /// Unknown or non-Redis session, an unknown group (`NOGROUP`), driver
    /// errors.
    pub async fn redis_stream_consumers(
        &self,
        session_id: &str,
        key: &str,
        group: &str,
    ) -> CmdResult<Vec<StreamConsumer>> {
        let key = name_bytes(key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        Self::redis_of(&lock, session_id)?
            .stream_consumers(&key, group)
            .await
            .map_err(driver_err)
    }

    /// `XPENDING key group`.
    ///
    /// # Errors
    /// As [`Self::redis_stream_consumers`].
    pub async fn redis_stream_pending_summary(
        &self,
        session_id: &str,
        key: &str,
        group: &str,
    ) -> CmdResult<PendingSummary> {
        let key = name_bytes(key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        Self::redis_of(&lock, session_id)?
            .stream_pending_summary(&key, group)
            .await
            .map_err(driver_err)
    }

    /// A page of pending entries.
    ///
    /// # Errors
    /// As [`Self::redis_stream_consumers`].
    pub async fn redis_stream_pending(
        &self,
        session_id: &str,
        key: &str,
        group: &str,
        page: &PendingQuery,
    ) -> CmdResult<PendingPage> {
        let key = name_bytes(key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        Self::redis_of(&lock, session_id)?
            .stream_pending(&key, group, page)
            .await
            .map_err(driver_err)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn deferred(read: Option<&str>) -> KeyExpect {
        KeyExpect::Present {
            key_type: varve_driver_redis::KeyType::String,
            version: Some(KeyVersion {
                key_type: varve_driver_redis::KeyType::String,
                length: Some(5),
                digest: None,
                digest_missing: Some(DigestMissing::Deferred),
                expire_at_ms: None,
                digest_read: read.map(str::to_string),
                db: Some(3),
                unchecked_reason: None,
            }),
        }
    }

    fn reason_of(expect: &KeyExpect) -> Option<String> {
        match expect {
            KeyExpect::Present {
                version: Some(v), ..
            } => v.unchecked_reason.clone(),
            _ => None,
        }
    }

    #[test]
    fn unchecked_plans_say_exactly_why() -> Result<(), CmdError> {
        let reads = DigestReads::default();
        let entry = |session: &str, key: &[u8], db: u16, state: ReadState| DigestRead {
            session_id: session.into(),
            key: key.to_vec(),
            db,
            started: Instant::now(),
            state,
            task: None,
        };
        {
            let mut map = lock_reads(&reads.reads);
            map.insert("running".into(), entry("s1", b"k", 3, ReadState::Running));
            map.insert("other".into(), entry("s2", b"k", 3, ReadState::Running));
            map.insert("key".into(), entry("s1", b"x", 3, ReadState::Running));
            map.insert("db".into(), entry("s1", b"k", 4, ReadState::Running));
            map.insert(
                "failed".into(),
                entry("s1", b"k", 3, ReadState::Failed("lost".into())),
            );
            map.insert(
                "done".into(),
                entry(
                    "s1",
                    b"k",
                    3,
                    ReadState::Done(FullDigest::Done("s-1".into())),
                ),
            );
            map.insert(
                "stalled".into(),
                entry(
                    "s1",
                    b"k",
                    3,
                    ReadState::Done(FullDigest::Missing(DigestMissing::Stalled)),
                ),
            );
            map.insert(
                "changed".into(),
                entry(
                    "s1",
                    b"k",
                    3,
                    ReadState::Done(FullDigest::Changed("The key is now a hash".into())),
                ),
            );
        }
        let why = |read: Option<&str>| -> Result<Option<String>, CmdError> {
            Ok(reason_of(&reads.resolve("s1", b"k", 3, &deferred(read))?))
        };
        let has = |read: Option<&str>, text: &str| -> Result<(), CmdError> {
            let reason = why(read)?.unwrap_or_default();
            assert!(reason.contains(text), "{read:?}: {reason}");
            Ok(())
        };
        has(None, "no background read")?;
        has(Some("evicted"), "no longer kept")?;
        has(Some("other"), "another tab's session")?;
        has(Some("key"), "another key")?;
        has(Some("db"), "database 4")?;
        has(Some("running"), "had not finished")?;
        has(Some("failed"), "failed: lost")?;
        assert_eq!(why(Some("done"))?, None);
        let done = reads.resolve("s1", b"k", 3, &deferred(Some("done")))?;
        assert!(matches!(
            done,
            KeyExpect::Present {
                version: Some(KeyVersion {
                    digest: Some(_),
                    ..
                }),
                ..
            }
        ));
        let stalled = reads.resolve("s1", b"k", 3, &deferred(Some("stalled")))?;
        assert!(matches!(
            stalled,
            KeyExpect::Present {
                version: Some(KeyVersion {
                    digest_missing: Some(DigestMissing::Stalled),
                    unchecked_reason: None,
                    ..
                }),
                ..
            }
        ));
        assert!(matches!(
            reads.resolve("s1", b"k", 3, &deferred(Some("changed"))),
            Err(CmdError::Invalid { .. })
        ));
        Ok(())
    }

    #[test]
    fn requests_and_errors_have_ui_shapes() -> Result<(), serde_json::Error> {
        let request: RedisEditRequest = serde_json::from_value(json!({
            "key": "user:1",
            "expect": { "state": "absent" },
            "edits": [{ "op": "setAdd", "members": [{ "t": "text", "v": "a" }] }]
        }))?;
        assert_eq!(request.edits.len(), 1);
        let read_only = driver_err(DriverError::new(ErrorKind::ReadOnly, "no"));
        assert_eq!(serde_json::to_value(read_only)?["kind"], "readOnly");
        let range =
            driver_err(DriverError::new(ErrorKind::Query, "bad").with_code("INVALID_RANGE"));
        assert_eq!(serde_json::to_value(range)?["kind"], "invalid");
        let other = driver_err(DriverError::new(ErrorKind::Query, "x").with_code("WRONGTYPE"));
        assert_eq!(
            serde_json::to_value(other)?["error"]["error"]["code"],
            "WRONGTYPE"
        );
        assert_eq!(
            serde_json::to_value(edit_err(EditError::Invalid("bad".into())))?["kind"],
            "invalid"
        );
        Ok(())
    }

    #[test]
    fn outcomes_serialize_for_the_ui() -> Result<(), serde_json::Error> {
        let conflict = EditOutcome::Conflict {
            edit: Some(2),
            reason: varve_driver_redis::ConflictReason::ValueChanged,
            message: "m".into(),
        };
        assert_eq!(
            serde_json::to_value(&conflict)?,
            json!({ "status": "conflict", "edit": 2, "reason": "valueChanged", "message": "m" })
        );
        let applied = EditOutcome::Applied {
            replies: vec![varve_driver::Value::Int(1)],
        };
        assert_eq!(serde_json::to_value(&applied)?["status"], "applied");
        Ok(())
    }
}
