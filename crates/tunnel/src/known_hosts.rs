//! Host key checking against an OpenSSH-format `known_hosts` file that the
//! app owns.
//!
//! Supported syntax (see `sshd(8)`, section SSH_KNOWN_HOSTS FILE FORMAT):
//!
//! - plain host patterns, comma separated, with `*` and `?` wildcards and
//!   `!` negation, matched case-insensitively;
//! - `[host]:port` for servers that do not listen on port 22;
//! - hashed host names (`|1|<salt>|<hmac-sha1>`, as written by
//!   `HashKnownHosts yes` or `ssh-keygen -H`);
//! - the `@revoked` marker: a matching revoked key is always refused;
//! - comments (`#`) and blank lines.
//!
//! Lines with the `@cert-authority` marker are **ignored**: tunnels do not
//! ask servers for host certificates, so a certificate authority can never
//! vouch for a host here. Hosts are trusted only through plain key entries.
//! Lines that cannot be parsed (unknown markers, unsupported key types such
//! as DSA, broken base64) are skipped, like OpenSSH does.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{Algorithm, PublicKey};
use sha1::Sha1;

use crate::error::{HostKeyChange, TunnelError};
use crate::keys::fingerprint;

/// Known hosts files (by path as given) that a thread of this process is
/// writing. Together with [`PATH_RELEASED`] this is a per-path lock that
/// serializes writers within the process, so concurrent tunnels and UI
/// actions never lose an entry (a `trust` rewrites the file, an `add`
/// appends to it), while writers of different files never wait for each
/// other. It is only taken by synchronous code: the tunnel runs its checks
/// on the blocking thread pool, never on a runtime worker.
///
/// Writers also hold an exclusive file lock on a sidecar file
/// (`<known hosts file>.lock`) for the whole read-modify-write, which
/// serializes them with other processes (another app instance). Threads
/// of this process queue on the in-process lock first, so only one of them
/// at a time polls the file lock.
///
/// Read-only checks take neither lock: the file is only ever replaced by an
/// atomic rename, and a recording check looks again under both locks.
static LOCKED_PATHS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Signalled whenever a path is removed from [`LOCKED_PATHS`].
static PATH_RELEASED: Condvar = Condvar::new();

/// The longest a writer waits for the in-process and the file lock before
/// it gives up with an error, so that a process that holds the lock file
/// forever cannot hang connects.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// Longest pause between two attempts to take the file lock.
const LOCK_RETRY_MAX: Duration = Duration::from_millis(100);

/// How many unique temporary names to try before giving up.
const TEMP_ATTEMPTS: usize = 16;

/// Temporary files older than this were left behind by a crash (a rewrite
/// takes milliseconds) and are removed by the next writer.
const STALE_TEMP_AGE: Duration = Duration::from_secs(5 * 60);

/// What to do with a host key that is not in the file yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostKeyPolicy {
    /// Refuse unknown and changed keys. The UI asks the user to confirm the
    /// fingerprint and calls [`KnownHosts::trust`].
    #[default]
    Strict,
    /// Record unknown keys and refuse changed keys (OpenSSH
    /// `StrictHostKeyChecking accept-new`).
    AcceptNew,
}

/// The known hosts file used to check SSH server keys.
///
/// The methods read and write the file synchronously, and the writing ones
/// (and [`KnownHosts::verify`] when it records a key) wait up to 10 seconds
/// for the write locks. From async code, call them inside
/// `tokio::task::spawn_blocking`.
///
/// # Files next to the known hosts file
///
/// When the path is a symbolic link, writes go to the file it points to (the
/// link stays), and the files below live next to that real file.
///
/// - `<name>.lock` (for example `known_hosts.lock`): an empty file that
///   writers lock to serialize with other processes. It is created by the
///   first write and kept permanently; deleting it while no Varve process
///   writes is harmless.
/// - `.<name>.<random>.varve-tmp`: the new contents during a rewrite, renamed
///   over the file when complete. One left behind by a crash is removed by
///   the next writer once it is older than five minutes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHosts {
    /// Path of the OpenSSH-format file. It is created when a key is recorded.
    pub path: PathBuf,
    /// Policy for keys that are not in the file.
    pub policy: HostKeyPolicy,
    /// Write new entries with hashed host names instead of plain ones.
    pub hash_new_entries: bool,
}

/// Result of looking a presented key up in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// The key is recorded for this host.
    Known,
    /// Nothing is recorded for this host.
    Unknown,
    /// The host is recorded with a different key.
    Changed {
        /// The recorded key that does not match.
        expected: PublicKey,
        /// Its line (1-based).
        line: usize,
    },
    /// The key is marked `@revoked`.
    Revoked {
        /// The line of the revocation (1-based).
        line: usize,
    },
}

impl KnownHosts {
    /// A known hosts file with plain (unhashed) new entries.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, policy: HostKeyPolicy) -> Self {
        Self {
            path: path.into(),
            policy,
            hash_new_entries: false,
        }
    }

    /// Reads and parses the file. A missing file is empty.
    ///
    /// # Errors
    /// Returns [`TunnelError::Io`] when the file exists but cannot be read.
    pub fn load(&self) -> Result<KnownHostsFile, TunnelError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(KnownHostsFile::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(KnownHostsFile::default()),
            Err(e) => Err(TunnelError::io(
                format!("cannot read known hosts file {}", self.path.display()),
                e,
            )),
        }
    }

    /// Looks up a presented key.
    ///
    /// # Errors
    /// Returns [`TunnelError::Io`] when the file cannot be read.
    pub fn status(
        &self,
        host: &str,
        port: u16,
        key: &PublicKey,
    ) -> Result<HostKeyStatus, TunnelError> {
        Ok(self.load()?.status(host, port, key))
    }

    /// Checks a presented key under the policy, recording it when the
    /// policy is [`HostKeyPolicy::AcceptNew`] and the host is unknown.
    ///
    /// # Errors
    /// [`TunnelError::HostKeyUnknown`], [`TunnelError::HostKeyChanged`],
    /// [`TunnelError::HostKeyRevoked`], or [`TunnelError::Io`] when the file
    /// cannot be read or written.
    pub fn verify(&self, host: &str, port: u16, key: &PublicKey) -> Result<(), TunnelError> {
        self.verify_unless_abandoned(host, port, key, &AtomicBool::new(false))
    }

    /// [`KnownHosts::verify`] that does not record a new key once
    /// `abandoned` is set (the connection that presented it was given up).
    fn verify_unless_abandoned(
        &self,
        host: &str,
        port: u16,
        key: &PublicKey,
        abandoned: &AtomicBool,
    ) -> Result<(), TunnelError> {
        let key_type = key.algorithm().as_str().to_owned();
        // Looking up takes no lock (Strict checks never wait). Recording
        // happens under the write locks after a second lookup, so that
        // concurrent connects to a new host record it once.
        let mut status = self.status(host, port, key)?;
        if status == HostKeyStatus::Unknown && self.policy == HostKeyPolicy::AcceptNew {
            let lock = self.lock_for_write()?;
            // Another thread or process may have recorded the host meanwhile.
            status = self.status(host, port, key)?;
            if status == HostKeyStatus::Unknown {
                if abandoned.load(Ordering::SeqCst) {
                    return Err(TunnelError::Closed {
                        reason: "the connection was given up before its host key was recorded"
                            .to_owned(),
                    });
                }
                tracing::info!(host, port, fingerprint = %fingerprint(key), "recording new SSH host key");
                return lock.file.append_locked(host, port, key);
            }
        }
        match status {
            HostKeyStatus::Known => Ok(()),
            HostKeyStatus::Unknown => match self.policy {
                // Recorded above; a second lookup cannot be Unknown here.
                HostKeyPolicy::AcceptNew => Ok(()),
                HostKeyPolicy::Strict => Err(TunnelError::HostKeyUnknown {
                    host: host.to_owned(),
                    port,
                    key_type,
                    fingerprint: fingerprint(key),
                    presented_key: openssh_public(key),
                }),
            },
            HostKeyStatus::Changed { expected, line } => {
                Err(TunnelError::HostKeyChanged(Box::new(HostKeyChange {
                    host: host.to_owned(),
                    port,
                    key_type,
                    expected_fingerprint: fingerprint(&expected),
                    actual_fingerprint: fingerprint(key),
                    line,
                    presented_key: openssh_public(key),
                })))
            }
            HostKeyStatus::Revoked { line } => Err(TunnelError::HostKeyRevoked {
                host: host.to_owned(),
                port,
                fingerprint: fingerprint(key),
                line,
            }),
        }
    }

    /// [`KnownHosts::verify`] on tokio's blocking thread pool, so that the
    /// file IO, the `fsync` of a recorded key and the wait for the write
    /// locks never stall a runtime worker. The locks are taken and released
    /// inside the blocking task; nothing is held across the `await`.
    ///
    /// The blocking task keeps running when the caller stops waiting (for
    /// example on a connect timeout). The caller then sets `abandoned`, and
    /// a key that is not recorded yet is not recorded for that connection.
    pub(crate) async fn verify_on_blocking_pool(
        &self,
        host: String,
        port: u16,
        key: PublicKey,
        abandoned: Arc<AtomicBool>,
    ) -> Result<(), TunnelError> {
        let known_hosts = self.clone();
        tokio::task::spawn_blocking(move || {
            known_hosts.verify_unless_abandoned(&host, port, &key, &abandoned)
        })
        .await
        .map_err(|e| {
            TunnelError::io(
                "checking the SSH host key failed",
                std::io::Error::other(e.to_string()),
            )
        })?
    }

    /// Appends an entry for the host. Existing entries are kept.
    ///
    /// # Errors
    /// Returns [`TunnelError::Io`] when the file cannot be written.
    pub fn add(&self, host: &str, port: u16, key: &PublicKey) -> Result<(), TunnelError> {
        let lock = self.lock_for_write()?;
        lock.file.append_locked(host, port, key)
    }

    /// Takes the in-process lock for this path and then the cross-process
    /// file lock, waiting at most [`LOCK_WAIT`] in total, and removes stale
    /// temporary files. Creates the parent directory and the sidecar lock
    /// file when needed. Both locks are held until the result is dropped.
    fn lock_for_write(&self) -> Result<WriteLock, TunnelError> {
        self.lock_for_write_within(LOCK_WAIT)
    }

    fn lock_for_write_within(&self, wait: Duration) -> Result<WriteLock, TunnelError> {
        let deadline = Instant::now() + wait;
        // Lock, write and rename the real file: a symlinked known hosts file
        // keeps its link, and two names of one file share one lock.
        let real = KnownHosts {
            path: resolve_path(&self.path),
            ..self.clone()
        };
        let lock_path = real.lock_path()?;
        let io = |e| {
            TunnelError::io(
                format!("cannot lock known hosts file {}", self.path.display()),
                e,
            )
        };
        let timed_out = |holder: String| {
            TunnelError::io(
                format!(
                    "gave up after {} s waiting to write known hosts file {}: {holder}",
                    wait.as_secs_f32(),
                    self.path.display()
                ),
                std::io::Error::from(std::io::ErrorKind::TimedOut),
            )
        };
        let path_lock = PathLock::acquire(&real.path, deadline)
            .ok_or_else(|| timed_out("another connection of this app is writing it".to_owned()))?;
        if let Some(parent) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let lock_file = open_options()
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(io)?;
        let mut pause = Duration::from_millis(5);
        loop {
            match lock_file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(timed_out(format!(
                            "another process holds the lock file {}",
                            lock_path.display()
                        )));
                    }
                    std::thread::sleep(pause.min(deadline - now));
                    pause = (pause * 2).min(LOCK_RETRY_MAX);
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(io(e)),
            }
        }
        remove_stale_temps(&real.path);
        Ok(WriteLock {
            file: real,
            _lock_file: lock_file,
            _path: path_lock,
        })
    }

    /// The sidecar lock file: `known_hosts` becomes `known_hosts.lock`.
    fn lock_path(&self) -> Result<PathBuf, TunnelError> {
        let Some(name) = self.path.file_name() else {
            return Err(TunnelError::io(
                format!("{} is not a file path", self.path.display()),
                std::io::Error::from(std::io::ErrorKind::InvalidInput),
            ));
        };
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        Ok(self.path.with_file_name(lock_name))
    }

    fn append_locked(&self, host: &str, port: u16, key: &PublicKey) -> Result<(), TunnelError> {
        let line = entry_line(host, port, key, self.hash_new_entries)?;
        let io = |e| {
            TunnelError::io(
                format!("cannot write known hosts file {}", self.path.display()),
                e,
            )
        };
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let needs_newline = match std::fs::read(&self.path) {
            Ok(bytes) => !bytes.is_empty() && !bytes.ends_with(b"\n"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(io(e)),
        };
        let mut file = open_options().append(true).open(&self.path).map_err(io)?;
        let mut text = String::new();
        if needs_newline {
            text.push('\n');
        }
        text.push_str(&line);
        text.push('\n');
        file.write_all(text.as_bytes()).map_err(io)?;
        file.sync_all().map_err(io)
    }

    /// Removes every entry for the host (whole lines, like `ssh-keygen -R`).
    /// Wildcard and `@`-marked lines are never removed. Returns how many
    /// lines were removed.
    ///
    /// # Errors
    /// Returns [`TunnelError::Io`] when the file cannot be read or written.
    pub fn forget(&self, host: &str, port: u16) -> Result<usize, TunnelError> {
        // Nothing to forget: do not create the directory or the lock file.
        // A file created right after this check has no entry of ours either.
        // `metadata` follows links, so a dangling link counts as missing.
        match std::fs::metadata(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(TunnelError::io(
                    format!("cannot read known hosts file {}", self.path.display()),
                    e,
                ));
            }
            Ok(_) => {}
        }
        let lock = self.lock_for_write()?;
        lock.file.forget_locked(host, port)
    }

    fn forget_locked(&self, host: &str, port: u16) -> Result<usize, TunnelError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(TunnelError::io(
                    format!("cannot read known hosts file {}", self.path.display()),
                    e,
                ));
            }
        };
        let name = lookup_name(host, port);
        let mut kept = String::with_capacity(text.len());
        let mut removed = 0;
        for line in text.lines() {
            if is_exact_entry_for(line, &name) {
                removed += 1;
            } else {
                kept.push_str(line);
                kept.push('\n');
            }
        }
        if removed > 0 {
            self.replace_contents(&kept)?;
        }
        Ok(removed)
    }

    /// Trusts a key for the host after the user confirmed its fingerprint:
    /// removes the old entries of the host and records the key.
    ///
    /// # Errors
    /// Returns [`TunnelError::Io`] when the file cannot be written.
    pub fn trust(&self, host: &str, port: u16, key: &PublicKey) -> Result<(), TunnelError> {
        let lock = self.lock_for_write()?;
        lock.file.forget_locked(host, port)?;
        lock.file.append_locked(host, port, key)
    }

    /// Key types recorded for the host, to prefer them during key exchange.
    pub(crate) fn known_algorithms(&self, host: &str, port: u16) -> Vec<Algorithm> {
        match self.load() {
            Ok(file) => file
                .entries_for(host, port)
                .filter(|e| e.marker.is_none())
                .map(|e| e.key.algorithm())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn replace_contents(&self, text: &str) -> Result<(), TunnelError> {
        let io = |e| {
            TunnelError::io(
                format!("cannot write known hosts file {}", self.path.display()),
                e,
            )
        };
        let (tmp, mut file) = create_temp_beside(&self.path).map_err(io)?;
        let written = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .and_then(|()| {
                drop(file);
                std::fs::rename(&tmp, &self.path)
            });
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(e));
        }
        Ok(())
    }
}

/// Holds the in-process lock and the cross-process file lock. The lock file
/// is dropped (closing it releases the file lock) before the path lock.
struct WriteLock {
    /// The known hosts file to write, with symbolic links resolved.
    file: KnownHosts,
    _lock_file: std::fs::File,
    _path: PathLock,
}

/// The in-process lock of one known hosts path (see [`LOCKED_PATHS`]).
struct PathLock {
    path: PathBuf,
}

impl PathLock {
    /// Waits until no other thread of this process holds the path, up to
    /// `deadline`. `None` when the deadline passed.
    fn acquire(path: &Path, deadline: Instant) -> Option<PathLock> {
        let mut locked = locked_paths();
        while locked.iter().any(|p| p == path) {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            locked = PATH_RELEASED
                .wait_timeout(locked, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        locked.push(path.to_path_buf());
        Some(PathLock {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for PathLock {
    fn drop(&mut self) {
        let mut locked = locked_paths();
        if let Some(i) = locked.iter().position(|p| *p == self.path) {
            locked.swap_remove(i);
        }
        drop(locked);
        PATH_RELEASED.notify_all();
    }
}

fn locked_paths() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    LOCKED_PATHS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Maximum number of symbolic links followed when resolving a path.
const MAX_LINKS: usize = 40;

/// The real file behind `path`, so that writes replace the target of a
/// symbolic link instead of the link, and every name of one file maps to
/// one lock. An existing file is canonicalized. A missing file (or the
/// missing target of a dangling link) becomes its canonical directory plus
/// its name, so the result is the same before and after the file is
/// created. When even the directory is missing, the path is used as given.
fn resolve_path(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINKS {
        let is_link = std::fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            break;
        }
        let Ok(target) = std::fs::read_link(&current) else {
            break;
        };
        current = match current.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target,
        };
    }
    match (current.parent(), current.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            match std::fs::canonicalize(parent) {
                Ok(parent) => parent.join(name),
                Err(_) => current,
            }
        }
        _ => current,
    }
}

/// Removes temporary files of `path` older than [`STALE_TEMP_AGE`]: left
/// behind by a writer that crashed between creating and renaming them.
/// Called with both write locks held. Failures are only logged.
fn remove_stale_temps(path: &Path) {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    let dir = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir,
        None => Path::new("."),
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!(".{name}.");
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.starts_with(&prefix) || !file_name.ends_with(".varve-tmp") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_TEMP_AGE);
        if stale {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {
                    tracing::info!(file = %entry.path().display(), "removed a stale temporary known hosts file")
                }
                Err(e) => {
                    tracing::debug!(error = %e, file = %entry.path().display(), "cannot remove a stale temporary known hosts file")
                }
            }
        }
    }
}

/// Creates a new temporary file with a random name in the directory of
/// `path` (so the rename stays on one file system). `create_new` never
/// follows or reuses an existing file or link.
fn create_temp_beside(path: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let mut last = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    for _ in 0..TEMP_ATTEMPTS {
        let mut suffix = [0u8; 8];
        getrandom::fill(&mut suffix).map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(name);
        tmp_name.push(format!(
            ".{}.varve-tmp",
            data_encoding::HEXLOWER.encode(&suffix)
        ));
        let tmp = path.with_file_name(tmp_name);
        match open_options().write(true).create_new(true).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

fn open_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// The OpenSSH public key text without comment.
fn openssh_public(key: &PublicKey) -> String {
    let mut key = key.clone();
    key.set_comment("");
    key.to_openssh().unwrap_or_default()
}

/// The name a host is recorded under: `host` for port 22, else `[host]:port`.
/// Host names are lowercased like OpenSSH does.
#[must_use]
pub fn lookup_name(host: &str, port: u16) -> String {
    let host = host.to_ascii_lowercase();
    if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

/// One known hosts line in OpenSSH format for the host and key.
///
/// # Errors
/// Returns [`TunnelError::Io`] when no random salt can be obtained for a
/// hashed entry.
pub fn entry_line(
    host: &str,
    port: u16,
    key: &PublicKey,
    hashed: bool,
) -> Result<String, TunnelError> {
    let name = lookup_name(host, port);
    let pattern = if hashed {
        let mut salt = [0u8; 20];
        getrandom::fill(&mut salt).map_err(|e| {
            TunnelError::io(
                "cannot create a salt for a hashed host name",
                std::io::Error::other(e.to_string()),
            )
        })?;
        hashed_name(&salt, &name)
    } else {
        name
    };
    Ok(format!("{pattern} {}", openssh_public(key)))
}

fn hashed_name(salt: &[u8], name: &str) -> String {
    let mut out = String::from("|1|");
    out.push_str(&BASE64.encode(salt));
    out.push('|');
    out.push_str(&BASE64.encode(&hmac_sha1(salt, name)));
    out
}

fn hmac_sha1(salt: &[u8], name: &str) -> Vec<u8> {
    match Hmac::<Sha1>::new_from_slice(salt) {
        Ok(mac) => mac
            .chain_update(name.as_bytes())
            .finalize()
            .into_bytes()
            .to_vec(),
        // HMAC accepts keys of any length; this branch cannot be reached.
        Err(_) => Vec::new(),
    }
}

/// A marker at the start of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// `@revoked`: the key must never be accepted.
    Revoked,
    /// `@cert-authority`: parsed but ignored when checking (see the module docs).
    CertAuthority,
}

/// One parsed key line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHostEntry {
    /// Line number in the file (1-based).
    pub line: usize,
    /// Marker, if any.
    pub marker: Option<Marker>,
    /// The host pattern field as written (comma separated).
    pub hosts: String,
    /// The key.
    pub key: PublicKey,
}

impl KnownHostEntry {
    /// Whether the entry's host field matches `[host]:port` / `host`.
    #[must_use]
    pub fn matches(&self, host: &str, port: u16) -> bool {
        hosts_match(&self.hosts, &lookup_name(host, port))
    }
}

/// A parsed known hosts file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownHostsFile {
    /// Parsed key lines. Comments, blank and unparsable lines are not kept.
    pub entries: Vec<KnownHostEntry>,
}

impl KnownHostsFile {
    /// Parses file contents. Unparsable lines are skipped.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let entries = text
            .lines()
            .enumerate()
            .filter_map(|(i, line)| parse_line(i + 1, line))
            .collect();
        Self { entries }
    }

    /// Entries whose host field matches.
    pub fn entries_for<'a>(
        &'a self,
        host: &str,
        port: u16,
    ) -> impl Iterator<Item = &'a KnownHostEntry> + 'a {
        let name = lookup_name(host, port);
        self.entries
            .iter()
            .filter(move |e| hosts_match(&e.hosts, &name))
    }

    /// Looks up a presented key.
    ///
    /// A recorded key of any type that differs from the presented one counts
    /// as changed. The tunnel prefers the recorded key types during key
    /// exchange (like OpenSSH), so a server that answers with another type
    /// no longer has the recorded key.
    #[must_use]
    pub fn status(&self, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
        let mut changed = None;
        let mut known = false;
        for entry in self.entries_for(host, port) {
            match entry.marker {
                Some(Marker::Revoked) => {
                    if same_key(&entry.key, key) {
                        return HostKeyStatus::Revoked { line: entry.line };
                    }
                }
                Some(Marker::CertAuthority) => {}
                None => {
                    if same_key(&entry.key, key) {
                        known = true;
                    } else if changed.is_none()
                        || (entry.key.algorithm() == key.algorithm()
                            && changed.as_ref().is_some_and(|(k, _): &(PublicKey, usize)| {
                                k.algorithm() != key.algorithm()
                            }))
                    {
                        // Prefer reporting a recorded key of the same type.
                        changed = Some((entry.key.clone(), entry.line));
                    }
                }
            }
        }
        match (known, changed) {
            (true, _) => HostKeyStatus::Known,
            (false, Some((expected, line))) => HostKeyStatus::Changed { expected, line },
            (false, None) => HostKeyStatus::Unknown,
        }
    }

    /// Renders the entries back to OpenSSH format, one per line.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for e in &self.entries {
            match e.marker {
                Some(Marker::Revoked) => out.push_str("@revoked "),
                Some(Marker::CertAuthority) => out.push_str("@cert-authority "),
                None => {}
            }
            let _ = writeln!(out, "{} {}", e.hosts, openssh_public(&e.key));
        }
        out
    }
}

fn same_key(a: &PublicKey, b: &PublicKey) -> bool {
    a.key_data() == b.key_data()
}

fn parse_line(number: usize, line: &str) -> Option<KnownHostEntry> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split_whitespace();
    let mut first = fields.next()?;
    let marker = if let Some(m) = first.strip_prefix('@') {
        let marker = match m {
            "revoked" => Marker::Revoked,
            "cert-authority" => Marker::CertAuthority,
            _ => return None,
        };
        first = fields.next()?;
        Some(marker)
    } else {
        None
    };
    let key_type = fields.next()?;
    let blob = fields.next()?;
    let key = PublicKey::from_openssh(&format!("{key_type} {blob}")).ok()?;
    Some(KnownHostEntry {
        line: number,
        marker,
        hosts: first.to_owned(),
        key,
    })
}

/// OpenSSH `match_hostname`: a negated match wins, otherwise any positive match.
fn hosts_match(patterns: &str, name: &str) -> bool {
    if let Some(hashed) = patterns.strip_prefix("|1|") {
        return hashed_matches(hashed, name);
    }
    let mut matched = false;
    for pattern in patterns.split(',') {
        if let Some(negated) = pattern.strip_prefix('!') {
            if wildcard_match(&negated.to_ascii_lowercase(), name) {
                return false;
            }
        } else if let Some(hashed) = pattern.strip_prefix("|1|") {
            if hashed_matches(hashed, name) {
                matched = true;
            }
        } else if wildcard_match(&pattern.to_ascii_lowercase(), name) {
            matched = true;
        }
    }
    matched
}

fn hashed_matches(salt_and_hash: &str, name: &str) -> bool {
    let Some((salt, hash)) = salt_and_hash.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (
        BASE64.decode(salt.as_bytes()),
        BASE64.decode(hash.as_bytes()),
    ) else {
        return false;
    };
    match Hmac::<Sha1>::new_from_slice(&salt) {
        Ok(mac) => mac
            .chain_update(name.as_bytes())
            .verify_slice(&hash)
            .is_ok(),
        Err(_) => false,
    }
}

/// Glob match with `*` (any run) and `?` (one character).
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(&c) if c == '?' || Some(&c) == t.get(ti) => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p.get(pi..)
        .is_some_and(|rest| rest.iter().all(|&c| c == '*'))
}

/// Whether `ssh-keygen -R` semantics would remove this line for `name`:
/// an unmarked line whose host field matches without wildcards.
fn is_exact_entry_for(line: &str, name: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('@') {
        return false;
    }
    let Some(hosts) = trimmed.split_whitespace().next() else {
        return false;
    };
    hosts.split(',').any(|p| {
        if let Some(hashed) = p.strip_prefix("|1|") {
            hashed_matches(hashed, name)
        } else {
            !p.starts_with('!') && !p.contains(['*', '?']) && p.eq_ignore_ascii_case(name)
        }
    })
}

/// Reorders host key algorithms so that types recorded for the host come
/// first (OpenSSH does the same), keeping the library's order otherwise.
pub(crate) fn prefer_known(preferred: &[Algorithm], known: &[Algorithm]) -> Vec<Algorithm> {
    let is_known = |a: &Algorithm| known.iter().any(|k| same_type(k, a));
    let mut out: Vec<Algorithm> = preferred.iter().filter(|a| is_known(a)).cloned().collect();
    out.extend(preferred.iter().filter(|a| !is_known(a)).cloned());
    out
}

fn same_type(a: &Algorithm, b: &Algorithm) -> bool {
    match (a, b) {
        (Algorithm::Rsa { .. }, Algorithm::Rsa { .. }) => true,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::PrivateKey;
    use std::time::Duration;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn key(alg: Algorithm) -> Result<PublicKey, Box<dyn std::error::Error>> {
        Ok(PrivateKey::random(&mut rand::rng(), alg)?
            .public_key()
            .clone())
    }

    fn ed() -> Result<PublicKey, Box<dyn std::error::Error>> {
        key(Algorithm::Ed25519)
    }

    fn ecdsa() -> Result<PublicKey, Box<dyn std::error::Error>> {
        key(Algorithm::Ecdsa {
            curve: russh::keys::EcdsaCurve::NistP256,
        })
    }

    #[test]
    fn lookup_names() {
        assert_eq!(lookup_name("Example.COM", 22), "example.com");
        assert_eq!(lookup_name("db.example.com", 2222), "[db.example.com]:2222");
        assert_eq!(lookup_name("::1", 2222), "[::1]:2222");
    }

    #[test]
    fn parses_plain_hashed_ports_and_markers() -> TestResult {
        let a = ed()?;
        let b = ecdsa()?;
        let c = ed()?;
        let hashed = entry_line("hashed.example", 22, &a, true)?;
        assert!(hashed.starts_with("|1|"), "{hashed}");
        let text = format!(
            "# comment\n\n\
             plain.example,10.0.0.1 {a}\n\
             [ported.example]:2222 {b} trailing comment\n\
             {hashed}\n\
             @cert-authority *.example {c}\n\
             @revoked revoked.example {c}\n\
             @unknown-marker x.example {a}\n\
             broken.example ssh-ed25519 !!!notbase64\n\
             dsa.example ssh-dss AAAAB3NzaC1kc3MAAACBAP\n\
             *.wild.example,!bad.wild.example {b}\n",
            a = openssh_public(&a),
            b = openssh_public(&b),
            c = openssh_public(&c),
        );
        let file = KnownHostsFile::parse(&text);
        assert_eq!(file.entries.len(), 6, "{:#?}", file.entries);

        assert_eq!(file.status("plain.example", 22, &a), HostKeyStatus::Known);
        assert_eq!(file.status("10.0.0.1", 22, &a), HostKeyStatus::Known);
        assert_eq!(file.status("PLAIN.example", 22, &a), HostKeyStatus::Known);
        // Same host on another port is a different entry.
        assert_eq!(
            file.status("plain.example", 2222, &a),
            HostKeyStatus::Unknown
        );

        assert_eq!(
            file.status("ported.example", 2222, &b),
            HostKeyStatus::Known
        );
        assert_eq!(
            file.status("ported.example", 22, &b),
            HostKeyStatus::Unknown
        );

        assert_eq!(file.status("hashed.example", 22, &a), HostKeyStatus::Known);
        assert!(matches!(
            file.status("hashed.example", 22, &b),
            HostKeyStatus::Changed { line: 5, .. }
        ));

        // @cert-authority lines never vouch for a host.
        assert_eq!(file.status("host.example", 22, &c), HostKeyStatus::Unknown);
        assert_eq!(
            file.status("revoked.example", 22, &c),
            HostKeyStatus::Revoked { line: 7 }
        );

        assert_eq!(file.status("a.wild.example", 22, &b), HostKeyStatus::Known);
        assert_eq!(
            file.status("bad.wild.example", 22, &b),
            HostKeyStatus::Unknown
        );
        Ok(())
    }

    #[test]
    fn changed_prefers_same_type_and_reports_line() -> TestResult {
        let recorded_ec = ecdsa()?;
        let recorded_ed = ed()?;
        let presented = ed()?;
        let text = format!(
            "h.example {}\nh.example {}\n",
            openssh_public(&recorded_ec),
            openssh_public(&recorded_ed)
        );
        let file = KnownHostsFile::parse(&text);
        match file.status("h.example", 22, &presented) {
            HostKeyStatus::Changed { expected, line } => {
                assert_eq!(expected, recorded_ed);
                assert_eq!(line, 2);
            }
            other => panic!("expected Changed, got {other:?}"),
        }
        // Another type only recorded: still a change.
        let file = KnownHostsFile::parse(&format!("h.example {}\n", openssh_public(&recorded_ec)));
        assert!(matches!(
            file.status("h.example", 22, &presented),
            HostKeyStatus::Changed { line: 1, .. }
        ));
        Ok(())
    }

    #[test]
    fn hashed_names_round_trip() {
        let salt = [7u8; 20];
        let line = hashed_name(&salt, "[127.0.0.1]:2222");
        assert!(hosts_match(&line, "[127.0.0.1]:2222"));
        assert!(!hosts_match(&line, "127.0.0.1"));
        assert!(!hosts_match("|1|bad", "127.0.0.1"));
    }

    #[test]
    fn hashed_entry_from_ssh_keygen() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let k = ed()?;
        std::fs::write(&path, format!("[db.example]:2200 {}\n", openssh_public(&k)))?;
        let out = std::process::Command::new("ssh-keygen")
            .arg("-H")
            .arg("-f")
            .arg(&path)
            .output()
            .map_err(|e| format!("ssh-keygen is needed for this test: {e}"))?;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = std::fs::read_to_string(&path)?;
        assert!(text.starts_with("|1|"), "{text}");
        let hosts = KnownHosts::new(&path, HostKeyPolicy::Strict);
        assert_eq!(hosts.status("db.example", 2200, &k)?, HostKeyStatus::Known);
        assert_eq!(hosts.status("db.example", 22, &k)?, HostKeyStatus::Unknown);
        Ok(())
    }

    #[test]
    fn verify_policies_and_writing() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sub").join("known_hosts");
        let k = ed()?;
        let strict = KnownHosts::new(&path, HostKeyPolicy::Strict);
        match strict.verify("srv.example", 2222, &k) {
            Err(TunnelError::HostKeyUnknown {
                fingerprint: fp,
                presented_key,
                port: 2222,
                ..
            }) => {
                assert_eq!(fp, fingerprint(&k));
                assert_eq!(crate::keys::parse_public_key(&presented_key)?, {
                    let mut k = k.clone();
                    k.set_comment("");
                    k
                });
            }
            other => panic!("expected HostKeyUnknown, got {other:?}"),
        }
        assert!(!path.exists());

        let accept = KnownHosts::new(&path, HostKeyPolicy::AcceptNew);
        accept.verify("srv.example", 2222, &k)?;
        let text = std::fs::read_to_string(&path)?;
        assert!(
            text.starts_with("[srv.example]:2222 ssh-ed25519 "),
            "{text}"
        );
        strict.verify("srv.example", 2222, &k)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)?.permissions().mode() & 0o777,
                0o600
            );
        }

        // Changed key: both policies refuse, with both fingerprints.
        let other = ed()?;
        for hosts in [&strict, &accept] {
            match hosts.verify("srv.example", 2222, &other) {
                Err(TunnelError::HostKeyChanged(change)) => {
                    assert_eq!(change.expected_fingerprint, fingerprint(&k));
                    assert_eq!(change.actual_fingerprint, fingerprint(&other));
                    assert_eq!(change.line, 1);
                    assert_eq!(change.port, 2222);
                }
                other => panic!("expected HostKeyChanged, got {other:?}"),
            }
        }

        // Hashed writing.
        let hashed = KnownHosts {
            hash_new_entries: true,
            ..KnownHosts::new(&path, HostKeyPolicy::AcceptNew)
        };
        hashed.verify("secret.example", 22, &other)?;
        let text = std::fs::read_to_string(&path)?;
        assert!(!text.contains("secret.example"), "{text}");
        strict.verify("secret.example", 22, &other)?;

        // Trust replaces the old key.
        strict.trust("srv.example", 2222, &other)?;
        strict.verify("srv.example", 2222, &other)?;
        assert!(matches!(
            strict.verify("srv.example", 2222, &k),
            Err(TunnelError::HostKeyChanged(_))
        ));
        // Forget removes hashed entries too; other lines are kept.
        assert_eq!(strict.forget("secret.example", 22)?, 1);
        assert!(matches!(
            strict.verify("secret.example", 22, &other),
            Err(TunnelError::HostKeyUnknown { .. })
        ));
        strict.verify("srv.example", 2222, &other)?;
        Ok(())
    }

    #[test]
    fn concurrent_accept_new_records_once() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let k = ed()?;
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let hosts = KnownHosts::new(&path, HostKeyPolicy::AcceptNew);
                let k = k.clone();
                std::thread::spawn(move || hosts.verify("race.example", 2200, &k))
            })
            .collect();
        for t in threads {
            t.join().map_err(|_| "thread panicked")??;
        }
        assert_eq!(std::fs::read_to_string(&path)?.lines().count(), 1);
        Ok(())
    }

    #[test]
    fn concurrent_trusts_of_different_hosts_all_end_up_in_the_file() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        // Every host has an old entry, so each trust rewrites the whole file
        // (forget) and then appends: the read-modify-write that loses entries
        // when two run at once without serialization.
        let hosts: Vec<(String, PublicKey, PublicKey)> = (0..16)
            .map(|i| Ok((format!("h{i}.example"), ed()?, ed()?)))
            .collect::<Result<_, Box<dyn std::error::Error>>>()?;
        let mut text = String::from("# kept\n");
        for (host, old, _) in &hosts {
            text.push_str(&entry_line(host, 2200, old, false)?);
            text.push('\n');
        }
        std::fs::write(&path, text)?;

        let start = std::sync::Arc::new(std::sync::Barrier::new(hosts.len()));
        let threads: Vec<_> = hosts
            .iter()
            .map(|(host, _, new)| {
                let known = KnownHosts::new(&path, HostKeyPolicy::Strict);
                let (host, new, start) = (host.clone(), new.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    known.trust(&host, 2200, &new)
                })
            })
            .collect();
        for t in threads {
            t.join().map_err(|_| "thread panicked")??;
        }

        let text = std::fs::read_to_string(&path)?;
        assert!(text.starts_with("# kept\n"), "{text}");
        let file = KnownHostsFile::parse(&text);
        assert_eq!(file.entries.len(), hosts.len(), "{text}");
        for (host, old, new) in &hosts {
            assert_eq!(file.status(host, 2200, new), HostKeyStatus::Known, "{host}");
            assert!(
                matches!(file.status(host, 2200, old), HostKeyStatus::Changed { .. }),
                "{host}"
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_accept_new_of_different_hosts_on_the_runtime() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let (a, b) = (ed()?, ecdsa()?);
        let known = KnownHosts::new(&path, HostKeyPolicy::AcceptNew);
        let (ra, rb) = tokio::join!(
            known.verify_on_blocking_pool("a.example".to_owned(), 22, a.clone(), Arc::default()),
            known.verify_on_blocking_pool("b.example".to_owned(), 2222, b.clone(), Arc::default()),
        );
        ra?;
        rb?;
        let file = known.load()?;
        assert_eq!(file.entries.len(), 2);
        assert_eq!(file.status("a.example", 22, &a), HostKeyStatus::Known);
        assert_eq!(file.status("b.example", 2222, &b), HostKeyStatus::Known);
        Ok(())
    }

    /// A host key check that has to wait for the write lock (another tunnel
    /// or the UI is writing the file) must not stall the runtime: on a
    /// single-threaded runtime, other tasks keep running meanwhile.
    #[test]
    fn host_key_check_does_not_block_the_runtime() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let k = ed()?;
        let (progress_tx, progress_rx) = std::sync::mpsc::channel();

        let guard = PathLock::acquire(
            &resolve_path(&path),
            Instant::now() + Duration::from_secs(60),
        )
        .ok_or("path lock")?;
        let runtime_thread = {
            let path = path.clone();
            let k = k.clone();
            std::thread::spawn(move || -> Result<(), String> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())?;
                rt.block_on(async move {
                    let known = KnownHosts::new(&path, HostKeyPolicy::AcceptNew);
                    let check = tokio::spawn(async move {
                        known
                            .verify_on_blocking_pool(
                                "lock.example".to_owned(),
                                22,
                                k,
                                Arc::default(),
                            )
                            .await
                    });
                    tokio::task::yield_now().await;
                    // Another task on the same (only) runtime thread.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let _ = progress_tx.send(());
                    check
                        .await
                        .map_err(|e| e.to_string())?
                        .map_err(|e| e.to_string())
                })
            })
        };

        let progressed = progress_rx.recv_timeout(Duration::from_secs(5));
        drop(guard);
        let result = runtime_thread
            .join()
            .map_err(|_| "runtime thread panicked")?;
        assert!(
            progressed.is_ok(),
            "the runtime stalled while the host key check waited for the lock"
        );
        result?;
        assert_eq!(
            KnownHosts::new(&path, HostKeyPolicy::Strict).status("lock.example", 22, &k)?,
            HostKeyStatus::Known
        );
        Ok(())
    }

    /// A host key check whose connect was given up while it waited must not
    /// record the key.
    #[test]
    fn abandoned_check_does_not_record() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let k = ed()?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let abandoned = Arc::new(AtomicBool::new(false));

        let guard = PathLock::acquire(
            &resolve_path(&path),
            Instant::now() + Duration::from_secs(60),
        )
        .ok_or("path lock")?;
        let check = {
            let known = KnownHosts::new(&path, HostKeyPolicy::AcceptNew);
            let (k, abandoned) = (k.clone(), abandoned.clone());
            rt.spawn(async move {
                known
                    .verify_on_blocking_pool("late.example".to_owned(), 22, k, abandoned)
                    .await
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        // The connect timed out while the check waited for the lock.
        abandoned.store(true, Ordering::SeqCst);
        drop(guard);
        let result = rt.block_on(check)?;
        assert!(
            matches!(result, Err(TunnelError::Closed { .. })),
            "{result:?}"
        );
        assert!(
            !path.exists(),
            "the key of an abandoned connect was recorded"
        );

        // A known key still verifies when abandoned: nothing is written.
        let known = KnownHosts::new(&path, HostKeyPolicy::AcceptNew);
        known.add("late.example", 22, &k)?;
        let flag = Arc::new(AtomicBool::new(true));
        rt.block_on(known.verify_on_blocking_pool("late.example".to_owned(), 22, k, flag))?;
        Ok(())
    }

    /// Writers wait for the sidecar file lock, which another process (here:
    /// an independent lock holder with its own file handle) may hold.
    #[test]
    fn writers_wait_for_the_cross_process_file_lock() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let lock_path = dir.path().join("known_hosts.lock");
        let k = ed()?;
        let other_process = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        other_process.lock()?;

        let writers: Vec<_> = [
            Box::new({
                let (path, k) = (path.clone(), k.clone());
                move || KnownHosts::new(&path, HostKeyPolicy::Strict).add("add.example", 22, &k)
            }) as Box<dyn FnOnce() -> Result<(), TunnelError> + Send>,
            Box::new({
                let (path, k) = (path.clone(), k.clone());
                move || KnownHosts::new(&path, HostKeyPolicy::Strict).trust("trust.example", 22, &k)
            }),
            Box::new({
                let (path, k) = (path.clone(), k.clone());
                move || {
                    KnownHosts::new(&path, HostKeyPolicy::AcceptNew).verify("new.example", 22, &k)
                }
            }),
        ]
        .into_iter()
        .map(std::thread::spawn)
        .collect();

        std::thread::sleep(Duration::from_millis(300));
        assert!(
            writers.iter().all(|w| !w.is_finished()),
            "a writer did not wait for the file lock"
        );
        assert!(!path.exists());
        other_process.unlock()?;
        for w in writers {
            w.join().map_err(|_| "writer panicked")??;
        }
        let file = KnownHosts::new(&path, HostKeyPolicy::Strict).load()?;
        for host in ["add.example", "trust.example", "new.example"] {
            assert_eq!(file.status(host, 22, &k), HostKeyStatus::Known, "{host}");
        }
        Ok(())
    }

    /// Environment of [`child_process_trusts_its_host`]: the known hosts
    /// path, the host, and the start time in milliseconds since the epoch.
    const CHILD_ENV: &str = "VARVE_KNOWN_HOSTS_CHILD";

    /// Not a test on its own: the body runs only in the child processes
    /// that [`concurrent_trusts_from_several_processes`] starts.
    #[test]
    fn child_process_trusts_its_host() -> TestResult {
        let Ok(spec) = std::env::var(CHILD_ENV) else {
            return Ok(());
        };
        let mut parts = spec.split('|');
        let (Some(path), Some(host), Some(start)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err("bad child spec".into());
        };
        let start = std::time::UNIX_EPOCH + Duration::from_millis(start.parse()?);
        if let Ok(wait) = start.duration_since(std::time::SystemTime::now()) {
            std::thread::sleep(wait);
        }
        let known = KnownHosts::new(path, HostKeyPolicy::Strict);
        let mut last = None;
        for _ in 0..25 {
            let k = ed()?;
            known.trust(host, 2200, &k)?;
            last = Some(k);
        }
        let last = last.ok_or("no key")?;
        let result = Path::new(path).with_file_name(format!("{host}.pub"));
        std::fs::write(result, openssh_public(&last))?;
        Ok(())
    }

    #[test]
    fn concurrent_trusts_from_several_processes() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let hosts: Vec<String> = (0..6).map(|i| format!("p{i}.example")).collect();
        let mut text = String::new();
        for host in &hosts {
            text.push_str(&entry_line(host, 2200, &ed()?, false)?);
            text.push('\n');
        }
        std::fs::write(&path, text)?;

        let start = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?
            + Duration::from_millis(1500);
        let exe = std::env::current_exe()?;
        let children: Vec<_> = hosts
            .iter()
            .map(|host| {
                std::process::Command::new(&exe)
                    .args([
                        "--exact",
                        "known_hosts::tests::child_process_trusts_its_host",
                        "--test-threads",
                        "1",
                    ])
                    .env(
                        CHILD_ENV,
                        format!("{}|{host}|{}", path.display(), start.as_millis()),
                    )
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
            })
            .collect::<Result<_, _>>()?;
        for child in children {
            let out = child.wait_with_output()?;
            assert!(
                out.status.success(),
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let file = KnownHosts::new(&path, HostKeyPolicy::Strict).load()?;
        assert_eq!(file.entries.len(), hosts.len(), "{file:#?}");
        for host in &hosts {
            let expected = std::fs::read_to_string(dir.path().join(format!("{host}.pub")))?;
            let expected = crate::keys::parse_public_key(&expected)?;
            assert_eq!(
                file.status(host, 2200, &expected),
                HostKeyStatus::Known,
                "{host}"
            );
        }
        assert_no_temp_files(dir.path())
    }

    fn assert_no_temp_files(dir: &Path) -> TestResult {
        for entry in std::fs::read_dir(dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            assert!(!name.ends_with(".varve-tmp"), "left behind: {name}");
        }
        Ok(())
    }

    /// The temporary file of a rewrite has a fresh name, so whatever sits at
    /// a predictable name (another writer's file, a link, a directory) is
    /// neither used nor followed.
    #[test]
    fn rewrite_uses_a_unique_temporary_file() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let (old, new) = (ed()?, ed()?);
        std::fs::write(
            &path,
            format!("{}\n", entry_line("t.example", 22, &old, false)?),
        )?;
        // The name the old code always used.
        std::fs::create_dir(dir.path().join("known_hosts.varve-tmp"))?;
        let known = KnownHosts::new(&path, HostKeyPolicy::Strict);
        known.trust("t.example", 22, &new)?;
        assert_eq!(known.status("t.example", 22, &new)?, HostKeyStatus::Known);
        assert_eq!(known.load()?.entries.len(), 1);
        assert!(dir.path().join("known_hosts.varve-tmp").is_dir());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".known_hosts.") && n.ends_with(".varve-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        Ok(())
    }

    fn hold_lock_file(dir: &Path) -> Result<std::fs::File, Box<dyn std::error::Error>> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("known_hosts.lock"))?;
        file.lock()?;
        Ok(file)
    }

    /// Strict checks only read: no lock file, no directory, and no waiting
    /// for a lock that another process holds.
    #[test]
    fn strict_checks_take_no_lock() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sub").join("known_hosts");
        let k = ed()?;
        let strict = KnownHosts::new(&path, HostKeyPolicy::Strict);
        assert!(matches!(
            strict.verify("s.example", 22, &k),
            Err(TunnelError::HostKeyUnknown { .. })
        ));
        assert!(!dir.path().join("sub").exists());

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        std::fs::write(
            &path,
            format!("{}\n", entry_line("s.example", 22, &k, false)?),
        )?;
        let strict = KnownHosts::new(&path, HostKeyPolicy::Strict);
        strict.verify("s.example", 22, &k)?;
        assert!(matches!(
            strict.verify("other.example", 22, &k),
            Err(TunnelError::HostKeyUnknown { .. })
        ));
        assert!(!dir.path().join("known_hosts.lock").exists());

        // Another process holds the lock forever: reads still answer at once.
        let _held = hold_lock_file(dir.path())?;
        let started = Instant::now();
        strict.verify("s.example", 22, &k)?;
        assert!(strict.verify("other.example", 22, &k).is_err());
        KnownHosts::new(&path, HostKeyPolicy::AcceptNew).verify("s.example", 22, &k)?;
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    /// A writer gives up with a clear error when another process holds the
    /// lock file for too long, instead of blocking forever.
    #[test]
    fn writer_waits_for_the_file_lock_only_so_long() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let _held = hold_lock_file(dir.path())?;
        let known = KnownHosts::new(&path, HostKeyPolicy::AcceptNew);
        let started = Instant::now();
        match known.lock_for_write_within(Duration::from_millis(300)) {
            Err(TunnelError::Io { context, source }) => {
                assert_eq!(source.kind(), std::io::ErrorKind::TimedOut);
                assert!(
                    context.contains("another process holds the lock file"),
                    "{context}"
                );
                assert!(context.contains("known_hosts.lock"), "{context}");
            }
            Err(other) => panic!("expected a timeout, got {other:?}"),
            Ok(_) => panic!("took a lock another process holds"),
        }
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(300), "{waited:?}");
        assert!(waited < Duration::from_secs(3), "{waited:?}");
        // The in-process lock was released again.
        assert!(PathLock::acquire(&resolve_path(&path), Instant::now()).is_some());
        Ok(())
    }

    /// Writers of different files never wait for each other.
    #[test]
    fn writers_of_different_files_do_not_wait_for_each_other() -> TestResult {
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a_known_hosts");
        let b = dir.path().join("b_known_hosts");
        let k = ed()?;
        let _a_busy =
            PathLock::acquire(&resolve_path(&a), Instant::now() + Duration::from_secs(60))
                .ok_or("path lock")?;
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer = {
            let (b, k) = (b.clone(), k.clone());
            std::thread::spawn(move || {
                let result = KnownHosts::new(&b, HostKeyPolicy::Strict).add("b.example", 22, &k);
                let _ = done_tx.send(());
                result
            })
        };
        let finished = done_rx.recv_timeout(Duration::from_secs(5));
        drop(_a_busy);
        writer.join().map_err(|_| "writer panicked")??;
        assert!(finished.is_ok(), "writing b waited for the lock of a");
        Ok(())
    }

    #[test]
    fn writers_remove_stale_temporary_files() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let ten_minutes_ago = SystemTime::now() - Duration::from_secs(600);
        let make = |name: &str, old: bool| -> Result<PathBuf, Box<dyn std::error::Error>> {
            let file_path = dir.path().join(name);
            let file = std::fs::File::create(&file_path)?;
            if old {
                file.set_modified(ten_minutes_ago)?;
            }
            Ok(file_path)
        };
        let stale = make(".known_hosts.0011223344556677.varve-tmp", true)?;
        let fresh = make(".known_hosts.8899aabbccddeeff.varve-tmp", false)?;
        let other_file = make(".other_hosts.0011223344556677.varve-tmp", true)?;
        let unrelated = make(".known_hosts.backup", true)?;

        KnownHosts::new(&path, HostKeyPolicy::Strict).add("t.example", 22, &ed()?)?;
        assert!(!stale.exists(), "a stale temporary file was kept");
        assert!(fresh.exists(), "a temporary file in use may not be removed");
        assert!(other_file.exists());
        assert!(unrelated.exists());
        Ok(())
    }

    #[test]
    fn forget_on_a_missing_file_creates_nothing() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("missing").join("known_hosts");
        let known = KnownHosts::new(&path, HostKeyPolicy::Strict);
        assert_eq!(known.forget("x.example", 22)?, 0);
        assert!(!dir.path().join("missing").exists());

        let path = dir.path().join("known_hosts");
        let known = KnownHosts::new(&path, HostKeyPolicy::Strict);
        assert_eq!(known.forget("x.example", 22)?, 0);
        assert!(!path.exists());
        assert!(!dir.path().join("known_hosts.lock").exists());
        Ok(())
    }

    /// A symlinked known hosts file: the real file is rewritten and locked,
    /// the link stays a link.
    #[cfg(unix)]
    #[test]
    fn symlinked_file_is_written_through_the_link() -> TestResult {
        let dir = tempfile::tempdir()?;
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir)?;
        let real = real_dir.join("known_hosts");
        let link = dir.path().join("known_hosts");
        let (old, new, other) = (ed()?, ed()?, ed()?);
        std::fs::write(
            &real,
            format!("{}\n", entry_line("l.example", 22, &old, false)?),
        )?;
        std::os::unix::fs::symlink(&real, &link)?;

        let known = KnownHosts::new(&link, HostKeyPolicy::Strict);
        // trust rewrites the file (temp file plus rename).
        known.trust("l.example", 22, &new)?;
        known.add("m.example", 22, &other)?;
        assert_eq!(known.forget("m.example", 22)?, 1);
        assert!(
            std::fs::symlink_metadata(&link)?.file_type().is_symlink(),
            "the link was replaced by a plain file"
        );
        let through_real = KnownHosts::new(&real, HostKeyPolicy::Strict);
        assert_eq!(
            through_real.status("l.example", 22, &new)?,
            HostKeyStatus::Known
        );
        assert_eq!(through_real.load()?.entries.len(), 1);
        // The lock and temporary files live next to the real file.
        assert!(real_dir.join("known_hosts.lock").exists());
        assert!(!dir.path().join("known_hosts.lock").exists());
        for entry in std::fs::read_dir(dir.path())? {
            let name = entry?.file_name();
            assert!(!name.to_string_lossy().ends_with(".varve-tmp"), "{name:?}");
        }

        // Both names of the file share one in-process lock.
        assert_eq!(resolve_path(&link), resolve_path(&real));
        Ok(())
    }

    /// A dangling link: the missing target is created, the link stays.
    #[cfg(unix)]
    #[test]
    fn dangling_link_creates_its_target() -> TestResult {
        let dir = tempfile::tempdir()?;
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir)?;
        let real = real_dir.join("known_hosts");
        let link = dir.path().join("known_hosts");
        std::os::unix::fs::symlink("real/known_hosts", &link)?;
        let k = ed()?;
        let known = KnownHosts::new(&link, HostKeyPolicy::AcceptNew);
        // A missing target has nothing to forget and creates nothing.
        assert_eq!(known.forget("d.example", 22)?, 0);
        assert!(!real_dir.join("known_hosts.lock").exists());
        // The key is recorded in the target.
        known.verify("d.example", 22, &k)?;
        assert!(std::fs::symlink_metadata(&link)?.file_type().is_symlink());
        assert_eq!(
            KnownHosts::new(&real, HostKeyPolicy::Strict).status("d.example", 22, &k)?,
            HostKeyStatus::Known
        );
        known.trust("d.example", 22, &ed()?)?;
        assert!(std::fs::symlink_metadata(&link)?.file_type().is_symlink());
        assert_eq!(resolve_path(&link), resolve_path(&real));
        Ok(())
    }

    #[test]
    fn add_keeps_existing_content_without_trailing_newline() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("known_hosts");
        let a = ed()?;
        let b = ed()?;
        std::fs::write(&path, format!("# mine\na.example {}", openssh_public(&a)))?;
        let hosts = KnownHosts::new(&path, HostKeyPolicy::AcceptNew);
        hosts.add("b.example", 22, &b)?;
        let file = hosts.load()?;
        assert_eq!(file.entries.len(), 2);
        assert!(std::fs::read_to_string(&path)?.starts_with("# mine\n"));
        assert_eq!(file.status("a.example", 22, &a), HostKeyStatus::Known);
        assert_eq!(file.status("b.example", 22, &b), HostKeyStatus::Known);
        assert_eq!(KnownHostsFile::parse(&file.to_text()), {
            let mut f = file.clone();
            for (i, e) in f.entries.iter_mut().enumerate() {
                e.line = i + 1;
            }
            f
        });
        Ok(())
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.example.com", "db.example.com"));
        assert!(!wildcard_match("*.example.com", "example.com"));
        assert!(wildcard_match("db?.example.com", "db1.example.com"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("a*b*c", "aXXbYY"));
        assert!(wildcard_match("[db]:22*", "[db]:2222"));
    }

    #[test]
    fn prefers_known_algorithms() {
        let preferred = vec![
            Algorithm::Ed25519,
            Algorithm::Ecdsa {
                curve: russh::keys::EcdsaCurve::NistP256,
            },
            Algorithm::Rsa {
                hash: Some(russh::keys::HashAlg::Sha512),
            },
        ];
        let out = prefer_known(&preferred, &[Algorithm::Rsa { hash: None }]);
        assert_eq!(out.first(), preferred.get(2));
        assert_eq!(out.len(), 3);
        assert_eq!(prefer_known(&preferred, &[]), preferred);
    }
}
