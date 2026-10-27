//! Scheduled events.

use serde::{Deserialize, Serialize};

use super::types::Definer;
use super::{Plan, Target, fragment, invalid, object_name, plain_text, value_text};
use crate::model::CatalogResult;

/// Unit of an event interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IntervalUnit {
    /// `YEAR`.
    Year,
    /// `QUARTER`.
    Quarter,
    /// `MONTH`.
    Month,
    /// `WEEK`.
    Week,
    /// `DAY`.
    Day,
    /// `HOUR`.
    Hour,
    /// `MINUTE`.
    Minute,
    /// `SECOND`.
    Second,
    /// `YEAR_MONTH` (quantity like `'1-6'`).
    YearMonth,
    /// `DAY_HOUR` (`'1 12'`).
    DayHour,
    /// `DAY_MINUTE` (`'1 12:30'`).
    DayMinute,
    /// `DAY_SECOND` (`'1 12:30:15'`).
    DaySecond,
    /// `HOUR_MINUTE` (`'1:30'`).
    HourMinute,
    /// `HOUR_SECOND` (`'1:30:15'`).
    HourSecond,
    /// `MINUTE_SECOND` (`'30:15'`).
    MinuteSecond,
}

impl IntervalUnit {
    fn sql(self) -> &'static str {
        match self {
            Self::Year => "YEAR",
            Self::Quarter => "QUARTER",
            Self::Month => "MONTH",
            Self::Week => "WEEK",
            Self::Day => "DAY",
            Self::Hour => "HOUR",
            Self::Minute => "MINUTE",
            Self::Second => "SECOND",
            Self::YearMonth => "YEAR_MONTH",
            Self::DayHour => "DAY_HOUR",
            Self::DayMinute => "DAY_MINUTE",
            Self::DaySecond => "DAY_SECOND",
            Self::HourMinute => "HOUR_MINUTE",
            Self::HourSecond => "HOUR_SECOND",
            Self::MinuteSecond => "MINUTE_SECOND",
        }
    }

    fn is_compound(self) -> bool {
        matches!(
            self,
            Self::YearMonth
                | Self::DayHour
                | Self::DayMinute
                | Self::DaySecond
                | Self::HourMinute
                | Self::HourSecond
                | Self::MinuteSecond
        )
    }
}

/// An interval: a quantity and a unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interval {
    /// A positive whole number for simple units, or the compound form
    /// (`'1:30'` for `HOUR_MINUTE`) for compound units.
    pub quantity: String,
    /// Unit.
    pub unit: IntervalUnit,
}

impl Interval {
    fn sql(&self) -> CatalogResult<String> {
        let quantity = self.quantity.trim();
        let value = if self.unit.is_compound() {
            let ok = !quantity.is_empty()
                && quantity.chars().any(|c| c.is_ascii_digit())
                && quantity
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, ' ' | ':' | '-' | '.'));
            if !ok {
                return Err(invalid(format!(
                    "{quantity:?} is not a {} quantity",
                    self.unit.sql()
                )));
            }
            plain_text(quantity, "an interval")?
        } else {
            let n: u64 = quantity
                .parse()
                .map_err(|_| invalid(format!("{quantity:?} is not a positive whole number")))?;
            if n == 0 {
                return Err(invalid("an interval must be at least 1"));
            }
            n.to_string()
        };
        Ok(format!("{value} {}", self.unit.sql()))
    }
}

/// Starting point of an event time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TimeBase {
    /// `CURRENT_TIMESTAMP`.
    CurrentTimestamp,
    /// A timestamp such as `2030-01-01 00:00:00`, in the session time zone.
    Timestamp {
        /// The timestamp text.
        value: String,
    },
}

/// A point in time: a base plus intervals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventTime {
    /// The base.
    pub base: TimeBase,
    /// Intervals added to it.
    #[serde(default)]
    pub plus: Vec<Interval>,
}

impl EventTime {
    fn sql(&self) -> CatalogResult<String> {
        let mut out = match &self.base {
            TimeBase::CurrentTimestamp => "CURRENT_TIMESTAMP".to_string(),
            TimeBase::Timestamp { value } => {
                if value.trim().is_empty() {
                    return Err(invalid("an event time needs a timestamp"));
                }
                value_text(value.trim())?
            }
        };
        for interval in &self.plus {
            out.push_str(&format!(" + INTERVAL {}", interval.sql()?));
        }
        Ok(out)
    }
}

/// When an event runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EventSchedule {
    /// Once: `AT time`.
    At {
        /// The time.
        at: EventTime,
    },
    /// Repeatedly: `EVERY interval [STARTS time] [ENDS time]`.
    Every {
        /// The interval.
        interval: Interval,
        /// First run.
        #[serde(default)]
        starts: Option<EventTime>,
        /// Last possible run.
        #[serde(default)]
        ends: Option<EventTime>,
    },
}

impl EventSchedule {
    fn sql(&self) -> CatalogResult<String> {
        Ok(match self {
            Self::At { at } => format!("ON SCHEDULE AT {}", at.sql()?),
            Self::Every {
                interval,
                starts,
                ends,
            } => {
                let mut out = format!("ON SCHEDULE EVERY {}", interval.sql()?);
                if let Some(starts) = starts {
                    out.push_str(&format!(" STARTS {}", starts.sql()?));
                }
                if let Some(ends) = ends {
                    out.push_str(&format!(" ENDS {}", ends.sql()?));
                }
                out
            }
        })
    }
}

/// Whether an event runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventStatus {
    /// `ENABLE`.
    Enable,
    /// `DISABLE`.
    Disable,
    /// `DISABLE ON SLAVE` / `DISABLE ON REPLICA`: runs on the source only.
    DisableOnReplica,
}

impl EventStatus {
    fn sql(self, t: &Target) -> &'static str {
        match self {
            Self::Enable => "ENABLE",
            Self::Disable => "DISABLE",
            // MySQL 8.0 only knows ON SLAVE; MySQL 8.4 deprecates it.
            Self::DisableOnReplica if t.mysql_at(8, 4, 0) => "DISABLE ON REPLICA",
            Self::DisableOnReplica => "DISABLE ON SLAVE",
        }
    }
}

/// `CREATE EVENT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateEvent {
    /// Database.
    pub database: String,
    /// Event name.
    pub name: String,
    /// `IF NOT EXISTS`.
    #[serde(default)]
    pub if_not_exists: bool,
    /// `DEFINER`.
    #[serde(default)]
    pub definer: Option<Definer>,
    /// Schedule.
    pub schedule: EventSchedule,
    /// `ON COMPLETION PRESERVE` (true) or `NOT PRESERVE` (false).
    #[serde(default)]
    pub on_completion_preserve: Option<bool>,
    /// Status.
    #[serde(default)]
    pub status: Option<EventStatus>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
    /// The body: one statement or a `BEGIN ... END` block.
    pub body: String,
}

/// `ALTER EVENT`. Fields left `None` are not changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlterEvent {
    /// Database.
    pub database: String,
    /// Event name.
    pub name: String,
    /// `DEFINER`.
    #[serde(default)]
    pub definer: Option<Definer>,
    /// New schedule.
    #[serde(default)]
    pub schedule: Option<EventSchedule>,
    /// `ON COMPLETION [NOT] PRESERVE`.
    #[serde(default)]
    pub on_completion_preserve: Option<bool>,
    /// New name (`RENAME TO`), in the same database.
    #[serde(default)]
    pub rename_to: Option<String>,
    /// Status.
    #[serde(default)]
    pub status: Option<EventStatus>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
    /// New body.
    #[serde(default)]
    pub body: Option<String>,
}

/// `DROP EVENT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropEvent {
    /// Database.
    pub database: String,
    /// Event name.
    pub name: String,
    /// `IF EXISTS`.
    #[serde(default)]
    pub if_exists: bool,
}

fn completion(preserve: bool) -> &'static str {
    if preserve {
        "ON COMPLETION PRESERVE"
    } else {
        "ON COMPLETION NOT PRESERVE"
    }
}

fn scheduler_note(plan: &mut Plan) {
    plan.warn("events only run while the event scheduler is ON (event_scheduler)");
}

pub(super) fn create(t: &Target, c: &CreateEvent, plan: &mut Plan) -> CatalogResult<()> {
    let mut sql = String::from("CREATE");
    if let Some(definer) = &c.definer {
        sql.push(' ');
        sql.push_str(&definer.clause()?);
    }
    sql.push_str(" EVENT ");
    if c.if_not_exists {
        sql.push_str("IF NOT EXISTS ");
    }
    sql.push_str(&object_name(&c.database, &c.name)?);
    sql.push(' ');
    sql.push_str(&c.schedule.sql()?);
    if let Some(preserve) = c.on_completion_preserve {
        sql.push(' ');
        sql.push_str(completion(preserve));
    }
    if let Some(status) = c.status {
        sql.push(' ');
        sql.push_str(status.sql(t));
    }
    if let Some(comment) = &c.comment {
        sql.push_str(&format!(" COMMENT {}", plain_text(comment, "a comment")?));
    }
    sql.push_str(&format!("\nDO {}", fragment(&c.body, "an event body")?));
    scheduler_note(plan);
    plan.push(sql);
    Ok(())
}

pub(super) fn alter(t: &Target, c: &AlterEvent, plan: &mut Plan) -> CatalogResult<()> {
    let mut parts = Vec::new();
    if let Some(schedule) = &c.schedule {
        parts.push(schedule.sql()?);
    }
    if let Some(preserve) = c.on_completion_preserve {
        parts.push(completion(preserve).to_string());
    }
    if let Some(name) = &c.rename_to {
        parts.push(format!("RENAME TO {}", object_name(&c.database, name)?));
    }
    if let Some(status) = c.status {
        parts.push(status.sql(t).to_string());
    }
    if let Some(comment) = &c.comment {
        parts.push(format!("COMMENT {}", plain_text(comment, "a comment")?));
    }
    if let Some(body) = &c.body {
        parts.push(format!("DO {}", fragment(body, "an event body")?));
    }
    if parts.is_empty() && c.definer.is_none() {
        return Err(invalid("ALTER EVENT needs at least one change"));
    }
    let mut sql = String::from("ALTER");
    if let Some(definer) = &c.definer {
        sql.push(' ');
        sql.push_str(&definer.clause()?);
    }
    sql.push_str(&format!(" EVENT {}", object_name(&c.database, &c.name)?));
    if !parts.is_empty() {
        sql.push(' ');
        sql.push_str(&parts.join(" "));
    }
    plan.push(sql);
    Ok(())
}

pub(super) fn drop(c: &DropEvent, plan: &mut Plan) -> CatalogResult<()> {
    plan.destructive();
    plan.push(format!(
        "DROP EVENT {}{}",
        if c.if_exists { "IF EXISTS " } else { "" },
        object_name(&c.database, &c.name)?
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::model::CatalogResult;

    const EVENT: &str = r#"{"change": "createEvent", "database": "d", "name": "e", "ifNotExists": true,
        "schedule": {"kind": "every", "interval": {"quantity": "1:30", "unit": "hourMinute"},
            "starts": {"base": {"kind": "currentTimestamp"}, "plus": [{"quantity": "1", "unit": "hour"}]},
            "ends": {"base": {"kind": "timestamp", "value": "2037-01-01 00:00:00"}}},
        "onCompletionPreserve": true, "status": "disableOnReplica", "comment": "cleanup",
        "body": "DELETE FROM log WHERE at < NOW() - INTERVAL 30 DAY;"}"#;

    #[test]
    fn create_per_version() -> CatalogResult<()> {
        let sql_for = |status: &str| {
            format!(
                "CREATE EVENT IF NOT EXISTS `d`.`e` ON SCHEDULE EVERY '1:30' HOUR_MINUTE STARTS CURRENT_TIMESTAMP + INTERVAL 1 HOUR ENDS '2037-01-01 00:00:00' ON COMPLETION PRESERVE {status} COMMENT 'cleanup'\nDO DELETE FROM log WHERE at < NOW() - INTERVAL 30 DAY"
            )
        };
        let plan = plan_json(&my80(), EVENT)?;
        assert_eq!(sql(&plan), sql_for("DISABLE ON SLAVE"));
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(
            sql(&plan_json(&my97(), EVENT)?),
            sql_for("DISABLE ON REPLICA")
        );
        assert_eq!(
            sql(&plan_json(&ma106(), EVENT)?),
            sql_for("DISABLE ON SLAVE")
        );
        let at = r#"{"change": "createEvent", "database": "d", "name": "once",
            "definer": {"kind": "currentUser"},
            "schedule": {"kind": "at", "at": {"base": {"kind": "currentTimestamp"}, "plus": [{"quantity": "2", "unit": "day"}]}},
            "body": "SELECT 1"}"#;
        assert_eq!(
            sql(&plan_json(&ma123(), at)?),
            "CREATE DEFINER = CURRENT_USER EVENT `d`.`once` ON SCHEDULE AT CURRENT_TIMESTAMP + INTERVAL 2 DAY\nDO SELECT 1"
        );
        let bad = at.replace(r#""quantity": "2""#, r#""quantity": "2; drop""#);
        assert!(is_invalid(&plan_json(&ma123(), &bad)));
        let bad_compound = EVENT.replace("1:30", "1:30' x");
        assert!(is_invalid(&plan_json(&my80(), &bad_compound)));
        Ok(())
    }

    #[test]
    fn alter_and_drop() -> CatalogResult<()> {
        let json = r#"{"change": "alterEvent", "database": "d", "name": "e",
            "schedule": {"kind": "at", "at": {"base": {"kind": "timestamp", "value": "2030-06-01 12:00:00"}}},
            "onCompletionPreserve": false, "renameTo": "e2", "status": "enable", "comment": "x", "body": "SELECT 2"}"#;
        assert_eq!(
            sql(&plan_json(&my97(), json)?),
            "ALTER EVENT `d`.`e` ON SCHEDULE AT '2030-06-01 12:00:00' ON COMPLETION NOT PRESERVE RENAME TO `d`.`e2` ENABLE COMMENT 'x' DO SELECT 2"
        );
        assert!(is_invalid(&plan_json(
            &my97(),
            r#"{"change": "alterEvent", "database": "d", "name": "e"}"#
        )));
        let drop = plan_json(
            &ma106(),
            r#"{"change": "dropEvent", "database": "d", "name": "e", "ifExists": true}"#,
        )?;
        assert_eq!(sql(&drop), "DROP EVENT IF EXISTS `d`.`e`");
        assert!(drop.destructive);
        Ok(())
    }
}
