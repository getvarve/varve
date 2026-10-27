//! Errors of SSH tunnels.
//!
//! Every message is safe to show in the UI and to log: none of them ever
//! contains a password, a passphrase or the contents of a key file.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// How the client tried to authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Password authentication.
    Password,
    /// Public key authentication with a private key file.
    KeyFile,
    /// Public key authentication through the SSH agent.
    Agent,
}

impl AuthMethod {
    /// Stable lowercase name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::Password => "password",
            AuthMethod::KeyFile => "key file",
            AuthMethod::Agent => "SSH agent",
        }
    }
}

impl fmt::Display for AuthMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The step of opening or using a tunnel that took too long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutStage {
    /// TCP connect plus SSH key exchange.
    Connect,
    /// User authentication.
    Authentication,
    /// Opening a forwarding channel to the target.
    Forward,
    /// Waiting for the server to answer a health check.
    HealthCheck,
}

impl TimeoutStage {
    /// Stable lowercase name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TimeoutStage::Connect => "connecting",
            TimeoutStage::Authentication => "authenticating",
            TimeoutStage::Forward => "opening the forwarding channel",
            TimeoutStage::HealthCheck => "waiting for the health check reply",
        }
    }
}

impl fmt::Display for TimeoutStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a private key file cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyFileProblem {
    /// The file does not exist.
    NotFound,
    /// The file exists but cannot be read (permissions, not a file, ...).
    Unreadable(String),
    /// The key is encrypted and no passphrase was given.
    PassphraseRequired,
    /// The passphrase does not decrypt the key.
    WrongPassphrase,
    /// The key type is not supported (for example DSA).
    UnsupportedKeyType(String),
    /// The file is not a private key in a supported format.
    InvalidFormat,
}

impl fmt::Display for KeyFileProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyFileProblem::NotFound => f.write_str("the file does not exist"),
            KeyFileProblem::Unreadable(reason) => write!(f, "the file cannot be read ({reason})"),
            KeyFileProblem::PassphraseRequired => {
                f.write_str("the key is protected by a passphrase and none was given")
            }
            KeyFileProblem::WrongPassphrase => f.write_str("the passphrase is not correct"),
            KeyFileProblem::UnsupportedKeyType(kind) => {
                write!(f, "keys of type {kind} are not supported")
            }
            KeyFileProblem::InvalidFormat => f.write_str(
                "the file is not a private key in OpenSSH, PKCS#1, PKCS#8 or SEC1 PEM format",
            ),
        }
    }
}

/// Details of [`TunnelError::HostKeyChanged`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyChange {
    /// SSH host.
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// Type of the presented key, for example `ssh-ed25519`.
    pub key_type: String,
    /// SHA256 fingerprint of the recorded key, OpenSSH style (`SHA256:...`).
    pub expected_fingerprint: String,
    /// SHA256 fingerprint of the presented key.
    pub actual_fingerprint: String,
    /// Line of the recorded key in the known hosts file (1-based).
    pub line: usize,
    /// The presented public key in OpenSSH format, for
    /// [`KnownHosts::trust`](crate::KnownHosts::trust) after the user confirms.
    pub presented_key: String,
}

/// A coarse classification of [`TunnelError`] for UI decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelErrorKind {
    /// See [`TunnelError::Connect`].
    Connect,
    /// See [`TunnelError::Auth`].
    Auth,
    /// See [`TunnelError::KeyFile`].
    KeyFile,
    /// See [`TunnelError::InvalidPublicKey`].
    InvalidPublicKey,
    /// See [`TunnelError::Agent`].
    Agent,
    /// See [`TunnelError::HostKeyUnknown`].
    HostKeyUnknown,
    /// See [`TunnelError::HostKeyChanged`].
    HostKeyChanged,
    /// See [`TunnelError::HostKeyRevoked`].
    HostKeyRevoked,
    /// See [`TunnelError::Forward`].
    Forward,
    /// See [`TunnelError::Timeout`].
    Timeout,
    /// See [`TunnelError::Closed`].
    Closed,
    /// See [`TunnelError::Unsupported`].
    Unsupported,
    /// See [`TunnelError::Io`].
    Io,
}

/// Everything that can go wrong with an SSH tunnel.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TunnelError {
    /// The SSH server cannot be reached, or the SSH handshake failed.
    #[error("cannot connect to the SSH server {host}:{port}: {reason}")]
    Connect {
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
        /// What went wrong.
        reason: String,
    },

    /// The server rejected the credentials.
    #[error(
        "SSH authentication of user `{username}` with {method} failed{}; the server accepts: {}",
        if *partial_success { " (the server requires a further authentication step)" } else { "" },
        if accepted.is_empty() { "nothing".to_owned() } else { accepted.join(", ") }
    )]
    Auth {
        /// The user name that was tried.
        username: String,
        /// The method that was tried.
        method: AuthMethod,
        /// The methods the server says can continue (`password`, `publickey`, ...).
        accepted: Vec<String>,
        /// The server accepted the method but wants another one as well
        /// (multi-factor authentication), which tunnels do not support.
        partial_success: bool,
    },

    /// The private key file cannot be used.
    #[error("cannot use the SSH key file {}: {problem}", path.display())]
    KeyFile {
        /// Path of the key file.
        path: PathBuf,
        /// What is wrong with it.
        problem: KeyFileProblem,
    },

    /// A text that should be an OpenSSH public key is not one.
    #[error("not a public key in OpenSSH format")]
    InvalidPublicKey,

    /// The SSH agent is not available or holds no usable key.
    #[error("SSH agent: {reason}")]
    Agent {
        /// What went wrong.
        reason: String,
    },

    /// The server's host key is not in the known hosts file and the policy
    /// is strict.
    #[error(
        "the host key of {host}:{port} is not known ({key_type} key {fingerprint}); \
         confirm the fingerprint before trusting it"
    )]
    HostKeyUnknown {
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
        /// Key type, for example `ssh-ed25519`.
        key_type: String,
        /// SHA256 fingerprint of the presented key, OpenSSH style (`SHA256:...`).
        fingerprint: String,
        /// The presented public key in OpenSSH format, for
        /// [`KnownHosts::trust`](crate::KnownHosts::trust) after the user confirms.
        presented_key: String,
    },

    /// The server presented a key that differs from the recorded one.
    /// This can be a man-in-the-middle attack.
    #[error(
        "the host key of {}:{} has changed: expected {} (known hosts line {}), got {} key {}",
        .0.host, .0.port, .0.expected_fingerprint, .0.line, .0.key_type, .0.actual_fingerprint
    )]
    HostKeyChanged(Box<HostKeyChange>),

    /// The server presented a key that is marked `@revoked`.
    #[error("the host key of {host}:{port} ({fingerprint}) is revoked (known hosts line {line})")]
    HostKeyRevoked {
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
        /// SHA256 fingerprint of the presented key.
        fingerprint: String,
        /// Line of the `@revoked` entry (1-based).
        line: usize,
    },

    /// The SSH server could not open a connection to the target.
    #[error("the SSH server cannot forward to {target_host}:{target_port}: {reason}")]
    Forward {
        /// Target host as seen from the SSH server.
        target_host: String,
        /// Target port.
        target_port: u16,
        /// Why the server refused.
        reason: String,
    },

    /// A step took longer than allowed.
    #[error("timed out after {} ms while {stage}", after.as_millis())]
    Timeout {
        /// The step that timed out.
        stage: TimeoutStage,
        /// The limit that was hit.
        after: Duration,
    },

    /// The tunnel is closed or its SSH session dropped.
    #[error("the SSH tunnel is closed: {reason}")]
    Closed {
        /// Why it closed.
        reason: String,
    },

    /// The feature is not available on this platform.
    #[error("{feature} is not supported on this platform")]
    Unsupported {
        /// What is not supported.
        feature: String,
    },

    /// A local I/O operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },
}

impl TunnelError {
    /// The coarse kind of this error.
    #[must_use]
    pub fn kind(&self) -> TunnelErrorKind {
        match self {
            TunnelError::Connect { .. } => TunnelErrorKind::Connect,
            TunnelError::Auth { .. } => TunnelErrorKind::Auth,
            TunnelError::KeyFile { .. } => TunnelErrorKind::KeyFile,
            TunnelError::InvalidPublicKey => TunnelErrorKind::InvalidPublicKey,
            TunnelError::Agent { .. } => TunnelErrorKind::Agent,
            TunnelError::HostKeyUnknown { .. } => TunnelErrorKind::HostKeyUnknown,
            TunnelError::HostKeyChanged(_) => TunnelErrorKind::HostKeyChanged,
            TunnelError::HostKeyRevoked { .. } => TunnelErrorKind::HostKeyRevoked,
            TunnelError::Forward { .. } => TunnelErrorKind::Forward,
            TunnelError::Timeout { .. } => TunnelErrorKind::Timeout,
            TunnelError::Closed { .. } => TunnelErrorKind::Closed,
            TunnelError::Unsupported { .. } => TunnelErrorKind::Unsupported,
            TunnelError::Io { .. } => TunnelErrorKind::Io,
        }
    }

    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        TunnelError::Io {
            context: context.into(),
            source,
        }
    }
}
