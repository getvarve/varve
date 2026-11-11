//! The OS trust store, shared by every driver.
//!
//! Loading it walks the keychain on macOS (and the certificate directories
//! elsewhere), which can take hundreds of milliseconds: it runs on the
//! blocking pool, never on an async worker thread, and its result is kept,
//! so it does not run for every connection, reconnect and cancel request.
//!
//! Only a load that found certificates is kept: a load that failed or found
//! nothing is handed to its caller and tried again by the next one. After a
//! TLS failure ([`refresh_native_roots_after`]) the next caller loads the
//! store again, so a CA the user has just added to the OS store is used by
//! the next connect, without a restart.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Mutex;

use crate::{DriverError, ErrorKind};

/// The certificates of the OS trust store.
#[derive(Debug, Default)]
pub struct NativeRoots {
    /// The certificates, DER encoded.
    pub certs: Vec<Vec<u8>>,
    /// Why parts of the store could not be loaded (or why loading failed).
    pub errors: Vec<String>,
    /// [`NativeRoots::unreported_errors`] has handed the errors out.
    reported: AtomicBool,
}

impl NativeRoots {
    /// [`NativeRoots::errors`] for the first caller, nothing for every
    /// later one: for logging the problems of a load once, not for every
    /// connection, reconnect and side connection that uses it.
    #[must_use]
    pub fn unreported_errors(&self) -> &[String] {
        if self.reported.swap(true, Ordering::AcqRel) {
            &[]
        } else {
            &self.errors
        }
    }
}

/// A loaded trust store, kept until it is marked stale.
struct Cache {
    /// The last load that found certificates. The lock is held while
    /// loading, so callers that arrive together share one load.
    kept: Mutex<Option<Arc<NativeRoots>>>,
    /// The kept store may be out of date: the next caller loads again.
    stale: AtomicBool,
}

impl Cache {
    const fn new() -> Self {
        Self {
            kept: Mutex::const_new(None),
            stale: AtomicBool::new(false),
        }
    }

    fn mark_stale(&self) {
        self.stale.store(true, Ordering::Release);
    }

    /// The kept store, or what `load` gives (run on the blocking pool). A
    /// load without certificates is returned but not kept.
    async fn get(&self, load: impl FnOnce() -> NativeRoots + Send + 'static) -> Arc<NativeRoots> {
        let mut kept = self.kept.lock().await;
        if self.stale.swap(false, Ordering::AcqRel) {
            *kept = None;
        }
        if let Some(roots) = kept.as_ref() {
            return Arc::clone(roots);
        }
        let loaded = tokio::task::spawn_blocking(load)
            .await
            .unwrap_or_else(|error| NativeRoots {
                certs: Vec::new(),
                errors: vec![format!("loading the OS trust store failed: {error}")],
                reported: AtomicBool::new(false),
            });
        let loaded = Arc::new(loaded);
        if !loaded.certs.is_empty() {
            *kept = Some(Arc::clone(&loaded));
        }
        loaded
    }
}

static ROOTS: Cache = Cache::new();

/// The OS trust store: loaded on the blocking pool by the first caller and
/// shared from then on. A load that failed or found no certificate is not
/// kept, so the next caller tries again.
pub async fn native_roots() -> Arc<NativeRoots> {
    ROOTS
        .get(|| {
            let result = rustls_native_certs::load_native_certs();
            NativeRoots {
                certs: result
                    .certs
                    .into_iter()
                    .map(|cert| cert.as_ref().to_vec())
                    .collect(),
                errors: result.errors.iter().map(ToString::to_string).collect(),
                reported: AtomicBool::new(false),
            }
        })
        .await
}

/// Call with the error of a failed connect. After a TLS failure (a
/// certificate that no CA in the store signed, for example) the next
/// [`native_roots`] call loads the OS trust store again, so a CA added to
/// it since is found.
pub fn refresh_native_roots_after(error: &DriverError) {
    if error.kind == ErrorKind::Tls {
        ROOTS.mark_stale();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A loader that counts its runs and returns `certs` certificates.
    fn loader(
        runs: &Arc<AtomicUsize>,
        certs: usize,
    ) -> impl FnOnce() -> NativeRoots + Send + 'static {
        let runs = Arc::clone(runs);
        move || {
            let run = runs.fetch_add(1, Ordering::SeqCst);
            NativeRoots {
                certs: vec![vec![u8::try_from(run).unwrap_or(u8::MAX)]; certs],
                ..NativeRoots::default()
            }
        }
    }

    #[tokio::test]
    async fn loads_once_and_shares_the_result() {
        let cache = Cache::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let first = cache.get(loader(&runs, 2)).await;
        let second = cache.get(loader(&runs, 2)).await;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_empty_or_failed_load_is_not_kept() {
        let cache = Cache::new();
        let runs = Arc::new(AtomicUsize::new(0));
        // Nothing found: handed out, loaded again by the next caller.
        let empty = cache.get(loader(&runs, 0)).await;
        assert!(empty.certs.is_empty());
        // The load itself fails (its task panics).
        let failed = cache
            .get(|| -> NativeRoots { panic!("keychain not available") })
            .await;
        assert!(failed.certs.is_empty());
        assert!(failed.errors[0].contains("loading the OS trust store failed"));
        // The store is there now: this load is kept.
        let loaded = cache.get(loader(&runs, 1)).await;
        assert_eq!(loaded.certs.len(), 1);
        let again = cache.get(loader(&runs, 1)).await;
        assert!(Arc::ptr_eq(&loaded, &again));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_stale_store_is_loaded_again() {
        let cache = Cache::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let first = cache.get(loader(&runs, 1)).await;
        cache.mark_stale();
        // A CA was added in between: the next caller sees it.
        let second = cache.get(loader(&runs, 2)).await;
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second.certs.len(), 2);
        // And that load is kept in turn.
        let third = cache.get(loader(&runs, 2)).await;
        assert!(Arc::ptr_eq(&second, &third));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        // A reload that finds nothing does not bring the old store back.
        cache.mark_stale();
        assert!(cache.get(loader(&runs, 0)).await.certs.is_empty());
        assert_eq!(cache.get(loader(&runs, 3)).await.certs.len(), 3);
    }

    #[test]
    fn errors_are_reported_once() {
        let roots = NativeRoots {
            errors: vec!["a certificate could not be read".into()],
            ..NativeRoots::default()
        };
        assert_eq!(roots.unreported_errors().len(), 1);
        assert!(roots.unreported_errors().is_empty());
        // The errors themselves stay, for messages.
        assert_eq!(roots.errors.len(), 1);
    }

    #[tokio::test]
    async fn only_tls_failures_ask_for_a_reload() {
        // The process-wide store: only its flag is looked at here.
        ROOTS.stale.store(false, Ordering::SeqCst);
        refresh_native_roots_after(&DriverError::new(ErrorKind::Auth, "wrong password"));
        refresh_native_roots_after(&DriverError::new(ErrorKind::Connect, "refused"));
        assert!(!ROOTS.stale.load(Ordering::SeqCst));
        refresh_native_roots_after(&DriverError::new(ErrorKind::Tls, "unknown issuer"));
        assert!(ROOTS.stale.swap(false, Ordering::SeqCst));
    }
}
