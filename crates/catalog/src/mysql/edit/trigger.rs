//! Triggers.

use serde::{Deserialize, Serialize};

use super::types::Definer;
use super::{Plan, Target, fragment, ident, invalid, object_name};
use crate::model::CatalogResult;

/// `BEFORE` or `AFTER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TriggerTiming {
    /// `BEFORE`.
    Before,
    /// `AFTER`.
    After,
}

/// The row event that fires a trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TriggerEvent {
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
}

/// Order relative to another trigger with the same timing and event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TriggerOrder {
    /// `FOLLOWS other`.
    Follows {
        /// The other trigger.
        trigger: String,
    },
    /// `PRECEDES other`.
    Precedes {
        /// The other trigger.
        trigger: String,
    },
}

/// `CREATE TRIGGER`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTrigger {
    /// Database of the trigger and its table.
    pub database: String,
    /// Table.
    pub table: String,
    /// Trigger name.
    pub name: String,
    /// Replace an existing trigger of that name: `CREATE OR REPLACE` on
    /// MariaDB, `DROP TRIGGER IF EXISTS` then `CREATE` on MySQL.
    #[serde(default)]
    pub replace: bool,
    /// `DEFINER`.
    #[serde(default)]
    pub definer: Option<Definer>,
    /// Timing.
    pub timing: TriggerTiming,
    /// Events, at least one. More than one (`INSERT OR UPDATE`) needs
    /// MariaDB 12.0+.
    pub events: Vec<TriggerEvent>,
    /// `FOLLOWS` / `PRECEDES`.
    #[serde(default)]
    pub order: Option<TriggerOrder>,
    /// The body: one statement or a `BEGIN ... END` block.
    pub body: String,
}

/// `DROP TRIGGER`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropTrigger {
    /// Database.
    pub database: String,
    /// Trigger name.
    pub name: String,
    /// `IF EXISTS`.
    #[serde(default)]
    pub if_exists: bool,
}

pub(super) fn create(t: &Target, c: &CreateTrigger, plan: &mut Plan) -> CatalogResult<()> {
    let name = object_name(&c.database, &c.name)?;
    if c.events.is_empty() {
        return Err(invalid("a trigger needs at least one event"));
    }
    if c.events.len() > 1 && !t.mariadb_at(12, 0) {
        return Err(t.lacks("triggers on more than one event (MariaDB 12.0+)"));
    }
    let mut keywords: Vec<&str> = Vec::with_capacity(c.events.len());
    for event in &c.events {
        let keyword = match event {
            TriggerEvent::Insert => "INSERT",
            TriggerEvent::Update => "UPDATE",
            TriggerEvent::Delete => "DELETE",
        };
        if keywords.contains(&keyword) {
            return Err(invalid(format!(
                "{keyword} appears twice in the trigger events"
            )));
        }
        keywords.push(keyword);
    }
    let events = keywords.join(" OR ");
    let mut sql = String::from("CREATE");
    if c.replace {
        if t.is_mariadb() {
            sql.push_str(" OR REPLACE");
        } else {
            plan.destructive();
            plan.warn(
                "MySQL has no CREATE OR REPLACE TRIGGER: the trigger is dropped and created again, and rows written in between do not fire it",
            );
            plan.push(format!("DROP TRIGGER IF EXISTS {name}"));
        }
    }
    if let Some(definer) = &c.definer {
        sql.push(' ');
        sql.push_str(&definer.clause()?);
    }
    sql.push_str(&format!(
        " TRIGGER {name} {} {} ON {} FOR EACH ROW",
        match c.timing {
            TriggerTiming::Before => "BEFORE",
            TriggerTiming::After => "AFTER",
        },
        events,
        object_name(&c.database, &c.table)?
    ));
    match &c.order {
        Some(TriggerOrder::Follows { trigger }) => {
            sql.push_str(&format!(" FOLLOWS {}", ident(trigger)?));
        }
        Some(TriggerOrder::Precedes { trigger }) => {
            sql.push_str(&format!(" PRECEDES {}", ident(trigger)?));
        }
        None => {}
    }
    sql.push_str(&format!("\n{}", fragment(&c.body, "a trigger body")?));
    plan.push(sql);
    Ok(())
}

pub(super) fn drop(c: &DropTrigger, plan: &mut Plan) -> CatalogResult<()> {
    plan.destructive();
    plan.push(format!(
        "DROP TRIGGER {}{}",
        if c.if_exists { "IF EXISTS " } else { "" },
        object_name(&c.database, &c.name)?
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::model::CatalogResult;

    const TRIGGER: &str = r#"{"change": "createTrigger", "database": "d", "table": "t", "name": "trg", "replace": true,
        "timing": "before", "events": ["update"], "order": {"kind": "follows", "trigger": "other"},
        "body": "SET NEW.updated = NOW()"}"#;

    #[test]
    fn create_per_engine() -> CatalogResult<()> {
        let tail = " TRIGGER `d`.`trg` BEFORE UPDATE ON `d`.`t` FOR EACH ROW FOLLOWS `other`\nSET NEW.updated = NOW()";
        let mysql = plan_json(&my97(), TRIGGER)?;
        assert_eq!(
            mysql.statements,
            vec![
                "DROP TRIGGER IF EXISTS `d`.`trg`".to_string(),
                format!("CREATE{tail}"),
            ]
        );
        assert!(mysql.destructive);
        let maria = plan_json(&ma106(), TRIGGER)?;
        assert_eq!(maria.statements, vec![format!("CREATE OR REPLACE{tail}")]);
        assert!(!maria.destructive);
        let plain = r#"{"change": "createTrigger", "database": "d", "table": "t", "name": "trg",
            "definer": {"kind": "account", "user": "app", "host": "localhost"},
            "timing": "after", "events": ["delete"], "order": {"kind": "precedes", "trigger": "x"},
            "body": "BEGIN DELETE FROM log WHERE id = OLD.id; END"}"#;
        assert_eq!(
            sql(&plan_json(&my80(), plain)?),
            "CREATE DEFINER = 'app'@'localhost' TRIGGER `d`.`trg` AFTER DELETE ON `d`.`t` FOR EACH ROW PRECEDES `x`\nBEGIN DELETE FROM log WHERE id = OLD.id; END"
        );
        Ok(())
    }

    #[test]
    fn multi_event_triggers_need_mariadb_12() -> CatalogResult<()> {
        let json = r#"{"change": "createTrigger", "database": "d", "table": "t", "name": "trg",
            "timing": "after", "events": ["insert", "update", "delete"], "body": "SET @n = 1"}"#;
        assert_eq!(
            sql(&plan_json(&ma123(), json)?),
            "CREATE TRIGGER `d`.`trg` AFTER INSERT OR UPDATE OR DELETE ON `d`.`t` FOR EACH ROW\nSET @n = 1"
        );
        assert!(is_unsupported(&plan_json(&ma106(), json)));
        assert!(is_unsupported(&plan_json(&my97(), json)));
        let twice = json.replace(
            r#"["insert", "update", "delete"]"#,
            r#"["insert", "insert"]"#,
        );
        assert!(is_invalid(&plan_json(&ma123(), &twice)));
        let none = json.replace(r#"["insert", "update", "delete"]"#, "[]");
        assert!(is_invalid(&plan_json(&ma123(), &none)));
        Ok(())
    }

    #[test]
    fn drop_trigger() -> CatalogResult<()> {
        let plan = plan_json(
            &ma123(),
            r#"{"change": "dropTrigger", "database": "d", "name": "trg"}"#,
        )?;
        assert_eq!(sql(&plan), "DROP TRIGGER `d`.`trg`");
        assert!(plan.destructive);
        Ok(())
    }
}
