//! Driver-held state for hash and set pages that end inside a scan step.
//!
//! `HSCAN`/`SSCAN` only guarantee completeness across a whole scan: running
//! the same cursor twice can return a different step (incremental rehash,
//! deletes, growth). So a step is never re-read to continue it. When a page
//! ends inside a step, the rest of that step is kept here, in the session,
//! under an opaque page cursor `p:<id>`; the next page serves it and then
//! continues with the real scan cursor after the step.
//!
//! The session keeps at most [`MAX_PAGE_STATES`] states and
//! [`MAX_PAGE_STATE_BYTES`] of their data, evicting the least recently used
//! first; a state unused for [`PAGE_STATE_IDLE`] expires; all are dropped
//! when the session reconnects after a lost or closed connection. Each
//! cursor works once: serving a page removes its state (and stores the
//! rest under a new id). A failed page puts its state back, so the same
//! cursor can be retried.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::time::{Duration, Instant};

use varve_driver::{DriverError, ErrorKind};

/// Most page states one session keeps.
pub const MAX_PAGE_STATES: usize = 16;

/// Most bytes of names and values all page states of one session keep
/// (64 MiB).
pub const MAX_PAGE_STATE_BYTES: u64 = 64 * 1024 * 1024;

/// A page state unused this long expires (10 minutes).
pub const PAGE_STATE_IDLE: Duration = Duration::from_secs(600);

/// Prefix of a page cursor that refers to a page state.
pub(crate) const STATE_CURSOR_PREFIX: &str = "p:";

/// Which kind of value a state belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanKind {
    /// A hash (`HSCAN`).
    Hash,
    /// A set (`SSCAN`).
    Set,
}

/// One entry of a step that is still to be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pending {
    /// A hash field and its value, already read.
    Pair(Vec<u8>, Vec<u8>),
    /// A hash field whose value is read by name when it is shown (the
    /// field is skipped when it was deleted meanwhile).
    Field(Vec<u8>),
    /// A hash field whose name is too large to read: its sizes.
    OversizedField {
        /// Bytes of the field name.
        field_bytes: u64,
        /// Bytes of the value, when known.
        value_bytes: Option<u64>,
    },
    /// A set member, already read.
    Member(Vec<u8>),
    /// A set member too large to keep: its size when the step was read.
    OversizedMember(u64),
}

impl Pending {
    fn bytes(&self) -> u64 {
        match self {
            Pending::Pair(field, value) => (field.len() + value.len()) as u64,
            Pending::Field(name) | Pending::Member(name) => name.len() as u64,
            Pending::OversizedField { .. } | Pending::OversizedMember(_) => 16,
        }
    }
}

/// The rest of a scan step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanState {
    /// Hash or set.
    pub(crate) kind: ScanKind,
    /// Logical database of the key.
    pub(crate) db: u16,
    /// The key.
    pub(crate) key: Vec<u8>,
    /// Entries still to show, in order.
    pub(crate) pending: Vec<Pending>,
    /// The real scan cursor after the step; `0` when the step was the last.
    pub(crate) next: u64,
}

impl ScanState {
    fn bytes(&self) -> u64 {
        self.key.len() as u64 + self.pending.iter().map(Pending::bytes).sum::<u64>()
    }
}

/// The page states of one session.
#[derive(Debug)]
pub(crate) struct PageStates {
    states: HashMap<String, (ScanState, Instant)>,
    counter: u64,
    random: RandomState,
}

impl Default for PageStates {
    fn default() -> Self {
        Self {
            states: HashMap::new(),
            counter: 0,
            random: RandomState::new(),
        }
    }
}

impl PageStates {
    /// Stores a state and returns its page cursor (`p:<id>`).
    pub(crate) fn insert(&mut self, state: ScanState) -> String {
        self.counter = self.counter.wrapping_add(1);
        let mut hasher = self.random.build_hasher();
        hasher.write_u64(self.counter);
        let id = format!("{:x}{:016x}", self.counter, hasher.finish());
        self.put(id.clone(), state, Instant::now());
        format!("{STATE_CURSOR_PREFIX}{id}")
    }

    /// Puts a state back under the id it had (a failed page).
    pub(crate) fn restore(&mut self, id: String, state: ScanState) {
        self.put(id, state, Instant::now());
    }

    fn put(&mut self, id: String, state: ScanState, now: Instant) {
        self.expire(now);
        self.states.insert(id, (state, now));
        self.evict();
    }

    /// Removes and returns the state of a page cursor id.
    ///
    /// # Errors
    ///
    /// [`expired`] for an unknown, evicted or expired id.
    pub(crate) fn take(&mut self, id: &str) -> Result<ScanState, DriverError> {
        self.take_at(id, Instant::now())
    }

    fn take_at(&mut self, id: &str, now: Instant) -> Result<ScanState, DriverError> {
        self.expire(now);
        self.states
            .remove(id)
            .map(|(state, _)| state)
            .ok_or_else(expired)
    }

    /// Drops every state.
    pub(crate) fn clear(&mut self) {
        self.states.clear();
    }

    fn expire(&mut self, now: Instant) {
        self.states
            .retain(|_, (_, used)| now.saturating_duration_since(*used) < PAGE_STATE_IDLE);
    }

    /// Evicts the least recently used states while over a limit, never the
    /// newest one.
    fn evict(&mut self) {
        loop {
            let bytes: u64 = self.states.values().map(|(s, _)| s.bytes()).sum();
            if self.states.len() <= MAX_PAGE_STATES && bytes <= MAX_PAGE_STATE_BYTES {
                return;
            }
            if self.states.len() <= 1 {
                return;
            }
            let oldest = self
                .states
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => {
                    self.states.remove(&id);
                }
                None => return,
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.states.len()
    }
}

/// The error for a page cursor whose state is gone.
pub(crate) fn expired() -> DriverError {
    DriverError::new(
        ErrorKind::Query,
        format!(
            "This page cursor is no longer valid: Varve keeps at most {MAX_PAGE_STATES} open \
             scans per session, drops them after {} minutes or a reconnect, and each cursor \
             works once. Start the scan again from the beginning.",
            PAGE_STATE_IDLE.as_secs() / 60
        ),
    )
    .with_code("CURSOR_EXPIRED")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(bytes: usize) -> ScanState {
        ScanState {
            kind: ScanKind::Set,
            db: 0,
            key: b"k".to_vec(),
            pending: vec![Pending::Member(vec![0; bytes])],
            next: 7,
        }
    }

    fn id(cursor: &str) -> &str {
        cursor.strip_prefix(STATE_CURSOR_PREFIX).unwrap_or(cursor)
    }

    #[test]
    fn a_cursor_works_once() -> Result<(), DriverError> {
        let mut states = PageStates::default();
        let cursor = states.insert(state(3));
        assert!(cursor.starts_with("p:"));
        assert_eq!(states.take(id(&cursor))?.next, 7);
        let error = states.take(id(&cursor)).err();
        assert_eq!(
            error.and_then(|e| e.code).as_deref(),
            Some("CURSOR_EXPIRED")
        );
        assert!(states.take("nonsense").is_err());
        Ok(())
    }

    #[test]
    fn restored_states_can_be_retried() -> Result<(), DriverError> {
        let mut states = PageStates::default();
        let cursor = states.insert(state(3));
        let taken = states.take(id(&cursor))?;
        states.restore(id(&cursor).to_string(), taken);
        assert_eq!(states.take(id(&cursor))?.next, 7);
        Ok(())
    }

    #[test]
    fn the_least_recently_used_state_is_evicted() {
        let mut states = PageStates::default();
        let first = states.insert(state(1));
        let mut last = String::new();
        for _ in 0..MAX_PAGE_STATES {
            last = states.insert(state(1));
        }
        assert_eq!(states.len(), MAX_PAGE_STATES);
        assert!(states.take(id(&first)).is_err(), "oldest was kept");
        assert!(states.take(id(&last)).is_ok());
    }

    #[test]
    fn the_byte_limit_evicts_too() {
        let mut states = PageStates::default();
        let big = (MAX_PAGE_STATE_BYTES / 2) as usize;
        let a = states.insert(state(big));
        let b = states.insert(state(big));
        assert!(states.take(id(&a)).is_err(), "over the byte limit");
        assert!(states.take(id(&b)).is_ok());
    }

    #[test]
    fn idle_states_expire() {
        let mut states = PageStates::default();
        let cursor = states.insert(state(1));
        let later = Instant::now() + PAGE_STATE_IDLE + Duration::from_secs(1);
        assert!(states.take_at(id(&cursor), later).is_err());
    }
}
