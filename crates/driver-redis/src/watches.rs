//! The keys console `WATCH`es hold, over every session of this process.
//!
//! Some module reads count as a change of a key for every client's `WATCH`
//! (`TS.INFO` opens the key for writing). Each session of the app is its
//! own connection (a console tab, the key browser), so a guard on the
//! reading session alone misses the common case: a `WATCH` in a console
//! tab and the key opened in the key browser. Sessions record here which
//! keys their console `WATCH` holds (by server, database and key) until it
//! ends (`UNWATCH`, `EXEC`, `DISCARD`, a replaced connection, the session
//! closing); readers ask before such a read.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// A watched key: server identity, database, key.
type Watched = (String, u16, Vec<u8>);

static NEXT: AtomicU64 = AtomicU64::new(1);
static WATCHES: Mutex<Option<HashMap<u64, Vec<Watched>>>> = Mutex::new(None);

fn lock() -> MutexGuard<'static, Option<HashMap<u64, Vec<Watched>>>> {
    WATCHES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A token for a new session.
pub(crate) fn token() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Session `token` now also watches `keys` of database `db` on `server`.
pub(crate) fn add(token: u64, server: &str, db: u16, keys: &[Vec<u8>]) {
    let mut map = lock();
    let entries = map
        .get_or_insert_with(HashMap::new)
        .entry(token)
        .or_default();
    for key in keys {
        entries.push((server.to_string(), db, key.clone()));
    }
}

/// Session `token` no longer watches anything.
pub(crate) fn clear(token: u64) {
    if let Some(map) = lock().as_mut() {
        map.remove(&token);
    }
}

/// True when any session watches `key` of database `db` on `server`.
pub(crate) fn is_watched(server: &str, db: u16, key: &[u8]) -> bool {
    lock().as_ref().is_some_and(|map| {
        map.values()
            .flatten()
            .any(|(s, d, k)| s == server && *d == db && k == key)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watches_are_per_server_database_and_key() {
        let a = token();
        let b = token();
        add(a, "h:1", 0, &[b"k".to_vec()]);
        add(b, "h:2", 0, &[b"k".to_vec()]);
        assert!(is_watched("h:1", 0, b"k"));
        assert!(!is_watched("h:1", 1, b"k"));
        assert!(!is_watched("h:1", 0, b"other"));
        clear(a);
        assert!(!is_watched("h:1", 0, b"k"));
        assert!(is_watched("h:2", 0, b"k"));
        clear(b);
    }
}
