//! Tauri commands for records, keychain secrets and sessions.
//!
//! Every command returns `Result<T, CmdError>`; the UI receives errors as
//! JSON with a `kind` so it can show validation messages next to fields,
//! ask for confirmation, or show driver errors with their codes.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use varve_core::Engine;
use varve_driver::{ErrorKind, ExecOptions, ExecOutput, ServerInfo, tls_key};
use varve_records::{
    ConnectionInput, ConnectionProfile, Folder, FolderContents, FolderDeleteMode,
    PERSONAL_WORKSPACE, RecordError, Records, ReorderKind, SavedQuery, SavedQueryInput,
    SecretKindName, SshAuthMethod, connect_params,
};
use varve_secrets::{Secret, SecretCache, SecretKey, SecretKind, SecretStore};

use varve_activity::{ActivityFilter, Page, Recorder};
use varve_driver_redis::{
    DbKeyspace, KeyInfo, KeyType, RedisSession, ScanPage, ValueChunk, ValuePage, key_from_display,
};
use varve_store::Store;

use crate::keychain_cleanup::{
    BackgroundCleanup, PendingDeletion, lock_guard, tree_lock, try_lock_guard,
};
use crate::m2::metadata::Typed;
use crate::secret_versions::ALL_KINDS;
use crate::sessions::{SessionError, SessionInfo, SessionManager};
use crate::settings::{self, ActivitySettings, SecretSettings};
use crate::tunnels::Tunnels;

/// Services the commands use.
pub struct Services {
    /// `app.db`, for stored settings.
    pub store: Arc<Store>,
    /// Folders, connections, saved queries.
    pub records: Records,
    /// The OS keychain (or the in-memory store in tests) behind the
    /// in-memory cache: every read of a profile's secrets goes through
    /// [`Self::secret_cache`], so a secret is read once per app run.
    pub secrets: Arc<dyn SecretStore>,
    /// The cache [`Self::secrets`] reads through, to forget secrets on
    /// profile changes, on request and on quit.
    pub secret_cache: Arc<SecretCache>,
    /// Open sessions.
    pub sessions: Arc<SessionManager>,
    /// SSH tunnels.
    pub tunnels: Tunnels,
    /// Local activity log.
    pub activity: Arc<Recorder>,
    /// Catalogs of open metadata sessions, opened on first use (M2).
    pub catalogs: Arc<crate::m2::CatalogCache>,
    /// Metadata and admin sessions of the object browser and live panels (M2).
    pub metadata: Arc<crate::m2::metadata::MetadataPool>,
    /// Server-held plans and DDL collation state (M2).
    pub m2: Arc<crate::m2::M2State>,
    /// The background retry of failed keychain removals.
    pub keychain_cleanup: BackgroundCleanup,
}

/// Error returned to the UI.
#[derive(Debug, Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CmdError {
    /// Input failed validation; show `message` next to the form.
    #[error("{message}")]
    Invalid {
        /// User-facing message.
        message: String,
    },
    /// A record does not exist (any more).
    #[error("{message}")]
    NotFound {
        /// User-facing message.
        message: String,
    },
    /// A session operation failed or needs confirmation.
    #[error("{error}")]
    Session {
        /// Details.
        error: SessionError,
    },
    /// The keychain failed.
    #[error("{message}")]
    Keychain {
        /// User-facing message.
        message: String,
        /// When the call stored a profile before the keychain failed
        /// (`update_connection`): the profile as stored, so the UI goes on
        /// with exactly that version. Left out otherwise.
        #[serde(skip_serializing_if = "Option::is_none")]
        profile: Option<Box<ConnectionProfile>>,
    },
    /// The SSH tunnel failed.
    #[error("{message}")]
    #[serde(rename_all = "camelCase")]
    Tunnel {
        /// User-facing message, never containing secrets.
        message: String,
        /// Tunnel error kind, for example `host_key_unknown`.
        code: String,
        /// Fingerprints for host key prompts: the presented key, or the
        /// recorded and the presented key when it changed.
        fingerprints: Vec<String>,
        /// The presented host key in OpenSSH format, to trust after the user confirms.
        presented_key: Option<String>,
        /// SSH host the presented key came from (host key errors only): trust
        /// exactly this host, not whatever the profile says by then.
        host: Option<String>,
        /// SSH port the presented key came from (host key errors only).
        port: Option<u16>,
    },
    /// A password or passphrase is needed and not saved; ask the user.
    #[error("a {secret:?} is needed")]
    SecretRequired {
        /// Which secret.
        secret: SecretKindName,
    },
    /// The tab's connection settings were edited since it connected; the
    /// call needs a session of the new settings. Reconnect the tab.
    #[error("{message}")]
    ReconnectRequired {
        /// User-facing message.
        message: String,
    },
    /// The session is read-only and the call would change something.
    #[error("{message}")]
    ReadOnly {
        /// User-facing message.
        message: String,
    },
    /// The object browser, an editor, `EXPLAIN` or grid editing failed.
    /// A struct variant, because `CatalogError` has its own `kind`.
    #[error("{error}")]
    Catalog {
        /// What the catalog reported.
        error: varve_catalog::CatalogError,
    },
    /// A live server panel failed (a missing privilege is
    /// `permissionDenied`). A struct variant, because `LiveError` has its own
    /// `kind`.
    #[error("{error}")]
    Live {
        /// What the panel reported.
        error: varve_live_stats::sql::LiveError,
    },
    /// Anything else.
    #[error("{message}")]
    Internal {
        /// Message.
        message: String,
    },
}

impl From<varve_catalog::CatalogError> for CmdError {
    fn from(error: varve_catalog::CatalogError) -> Self {
        CmdError::Catalog { error }
    }
}

impl From<varve_live_stats::sql::LiveError> for CmdError {
    fn from(error: varve_live_stats::sql::LiveError) -> Self {
        CmdError::Live { error }
    }
}

impl From<RecordError> for CmdError {
    fn from(error: RecordError) -> Self {
        match error {
            RecordError::Invalid(message) => CmdError::Invalid { message },
            RecordError::Cycle => CmdError::Invalid {
                message: error.to_string(),
            },
            RecordError::NotFound { .. } => CmdError::NotFound {
                message: error.to_string(),
            },
            other => CmdError::Internal {
                message: other.to_string(),
            },
        }
    }
}

impl From<SessionError> for CmdError {
    fn from(error: SessionError) -> Self {
        CmdError::Session { error }
    }
}

impl From<varve_activity::ActivityError> for CmdError {
    fn from(error: varve_activity::ActivityError) -> Self {
        CmdError::Internal {
            message: error.to_string(),
        }
    }
}

impl From<varve_store::StoreError> for CmdError {
    fn from(error: varve_store::StoreError) -> Self {
        CmdError::Internal {
            message: error.to_string(),
        }
    }
}

impl From<varve_secrets::SecretError> for CmdError {
    fn from(error: varve_secrets::SecretError) -> Self {
        CmdError::Keychain {
            message: error.to_string(),
            profile: None,
        }
    }
}

type CmdResult<T> = Result<T, CmdError>;

/// Takes every lock of `locks`, but never holds one while waiting for
/// another: each is tried without waiting, and when one is busy (a secret
/// save or a keychain cleanup of that connection, which may wait for a
/// keychain prompt) all taken so far are released, the busy one is waited
/// for alone, and the round starts again. So connections whose locks are
/// free are never held up by a busy sibling.
fn lock_all_without_holding_while_waiting(
    locks: &[Arc<std::sync::Mutex<()>>],
) -> Vec<std::sync::MutexGuard<'_, ()>> {
    loop {
        let mut guards = Vec::with_capacity(locks.len());
        let mut busy = None;
        for lock in locks {
            match try_lock_guard(lock) {
                Some(guard) => guards.push(guard),
                None => {
                    busy = Some(lock);
                    break;
                }
            }
        }
        let Some(busy) = busy else {
            return guards;
        };
        drop(guards);
        drop(lock_guard(busy));
    }
}

/// Runs blocking work (`app.db`, record and tree locks, keychain calls) on
/// the blocking thread pool, so neither the main thread (the window) nor an
/// async worker waits for a slow keychain.
///
/// # Errors
/// The work's own error, or `internal` when it panicked.
pub(crate) async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, CmdError> + Send + 'static,
) -> Result<T, CmdError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| CmdError::Internal {
            message: format!("A background task failed: {error}"),
        })?
}

/// Everything the sidebar tree needs, in one call.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tree {
    /// Folders.
    pub folders: Vec<Folder>,
    /// Connection profiles.
    pub connections: Vec<ConnectionProfile>,
    /// Saved queries.
    pub queries: Vec<SavedQuery>,
}

/// Result of testing a connection.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    /// Server identity.
    pub server: ServerInfo,
    /// Connect plus ping time in milliseconds.
    pub latency_ms: u64,
}

/// A secret value from a form. Wrapped so it never prints.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct SecretInput(String);

impl std::fmt::Debug for SecretInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretInput([redacted])")
    }
}

impl SecretInput {
    fn into_secret(self) -> Secret {
        Secret::new(self.0)
    }
}

impl Services {
    // Tree and folders

    /// The whole personal workspace tree.
    ///
    /// # Errors
    /// Storage errors.
    pub fn tree(&self) -> CmdResult<Tree> {
        Ok(Tree {
            folders: self.records.folders(PERSONAL_WORKSPACE)?,
            connections: self.records.connections(PERSONAL_WORKSPACE)?,
            queries: self.records.queries(PERSONAL_WORKSPACE)?,
        })
    }

    /// Creates a folder.
    ///
    /// # Errors
    /// Validation or storage errors.
    pub fn create_folder(
        &self,
        parent_id: Option<&str>,
        name: &str,
        color: Option<&str>,
    ) -> CmdResult<Folder> {
        Ok(self
            .records
            .create_folder(PERSONAL_WORKSPACE, parent_id, name, color)?)
    }

    /// Renames a folder.
    ///
    /// # Errors
    /// Validation or storage errors.
    pub fn update_folder(&self, id: &str, name: &str, color: Option<&str>) -> CmdResult<Folder> {
        Ok(self.records.update_folder(id, name, color)?)
    }

    /// Moves a folder.
    ///
    /// # Errors
    /// Cycles or storage errors.
    pub fn move_folder(&self, id: &str, parent_id: Option<&str>) -> CmdResult<Folder> {
        let _tree = tree_lock();
        Ok(self.records.move_folder(id, parent_id)?)
    }

    /// Counts what a folder contains.
    ///
    /// # Errors
    /// Storage errors.
    pub fn folder_contents(&self, id: &str) -> CmdResult<FolderContents> {
        Ok(self.records.folder_contents(id)?)
    }

    /// Deletes a folder; removes keychain entries of deleted connections.
    ///
    /// # Errors
    /// Storage or keychain errors.
    pub async fn delete_folder(
        self: &Arc<Self>,
        id: &str,
        mode: FolderDeleteMode,
    ) -> CmdResult<()> {
        let this = Arc::clone(self);
        let folder = id.to_string();
        let deleted = run_blocking(move || this.delete_folder_records(&folder, mode)).await;
        // Earlier failed removals of other connections, without waiting.
        self.retry_keychain_cleanup_in_background();
        let deleted = deleted?;
        let mut problems = Vec::new();
        for (profile, cleanup) in deleted {
            self.profile_deleted(&profile).await;
            if let Err(error) = cleanup {
                problems.push(format!("{}: {error}", profile.name));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(CmdError::Keychain {
                message: format!(
                    "The folder was deleted, but some keychain entries could not be removed (Varve \
                     retries later): {}",
                    problems.join("; ")
                ),
                profile: None,
            })
        }
    }

    /// The blocking part of [`Self::delete_folder`]. The profiles the
    /// delete removes are deleted under their record locks (taken without
    /// ever holding one while waiting for another), so a secret save in flight on one of them either finishes
    /// first (and its secret is then removed with the others) or finds the
    /// profile gone and writes nothing. The tree lock is taken only after
    /// those locks and only around the `app.db` delete. Connections that
    /// were created in or moved into the folder while the locks were taken
    /// are locked under the tree lock without waiting; only when one of
    /// them is busy are all locks released and taken again for the new
    /// set, so fast creates cannot keep the delete from finishing.
    fn delete_folder_records(
        &self,
        id: &str,
        mode: FolderDeleteMode,
    ) -> CmdResult<Vec<(ConnectionProfile, CmdResult<()>)>> {
        let mut ids = {
            let _tree = tree_lock();
            self.profiles_deleted_with(id, mode)?
        };
        loop {
            let locks: Vec<_> = ids.iter().map(|p| self.profile_record_lock(p)).collect();
            let guards = lock_all_without_holding_while_waiting(&locks);
            let tree = tree_lock();
            let now = self.profiles_deleted_with(id, mode)?;
            let extra_locks: Vec<_> = now
                .iter()
                .filter(|p| !ids.contains(p))
                .map(|p| self.profile_record_lock(p))
                .collect();
            let extra_guards: Vec<_> = extra_locks
                .iter()
                .map_while(|l| try_lock_guard(l))
                .collect();
            if extra_guards.len() < extra_locks.len() {
                // One of them is busy (a secret save, an edit): waiting for
                // it under the tree lock would hold up every other
                // connection, so start over with the new set.
                drop(extra_guards);
                drop(tree);
                drop(guards);
                ids = now;
                continue;
            }
            // Recorded before they are unlisted, so a crash leaves them
            // recorded; taken off again when the delete fails.
            let mut entries = Vec::new();
            for profile_id in &now {
                let profile = self.records.connection(profile_id)?;
                entries.extend(PendingDeletion::of(profile_id, &profile.saved_secrets));
            }
            self.queue_secret_deletions(&entries, None)
                .map_err(|error| CmdError::Internal {
                    message: format!(
                        "Varve cannot record the keychain entries to remove, so the folder was \
                         not deleted: {error}"
                    ),
                })?;
            let removed = self.records.delete_folder(id, mode);
            drop(tree);
            let profiles = match removed {
                Ok(profiles) => profiles,
                Err(error) => {
                    self.unqueue_secret_deletions(&entries);
                    return Err(error.into());
                }
            };
            let mut deleted = Vec::new();
            for profile in profiles {
                self.metadata.profile_edited(&profile.id);
                self.secret_cache.forget_profile(&profile.id);
                self.forget_secret_versions(&profile.id);
                let cleanup = self.forget_all_secrets(&profile);
                deleted.push((profile, cleanup));
            }
            drop(extra_guards);
            drop(guards);
            return Ok(deleted);
        }
    }

    /// The ids of the connections a folder delete removes, sorted.
    fn profiles_deleted_with(&self, id: &str, mode: FolderDeleteMode) -> CmdResult<Vec<String>> {
        if matches!(mode, FolderDeleteMode::MoveContentsToParent) {
            return Ok(Vec::new());
        }
        let folders = self.records.folders(PERSONAL_WORKSPACE)?;
        let mut subtree = vec![id.to_string()];
        let mut index = 0;
        while index < subtree.len() {
            let parent = subtree[index].clone();
            subtree.extend(
                folders
                    .iter()
                    .filter(|f| f.parent_id.as_deref() == Some(parent.as_str()))
                    .map(|f| f.id.clone()),
            );
            index += 1;
        }
        let mut ids: Vec<String> = self
            .records
            .connections(PERSONAL_WORKSPACE)?
            .into_iter()
            .filter(|c| c.folder_id.as_ref().is_some_and(|f| subtree.contains(f)))
            .map(|c| c.id)
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// Reorders siblings.
    ///
    /// # Errors
    /// Ids from different parents, or storage errors.
    pub fn reorder(&self, kind: ReorderKind, ids: &[String]) -> CmdResult<()> {
        Ok(self.records.reorder(kind, ids)?)
    }

    // Connections

    /// Creates a connection profile.
    ///
    /// # Errors
    /// Validation or storage errors.
    pub fn create_connection(&self, input: &ConnectionInput) -> CmdResult<ConnectionProfile> {
        let _tree = tree_lock();
        Ok(self.records.create_connection(PERSONAL_WORKSPACE, input)?)
    }

    /// Updates a connection profile.
    ///
    /// Saved secrets follow the profile only while it points at the same
    /// server and account over an equally trusted route (the rule of
    /// [`reusable_secrets`]). The others are unlisted before the edit is
    /// stored (so at no moment does the stored profile point at the new
    /// server while listing them), their kinds are marked so a secret
    /// typed for the old version cannot be saved for the new one, and they
    /// are removed from the keychain (retried later on failure). All of
    /// it runs under the profile's record lock.
    ///
    /// # Errors
    /// Validation or storage errors; `keychain` when the edit was stored
    /// but some old keychain entries could not be removed yet.
    pub async fn update_connection(
        self: &Arc<Self>,
        id: &str,
        input: &ConnectionInput,
    ) -> CmdResult<ConnectionProfile> {
        let this = Arc::clone(self);
        let (owned_id, owned_input) = (id.to_string(), input.clone());
        let stored =
            run_blocking(move || this.store_connection_edit(&owned_id, &owned_input)).await;
        // Earlier failed removals of other connections, without waiting.
        self.retry_keychain_cleanup_in_background();
        let (old, new, problems, version_problem) = stored?;
        // Typed secrets and the pooled sessions belong to the old server
        // and account.
        self.profile_changed(&old, &new).await;
        self.metadata.profile_edited(id);
        let mut notes = Vec::new();
        if !problems.is_empty() {
            notes.push(format!(
                "some keychain entries of the old server could not be removed (Varve retries \
                 later): {}",
                problems.join("; ")
            ));
        }
        if let Some(problem) = version_problem {
            notes.push(format!(
                "Varve could not record which saved secrets the change affects ({problem}); it \
                 checks secrets against the connection's current settings instead"
            ));
        }
        if notes.is_empty() {
            Ok(new)
        } else {
            Err(CmdError::Keychain {
                message: format!("The connection was saved, but {}", notes.join("; and ")),
                profile: Some(Box::new(new)),
            })
        }
    }

    /// The blocking part of [`Self::update_connection`]: everything runs
    /// under the profile's record lock, the tree lock only around the
    /// `app.db` writes, and the keychain removals after it.
    fn store_connection_edit(
        &self,
        id: &str,
        input: &ConnectionInput,
    ) -> CmdResult<(
        ConnectionProfile,
        ConnectionProfile,
        Vec<String>,
        Option<String>,
    )> {
        self.metadata.profile_edited(id);
        self.secret_cache.forget_profile(id);
        self.with_profile_record(id, || {
            self.retry_own_keychain_cleanup_quietly(id);
            let old = self.records.connection(id)?;
            let mut preview = Records::preview_connection(PERSONAL_WORKSPACE, input)?;
            preview.id = old.id.clone();
            preview.saved_secrets = old.saved_secrets.clone();
            let keep = reusable_secrets(&old, &preview);
            let dropped: Vec<SecretKindName> = old
                .saved_secrets
                .iter()
                .copied()
                .filter(|k| !keep.contains(k))
                .collect();
            // Kinds that were usable before the edit and are not after it
            // (kinds unused before, such as SSH secrets without a tunnel,
            // are left alone).
            let mut every = old.clone();
            every.saved_secrets = ALL_KINDS.to_vec();
            let usable_before = reusable_secrets(&every, &old);
            let usable_after = reusable_secrets(&every, &preview);
            let invalidated: Vec<SecretKindName> = usable_before
                .iter()
                .copied()
                .filter(|k| !usable_after.contains(k))
                .collect();
            // The dropped kinds are recorded for removal in the same write
            // as the marks, before they are unlisted.
            let versions_before = self
                .begin_secret_change(id, &invalidated, old.version, &dropped)
                .map_err(|error| CmdError::Internal {
                    message: format!(
                        "Varve cannot read which saved secrets belong to this connection, so the \
                         change was not saved: {error}"
                    ),
                })?;
            let stored = {
                let _tree = tree_lock();
                (|| -> CmdResult<ConnectionProfile> {
                    if !dropped.is_empty() {
                        self.records.set_saved_secrets(id, &keep)?;
                    }
                    Ok(self.records.update_connection(id, input)?)
                })()
            };
            let new = match stored {
                Ok(new) => new,
                Err(error) => {
                    // Nothing of the edit is kept: the secrets are listed
                    // again (they are still in the keychain) and the
                    // versions are as before. If that fails, the pending
                    // marks resolve to the old values on the next read
                    // (the profile kept its version).
                    if !dropped.is_empty() {
                        match self.records.set_saved_secrets(id, &old.saved_secrets) {
                            Ok(_) => {
                                self.unqueue_secret_deletions(&PendingDeletion::of(id, &dropped));
                            }
                            Err(relist) => {
                                tracing::warn!(%relist, "could not list the secrets again after a failed edit");
                                // The profile may be gone: its unlisted
                                // secrets must not stay in the keychain
                                // (they stay recorded when this fails).
                                for kind in &dropped {
                                    let _ = self.remove_queued_secret(id, *kind);
                                }
                            }
                        }
                    }
                    if let Err(restore) = self.abandon_secret_change(id, &versions_before) {
                        tracing::warn!(%restore, "could not restore the secret versions after a failed edit");
                    }
                    return Err(error);
                }
            };
            let version_problem = self
                .finish_secret_change(id, &invalidated, &versions_before, new.version)
                .err();
            let mut problems = Vec::new();
            for kind in &dropped {
                if let Err(error) = self.remove_queued_secret(id, *kind) {
                    problems.push(format!("{kind:?}: {error}"));
                }
            }
            Ok((old, new, problems, version_problem))
        })
    }

    /// Moves a connection to a folder.
    ///
    /// # Errors
    /// Storage errors.
    pub fn move_connection(
        &self,
        id: &str,
        folder_id: Option<&str>,
    ) -> CmdResult<ConnectionProfile> {
        self.with_profile_record(id, || {
            let _tree = tree_lock();
            Ok(self.records.move_connection(id, folder_id)?)
        })
    }

    /// Duplicates a connection (without secrets).
    ///
    /// # Errors
    /// Storage errors.
    pub fn duplicate_connection(&self, id: &str) -> CmdResult<ConnectionProfile> {
        let _tree = tree_lock();
        Ok(self.records.duplicate_connection(id)?)
    }

    /// Deletes a connection and its keychain entries.
    ///
    /// # Errors
    /// Storage or keychain errors.
    pub async fn delete_connection(self: &Arc<Self>, id: &str) -> CmdResult<()> {
        let this = Arc::clone(self);
        let owned_id = id.to_string();
        let removed = run_blocking(move || this.delete_connection_records(&owned_id)).await;
        // Earlier failed removals of other connections, without waiting.
        self.retry_keychain_cleanup_in_background();
        let (profile, cleanup) = removed?;
        self.profile_deleted(&profile).await;
        cleanup
    }

    /// The blocking part of [`Self::delete_connection`], under the
    /// profile's record lock; the tree lock is held only around the
    /// `app.db` delete, the keychain removals come after it.
    fn delete_connection_records(&self, id: &str) -> CmdResult<(ConnectionProfile, CmdResult<()>)> {
        self.metadata.profile_edited(id);
        self.secret_cache.forget_profile(id);
        self.with_profile_record(id, || {
            self.retry_own_keychain_cleanup_quietly(id);
            let listed = self.records.connection(id)?;
            // Recorded before they are unlisted, so a crash leaves them
            // recorded; taken off again when the delete fails.
            let entries = PendingDeletion::of(id, &listed.saved_secrets);
            self.queue_secret_deletions(&entries, None)
                .map_err(|error| CmdError::Internal {
                    message: format!(
                        "Varve cannot record the keychain entries to remove, so the connection \
                         was not deleted: {error}"
                    ),
                })?;
            let removed = {
                let _tree = tree_lock();
                self.records.delete_connection(id)
            };
            let profile = match removed {
                Ok(profile) => profile,
                Err(error) => {
                    self.unqueue_secret_deletions(&entries);
                    return Err(error.into());
                }
            };
            self.forget_secret_versions(id);
            let cleanup = self.forget_all_secrets(&profile);
            Ok((profile, cleanup))
        })
    }

    /// Retries the recorded keychain removals of one profile, logging what
    /// still fails (the cleanup command reports it to the UI). Call it
    /// under the profile's record lock.
    fn retry_own_keychain_cleanup_quietly(&self, id: &str) {
        for problem in self.retry_own_keychain_cleanup(id) {
            tracing::warn!(%problem, "a keychain entry still could not be removed");
        }
    }

    /// Removes every saved secret of a deleted profile from the keychain,
    /// trying all of them and reporting every failure. The entries must be
    /// recorded as pending removals already; failed ones stay recorded.
    /// Call it under the profile's record lock.
    fn forget_all_secrets(&self, profile: &ConnectionProfile) -> CmdResult<()> {
        let mut problems = Vec::new();
        for kind in &profile.saved_secrets {
            if let Err(error) = self.remove_queued_secret(&profile.id, *kind) {
                problems.push(format!("{kind:?}: {error}"));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(CmdError::Keychain {
                message: format!(
                    "Some keychain entries could not be removed: {}",
                    problems.join("; ")
                ),
                profile: None,
            })
        }
    }

    // Secrets

    /// Saves a secret of a profile in the keychain.
    ///
    /// # Errors
    /// Keychain or storage errors.
    pub fn save_secret(
        &self,
        id: &str,
        kind: SecretKindName,
        value: SecretInput,
    ) -> CmdResult<ConnectionProfile> {
        self.save_secret_for(id, kind, value, None)
    }

    /// Saves a secret entered for version `entered_for` of the profile
    /// (the `version` the UI showed when the user typed it). It is refused
    /// when an edit since then made secrets of that kind unusable (another
    /// server, account or route): the secret belongs to the old settings.
    /// The keychain write and the listing happen under the profile's record
    /// lock, the kind is added to the list as stored at that moment, and
    /// the slot is recorded as a pending removal before the keychain write
    /// and taken off once listed, so a crash in between never leaves an
    /// unlisted, unrecorded keychain item.
    ///
    /// # Errors
    /// `invalid` for a secret entered for older settings; keychain or
    /// storage errors.
    pub fn save_secret_for(
        &self,
        id: &str,
        kind: SecretKindName,
        value: SecretInput,
        entered_for: Option<i64>,
    ) -> CmdResult<ConnectionProfile> {
        self.metadata.profile_edited(id);
        self.with_profile_record(id, || {
            let profile = self.records.connection(id)?;
            // Fail closed: when the versions cannot be read, nothing is saved.
            let since = self
                .secret_versions(id, profile.version)
                .map_err(|error| CmdError::Internal {
                    message: format!(
                        "Varve cannot check whether this secret still belongs to the connection, \
                         so it was not saved: {error}"
                    ),
                })?
                .get(&format!("{kind:?}"))
                .copied();
            let refused =
                matches!((entered_for, since), (Some(entered), Some(since)) if entered < since);
            if refused {
                return Err(CmdError::Invalid {
                    message: "The connection's server, account or route changed after this \
                              secret was entered, so it was not saved. Enter it again for the \
                              new settings."
                        .into(),
                });
            }
            self.with_secret_slot(id, kind, || {
                // Recorded before the keychain write, so a crash between the
                // write and the listing leaves the item recorded: the
                // cleanup removes it unless the profile lists the kind.
                // Recording also replaces an earlier pending removal of
                // this slot, which is resolved below like this one.
                self.queue_secret_deletions(&PendingDeletion::of(id, &[kind]), None)
                    .map_err(|error| CmdError::Internal {
                        message: format!(
                            "Varve cannot record the secret before writing it to the keychain, \
                             so it was not saved: {error}"
                        ),
                    })?;
                let key = SecretKey::new(profile.id.clone(), kind.into())?;
                if let Err(error) = self.secrets.set(&key, &value.into_secret()) {
                    if profile.saved_secrets.contains(&kind) {
                        // Still listed: nothing to remove.
                        self.unqueue_secret_deletion(id, kind);
                    }
                    // Otherwise whatever the failed write left stays
                    // recorded and is removed by the cleanup.
                    return Err(error.into());
                }
                let mut kinds = profile.saved_secrets.clone();
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
                match self.records.set_saved_secrets(id, &kinds) {
                    Ok(saved) => {
                        self.unqueue_secret_deletion(id, kind);
                        Ok(saved)
                    }
                    Err(error) => {
                        if profile.saved_secrets.contains(&kind) {
                            // Saved over a listed secret: the listing is
                            // already right, so the new item stays (deleting
                            // it would leave a listed secret that does not
                            // exist).
                            self.unqueue_secret_deletion(id, kind);
                        } else if self.secrets.delete(&key).is_ok() {
                            // Not listed, so it must not stay in the
                            // keychain; it stays recorded when the delete
                            // fails.
                            self.unqueue_secret_deletion(id, kind);
                        }
                        Err(error.into())
                    }
                }
            })
        })
    }

    /// Removes a secret of a profile from the keychain.
    ///
    /// # Errors
    /// Keychain or storage errors.
    pub fn forget_secret(&self, id: &str, kind: SecretKindName) -> CmdResult<ConnectionProfile> {
        self.metadata.profile_edited(id);
        self.with_profile_record(id, || {
            let profile = self.records.connection(id)?;
            if kind == SecretKindName::TlsKeyPassphrase {
                crate::m2::metadata::forget_tls_key(&profile);
            }
            self.with_secret_slot(id, kind, || {
                self.secrets
                    .delete(&SecretKey::new(profile.id.clone(), kind.into())?)?;
                let kinds: Vec<SecretKindName> = profile
                    .saved_secrets
                    .iter()
                    .copied()
                    .filter(|k| *k != kind)
                    .collect();
                Ok(self.records.set_saved_secrets(id, &kinds)?)
            })
        })
    }

    // Saved queries

    /// Saves a new query.
    ///
    /// # Errors
    /// Validation or storage errors.
    pub fn create_query(&self, input: &SavedQueryInput) -> CmdResult<SavedQuery> {
        Ok(self.records.create_query(PERSONAL_WORKSPACE, input)?)
    }

    /// Updates a saved query.
    ///
    /// # Errors
    /// Validation or storage errors.
    pub fn update_query(&self, id: &str, input: &SavedQueryInput) -> CmdResult<SavedQuery> {
        Ok(self.records.update_query(id, input)?)
    }

    /// Deletes a saved query.
    ///
    /// # Errors
    /// Storage errors.
    pub fn delete_query(&self, id: &str) -> CmdResult<()> {
        Ok(self.records.delete_query(id)?)
    }

    // Sessions

    /// Tests a saved profile, or unsaved editor input, with an optional
    /// typed password, SSH secret and TLS key passphrase.
    ///
    /// # Errors
    /// Validation, tunnel or driver errors; `secretRequired` for the TLS key
    /// passphrase when the client key is encrypted and none is typed or
    /// saved.
    pub async fn test_connection(
        &self,
        target: TestTarget,
        password: Option<SecretInput>,
        ssh_secret: Option<SecretInput>,
        tls_passphrase: Option<SecretInput>,
    ) -> CmdResult<TestResult> {
        let profile = match target {
            TestTarget::Saved { id } => self.records.connection(&id)?,
            TestTarget::Unsaved { input, based_on } => {
                let mut preview = Records::preview_connection(PERSONAL_WORKSPACE, &input)?;
                if let Some(id) = based_on {
                    let saved = self.records.connection(&id)?;
                    // The preview borrows the saved profile's id so the
                    // keychain lookups find its entries, and lists only the
                    // secrets that are safe to reuse for the edited input.
                    preview.saved_secrets = reusable_secrets(&saved, &preview);
                    preview.id = saved.id;
                }
                preview
            }
        };
        let lease = self
            .tunnels
            .lease_for(
                &profile,
                self.secrets.as_ref(),
                ssh_secret.map(SecretInput::into_secret),
            )
            .await?;
        let params = connect_params(
            &profile,
            self.secrets.as_ref(),
            password.map(SecretInput::into_secret),
            tls_passphrase.map(SecretInput::into_secret),
            lease.as_ref().map(|l| l.endpoint()),
        )?;
        let tested = self
            .sessions
            .test(
                &profile.id,
                profile.engine,
                &params,
                crate::sessions::TunnelRoute::of(&profile).as_ref(),
            )
            .await;
        // The test session is closed: a decrypted TLS client key (and the
        // passphrase it is keyed by) stays cached only while a session of
        // the profile uses it.
        if !self.has_open_sessions(&profile.id) {
            crate::m2::metadata::forget_tls_key(&profile);
        }
        let (server, latency_ms) = tested.map_err(|error| secret_required(error.into()))?;
        Ok(TestResult { server, latency_ms })
    }

    /// True when a session (user, metadata or admin) of the profile is open.
    fn has_open_sessions(&self, connection_id: &str) -> bool {
        self.sessions
            .list()
            .iter()
            .any(|s| s.connection_id == connection_id)
    }

    /// Opens a session for a profile.
    ///
    /// # Errors
    /// Tunnel or driver errors; `secretRequired` for the TLS key passphrase
    /// when the client key is encrypted and none is typed or saved.
    pub async fn open_session(
        &self,
        id: &str,
        password: Option<SecretInput>,
        ssh_secret: Option<SecretInput>,
        tls_passphrase: Option<SecretInput>,
    ) -> CmdResult<SessionInfo> {
        let profile = self.records.connection(id)?;
        self.open_profile_session(&profile, password, ssh_secret, tls_passphrase)
            .await
    }

    /// Opens a session for `profile` (a saved profile, possibly with fields
    /// overridden), through its SSH tunnel when it has one.
    pub(crate) async fn open_profile_session(
        &self,
        profile: &ConnectionProfile,
        password: Option<SecretInput>,
        ssh_secret: Option<SecretInput>,
        tls_passphrase: Option<SecretInput>,
    ) -> CmdResult<SessionInfo> {
        let typed = Typed {
            password: password.map(SecretInput::into_secret),
            ssh: ssh_secret.map(SecretInput::into_secret),
            tls: tls_passphrase.map(SecretInput::into_secret),
        };
        // Not interleaved with releasing the profile's pooled sessions and
        // typed secrets when its last user session closes.
        let profile_lock = self.metadata.profile_lock(&profile.id);
        let _profile = profile_lock.lock().await;
        let opened = self.open_with_retry(profile, typed).await;
        // A decrypted TLS client key is kept only while sessions of the
        // profile use it (see `varve_driver::tls_key`).
        if opened.is_err() && !self.has_open_sessions(&profile.id) {
            crate::m2::metadata::forget_tls_key(profile);
        }
        opened
    }

    /// [`Self::open_profile_session`] under the profile lock: one attempt,
    /// and one more with the keychain read again when a cached saved
    /// secret was refused.
    async fn open_with_retry(
        &self,
        profile: &ConnectionProfile,
        typed: Typed,
    ) -> CmdResult<SessionInfo> {
        // Only a cached secret can be stale: a cold one is read from the
        // keychain by the attempt itself, and a refusal then is final.
        // Per kind, so a warm SSH password never excuses a cold, wrong
        // database password (one login per click, whatever is cached).
        let warm_before = |kind: SecretKind| {
            SecretKey::new(profile.id.clone(), kind).is_ok_and(|key| self.secret_cache.has(&key))
        };
        let (warm_db, warm_ssh, warm_tls) = (
            warm_before(SecretKind::DbPassword),
            ssh_secret_kind(profile).is_some_and(warm_before),
            warm_before(SecretKind::TlsKeyPassphrase),
        );
        let info = match self.open_with_secrets(profile, typed.clone()).await {
            Ok(info) => info,
            Err(error) if typed.is_empty() => {
                let Some(refused) = refused_kind(profile, &error) else {
                    return Err(error);
                };
                // Whatever the attempt cached is refused by the server or
                // the bastion: nothing known to be wrong stays cached.
                self.secret_cache.forget_profile(&profile.id);
                let stale = match refused {
                    SecretKind::DbPassword => warm_db,
                    SecretKind::TlsKeyPassphrase => warm_tls,
                    SecretKind::SshPassword | SecretKind::SshKeyPassphrase => warm_ssh,
                };
                if !stale {
                    return Err(error);
                }
                // A cached saved secret the server or bastion refused may
                // have been changed in the keychain since it was cached
                // (another Varve build, Keychain Access): read it again
                // once, then give up.
                tracing::info!(
                    connection_id = %profile.id,
                    "a cached secret was refused; reading the keychain again"
                );
                match self.open_with_secrets(profile, Typed::default()).await {
                    Ok(info) => info,
                    Err(error) => {
                        if refused_kind(profile, &error).is_some() {
                            self.secret_cache.forget_profile(&profile.id);
                        }
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        };
        // The object browser's metadata session connects with the same
        // typed secrets (see `m2::metadata`).
        self.metadata.remember_typed(profile, typed);
        Ok(info)
    }

    /// One attempt of [`Self::open_profile_session`]: the tunnel, then the
    /// session, with the typed secrets or the saved ones.
    async fn open_with_secrets(
        &self,
        profile: &ConnectionProfile,
        typed: Typed,
    ) -> CmdResult<SessionInfo> {
        let lease = self
            .tunnels
            .lease_for(profile, self.secrets.as_ref(), typed.ssh)
            .await?;
        let params = connect_params(
            profile,
            self.secrets.as_ref(),
            typed.password,
            typed.tls,
            lease.as_ref().map(|l| l.endpoint()),
        )?;
        let guard: Option<crate::sessions::SessionGuard> =
            lease.map(|l| Box::new(l) as crate::sessions::SessionGuard);
        self.sessions
            .open(profile, params, guard)
            .await
            .map_err(|error| secret_required(error.into()))
    }

    /// Runs a script.
    ///
    /// # Errors
    /// Confirmation needed or driver errors.
    pub async fn execute(
        &self,
        session_id: &str,
        sql: &str,
        confirmed: bool,
        max_rows: Option<usize>,
    ) -> CmdResult<ExecOutput> {
        let options = ExecOptions {
            max_rows: max_rows
                .unwrap_or(ExecOptions::default().max_rows)
                .clamp(1, 1_000_000),
        };
        Ok(self
            .sessions
            .execute(session_id, sql, confirmed, &options)
            .await?)
    }

    /// Cancels the running statement of a session.
    ///
    /// # Errors
    /// Unknown session or driver errors.
    pub async fn cancel(&self, session_id: &str) -> CmdResult<()> {
        // Long app sequences on the session (a DDL run with its collation
        // restore) stop at their next step, whatever they are doing.
        // Only open sessions are counted, so unknown ids add nothing.
        if self.sessions.info(session_id).is_ok() {
            self.m2.note_cancel(session_id);
        }
        Ok(self.sessions.cancel(session_id).await?)
    }

    /// Closes a session.
    ///
    /// # Errors
    /// Unknown session or driver errors.
    pub async fn close_session(&self, session_id: &str) -> CmdResult<()> {
        // App sequences on the session stop at their next step.
        let info = self.sessions.info(session_id).ok();
        if info.is_some() {
            self.m2.note_cancel(session_id);
        }
        let closed = self.sessions.close(session_id).await;
        // Nothing runs on a closed session any more.
        self.m2.forget_cancels(session_id);
        closed?;
        // The last user session of a profile takes its metadata sessions
        // (object browser) with it.
        if let Some(info) = info
            && info.purpose == crate::sessions::SessionPurpose::User
        {
            self.release_metadata_if_unused(&info.connection_id).await;
        }
        Ok(())
    }

    /// Records an SSH host key for a host and port typed in the editor but
    /// not saved yet.
    ///
    /// # Errors
    /// An invalid key or host, or file errors.
    pub async fn trust_ssh_host_key(
        &self,
        host: &str,
        port: u16,
        presented_key: &str,
    ) -> CmdResult<()> {
        if !varve_records::is_valid_ssh_host(host) || port == 0 {
            return Err(CmdError::Invalid {
                message: "the SSH host must be a host name or IP address and the port 1 to 65535"
                    .into(),
            });
        }
        self.tunnels.trust_host_key(host, port, presented_key).await
    }

    /// Open sessions.
    #[must_use]
    pub fn sessions(&self) -> Vec<SessionInfo> {
        self.sessions.list()
    }

    // Activity

    /// A page of the activity log, newest first.
    ///
    /// # Errors
    /// Storage errors or an unknown cursor.
    pub fn activity(&self, filter: &ActivityFilter) -> CmdResult<Page> {
        Ok(self.activity.list(filter)?)
    }

    /// Query history of a connection, built on the activity log.
    ///
    /// # Errors
    /// Storage errors.
    pub fn query_history(&self, connection_id: &str, limit: u32) -> CmdResult<Page> {
        Ok(Page {
            events: self.activity.query_history(connection_id, limit)?,
            next_cursor: None,
        })
    }

    /// Records `data.exported` for a result export the UI wrote to a file
    /// (design doc 9.2). Only the file's name is kept, never its path or
    /// contents.
    ///
    /// # Errors
    /// `invalid` for an empty file name or an unknown format; storage
    /// errors.
    pub fn record_export(&self, record: &ExportRecord) -> CmdResult<()> {
        let file_name = std::path::Path::new(record.file_name.trim())
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        if file_name.is_empty() {
            return Err(CmdError::Invalid {
                message: "An export needs the name of the file it wrote.".into(),
            });
        }
        let format = record.format.trim().to_ascii_lowercase();
        if !EXPORT_FORMATS.contains(&format.as_str()) {
            return Err(CmdError::Invalid {
                message: format!(
                    "Unknown export format; expected one of {}.",
                    EXPORT_FORMATS.join(", ")
                ),
            });
        }
        let details = varve_activity::ExportDetails {
            objects: record.objects.clone(),
            file_name,
            format,
            size_bytes: record.size_bytes,
            rows: record.rows,
        };
        let mut builder = varve_activity::ActivityEvent::data_exported(&details);
        // Linked only to a connection that exists (a tab may outlive it).
        if let Some(id) = record.connection_id.as_deref()
            && self.records.connection(id).is_ok()
        {
            builder = builder.connection(id);
        }
        let event = builder.build().map_err(|error| CmdError::Internal {
            message: error.to_string(),
        })?;
        self.activity.record(event)?;
        Ok(())
    }

    /// Clears the log (one connection, or all) and returns how many events were removed.
    ///
    /// # Errors
    /// Storage errors.
    pub fn clear_activity(&self, connection_id: Option<&str>) -> CmdResult<u64> {
        Ok(self.activity.clear(connection_id)?)
    }

    /// The stored activity settings.
    ///
    /// # Errors
    /// Storage errors.
    pub fn activity_settings(&self) -> CmdResult<ActivitySettings> {
        Ok(settings::load(&self.store)?)
    }

    /// Stores new activity settings, applies the text level to new events
    /// at once and prunes events older than the new retention period.
    /// Returns the number of events removed.
    ///
    /// # Errors
    /// An invalid retention period, or storage errors.
    pub fn set_activity_settings(&self, new: ActivitySettings) -> CmdResult<u64> {
        settings::validate(&new).map_err(|message| CmdError::Invalid { message })?;
        settings::save(&self.store, &new)?;
        self.activity.set_text_level(new.text_level);
        Ok(self.activity.apply_retention(new.retention())?)
    }

    /// The secret cache settings.
    ///
    /// # Errors
    /// Storage errors.
    pub fn secret_settings(&self) -> CmdResult<SecretSettings> {
        Ok(settings::load_secrets(&self.store)?)
    }

    /// Stores the secret cache settings and applies them at once; switching
    /// remembering off forgets every cached secret.
    ///
    /// # Errors
    /// Storage errors.
    pub fn set_secret_settings(&self, new: SecretSettings) -> CmdResult<()> {
        settings::save_secrets(&self.store, &new)?;
        self.secret_cache.set_remember(new.remember_for_session);
        Ok(())
    }

    /// Drops every cached secret, so the next connection reads the
    /// keychain again. Returns how many were dropped.
    pub fn forget_cached_secrets(&self) -> usize {
        let count = self.secret_cache.len();
        self.secret_cache.forget_all();
        varve_driver::tls_key::forget_cached_keys();
        count
    }

    /// Prunes events older than the stored retention period.
    ///
    /// # Errors
    /// Storage errors.
    pub fn apply_stored_retention(&self) -> CmdResult<u64> {
        let stored = settings::load(&self.store)?;
        Ok(self.activity.apply_retention(stored.retention())?)
    }

    /// Exports the log to a file the user picked, as CSV or JSON Lines.
    ///
    /// # Errors
    /// File or storage errors.
    pub fn export_activity(
        &self,
        filter: &ActivityFilter,
        path: &std::path::Path,
        format: ExportFormat,
    ) -> CmdResult<u64> {
        // Write next to the target and rename at the end, so a failed export
        // never leaves a partial file in place of the user's.
        let io_error = |e: std::io::Error| CmdError::Internal {
            message: format!("{}: {e}", path.display()),
        };
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let name = path
            .file_name()
            .ok_or_else(|| CmdError::Invalid {
                message: "choose a file name for the export".into(),
            })?
            .to_string_lossy()
            .into_owned();
        let temp = dir.join(format!(".{name}.{}.varve-export", uuid::Uuid::now_v7()));
        let result = (|| {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(io_error)?;
            let mut writer = std::io::BufWriter::new(file);
            let count = match format {
                ExportFormat::Csv => self.activity.export_csv(filter, &mut writer)?,
                ExportFormat::Jsonl => self.activity.export_jsonl(filter, &mut writer)?,
            };
            let file = writer.into_inner().map_err(|e| io_error(e.into_error()))?;
            file.sync_all().map_err(io_error)?;
            std::fs::rename(&temp, path).map_err(io_error)?;
            Ok(count)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    // Redis key browser (read only in M1)

    async fn redis_lock(&self, session_id: &str) -> CmdResult<crate::sessions::SessionLock> {
        let lock = self.sessions.lock_session(session_id).await?;
        let is_redis = lock
            .session()
            .is_some_and(|s| s.as_any().downcast_ref::<RedisSession>().is_some());
        if is_redis {
            Ok(lock)
        } else {
            Err(CmdError::Session {
                error: SessionError::Driver {
                    error: varve_driver::DriverError::new(
                        varve_driver::ErrorKind::Unsupported,
                        "this session is not a Redis connection",
                    ),
                },
            })
        }
    }

    fn as_redis<'a>(
        lock: &'a crate::sessions::SessionLock,
        session_id: &str,
    ) -> CmdResult<&'a RedisSession> {
        // The session was closed while the call waited for its lock: report
        // it like any other unknown session so the UI offers to reconnect.
        lock.session()
            .and_then(|s| s.as_any().downcast_ref::<RedisSession>())
            .ok_or_else(|| CmdError::Session {
                error: SessionError::NotFound {
                    session_id: session_id.to_string(),
                },
            })
    }

    /// One SCAN page of keys.
    ///
    /// # Errors
    /// Not a Redis session, or driver errors.
    pub async fn redis_scan(
        &self,
        session_id: &str,
        cursor: u64,
        pattern: &str,
        key_type: Option<KeyType>,
        count: u32,
    ) -> CmdResult<ScanPage> {
        let lock = self.redis_lock(session_id).await?;
        Ok(Self::as_redis(&lock, session_id)?
            .scan_keys(cursor, pattern, key_type, count)
            .await
            .map_err(|error| SessionError::Driver { error })?)
    }

    /// Type, TTL, size and encoding of a key.
    ///
    /// # Errors
    /// Not a Redis session, or driver errors.
    pub async fn redis_key_info(&self, session_id: &str, key: &str) -> CmdResult<Option<KeyInfo>> {
        let key = key_from_display(key).map_err(|e| CmdError::Invalid { message: e.message })?;
        let lock = self.redis_lock(session_id).await?;
        Ok(Self::as_redis(&lock, session_id)?
            .key_info(&key)
            .await
            .map_err(|error| SessionError::Driver { error })?)
    }

    /// A page of a key's value.
    ///
    /// # Errors
    /// Not a Redis session, or driver errors.
    pub async fn redis_read_value(
        &self,
        session_id: &str,
        key: &str,
        page: ValuePage,
    ) -> CmdResult<Option<ValueChunk>> {
        let key = key_from_display(key).map_err(|e| CmdError::Invalid { message: e.message })?;
        let lock = self.redis_lock(session_id).await?;
        Ok(Self::as_redis(&lock, session_id)?
            .read_value(&key, &page)
            .await
            .map_err(|error| SessionError::Driver { error })?)
    }

    /// Keys per logical database.
    ///
    /// # Errors
    /// Not a Redis session, an index the server rejects, or driver errors.
    pub async fn redis_select_db(&self, session_id: &str, db: u16) -> CmdResult<()> {
        let lock = self.redis_lock(session_id).await?;
        Ok(Self::as_redis(&lock, session_id)?
            .select_db(db)
            .await
            .map_err(|error| SessionError::Driver { error })?)
    }

    /// Key counts per database.
    ///
    /// # Errors
    /// Not a Redis session, or driver errors.
    pub async fn redis_keyspace(&self, session_id: &str) -> CmdResult<Vec<DbKeyspace>> {
        let lock = self.redis_lock(session_id).await?;
        Ok(Self::as_redis(&lock, session_id)?
            .keyspace()
            .await
            .map_err(|error| SessionError::Driver { error })?)
    }
}

/// File format of an activity export.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// Comma separated values.
    Csv,
    /// One JSON object per line.
    Jsonl,
}

/// File formats a result export is recorded with.
const EXPORT_FORMATS: [&str; 3] = ["csv", "json", "jsonl"];

/// A result export the UI wrote to a file, recorded as `data.exported`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRecord {
    /// The connection the exported result came from.
    pub connection_id: Option<String>,
    /// What was exported: the query tab or panel name.
    pub objects: Vec<String>,
    /// The file name; a path is cut to its last part.
    pub file_name: String,
    /// `csv`, `json` or `jsonl`.
    pub format: String,
    /// Bytes written.
    pub size_bytes: Option<u64>,
    /// Rows written.
    pub rows: Option<u64>,
}

/// What to test.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TestTarget {
    /// A saved profile.
    Saved {
        /// Profile id.
        id: String,
    },
    /// Editor input that is not saved yet.
    #[serde(rename_all = "camelCase")]
    Unsaved {
        /// Input.
        input: Box<ConnectionInput>,
        /// The saved profile being edited, if any. Its keychain secrets are
        /// used only while the input still points at the same server and
        /// account (see [`reusable_secrets`]).
        #[serde(default)]
        based_on: Option<String>,
    },
}

/// The saved secret kind the SSH tunnel of a profile authenticates with,
/// if any.
fn ssh_secret_kind(profile: &ConnectionProfile) -> Option<SecretKind> {
    if !profile.ssh.enabled {
        return None;
    }
    match profile.ssh.auth {
        SshAuthMethod::Password => Some(SecretKind::SshPassword),
        SshAuthMethod::KeyFile => Some(SecretKind::SshKeyPassphrase),
        SshAuthMethod::Agent => None,
    }
}

/// The secret kind a failed connection attempt was refused for: the
/// database password when the server refused the credentials, the SSH
/// secret when the bastion did, the TLS key passphrase when it did not
/// decrypt the client key. `None` for every other failure.
fn refused_kind(profile: &ConnectionProfile, error: &CmdError) -> Option<SecretKind> {
    match error {
        CmdError::Session {
            error: SessionError::Driver { error },
        } if error.kind == ErrorKind::Auth => Some(SecretKind::DbPassword),
        CmdError::Session {
            error: SessionError::Driver { error },
        } if error.code.as_deref() == Some(tls_key::PASSPHRASE_WRONG) => {
            Some(SecretKind::TlsKeyPassphrase)
        }
        CmdError::Tunnel { code, .. } if code == "auth" => ssh_secret_kind(profile),
        _ => None,
    }
}

/// A driver error saying the client key is encrypted and no passphrase was
/// given becomes `secretRequired`, so the UI asks for it like for an SSH
/// secret. Everything else is returned as is.
fn secret_required(error: CmdError) -> CmdError {
    match &error {
        CmdError::Session {
            error: SessionError::Driver { error: driver },
        } if driver.code.as_deref() == Some(tls_key::PASSPHRASE_REQUIRED) => {
            CmdError::SecretRequired {
                secret: SecretKindName::TlsKeyPassphrase,
            }
        }
        _ => error,
    }
}

/// The secrets of `saved` that may be used for `edited`: the database
/// password only while engine, host, port and user are unchanged, the SSH
/// route is the same and TLS is not weaker, SSH
/// secrets only while the SSH host, port, user, auth method and key file are
/// unchanged, and the TLS key passphrase only while the key file is
/// unchanged. Sending a saved password to a server the user just typed in
/// would hand it to whoever runs that server.
#[must_use]
pub fn reusable_secrets(
    saved: &ConnectionProfile,
    edited: &ConnectionProfile,
) -> Vec<SecretKindName> {
    // The same database is only reached the same way: through the same SSH
    // endpoint (or none), and with TLS at least as strict, verified against
    // the same CA. A changed bastion can put a different server behind the
    // same host name; a weaker TLS mode lets someone in between ask for the
    // password in clear text.
    let same_route = saved.ssh.enabled == edited.ssh.enabled
        && (!saved.ssh.enabled
            || (saved.ssh.host.eq_ignore_ascii_case(&edited.ssh.host)
                && saved.ssh.port == edited.ssh.port
                && saved.ssh.username == edited.ssh.username));
    let tls_rank = |mode: varve_driver::TlsMode| match mode {
        varve_driver::TlsMode::Disable => 0,
        varve_driver::TlsMode::Prefer => 1,
        varve_driver::TlsMode::Require => 2,
        varve_driver::TlsMode::VerifyCa => 3,
        varve_driver::TlsMode::VerifyFull => 4,
    };
    let same_trust = tls_rank(edited.tls.mode) >= tls_rank(saved.tls.mode)
        && (tls_rank(edited.tls.mode) < 3 || saved.tls.ca_path == edited.tls.ca_path);
    let same_server = saved.engine == edited.engine
        && saved.host.eq_ignore_ascii_case(&edited.host)
        && saved.port == edited.port
        && saved.username == edited.username
        && same_route
        && same_trust;
    let same_ssh = saved.ssh.enabled
        && edited.ssh.enabled
        && saved.ssh.host.eq_ignore_ascii_case(&edited.ssh.host)
        && saved.ssh.port == edited.ssh.port
        && saved.ssh.username == edited.ssh.username
        && saved.ssh.auth == edited.ssh.auth
        && saved.ssh.key_path == edited.ssh.key_path;
    let same_tls_key = saved.tls.client_key_path.is_some()
        && saved.tls.client_key_path == edited.tls.client_key_path;
    saved
        .saved_secrets
        .iter()
        .copied()
        .filter(|kind| match kind {
            SecretKindName::DbPassword => same_server,
            SecretKindName::SshPassword | SecretKindName::SshKeyPassphrase => same_ssh,
            SecretKindName::TlsKeyPassphrase => same_tls_key,
        })
        .collect()
}

/// Engines the app can open, for the connection editor.
#[must_use]
pub fn engines() -> Vec<Engine> {
    Engine::ALL.to_vec()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn profile(host: &str) -> Result<ConnectionProfile, Box<dyn std::error::Error>> {
        let input: ConnectionInput = serde_json::from_value(json!({
            "name": "db",
            "engine": "postgres",
            "host": host,
            "port": 5432,
            "username": "app",
            "environment": "development",
            "tls": { "mode": "prefer", "clientCertPath": "/c.pem", "clientKeyPath": "/k.pem" },
            "ssh": { "enabled": true, "host": "bastion", "port": 22, "username": "ops", "auth": "password" },
            "options": { "readOnly": false, "confirmDestructive": true, "showSystemObjects": false, "redisDb": 0 }
        }))?;
        let mut profile = Records::preview_connection(PERSONAL_WORKSPACE, &input)?;
        profile.saved_secrets = vec![
            SecretKindName::DbPassword,
            SecretKindName::SshPassword,
            SecretKindName::TlsKeyPassphrase,
        ];
        Ok(profile)
    }

    #[test]
    fn saved_secrets_follow_only_the_same_server() -> Result<(), Box<dyn std::error::Error>> {
        let saved = profile("db.internal")?;
        let mut same = profile("DB.internal")?;
        same.name = "renamed".into();
        assert_eq!(reusable_secrets(&saved, &same), saved.saved_secrets);

        let moved = profile("evil.example")?;
        assert_eq!(
            reusable_secrets(&saved, &moved),
            [
                SecretKindName::SshPassword,
                SecretKindName::TlsKeyPassphrase
            ]
        );

        let mut other_user = profile("db.internal")?;
        other_user.username = Some("root".into());
        assert!(!reusable_secrets(&saved, &other_user).contains(&SecretKindName::DbPassword));

        // Another bastion can put another server behind the same host name.
        let mut other_bastion = profile("db.internal")?;
        other_bastion.ssh.host = "elsewhere".into();
        assert_eq!(
            reusable_secrets(&saved, &other_bastion),
            [SecretKindName::TlsKeyPassphrase]
        );

        let mut no_tunnel = profile("db.internal")?;
        no_tunnel.ssh.enabled = false;
        assert!(!reusable_secrets(&saved, &no_tunnel).contains(&SecretKindName::DbPassword));

        // A weaker TLS mode is refused, a stricter one is fine.
        let mut weaker = profile("db.internal")?;
        weaker.tls.mode = varve_driver::TlsMode::Disable;
        assert!(!reusable_secrets(&saved, &weaker).contains(&SecretKindName::DbPassword));
        let mut stricter = profile("db.internal")?;
        stricter.tls.mode = varve_driver::TlsMode::Require;
        assert!(reusable_secrets(&saved, &stricter).contains(&SecretKindName::DbPassword));
        // Verification needs the same CA.
        let mut verify = profile("db.internal")?;
        verify.tls.mode = varve_driver::TlsMode::VerifyFull;
        verify.tls.ca_path = Some("/other-ca.pem".into());
        assert!(!reusable_secrets(&saved, &verify).contains(&SecretKindName::DbPassword));

        let mut other_key = profile("db.internal")?;
        other_key.tls.client_key_path = Some("/other.pem".into());
        assert!(!reusable_secrets(&saved, &other_key).contains(&SecretKindName::TlsKeyPassphrase));
        Ok(())
    }

    #[test]
    fn unsaved_target_takes_the_profile_it_edits() -> Result<(), serde_json::Error> {
        let target: TestTarget = serde_json::from_value(json!({
            "kind": "unsaved",
            "basedOn": "abc",
            "input": {
                "name": "db", "engine": "redis", "host": "h", "environment": "development",
                "tls": { "mode": "disable" },
                "ssh": { "enabled": false, "host": "", "port": 22, "username": "", "auth": "password" },
                "options": { "readOnly": false, "confirmDestructive": true, "showSystemObjects": false, "redisDb": 0 }
            }
        }))?;
        assert!(matches!(target, TestTarget::Unsaved { based_on: Some(id), .. } if id == "abc"));
        Ok(())
    }

    #[test]
    fn errors_have_the_shapes_the_ui_expects() -> Result<(), serde_json::Error> {
        assert_eq!(
            serde_json::to_value(CmdError::SecretRequired {
                secret: SecretKindName::SshPassword
            })?,
            json!({ "kind": "secretRequired", "secret": "ssh_password" })
        );
        assert_eq!(
            serde_json::to_value(CmdError::Tunnel {
                message: "unknown host key".into(),
                code: "host_key_unknown".into(),
                fingerprints: vec!["SHA256:abc".into()],
                presented_key: Some("ssh-ed25519 AAAA".into()),
                host: Some("bastion".into()),
                port: Some(22),
            })?,
            json!({
                "kind": "tunnel",
                "message": "unknown host key",
                "code": "host_key_unknown",
                "fingerprints": ["SHA256:abc"],
                "presentedKey": "ssh-ed25519 AAAA",
                "host": "bastion",
                "port": 22
            })
        );
        assert_eq!(
            serde_json::to_value(CmdError::Invalid {
                message: "Enter the host name or IP address.".into()
            })?,
            json!({ "kind": "invalid", "message": "Enter the host name or IP address." })
        );
        let session = serde_json::to_value(CmdError::Session {
            error: SessionError::NotFound {
                session_id: "s1".into(),
            },
        })?;
        assert_eq!(
            session,
            json!({ "kind": "session", "error": { "kind": "notFound", "sessionId": "s1" } })
        );
        Ok(())
    }

    #[test]
    fn secret_input_never_prints() -> Result<(), serde_json::Error> {
        let secret: SecretInput = serde_json::from_value(json!("hunter2"))?;
        assert_eq!(format!("{secret:?}"), "SecretInput([redacted])");
        Ok(())
    }
}
