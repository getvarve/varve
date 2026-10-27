//! SSH tunnels for connection profiles (design doc 3.1 and 3.3).
//!
//! Host keys are checked strictly against the app's own known_hosts file.
//! An unknown or changed key stops the connection with the fingerprints, the
//! UI asks the user, and [`Tunnels::trust_host_key`] records the key.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use varve_activity::{ActivityEvent, ActivityEventBuilder, ErrorDetails, Recorder, TunnelDetails};
use varve_records::{ConnectionProfile, SecretKindName, SshAuthMethod, TunnelEndpoint};
use varve_secrets::{Secret, SecretKey, SecretKind, SecretStore};
use varve_tunnel::{
    HostKeyPolicy, KeyFileProblem, KnownHosts, SshAuth, TunnelError, TunnelErrorKind, TunnelKey,
    TunnelLease, TunnelManager, TunnelParams, parse_public_key,
};

use crate::commands::CmdError;

/// Shared tunnels of this device.
pub struct Tunnels {
    manager: TunnelManager,
    known_hosts_path: PathBuf,
    device_id: String,
    /// Records `tunnel.opened` and `tunnel.failed` (design doc 9.2).
    activity: Arc<Recorder>,
}

impl std::fmt::Debug for Tunnels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tunnels")
            .field("known_hosts_path", &self.known_hosts_path)
            .finish_non_exhaustive()
    }
}

/// A tunnel lease kept alive by a session; dropping it releases the tunnel.
pub struct LeaseGuard {
    _lease: TunnelLease,
    endpoint: TunnelEndpoint,
}

impl LeaseGuard {
    /// Local endpoint to connect the driver to.
    #[must_use]
    pub fn endpoint(&self) -> &TunnelEndpoint {
        &self.endpoint
    }
}

fn tunnel_error(error: &TunnelError) -> CmdError {
    let (code, fingerprints, presented_key, host) = match error {
        TunnelError::HostKeyUnknown {
            host,
            port,
            fingerprint,
            presented_key,
            ..
        } => (
            "host_key_unknown",
            vec![fingerprint.clone()],
            Some(presented_key.clone()),
            Some((host.clone(), *port)),
        ),
        TunnelError::HostKeyChanged(change) => (
            "host_key_changed",
            vec![
                change.expected_fingerprint.clone(),
                change.actual_fingerprint.clone(),
            ],
            Some(change.presented_key.clone()),
            Some((change.host.clone(), change.port)),
        ),
        other => (kind_code(other.kind()), Vec::new(), None, None),
    };
    let (host, port) = host.map_or((None, None), |(h, p)| (Some(h), Some(p)));
    CmdError::Tunnel {
        message: error.to_string(),
        code: code.to_string(),
        fingerprints,
        presented_key,
        host,
        port,
    }
}

fn kind_code(kind: TunnelErrorKind) -> &'static str {
    match kind {
        TunnelErrorKind::Connect => "connect",
        TunnelErrorKind::Auth => "auth",
        TunnelErrorKind::KeyFile => "key_file",
        TunnelErrorKind::InvalidPublicKey => "invalid_public_key",
        TunnelErrorKind::Agent => "agent",
        TunnelErrorKind::HostKeyUnknown => "host_key_unknown",
        TunnelErrorKind::HostKeyChanged => "host_key_changed",
        TunnelErrorKind::HostKeyRevoked => "host_key_revoked",
        TunnelErrorKind::Forward => "forward",
        TunnelErrorKind::Timeout => "timeout",
        TunnelErrorKind::Closed => "closed",
        TunnelErrorKind::Unsupported => "unsupported",
        TunnelErrorKind::Io => "io",
    }
}

impl Tunnels {
    /// Creates the tunnel manager with the app's known_hosts file; tunnel
    /// events go to `activity`.
    #[must_use]
    pub fn new(known_hosts_path: PathBuf, device_id: String, activity: Arc<Recorder>) -> Self {
        Self {
            manager: TunnelManager::new(),
            known_hosts_path,
            device_id,
            activity,
        }
    }

    /// Records a tunnel event of `profile` (an unsaved editor test has no
    /// connection to link it to).
    fn record(&self, profile: &ConnectionProfile, builder: ActivityEventBuilder) {
        let builder = if profile.id == "unsaved" {
            builder
        } else {
            builder.connection(profile.id.clone())
        };
        match builder.build() {
            Ok(event) => {
                if let Err(error) = self.activity.record(event) {
                    tracing::warn!(%error, "could not record a tunnel event");
                }
            }
            Err(error) => tracing::warn!(%error, "invalid tunnel event"),
        }
    }

    fn known_hosts(&self) -> KnownHosts {
        KnownHosts::new(self.known_hosts_path.clone(), HostKeyPolicy::Strict)
    }

    fn keychain_secret(
        profile: &ConnectionProfile,
        secrets: &dyn SecretStore,
        kind: SecretKindName,
    ) -> Result<Option<Secret>, CmdError> {
        if !profile.saved_secrets.contains(&kind) {
            return Ok(None);
        }
        let key = SecretKey::new(profile.id.clone(), SecretKind::from(kind))?;
        Ok(secrets.get(&key)?)
    }

    /// Opens (or reuses) the tunnel of a profile. `None` when the profile
    /// does not use SSH. `typed` is a password or passphrase from the prompt.
    ///
    /// # Errors
    /// [`CmdError::SecretRequired`] when a password is needed and not
    /// available, or [`CmdError::Tunnel`] with the reason.
    pub async fn lease_for(
        &self,
        profile: &ConnectionProfile,
        secrets: &dyn SecretStore,
        typed: Option<Secret>,
    ) -> Result<Option<LeaseGuard>, CmdError> {
        let ssh = &profile.ssh;
        if !ssh.enabled {
            return Ok(None);
        }
        let auth = match ssh.auth {
            SshAuthMethod::Password => {
                let password = match typed {
                    Some(secret) => secret,
                    None => Self::keychain_secret(profile, secrets, SecretKindName::SshPassword)?
                        .ok_or(CmdError::SecretRequired {
                        secret: SecretKindName::SshPassword,
                    })?,
                };
                SshAuth::Password(password)
            }
            SshAuthMethod::KeyFile => {
                let Some(path) = ssh.key_path.clone() else {
                    return Err(CmdError::Invalid {
                        message: "Choose the SSH private key file.".into(),
                    });
                };
                let passphrase = match typed {
                    Some(secret) => Some(secret),
                    None => {
                        Self::keychain_secret(profile, secrets, SecretKindName::SshKeyPassphrase)?
                    }
                };
                SshAuth::KeyFile {
                    path: PathBuf::from(path),
                    passphrase,
                }
            }
            SshAuthMethod::Agent => SshAuth::Agent,
        };
        let mut params = TunnelParams::new(
            ssh.host.clone(),
            ssh.username.clone(),
            auth,
            profile.host.clone(),
            profile.port,
            self.known_hosts(),
        );
        params.ssh_port = ssh.port;
        params.keepalive =
            Duration::from_secs(u64::from(profile.options.keepalive_s.unwrap_or(15)));
        let details = TunnelDetails {
            ssh_host: ssh.host.clone(),
            ssh_port: ssh.port,
            ssh_username: Some(ssh.username.clone()).filter(|u| !u.is_empty()),
            auth_method: Some(
                match ssh.auth {
                    SshAuthMethod::Password => "password",
                    SshAuthMethod::KeyFile => "key",
                    SshAuthMethod::Agent => "agent",
                }
                .to_string(),
            ),
        };
        let lease = match self
            .manager
            .acquire(
                TunnelKey::new(profile.id.clone(), self.device_id.clone()),
                params,
            )
            .await
        {
            Ok(lease) => lease,
            // Not a failure: the UI asks for the passphrase and tries again.
            Err(TunnelError::KeyFile {
                problem: KeyFileProblem::PassphraseRequired,
                ..
            }) => {
                return Err(CmdError::SecretRequired {
                    secret: SecretKindName::SshKeyPassphrase,
                });
            }
            Err(error) => {
                let cmd = tunnel_error(&error);
                let code = match &cmd {
                    CmdError::Tunnel { code, .. } => Some(code.clone()),
                    _ => None,
                };
                self.record(
                    profile,
                    ActivityEvent::tunnel_failed(
                        &details,
                        &ErrorDetails {
                            code,
                            message: error.to_string(),
                        },
                    ),
                );
                return Err(cmd);
            }
        };
        // A tunnel shared with the profile's other sessions was opened once.
        if lease.opened() {
            self.record(profile, ActivityEvent::tunnel_opened(&details));
        }
        let addr = lease.local_addr();
        Ok(Some(LeaseGuard {
            endpoint: TunnelEndpoint {
                host: addr.ip().to_string(),
                port: addr.port(),
            },
            _lease: lease,
        }))
    }

    /// Records the host key the user accepted for a profile's SSH server,
    /// replacing any key recorded before.
    ///
    /// # Errors
    /// An invalid key or a file error.
    pub async fn trust_host_key(
        &self,
        host: &str,
        port: u16,
        presented_key: &str,
    ) -> Result<(), CmdError> {
        let key = parse_public_key(presented_key).map_err(|e| tunnel_error(&e))?;
        let known_hosts = self.known_hosts();
        let host = host.to_string();
        // Writing known_hosts reads, rewrites and syncs a file: keep it off
        // the async runtime.
        tokio::task::spawn_blocking(move || known_hosts.trust(&host, port, &key))
            .await
            .map_err(|e| CmdError::Internal {
                message: format!("trusting the host key stopped: {e}"),
            })?
            .map_err(|e| tunnel_error(&e))
    }
}
