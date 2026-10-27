//! Stored activity settings (design doc 9.3 and 9.5): how much statement
//! text the local log keeps, and how long events are kept. Both live in
//! `core_meta` so they survive restarts; retention is applied at startup and
//! once a day while the app runs.

use serde::{Deserialize, Serialize};
use varve_activity::{Retention, TextLevel};
use varve_store::{Store, StoreError};

const TEXT_LEVEL_KEY: &str = "activity.text_level";
const RETENTION_KEY: &str = "activity.retention_days";

/// Longest retention period that can be set (100 years); longer means
/// forever.
pub const MAX_RETENTION_DAYS: u32 = 36_500;

/// Activity log settings shown on the settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySettings {
    /// How much statement text is kept.
    pub text_level: TextLevel,
    /// Days to keep events; `None` keeps them forever (the default).
    pub retention_days: Option<u32>,
}

impl ActivitySettings {
    /// The retention period for the recorder.
    #[must_use]
    pub fn retention(&self) -> Retention {
        self.retention_days
            .map_or(Retention::Forever, Retention::Days)
    }
}

/// Reads the stored settings; missing or unreadable values fall back to the
/// defaults (normalized text, kept forever).
///
/// # Errors
/// Storage errors.
pub fn load(store: &Store) -> Result<ActivitySettings, StoreError> {
    let text_level = match meta_get(store, TEXT_LEVEL_KEY)?.as_deref() {
        Some("none") => TextLevel::None,
        Some("full") => TextLevel::Full,
        _ => TextLevel::Normalized,
    };
    let retention_days = meta_get(store, RETENTION_KEY)?
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|d| (1..=MAX_RETENTION_DAYS).contains(d));
    Ok(ActivitySettings {
        text_level,
        retention_days,
    })
}

/// Checks a settings change from the UI.
///
/// # Errors
/// A message for a retention period outside 1 to [`MAX_RETENTION_DAYS`] days.
pub fn validate(settings: &ActivitySettings) -> Result<(), String> {
    match settings.retention_days {
        Some(days) if !(1..=MAX_RETENTION_DAYS).contains(&days) => Err(format!(
            "Keep events for 1 to {MAX_RETENTION_DAYS} days, or forever."
        )),
        _ => Ok(()),
    }
}

/// Stores the settings.
///
/// # Errors
/// Storage errors.
pub fn save(store: &Store, settings: &ActivitySettings) -> Result<(), StoreError> {
    let level = match settings.text_level {
        TextLevel::None => "none",
        TextLevel::Normalized => "normalized",
        TextLevel::Full => "full",
    };
    let retention = settings
        .retention_days
        .map_or_else(|| "forever".to_string(), |d| d.to_string());
    store.with_conn(|c| {
        let tx = c.unchecked_transaction()?;
        for (key, value) in [(TEXT_LEVEL_KEY, level), (RETENTION_KEY, retention.as_str())] {
            tx.execute(
                "INSERT INTO core_meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

/// `core_meta` key of [`SecretSettings::remember_for_session`].
const REMEMBER_SECRETS_KEY: &str = "secrets.remember_for_session";

/// Settings of the in-memory secret cache (`varve_secrets::SecretCache`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretSettings {
    /// Keep each secret in memory after its first keychain read until the
    /// app quits or the user forgets them, so every session of a profile
    /// shares one keychain access. On by default.
    pub remember_for_session: bool,
}

impl Default for SecretSettings {
    fn default() -> Self {
        Self {
            remember_for_session: true,
        }
    }
}

/// Reads the stored secret settings. A missing value means the default
/// (remember). A value that is neither `true` nor `false` (a damaged row,
/// or a richer setting of a newer build) is logged and read as off: not
/// caching costs prompts, never a secret kept against the user's choice.
///
/// # Errors
/// Storage errors.
pub fn load_secrets(store: &Store) -> Result<SecretSettings, StoreError> {
    let remember_for_session = match meta_get(store, REMEMBER_SECRETS_KEY)?.as_deref() {
        None | Some("true") => true,
        Some("false") => false,
        Some(other) => {
            tracing::warn!(
                key = REMEMBER_SECRETS_KEY,
                value = other,
                "unknown stored value; secrets are not remembered until the setting is saved again"
            );
            false
        }
    };
    Ok(SecretSettings {
        remember_for_session,
    })
}

/// Stores the secret settings.
///
/// # Errors
/// Storage errors.
pub fn save_secrets(store: &Store, settings: &SecretSettings) -> Result<(), StoreError> {
    let value = if settings.remember_for_session {
        "true"
    } else {
        "false"
    };
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO core_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [REMEMBER_SECRETS_KEY, value],
        )?;
        Ok(())
    })
}

fn meta_get(store: &Store, key: &str) -> Result<Option<String>, StoreError> {
    store.with_conn(|c| {
        let mut stmt = c.prepare("SELECT value FROM core_meta WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Result<Store, StoreError> {
        let store = Store::open_in_memory()?;
        store.migrate(&[varve_store::core_migrations()])?;
        Ok(store)
    }

    #[test]
    fn defaults_keep_everything_normalized() -> Result<(), StoreError> {
        let store = store()?;
        assert_eq!(
            load(&store)?,
            ActivitySettings {
                text_level: TextLevel::Normalized,
                retention_days: None
            }
        );
        Ok(())
    }

    #[test]
    fn settings_round_trip() -> Result<(), StoreError> {
        let store = store()?;
        for settings in [
            ActivitySettings {
                text_level: TextLevel::Full,
                retention_days: Some(365),
            },
            ActivitySettings {
                text_level: TextLevel::None,
                retention_days: None,
            },
        ] {
            save(&store, &settings)?;
            assert_eq!(load(&store)?, settings);
        }
        Ok(())
    }

    #[test]
    fn out_of_range_retention_is_refused() {
        for days in [0, MAX_RETENTION_DAYS + 1] {
            assert!(
                validate(&ActivitySettings {
                    text_level: TextLevel::Normalized,
                    retention_days: Some(days),
                })
                .is_err()
            );
        }
    }

    #[test]
    fn serializes_for_the_ui() -> Result<(), serde_json::Error> {
        let json = serde_json::to_value(ActivitySettings {
            text_level: TextLevel::Normalized,
            retention_days: Some(90),
        })?;
        assert_eq!(
            json,
            serde_json::json!({ "textLevel": "normalized", "retentionDays": 90 })
        );
        Ok(())
    }

    #[test]
    fn secrets_are_remembered_by_default() -> Result<(), StoreError> {
        let store = store()?;
        assert!(load_secrets(&store)?.remember_for_session);
        save_secrets(
            &store,
            &SecretSettings {
                remember_for_session: false,
            },
        )?;
        assert!(!load_secrets(&store)?.remember_for_session);
        save_secrets(&store, &SecretSettings::default())?;
        assert!(load_secrets(&store)?.remember_for_session);
        Ok(())
    }

    #[test]
    fn an_unknown_stored_value_reads_as_off() -> Result<(), StoreError> {
        let store = store()?;
        store.with_conn(|c| {
            c.execute(
                "INSERT INTO core_meta (key, value) VALUES ('secrets.remember_for_session', 'maybe')",
                [],
            )?;
            Ok(())
        })?;
        assert!(!load_secrets(&store)?.remember_for_session);
        Ok(())
    }
}
