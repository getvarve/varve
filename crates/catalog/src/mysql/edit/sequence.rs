//! MariaDB sequences (10.3+). MySQL has none.

use serde::{Deserialize, Serialize};

use super::types::TypeName;
use super::{Plan, Target, invalid, object_name, plain_text};
use crate::model::CatalogResult;

/// A sequence value: any signed or unsigned 64-bit integer (MariaDB
/// sequences range from the signed `BIGINT` minimum to the unsigned
/// `BIGINT` maximum). Serialized as decimal text, because JavaScript numbers
/// are exact only up to 2^53 - 1; read from decimal text or a JSON integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SequenceNumber(pub i128);

impl SequenceNumber {
    const MIN: i128 = i64::MIN as i128;
    const MAX: i128 = u64::MAX as i128;
}

impl std::fmt::Display for SequenceNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<i64> for SequenceNumber {
    fn from(value: i64) -> Self {
        Self(i128::from(value))
    }
}

impl Serialize for SequenceNumber {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if (Self::MIN..=Self::MAX).contains(&self.0) {
            serializer.serialize_str(&self.0.to_string())
        } else {
            Err(serde::ser::Error::custom(
                "a sequence value is a 64-bit integer",
            ))
        }
    }
}

impl<'de> Deserialize<'de> for SequenceNumber {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = SequenceNumber;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a 64-bit integer as decimal text or a JSON integer")
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(SequenceNumber(i128::from(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(SequenceNumber(i128::from(v)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                let t = v.trim();
                let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(E::custom(format!("{v:?} is not a whole number")));
                }
                t.parse::<i128>()
                    .map(SequenceNumber)
                    .map_err(|_| E::custom("a sequence value is a 64-bit integer"))
            }
        }
        let value = deserializer.deserialize_any(Visitor)?;
        if !(SequenceNumber::MIN..=SequenceNumber::MAX).contains(&value.0) {
            return Err(serde::de::Error::custom(
                "a sequence value is a 64-bit integer",
            ));
        }
        Ok(value)
    }
}

/// `Option<u64>` as decimal text (read from text or a JSON integer).
mod opt_u64_text {
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrText {
        Int(u64),
        Text(String),
    }

    #[allow(clippy::ref_option)]
    pub(super) fn serialize<S: Serializer>(
        value: &Option<u64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => serializer.serialize_str(&v.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        match Option::<IntOrText>::deserialize(deserializer)? {
            None => Ok(None),
            Some(IntOrText::Int(v)) => Ok(Some(v)),
            Some(IntOrText::Text(t)) => {
                let trimmed = t.trim();
                let digits = trimmed.strip_prefix('+').unwrap_or(trimmed);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(serde::de::Error::custom(format!(
                        "{t:?} is not a whole number of at least 0"
                    )));
                }
                digits.parse().map(Some).map_err(|_| {
                    serde::de::Error::custom(format!("{t} is above the 64-bit unsigned maximum"))
                })
            }
        }
    }
}

/// MariaDB 11.5+: the integer type of a sequence (`AS type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SequenceType {
    /// An integer type: `tinyint`, `smallint`, `mediumint`, `int`, `bigint`.
    pub name: TypeName,
    /// `UNSIGNED`.
    #[serde(default)]
    pub unsigned: bool,
}

/// A minimum or maximum value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SequenceLimit {
    /// `NO MINVALUE` / `NO MAXVALUE`: the type's limit.
    Unbounded,
    /// A value.
    Value {
        /// The limit.
        value: SequenceNumber,
    },
}

/// `RESTART` of `ALTER SEQUENCE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SequenceRestart {
    /// `RESTART`: back to the start value.
    Start,
    /// `RESTART WITH value`.
    With {
        /// Next value.
        value: SequenceNumber,
    },
}

/// `CREATE SEQUENCE` (MariaDB).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSequence {
    /// Database.
    pub database: String,
    /// Sequence name.
    pub name: String,
    /// `OR REPLACE`.
    #[serde(default)]
    pub replace: bool,
    /// `IF NOT EXISTS`.
    #[serde(default)]
    pub if_not_exists: bool,
    /// MariaDB 11.5+: `AS type`.
    #[serde(default)]
    pub data_type: Option<SequenceType>,
    /// `START WITH`.
    #[serde(default)]
    pub start: Option<SequenceNumber>,
    /// `INCREMENT BY` (0 uses `auto_increment_increment`).
    #[serde(default)]
    pub increment: Option<SequenceNumber>,
    /// `MINVALUE`.
    #[serde(default)]
    pub min_value: Option<SequenceLimit>,
    /// `MAXVALUE`.
    #[serde(default)]
    pub max_value: Option<SequenceLimit>,
    /// `CACHE n`; 0 is `NOCACHE` (decimal text in JSON).
    #[serde(default, with = "opt_u64_text")]
    pub cache: Option<u64>,
    /// `CYCLE` / `NOCYCLE`.
    #[serde(default)]
    pub cycle: Option<bool>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// `ALTER SEQUENCE` (MariaDB). Fields left `None` are not changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlterSequence {
    /// Database.
    pub database: String,
    /// Sequence name.
    pub name: String,
    /// MariaDB 11.5+: `AS type`.
    #[serde(default)]
    pub data_type: Option<SequenceType>,
    /// `START WITH` (used by a later `RESTART`).
    #[serde(default)]
    pub start: Option<SequenceNumber>,
    /// `INCREMENT BY`.
    #[serde(default)]
    pub increment: Option<SequenceNumber>,
    /// `MINVALUE`.
    #[serde(default)]
    pub min_value: Option<SequenceLimit>,
    /// `MAXVALUE`.
    #[serde(default)]
    pub max_value: Option<SequenceLimit>,
    /// `CACHE n`; 0 is `NOCACHE` (decimal text in JSON).
    #[serde(default, with = "opt_u64_text")]
    pub cache: Option<u64>,
    /// `CYCLE` / `NOCYCLE`.
    #[serde(default)]
    pub cycle: Option<bool>,
    /// `RESTART`.
    #[serde(default)]
    pub restart: Option<SequenceRestart>,
}

/// `DROP SEQUENCE` (MariaDB).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropSequence {
    /// Database.
    pub database: String,
    /// Sequence name.
    pub name: String,
    /// `IF EXISTS`.
    #[serde(default)]
    pub if_exists: bool,
}

/// `SELECT SETVAL(seq, value, is_used, round)` (MariaDB).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSequenceValue {
    /// Database.
    pub database: String,
    /// Sequence name.
    pub name: String,
    /// The value.
    pub value: SequenceNumber,
    /// True: `value` counts as used and the next value follows it; false:
    /// the next value is `value` itself.
    #[serde(default = "yes")]
    pub is_used: bool,
    /// Cycle round (decimal text in JSON).
    #[serde(default, with = "opt_u64_text")]
    pub round: Option<u64>,
}

fn yes() -> bool {
    true
}

fn require(t: &Target) -> CatalogResult<()> {
    if t.mariadb_at(10, 3) {
        Ok(())
    } else {
        Err(t.lacks("sequences (MariaDB 10.3+)"))
    }
}

struct Options<'a> {
    data_type: Option<&'a SequenceType>,
    start: Option<SequenceNumber>,
    increment: Option<SequenceNumber>,
    min_value: Option<SequenceLimit>,
    max_value: Option<SequenceLimit>,
    cache: Option<u64>,
    cycle: Option<bool>,
}

fn options(t: &Target, o: &Options<'_>) -> CatalogResult<Vec<String>> {
    let mut out = Vec::new();
    if let Some(ty) = o.data_type {
        if !t.mariadb_at(11, 5) {
            return Err(t.lacks("sequence data types (MariaDB 11.5+)"));
        }
        if !ty.name.is_integer() {
            return Err(invalid("a sequence type is an integer type"));
        }
        let keyword = match ty.name {
            TypeName::Tinyint => "TINYINT",
            TypeName::Smallint => "SMALLINT",
            TypeName::Mediumint => "MEDIUMINT",
            TypeName::Int => "INT",
            _ => "BIGINT",
        };
        out.push(format!(
            "AS {keyword}{}",
            if ty.unsigned { " UNSIGNED" } else { "" }
        ));
    }
    if let Some(start) = o.start {
        out.push(format!("START WITH {start}"));
    }
    if let Some(increment) = o.increment {
        out.push(format!("INCREMENT BY {increment}"));
    }
    if let Some(limit) = o.min_value {
        out.push(match limit {
            SequenceLimit::Unbounded => "NO MINVALUE".into(),
            SequenceLimit::Value { value } => format!("MINVALUE {value}"),
        });
    }
    if let Some(limit) = o.max_value {
        out.push(match limit {
            SequenceLimit::Unbounded => "NO MAXVALUE".into(),
            SequenceLimit::Value { value } => format!("MAXVALUE {value}"),
        });
    }
    if let (Some(SequenceLimit::Value { value: min }), Some(SequenceLimit::Value { value: max })) =
        (o.min_value, o.max_value)
        && min >= max
    {
        return Err(invalid("MINVALUE must be below MAXVALUE"));
    }
    if let Some(cache) = o.cache {
        out.push(if cache == 0 {
            "NOCACHE".into()
        } else {
            format!("CACHE {cache}")
        });
    }
    if let Some(cycle) = o.cycle {
        out.push(if cycle { "CYCLE" } else { "NOCYCLE" }.into());
    }
    Ok(out)
}

pub(super) fn create(t: &Target, c: &CreateSequence, plan: &mut Plan) -> CatalogResult<()> {
    require(t)?;
    if c.replace && c.if_not_exists {
        return Err(invalid("OR REPLACE and IF NOT EXISTS exclude each other"));
    }
    let mut sql = String::from("CREATE ");
    if c.replace {
        sql.push_str("OR REPLACE ");
        plan.destructive();
        plan.warn("replacing a sequence resets its current value");
    }
    sql.push_str("SEQUENCE ");
    if c.if_not_exists {
        sql.push_str("IF NOT EXISTS ");
    }
    sql.push_str(&object_name(&c.database, &c.name)?);
    for option in options(
        t,
        &Options {
            data_type: c.data_type.as_ref(),
            start: c.start,
            increment: c.increment,
            min_value: c.min_value,
            max_value: c.max_value,
            cache: c.cache,
            cycle: c.cycle,
        },
    )? {
        sql.push(' ');
        sql.push_str(&option);
    }
    if let Some(comment) = &c.comment {
        sql.push_str(&format!(" COMMENT={}", plain_text(comment, "a comment")?));
    }
    plan.push(sql);
    Ok(())
}

pub(super) fn alter(t: &Target, c: &AlterSequence, plan: &mut Plan) -> CatalogResult<()> {
    require(t)?;
    let name = object_name(&c.database, &c.name)?;
    let mut parts = options(
        t,
        &Options {
            data_type: None,
            start: c.start,
            increment: c.increment,
            min_value: c.min_value,
            max_value: c.max_value,
            cache: c.cache,
            cycle: c.cycle,
        },
    )?;
    if let Some(restart) = c.restart {
        plan.destructive();
        plan.warn("RESTART moves the sequence: values it hands out again may already be in use");
        parts.push(match restart {
            SequenceRestart::Start => "RESTART".into(),
            SequenceRestart::With { value } => format!("RESTART WITH {value}"),
        });
    }
    // MariaDB takes AS <type> only alone, after the limits fit the type.
    let retype = match &c.data_type {
        Some(ty) => {
            plan.warn(
                "the sequence's MINVALUE, MAXVALUE and current value must fit the new type, or MariaDB refuses it; set the limits in the same change",
            );
            options(
                t,
                &Options {
                    data_type: Some(ty),
                    start: None,
                    increment: None,
                    min_value: None,
                    max_value: None,
                    cache: None,
                    cycle: None,
                },
            )?
        }
        None => Vec::new(),
    };
    if parts.is_empty() && retype.is_empty() {
        return Err(invalid("ALTER SEQUENCE needs at least one change"));
    }
    if !parts.is_empty() {
        plan.push(format!("ALTER SEQUENCE {name} {}", parts.join(" ")));
    }
    if !retype.is_empty() {
        plan.push(format!("ALTER SEQUENCE {name} {}", retype.join(" ")));
    }
    Ok(())
}

pub(super) fn drop(t: &Target, c: &DropSequence, plan: &mut Plan) -> CatalogResult<()> {
    require(t)?;
    plan.destructive();
    plan.push(format!(
        "DROP SEQUENCE {}{}",
        if c.if_exists { "IF EXISTS " } else { "" },
        object_name(&c.database, &c.name)?
    ));
    Ok(())
}

pub(super) fn set_value(t: &Target, c: &SetSequenceValue, plan: &mut Plan) -> CatalogResult<()> {
    require(t)?;
    let mut args = format!(
        "{}, {}, {}",
        object_name(&c.database, &c.name)?,
        c.value,
        if c.is_used { "TRUE" } else { "FALSE" }
    );
    if let Some(round) = c.round {
        args.push_str(&format!(", {round}"));
    }
    plan.destructive();
    plan.warn(
        "SETVAL moves the sequence; it never moves backwards within a round (use ALTER SEQUENCE ... RESTART WITH for that)",
    );
    plan.push(format!("SELECT SETVAL({args})"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::model::CatalogResult;

    #[test]
    fn create_and_alter() -> CatalogResult<()> {
        let json = r#"{"change": "createSequence", "database": "d", "name": "s", "ifNotExists": true,
            "start": 10, "increment": 5, "minValue": {"kind": "value", "value": 1}, "maxValue": {"kind": "unbounded"},
            "cache": 0, "cycle": true, "comment": "ids"}"#;
        let plan = plan_json(&ma106(), json)?;
        assert_eq!(
            sql(&plan),
            "CREATE SEQUENCE IF NOT EXISTS `d`.`s` START WITH 10 INCREMENT BY 5 MINVALUE 1 NO MAXVALUE NOCACHE CYCLE COMMENT='ids'"
        );
        assert!(!plan.destructive);
        assert!(is_unsupported(&plan_json(&my97(), json)));
        let typed = r#"{"change": "createSequence", "database": "d", "name": "s", "replace": true,
            "dataType": {"name": "bigint", "unsigned": true}, "cache": 20}"#;
        let plan = plan_json(&ma123(), typed)?;
        assert_eq!(
            sql(&plan),
            "CREATE OR REPLACE SEQUENCE `d`.`s` AS BIGINT UNSIGNED CACHE 20"
        );
        assert!(plan.destructive);
        assert!(is_unsupported(&plan_json(&ma106(), typed)));
        let alter = r#"{"change": "alterSequence", "database": "d", "name": "s", "increment": -1,
            "cycle": false, "restart": {"kind": "with", "value": 50}}"#;
        let restart = plan_json(&ma106(), alter)?;
        assert_eq!(
            sql(&restart),
            "ALTER SEQUENCE `d`.`s` INCREMENT BY -1 NOCYCLE RESTART WITH 50"
        );
        assert!(restart.destructive && restart.warnings.len() == 1);
        let retype = r#"{"change": "alterSequence", "database": "d", "name": "s",
            "dataType": {"name": "int"}, "maxValue": {"kind": "value", "value": 2147483646}}"#;
        let plan = plan_json(&ma123(), retype)?;
        assert_eq!(
            plan.statements,
            vec![
                "ALTER SEQUENCE `d`.`s` MAXVALUE 2147483646".to_string(),
                "ALTER SEQUENCE `d`.`s` AS INT".to_string(),
            ]
        );
        assert!(!plan.destructive && plan.warnings.len() == 1);
        assert!(is_unsupported(&plan_json(&ma106(), retype)));
        assert!(is_invalid(&plan_json(
            &ma106(),
            r#"{"change": "alterSequence", "database": "d", "name": "s"}"#
        )));
        assert!(is_invalid(&plan_json(
            &ma106(),
            r#"{"change": "createSequence", "database": "d", "name": "s",
                "minValue": {"kind": "value", "value": 5}, "maxValue": {"kind": "value", "value": 5}}"#
        )));
        Ok(())
    }

    #[test]
    fn values_beyond_javascript_numbers_travel_as_text() -> CatalogResult<()> {
        let json = r#"{"change": "createSequence", "database": "d", "name": "s",
            "start": "9223372036854775806", "increment": "-1",
            "minValue": {"kind": "value", "value": "-9223372036854775808"},
            "maxValue": {"kind": "value", "value": "18446744073709551615"},
            "cache": "18446744073709551615"}"#;
        let plan = plan_json(&ma106(), json)?;
        assert_eq!(
            sql(&plan),
            "CREATE SEQUENCE `d`.`s` START WITH 9223372036854775806 INCREMENT BY -1 MINVALUE -9223372036854775808 MAXVALUE 18446744073709551615 CACHE 18446744073709551615"
        );
        let setval = plan_json(
            &ma123(),
            r#"{"change": "setSequenceValue", "database": "d", "name": "s", "value": "9007199254740993", "round": "2"}"#,
        )?;
        assert_eq!(
            sql(&setval),
            "SELECT SETVAL(`d`.`s`, 9007199254740993, TRUE, 2)"
        );
        let value =
            serde_json::to_value(super::SequenceNumber(9_007_199_254_740_993)).map_err(|e| {
                crate::model::CatalogError::Invalid {
                    message: e.to_string(),
                }
            })?;
        assert_eq!(value, serde_json::json!("9007199254740993"));
        for bad in [
            r#""18446744073709551616""#,
            r#""-9223372036854775809""#,
            r#""1.5""#,
            r#""""#,
        ] {
            assert!(serde_json::from_str::<super::SequenceNumber>(bad).is_err());
        }
        assert!(is_invalid(&plan_json(
            &ma106(),
            r#"{"change": "createSequence", "database": "d", "name": "s", "cache": "-1"}"#
        )));
        Ok(())
    }

    #[test]
    fn setval_and_drop() -> CatalogResult<()> {
        let plan = plan_json(
            &ma123(),
            r#"{"change": "setSequenceValue", "database": "d", "name": "s", "value": 70, "round": 0}"#,
        )?;
        assert_eq!(sql(&plan), "SELECT SETVAL(`d`.`s`, 70, TRUE, 0)");
        assert!(plan.destructive && plan.warnings.len() == 1);
        assert!(is_unsupported(&plan_json(
            &my80(),
            r#"{"change": "setSequenceValue", "database": "d", "name": "s", "value": 1}"#
        )));
        let drop = plan_json(
            &ma106(),
            r#"{"change": "dropSequence", "database": "d", "name": "s"}"#,
        )?;
        assert_eq!(sql(&drop), "DROP SEQUENCE `d`.`s`");
        assert!(drop.destructive);
        Ok(())
    }
}
