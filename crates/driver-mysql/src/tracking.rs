//! Session state tracking: what the server reports in its OK packets.
//!
//! The session asks the server to track `time_zone`, the access mode
//! variable and the tracking variables themselves, and (in read-only
//! sessions) the transaction characteristics. The changes arrive with every
//! OK packet, so the driver sees a change of the time zone or of read-only
//! mode right after the statement that made it, without a query of its own.
//! `mysql_async` collects them through the Varve patch in
//! `vendor/mysql_async` (see `VARVE-PATCH.md` there).

use mysql_async::SessionStateChange;

use crate::types::TimestampZone;

/// Variables every session tracks, besides the server's own list.
/// `sql_mode` decides how the server reads string literals and quoted
/// names, so where the statements of a script end.
pub(crate) const ALWAYS_TRACKED: [&str; 8] = [
    "time_zone",
    "sql_mode",
    "session_track_system_variables",
    "session_track_transaction_info",
    "character_set_client",
    "character_set_connection",
    "character_set_results",
    "collation_connection",
];

/// The client character set of a session, as the server reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientCharset {
    /// `character_set_client`.
    pub client: String,
    /// `character_set_connection`.
    pub connection: String,
    /// `character_set_results` (empty for `NULL`).
    pub results: String,
    /// `collation_connection`.
    pub collation: String,
}

impl ClientCharset {
    /// True when two collations count as the same: equal, or both
    /// `utf8mb4_*` (Varve accepts any utf8mb4 collation, which is what
    /// `SET NAMES utf8mb4` in a dump picks).
    fn same_collation(a: &str, b: &str) -> bool {
        let utf8mb4 = |name: &str| name.to_ascii_lowercase().starts_with("utf8mb4_");
        a.eq_ignore_ascii_case(b) || (utf8mb4(a) && utf8mb4(b))
    }

    /// True when a reported `name = value` disagrees with this charset.
    fn differs(&self, name: &str, value: &str) -> bool {
        if name.eq_ignore_ascii_case("collation_connection") {
            return !Self::same_collation(&self.collation, value);
        }
        let expected = match name.to_ascii_lowercase().as_str() {
            "character_set_client" => &self.client,
            "character_set_connection" => &self.connection,
            "character_set_results" => &self.results,
            "collation_connection" => &self.collation,
            _ => return false,
        };
        let value = if value.eq_ignore_ascii_case("NULL") {
            ""
        } else {
            value
        };
        !expected.eq_ignore_ascii_case(value)
    }

    /// True when client, connection and results are utf8mb4 and the
    /// collation is a utf8mb4 one.
    pub(crate) fn is_utf8mb4(&self) -> bool {
        ["utf8mb4"; 3]
            == [
                self.client.to_ascii_lowercase().as_str(),
                self.connection.to_ascii_lowercase().as_str(),
                self.results.to_ascii_lowercase().as_str(),
            ]
            && self.collation.to_ascii_lowercase().starts_with("utf8mb4_")
    }

    /// True when both name the same character sets (case does not matter).
    pub(crate) fn same_as(&self, other: &ClientCharset) -> bool {
        self.client.eq_ignore_ascii_case(&other.client)
            && self.connection.eq_ignore_ascii_case(&other.connection)
            && self.results.eq_ignore_ascii_case(&other.results)
            && Self::same_collation(&self.collation, &other.collation)
    }

    /// The statement that sets this charset again. `collation_connection`
    /// comes last because it also sets `character_set_connection`.
    pub(crate) fn restore_statement(&self) -> String {
        let quote = |value: &str| format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"));
        let results = if self.results.is_empty() {
            "NULL".to_owned()
        } else {
            quote(&self.results)
        };
        format!(
            "SET character_set_client = {}, character_set_results = {results}, \
             collation_connection = {}",
            quote(&self.client),
            quote(&self.collation)
        )
    }
}

/// The names the access mode is reported under.
const READ_ONLY_VARIABLES: [&str; 2] = ["transaction_read_only", "tx_read_only"];

/// The tracking variables; a change of one of them means the session may
/// no longer see what it relies on.
const TRACKING_VARIABLES: [&str; 5] = [
    "session_track_system_variables",
    "session_track_transaction_info",
    "session_track_state_change",
    "session_track_schema",
    "session_track_gtids",
];

/// The tracked variable list with `wanted` appended, or `None` when the
/// list already covers them (`*` tracks everything). Names are compared
/// without case, and the server's own entries stay in place.
pub(crate) fn tracking_list(current: &str, wanted: &[&str]) -> Option<String> {
    let current = current.trim();
    let entries: Vec<&str> = current
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    if entries.contains(&"*") {
        return None;
    }
    let missing: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|name| !entries.iter().any(|entry| same_variable(entry, name)))
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(
        entries
            .into_iter()
            .chain(missing)
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// True when two variable names mean the same variable. MariaDB 11.1 and
/// later keep `tx_read_only` as `transaction_read_only` in the tracking list.
fn same_variable(a: &str, b: &str) -> bool {
    let read_only = |name: &str| {
        READ_ONLY_VARIABLES
            .iter()
            .any(|known| name.eq_ignore_ascii_case(known))
    };
    a.eq_ignore_ascii_case(b) || (read_only(a) && read_only(b))
}

/// What a list of reported changes says, in the order it was reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Observed {
    /// The last reported time zone.
    pub zone: Option<TimestampZone>,
    /// `time_zone` was reported (also when its value did not change).
    pub zone_reported: bool,
    /// The access mode was reported as off.
    pub read_only_off: bool,
    /// A transaction (or the next one) was given `READ WRITE`.
    pub read_write_transaction: bool,
    /// A tracking variable changed.
    pub tracking_changed: bool,
    /// The reported client character set variables, in order.
    pub charset: Vec<(String, String)>,
    /// The last reported `sql_mode`.
    pub sql_mode: Option<String>,
}

impl Observed {
    /// True when a reported client character set variable disagrees with
    /// `expected`.
    pub(crate) fn charset_differs(&self, expected: &ClientCharset) -> bool {
        self.charset
            .iter()
            .any(|(name, value)| expected.differs(name, value))
    }

    /// True when the session may have left read-only mode.
    pub(crate) fn left_read_only(&self) -> bool {
        self.read_only_off || self.read_write_transaction || self.tracking_changed
    }

    /// What happened, for the error message.
    pub(crate) fn describe(&self) -> &'static str {
        if self.read_only_off {
            "switched read-only mode off"
        } else if self.read_write_transaction {
            "started a READ WRITE transaction"
        } else {
            "changed session state tracking, which read-only mode relies on"
        }
    }
}

/// The zone a reported `time_zone` value stands for.
fn zone_of(value: &str) -> TimestampZone {
    if crate::session::is_utc_zone(value) {
        TimestampZone::Utc
    } else {
        TimestampZone::Unknown
    }
}

/// Reads a list of reported changes.
pub(crate) fn observe(changes: &[SessionStateChange<'static>]) -> Observed {
    let mut observed = Observed::default();
    for change in changes {
        match change {
            SessionStateChange::SystemVariables(variables) => {
                for variable in variables {
                    let name = variable.name_str();
                    let value = variable.value_str();
                    if name.eq_ignore_ascii_case("time_zone") {
                        observed.zone = Some(zone_of(&value));
                        observed.zone_reported = true;
                    } else if name.eq_ignore_ascii_case("sql_mode") {
                        observed.sql_mode = Some(value.into_owned());
                    } else if READ_ONLY_VARIABLES
                        .iter()
                        .any(|known| name.eq_ignore_ascii_case(known))
                    {
                        // Anything but on counts as off.
                        let on = value.eq_ignore_ascii_case("ON") || value.trim() == "1";
                        observed.read_only_off |= !on;
                    } else if [
                        "character_set_client",
                        "character_set_connection",
                        "character_set_results",
                        "collation_connection",
                    ]
                    .iter()
                    .any(|known| name.eq_ignore_ascii_case(known))
                    {
                        observed
                            .charset
                            .push((name.to_ascii_lowercase(), value.into_owned()));
                    } else if TRACKING_VARIABLES
                        .iter()
                        .any(|known| name.eq_ignore_ascii_case(known))
                    {
                        observed.tracking_changed = true;
                    }
                }
            }
            SessionStateChange::TransactionCharacteristics(characteristics) => {
                let text = characteristics.as_str().to_ascii_uppercase();
                let words: Vec<&str> = text
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|word| !word.is_empty())
                    .collect();
                observed.read_write_transaction |=
                    words.windows(2).any(|pair| pair == ["READ", "WRITE"]);
            }
            _ => {}
        }
    }
    observed
}

/// The latest zone in `changes`, if `time_zone` was reported.
pub(crate) fn latest_zone(changes: &[SessionStateChange<'static>]) -> Option<TimestampZone> {
    observe(changes).zone
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::{SystemVariable, TransactionCharacteristics};

    fn variable(name: &str, value: &str) -> SessionStateChange<'static> {
        SessionStateChange::SystemVariables(vec![
            SystemVariable::new(name.as_bytes().to_vec(), value.as_bytes().to_vec()).into_owned(),
        ])
    }

    fn characteristics(text: &str) -> SessionStateChange<'static> {
        SessionStateChange::TransactionCharacteristics(
            TransactionCharacteristics::new(text.as_bytes().to_vec()).into_owned(),
        )
    }

    fn utf8mb4() -> ClientCharset {
        ClientCharset {
            client: "utf8mb4".into(),
            connection: "utf8mb4".into(),
            results: "utf8mb4".into(),
            collation: "utf8mb4_general_ci".into(),
        }
    }

    #[test]
    fn charset_changes_are_seen() {
        let expected = utf8mb4();
        assert!(observe(&[variable("character_set_client", "gbk")]).charset_differs(&expected));
        assert!(
            observe(&[variable("collation_connection", "latin1_swedish_ci")])
                .charset_differs(&expected)
        );
        assert!(
            !observe(&[variable("character_set_client", "UTF8MB4")]).charset_differs(&expected)
        );
        assert!(
            !observe(&[variable("collation_connection", "utf8mb4_0900_ai_ci")])
                .charset_differs(&expected)
        );
        let mut null_results = utf8mb4();
        null_results.results = String::new();
        assert!(
            !observe(&[variable("character_set_results", "NULL")]).charset_differs(&null_results)
        );
        assert_eq!(
            null_results.restore_statement(),
            "SET character_set_client = 'utf8mb4', character_set_results = NULL, \
             collation_connection = 'utf8mb4_general_ci'"
        );
    }

    #[test]
    fn appends_only_missing_names() {
        let mysql = "time_zone,autocommit,character_set_client";
        assert_eq!(
            tracking_list(mysql, &["time_zone", "transaction_read_only"]).as_deref(),
            Some("time_zone,autocommit,character_set_client,transaction_read_only")
        );
        assert_eq!(tracking_list(mysql, &["TIME_ZONE"]), None);
        // MariaDB 12.3 keeps tx_read_only as transaction_read_only.
        assert_eq!(
            tracking_list("time_zone,transaction_read_only", &["tx_read_only"]),
            None
        );
        assert_eq!(tracking_list("*", &["time_zone"]), None);
        assert_eq!(
            tracking_list("", &["time_zone", "tx_read_only"]).as_deref(),
            Some("time_zone,tx_read_only")
        );
        assert_eq!(
            tracking_list(" autocommit , ", &["time_zone"]).as_deref(),
            Some("autocommit,time_zone")
        );
    }

    #[test]
    fn reads_reported_changes() {
        let observed = observe(&[variable("time_zone", "+02:00")]);
        assert_eq!(observed.zone, Some(TimestampZone::Unknown));
        assert!(observed.zone_reported && !observed.left_read_only());

        let observed = observe(&[
            variable("time_zone", "+02:00"),
            variable("time_zone", "UTC"),
        ]);
        assert_eq!(observed.zone, Some(TimestampZone::Utc));

        for (name, value) in [
            ("transaction_read_only", "OFF"),
            ("tx_read_only", "0"),
            ("TX_READ_ONLY", "off"),
        ] {
            let observed = observe(&[variable(name, value)]);
            assert!(observed.read_only_off, "{name}={value}");
            assert_eq!(observed.describe(), "switched read-only mode off");
        }
        assert!(!observe(&[variable("tx_read_only", "ON")]).left_read_only());
        assert!(!observe(&[variable("transaction_read_only", "1")]).left_read_only());

        let observed = observe(&[characteristics("START TRANSACTION READ WRITE;")]);
        assert!(observed.read_write_transaction);
        assert!(observe(&[characteristics("SET TRANSACTION READ WRITE;")]).left_read_only());
        assert!(!observe(&[characteristics("START TRANSACTION READ ONLY;")]).left_read_only());
        assert!(!observe(&[characteristics("")]).left_read_only());

        let observed = observe(&[variable("session_track_system_variables", "autocommit")]);
        assert!(observed.tracking_changed && observed.left_read_only());
        assert!(observe(&[variable("session_track_transaction_info", "OFF")]).left_read_only());
        assert!(!observe(&[variable("autocommit", "OFF")]).left_read_only());
    }
}
