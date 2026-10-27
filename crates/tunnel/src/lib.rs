//! SSH tunnels for Varve (design doc sections 3.1 and 3.3).
//!
//! A [`Tunnel`] connects to an SSH server, checks its host key against an
//! app-owned [`KnownHosts`] file, authenticates with a password, a key file
//! or the SSH agent ([`SshAuth`]), and listens on `127.0.0.1` with a free
//! port. Every local connection is forwarded over a `direct-tcpip` channel
//! to the target, so a database driver connects to [`Tunnel::local_addr`]
//! instead of the real server.
//!
//! [`TunnelManager`] shares one tunnel per connection profile and device and
//! counts its users; an unused tunnel closes after a grace period.
//!
//! # Local port security
//!
//! The local listener binds `127.0.0.1`, like `ssh -L`: it cannot be reached
//! from other machines, but **any local user or process can connect to it**
//! and reach the target through the SSH account. The database still checks
//! its own credentials. To keep that window small:
//!
//! - a [`Tunnel`] used on its own forwards until it is closed, dropped or its
//!   SSH session breaks;
//! - a tunnel shared through [`TunnelManager`] forwards new connections only
//!   while a [`TunnelLease`] is held. During the grace period after the last
//!   lease is released it resets every new connection without forwarding
//!   it; after the grace period the tunnel closes.
//!
//! Whenever a tunnel does not forward (no lease, closed, or broken) the port
//! stays bound and resets new connections, so no other local process can
//! bind it and receive the next driver connection with its credentials. The
//! port is released only when the [`Tunnel`] itself is dropped: for a shared
//! tunnel, after the last lease is gone and the tunnel was closed or
//! replaced. Connections forwarded earlier keep running until one side
//! closes them, or until the tunnel is closed or breaks.
//!
//! Secrets are passed as [`varve_secrets::Secret`] and only handed to the
//! SSH library. No error message contains a password, passphrase or key.

mod error;
mod keys;
mod known_hosts;
mod manager;
mod pkcs8_ec;
mod tunnel;

pub use error::{
    AuthMethod, HostKeyChange, KeyFileProblem, TimeoutStage, TunnelError, TunnelErrorKind,
};
pub use keys::{
    fingerprint, fingerprint_openssh, key_file_fingerprint, load_private_key, parse_public_key,
};
pub use known_hosts::{
    HostKeyPolicy, HostKeyStatus, KnownHostEntry, KnownHosts, KnownHostsFile, Marker, entry_line,
    lookup_name,
};
pub use manager::{TunnelKey, TunnelLease, TunnelManager};
pub use russh::keys::{PrivateKey, PublicKey};
pub use tunnel::{SshAuth, Tunnel, TunnelParams, TunnelStatus};
