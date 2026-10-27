//! OS keychain access for Varve.
//!
//! Passwords, passphrases and keys live only in the operating system's
//! credential store (macOS Keychain, Windows Credential Manager, Secret
//! Service on Linux). They are never written to `app.db`, never logged and
//! never synced (design doc section 3.1).
//!
//! Entries use the service `varve` and the account
//! `<profile id>/<secret kind>`, for example `5f0c.../db_password`.
//!
//! Every read of the OS store can ask the user for permission (macOS asks
//! for the login password until the app is allowed for good), so the app
//! reads through a [`SecretCache`]: one read per secret per run, shared by
//! every session of a profile.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use zeroize::Zeroizing;

/// Keychain service name for every Varve secret.
pub const SERVICE: &str = "varve";

/// Which secret of a connection profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    /// Database password.
    DbPassword,
    /// SSH password.
    SshPassword,
    /// Passphrase of the SSH private key.
    SshKeyPassphrase,
    /// Passphrase of the TLS client key.
    TlsKeyPassphrase,
}

impl SecretKind {
    /// Stable name used in the keychain account.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SecretKind::DbPassword => "db_password",
            SecretKind::SshPassword => "ssh_password",
            SecretKind::SshKeyPassphrase => "ssh_key_passphrase",
            SecretKind::TlsKeyPassphrase => "tls_key_passphrase",
        }
    }
}

/// Address of one secret.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretKey {
    profile_id: String,
    kind: SecretKind,
}

impl SecretKey {
    /// The profile the secret belongs to.
    #[must_use]
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    /// Which secret of the profile.
    #[must_use]
    pub fn kind(&self) -> SecretKind {
        self.kind
    }

    /// Creates a key for a connection profile.
    ///
    /// # Errors
    /// Returns [`SecretError::InvalidProfileId`] when the id is empty or
    /// contains characters other than ASCII letters, digits and `-`.
    pub fn new(profile_id: impl Into<String>, kind: SecretKind) -> Result<Self, SecretError> {
        let profile_id = profile_id.into();
        let valid = !profile_id.is_empty()
            && profile_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if valid {
            Ok(Self { profile_id, kind })
        } else {
            Err(SecretError::InvalidProfileId(profile_id))
        }
    }

    /// Keychain account name.
    #[must_use]
    pub fn account(&self) -> String {
        format!("{}/{}", self.profile_id, self.kind.as_str())
    }
}

/// A secret value. Its memory is wiped on drop and it never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wraps a value.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// The secret text. Pass it straight to the driver; never log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// Errors from the secret store.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The profile id cannot be used in a keychain account name.
    #[error("invalid profile id `{0}` for a keychain entry")]
    InvalidProfileId(String),
    /// The OS credential store failed or is not available.
    #[error("the system keychain is not available: {0}")]
    Keychain(String),
    /// The in-memory store mutex was poisoned.
    #[error("the secret store is unusable after a panic in another thread")]
    Poisoned,
}

/// Somewhere secrets can be kept.
pub trait SecretStore: Send + Sync {
    /// Stores or replaces a secret.
    ///
    /// # Errors
    /// Returns an error when the store rejects the write.
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError>;

    /// Reads a secret; `Ok(None)` when none is saved.
    ///
    /// # Errors
    /// Returns an error when the store cannot be read.
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError>;

    /// Removes a secret. Removing a secret that does not exist succeeds.
    ///
    /// # Errors
    /// Returns an error when the store rejects the delete.
    fn delete(&self, key: &SecretKey) -> Result<(), SecretError>;
}

/// The operating system credential store.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeychainStore;

impl KeychainStore {
    fn entry(key: &SecretKey) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(SERVICE, &key.account())
            .map_err(|e| SecretError::Keychain(e.to_string()))
    }
}

impl SecretStore for KeychainStore {
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        Self::entry(key)?
            .set_password(secret.expose())
            .map_err(|e| SecretError::Keychain(e.to_string()))
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        match Self::entry(key)?.get_password() {
            Ok(value) => Ok(Some(Secret::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError::Keychain(e.to_string())),
        }
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Keychain(e.to_string())),
        }
    }
}

/// A process-local store for tests and for machines without a keychain.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: Mutex<HashMap<String, Secret>>,
}

impl SecretStore for MemoryStore {
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        let mut entries = self.entries.lock().map_err(|_| SecretError::Poisoned)?;
        entries.insert(key.account(), secret.clone());
        Ok(())
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        let entries = self.entries.lock().map_err(|_| SecretError::Poisoned)?;
        Ok(entries.get(&key.account()).cloned())
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        let mut entries = self.entries.lock().map_err(|_| SecretError::Poisoned)?;
        entries.remove(&key.account());
        Ok(())
    }
}

/// Keeps secrets in memory after their first successful read, so every
/// session of a profile (user, metadata, admin, listener, Pub/Sub, edits)
/// shares one read of the OS store per secret per app run.
///
/// Writes go to the inner store and update the cache; deletes drop the
/// cached value before the inner store is asked. Only values that were
/// found are kept: a missing secret is looked up again next time, so a
/// secret saved meanwhile is seen. Cached values are wiped on drop and
/// never printed or serialized. The whole cache, or one profile, can be
/// forgotten at any time, and remembering can be switched off entirely.
///
/// A read of the inner store can take long (a macOS prompt). Every change
/// of the cache bumps a generation, and a value read from the inner store
/// is only kept when the generation is still the one from before the read:
/// a secret saved, forgotten or dropped meanwhile is never overwritten by
/// the older value that came back.
pub struct SecretCache {
    inner: Arc<dyn SecretStore>,
    state: Mutex<CacheState>,
    remember: AtomicBool,
    hits: AtomicU64,
}

/// The cached values and the generation of the last change.
#[derive(Default)]
struct CacheState {
    values: HashMap<String, Secret>,
    generation: u64,
}

impl fmt::Debug for SecretCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretCache")
            .field("remember", &self.remember())
            .field("cached", &self.len())
            .finish_non_exhaustive()
    }
}

impl SecretCache {
    /// Caches reads of `inner`. With `remember` off, every read goes to
    /// the inner store and nothing is kept.
    #[must_use]
    pub fn new(inner: Arc<dyn SecretStore>, remember: bool) -> Self {
        Self {
            inner,
            state: Mutex::new(CacheState::default()),
            remember: AtomicBool::new(remember),
            hits: AtomicU64::new(0),
        }
    }

    /// The state, recovered when a thread panicked while holding it: it
    /// is only a cache, and wiping or reading it stays correct.
    fn state(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether secrets are kept after their first read.
    #[must_use]
    pub fn remember(&self) -> bool {
        self.remember.load(Ordering::Acquire)
    }

    /// Switches remembering on or off; switching it off forgets everything.
    pub fn set_remember(&self, remember: bool) {
        let mut state = self.state();
        self.remember.store(remember, Ordering::Release);
        state.generation += 1;
        if !remember {
            state.values.clear();
        }
    }

    /// Drops every cached secret. Later reads go to the inner store again.
    pub fn forget_all(&self) {
        let mut state = self.state();
        state.generation += 1;
        state.values.clear();
    }

    /// Drops the cached secrets of one profile. Returns whether any were
    /// cached.
    pub fn forget_profile(&self, profile_id: &str) -> bool {
        let mut state = self.state();
        state.generation += 1;
        let before = state.values.len();
        let prefix = format!("{profile_id}/");
        state
            .values
            .retain(|account, _| !account.starts_with(&prefix));
        state.values.len() != before
    }

    /// Whether a secret is cached, so a caller knows whether a read that
    /// follows is answered from the cache (and a refused secret may be a
    /// stale one).
    #[must_use]
    pub fn has(&self, key: &SecretKey) -> bool {
        self.state().values.contains_key(&key.account())
    }

    /// Whether any secret of a profile is cached.
    #[must_use]
    pub fn has_profile(&self, profile_id: &str) -> bool {
        let prefix = format!("{profile_id}/");
        self.state()
            .values
            .keys()
            .any(|account| account.starts_with(&prefix))
    }

    /// How many cached secrets are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state().values.len()
    }

    /// Whether no secret is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many reads were answered from the cache.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// The cached value and the current generation.
    fn cached(&self, key: &SecretKey) -> (Option<Secret>, u64) {
        let state = self.state();
        (state.values.get(&key.account()).cloned(), state.generation)
    }

    /// Keeps a value read at `generation`, unless the cache changed since
    /// then or remembering is off.
    fn keep(&self, key: &SecretKey, secret: &Secret, generation: u64) {
        let mut state = self.state();
        if !self.remember() || state.generation != generation {
            return;
        }
        state.values.insert(key.account(), secret.clone());
    }

    /// Keeps a value the inner store has just stored, unless the cache was
    /// forgotten (or remembering switched off) since `generation`, the
    /// one from before the write. The generation is bumped either way, so
    /// a read that started during the (possibly slow) write and saw the
    /// old value cannot keep it afterwards.
    fn keep_written(&self, key: &SecretKey, secret: &Secret, generation: u64) {
        let mut state = self.state();
        let unchanged = state.generation == generation;
        state.generation += 1;
        if unchanged && self.remember() {
            state.values.insert(key.account(), secret.clone());
        }
    }

    /// Drops a cached value and bumps the generation; returns the new
    /// generation.
    fn drop_cached(&self, key: &SecretKey) -> u64 {
        let mut state = self.state();
        state.generation += 1;
        state.values.remove(&key.account());
        state.generation
    }
}

impl SecretStore for SecretCache {
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        // The old value goes first: a failed write leaves nothing stale.
        let generation = self.drop_cached(key);
        self.inner.set(key, secret)?;
        self.keep_written(key, secret, generation);
        Ok(())
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        let (cached, generation) = self.cached(key);
        if self.remember()
            && let Some(secret) = cached
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Some(secret));
        }
        let found = self.inner.get(key)?;
        if let Some(secret) = &found {
            self.keep(key, secret, generation);
        }
        Ok(found)
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        self.drop_cached(key);
        let result = self.inner.delete(key);
        // Again after the (possibly slow) delete: a read that started
        // during it and found the value must not keep it.
        self.drop_cached(key);
        result
    }
}

/// Counts the calls that reach a store. For tests that prove how often the
/// OS store would be asked.
#[derive(Debug)]
pub struct CountingStore<S> {
    inner: S,
    reads: AtomicU64,
    writes: AtomicU64,
    deletes: AtomicU64,
}

impl<S: SecretStore> CountingStore<S> {
    /// Counts the calls that reach `inner`.
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            deletes: AtomicU64::new(0),
        }
    }

    /// The wrapped store.
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Reads that reached the inner store so far.
    #[must_use]
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Writes that reached the inner store so far.
    #[must_use]
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    /// Deletes that reached the inner store so far.
    #[must_use]
    pub fn deletes(&self) -> u64 {
        self.deletes.load(Ordering::Relaxed)
    }
}

impl<S: SecretStore> SecretStore for CountingStore<S> {
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.set(key, secret)
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(key)
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        self.deletes.fetch_add(1, Ordering::Relaxed);
        self.inner.delete(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counted() -> (Arc<CountingStore<MemoryStore>>, SecretCache) {
        let store = Arc::new(CountingStore::new(MemoryStore::default()));
        let cache = SecretCache::new(Arc::clone(&store) as Arc<dyn SecretStore>, true);
        (store, cache)
    }

    fn text(secret: Option<Secret>) -> Option<String> {
        secret.map(|s| s.expose().to_string())
    }

    #[test]
    fn a_secret_is_read_from_the_store_once() -> Result<(), SecretError> {
        let (store, cache) = counted();
        let key = SecretKey::new("p1", SecretKind::DbPassword)?;
        store.inner().set(&key, &Secret::new("pw".into()))?;
        for _ in 0..5 {
            assert_eq!(text(cache.get(&key)?), Some("pw".into()));
        }
        assert_eq!(store.reads(), 1);
        assert_eq!(cache.hits(), 4);
        assert_eq!(cache.len(), 1);
        Ok(())
    }

    #[test]
    fn a_missing_secret_is_looked_up_again() -> Result<(), SecretError> {
        let (store, cache) = counted();
        let key = SecretKey::new("p1", SecretKind::DbPassword)?;
        assert!(cache.get(&key)?.is_none());
        assert!(cache.get(&key)?.is_none());
        assert_eq!(store.reads(), 2);
        // Saved elsewhere meanwhile: seen on the next read.
        store.inner().set(&key, &Secret::new("late".into()))?;
        assert_eq!(text(cache.get(&key)?), Some("late".into()));
        assert_eq!(store.reads(), 3);
        Ok(())
    }

    #[test]
    fn writes_fill_and_deletes_drop_the_cache() -> Result<(), SecretError> {
        let (store, cache) = counted();
        let key = SecretKey::new("p1", SecretKind::SshPassword)?;
        cache.set(&key, &Secret::new("one".into()))?;
        assert_eq!(text(cache.get(&key)?), Some("one".into()));
        assert_eq!(store.reads(), 0, "a saved secret needs no read");
        cache.set(&key, &Secret::new("two".into()))?;
        assert_eq!(text(cache.get(&key)?), Some("two".into()));
        cache.delete(&key)?;
        assert!(cache.is_empty());
        assert!(cache.get(&key)?.is_none());
        assert_eq!(store.deletes(), 1);
        Ok(())
    }

    #[test]
    fn forgetting_a_profile_leaves_the_others() -> Result<(), SecretError> {
        let (store, cache) = counted();
        let a = SecretKey::new("aa", SecretKind::DbPassword)?;
        let a_ssh = SecretKey::new("aa", SecretKind::SshKeyPassphrase)?;
        let b = SecretKey::new("bb", SecretKind::DbPassword)?;
        // A profile id that starts with another's id is a different prefix.
        let a_long = SecretKey::new("aa-2", SecretKind::DbPassword)?;
        for key in [&a, &a_ssh, &b, &a_long] {
            cache.set(key, &Secret::new("x".into()))?;
        }
        assert_eq!(cache.len(), 4);
        assert!(cache.forget_profile("aa"));
        assert!(!cache.forget_profile("aa"));
        assert_eq!(cache.len(), 2);
        cache.get(&b)?;
        cache.get(&a_long)?;
        assert_eq!(store.reads(), 0);
        cache.get(&a)?;
        assert_eq!(store.reads(), 1);
        cache.forget_all();
        assert!(cache.is_empty());
        Ok(())
    }

    #[test]
    fn remembering_can_be_switched_off() -> Result<(), SecretError> {
        let (store, cache) = counted();
        let key = SecretKey::new("p1", SecretKind::DbPassword)?;
        cache.set(&key, &Secret::new("pw".into()))?;
        assert_eq!(cache.len(), 1);
        cache.set_remember(false);
        assert!(!cache.remember());
        assert!(cache.is_empty());
        cache.get(&key)?;
        cache.get(&key)?;
        assert_eq!(store.reads(), 2);
        assert!(cache.is_empty());
        cache.set(&key, &Secret::new("pw2".into()))?;
        assert!(cache.is_empty(), "nothing is kept while remembering is off");
        cache.set_remember(true);
        cache.get(&key)?;
        cache.get(&key)?;
        assert_eq!(store.reads(), 3);
        Ok(())
    }

    /// A store whose reads are held back until the test lets them through,
    /// like a keychain read that returns only after the user's answer.
    struct BlockingStore {
        inner: MemoryStore,
        entered: std::sync::Barrier,
        release: std::sync::Barrier,
    }

    impl SecretStore for BlockingStore {
        fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
            self.inner.set(key, secret)
        }

        fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
            // The value is read first and then held back, like a result
            // that is in flight while the user answers the prompt.
            let found = self.inner.get(key);
            self.entered.wait();
            self.release.wait();
            found
        }

        fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
            self.inner.delete(key)
        }
    }

    /// Runs `invalidate` while a read of `key` is waiting on the store,
    /// then lets the read finish, and returns what the cache holds for
    /// `key` afterwards.
    fn read_during(
        invalidate: impl FnOnce(&SecretCache),
    ) -> Result<(Option<String>, Option<String>), SecretError> {
        let store = Arc::new(BlockingStore {
            inner: MemoryStore::default(),
            entered: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        });
        let key = SecretKey::new("p1", SecretKind::DbPassword)?;
        store.inner.set(&key, &Secret::new("old".into()))?;
        let cache = Arc::new(SecretCache::new(
            Arc::clone(&store) as Arc<dyn SecretStore>,
            true,
        ));
        let reader = {
            let cache = Arc::clone(&cache);
            let key = key.clone();
            std::thread::spawn(move || cache.get(&key).map(text))
        };
        store.entered.wait();
        invalidate(&cache);
        store.release.wait();
        let returned = reader
            .join()
            .map_err(|_| SecretError::Keychain("the reader panicked".into()))??;
        let (kept, _) = cache.cached(&key);
        Ok((returned, text(kept)))
    }

    #[test]
    fn a_secret_saved_during_a_slow_read_is_not_overwritten() -> Result<(), SecretError> {
        let (returned, kept) = read_during(|cache| {
            let key = SecretKey::new("p1", SecretKind::DbPassword).ok();
            if let Some(key) = key {
                let _ = cache.set(&key, &Secret::new("new".into()));
            }
        })?;
        assert_eq!(
            returned,
            Some("old".into()),
            "the slow read returns what it read"
        );
        assert_eq!(kept, Some("new".into()), "the cache keeps the newer save");
        Ok(())
    }

    #[test]
    fn a_profile_forgotten_during_a_slow_read_stays_forgotten() -> Result<(), SecretError> {
        for invalidate in [
            (|cache: &SecretCache| {
                cache.forget_profile("p1");
            }) as fn(&SecretCache),
            |cache| cache.forget_all(),
            |cache| cache.set_remember(false),
            |cache| {
                if let Ok(key) = SecretKey::new("p1", SecretKind::DbPassword) {
                    let _ = cache.delete(&key);
                }
            },
        ] {
            let (returned, kept) = read_during(invalidate)?;
            assert_eq!(returned, Some("old".into()));
            assert_eq!(kept, None, "nothing from before the change is kept");
        }
        Ok(())
    }

    /// A store whose writes and deletes wait until the test lets them
    /// through (a keychain prompting before it writes), while reads return
    /// at once but hand their result over only when released.
    struct SlowWriteStore {
        inner: MemoryStore,
        write_entered: std::sync::Barrier,
        write_release: std::sync::Barrier,
        read_entered: std::sync::Barrier,
        read_release: std::sync::Barrier,
    }

    impl SlowWriteStore {
        fn new() -> Self {
            Self {
                inner: MemoryStore::default(),
                write_entered: std::sync::Barrier::new(2),
                write_release: std::sync::Barrier::new(2),
                read_entered: std::sync::Barrier::new(2),
                read_release: std::sync::Barrier::new(2),
            }
        }
    }

    impl SecretStore for SlowWriteStore {
        fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
            self.write_entered.wait();
            self.write_release.wait();
            self.inner.set(key, secret)
        }

        fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
            let found = self.inner.get(key);
            self.read_entered.wait();
            self.read_release.wait();
            found
        }

        fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
            self.write_entered.wait();
            self.write_release.wait();
            self.inner.delete(key)
        }
    }

    /// Starts `write` on the cache, lets a read of the old value begin
    /// while the write waits on the store, finishes the write, then lets
    /// the read keep its (old) result. Returns what is cached afterwards.
    fn read_during_write(
        write: impl FnOnce(&SecretCache, &SecretKey) -> Result<(), SecretError> + Send + 'static,
    ) -> Result<Option<String>, SecretError> {
        let store = Arc::new(SlowWriteStore::new());
        let key = SecretKey::new("p1", SecretKind::DbPassword)?;
        store.inner.set(&key, &Secret::new("old".into()))?;
        let cache = Arc::new(SecretCache::new(
            Arc::clone(&store) as Arc<dyn SecretStore>,
            true,
        ));
        let writer = {
            let (cache, key) = (Arc::clone(&cache), key.clone());
            std::thread::spawn(move || write(&cache, &key))
        };
        store.write_entered.wait();
        let reader = {
            let (cache, key) = (Arc::clone(&cache), key.clone());
            std::thread::spawn(move || cache.get(&key).map(text))
        };
        store.read_entered.wait();
        store.write_release.wait();
        writer
            .join()
            .map_err(|_| SecretError::Keychain("the writer panicked".into()))??;
        store.read_release.wait();
        let returned = reader
            .join()
            .map_err(|_| SecretError::Keychain("the reader panicked".into()))??;
        assert_eq!(returned, Some("old".into()), "the read saw the old value");
        let (kept, _) = cache.cached(&key);
        Ok(text(kept))
    }

    /// Runs `forget` while a save waits on the store, lets the save
    /// finish, and returns what is cached afterwards.
    fn forget_during_save(forget: fn(&SecretCache)) -> Result<usize, SecretError> {
        let store = Arc::new(SlowWriteStore::new());
        let key = SecretKey::new("p1", SecretKind::DbPassword)?;
        let cache = Arc::new(SecretCache::new(
            Arc::clone(&store) as Arc<dyn SecretStore>,
            true,
        ));
        let writer = {
            let (cache, key) = (Arc::clone(&cache), key.clone());
            std::thread::spawn(move || cache.set(&key, &Secret::new("new".into())))
        };
        store.write_entered.wait();
        forget(&cache);
        store.write_release.wait();
        writer
            .join()
            .map_err(|_| SecretError::Keychain("the writer panicked".into()))??;
        assert_eq!(
            text(store.inner.get(&key)?),
            Some("new".into()),
            "the store got the value"
        );
        Ok(cache.len())
    }

    #[test]
    fn a_forget_during_a_slow_save_leaves_nothing_cached() -> Result<(), SecretError> {
        for forget in [
            (|cache: &SecretCache| cache.forget_all()) as fn(&SecretCache),
            |cache| {
                cache.forget_profile("p1");
            },
            |cache| cache.set_remember(false),
        ] {
            assert_eq!(forget_during_save(forget)?, 0);
        }
        Ok(())
    }

    #[test]
    fn a_read_during_a_slow_save_cannot_keep_the_old_value() -> Result<(), SecretError> {
        let kept = read_during_write(|cache, key| cache.set(key, &Secret::new("new".into())))?;
        assert_eq!(kept, Some("new".into()));
        Ok(())
    }

    #[test]
    fn a_read_during_a_slow_delete_cannot_keep_the_value() -> Result<(), SecretError> {
        let kept = read_during_write(|cache, key| cache.delete(key))?;
        assert_eq!(kept, None);
        Ok(())
    }

    #[test]
    fn a_poisoned_cache_is_still_wiped() -> Result<(), SecretError> {
        let (_, cache) = counted();
        let cache = Arc::new(cache);
        let key = SecretKey::new("p1", SecretKind::DbPassword)?;
        cache.set(&key, &Secret::new("pw".into()))?;
        let poisoner = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                let _guard = cache.state.lock();
                panic!("poisoning the cache lock on purpose");
            })
        };
        assert!(poisoner.join().is_err());
        assert!(cache.state.lock().is_err(), "the lock is poisoned");
        assert_eq!(cache.len(), 1);
        assert!(cache.has_profile("p1"));
        assert!(cache.forget_profile("p1"));
        assert!(cache.is_empty());
        cache.set(&key, &Secret::new("pw".into()))?;
        cache.forget_all();
        assert!(cache.is_empty());
        assert!(cache.get(&key)?.is_some(), "reads still work");
        Ok(())
    }

    #[test]
    fn has_profile_follows_the_cached_values() -> Result<(), SecretError> {
        let (_, cache) = counted();
        let key = SecretKey::new("p1", SecretKind::SshPassword)?;
        assert!(!cache.has_profile("p1"));
        cache.set(&key, &Secret::new("pw".into()))?;
        assert!(cache.has_profile("p1"));
        assert!(!cache.has_profile("p"));
        cache.delete(&key)?;
        assert!(!cache.has_profile("p1"));
        Ok(())
    }

    #[test]
    fn the_cache_never_prints_its_values() -> Result<(), SecretError> {
        let (_, cache) = counted();
        let key = SecretKey::new("p1", SecretKind::DbPassword)?;
        cache.set(&key, &Secret::new("hunter2".into()))?;
        let printed = format!("{cache:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("cached: 1"), "{printed}");
        Ok(())
    }

    #[test]
    fn account_names_follow_the_design() -> Result<(), SecretError> {
        let key = SecretKey::new("5f0c-1a2b", SecretKind::SshKeyPassphrase)?;
        assert_eq!(key.account(), "5f0c-1a2b/ssh_key_passphrase");
        Ok(())
    }

    #[test]
    fn profile_ids_cannot_escape_the_account_format() {
        for bad in ["", "a/b", "a b", "../x", "ü"] {
            assert!(
                SecretKey::new(bad, SecretKind::DbPassword).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn secrets_never_print() {
        let secret = Secret::new("hunter2".into());
        assert_eq!(format!("{secret:?}"), "Secret([redacted])");
    }

    #[test]
    fn memory_store_round_trip() -> Result<(), SecretError> {
        let store = MemoryStore::default();
        let key = SecretKey::new("p1", SecretKind::DbPassword)?;
        assert!(store.get(&key)?.is_none());
        store.set(&key, &Secret::new("s3cret".into()))?;
        assert_eq!(
            store.get(&key)?.map(|s| s.expose().to_string()),
            Some("s3cret".into())
        );
        store.delete(&key)?;
        store.delete(&key)?;
        assert!(store.get(&key)?.is_none());
        Ok(())
    }

    /// Writes to the real OS keychain. Run manually with
    /// `cargo test -p varve-secrets -- --ignored`.
    #[test]
    #[ignore = "touches the real OS keychain"]
    fn keychain_round_trip() -> Result<(), SecretError> {
        let store = KeychainStore;
        let key = SecretKey::new("varve-selftest", SecretKind::DbPassword)?;
        store.set(&key, &Secret::new("selftest".into()))?;
        assert_eq!(
            store.get(&key)?.map(|s| s.expose().to_string()),
            Some("selftest".into())
        );
        store.delete(&key)?;
        assert!(store.get(&key)?.is_none());
        Ok(())
    }
}
