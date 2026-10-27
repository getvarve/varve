//! The session header of MySQL and MariaDB DDL.
//!
//! Views, routines, triggers, events and MariaDB packages keep the
//! character set, collations, SQL mode and (events) time zone of the session
//! that created them, and read their own text with those settings. DDL that
//! re-creates such an object must run with the same settings, or the
//! object changes (a procedure created under `SET NAMES latin1` would come
//! back as `utf8mb4`). `mysqldump` sets them with statements around each
//! `CREATE`; Varve sessions do not accept those statements in scripts, so
//! the DDL carries them as a header on its first line instead:
//!
//! ```text
//! -- varve-session: character_set_client=latin1; collation_connection=latin1_swedish_ci
//! CREATE DEFINER=`root`@`%` PROCEDURE `db`.`p`() ...
//! ```
//!
//! The header is only written when an object's values differ from the
//! session the DDL was read in, and it names only the settings that
//! differ. Whoever runs the DDL reads it with [`ddl_session_header`],
//! applies the settings for the duration of the script (the MySQL driver's
//! dedicated path), runs [`strip_ddl_session_header`]'s result and restores
//! the session. To the server the header is a plain comment, so running
//! the text unchanged still works, only with the session's own settings.
//!
//! Keys, each at most once, in any order:
//!
//! - `character_set_client`, `collation_connection`, `sql_mode`,
//!   `time_zone`: session variables to set while the script runs;
//! - `character_set_results`: recorded for completeness (it is what
//!   `mysqldump` sets next to the client character set), but not applied:
//!   it only says how the server encodes result text sent back to this
//!   session, not how the object is stored, and Varve always reads results
//!   as utf8mb4, so the MySQL driver keeps it at utf8mb4;
//! - `collation_database`: the default collation the object's database had
//!   when the object was created (routines, triggers, events and packages
//!   keep it). Apply it with `ALTER DATABASE <the object's database>
//!   COLLATE <value>` before the script and restore the database's own
//!   collation afterwards.
//!
//! Values are non-empty (except `sql_mode`, which may be empty: all
//! options off) and use only letters, digits and `_ , + - : / .`
//! (character set, collation and time zone names, `sql_mode` lists, UTC
//! offsets). Anything else, an unknown key or a repeated key is an error.

use serde::{Deserialize, Serialize};

use crate::model::{CatalogError, CatalogResult};

/// The first characters of a DDL session header line.
pub const DDL_SESSION_PREFIX: &str = "-- varve-session: ";

/// Session settings a piece of DDL must run with; `None` fields keep the
/// session's value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DdlSession {
    /// `character_set_client`.
    pub character_set_client: Option<String>,
    /// `character_set_results`. Recorded, not applied: results stay
    /// utf8mb4 (see the module docs).
    pub character_set_results: Option<String>,
    /// `collation_connection`.
    pub collation_connection: Option<String>,
    /// `sql_mode`.
    pub sql_mode: Option<String>,
    /// `time_zone`.
    pub time_zone: Option<String>,
    /// Default collation of the object's database while the DDL runs.
    pub collation_database: Option<String>,
}

impl DdlSession {
    /// True when no setting is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs().is_empty()
    }

    /// The settings that are set, as `(key, value)` in header order.
    #[must_use]
    pub fn pairs(&self) -> Vec<(&'static str, &str)> {
        [
            ("character_set_client", &self.character_set_client),
            ("character_set_results", &self.character_set_results),
            ("collation_connection", &self.collation_connection),
            ("sql_mode", &self.sql_mode),
            ("time_zone", &self.time_zone),
            ("collation_database", &self.collation_database),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.as_deref().map(|v| (key, v)))
        .collect()
    }

    fn slot(&mut self, key: &str) -> Option<&mut Option<String>> {
        Some(match key {
            "character_set_client" => &mut self.character_set_client,
            "character_set_results" => &mut self.character_set_results,
            "collation_connection" => &mut self.collation_connection,
            "sql_mode" => &mut self.sql_mode,
            "time_zone" => &mut self.time_zone,
            "collation_database" => &mut self.collation_database,
            _ => return None,
        })
    }

    /// The header line (without the line break), or `None` when nothing is
    /// set.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Invalid`] for a value the header cannot carry.
    pub fn header(&self) -> CatalogResult<Option<String>> {
        let pairs = self.pairs();
        if pairs.is_empty() {
            return Ok(None);
        }
        let mut parts = Vec::with_capacity(pairs.len());
        for (key, value) in pairs {
            check_value(key, value)?;
            parts.push(format!("{key}={value}"));
        }
        Ok(Some(format!("{DDL_SESSION_PREFIX}{}", parts.join("; "))))
    }
}

fn invalid(message: String) -> CatalogError {
    CatalogError::Invalid { message }
}

fn check_value(key: &str, value: &str) -> CatalogResult<()> {
    // An empty sql_mode is a real mode (all options off).
    let ok = (!value.is_empty() || key == "sql_mode")
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | ',' | '+' | '-' | ':' | '/' | '.')
        });
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "the DDL session header has an invalid value for {key}"
        )))
    }
}

/// The session settings in the header of `ddl`, or `None` when it has no
/// header.
///
/// # Errors
///
/// [`CatalogError::Invalid`] for a malformed header: an entry without
/// `=`, an unknown or repeated key, or an invalid value.
pub fn ddl_session_header(ddl: &str) -> CatalogResult<Option<DdlSession>> {
    let Some(rest) = ddl.strip_prefix(DDL_SESSION_PREFIX) else {
        return Ok(None);
    };
    let line = rest.split('\n').next().unwrap_or("").trim_end_matches('\r');
    let mut session = DdlSession::default();
    for entry in line.split(';') {
        let entry = entry.trim();
        let (key, value) = entry
            .split_once('=')
            .ok_or_else(|| invalid(format!("the DDL session header entry {entry:?} has no =")))?;
        let (key, value) = (key.trim(), value.trim());
        check_value(key, value)?;
        let slot = session
            .slot(key)
            .ok_or_else(|| invalid(format!("the DDL session header has an unknown key {key:?}")))?;
        if slot.is_some() {
            return Err(invalid(format!("the DDL session header repeats {key}")));
        }
        *slot = Some(value.to_owned());
    }
    Ok(Some(session))
}

/// `ddl` without its session header line.
#[must_use]
pub fn strip_ddl_session_header(ddl: &str) -> &str {
    if !ddl.starts_with(DDL_SESSION_PREFIX) {
        return ddl;
    }
    ddl.split_once('\n').map_or("", |(_, rest)| rest)
}

/// Puts a header line (when there is one) before the statements.
pub(crate) fn with_header(session: &DdlSession, script: String) -> CatalogResult<String> {
    Ok(match session.header()? {
        Some(header) => format!("{header}\n{script}"),
        None => script,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() -> CatalogResult<()> {
        let session = DdlSession {
            character_set_client: Some("latin1".into()),
            collation_connection: Some("latin1_swedish_ci".into()),
            sql_mode: Some("PIPES_AS_CONCAT,ANSI_QUOTES,ORACLE".into()),
            time_zone: Some("+02:00".into()),
            collation_database: Some("utf8mb4_0900_ai_ci".into()),
            ..DdlSession::default()
        };
        let ddl = with_header(&session, "CREATE PROCEDURE p() BEGIN END".into())?;
        assert_eq!(
            ddl,
            "-- varve-session: character_set_client=latin1; collation_connection=latin1_swedish_ci; \
             sql_mode=PIPES_AS_CONCAT,ANSI_QUOTES,ORACLE; time_zone=+02:00; \
             collation_database=utf8mb4_0900_ai_ci\nCREATE PROCEDURE p() BEGIN END"
        );
        assert_eq!(ddl_session_header(&ddl)?, Some(session));
        assert_eq!(
            strip_ddl_session_header(&ddl),
            "CREATE PROCEDURE p() BEGIN END"
        );
        assert_eq!(ddl_session_header("CREATE TABLE t (a int)")?, None);
        assert_eq!(strip_ddl_session_header("CREATE TABLE t"), "CREATE TABLE t");
        assert_eq!(with_header(&DdlSession::default(), "X".into())?, "X");
        let empty_mode = DdlSession {
            sql_mode: Some(String::new()),
            ..DdlSession::default()
        };
        let ddl = with_header(&empty_mode, "X".into())?;
        assert_eq!(ddl, "-- varve-session: sql_mode=\nX");
        assert_eq!(ddl_session_header(&ddl)?, Some(empty_mode));
        assert_eq!(
            ddl_session_header("-- varve-session: time_zone=Europe/Amsterdam\r\nX")?
                .and_then(|s| s.time_zone),
            Some("Europe/Amsterdam".into())
        );
        Ok(())
    }

    #[test]
    fn malformed_headers_are_errors() {
        for bad in [
            "-- varve-session: \nX",
            "-- varve-session: sql_mode\nX",
            "-- varve-session: colour=red\nX",
            "-- varve-session: sql_mode=A; sql_mode=B\nX",
            "-- varve-session: time_zone='+00:00'\nX",
            "-- varve-session: time_zone=\nX",
            "-- varve-session: character_set_client=latin1 x\nX",
            "-- varve-session: character_set_client=latin1;\nX",
        ] {
            assert!(ddl_session_header(bad).is_err(), "{bad}");
        }
        let bad = DdlSession {
            time_zone: Some("a;b".into()),
            ..DdlSession::default()
        };
        assert!(bad.header().is_err());
    }
}
