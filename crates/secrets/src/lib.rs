//! OS keychain access for Varve.
//!
//! Passwords, passphrases and keys live only in the operating system's
//! credential store (macOS Keychain, Windows Credential Manager, Secret
//! Service on Linux). They are never written to `app.db`, never logged and
//! never synced (design doc section 3.1).
//!
//! Entries use the service `varve` and the account
//! `<profile id>/<secret kind>`, for example `5f0c.../db_password`.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

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

#[cfg(test)]
mod tests {
    use super::*;

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
