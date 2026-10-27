//! Which version of a connection profile a saved secret must have been
//! entered for.
//!
//! An edit that makes a kind of saved secret unusable for the profile
//! (another server, account or route) records, per kind, the profile
//! version it produced. `save_secret` refuses a secret entered for an older
//! version, so a password typed for the old server never gets listed for
//! the new one.
//!
//! Every connection has its own `core_meta` key
//! (`secrets.invalidated_at.<profile id>`), read and written only under the
//! profile's record lock. While an edit is being stored, its kinds hold a
//! pending mark with the version the edit started from and the value
//! before it. Whoever reads the key resolves pending marks against the
//! stored profile, so no outcome leaves a state the user cannot leave:
//!
//! - the profile moved past the start version: the edit landed, and the
//!   kind requires the current version;
//! - the profile is still at the start version: the edit did not happen,
//!   and the previous value comes back.
//!
//! A value that cannot be parsed is replaced by "every kind requires the
//! current version" (the user enters secrets again), so damage stays with
//! one connection. A read error of the store refuses the save (fail
//! closed). The one map of round 6 builds is moved into the per-profile
//! keys at start and then removed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use varve_records::SecretKindName;

use crate::commands::Services;
use crate::keychain_cleanup::{PendingDeletion, meta_get, meta_set};

/// Prefix of the per-profile key.
const PREFIX: &str = "secrets.invalidated_at.";

/// The single map of round 6 builds: profile id, then kind, then version.
const SHARED_KEY: &str = "secrets.invalidated_at";

/// The mark round 7 builds wrote while an edit was stored.
const ROUND7_IN_PROGRESS: i64 = i64::MAX;

/// Every kind of saved secret.
pub(crate) const ALL_KINDS: [SecretKindName; 4] = [
    SecretKindName::DbPassword,
    SecretKindName::SshPassword,
    SecretKindName::SshKeyPassphrase,
    SecretKindName::TlsKeyPassphrase,
];

/// What is stored for one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum Mark {
    /// Secrets must have been entered for this version or later.
    Since(i64),
    /// An edit from `pending_from` is being stored; `previous` is the value
    /// before it.
    #[serde(rename_all = "camelCase")]
    Pending {
        pending_from: i64,
        previous: Option<i64>,
    },
}

type Marks = BTreeMap<String, Mark>;

/// The resolved versions of one profile, by kind (`Debug` name).
pub(crate) type Versions = BTreeMap<String, i64>;

fn key_of(connection_id: &str) -> String {
    format!("{PREFIX}{connection_id}")
}

fn kind_key(kind: SecretKindName) -> String {
    format!("{kind:?}")
}

/// Resolves stored marks against the profile's current version. Returns
/// the versions and whether the stored marks need rewriting.
fn resolve(marks: &Marks, current: i64) -> (Versions, bool) {
    let mut versions = Versions::new();
    let mut changed = false;
    for (kind, mark) in marks {
        match *mark {
            Mark::Since(ROUND7_IN_PROGRESS) => {
                // A round 7 mark: whatever happened, secrets for the
                // current version belong to the current settings.
                versions.insert(kind.clone(), current);
                changed = true;
            }
            Mark::Since(version) => {
                versions.insert(kind.clone(), version);
            }
            Mark::Pending {
                pending_from,
                previous,
            } => {
                changed = true;
                if current > pending_from {
                    versions.insert(kind.clone(), current);
                } else if let Some(previous) = previous {
                    versions.insert(kind.clone(), previous);
                }
            }
        }
    }
    (versions, changed)
}

fn to_marks(versions: &Versions) -> Marks {
    versions
        .iter()
        .map(|(kind, version)| (kind.clone(), Mark::Since(*version)))
        .collect()
}

impl Services {
    fn write_marks(&self, connection_id: &str, marks: &Marks) -> Result<(), String> {
        let json = serde_json::to_string(marks).map_err(|e| e.to_string())?;
        meta_set(&self.store, &key_of(connection_id), &json)
    }

    /// The versions of a profile, with pending marks resolved against
    /// `current` (the stored profile's version) and written back. Call it
    /// under the profile's record lock.
    ///
    /// # Errors
    /// The store cannot be read (callers refuse saves then).
    pub(crate) fn secret_versions(
        &self,
        connection_id: &str,
        current: i64,
    ) -> Result<Versions, String> {
        let Some(text) = meta_get(&self.store, &key_of(connection_id))? else {
            return Ok(Versions::new());
        };
        let (versions, changed) = match serde_json::from_str::<Marks>(&text) {
            Ok(marks) => resolve(&marks, current),
            Err(error) => {
                tracing::warn!(
                    connection = connection_id,
                    %error,
                    "the saved-secret versions of a connection are damaged; secrets must be entered again"
                );
                (
                    ALL_KINDS.iter().map(|k| (kind_key(*k), current)).collect(),
                    true,
                )
            }
        };
        if changed && let Err(error) = self.write_marks(connection_id, &to_marks(&versions)) {
            tracing::warn!(%error, "could not write the resolved saved-secret versions");
        }
        Ok(versions)
    }

    /// The profile version from which on a secret of `kind` must have been
    /// entered, or `None` when no edit made that kind unusable. Takes the
    /// profile's record lock.
    ///
    /// # Errors
    /// The profile or the store cannot be read.
    pub fn secret_invalidated_at(
        &self,
        connection_id: &str,
        kind: SecretKindName,
    ) -> Result<Option<i64>, String> {
        self.with_profile_record(connection_id, || {
            let profile = self
                .records
                .connection(connection_id)
                .map_err(|e| e.to_string())?;
            Ok(self
                .secret_versions(connection_id, profile.version)?
                .get(&kind_key(kind))
                .copied())
        })
    }

    /// Marks `kinds` before an edit from version `from` is stored, and
    /// records the keychain entries of `unlisted` (the kinds the edit
    /// unlists) as pending removals in the same `app.db` transaction, so a
    /// crash after the edit always leaves them recorded. Returns the
    /// versions before the mark. Call it under the record lock.
    ///
    /// # Errors
    /// The store cannot be read or written: the edit must not go on.
    pub(crate) fn begin_secret_change(
        &self,
        connection_id: &str,
        kinds: &[SecretKindName],
        from: i64,
        unlisted: &[SecretKindName],
    ) -> Result<Versions, String> {
        let before = self.secret_versions(connection_id, from)?;
        let marks_json = if kinds.is_empty() {
            None
        } else {
            let mut marks = to_marks(&before);
            for kind in kinds {
                marks.insert(
                    kind_key(*kind),
                    Mark::Pending {
                        pending_from: from,
                        previous: before.get(&kind_key(*kind)).copied(),
                    },
                );
            }
            Some(serde_json::to_string(&marks).map_err(|e| e.to_string())?)
        };
        let key = key_of(connection_id);
        match (unlisted.is_empty(), &marks_json) {
            (true, None) => {}
            (true, Some(json)) => meta_set(&self.store, &key, json)?,
            (false, json) => self.queue_secret_deletions(
                &PendingDeletion::of(connection_id, unlisted),
                json.as_deref().map(|json| (key.as_str(), json)),
            )?,
        }
        Ok(before)
    }

    /// Records that the edit was stored as `version`. Call it under the
    /// record lock.
    ///
    /// # Errors
    /// The store cannot be written. The pending mark then resolves to the
    /// stored version on the next read, so saves still work.
    pub(crate) fn finish_secret_change(
        &self,
        connection_id: &str,
        kinds: &[SecretKindName],
        before: &Versions,
        version: i64,
    ) -> Result<(), String> {
        if kinds.is_empty() {
            return Ok(());
        }
        let mut versions = before.clone();
        for kind in kinds {
            versions.insert(kind_key(*kind), version);
        }
        self.write_marks(connection_id, &to_marks(&versions))
    }

    /// Puts the versions back after an edit that was not stored. Call it
    /// under the record lock.
    ///
    /// # Errors
    /// The store cannot be written. The pending mark then resolves to the
    /// previous values on the next read (the profile kept its version).
    pub(crate) fn abandon_secret_change(
        &self,
        connection_id: &str,
        before: &Versions,
    ) -> Result<(), String> {
        self.write_marks(connection_id, &to_marks(before))
    }

    /// Drops the versions of a deleted profile. Call it under the record
    /// lock.
    pub(crate) fn forget_secret_versions(&self, connection_id: &str) {
        let key = key_of(connection_id);
        let removed = self.store.with_conn(|c| {
            c.execute("DELETE FROM core_meta WHERE key = ?1", [key.as_str()])?;
            Ok(())
        });
        if let Err(error) = removed {
            tracing::warn!(%error, "could not drop the secret versions of a deleted connection");
        }
    }

    /// Moves the one map of round 6 builds into the per-profile keys (the
    /// higher version wins) and removes it. Runs once at start, before any
    /// command. A damaged map is logged and removed: the connections it
    /// covered then accept secrets for any version, as before round 6.
    pub fn migrate_shared_secret_versions(&self) {
        let text = match meta_get(&self.store, SHARED_KEY) {
            Ok(Some(text)) => text,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "could not read the old saved-secret versions");
                return;
            }
        };
        match serde_json::from_str::<BTreeMap<String, BTreeMap<String, i64>>>(&text) {
            Ok(all) => {
                for (connection_id, kinds) in all {
                    self.with_profile_record(&connection_id, || {
                        let Ok(profile) = self.records.connection(&connection_id) else {
                            return;
                        };
                        let Ok(mut versions) =
                            self.secret_versions(&connection_id, profile.version)
                        else {
                            return;
                        };
                        for (kind, version) in kinds {
                            let version = if version == ROUND7_IN_PROGRESS {
                                profile.version
                            } else {
                                version
                            };
                            let slot = versions.entry(kind).or_insert(version);
                            *slot = (*slot).max(version);
                        }
                        if let Err(error) = self.write_marks(&connection_id, &to_marks(&versions)) {
                            tracing::warn!(%error, "could not move old saved-secret versions");
                        }
                    });
                }
            }
            Err(error) => {
                tracing::warn!(%error, "the old saved-secret versions are damaged; removing them");
            }
        }
        let removed = self.store.with_conn(|c| {
            c.execute("DELETE FROM core_meta WHERE key = ?1", [SHARED_KEY])?;
            Ok(())
        });
        if let Err(error) = removed {
            tracing::warn!(%error, "could not remove the old saved-secret versions");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(json: &str) -> Result<Marks, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn pending_marks_resolve_against_the_stored_version() -> Result<(), serde_json::Error> {
        // The edit landed: the current version is required.
        let (v, changed) = resolve(
            &marks(r#"{"DbPassword":{"pendingFrom":3,"previous":1}}"#)?,
            4,
        );
        assert_eq!(v.get("DbPassword"), Some(&4));
        assert!(changed);
        // It did not: the previous value comes back, or nothing.
        let (v, _) = resolve(
            &marks(r#"{"DbPassword":{"pendingFrom":3,"previous":1}}"#)?,
            3,
        );
        assert_eq!(v.get("DbPassword"), Some(&1));
        let (v, _) = resolve(
            &marks(r#"{"DbPassword":{"pendingFrom":3,"previous":null}}"#)?,
            3,
        );
        assert_eq!(v.get("DbPassword"), None);
        // Plain versions stay; a round 7 mark becomes the current version.
        let (v, changed) = resolve(&marks(r#"{"DbPassword":2}"#)?, 5);
        assert_eq!(v.get("DbPassword"), Some(&2));
        assert!(!changed);
        let (v, changed) = resolve(&marks(&format!(r#"{{"DbPassword":{}}}"#, i64::MAX))?, 5);
        assert_eq!(v.get("DbPassword"), Some(&5));
        assert!(changed);
        Ok(())
    }
}
