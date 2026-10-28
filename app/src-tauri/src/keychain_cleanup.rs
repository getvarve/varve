//! Keychain entries that could not be removed.
//!
//! When a profile is deleted, or edited so that its saved secrets no longer
//! belong to it (another server or route), the profile stops listing those
//! secrets at once, so they are never sent anywhere. Removing them from the
//! OS keychain can fail (a locked keychain, an item another build created).
//! Those entries are recorded in `app.db` and removed later: in the
//! background at every start and after every profile edit or delete
//! ([`Services::retry_keychain_cleanup_in_background`]), a profile's own
//! entries before it is edited or deleted, and on request
//! ([`Services::retry_keychain_cleanup`]). Failures are reported, never
//! silently dropped.
//!
//! Locks, always taken in this order:
//!
//! 1. the profile's record lock (`Services::with_profile_record`): every
//!    read-modify-write of a profile and its list of saved secrets (edits,
//!    saving and forgetting a secret, deleting, the cleanup) holds it, and
//!    secret list changes are applied to the list as stored at that moment.
//!    A folder delete holds the locks of every connection it removes; it
//!    takes them without waiting and, when one is busy, releases all of
//!    them and waits for that one alone, so it never holds a connection
//!    while waiting for another;
//! 2. the folder tree lock (`tree_lock`), only around the `app.db` writes
//!    that change where connections live. It is never held while waiting
//!    for a record lock or across a keychain call, so a slow keychain (a
//!    macOS prompt can wait for minutes) only holds up the connection whose
//!    secret it is writing;
//! 3. the secret slot lock (`Services::with_secret_slot`) for keychain
//!    writes and deletes of one profile and kind;
//! 4. the cleanup list's own lock.
//!
//! A profile edit or delete records the entries it is about to unlist as
//! pending removals before it unlists them (an edit in the same `app.db`
//! transaction as its secret version marks), and takes each off the list
//! once the keychain removed it. A crash at any point therefore leaves
//! every unlisted entry on the list; entries whose profile still lists
//! them are taken off by the cleanup without removing anything.
//!
//! Saving a secret first takes the slot off the cleanup list, and the
//! cleanup re-checks under the locks that the slot is still listed for
//! removal and not saved again, so a live secret is never deleted.
//!
//! Which profile version a secret must have been entered for is kept in
//! `crate::secret_versions`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use varve_records::SecretKindName;
use varve_secrets::SecretKey;
use varve_store::{Store, StoreError};

use crate::commands::Services;

/// `core_meta` key of the pending deletions.
const PENDING_KEY: &str = "secrets.pending_deletions";

/// Serializes the read, change and write of the pending list.
static PENDING_LOCK: Mutex<()> = Mutex::new(());

/// The locks of the secret slots, by profile id and kind.
type SlotLocks = HashMap<(String, String), Arc<Mutex<()>>>;

/// The record locks, by profile id.
type RecordLocks = HashMap<String, Arc<Mutex<()>>>;

/// One lock per profile record.
static RECORD_LOCKS: Mutex<Option<RecordLocks>> = Mutex::new(None);

/// One lock per secret slot (profile id, kind).
static SLOT_LOCKS: Mutex<Option<SlotLocks>> = Mutex::new(None);

/// Takes a lock, recovering it when a thread panicked while holding it:
/// the data it guards lives in `app.db` and the keychain, never half
/// written in memory.
pub(crate) fn lock_recovering<'a, T>(lock: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        tracing::warn!(what, "recovering a lock after a panic in another thread");
        poisoned.into_inner()
    })
}

/// Serializes changes of where connections live (create, duplicate, move,
/// edit, folder moves and deletes), so the connections a folder delete
/// removes are exactly the ones whose record locks it holds. Taken after
/// any record lock, only around `app.db` writes: never while waiting for a
/// record lock and never across a keychain call.
static TREE_LOCK: Mutex<()> = Mutex::new(());

/// Takes the folder tree lock.
pub(crate) fn tree_lock() -> MutexGuard<'static, ()> {
    lock_recovering(&TREE_LOCK, "the folder tree")
}

/// Takes a profile record lock got from [`Services::profile_record_lock`].
pub(crate) fn lock_guard(lock: &Mutex<()>) -> MutexGuard<'_, ()> {
    lock_recovering(lock, "a profile record")
}

/// Takes a profile record lock without waiting: `None` when another
/// thread holds it.
pub(crate) fn try_lock_guard(lock: &Mutex<()>) -> Option<MutexGuard<'_, ()>> {
    match lock.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            tracing::warn!("recovering a lock after a panic in another thread");
            Some(poisoned.into_inner())
        }
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The lock of one secret slot. Locks nobody holds or waits for are
/// dropped on the way, so the map does not grow with deleted profiles.
fn slot_lock(connection_id: &str, kind: SecretKindName) -> Arc<Mutex<()>> {
    let mut map = lock_recovering(&SLOT_LOCKS, "the keychain slot locks");
    let map = map.get_or_insert_with(HashMap::new);
    map.retain(|_, lock| Arc::strong_count(lock) > 1);
    Arc::clone(
        map.entry((connection_id.to_string(), format!("{kind:?}")))
            .or_default(),
    )
}

/// The record lock of one profile.
fn record_lock(connection_id: &str) -> Arc<Mutex<()>> {
    let mut map = lock_recovering(&RECORD_LOCKS, "the profile record locks");
    let map = map.get_or_insert_with(HashMap::new);
    map.retain(|_, lock| Arc::strong_count(lock) > 1);
    Arc::clone(map.entry(connection_id.to_string()).or_default())
}

pub(crate) fn meta_get(store: &Store, key: &str) -> Result<Option<String>, String> {
    store
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT value FROM core_meta WHERE key = ?1")?;
            let mut rows = stmt.query([key])?;
            Ok(match rows.next()? {
                Some(row) => Some(row.get(0)?),
                None => None,
            })
        })
        .map_err(|e: StoreError| e.to_string())
}

pub(crate) fn meta_set(store: &Store, key: &str, value: &str) -> Result<(), String> {
    store
        .with_conn(|c| {
            c.execute(
                "INSERT INTO core_meta (key, value) VALUES (?1, ?2) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [key, value],
            )?;
            Ok(())
        })
        .map_err(|e| e.to_string())
}

/// A keychain entry still to be removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingDeletion {
    /// The profile the secret belonged to.
    pub connection_id: String,
    /// Which secret.
    pub kind: SecretKindName,
}

impl PendingDeletion {
    /// The entries of `kinds` of one profile.
    pub(crate) fn of(connection_id: &str, kinds: &[SecretKindName]) -> Vec<Self> {
        kinds
            .iter()
            .map(|kind| Self {
                connection_id: connection_id.to_string(),
                kind: *kind,
            })
            .collect()
    }
}

fn load(store: &Store) -> Result<Vec<PendingDeletion>, String> {
    match meta_get(store, PENDING_KEY)? {
        Some(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
        None => Ok(Vec::new()),
    }
}

fn save(store: &Store, list: &[PendingDeletion]) -> Result<(), String> {
    let json = serde_json::to_string(list).map_err(|e| e.to_string())?;
    meta_set(store, PENDING_KEY, &json)
}

impl Services {
    /// Runs `work` while holding a profile's record lock. Every
    /// read-modify-write of a profile and its list of saved secrets goes
    /// through here; take it before any secret slot lock.
    pub(crate) fn with_profile_record<T>(
        &self,
        connection_id: &str,
        work: impl FnOnce() -> T,
    ) -> T {
        let lock = record_lock(connection_id);
        let _record = lock_recovering(&lock, "a profile record");
        work()
    }

    /// The record lock of a profile, for callers that must hold several
    /// (always taken in profile id order).
    pub(crate) fn profile_record_lock(&self, connection_id: &str) -> Arc<Mutex<()>> {
        record_lock(connection_id)
    }

    /// Runs `work` while holding the lock of a secret slot. Every keychain
    /// write or delete of that slot goes through here.
    pub(crate) fn with_secret_slot<T>(
        &self,
        connection_id: &str,
        kind: SecretKindName,
        work: impl FnOnce() -> T,
    ) -> T {
        let lock = slot_lock(connection_id, kind);
        let _slot = lock_recovering(&lock, "a keychain slot");
        work()
    }

    /// Records a keychain entry whose removal failed, for a later retry.
    /// Call it while holding the slot's lock.
    pub(crate) fn queue_secret_deletion(&self, connection_id: &str, kind: SecretKindName) {
        let _guard = lock_recovering(&PENDING_LOCK, "the keychain cleanup list");
        let result = load(&self.store).and_then(|mut list| {
            let entry = PendingDeletion {
                connection_id: connection_id.to_string(),
                kind,
            };
            if !list.contains(&entry) {
                list.push(entry);
            }
            save(&self.store, &list)
        });
        if let Err(error) = result {
            tracing::warn!(%error, "could not record a keychain entry to remove later");
        }
    }

    /// Records keychain entries that are about to be unlisted, before
    /// they are unlisted, so a crash leaves them recorded. `also` (a
    /// `core_meta` key and value) is written in the same transaction. Call
    /// it under the record locks of the profiles.
    ///
    /// # Errors
    /// The store cannot be read or written: the caller must not unlist.
    pub(crate) fn queue_secret_deletions(
        &self,
        entries: &[PendingDeletion],
        also: Option<(&str, &str)>,
    ) -> Result<(), String> {
        let _guard = lock_recovering(&PENDING_LOCK, "the keychain cleanup list");
        let mut list = load(&self.store)?;
        for entry in entries {
            if !list.contains(entry) {
                list.push(entry.clone());
            }
        }
        let json = serde_json::to_string(&list).map_err(|e| e.to_string())?;
        self.store
            .with_conn(|c| {
                let tx = c.unchecked_transaction()?;
                let upsert = "INSERT INTO core_meta (key, value) VALUES (?1, ?2) \
                              ON CONFLICT (key) DO UPDATE SET value = excluded.value";
                tx.execute(upsert, [PENDING_KEY, json.as_str()])?;
                if let Some((key, value)) = also {
                    tx.execute(upsert, [key, value])?;
                }
                tx.commit()?;
                Ok(())
            })
            .map_err(|e| e.to_string())
    }

    /// Takes entries off the cleanup list that stay listed after all (the
    /// change that queued them was not stored). Call it under the record
    /// locks of the profiles.
    pub(crate) fn unqueue_secret_deletions(&self, entries: &[PendingDeletion]) {
        let _guard = lock_recovering(&PENDING_LOCK, "the keychain cleanup list");
        let result = load(&self.store).and_then(|mut list| {
            let before = list.len();
            list.retain(|e| !entries.contains(e));
            if list.len() == before {
                Ok(())
            } else {
                save(&self.store, &list)
            }
        });
        if let Err(error) = result {
            // The entries are still listed by their profiles, so the
            // cleanup takes them off without removing anything.
            tracing::warn!(%error, "could not update the keychain cleanup list");
        }
    }

    /// Removes a recorded keychain entry and takes it off the list; on
    /// failure it stays recorded for a later retry. Call it under the
    /// profile's record lock, after the entry was recorded.
    ///
    /// # Errors
    /// The keychain refused the delete.
    pub(crate) fn remove_queued_secret(
        &self,
        connection_id: &str,
        kind: SecretKindName,
    ) -> Result<(), varve_secrets::SecretError> {
        self.with_secret_slot(connection_id, kind, || {
            let key = SecretKey::new(connection_id.to_string(), kind.into())?;
            match self.secrets.delete(&key) {
                Ok(()) => {
                    self.unqueue_secret_deletion(connection_id, kind);
                    Ok(())
                }
                Err(error) => {
                    // Normally recorded already; recorded again in case
                    // the first record was taken off by a save meanwhile.
                    self.queue_secret_deletion(connection_id, kind);
                    Err(error)
                }
            }
        })
    }

    /// Takes a slot off the cleanup list, because a secret is being saved
    /// in it. Call it while holding the slot's lock.
    pub(crate) fn unqueue_secret_deletion(&self, connection_id: &str, kind: SecretKindName) {
        let _guard = lock_recovering(&PENDING_LOCK, "the keychain cleanup list");
        let result = load(&self.store).and_then(|mut list| {
            let before = list.len();
            list.retain(|e| !(e.connection_id == connection_id && e.kind == kind));
            if list.len() == before {
                Ok(())
            } else {
                save(&self.store, &list)
            }
        });
        if let Err(error) = result {
            tracing::warn!(%error, "could not update the keychain cleanup list");
        }
    }

    fn still_pending(&self, entry: &PendingDeletion) -> bool {
        let _guard = lock_recovering(&PENDING_LOCK, "the keychain cleanup list");
        load(&self.store).is_ok_and(|list| list.contains(entry))
    }

    /// Keychain entries still to be removed.
    #[must_use]
    pub fn pending_keychain_deletions(&self) -> Vec<PendingDeletion> {
        let _guard = lock_recovering(&PENDING_LOCK, "the keychain cleanup list");
        load(&self.store).unwrap_or_else(|error| {
            tracing::warn!(%error, "could not read the keychain entries to remove");
            Vec::new()
        })
    }

    /// Tries to remove every recorded keychain entry again, waiting for
    /// each profile's record lock. Each entry is handled under its slot's
    /// lock and re-checked there: it must still be on the list (a save
    /// takes it off) and its profile must not list the secret again.
    /// Returns a message per entry that still could not be removed.
    pub fn retry_keychain_cleanup(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for entry in self.pending_keychain_deletions() {
            let outcome =
                self.with_profile_record(&entry.connection_id, || self.retry_entry(&entry));
            if let Err(error) = outcome {
                problems.push(describe(&entry, &error));
            }
        }
        problems
    }

    /// Retries the recorded entries of one profile only. Call it under
    /// that profile's record lock (edits and deletes do, before they
    /// change the profile); other profiles' entries are left to
    /// [`Self::retry_keychain_cleanup_in_background`].
    pub(crate) fn retry_own_keychain_cleanup(&self, connection_id: &str) -> Vec<String> {
        let mut problems = Vec::new();
        for entry in self.pending_keychain_deletions() {
            if entry.connection_id != connection_id {
                continue;
            }
            if let Err(error) = self.retry_entry(&entry) {
                problems.push(describe(&entry, &error));
            }
        }
        problems
    }

    /// Runs [`Self::retry_keychain_cleanup`] on a background thread and
    /// returns at once, so no command waits for another connection's
    /// keychain call. At most one runs per [`Services`]; a request while
    /// one runs makes it go over the list once more when it ends.
    pub fn retry_keychain_cleanup_in_background(self: &Arc<Self>) {
        let state = &self.keychain_cleanup;
        state.again.store(true, Ordering::SeqCst);
        if state.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let services = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("varve-keychain-cleanup".into())
            .spawn(move || {
                let state = &services.keychain_cleanup;
                loop {
                    state.again.store(false, Ordering::SeqCst);
                    for problem in services.retry_keychain_cleanup() {
                        tracing::warn!(%problem, "a keychain entry still could not be removed");
                    }
                    state.running.store(false, Ordering::SeqCst);
                    // A request that came in while the pass ran, and that
                    // no other thread took over, gets one more pass.
                    if !state.again.load(Ordering::SeqCst)
                        || state.running.swap(true, Ordering::SeqCst)
                    {
                        break;
                    }
                }
            });
        if let Err(error) = spawned {
            state.running.store(false, Ordering::SeqCst);
            tracing::warn!(%error, "could not start the keychain cleanup");
        }
    }

    /// Handles one recorded entry under its slot lock. Call it under the
    /// profile's record lock.
    fn retry_entry(&self, entry: &PendingDeletion) -> Result<(), String> {
        self.with_secret_slot(&entry.connection_id, entry.kind, || {
            if !self.still_pending(entry) {
                return Ok(());
            }
            let relisted = self
                .records
                .connection(&entry.connection_id)
                .is_ok_and(|p| p.saved_secrets.contains(&entry.kind));
            if !relisted {
                SecretKey::new(entry.connection_id.clone(), entry.kind.into())
                    .map_err(|e| e.to_string())
                    .and_then(|key| self.secrets.delete(&key).map_err(|e| e.to_string()))?;
            }
            // Still under the slot lock: a failed delete queued meanwhile
            // cannot be dropped by mistake.
            self.unqueue_secret_deletion(&entry.connection_id, entry.kind);
            Ok(())
        })
    }
}

fn describe(entry: &PendingDeletion, error: &str) -> String {
    format!(
        "{:?} of connection {}: {error}",
        entry.kind, entry.connection_id
    )
}

/// Whether a background cleanup runs, and whether another pass was asked
/// for while it ran.
#[derive(Debug, Default)]
pub struct BackgroundCleanup {
    running: AtomicBool,
    again: AtomicBool,
}

impl BackgroundCleanup {
    /// True while a background pass runs (or is about to run again).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_deletions_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let store = Store::open_in_memory()?;
        store.migrate(&[varve_store::core_migrations()])?;
        assert!(load(&store)?.is_empty());
        let list = vec![PendingDeletion {
            connection_id: "p1".into(),
            kind: SecretKindName::DbPassword,
        }];
        save(&store, &list)?;
        assert_eq!(load(&store)?, list);
        Ok(())
    }
}
