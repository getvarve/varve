//! Session settings for one call, for replaying DDL as it was created
//! (see [`crate::MysqlSession::execute_with_session`]).

use varve_driver::{DriverError, ErrorKind};

/// Session settings that apply to one call only.
///
/// Every field is optional; `None` keeps the session's value. The values are
/// checked before anything runs: character sets and collations against the
/// server's `information_schema`, `sql_mode` against the known mode names,
/// and the time zone as an offset (`+02:00`), `SYSTEM`, or a named zone the
/// server knows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionOverrides {
    /// `character_set_client`: how the server reads the statement text.
    /// UTF-8 SQL is transcoded for single-byte character sets (latin1,
    /// cp1252 and the like); non-ASCII SQL under other multibyte character
    /// sets is refused. Results stay utf8mb4.
    pub character_set_client: Option<String>,
    /// `collation_connection` (also sets `character_set_connection`).
    pub collation_connection: Option<String>,
    /// `sql_mode`, as a comma separated list of mode names (may be empty).
    pub sql_mode: Option<String>,
    /// `time_zone`.
    pub time_zone: Option<String>,
}

/// Character sets in which an ASCII quote, backslash or backtick byte can
/// be part of a multibyte character (or that are not ASCII based at all),
/// so the lexer cannot find statement boundaries in text sent with them.
const LEXER_UNSAFE: [&str; 9] = [
    "big5", "cp932", "gbk", "gb18030", "sjis", "ucs2", "utf16", "utf16le", "utf32",
];

/// Single-byte character sets that put letters on ASCII punctuation bytes.
const NOT_ASCII_COMPATIBLE: [&str; 1] = ["swe7"];

/// The `sql_mode` names of MySQL and MariaDB.
const SQL_MODES: [&str; 37] = [
    "ALLOW_INVALID_DATES",
    "ANSI",
    "ANSI_QUOTES",
    "DB2",
    "EMPTY_STRING_IS_NULL",
    "ERROR_FOR_DIVISION_BY_ZERO",
    "HIGH_NOT_PRECEDENCE",
    "IGNORE_BAD_TABLE_OPTIONS",
    "IGNORE_SPACE",
    "MAXDB",
    "MSSQL",
    "MYSQL323",
    "MYSQL40",
    "NO_AUTO_CREATE_USER",
    "NO_AUTO_VALUE_ON_ZERO",
    "NO_BACKSLASH_ESCAPES",
    "NO_DIR_IN_CREATE",
    "NO_ENGINE_SUBSTITUTION",
    "NO_FIELD_OPTIONS",
    "NO_KEY_OPTIONS",
    "NO_TABLE_OPTIONS",
    "NO_UNSIGNED_SUBTRACTION",
    "NO_ZERO_DATE",
    "NO_ZERO_IN_DATE",
    "ONLY_FULL_GROUP_BY",
    "ORACLE",
    "PAD_CHAR_TO_FULL_LENGTH",
    "PIPES_AS_CONCAT",
    "POSTGRESQL",
    "REAL_AS_FLOAT",
    "SIMULTANEOUS_ASSIGNMENT",
    "STRICT_ALL_TABLES",
    "STRICT_TRANS_TABLES",
    "TIME_ROUND_FRACTIONAL",
    "TIME_TRUNCATE_FRACTIONAL",
    "TRADITIONAL",
    "NO_NULL_NAME",
];

fn unsupported(message: String) -> DriverError {
    DriverError::new(ErrorKind::Unsupported, message)
}

/// A character set or collation name: letters, digits and `_` only.
fn plain_name(value: &str, what: &str) -> Result<String, DriverError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(unsupported(format!("{what} {value:?} is not a valid name")));
    }
    Ok(value.to_ascii_lowercase())
}

/// The character set a collation belongs to, by its name prefix.
pub(crate) fn collation_charset(collation: &str) -> &str {
    collation.split('_').next().unwrap_or(collation)
}

fn lexer_safe(charset: &str, what: &str) -> Result<(), DriverError> {
    if NOT_ASCII_COMPATIBLE.contains(&charset) {
        return Err(unsupported(format!(
            "{what} {charset} is not supported: it puts letters on ASCII punctuation bytes, so \
             Varve cannot find the statement boundaries"
        )));
    }
    if LEXER_UNSAFE.contains(&charset) {
        return Err(unsupported(format!(
            "{what} {charset} is not supported: in it a quote, backslash or backtick byte can \
             be part of a multibyte character, so Varve cannot find the statement boundaries"
        )));
    }
    Ok(())
}

/// The checked overrides, ready to be applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Checked {
    pub character_set_client: Option<String>,
    pub collation_connection: Option<String>,
    pub sql_mode: Option<String>,
    pub time_zone: Option<String>,
}

impl Checked {
    /// The `SET` statement that applies them, or `None` when empty.
    pub(crate) fn apply_statement(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(client) = &self.character_set_client {
            parts.push(format!("character_set_client = '{client}'"));
        }
        if let Some(collation) = &self.collation_connection {
            parts.push(format!("collation_connection = '{collation}'"));
        }
        if let Some(mode) = &self.sql_mode {
            parts.push(format!("sql_mode = '{mode}'"));
        }
        if let Some(zone) = &self.time_zone {
            parts.push(format!("time_zone = '{zone}'"));
        }
        (!parts.is_empty()).then(|| format!("SET {}", parts.join(", ")))
    }
}

/// Checks the form of the overrides; the server check and the encoding
/// of the SQL follow in the session.
pub(crate) fn check(overrides: &SessionOverrides) -> Result<Checked, DriverError> {
    let mut checked = Checked::default();
    if let Some(client) = &overrides.character_set_client {
        let client = plain_name(client, "the character set")?;
        lexer_safe(&client, "the client character set")?;
        checked.character_set_client = Some(client);
    }
    if let Some(collation) = &overrides.collation_connection {
        let collation = plain_name(collation, "the collation")?;
        lexer_safe(
            collation_charset(&collation),
            "the connection character set",
        )?;
        checked.collation_connection = Some(collation);
    }
    if let Some(mode) = &overrides.sql_mode {
        let words: Vec<String> = mode
            .split(',')
            .map(|word| word.trim().to_ascii_uppercase())
            .filter(|word| !word.is_empty())
            .collect();
        if let Some(unknown) = words
            .iter()
            .find(|word| !SQL_MODES.contains(&word.as_str()))
        {
            return Err(unsupported(format!("{unknown} is not an sql_mode")));
        }
        checked.sql_mode = Some(words.join(","));
    }
    if let Some(zone) = &overrides.time_zone {
        let zone = zone.trim();
        let offset = zone
            .strip_prefix(['+', '-'])
            .and_then(|rest| rest.split_once(':'))
            .is_some_and(|(hours, minutes)| {
                (1..=2).contains(&hours.len())
                    && minutes.len() == 2
                    && hours
                        .bytes()
                        .chain(minutes.bytes())
                        .all(|b| b.is_ascii_digit())
            });
        let named = zone.len() <= 64
            && zone.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
            && zone
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/' | b'+' | b'-'));
        if !offset && !named {
            return Err(unsupported(format!("{zone:?} is not a time zone")));
        }
        checked.time_zone = Some(zone.to_owned());
    }
    Ok(checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides() -> SessionOverrides {
        SessionOverrides::default()
    }

    #[test]
    fn checks_the_form() -> Result<(), DriverError> {
        let checked = check(&SessionOverrides {
            character_set_client: Some("latin1".into()),
            collation_connection: Some("latin1_swedish_ci".into()),
            sql_mode: Some("strict_trans_tables, NO_ENGINE_SUBSTITUTION".into()),
            time_zone: Some("+02:00".into()),
        })?;
        assert_eq!(
            checked.apply_statement().as_deref(),
            Some(
                "SET character_set_client = 'latin1', \
                 collation_connection = 'latin1_swedish_ci', \
                 sql_mode = 'STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION', time_zone = '+02:00'"
            )
        );
        assert_eq!(check(&overrides())?.apply_statement(), None);
        let empty_mode = SessionOverrides {
            sql_mode: Some(String::new()),
            ..overrides()
        };
        assert_eq!(
            check(&empty_mode)?.apply_statement().as_deref(),
            Some("SET sql_mode = ''")
        );
        for zone in ["Europe/Amsterdam", "SYSTEM", "-5:30", "UTC"] {
            let zone_only = SessionOverrides {
                time_zone: Some(zone.into()),
                ..overrides()
            };
            assert!(check(&zone_only).is_ok(), "{zone}");
        }
        Ok(())
    }

    #[test]
    fn refuses_what_the_lexer_cannot_read() {
        for (field, value) in [
            ("client", "gbk"),
            ("client", "SJIS"),
            ("collation", "gbk_bin"),
        ] {
            let mut bad = overrides();
            match field {
                "client" => bad.character_set_client = Some(value.into()),
                _ => bad.collation_connection = Some(value.into()),
            }
            let error = check(&bad).err();
            assert!(
                error.is_some_and(
                    |e| e.kind == ErrorKind::Unsupported && e.message.contains("multibyte")
                ),
                "{value}"
            );
        }
        let latin1 = SessionOverrides {
            character_set_client: Some("latin1".into()),
            ..overrides()
        };
        assert!(check(&latin1).is_ok());
        let swe7 = SessionOverrides {
            character_set_client: Some("swe7".into()),
            ..overrides()
        };
        assert!(check(&swe7).is_err());
    }

    #[test]
    fn refuses_bad_values() {
        for bad in [
            SessionOverrides {
                character_set_client: Some("latin1'; DROP".into()),
                ..overrides()
            },
            SessionOverrides {
                sql_mode: Some("STRICT_TRANS_TABLES,NOPE".into()),
                ..overrides()
            },
            SessionOverrides {
                time_zone: Some("+2".into()),
                ..overrides()
            },
            SessionOverrides {
                time_zone: Some("Europe/Amster'dam".into()),
                ..overrides()
            },
        ] {
            assert!(check(&bad).is_err(), "{bad:?}");
        }
    }
}
