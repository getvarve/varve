//! Shared types for Varve.
//!
//! Every other crate depends on this one, so it stays small: the supported
//! database engines, capability identifiers, the app edition and the common
//! error type.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A database engine Varve can connect to.
///
/// MySQL and MariaDB share a wire protocol but are separate engines everywhere
/// above the wire (design doc section 2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// PostgreSQL 13 and newer.
    Postgres,
    /// MySQL 8.0 and newer.
    Mysql,
    /// MariaDB 10.6 and newer.
    Mariadb,
    /// Redis 6.2 and newer.
    Redis,
}

impl Engine {
    /// All engines, in display order.
    pub const ALL: [Engine; 4] = [
        Engine::Postgres,
        Engine::Mysql,
        Engine::Mariadb,
        Engine::Redis,
    ];

    /// Human readable name.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::Mysql => "MySQL",
            Engine::Mariadb => "MariaDB",
            Engine::Redis => "Redis",
        }
    }

    /// Two-letter monogram used on engine tiles in the UI.
    #[must_use]
    pub fn monogram(self) -> &'static str {
        match self {
            Engine::Postgres => "PG",
            Engine::Mysql => "MY",
            Engine::Mariadb => "MA",
            Engine::Redis => "RD",
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Which build of the app is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edition {
    /// Built from the public repository only.
    Community,
    /// The official build with closed extensions.
    Pro,
}

impl Edition {
    /// Product name shown in the UI.
    #[must_use]
    pub fn product_name(self) -> &'static str {
        match self {
            Edition::Community => "Varve Community",
            Edition::Pro => "Varve Pro",
        }
    }
}

/// A dotted, lowercase identifier such as `pro.stats.history`.
///
/// Used for capabilities, extension ids, command names and activity actions.
/// Segments are `[a-z0-9_]+`, separated by single dots.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DottedId(String);

impl DottedId {
    /// Parses and validates an identifier.
    ///
    /// # Errors
    /// Returns [`Error::InvalidId`] when the value is empty, has empty
    /// segments or contains characters outside `[a-z0-9_.]`.
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.split('.').all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(Error::InvalidId(value))
        }
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True when this id equals `prefix` or starts with `prefix.`.
    #[must_use]
    pub fn is_within(&self, prefix: &DottedId) -> bool {
        self.0 == prefix.0
            || (self.0.len() > prefix.0.len()
                && self.0.starts_with(&prefix.0)
                && self.0.as_bytes()[prefix.0.len()] == b'.')
    }

    /// The id with dots replaced by underscores, for use in SQL names.
    #[must_use]
    pub fn to_sql_prefix(&self) -> String {
        self.0.replace('.', "_")
    }
}

impl fmt::Display for DottedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for DottedId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<DottedId> for String {
    fn from(value: DottedId) -> Self {
        value.0
    }
}

/// Errors shared across Varve crates.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An identifier did not match the dotted id rules.
    #[error("invalid identifier `{0}`: use lowercase segments of a-z, 0-9 and _ separated by dots")]
    InvalidId(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_id_accepts_valid_values() {
        for value in ["core", "pro.stats", "pro.stats.history_v2", "a1.b_2"] {
            assert!(DottedId::new(value).is_ok(), "{value} should be valid");
        }
    }

    #[test]
    fn dotted_id_rejects_invalid_values() {
        for value in [
            "",
            ".",
            "pro.",
            ".pro",
            "pro..stats",
            "Pro.stats",
            "pro-x",
            "pro stats",
        ] {
            assert!(DottedId::new(value).is_err(), "{value:?} should be invalid");
        }
    }

    #[test]
    fn is_within_respects_segment_boundaries() -> Result<(), Error> {
        let prefix = DottedId::new("pro.test")?;
        assert!(DottedId::new("pro.test")?.is_within(&prefix));
        assert!(DottedId::new("pro.test.ping")?.is_within(&prefix));
        assert!(!DottedId::new("pro.tests.ping")?.is_within(&prefix));
        assert!(!DottedId::new("pro")?.is_within(&prefix));
        Ok(())
    }

    #[test]
    fn sql_prefix_replaces_dots() -> Result<(), Error> {
        assert_eq!(DottedId::new("pro.test")?.to_sql_prefix(), "pro_test");
        Ok(())
    }

    #[test]
    fn dotted_id_round_trips_through_serde() -> Result<(), Box<dyn std::error::Error>> {
        let id = DottedId::new("pro.sync")?;
        let json = serde_json::to_string(&id)?;
        assert_eq!(json, "\"pro.sync\"");
        let back: DottedId = serde_json::from_str(&json)?;
        assert_eq!(back, id);
        assert!(serde_json::from_str::<DottedId>("\"Bad.Id\"").is_err());
        Ok(())
    }

    #[test]
    fn engine_names() {
        assert_eq!(Engine::Postgres.to_string(), "PostgreSQL");
        assert_eq!(Engine::Mariadb.monogram(), "MA");
        assert_eq!(Engine::ALL.len(), 4);
    }
}
