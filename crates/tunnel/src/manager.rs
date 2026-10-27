//! Shared, reference-counted tunnels (design doc 3.3).
//!
//! All sessions of a connection profile on this device share one tunnel.
//! Each user holds a [`TunnelLease`]; when the last lease is dropped the
//! tunnel stays open for a grace period so a quick reconnect reuses it.
//! While no lease is held the local port resets new connections instead of
//! forwarding them (see [`Tunnel`]'s documentation).

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use crate::error::TunnelError;
use crate::tunnel::{Tunnel, TunnelParams, TunnelStatus};

/// The default key: a connection profile on a device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TunnelKey {
    /// Connection profile id.
    pub profile_id: String,
    /// Device id (shared profiles can carry per-device SSH settings).
    pub device: String,
}

impl TunnelKey {
    /// Creates a key.
    #[must_use]
    pub fn new(profile_id: impl Into<String>, device: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
            device: device.into(),
        }
    }
}

struct Entry {
    tunnel: Tunnel,
    params: TunnelParams,
    leases: AtomicUsize,
    /// Bumped every time the lease count drops to zero, so a grace timer
    /// only closes the tunnel if nothing happened since it started.
    idle_epoch: AtomicU64,
}

struct Slot {
    current: Option<Arc<Entry>>,
    /// Serializes opening per key so concurrent acquires open one tunnel.
    open_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            current: None,
            open_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

struct Inner<K> {
    grace: Duration,
    slots: Mutex<HashMap<K, Slot>>,
}

impl<K> Inner<K> {
    fn slots(&self) -> MutexGuard<'_, HashMap<K, Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Shares tunnels between users of the same key. Cloning is cheap and
/// clones share the same tunnels.
pub struct TunnelManager<K = TunnelKey> {
    inner: Arc<Inner<K>>,
}

impl<K> Clone for TunnelManager<K> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<K> fmt::Debug for TunnelManager<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelManager")
            .field("grace", &self.inner.grace)
            .field("keys", &self.inner.slots().len())
            .finish()
    }
}

impl<K> Default for TunnelManager<K>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K> TunnelManager<K>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    /// Default time an unused tunnel stays open.
    pub const DEFAULT_GRACE: Duration = Duration::from_secs(30);

    /// A manager with the default grace period.
    #[must_use]
    pub fn new() -> Self {
        Self::with_grace(Self::DEFAULT_GRACE)
    }

    /// A manager that keeps unused tunnels open for `grace`.
    /// `Duration::ZERO` closes them as soon as the last lease is dropped.
    #[must_use]
    pub fn with_grace(grace: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                grace,
                slots: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Returns a lease on the live tunnel for `key`, opening one when there
    /// is none. A broken tunnel is replaced. When `params` describe a
    /// different tunnel than the open one (the profile was edited: another
    /// SSH server, user, authentication method or key file, known hosts
    /// file or policy, target, keepalive or timeout), a new tunnel is opened
    /// and the old one closes when its last lease is dropped. Only a changed
    /// password or passphrase reuses the open tunnel.
    ///
    /// Concurrent calls for the same key open at most one tunnel.
    ///
    /// # Errors
    /// Any error of [`Tunnel::open`].
    pub async fn acquire(
        &self,
        key: K,
        params: TunnelParams,
    ) -> Result<TunnelLease<K>, TunnelError> {
        let open_lock = self
            .inner
            .slots()
            .entry(key.clone())
            .or_default()
            .open_lock
            .clone();
        let _opening = open_lock.lock().await;

        if let Some(lease) = self.reuse(&key, &params) {
            return Ok(lease);
        }

        let tunnel = match Tunnel::open(params.clone()).await {
            Ok(tunnel) => tunnel,
            Err(e) => {
                self.remove_slot_if_unused(&key);
                return Err(e);
            }
        };
        let entry = Arc::new(Entry {
            tunnel,
            params,
            leases: AtomicUsize::new(1),
            idle_epoch: AtomicU64::new(0),
        });
        self.inner.slots().entry(key.clone()).or_default().current = Some(entry.clone());
        Ok(TunnelLease {
            key,
            entry,
            manager: Arc::downgrade(&self.inner),
            opened: true,
        })
    }

    /// Reuses the current tunnel or detaches it when it cannot be reused.
    fn reuse(&self, key: &K, params: &TunnelParams) -> Option<TunnelLease<K>> {
        let detached = {
            let mut slots = self.inner.slots();
            let slot = slots.get_mut(key)?;
            let entry = slot.current.as_ref()?;
            if entry.tunnel.is_alive() && entry.params.same_endpoint(params) {
                entry.leases.fetch_add(1, Ordering::SeqCst);
                // Idle during the grace period: forward again.
                entry.tunnel.set_accepting(true);
                return Some(TunnelLease {
                    key: key.clone(),
                    entry: entry.clone(),
                    manager: Arc::downgrade(&self.inner),
                    opened: false,
                });
            }
            let entry = slot.current.take()?;
            (entry.leases.load(Ordering::SeqCst) == 0).then_some(entry)
        };
        // An unused detached tunnel closes now; one with leases closes
        // when its last lease is dropped.
        if let Some(entry) = detached {
            tokio::spawn(async move { entry.tunnel.close().await });
        }
        None
    }

    /// Closes the tunnel of `key` now, for example after the profile was
    /// deleted. Holders of leases see it as closed.
    pub async fn close(&self, key: &K) {
        let entry = self
            .inner
            .slots()
            .get_mut(key)
            .and_then(|s| s.current.take());
        if let Some(entry) = entry {
            entry.tunnel.close().await;
        }
        self.remove_slot_if_unused(key);
    }

    /// Closes every tunnel, for example when the app quits.
    pub async fn close_all(&self) {
        let entries: Vec<Arc<Entry>> = self
            .inner
            .slots()
            .values_mut()
            .filter_map(|s| s.current.take())
            .collect();
        for entry in entries {
            entry.tunnel.close().await;
        }
    }

    /// Number of leases on the current tunnel of `key` (0 when none).
    #[must_use]
    pub fn lease_count(&self, key: &K) -> usize {
        self.inner
            .slots()
            .get(key)
            .and_then(|s| s.current.as_ref())
            .map_or(0, |e| e.leases.load(Ordering::SeqCst))
    }

    /// The status of the current tunnel of `key`, if there is one.
    #[must_use]
    pub fn status(&self, key: &K) -> Option<TunnelStatus> {
        self.inner
            .slots()
            .get(key)
            .and_then(|s| s.current.as_ref())
            .map(|e| e.tunnel.status())
    }

    fn remove_slot_if_unused(&self, key: &K) {
        let mut slots = self.inner.slots();
        // Strong count 1: no acquire holds or waits on the open lock.
        if slots
            .get(key)
            .is_some_and(|s| s.current.is_none() && Arc::strong_count(&s.open_lock) == 1)
        {
            slots.remove(key);
        }
    }
}

/// A share of a tunnel. The tunnel stays open while any lease exists, and
/// for the manager's grace period after the last one is dropped.
pub struct TunnelLease<K = TunnelKey>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    key: K,
    entry: Arc<Entry>,
    manager: Weak<Inner<K>>,
    /// This lease's `acquire` opened the tunnel (it was not reused).
    opened: bool,
}

impl<K> fmt::Debug for TunnelLease<K>
where
    K: Eq + Hash + Clone + Send + Sync + fmt::Debug + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelLease")
            .field("key", &self.key)
            .field("tunnel", &self.entry.tunnel)
            .finish()
    }
}

impl<K> TunnelLease<K>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    /// The key this lease was acquired for.
    #[must_use]
    pub fn key(&self) -> &K {
        &self.key
    }

    /// The shared tunnel.
    #[must_use]
    pub fn tunnel(&self) -> &Tunnel {
        &self.entry.tunnel
    }

    /// The local address to connect the database driver to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.entry.tunnel.local_addr()
    }

    /// Whether the tunnel accepts and forwards connections.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.entry.tunnel.is_alive()
    }

    /// True when the `acquire` that returned this lease opened the tunnel,
    /// false when it shared one that was already open (for activity
    /// logging: a shared tunnel is not opened again).
    #[must_use]
    pub fn opened(&self) -> bool {
        self.opened
    }
}

impl<K> Drop for TunnelLease<K>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    fn drop(&mut self) {
        let entry = self.entry.clone();
        let Some(inner) = self.manager.upgrade() else {
            // The manager is gone, so nothing can reuse the tunnel; it
            // closes when the last lease drops its reference.
            if entry.leases.fetch_sub(1, Ordering::SeqCst) == 1 {
                entry.tunnel.set_accepting(false);
            }
            return;
        };
        // The lease count changes only under the slots lock, so this cannot
        // race with `reuse` handing out a new lease.
        let mut slots = inner.slots();
        if entry.leases.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        // No lease is held: local connections are no longer forwarded.
        entry.tunnel.set_accepting(false);
        let epoch = entry.idle_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let is_current = slots
            .get(&self.key)
            .and_then(|s| s.current.as_ref())
            .is_some_and(|c| Arc::ptr_eq(c, &entry));
        let keep_for = if is_current && entry.tunnel.is_alive() {
            inner.grace
        } else {
            Duration::ZERO
        };
        let remove_now = keep_for.is_zero() && is_current;
        if remove_now && let Some(slot) = slots.get_mut(&self.key) {
            slot.current = None;
        }
        drop(slots);
        if remove_now {
            TunnelManager {
                inner: inner.clone(),
            }
            .remove_slot_if_unused(&self.key);
        }

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // No runtime: dropping the last reference closes the tunnel.
            return;
        };
        let key = self.key.clone();
        let weak = Arc::downgrade(&inner);
        drop(inner);
        runtime.spawn(async move {
            if !keep_for.is_zero() {
                tokio::time::sleep(keep_for).await;
                let Some(inner) = weak.upgrade() else {
                    entry.tunnel.close().await;
                    return;
                };
                {
                    let mut slots = inner.slots();
                    let still_idle = entry.leases.load(Ordering::SeqCst) == 0
                        && entry.idle_epoch.load(Ordering::SeqCst) == epoch;
                    if !still_idle {
                        return;
                    }
                    if let Some(slot) = slots.get_mut(&key)
                        && slot
                            .current
                            .as_ref()
                            .is_some_and(|c| Arc::ptr_eq(c, &entry))
                    {
                        slot.current = None;
                    }
                }
                TunnelManager {
                    inner: inner.clone(),
                }
                .remove_slot_if_unused(&key);
            }
            entry.tunnel.close().await;
        });
    }
}
