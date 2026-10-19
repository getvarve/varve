//! Turns a connection profile into driver connect parameters.

use std::time::Duration;

use varve_core::Engine;
use varve_driver::{ConnectParams, TlsParams};
use varve_secrets::{Secret, SecretKey, SecretKind, SecretStore};

use crate::model::{ConnectionProfile, SecretKindName};
use crate::repo::RecordError;

/// Where to actually connect when the profile uses an SSH tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelEndpoint {
    /// Local host of the tunnel, normally `127.0.0.1`.
    pub host: String,
    /// Local port of the tunnel.
    pub port: u16,
}

/// Builds [`ConnectParams`] for a profile.
///
/// The password comes from `password` when given (the prompt), otherwise
/// from the keychain when the profile says it is saved there. When the
/// connection runs through a tunnel, `tunnel` replaces host and port and the
/// original host is kept for TLS host name checks.
///
/// # Errors
/// Keychain errors, or a profile id that cannot address the keychain.
pub fn connect_params(
    profile: &ConnectionProfile,
    secrets: &dyn SecretStore,
    password: Option<Secret>,
    tunnel: Option<&TunnelEndpoint>,
) -> Result<ConnectParams, RecordError> {
    let password = match password {
        Some(password) => Some(password),
        None if profile.saved_secrets.contains(&SecretKindName::DbPassword) => {
            let key = SecretKey::new(profile.id.clone(), SecretKind::DbPassword)
                .map_err(|e| RecordError::Invalid(e.to_string()))?;
            secrets
                .get(&key)
                .map_err(|e| RecordError::Invalid(e.to_string()))?
        }
        None => None,
    };

    let (host, port, server_name) = match tunnel {
        Some(endpoint) => (
            endpoint.host.clone(),
            endpoint.port,
            Some(profile.host.clone()),
        ),
        None => (profile.host.clone(), profile.port, None),
    };

    let mut params = ConnectParams::new(profile.engine, host, port);
    params.database = profile.database.clone();
    params.username = profile.username.clone();
    params.password = password;
    params.tls = TlsParams {
        mode: profile.tls.mode,
        ca_path: profile.tls.ca_path.clone(),
        client_cert_path: profile.tls.client_cert_path.clone(),
        client_key_path: profile.tls.client_key_path.clone(),
        server_name,
    };
    params.redis_db = if profile.engine == Engine::Redis {
        profile.options.redis_db
    } else {
        0
    };
    params.statement_timeout = profile
        .options
        .statement_timeout_ms
        .map(Duration::from_millis);
    params.read_only = profile.options.read_only;
    params.init_sql = profile.options.init_sql.clone();
    Ok(params)
}
