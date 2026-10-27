//! Keeps a session in the state the driver depends on.
//!
//! Values are converted from PostgreSQL's text output, which depends on
//! session settings (`DateStyle`, `IntervalStyle`, `TimeZone`,
//! `extra_float_digits`, `bytea_output`, `client_encoding`,
//! `standard_conforming_strings`). A read-only session also depends on
//! `default_transaction_read_only`, a configured `statement_timeout` must
//! survive `RESET ALL`, and type lookups need tokio-postgres's own prepared
//! statements. A statement, or a function it calls, can change any of these;
//! the guard checks them and sets back what changed.
//!
//! The guard never sends anything while the backend is inside a transaction
//! block: a query there would take the transaction's snapshot (breaking
//! `SET TRANSACTION ISOLATION LEVEL` and `SET TRANSACTION SNAPSHOT`, and
//! moving a `REPEATABLE READ` snapshot earlier), and a restore there would be
//! undone by `ROLLBACK`. It runs only when the backend is idle: after an
//! autocommitted statement, when a transaction has ended, before a statement
//! whose values depend on unreported settings, and at the end of a run in
//! which something could have changed a setting (a risky statement, a
//! function call, a reported change). In a read-only session a check that
//! a cancel interrupted is owed: no further statement runs until it is done.
//! For read-only mode that is enough together with one more rule: a
//! transaction's access mode is fixed when it starts (`SET TRANSACTION READ
//! WRITE` is refused before anything runs), so switching
//! `default_transaction_read_only` off inside a transaction only affects
//! the next one, which starts after an idle check. The exception is a
//! procedure or `DO` block that commits inside itself and so starts a new
//! transaction under the changed default without returning to the driver:
//! a read-only session therefore runs every top-level `CALL` and `DO`
//! inside `BEGIN READ ONLY ... COMMIT`, where such a commit is refused.
//!
//! The settings the server reports with `ParameterStatus` are read from the
//! tap for free. The others need a query, which runs after statements that
//! can change them (`SET`, `RESET`, `DISCARD`, `CALL`, `DO`, unknown
//! statements and anything mentioning `set_config`), after every
//! autocommitted statement of a read-only session (that query covers the
//! unreported settings too), before any statement that returns float or
//! bytea values (or arrays, ranges, domains and composites of them) when any
//! statement has executed since the last check, in this run or an earlier
//! one (a view, trigger, operator, domain check, cast or column default can
//! change them unseen), and at the end of a run as above. tokio-postgres's type lookup
//! statements are checked by name (the tap records them).
//!
//! What remains: values returned by the very statement that changed a
//! setting, and values read inside a transaction block after a setting
//! changed there, are converted under the changed setting (timestamps stay
//! right, since they carry their offset; floats can lose digits, bytea and
//! intervals can come back as text). The settings are set back as soon as
//! the backend is idle again, with a warning, plus a notice when float or
//! bytea values were read in such a transaction.

use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, SimpleQueryMessage};
use varve_driver::{DriverError, ErrorKind, StatementKind};

use crate::error::{Interrupt, QUERY_CANCELED, exec_error};
use crate::tap::Shared;

/// A setting with the value the driver needs and the statement that sets it.
struct Wanted {
    name: &'static str,
    value: &'static str,
    restore: &'static str,
}

/// Settings the server reports on every change (`GUC_REPORT`), so the tap
/// sees them without a query.
const REPORTED: [Wanted; 5] = [
    Wanted {
        name: "DateStyle",
        value: "ISO, YMD",
        restore: "SET DateStyle = ISO, YMD",
    },
    Wanted {
        name: "IntervalStyle",
        value: "iso_8601",
        restore: "SET IntervalStyle = iso_8601",
    },
    Wanted {
        name: "TimeZone",
        value: "UTC",
        restore: "SET TimeZone = 'UTC'",
    },
    Wanted {
        name: "client_encoding",
        value: "UTF8",
        restore: "SET client_encoding = 'UTF8'",
    },
    Wanted {
        name: "standard_conforming_strings",
        value: "on",
        restore: "SET standard_conforming_strings = on",
    },
];

/// Settings the server does not report; they are queried.
const QUERIED: [Wanted; 2] = [
    Wanted {
        name: "extra_float_digits",
        value: "3",
        restore: "SET extra_float_digits = 3",
    },
    Wanted {
        name: "bytea_output",
        value: "hex",
        restore: "SET bytea_output = hex",
    },
];

/// The statements that put a new session in the state the guard keeps.
pub(crate) fn setup_sql() -> String {
    REPORTED
        .iter()
        .chain(QUERIED.iter())
        .map(|w| w.restore)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Name of the column that reads the session's `statement_timeout`.
const OBSERVED_TIMEOUT: &str = "observed statement_timeout";

/// Name of the column that reads the session's `lc_messages`.
const OBSERVED_LC_MESSAGES: &str = "observed lc_messages";

/// Name of the pseudo setting for tokio-postgres's type lookup statements.
const TYPE_STATEMENTS: &str = "type lookup statements";

/// What the session was opened with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Expected {
    /// The session is read-only.
    pub(crate) read_only: bool,
    /// The configured `statement_timeout`, in milliseconds.
    pub(crate) statement_timeout_ms: Option<u128>,
}

/// What to check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Scope {
    /// Query the settings the server does not report.
    pub(crate) query_unreported: bool,
    /// The statement returns settings to their defaults (`RESET`,
    /// `DISCARD`), so the configured `statement_timeout` is checked even
    /// when nothing else is queried. Every settings query checks it anyway:
    /// only a value a reset brought back to its default is replaced.
    pub(crate) resets: bool,
    /// Check that tokio-postgres's own prepared statements (its type
    /// lookups) still exist.
    pub(crate) type_statements: bool,
}

impl Scope {
    /// Everything (tests build the full query with it).
    #[cfg(test)]
    pub(crate) const ALL: Self = Self {
        query_unreported: true,
        resets: true,
        type_statements: true,
    };

    /// Every setting and the type lookups: used at the end of a run. A
    /// timeout the user set stays (only a reset one is replaced).
    pub(crate) const SETTINGS: Self = Self {
        query_unreported: true,
        resets: false,
        type_statements: true,
    };

    /// Both scopes together.
    pub(crate) fn merge(self, other: Self) -> Self {
        Self {
            query_unreported: self.query_unreported || other.query_unreported,
            resets: self.resets || other.resets,
            type_statements: self.type_statements || other.type_statements,
        }
    }

    /// True when nothing is to be queried.
    pub(crate) fn is_empty(self) -> bool {
        self == Self::default()
    }

    /// The scope for a statement of `kind` whose bare words (outside
    /// literals and comments) are `words`.
    pub(crate) fn for_statement(kind: StatementKind, words: &[String]) -> Self {
        let first = words.first().map(|w| w.to_ascii_uppercase());
        let resets = resets_statement_timeout(words);
        let query_unreported = resets
            || matches!(first.as_deref(), Some("RESET" | "DISCARD"))
            || matches!(
                kind,
                StatementKind::Session | StatementKind::Call | StatementKind::Other
            )
            || words.iter().any(|w| w.eq_ignore_ascii_case("set_config"));
        Self {
            query_unreported,
            resets,
            // Only DO, CALL and unknown statements can run DEALLOCATE ALL
            // (the statement itself is refused).
            type_statements: matches!(kind, StatementKind::Call | StatementKind::Other),
        }
    }
}

/// True when a statement returns `statement_timeout` to its default:
/// `RESET ALL`, `RESET statement_timeout`, `DISCARD ALL`, or
/// `SET [SESSION] statement_timeout TO DEFAULT` (`SET LOCAL` ends with its
/// transaction by itself). The profile's value is then set again.
fn resets_statement_timeout(words: &[String]) -> bool {
    let is = |i: usize, w: &str| words.get(i).is_some_and(|x| x.eq_ignore_ascii_case(w));
    let last_is_default = words
        .last()
        .is_some_and(|w| w.eq_ignore_ascii_case("DEFAULT"));
    (is(0, "RESET") && (is(1, "ALL") || is(1, "statement_timeout")))
        || (is(0, "DISCARD") && is(1, "ALL"))
        || (is(0, "SET")
            && !is(1, "LOCAL")
            && last_is_default
            && (is(1, "statement_timeout") || (is(1, "SESSION") && is(2, "statement_timeout"))))
}

/// What the guard had to set back.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Restored {
    /// Read-only mode was off and is on again.
    pub(crate) read_only: bool,
    /// `name = value` of every other setting that was set back.
    pub(crate) settings: Vec<String>,
    /// tokio-postgres's prepared statements are gone (a `DEALLOCATE ALL`
    /// inside a `DO` block, for example); the session must reconnect.
    pub(crate) type_statements_lost: bool,
    /// The session's `statement_timeout` when it was read (`Some(None)`:
    /// none), after any restore.
    pub(crate) statement_timeout_ms: Option<Option<u128>>,
    /// The session's `lc_messages`, when it was read.
    pub(crate) lc_messages: Option<String>,
}

/// What a guard call did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The session was checked; this was set back.
    Checked(Restored),
    /// Nothing was checked: the backend is in a transaction block, or a
    /// cancel request landed on the check. Check again later.
    Skipped,
}

impl Restored {
    /// True when `extra_float_digits` or `bytea_output` was set back.
    pub(crate) fn changed_unreported(&self) -> bool {
        self.settings
            .iter()
            .any(|s| QUERIED.iter().any(|w| s.starts_with(w.name)))
    }

    /// The warnings to show for this, with `who` (`statement 3`, `the init
    /// SQL`) as the culprit.
    pub(crate) fn warnings(&self, who: &str) -> Vec<String> {
        let mut out = Vec::new();
        if self.read_only {
            out.push(format!(
                "{who} switched read-only mode off; Varve switched it back on"
            ));
        }
        if !self.settings.is_empty() {
            out.push(format!(
                "{who} changed session settings Varve depends on; Varve set them back: {}",
                self.settings.join(", ")
            ));
        }
        out
    }
}

/// A setting that is checked: how to read it, the wanted value and how to
/// set it back.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Check {
    /// SQL expression that reads the value (empty for reported settings).
    expr: String,
    name: String,
    value: String,
    restore: String,
}

impl Check {
    fn new(expr: String, name: &str, value: impl Into<String>, restore: impl Into<String>) -> Self {
        Self {
            expr,
            name: name.to_owned(),
            value: value.into(),
            restore: restore.into(),
        }
    }

    fn from_wanted(w: &Wanted, expr: String) -> Self {
        Self::new(expr, w.name, w.value, w.restore)
    }
}

/// `name` as a SQL string literal.
fn literal(name: &str) -> String {
    format!("'{}'", name.replace('\'', "''"))
}

/// `ms` as the server shows a `statement_timeout` of that many
/// milliseconds (`current_setting`, `SHOW`): in the largest unit that
/// divides it (`20s`, `1min`, `1500ms`).
fn shown_ms(ms: u128) -> String {
    if ms == 0 {
        return "0".to_owned();
    }
    for (unit, size) in [
        ("d", 86_400_000),
        ("h", 3_600_000),
        ("min", 60_000),
        ("s", 1_000),
    ] {
        if ms.is_multiple_of(size) {
            return format!("{}{unit}", ms / size);
        }
    }
    format!("{ms}ms")
}

/// The settings query for `expected` and `scope`, or `None` when nothing
/// needs a query. `type_names` are tokio-postgres's type lookup statements
/// that should exist.
fn build_query(
    expected: Expected,
    scope: Scope,
    type_names: &[String],
) -> Option<(String, Vec<Check>)> {
    let mut checks = Vec::new();
    if scope.query_unreported || expected.read_only {
        for w in &QUERIED {
            checks.push(Check::from_wanted(
                w,
                format!("pg_catalog.current_setting('{}')", w.name),
            ));
        }
    }
    if expected.read_only {
        // The guard runs only between transactions, where the default is
        // what the next transaction gets.
        checks.push(Check::new(
            "pg_catalog.current_setting('default_transaction_read_only')".into(),
            "default_transaction_read_only",
            "on",
            "SET default_transaction_read_only = on",
        ));
    }
    if scope.type_statements && !type_names.is_empty() {
        // Every one of them must still be there: a lookup that is gone
        // fails the next prepare of a new type.
        let names: Vec<String> = type_names.iter().map(|n| literal(n)).collect();
        checks.push(Check::new(
            format!(
                "(SELECT pg_catalog.count(*) OPERATOR(pg_catalog.=) {} \
                 FROM pg_catalog.pg_prepared_statements WHERE NOT from_sql \
                 AND name OPERATOR(pg_catalog.=) ANY (ARRAY[{}]::pg_catalog.text[]))",
                type_names.len(),
                names.join(", ")
            ),
            TYPE_STATEMENTS,
            "t",
            "",
        ));
    }
    // Whenever the settings are queried at all, and after any statement
    // that could reset them: a reset can hide anywhere (`RESET
    // "statement_timeout"`, a DO block, a function, `set_config(..., NULL)`),
    // and the source below tells whether one took effect.
    if (scope.resets || !checks.is_empty())
        && let Some(ms) = expected.statement_timeout_ms
    {
        // Only a value that a reset took effect on is replaced. Its source
        // tells: every SET (Varve's own profile value, or the user's) has the
        // source "session", and a RESET (or SET TO DEFAULT, RESET ALL,
        // DISCARD ALL, set_config(..., NULL)) that took effect brings back
        // the source of the default (the server's, role's or database's). A
        // reset that was rolled back, with its transaction or its savepoint,
        // leaves the value and its source as they were: the user's own value
        // stays, even when it equals the default.
        //
        // Reading the source means reading pg_settings, which builds every
        // setting of the session. In the common case the value is the
        // profile's, which needs no restore whatever its source: a cheap
        // comparison of current_setting (shown in the server's unit, which
        // `shown_ms` reproduces) settles it, and the subquery in the other
        // CASE branch is not run. pg_settings gives the value in its base
        // unit (ms).
        //
        // A session in which the user set a timeout of their own takes the
        // pg_settings branch on every check (a read-only session checks
        // after every statement: about 0.4 ms more locally). That is kept on
        // purpose: a fast path that also accepted the user's last value
        // could not tell it from a reset to a default of the same value
        // (a role or database default equal to it), which must bring the
        // profile's timeout back.
        checks.push(Check::new(
            format!(
                "CASE WHEN pg_catalog.current_setting('statement_timeout') \
                 OPERATOR(pg_catalog.=) '{shown}' THEN '{ms}' \
                 ELSE (SELECT CASE WHEN source OPERATOR(pg_catalog.<>) 'session' THEN setting \
                 ELSE '{ms}' END FROM pg_catalog.pg_settings \
                 WHERE name OPERATOR(pg_catalog.=) 'statement_timeout') END",
                shown = shown_ms(ms)
            ),
            "statement_timeout",
            ms.to_string(),
            format!("SET statement_timeout = {ms}"),
        ));
    }
    if checks.is_empty() {
        return None;
    }
    if scope.query_unreported || scope.resets {
        // Not kept, only read: a statement_timeout the user sets overrides
        // the profile's for the session, but Varve must know it to tell a
        // timeout from a cancel.
        checks.push(Check::new(
            "pg_catalog.current_setting('statement_timeout')".into(),
            OBSERVED_TIMEOUT,
            "",
            "",
        ));
        // Read for the same reason: whether the server's messages are
        // translated decides how a timeout is told from a cancel.
        checks.push(Check::new(
            "pg_catalog.current_setting('lc_messages')".into(),
            OBSERVED_LC_MESSAGES,
            "",
            "",
        ));
    }
    let columns: Vec<&str> = checks.iter().map(|c| c.expr.as_str()).collect();
    Some((format!("SELECT {}", columns.join(", ")), checks))
}

/// The settings query with every check, for the SQL qualification test.
#[cfg(test)]
pub(crate) fn full_query(type_names: &[String]) -> Option<String> {
    let expected = Expected {
        read_only: true,
        statement_timeout_ms: Some(1500),
    };
    build_query(expected, Scope::ALL, type_names).map(|(sql, _)| sql)
}

/// The answer of the settings query.
enum Queried {
    /// The checks whose value differs, and what was only read.
    Drift(Vec<Check>, Observed),
    /// A cancel request interrupted the query.
    Cancelled,
}

/// Settings the query read without keeping them.
#[derive(Debug, Default)]
struct Observed {
    /// `statement_timeout` as the server shows it (`20s`, `0`).
    statement_timeout: Option<String>,
    /// `lc_messages`.
    lc_messages: Option<String>,
}

/// Runs the settings query.
async fn queried_drift(
    client: &Client,
    expected: Expected,
    scope: Scope,
    type_names: &[String],
) -> Result<Queried, DriverError> {
    let Some((sql, checks)) = build_query(expected, scope, type_names) else {
        return Ok(Queried::Drift(Vec::new(), Observed::default()));
    };
    let messages = match client.simple_query(&sql).await {
        Ok(messages) => messages,
        Err(error) if error.code().map(SqlState::code) == Some(QUERY_CANCELED) => {
            return Ok(Queried::Cancelled);
        }
        Err(error) => return Err(exec_error(&error, 0, Interrupt::NONE)),
    };
    let row = messages
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .ok_or_else(|| {
            DriverError::new(ErrorKind::Protocol, "the settings check returned no row")
        })?;
    let mut observed = Observed::default();
    let mut drift = Vec::new();
    for (i, check) in checks.into_iter().enumerate() {
        if check.name == OBSERVED_TIMEOUT {
            observed.statement_timeout = row.get(i).map(str::to_owned);
        } else if check.name == OBSERVED_LC_MESSAGES {
            observed.lc_messages = row.get(i).map(str::to_owned);
        } else if row.get(i) != Some(check.value.as_str()) {
            drift.push(check);
        }
    }
    Ok(Queried::Drift(drift, observed))
}

/// True when a reported setting differs from what the driver needs.
pub(crate) fn reported_drift_exists(shared: &Shared) -> bool {
    !reported_drift(shared).is_empty()
}

/// The reported settings whose last reported value differs.
fn reported_drift(shared: &Shared) -> Vec<Check> {
    REPORTED
        .iter()
        .filter(|w| {
            shared
                .parameter(w.name)
                .is_some_and(|v| !v.eq_ignore_ascii_case(w.value))
        })
        .map(|w| Check::from_wanted(w, String::new()))
        .collect()
}

/// Checks the session and sets back whatever changed. An error means the
/// session could not be put back and must not be used any more.
///
/// Runs only while the backend is idle (see the module docs), and only after
/// every cancel request in flight has been delivered, so a late cancel signal
/// is discarded by the idle backend instead of hitting the check. A cancel
/// that still lands on the check (sent while it runs) skips the check: it
/// interrupts nothing of the user's, since no transaction is open.
pub(crate) async fn ensure(
    client: &Client,
    shared: &Shared,
    expected: Expected,
    scope: Scope,
    type_names: &[String],
) -> Result<Outcome, DriverError> {
    let mut restored = Restored::default();
    for attempt in 0..2 {
        if shared.transaction_status() != Some(b'I') {
            return Ok(Outcome::Skipped);
        }
        shared.wait_for_cancels().await;
        let queried = match queried_drift(client, expected, scope, type_names).await? {
            Queried::Drift(drift, observed) => {
                if let Some(value) = observed
                    .statement_timeout
                    .and_then(|v| crate::session::parse_duration_ms(&v))
                {
                    restored.statement_timeout_ms = Some(value);
                }
                if observed.lc_messages.is_some() {
                    restored.lc_messages = observed.lc_messages;
                }
                drift
            }
            Queried::Cancelled => return Ok(Outcome::Skipped),
        };
        // Read after the query: its answer comes after every
        // ParameterStatus of the statement before it.
        let mut drift = reported_drift(shared);
        drift.extend(queried);
        if let Some(i) = drift.iter().position(|c| c.name == TYPE_STATEMENTS) {
            drift.remove(i);
            restored.type_statements_lost = true;
        }
        if drift.is_empty() {
            return Ok(Outcome::Checked(restored));
        }
        if attempt > 0 {
            let names: Vec<&str> = drift.iter().map(|c| c.name.as_str()).collect();
            return Err(DriverError::new(
                ErrorKind::Protocol,
                format!("{} could not be set back", names.join(", ")),
            ));
        }
        let mut fixes = Vec::new();
        for check in &drift {
            if check.name == "default_transaction_read_only" {
                restored.read_only = true;
            } else {
                restored
                    .settings
                    .push(format!("{} = {}", check.name, check.value));
            }
            fixes.push(check.restore.as_str());
        }
        match client.batch_execute(&fixes.join("; ")).await {
            Ok(()) => {}
            // A cancel (from Varve or another connection) interrupted the
            // restore; nothing of the user's was running. Check again later.
            Err(error) if error.code().map(SqlState::code) == Some(QUERY_CANCELED) => {
                return Ok(Outcome::Skipped);
            }
            Err(error) => return Err(exec_error(&error, 0, Interrupt::NONE)),
        }
    }
    Ok(Outcome::Checked(restored))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(sql: &str) -> Vec<String> {
        sql.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn scope_follows_the_statement() {
        let select = Scope::for_statement(StatementKind::Select, &words("SELECT 1"));
        assert!(!select.query_unreported && !select.resets);
        let set_config =
            Scope::for_statement(StatementKind::Select, &words("SELECT set_config ( ) "));
        assert!(set_config.query_unreported && !set_config.resets);
        let reset = Scope::for_statement(StatementKind::Session, &words("RESET ALL"));
        assert!(reset.query_unreported && reset.resets);
        let discard = Scope::for_statement(StatementKind::Other, &words("discard all"));
        assert!(discard.query_unreported && discard.resets);
        for (sql, resets) in [
            ("RESET statement_timeout", true),
            ("RESET work_mem", false),
            ("SET statement_timeout TO DEFAULT", true),
            ("SET SESSION statement_timeout = DEFAULT", true),
            ("SET LOCAL statement_timeout TO DEFAULT", false),
            ("SET work_mem TO DEFAULT", false),
            ("SET statement_timeout = 0", false),
            ("DISCARD PLANS", false),
        ] {
            let scope = Scope::for_statement(StatementKind::Session, &words(sql));
            assert_eq!(scope.resets, resets, "{sql}");
            assert!(scope.query_unreported, "{sql}");
        }
        let call = Scope::for_statement(StatementKind::Call, &words("DO"));
        assert!(call.query_unreported && !call.resets);
    }

    #[test]
    fn the_query_covers_what_is_expected() {
        let plain = Expected {
            read_only: false,
            statement_timeout_ms: Some(1500),
        };
        assert!(build_query(plain, Scope::default(), &[]).is_none());
        let Some((sql, checks)) = build_query(plain, Scope::ALL, &[]) else {
            panic!("no query");
        };
        assert!(sql.contains("pg_catalog.current_setting('bytea_output')"));
        assert!(sql.contains("name OPERATOR(pg_catalog.=) 'statement_timeout'"));
        assert!(!sql.contains("read_only"));
        assert_eq!(checks.len(), 5);
        assert_eq!(checks[3].name, OBSERVED_TIMEOUT);
        assert_eq!(checks[4].name, OBSERVED_LC_MESSAGES);
        assert_eq!(checks[2].value, "1500");
        // Only a reset that took effect (the default's source is back) is
        // replaced, never a value of the user's that equals the default.
        assert!(sql.contains(
            "CASE WHEN source OPERATOR(pg_catalog.<>) 'session' THEN setting ELSE '1500' END"
        ));
        // The common case (the profile's value) needs no pg_settings.
        assert!(sql.contains(
            "CASE WHEN pg_catalog.current_setting('statement_timeout') \
             OPERATOR(pg_catalog.=) '1500ms' THEN '1500'"
        ));
        assert_eq!(sql.matches("pg_settings").count(), 1, "{sql}");
        assert!(!sql.contains("reset_val"));
        let read_only = Expected {
            read_only: true,
            statement_timeout_ms: Some(1500),
        };
        let names = vec!["s1".to_owned(), "s'2".to_owned()];
        let Some((sql, _)) = build_query(read_only, Scope::default(), &names) else {
            panic!("no query");
        };
        assert!(sql.contains("'default_transaction_read_only'"));
        // Every settings query checks that no reset dropped the profile's
        // timeout.
        assert!(
            sql.contains("source OPERATOR(pg_catalog.<>) 'session'"),
            "{sql}"
        );
        assert!(!sql.contains("pg_prepared_statements"));
        let Some((sql, _)) = build_query(read_only, Scope::ALL, &names) else {
            panic!("no query");
        };
        assert!(
            sql.contains("pg_catalog.count(*) OPERATOR(pg_catalog.=) 2"),
            "{sql}"
        );
        assert!(
            sql.contains("ANY (ARRAY['s1', 's''2']::pg_catalog.text[])"),
            "{sql}"
        );
        assert!(
            build_query(plain, Scope::ALL, &[])
                .is_some_and(|(sql, _)| !sql.contains("pg_prepared"))
        );
    }

    #[test]
    fn shows_milliseconds_like_the_server() {
        for (ms, shown) in [
            (0, "0"),
            (1, "1ms"),
            (1500, "1500ms"),
            (2000, "2s"),
            (20_000, "20s"),
            (60_000, "1min"),
            (90_000, "90s"),
            (3_600_000, "1h"),
            (86_400_000, "1d"),
            (172_800_000, "2d"),
        ] {
            assert_eq!(shown_ms(ms), shown, "{ms}");
        }
    }

    #[test]
    fn setup_sets_every_kept_setting() {
        let sql = setup_sql();
        for w in REPORTED.iter().chain(QUERIED.iter()) {
            assert!(sql.contains(w.restore), "{}", w.name);
        }
    }

    #[test]
    fn warnings_name_the_culprit() {
        let restored = Restored {
            read_only: true,
            settings: vec!["TimeZone = UTC".into()],
            type_statements_lost: false,
            statement_timeout_ms: None,
            lc_messages: None,
        };
        assert_eq!(
            restored.warnings("statement 2"),
            vec![
                "statement 2 switched read-only mode off; Varve switched it back on".to_owned(),
                "statement 2 changed session settings Varve depends on; Varve set them back: \
                 TimeZone = UTC"
                    .to_owned(),
            ]
        );
        assert!(Restored::default().warnings("x").is_empty());
    }
}
