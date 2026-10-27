//! Metadata and admin sessions.
//!
//! Browsing and the live server panels never run on the user's session:
//! they must not wait behind or interleave with the user's queries (a
//! running query must be killable from the same tab), and the PostgreSQL
//! catalog refuses to run inside a transaction the user left open. The app
//! opens its own sessions per connection profile, lazily, with the
//! profile's secrets and tunnel, and shares them across tabs:
//!
//! - catalog sessions ([`PoolRole::Catalog`], purpose `metadata`):
//!   read-only, one per profile and PostgreSQL database (MySQL and MariaDB
//!   share one per profile);
//! - admin sessions ([`PoolRole::Admin`], purpose `admin`): one per profile,
//!   read-only only when the profile is, for the process list, kill,
//!   server variables and Redis panels.
//!
//! Pooled sessions are keyed by the connection identity the tab was opened
//! with, so a tab's panels and browser always reach the tab's server. When
//! the profile was edited to another identity since the tab connected,
//! these calls refuse with `reconnectRequired` (the pooled session would
//! connect with the edited profile and its secrets). Pooled sessions close
//! when the last user session of their profile closes, after
//! [`METADATA_IDLE`] without use, and when the profile's identity changes;
//! they are reopened when their connection is lost. Their `connection.opened`
//! and `connection.closed` events carry `purpose: metadata` or `admin`.
//!
//! Passwords the user typed (not saved in the keychain) are kept in memory
//! with the profile as it was when they were typed, while the profile has
//! open user sessions, so these sessions can connect without asking again.
//! They are used only while [`commands::reusable_secrets`] allows it for the
//! current profile (same server and user, same SSH endpoint), dropped when
//! an edit makes them unusable, and dropped with the last user session.
//!
//! [`commands::reusable_secrets`]: crate::commands::reusable_secrets

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use varve_catalog::CatalogError;
use varve_core::Engine;
use varve_records::{ConnectionProfile, SecretKindName, SshAuthMethod, connect_params};
use varve_secrets::Secret;

use crate::commands::{CmdError, Services, reusable_secrets};
use crate::sessions::{SessionGuard, SessionInfo, SessionPurpose, connection_identity};

type CmdResult<T> = Result<T, CmdError>;

/// How long a metadata or admin session may stay unused before it closes.
pub const METADATA_IDLE: Duration = Duration::from_secs(5 * 60);

/// What a pooled session is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PoolRole {
    /// The object browser (read-only).
    Catalog,
    /// The live server panels.
    Admin,
}

/// Which pooled session serves a user session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MetaKey {
    /// Connection profile.
    pub connection_id: String,
    /// The connection identity of the tab ([`connection_identity`]).
    pub identity: String,
    /// PostgreSQL database of a catalog session; `None` otherwise.
    pub database: Option<String>,
    /// What it is for.
    pub role: PoolRole,
}

impl MetaKey {
    /// The key for a user session and a role.
    #[must_use]
    pub fn of(info: &SessionInfo, role: PoolRole) -> Self {
        Self {
            connection_id: info.connection_id.clone(),
            identity: info.identity.clone(),
            database: if role == PoolRole::Catalog && info.engine == Engine::Postgres {
                info.database.clone()
            } else {
                None
            },
            role,
        }
    }
}

struct MetaEntry {
    session_id: String,
    last_used: Instant,
}

/// Secrets the user typed (not saved) for a profile, by slot.
#[derive(Clone, Default, Debug)]
pub struct Typed {
    /// Database password.
    pub password: Option<Secret>,
    /// SSH password or key passphrase.
    pub ssh: Option<Secret>,
    /// TLS client key passphrase.
    pub tls: Option<Secret>,
}

impl Typed {
    /// True when no secret is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.password.is_none() && self.ssh.is_none() && self.tls.is_none()
    }
}

#[derive(Clone)]
struct TypedSecrets {
    /// The profile as it was when the secrets were typed.
    captured: ConnectionProfile,
    secrets: Typed,
}

impl TypedSecrets {
    /// The secrets that may be used for `current` (the rule of
    /// [`reusable_secrets`]).
    fn usable(&self, current: &ConnectionProfile) -> Typed {
        let mut captured = self.captured.clone();
        captured.saved_secrets = vec![SecretKindName::DbPassword, SecretKindName::TlsKeyPassphrase];
        if self.secrets.ssh.is_some() {
            captured.saved_secrets.push(match captured.ssh.auth {
                SshAuthMethod::KeyFile => SecretKindName::SshKeyPassphrase,
                SshAuthMethod::Password | SshAuthMethod::Agent => SecretKindName::SshPassword,
            });
        }
        let allowed = reusable_secrets(&captured, current);
        let db = allowed.contains(&SecretKindName::DbPassword);
        let tls = allowed.contains(&SecretKindName::TlsKeyPassphrase);
        let ssh = allowed.iter().any(|k| {
            matches!(
                k,
                SecretKindName::SshPassword | SecretKindName::SshKeyPassphrase
            )
        });
        Typed {
            password: self.secrets.password.clone().filter(|_| db),
            ssh: self.secrets.ssh.clone().filter(|_| ssh),
            tls: self.secrets.tls.clone().filter(|_| tls),
        }
    }
}

type AsyncLock = Arc<tokio::sync::Mutex<()>>;

/// Drops the decrypted TLS client key of `profile` that the drivers keep
/// for the sessions of one connect (`varve_driver::tls_key`).
pub(crate) fn forget_tls_key(profile: &ConnectionProfile) {
    if let Some(path) = profile.tls.client_key_path.as_deref() {
        varve_driver::tls_key::forget_cached_key(path);
    }
}

/// The future of one call on an admin session.
pub(crate) type AdminFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = CmdResult<T>> + Send + 'a>>;

/// Pooled sessions, the typed secrets they connect with, and their locks.
#[derive(Default)]
pub struct MetadataPool {
    entries: Mutex<HashMap<MetaKey, MetaEntry>>,
    typed: Mutex<HashMap<String, TypedSecrets>>,
    /// One per key, so two calls do not open two sessions for it.
    opening: Mutex<HashMap<MetaKey, AsyncLock>>,
    /// One per profile: opening a user session (and remembering its typed
    /// secrets) and releasing the pooled sessions do not interleave.
    profiles: Mutex<HashMap<String, AsyncLock>>,
    /// Edit generation per profile, bumped by every change of the profile
    /// or its keychain secrets. A pooled session is only opened when no
    /// edit happened between reading the profile and reading its secrets,
    /// and only kept when none happened while it connected.
    edits: Mutex<HashMap<String, u64>>,
}

impl std::fmt::Debug for MetadataPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<MetaKey> = self
            .entries
            .lock()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        f.debug_struct("MetadataPool")
            .field("sessions", &keys)
            .finish_non_exhaustive()
    }
}

fn lock_of<K: Clone + Eq + std::hash::Hash>(
    map: &Mutex<HashMap<K, AsyncLock>>,
    key: &K,
) -> AsyncLock {
    match map.lock() {
        Ok(mut map) => Arc::clone(map.entry(key.clone()).or_default()),
        Err(_) => AsyncLock::default(),
    }
}

impl MetadataPool {
    /// Notes an edit of a profile or of its keychain secrets.
    pub fn profile_edited(&self, connection_id: &str) {
        if let Ok(mut map) = self.edits.lock() {
            *map.entry(connection_id.to_string()).or_default() += 1;
        }
    }

    fn edit_generation(&self, connection_id: &str) -> u64 {
        self.edits
            .lock()
            .ok()
            .and_then(|map| map.get(connection_id).copied())
            .unwrap_or_default()
    }

    /// The lock that serializes opening user sessions of a profile with
    /// releasing its pooled sessions.
    pub(crate) fn profile_lock(&self, connection_id: &str) -> AsyncLock {
        lock_of(&self.profiles, &connection_id.to_string())
    }

    /// Keeps secrets the user typed for a profile, with the profile they
    /// were typed for. Secrets not given keep their earlier value only
    /// while still usable for `profile`.
    pub fn remember_typed(&self, profile: &ConnectionProfile, typed: Typed) {
        if typed.is_empty() {
            return;
        }
        if let Ok(mut map) = self.typed.lock() {
            let old = map
                .get(&profile.id)
                .map(|t| t.usable(profile))
                .unwrap_or_default();
            map.insert(
                profile.id.clone(),
                TypedSecrets {
                    captured: profile.clone(),
                    secrets: Typed {
                        password: typed.password.or(old.password),
                        ssh: typed.ssh.or(old.ssh),
                        tls: typed.tls.or(old.tls),
                    },
                },
            );
        }
    }

    /// The typed secrets usable for `current`.
    pub(crate) fn typed_for(&self, current: &ConnectionProfile) -> Typed {
        self.typed
            .lock()
            .ok()
            .and_then(|map| map.get(&current.id).map(|t| t.usable(current)))
            .unwrap_or_default()
    }

    /// Drops typed secrets of a profile that `current` may no longer use.
    fn prune_typed(&self, current: &ConnectionProfile) {
        if let Ok(mut map) = self.typed.lock()
            && let Some(typed) = map.get(&current.id)
        {
            let usable = typed.usable(current);
            if usable.is_empty() {
                map.remove(&current.id);
            } else {
                map.insert(
                    current.id.clone(),
                    TypedSecrets {
                        captured: current.clone(),
                        secrets: usable,
                    },
                );
            }
        }
    }

    /// True when typed secrets are kept for a profile.
    #[must_use]
    pub fn has_typed(&self, connection_id: &str) -> bool {
        self.typed
            .lock()
            .is_ok_and(|map| map.contains_key(connection_id))
    }

    fn get(&self, key: &MetaKey) -> Option<String> {
        let mut map = self.entries.lock().ok()?;
        let entry = map.get_mut(key)?;
        entry.last_used = Instant::now();
        Some(entry.session_id.clone())
    }

    fn insert(&self, key: MetaKey, session_id: String) {
        if let Ok(mut map) = self.entries.lock() {
            map.insert(
                key,
                MetaEntry {
                    session_id,
                    last_used: Instant::now(),
                },
            );
        }
    }

    fn remove_session(&self, session_id: &str) {
        if let Ok(mut map) = self.entries.lock() {
            map.retain(|_, e| e.session_id != session_id);
        }
    }

    /// Removes the pooled sessions of a profile; returns their ids.
    fn take_profile_sessions(&self, connection_id: &str) -> Vec<String> {
        let Ok(mut map) = self.entries.lock() else {
            return Vec::new();
        };
        let keys: Vec<MetaKey> = map
            .keys()
            .filter(|k| k.connection_id == connection_id)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|k| map.remove(&k).map(|e| e.session_id))
            .collect()
    }

    /// Removes the pooled sessions of a profile that `matches` selects;
    /// returns their ids.
    fn take_where(&self, matches: impl Fn(&MetaKey, &str) -> bool) -> Vec<String> {
        let Ok(mut map) = self.entries.lock() else {
            return Vec::new();
        };
        let keys: Vec<MetaKey> = map
            .iter()
            .filter(|(k, e)| matches(k, &e.session_id))
            .map(|(k, _)| k.clone())
            .collect();
        keys.into_iter()
            .filter_map(|k| map.remove(&k).map(|e| e.session_id))
            .collect()
    }

    fn forget_typed(&self, connection_id: &str) {
        if let Ok(mut typed) = self.typed.lock() {
            typed.remove(connection_id);
        }
    }

    /// Removes the sessions unused for at least `idle`; returns their ids.
    fn take_idle(&self, idle: Duration) -> Vec<String> {
        let Ok(mut map) = self.entries.lock() else {
            return Vec::new();
        };
        let keys: Vec<MetaKey> = map
            .iter()
            .filter(|(_, e)| e.last_used.elapsed() >= idle)
            .map(|(k, _)| k.clone())
            .collect();
        keys.into_iter()
            .filter_map(|k| map.remove(&k).map(|e| e.session_id))
            .collect()
    }
}

impl Services {
    /// The catalog session for a user session's profile and database.
    ///
    /// # Errors
    /// Unknown session, Redis (`catalog` / `unsupported`), and everything
    /// opening a session returns (`secretRequired` when the password is
    /// neither saved nor typed for this profile in this app run, tunnel and
    /// driver errors).
    pub(crate) async fn metadata_session(&self, user_session_id: &str) -> CmdResult<String> {
        let info = self.session_info(user_session_id)?;
        if info.engine == Engine::Redis {
            return Err(CatalogError::Unsupported {
                message: "Redis keys are browsed with the key browser".into(),
            }
            .into());
        }
        self.pooled_session(&info, PoolRole::Catalog).await
    }

    /// The admin session for a user session's profile.
    ///
    /// # Errors
    /// As [`Self::metadata_session`], for every engine.
    pub(crate) async fn admin_session(&self, user_session_id: &str) -> CmdResult<String> {
        let info = self.session_info(user_session_id)?;
        self.pooled_session(&info, PoolRole::Admin).await
    }

    async fn pooled_session(&self, info: &SessionInfo, role: PoolRole) -> CmdResult<String> {
        let key = MetaKey::of(info, role);
        // An edit while connecting restarts the attempt (once more), so the
        // identity check, the secrets and the connection always belong to
        // one version of the profile.
        for _ in 0..3 {
            let generation = self.metadata.edit_generation(&info.connection_id);
            let profile = self.current_profile_for(info)?;
            if let Some(id) = self.live_pooled(&key).await {
                return Ok(id);
            }
            let opening = lock_of(&self.metadata.opening, &key);
            let _opening = opening.lock().await;
            if let Some(id) = self.live_pooled(&key).await {
                return Ok(id);
            }
            let mut connect = profile.clone();
            if role == PoolRole::Catalog && info.engine == Engine::Postgres {
                // The tab's own database, never the profile's current one.
                connect.database.clone_from(&key.database);
            }
            let purpose = match role {
                PoolRole::Catalog => {
                    connect.options.read_only = true;
                    SessionPurpose::Metadata
                }
                PoolRole::Admin => SessionPurpose::Admin,
            };
            let typed = self.metadata.typed_for(&profile);
            let lease = self
                .tunnels
                .lease_for(&connect, self.secrets.as_ref(), typed.ssh)
                .await?;
            let params = connect_params(
                &connect,
                self.secrets.as_ref(),
                typed.password,
                typed.tls,
                lease.as_ref().map(|l| l.endpoint()),
            )?;
            // The secrets were read after the tunnel came up: they must
            // still be those of the checked profile.
            if self.metadata.edit_generation(&info.connection_id) != generation {
                continue;
            }
            let guard: Option<SessionGuard> = lease.map(|l| Box::new(l) as SessionGuard);
            let opened = self
                .sessions
                .open_for(&connect, params, guard, purpose)
                .await?;
            if self.metadata.edit_generation(&info.connection_id) != generation {
                if let Err(error) = self.sessions.close(&opened.id).await {
                    tracing::debug!(%error, "closing a pooled session of an edited profile");
                }
                continue;
            }
            self.metadata.insert(key, opened.id.clone());
            return Ok(opened.id);
        }
        Err(CmdError::ReconnectRequired {
            message: "The connection settings kept changing while connecting; reconnect this \
                      tab."
                .into(),
        })
    }

    /// The tab's profile as stored now, refused with `reconnectRequired`
    /// when it no longer has the identity the tab was opened with: the
    /// pooled sessions connect with the current profile and its secrets,
    /// so they would reach another server or account.
    fn current_profile_for(&self, info: &SessionInfo) -> CmdResult<ConnectionProfile> {
        let profile = self.records.connection(&info.connection_id)?;
        if connection_identity(&profile) != info.identity {
            return Err(CmdError::ReconnectRequired {
                message: "The connection settings changed since this tab connected; reconnect \
                          this tab to use them."
                    .into(),
            });
        }
        Ok(profile)
    }

    /// The pooled session of a key when it is still open; a closed one is
    /// forgotten.
    async fn live_pooled(&self, key: &MetaKey) -> Option<String> {
        let id = self.metadata.get(key)?;
        if self.sessions.info(&id).is_ok() {
            return Some(id);
        }
        self.discard_metadata(&id).await;
        None
    }

    /// Runs a call on the admin session of a user session's profile, under
    /// the admin session's lock (never the user's). When the call fails and
    /// the admin connection no longer answers, the session is closed so the
    /// next call reopens it; with `retry` (calls that only read) the call is
    /// repeated once on a new session.
    pub(crate) async fn with_admin<T, F>(
        &self,
        user_session_id: &str,
        retry: bool,
        call: F,
    ) -> CmdResult<T>
    where
        F: for<'a> Fn(&'a dyn varve_driver::Session) -> AdminFuture<'a, T>,
    {
        let mut retried = false;
        loop {
            let admin = self.admin_session(user_session_id).await?;
            // Closed by the idle reaper or a profile edit after it was
            // handed out: open a fresh one (once).
            let lock = match self.lock(&admin).await {
                Ok(lock) => lock,
                Err(CmdError::Session {
                    error: crate::sessions::SessionError::NotFound { .. },
                }) if !retried => {
                    retried = true;
                    self.metadata.remove_session(&admin);
                    continue;
                }
                Err(other) => return Err(other),
            };
            let session = Self::locked(&lock, &admin)?;
            let result = call(session).await;
            if result.is_err() && session.ping().await.is_err() {
                drop(lock);
                self.discard_metadata(&admin).await;
                if retry && !retried {
                    retried = true;
                    continue;
                }
            }
            return result;
        }
    }

    /// Closes a pooled session that broke or went stale, so the next call
    /// reopens it.
    pub(crate) async fn discard_metadata(&self, session_id: &str) {
        self.metadata.remove_session(session_id);
        if let Err(error) = self.sessions.close(session_id).await {
            tracing::debug!(%error, "closing a pooled session");
        }
    }

    async fn close_pooled(&self, ids: Vec<String>) {
        for id in ids {
            if let Err(error) = self.sessions.close(&id).await {
                tracing::debug!(%error, "closing a pooled session");
            }
        }
    }

    /// Closes the pooled sessions of a profile when it has no user session
    /// left, and forgets its typed secrets.
    pub(crate) async fn release_metadata_if_unused(&self, connection_id: &str) {
        let lock = self.metadata.profile_lock(connection_id);
        let _profile = lock.lock().await;
        if self
            .sessions
            .list()
            .iter()
            .any(|s| s.connection_id == connection_id)
        {
            return;
        }
        self.metadata.forget_typed(connection_id);
        // The decrypted TLS client key is kept only while sessions use it.
        if let Ok(profile) = self.records.connection(connection_id) {
            forget_tls_key(&profile);
        }
        let ids = self.metadata.take_profile_sessions(connection_id);
        self.close_pooled(ids).await;
    }

    /// After a profile edit: drops typed secrets the new profile may not
    /// use, and closes its pooled sessions when its identity changed.
    pub(crate) async fn profile_changed(&self, old: &ConnectionProfile, new: &ConnectionProfile) {
        self.metadata.prune_typed(new);
        if old.tls.client_key_path != new.tls.client_key_path {
            forget_tls_key(old);
        }
        if connection_identity(old) != connection_identity(new) {
            let ids = self.metadata.take_profile_sessions(&new.id);
            self.close_pooled(ids).await;
        }
    }

    /// Closes the profile's pooled sessions (catalog and admin) connected
    /// to `database`, before a change PostgreSQL refuses while any session
    /// uses that database (drop, rename, moving its tablespace). They
    /// reopen lazily on their next use.
    pub(crate) async fn release_database_sessions(&self, connection_id: &str, database: &str) {
        let ids = self.metadata.take_where(|key, id| {
            key.connection_id == connection_id
                && (key.database.as_deref() == Some(database)
                    || self
                        .sessions
                        .info(id)
                        .is_ok_and(|info| info.database.as_deref() == Some(database)))
        });
        self.close_pooled(ids).await;
    }

    /// After a profile was deleted: forgets its typed secrets and its
    /// decrypted TLS client key, and closes its pooled sessions.
    pub(crate) async fn profile_deleted(&self, profile: &ConnectionProfile) {
        self.metadata.forget_typed(&profile.id);
        forget_tls_key(profile);
        let ids = self.metadata.take_profile_sessions(&profile.id);
        self.close_pooled(ids).await;
    }

    /// Closes pooled sessions unused for at least `idle` and returns how
    /// many were closed. The app calls it every minute with
    /// [`METADATA_IDLE`].
    pub async fn close_idle_metadata(&self, idle: Duration) -> usize {
        let ids = self.metadata.take_idle(idle);
        let count = ids.len();
        self.close_pooled(ids).await;
        count
    }

    /// The id of the catalog session serving a user session, if one is
    /// open (for diagnostics and tests).
    #[must_use]
    pub fn metadata_session_id(&self, user_session_id: &str) -> Option<String> {
        self.pooled_id(user_session_id, PoolRole::Catalog)
    }

    /// The id of the admin session serving a user session, if one is open.
    #[must_use]
    pub fn admin_session_id(&self, user_session_id: &str) -> Option<String> {
        self.pooled_id(user_session_id, PoolRole::Admin)
    }

    fn pooled_id(&self, user_session_id: &str, role: PoolRole) -> Option<String> {
        let info = self.sessions.info(user_session_id).ok()?;
        let id = self.metadata.get(&MetaKey::of(&info, role))?;
        self.sessions.info(&id).ok().map(|_| id)
    }
}

#[cfg(test)]
mod tests {
    use varve_records::{ConnectionInput, PERSONAL_WORKSPACE, Records};

    use super::*;

    fn profile(host: &str) -> Result<ConnectionProfile, Box<dyn std::error::Error>> {
        let input: ConnectionInput = serde_json::from_value(serde_json::json!({
            "name": "db", "engine": "postgres", "host": host, "port": 5432, "username": "app",
            "environment": "development", "tls": { "mode": "prefer" },
            "ssh": { "enabled": false, "host": "", "port": 22, "username": "", "auth": "password" },
            "options": { "readOnly": false, "confirmDestructive": true, "showSystemObjects": false, "redisDb": 0 }
        }))?;
        Ok(Records::preview_connection(PERSONAL_WORKSPACE, &input)?)
    }

    #[test]
    fn typed_secrets_follow_only_the_same_identity() -> Result<(), Box<dyn std::error::Error>> {
        let pool = MetadataPool::default();
        let original = profile("db.internal")?;
        pool.remember_typed(
            &original,
            Typed {
                password: Some(Secret::new("pw".to_string())),
                ..Typed::default()
            },
        );
        assert!(pool.typed_for(&original).password.is_some());

        let mut renamed = original.clone();
        renamed.name = "renamed".into();
        assert!(
            pool.typed_for(&renamed).password.is_some(),
            "a rename keeps them"
        );
        assert_eq!(
            connection_identity(&original),
            connection_identity(&renamed)
        );

        let mut moved = original.clone();
        moved.host = "evil.example".into();
        assert!(pool.typed_for(&moved).password.is_none(), "another server");
        assert_ne!(connection_identity(&original), connection_identity(&moved));
        pool.prune_typed(&moved);
        assert!(!pool.has_typed(&original.id));
        assert!(
            pool.typed_for(&original).password.is_none(),
            "dropped for good"
        );
        Ok(())
    }

    #[test]
    fn a_typed_tls_passphrase_follows_only_the_same_key_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let pool = MetadataPool::default();
        let mut original = profile("db.internal")?;
        original.tls.client_cert_path = Some("/c.pem".into());
        original.tls.client_key_path = Some("/k.pem".into());
        pool.remember_typed(
            &original,
            Typed {
                tls: Some(Secret::new("key-pass".to_string())),
                ..Typed::default()
            },
        );
        assert!(pool.typed_for(&original).tls.is_some());
        // Another server, same key file: the key's passphrase still fits.
        let mut moved = original.clone();
        moved.host = "other.example".into();
        assert!(pool.typed_for(&moved).tls.is_some());
        let mut other_key = original.clone();
        other_key.tls.client_key_path = Some("/other.pem".into());
        assert!(pool.typed_for(&other_key).tls.is_none(), "another key file");
        pool.prune_typed(&other_key);
        assert!(!pool.has_typed(&original.id));
        Ok(())
    }

    #[test]
    fn edits_move_the_generation() {
        let pool = MetadataPool::default();
        let before = pool.edit_generation("p1");
        pool.profile_edited("p1");
        assert_ne!(pool.edit_generation("p1"), before);
        assert_eq!(pool.edit_generation("p2"), 0);
    }

    #[test]
    fn pool_tracks_sessions_per_profile_role_and_database() {
        let pool = MetadataPool::default();
        let key = |db: Option<&str>, role| MetaKey {
            connection_id: "p1".into(),
            identity: "i".into(),
            database: db.map(Into::into),
            role,
        };
        pool.insert(key(Some("app"), PoolRole::Catalog), "s1".into());
        pool.insert(key(None, PoolRole::Admin), "s2".into());
        assert!(pool.get(&key(Some("app"), PoolRole::Catalog)).is_some());
        assert!(pool.get(&key(Some("other"), PoolRole::Catalog)).is_none());
        let mut ids = pool.take_profile_sessions("p1");
        ids.sort();
        assert_eq!(ids, ["s1", "s2"]);
        pool.insert(key(None, PoolRole::Admin), "s3".into());
        assert!(pool.take_idle(Duration::from_secs(3600)).is_empty());
        assert_eq!(pool.take_idle(Duration::ZERO), ["s3"]);
        assert!(!format!("{pool:?}").contains("pw"));
    }
}
