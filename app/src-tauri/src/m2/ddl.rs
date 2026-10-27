//! Running DDL from the object browser ("run this DDL"), including the
//! MySQL and MariaDB DDL session header
//! (`-- varve-session: key=value; ...`, see
//! `varve_catalog::mysql::session`).
//!
//! The DDL always runs through the session manager
//! ([`crate::sessions::SessionManager::execute_with`]): classification,
//! destructive confirmation, open transaction refusal and the query log
//! apply as in the SQL editor. A header's session variables
//! (`character_set_client`, `collation_connection`, `sql_mode`,
//! `time_zone`; `character_set_results` is ignored because results always
//! arrive as utf8mb4) apply for this call only, through `MysqlSession::execute_with_session` under the same
//! session lock. A `collation_database` entry means the object must be
//! created while its database has that default collation: the database is
//! altered to it before the script and back to its own collation
//! afterwards (also when the script fails or is cancelled). Because that
//! briefly changes the database for every other client, it always needs
//! confirmation, runs are serialized per database, and both
//! `ALTER DATABASE` statements are logged as queries.

use std::time::{Duration, Instant};

use serde::Serialize;
use varve_activity::{ActivityEvent, Category, Outcome as EventOutcome, SqlDialect};
use varve_catalog::CatalogError;
use varve_catalog::mysql::{DdlSession, ddl_session_header, strip_ddl_session_header};
use varve_catalog::sql::{ident, string};
use varve_core::Engine;
use varve_driver::{Dialect, ExecOptions, ExecOutput, Session, StatementKind, classify};
use varve_driver_mysql::SessionOverrides;

use super::editors::{confirmation_labels, transaction_effect};
use super::{CollationKey, PendingRestore, elapsed_ms, error_details, sql_dialect, unsupported};
use crate::commands::{CmdError, Services};
use crate::sessions::{ExecSpec, LockedSession, SessionError, SessionInfo, open_transaction};

type CmdResult<T> = Result<T, CmdError>;

/// What [`Services::ddl_apply`] ran.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DdlApplied {
    /// The server output of the DDL itself.
    pub output: ExecOutput,
    /// The session settings the DDL ran with, from its header.
    pub session: Option<DdlSession>,
    /// The database whose default collation was changed for the run and
    /// restored afterwards.
    pub collation_database: Option<String>,
    /// Problems the user must know about, for example a collation that
    /// could not be restored (see `ddl_pending_restores`).
    pub warnings: Vec<String>,
    /// Wall time.
    pub duration_ms: u64,
}

/// The per-call session variables of a header.
pub(crate) fn overrides(session: &DdlSession) -> SessionOverrides {
    SessionOverrides {
        character_set_client: session.character_set_client.clone(),
        collation_connection: session.collation_connection.clone(),
        sql_mode: session.sql_mode.clone(),
        time_zone: session.time_zone.clone(),
    }
}

/// True when a header sets a session variable the run applies.
/// `character_set_results` is not applied: results always arrive as
/// utf8mb4, and it does not change the object a DDL creates.
pub(crate) fn has_variables(session: &DdlSession) -> bool {
    let applied = overrides(session);
    applied.character_set_client.is_some()
        || applied.collation_connection.is_some()
        || applied.sql_mode.is_some()
        || applied.time_zone.is_some()
}

/// Restore attempts after a DDL run, and the pause before each retry.
const RESTORE_RETRIES: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(250),
    Duration::from_secs(1),
];

impl Services {
    /// Runs DDL from the object browser on the user's session, through the
    /// session manager (classification, destructive confirmation, the
    /// query log and `destructive.confirmed` / `destructive.blocked`), with
    /// the MySQL session header applied for this call only.
    ///
    /// `database` names the object's database; it is needed only when the
    /// header has `collation_database`. Read-only sessions refuse; a session
    /// with an open transaction refuses (MySQL would commit it, PostgreSQL
    /// would make the DDL part of it); destructive statements need
    /// `confirmed` on sessions that confirm them, and a `collation_database`
    /// header always does. Runs that change the same database's collation
    /// are serialized; its own collation is read once and restored with
    /// retries; when that still fails the run reports a warning, the
    /// database is listed by [`Self::ddl_pending_restores`], and
    /// [`Self::ddl_restore_collation`] retries. Logged as `schema.changed`
    /// with `source: ddl`.
    ///
    /// # Errors
    /// As described; `invalid` for a malformed header, a header on a
    /// PostgreSQL session, transaction control, or a missing `database`;
    /// driver errors.
    pub async fn ddl_apply(
        &self,
        session_id: &str,
        ddl: &str,
        database: Option<&str>,
        confirmed: bool,
    ) -> CmdResult<DdlApplied> {
        // A Cancel from here on (also while the run waits for the tab)
        // stops it before the next step.
        let cancels = self.m2.cancel_count(session_id);
        let info = self.session_info(session_id)?;
        let Some(dialect) = sql_dialect(info.engine) else {
            return Err(unsupported("Running DDL needs a SQL session"));
        };
        Self::require_writable(&info, "running DDL")?;
        let script = strip_ddl_session_header(ddl);
        let invalid = |message: &str| {
            self.ddl_refused(
                &info,
                script,
                CmdError::Invalid {
                    message: message.into(),
                },
            )
        };
        let header =
            ddl_session_header(ddl).map_err(|e| self.ddl_refused(&info, script, e.into()))?;
        if header.is_some() && dialect != Dialect::Mysql {
            return Err(invalid("DDL session headers are for MySQL and MariaDB."));
        }
        let kinds = classify(script, dialect);
        if kinds.is_empty() {
            return Err(invalid("There is no DDL to run."));
        }
        if kinds.contains(&StatementKind::Transaction) {
            return Err(invalid("The DDL may not control the transaction."));
        }
        let collation = match header.as_ref().and_then(|h| h.collation_database.clone()) {
            Some(collation) => {
                let database = database
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .ok_or_else(|| {
                        invalid(
                            "The DDL must run while its database has a given default \
                             collation; name the database.",
                        )
                    })?;
                Some((database.to_string(), collation))
            }
            None => None,
        };
        let classified = kinds.iter().any(|k| k.is_destructive());
        let mut labels = confirmation_labels(&kinds);
        if collation.is_some() {
            labels.push("AlterDatabase".into());
        }
        self.require_confirmation(
            &info,
            collation.is_some() || (info.confirm_destructive && classified),
            confirmed,
            labels.clone(),
        )?;
        let effect = transaction_effect(info.engine);
        let spec = ExecSpec {
            destructive: collation.is_some().then(|| labels.clone()),
            outside_transaction: Some(effect.clone()),
            rollback_on_error: false,
            overrides: header.as_ref().filter(|h| has_variables(h)).map(overrides),
        };

        let started = Instant::now();
        let mut warnings = Vec::new();
        let result = match &collation {
            None => match self.sessions.lock_for_scripts(&info.id).await {
                Ok(locked) if self.m2.cancel_count(&info.id) == cancels => locked
                    .execute_with(script, confirmed, &ExecOptions::default(), &spec)
                    .await
                    .map_err(CmdError::from),
                Ok(_) => Err(cancelled_error()),
                Err(error) => Err(error.into()),
            },
            Some((database, wanted)) => {
                self.run_with_collation(
                    &info,
                    script,
                    confirmed,
                    &spec,
                    &effect,
                    database,
                    wanted,
                    &mut warnings,
                    cancels,
                )
                .await
            }
        };
        let duration_ms = elapsed_ms(started);

        let mut event = ActivityEvent::builder(Category::Schema, "schema.changed")
            .detail("source", "ddl")
            .detail(
                "kinds",
                kinds
                    .iter()
                    .map(|k| format!("{k:?}").to_ascii_lowercase())
                    .collect::<Vec<_>>(),
            )
            .detail("session_header", header.is_some())
            .detail_opt(
                "collation_database",
                collation.as_ref().map(|(d, _)| d.clone()),
            )
            .duration_ms(duration_ms)
            .statement(script, SqlDialect::from(info.engine));
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

        let output = match result {
            Ok(output) => output,
            Err(error) => return Err(with_notes(error, &warnings)),
        };
        Ok(DdlApplied {
            output,
            session: header,
            collation_database: collation.map(|(d, _)| d),
            warnings,
            duration_ms,
        })
    }

    /// Runs DDL while its database has `wanted` as default collation.
    ///
    /// Serialized per server (as the server identifies itself, whatever
    /// route reaches it) and database, then everything runs under one lock
    /// of the tab's session, so no other call on the tab can start a
    /// transaction in between: the current collation is read from the
    /// server, changed to `wanted` when it differs, the script runs, and
    /// the database goes back to its own collation, with retries.
    ///
    /// A pending restore of an earlier run is used as the own collation
    /// only when it was recorded through this route on a verified server
    /// and the database is at a collation that run left; otherwise the run
    /// is refused until the restore is done or dismissed. The entry is
    /// extended (never replaced) before the first ALTER, and put back as it
    /// was when that ALTER changes nothing, so it stays valid. Both ALTERs
    /// give up after [`LOCK_WAIT_TIMEOUT_S`] when another connection holds
    /// the database's metadata lock. A transaction found open before the
    /// restore keeps the restore pending instead of committing it. A
    /// Cancel since `cancels` stops the run before the next step.
    #[allow(clippy::too_many_arguments)]
    async fn run_with_collation(
        &self,
        info: &SessionInfo,
        script: &str,
        confirmed: bool,
        spec: &ExecSpec,
        effect: &str,
        database: &str,
        wanted: &str,
        warnings: &mut Vec<String>,
        cancels: u64,
    ) -> CmdResult<ExecOutput> {
        let identity = self.server_identity(info).await?;
        let key = (identity.server.clone(), database.to_string());
        let serial = self.m2.collation_lock(&key);
        let _serial = serial.lock().await;
        let locked = self.sessions.lock_for_scripts(&info.id).await?;
        let session = locked.session().ok_or_else(|| SessionError::NotFound {
            session_id: info.id.clone(),
        })?;
        let cancelled = || self.m2.cancel_count(&info.id) != cancels;
        if cancelled() {
            return Err(cancelled_error());
        }
        let current = database_collation(session, database).await?;
        let previous = self.m2.pending_restore(&key);
        let original = match &previous {
            None => current.clone(),
            Some(pending) => {
                if let Some(reason) = pending_conflict(pending, &identity, &current) {
                    return Err(CmdError::Invalid {
                        message: format!(
                            "A collation restore is pending for database {database} (back to {}): \
                             {reason}",
                            pending.collation
                        ),
                    });
                }
                pending.collation.clone()
            }
        };
        let mut altered = false;
        if !current.eq_ignore_ascii_case(wanted) {
            let mut entry = previous.clone().unwrap_or_else(|| PendingRestore {
                server: identity.server.clone(),
                route: identity.route.clone(),
                database: database.to_string(),
                collation: original.clone(),
                temporaries: Vec::new(),
                temporary: None,
                verified: identity.verified,
                connection_id: info.connection_id.clone(),
                recorded_at: varve_activity::now_ms(),
            });
            if !entry.left_at(wanted) {
                entry.temporaries.push(wanted.to_string());
            }
            self.m2.set_pending_restore(&key, Some(entry));
            let alter = ExecSpec {
                outside_transaction: Some(effect.to_string()),
                ..ExecSpec::default()
            };
            match timed_alter(&locked, &alter_collation(database, wanted)?, &alter).await {
                Ok((_, reset_problem)) => {
                    altered = true;
                    warnings.extend(reset_problem);
                }
                Err(error) => {
                    let unchanged = database_collation(session, database)
                        .await
                        .is_ok_and(|now| now.eq_ignore_ascii_case(&current));
                    if unchanged {
                        // Nothing changed: the entry is exactly as before
                        // (or gone, when there was none), still valid.
                        self.m2.set_pending_restore(&key, previous);
                        return Err(error);
                    }
                    altered = true;
                    warnings.push(format!("The collation change reported an error: {error}"));
                }
            }
        }
        let result = if cancelled() {
            Err(cancelled_error())
        } else {
            locked
                .execute_with(script, confirmed, &ExecOptions::default(), spec)
                .await
                .map_err(CmdError::from)
        };
        // A Cancel aimed at the script stops the script; one that comes
        // during the restore stops the restore.
        let restore_cancels = self.m2.cancel_count(&info.id);
        let temporaries = match self.m2.pending_restore(&key) {
            Some(entry) => entry.temporaries,
            None if altered => vec![wanted.to_string()],
            None => Vec::new(),
        };
        if let Err(problem) = self
            .restore_collation(
                info,
                &locked,
                &key,
                &original,
                &temporaries,
                restore_cancels,
            )
            .await
        {
            warnings.push(problem);
        }
        result
    }

    /// Logs a DDL run refused before it reached the server as a failed
    /// `schema.changed`, and returns the error.
    fn ddl_refused(&self, info: &SessionInfo, script: &str, error: CmdError) -> CmdError {
        self.record(
            info,
            ActivityEvent::builder(Category::Schema, "schema.changed")
                .outcome(EventOutcome::Failure)
                .detail("source", "ddl")
                .detail("refused", error.to_string())
                .statement(script, SqlDialect::from(info.engine)),
        );
        error
    }

    /// Databases whose default collation could not be restored after a
    /// DDL run.
    #[must_use]
    pub fn ddl_pending_restores(&self) -> Vec<PendingRestore> {
        self.m2.pending_restores()
    }

    /// Retries restoring a database's own collation after a DDL run whose
    /// restore failed. Only a restore recorded on this session's server is
    /// applied, and only while the database is at a collation that run left
    /// (or already back at its own, which just clears the entry). A restore
    /// on a server without a verified identity, or recorded through another
    /// route, needs `confirmed`: the call first rejects with
    /// `confirmationRequired` saying that Varve cannot prove it is the same
    /// server.
    ///
    /// # Errors
    /// Unknown session, read-only session, nothing pending for this server
    /// and database (`notFound`), a stale entry (`invalid`), confirmation
    /// needed, or the restore failing again (`internal`).
    pub async fn ddl_restore_collation(
        &self,
        session_id: &str,
        database: &str,
        confirmed: bool,
    ) -> CmdResult<()> {
        let info = self.session_info(session_id)?;
        Self::require_writable(&info, "restoring a database collation")?;
        let identity = self.server_identity(&info).await?;
        let key = (identity.server.clone(), database.to_string());
        let serial = self.m2.collation_lock(&key);
        let _serial = serial.lock().await;
        let pending = self
            .m2
            .pending_restore(&key)
            .ok_or_else(|| CmdError::NotFound {
                message: format!(
                    "No collation is waiting to be restored for {database} on this server."
                ),
            })?;
        let locked = self.sessions.lock_for_scripts(&info.id).await?;
        let session = locked.session().ok_or_else(|| SessionError::NotFound {
            session_id: info.id.clone(),
        })?;
        let current = database_collation(session, database).await?;
        if current.eq_ignore_ascii_case(&pending.collation) {
            self.m2.set_pending_restore(&key, None);
            return Ok(());
        }
        if !pending.left_at(&current) {
            return Err(CmdError::Invalid {
                message: format!(
                    "Database {database} is at {current}, not at a collation the failed run left \
                     it at ({}), so this restore may belong to another server or the database \
                     was changed since. It was not applied; dismiss it if the database is as it \
                     should be.",
                    pending.temporaries.join(", ")
                ),
            });
        }
        let proven = pending.verified && pending.route == identity.route;
        let label = self.unverified_restore_label(&info, &identity, &pending, &current);
        self.require_confirmation(&info, !proven, confirmed, vec![label])?;
        let cancels = self.m2.cancel_count(&info.id);
        self.restore_collation(
            &info,
            &locked,
            &key,
            &pending.collation,
            &pending.temporaries,
            cancels,
        )
        .await
        .map_err(|message| CmdError::Internal { message })
    }

    /// The confirmation text for restoring a collation on a server that
    /// cannot be identified for sure (or reached through another route than
    /// the one that recorded the restore): says so plainly, with both
    /// routes, the database, its current and target collation, who
    /// recorded it and when (ISO 8601, UTC).
    fn unverified_restore_label(
        &self,
        info: &SessionInfo,
        identity: &ServerIdentity,
        pending: &PendingRestore,
        current: &str,
    ) -> String {
        let recorder = if pending.connection_id == info.connection_id {
            "this connection".to_string()
        } else {
            match self.records.connection(&pending.connection_id) {
                Ok(profile) => format!("the connection \"{}\"", profile.name),
                Err(_) => "a connection that no longer exists".to_string(),
            }
        };
        let when = iso_utc(pending.recorded_at)
            .map(|at| format!(" at {at}"))
            .unwrap_or_default();
        let route = if pending.route.is_empty() || pending.route == identity.route {
            format!("through {}", identity.route)
        } else {
            format!(
                "through {} (this tab connects through {})",
                pending.route, identity.route
            )
        };
        format!(
            "Varve cannot prove that this is the server the restore was recorded on: it was \
             recorded by {recorder}{when} {route}. Database {}: change its default collation \
             from {current} back to {}.",
            pending.database, pending.collation
        )
    }

    /// Forgets a pending restore without touching the server, for entries
    /// that are stale (a recreated server or database, an entry of an older
    /// version). Waits for a DDL run on the same key to finish first.
    /// Logged as `schema.collation_restore_dismissed`.
    ///
    /// # Errors
    /// `notFound` when no such entry exists.
    pub async fn ddl_dismiss_pending_restore(&self, server: &str, database: &str) -> CmdResult<()> {
        let key = (server.to_string(), database.to_string());
        let serial = self.m2.collation_lock(&key);
        let _serial = serial.lock().await;
        let pending = self
            .m2
            .pending_restore(&key)
            .ok_or_else(|| CmdError::NotFound {
                message: format!("No collation restore is pending for {database}."),
            })?;
        self.m2.set_pending_restore(&key, None);
        let event = ActivityEvent::builder(Category::Schema, "schema.collation_restore_dismissed")
            .target(database.to_string())
            .detail("database", database.to_string())
            .detail("collation", pending.collation)
            .connection(pending.connection_id);
        match event.build() {
            Ok(event) => {
                if let Err(error) = self.activity.record(event) {
                    tracing::warn!(%error, "could not record an activity event");
                }
            }
            Err(error) => tracing::warn!(%error, "invalid activity event"),
        }
        Ok(())
    }

    /// How the server identifies itself, for collation bookkeeping, and
    /// the route the tab reaches it through.
    ///
    /// - MySQL: `@@server_uuid`, kept across restarts and container
    ///   recreates (verified). Copies of one baked data directory share it,
    ///   so a restore also records its route and the `temporary` check
    ///   guards the rest.
    /// - MariaDB has no unique id. The host name, port, data directory and
    ///   server id repeat across containers from one image, and
    ///   `@@server_uid` (a hash of a MAC address and the port) can repeat
    ///   across docker hosts and changes when a container is recreated, so
    ///   the identity is unverified: such restores never run without the
    ///   user's confirmation.
    ///
    /// The server part (without the route) keys the serialization lock and
    /// the pending entry, so every route to one server shares them.
    ///
    /// # Errors
    /// Query errors as they are (a lost connection is not an identity
    /// problem); `unsupported` when the server reports nothing usable.
    async fn server_identity(&self, info: &SessionInfo) -> CmdResult<ServerIdentity> {
        let (sql, verified) = match info.engine {
            Engine::Mysql => ("SELECT @@server_uuid AS id", true),
            Engine::Mariadb => (
                "SELECT CONCAT_WS('|', @@server_uid, @@hostname, @@port, @@datadir, @@server_id) \
                 AS id",
                false,
            ),
            Engine::Postgres | Engine::Redis => {
                return Err(unsupported("DDL session headers are for MySQL and MariaDB"));
            }
        };
        let lock = self.lock(&info.id).await?;
        let session = Self::locked(&lock, &info.id)?;
        let rows = varve_catalog::rows::query(session, sql).await?;
        let id = rows
            .single("the server identity")?
            .text("id")?
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                unsupported(
                    "The server does not report an identity Varve can check, so the database \
                     collation is not changed. Run the DDL without the collation_database header.",
                )
            })?;
        let engine = format!("{:?}", info.engine).to_ascii_lowercase();
        Ok(ServerIdentity {
            server: format!("{engine}:{id}"),
            route: route_of(&info.identity),
            verified,
        })
    }

    /// Restores a database's collation on a locked session; clears the
    /// pending entry once the database has it. Each attempt checks that the
    /// database is still at `temporary` (otherwise someone else changed it:
    /// nothing is touched) and that no transaction is open (the ALTER would
    /// commit it). A lock wait timeout or a cancel ends the attempts at once
    /// (one cancel is enough); other errors are retried. On failure the
    /// restore stays pending, `schema.collation_restore_failed` is recorded
    /// and the warning returned.
    async fn restore_collation(
        &self,
        info: &SessionInfo,
        locked: &LockedSession<'_>,
        key: &CollationKey,
        original: &str,
        temporaries: &[String],
        cancels: u64,
    ) -> Result<(), String> {
        let (_, database) = key;
        let statement = alter_collation(database, original).map_err(|e| e.to_string())?;
        let cancelled = || self.m2.cancel_count(&info.id) != cancels;
        let mut last = String::new();
        for pause in RESTORE_RETRIES {
            if !pause.is_zero() {
                tokio::time::sleep(pause).await;
            }
            // One Cancel ends the restore in any phase (it stays pending).
            if cancelled() {
                last = "cancelled".into();
                break;
            }
            let Some(session) = locked.session() else {
                last = "the session closed".into();
                break;
            };
            match database_collation(session, database).await {
                Ok(now) if now.eq_ignore_ascii_case(original) => {
                    self.m2.set_pending_restore(key, None);
                    return Ok(());
                }
                Ok(now) if !temporaries.iter().any(|t| t.eq_ignore_ascii_case(&now)) => {
                    last = format!("the database is now at {now}, changed by someone else");
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    last = error.to_string();
                    continue;
                }
            }
            match open_transaction(info.engine, session).await {
                Ok(Some(true)) => {
                    last = "a transaction is open on the session, and the ALTER DATABASE would \
                            commit it"
                        .into();
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    last = error.to_string();
                    continue;
                }
            }
            if cancelled() {
                last = "cancelled".into();
                break;
            }
            match timed_alter(locked, &statement, &ExecSpec::default()).await {
                Ok((_, reset_problem)) => {
                    if let Some(problem) = reset_problem {
                        tracing::warn!(%problem, "after restoring a collation");
                    }
                    self.m2.set_pending_restore(key, None);
                    return Ok(());
                }
                Err(error) => {
                    let stop = stops_retries(&error) || cancelled();
                    last = error.to_string();
                    if stop {
                        break;
                    }
                }
            }
        }
        let warning = format!(
            "The default collation of database {database} is still changed: restoring \
             {original} failed ({last}). Retry with ddl_restore_collation, or run: {statement}"
        );
        self.record(
            info,
            ActivityEvent::builder(Category::Schema, "schema.collation_restore_failed")
                .outcome(EventOutcome::Failure)
                .target(database.clone())
                .detail("database", database.clone())
                .detail("collation", original.to_string()),
        );
        Err(warning)
    }
}

/// A server identity for collation bookkeeping.
struct ServerIdentity {
    /// The server as it identifies itself (lock and entry key).
    server: String,
    /// The route the tab reaches it through.
    route: String,
    /// True when the server reported a unique id of its own.
    verified: bool,
}

/// Why a run may not use a pending restore, or `None` when it may: the
/// entry must come from a verified server through the same route, with
/// the database at a collation a run of it left.
fn pending_conflict(
    pending: &PendingRestore,
    identity: &ServerIdentity,
    current: &str,
) -> Option<String> {
    if !pending.verified || !identity.verified {
        return Some(
            "this server cannot be identified for sure. Restore it (Varve asks you to confirm) \
             or dismiss it before running more DDL with a collation header."
                .into(),
        );
    }
    if pending.route != identity.route {
        return Some(format!(
            "it was recorded through {}, and this tab connects through {}. Restore it (Varve \
             asks you to confirm) or dismiss it before running more DDL with a collation header.",
            if pending.route.is_empty() {
                "an unknown route"
            } else {
                pending.route.as_str()
            },
            identity.route
        ));
    }
    if !pending.left_at(current) {
        return Some(format!(
            "it does not match the database, which is at {current}, not at a collation that run \
             left it at ({}). It may belong to another server with the same identity (a cloned \
             data directory) or the database changed since. Dismiss it if the database is as it \
             should be, then run the DDL again.",
            pending.temporaries.join(", ")
        ));
    }
    None
}

/// The error of a run stopped by a Cancel before its next step.
fn cancelled_error() -> CmdError {
    CmdError::Session {
        error: SessionError::Driver {
            error: varve_driver::DriverError::new(
                varve_driver::ErrorKind::Cancelled,
                "Cancelled before the DDL ran; nothing was changed.",
            ),
        },
    }
}

/// A unix millisecond time as ISO 8601 in UTC (`2026-10-19T12:34:56Z`), or
/// `None` for 0 and times before 1970.
fn iso_utc(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    let secs = ms / 1000;
    let days = secs / 86_400;
    let rest = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    ))
}

/// The route a tab reaches its server by (host, port, SSH bastion), from
/// its connection identity.
fn route_of(identity: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(identity).unwrap_or_default();
    let text = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => s.to_ascii_lowercase(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    };
    let ssh = &value["ssh"];
    let via = if ssh["enabled"].as_bool() == Some(true) {
        format!(" via {}:{}", text(&ssh["host"]), text(&ssh["port"]))
    } else {
        String::new()
    };
    format!("{}:{}{via}", text(&value["host"]), text(&value["port"]))
}

/// Seconds an ALTER DATABASE waits for the database's metadata lock.
pub const LOCK_WAIT_TIMEOUT_S: u32 = 2;

/// Runs an `ALTER DATABASE` with `lock_wait_timeout` set to
/// [`LOCK_WAIT_TIMEOUT_S`] for the session, restoring the session's value
/// afterwards, so a database in use makes it fail quickly instead of
/// blocking every new statement on that database.
///
/// Returns the output and, when the ALTER went through but the session's
/// value could not be put back, a note about that (the ALTER is not
/// undone).
async fn timed_alter(
    locked: &LockedSession<'_>,
    statement: &str,
    spec: &ExecSpec,
) -> CmdResult<(ExecOutput, Option<String>)> {
    let session = locked.session().ok_or_else(|| SessionError::NotFound {
        session_id: locked.info().id.clone(),
    })?;
    let rows =
        varve_catalog::rows::query(session, "SELECT @@SESSION.lock_wait_timeout AS t").await?;
    let previous = rows
        .single("the lock wait timeout")?
        .int("t")?
        .filter(|t| *t > 0)
        .ok_or_else(|| CatalogError::Unexpected {
            message: "the session reports no lock_wait_timeout".into(),
        })?;
    varve_catalog::rows::run(
        session,
        &format!("SET SESSION lock_wait_timeout = {LOCK_WAIT_TIMEOUT_S}"),
    )
    .await?;
    let result = locked
        .execute_with(statement, true, &ExecOptions::default(), spec)
        .await;
    let reset = varve_catalog::rows::run(
        session,
        &format!("SET SESSION lock_wait_timeout = {previous}"),
    )
    .await;
    match result {
        Ok(output) => Ok((
            output,
            reset.err().map(|error| {
                format!(
                    "The session's lock_wait_timeout could not be set back to {previous} ({error}); \
                     it stays at {LOCK_WAIT_TIMEOUT_S} s for this tab until it reconnects."
                )
            }),
        )),
        Err(SessionError::Driver { mut error }) if is_lock_wait_timeout(&error) => {
            error.detail = Some(format!(
                "The database is in use: another connection holds a lock on it, so the ALTER \
                 DATABASE gave up after {LOCK_WAIT_TIMEOUT_S} s instead of blocking it. Nothing \
                 was changed; try again when it is free."
            ));
            Err(SessionError::Driver { error }.into())
        }
        Err(other) => Err(other.into()),
    }
}

/// MySQL and MariaDB `ER_LOCK_WAIT_TIMEOUT` (1205).
fn is_lock_wait_timeout(error: &varve_driver::DriverError) -> bool {
    error
        .code
        .as_deref()
        .and_then(|c| c.split_whitespace().next())
        .is_some_and(|n| n == "1205")
}

/// True for errors after which retrying a restore makes no sense: the
/// user cancelled, or the database is in use.
fn stops_retries(error: &CmdError) -> bool {
    match error {
        CmdError::Session {
            error: SessionError::Driver { error },
        } => error.kind == varve_driver::ErrorKind::Cancelled || is_lock_wait_timeout(error),
        _ => false,
    }
}

/// The default collation of a database, read on a locked session.
async fn database_collation(session: &dyn Session, database: &str) -> CmdResult<String> {
    let sql = format!(
        "SELECT DEFAULT_COLLATION_NAME AS collation_name FROM information_schema.SCHEMATA \
         WHERE SCHEMA_NAME = {}",
        string(Dialect::Mysql, database)?
    );
    let rows = varve_catalog::rows::query(session, &sql).await?;
    if rows.is_empty() {
        return Err(CatalogError::NotFound {
            message: format!("database {database} does not exist"),
        }
        .into());
    }
    Ok(rows
        .single("the database collation")?
        .text("collation_name")?
        .ok_or_else(|| CatalogError::Unexpected {
            message: "the database has no default collation".into(),
        })?)
}

/// Adds warnings (a collation restore left pending, ...) to an error,
/// keeping its kind: the UI tells a closed session, a needed confirmation or
/// a refused input by the kind, so the notes go into the error's free text.
/// Kinds without free text (a closed session, a needed confirmation) stay as
/// they are; the pending restore notice shows those warnings.
fn with_notes(mut error: CmdError, notes: &[String]) -> CmdError {
    if notes.is_empty() {
        return error;
    }
    let note = notes.join("\n");
    match &mut error {
        CmdError::Session {
            error: SessionError::Driver { error: driver },
        }
        | CmdError::Catalog {
            error: CatalogError::Driver { error: driver },
        } => {
            driver.detail = Some(match driver.detail.take() {
                Some(detail) => format!("{detail}\n{note}"),
                None => note,
            });
        }
        CmdError::Session {
            error: SessionError::OpenTransaction { message },
        }
        | CmdError::Catalog {
            error:
                CatalogError::NotFound { message }
                | CatalogError::Unsupported { message, .. }
                | CatalogError::Invalid { message, .. }
                | CatalogError::Unexpected { message },
        }
        | CmdError::Invalid { message }
        | CmdError::NotFound { message }
        | CmdError::Keychain { message, .. }
        | CmdError::Tunnel { message, .. }
        | CmdError::ReconnectRequired { message }
        | CmdError::ReadOnly { message }
        | CmdError::Internal { message } => {
            message.push('\n');
            message.push_str(&note);
        }
        _ => {}
    }
    error
}

/// `ALTER DATABASE <db> COLLATE <collation>`.
fn alter_collation(database: &str, collation: &str) -> CmdResult<String> {
    Ok(format!(
        "ALTER DATABASE {} COLLATE {}",
        ident(Dialect::Mysql, database)?,
        ident(Dialect::Mysql, collation)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_keep_the_error_kind() {
        let notes = vec!["The collation of shop is still latin1_bin.".to_string()];
        // A closed session stays a closed session, so the UI offers Reconnect.
        let closed = with_notes(
            CmdError::Session {
                error: SessionError::NotFound {
                    session_id: "s1".into(),
                },
            },
            &notes,
        );
        assert!(matches!(
            closed,
            CmdError::Session {
                error: SessionError::NotFound { .. }
            }
        ));
        let invalid = with_notes(
            CmdError::Invalid {
                message: "bad".into(),
            },
            &notes,
        );
        assert!(
            matches!(&invalid, CmdError::Invalid { message } if message == "bad\nThe collation of shop is still latin1_bin.")
        );
        let catalog = with_notes(
            CmdError::Catalog {
                error: CatalogError::Driver {
                    error: varve_driver::DriverError::new(varve_driver::ErrorKind::Query, "x"),
                },
            },
            &notes,
        );
        assert!(matches!(
            &catalog,
            CmdError::Catalog { error: CatalogError::Driver { error } }
                if error.detail.as_deref() == Some("The collation of shop is still latin1_bin.")
        ));
    }

    #[test]
    fn headers_map_to_overrides() -> Result<(), CatalogError> {
        let ddl = "-- varve-session: character_set_client=latin1; sql_mode=ANSI_QUOTES; \
                   collation_database=latin1_swedish_ci\nCREATE PROCEDURE p() BEGIN END";
        let header = ddl_session_header(ddl)?.ok_or(CatalogError::Unexpected {
            message: "no header".into(),
        })?;
        let o = overrides(&header);
        assert_eq!(o.character_set_client.as_deref(), Some("latin1"));
        assert_eq!(o.sql_mode.as_deref(), Some("ANSI_QUOTES"));
        assert!(o.time_zone.is_none());
        assert!(has_variables(&header));
        assert_eq!(
            strip_ddl_session_header(ddl),
            "CREATE PROCEDURE p() BEGIN END"
        );
        let only_db = DdlSession {
            collation_database: Some("utf8mb4_bin".into()),
            ..DdlSession::default()
        };
        assert!(!has_variables(&only_db));
        Ok(())
    }

    #[test]
    fn times_are_iso_utc() {
        assert_eq!(iso_utc(0), None);
        assert_eq!(iso_utc(1_000).as_deref(), Some("1970-01-01T00:00:01Z"));
        assert_eq!(
            iso_utc(1_760_875_200_000).as_deref(),
            Some("2025-10-19T12:00:00Z")
        );
        assert_eq!(
            iso_utc(951_782_400_000).as_deref(),
            Some("2000-02-29T00:00:00Z")
        );
    }

    #[test]
    fn routes_and_lock_timeouts() {
        let identity =
            r#"{"host":"DB","port":3306,"ssh":{"enabled":true,"host":"Bastion","port":22}}"#;
        assert_eq!(route_of(identity), "db:3306 via bastion:22");
        let direct = r#"{"host":"db","port":3306,"ssh":{"enabled":false,"host":"","port":22}}"#;
        assert_eq!(route_of(direct), "db:3306");
        let timeout = varve_driver::DriverError::new(varve_driver::ErrorKind::Query, "Lock wait")
            .with_code("1205 (HY000)");
        assert!(is_lock_wait_timeout(&timeout));
        assert!(stops_retries(&CmdError::Session {
            error: SessionError::Driver { error: timeout }
        }));
        let cancelled =
            varve_driver::DriverError::new(varve_driver::ErrorKind::Cancelled, "interrupted");
        assert!(stops_retries(&CmdError::Session {
            error: SessionError::Driver { error: cancelled }
        }));
    }

    #[test]
    fn collation_changes_are_quoted() -> Result<(), CmdError> {
        assert_eq!(
            alter_collation("sh`op", "utf8mb4_bin")?,
            "ALTER DATABASE `sh``op` COLLATE `utf8mb4_bin`"
        );
        Ok(())
    }
}
