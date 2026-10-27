//! Triggers and event triggers.

use serde::{Deserialize, Serialize};
use varve_driver::ServerInfo;

use super::common::{
    DropBehavior, FiringState, PlanBuilder, QualifiedName, RoleSpec, comment_on, expr, ident,
    ident_list, invalid, lit, require,
};
use crate::model::{CatalogResult, EditPlan};

/// When a trigger fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum TriggerTiming {
    /// `BEFORE`.
    Before,
    /// `AFTER`.
    After,
    /// `INSTEAD OF` (views, row level).
    InsteadOf,
}

/// An event a trigger fires on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TriggerEvent {
    /// `INSERT`.
    Insert,
    /// `UPDATE [OF columns]`.
    Update {
        /// Only when these columns are updated; empty for any.
        #[serde(default)]
        columns: Vec<String>,
    },
    /// `DELETE`.
    Delete,
    /// `TRUNCATE` (statement level).
    Truncate,
}

/// Row or statement level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum TriggerLevel {
    /// `FOR EACH ROW`.
    Row,
    /// `FOR EACH STATEMENT`.
    Statement,
}

/// `CREATE [OR REPLACE] [CONSTRAINT] TRIGGER`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateTrigger {
    /// Trigger name.
    pub name: String,
    /// The table or view.
    pub table: QualifiedName,
    /// `OR REPLACE` (14+).
    #[serde(default)]
    pub or_replace: bool,
    /// `CONSTRAINT TRIGGER` (after, row level, can be deferred).
    #[serde(default)]
    pub constraint: bool,
    /// Timing.
    pub timing: TriggerTiming,
    /// Events, at least one.
    pub events: Vec<TriggerEvent>,
    /// Level.
    pub level: TriggerLevel,
    /// `REFERENCING OLD TABLE AS` (after triggers).
    #[serde(default)]
    pub old_table: Option<String>,
    /// `REFERENCING NEW TABLE AS` (after triggers).
    #[serde(default)]
    pub new_table: Option<String>,
    /// `WHEN (condition)`.
    #[serde(default)]
    pub condition: Option<String>,
    /// The trigger function.
    pub function: QualifiedName,
    /// Arguments passed to the function (as string literals).
    #[serde(default)]
    pub arguments: Vec<String>,
    /// `DEFERRABLE` (constraint triggers).
    #[serde(default)]
    pub deferrable: bool,
    /// `INITIALLY DEFERRED` (constraint triggers).
    #[serde(default)]
    pub initially_deferred: bool,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// Changes to triggers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TriggerChange {
    /// Create a trigger.
    Create(CreateTrigger),
    /// Enable or disable a trigger.
    SetState {
        /// Trigger.
        name: String,
        /// Table.
        table: QualifiedName,
        /// New state.
        state: FiringState,
    },
    /// Rename.
    Rename {
        /// Trigger.
        name: String,
        /// Table.
        table: QualifiedName,
        /// New name.
        new_name: String,
    },
    /// Drop.
    Drop {
        /// Trigger.
        name: String,
        /// Table.
        table: QualifiedName,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent objects.
        #[serde(default)]
        behavior: DropBehavior,
    },
}

pub(crate) fn function_call(
    function: &QualifiedName,
    arguments: &[String],
) -> CatalogResult<String> {
    let args = arguments
        .iter()
        .map(|a| lit(a))
        .collect::<CatalogResult<Vec<_>>>()?;
    Ok(format!("{}({})", function.sql()?, args.join(", ")))
}

pub(crate) fn plan_trigger(server: &ServerInfo, change: &TriggerChange) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        TriggerChange::Create(t) => {
            if t.events.is_empty() {
                return Err(invalid("a trigger needs at least one event"));
            }
            let row = t.level == TriggerLevel::Row;
            if t.timing == TriggerTiming::InsteadOf && !row {
                return Err(invalid("INSTEAD OF triggers fire for each row"));
            }
            if t.timing == TriggerTiming::InsteadOf && t.condition.is_some() {
                return Err(invalid("INSTEAD OF triggers cannot have a WHEN condition"));
            }
            if t.events.contains(&TriggerEvent::Truncate) && row {
                return Err(invalid("TRUNCATE triggers fire for each statement"));
            }
            if t.constraint && (t.timing != TriggerTiming::After || !row) {
                return Err(invalid("constraint triggers are AFTER ... FOR EACH ROW"));
            }
            if t.constraint && (t.old_table.is_some() || t.new_table.is_some()) {
                return Err(invalid("constraint triggers cannot have transition tables"));
            }
            if t.constraint && t.or_replace {
                return Err(invalid(
                    "a constraint trigger cannot be created with OR REPLACE; drop it and create it again",
                ));
            }
            if (t.deferrable || t.initially_deferred) && !t.constraint {
                return Err(invalid("only constraint triggers can be deferred"));
            }
            if t.initially_deferred && !t.deferrable {
                return Err(invalid("INITIALLY DEFERRED needs DEFERRABLE"));
            }
            if (t.old_table.is_some() || t.new_table.is_some()) && t.timing != TriggerTiming::After
            {
                return Err(invalid(
                    "transition tables are only available to AFTER triggers",
                ));
            }
            let mut s = String::from("CREATE ");
            if t.or_replace {
                require(server, 14, "CREATE OR REPLACE TRIGGER")?;
                s.push_str("OR REPLACE ");
            }
            if t.constraint {
                s.push_str("CONSTRAINT ");
            }
            s.push_str(&format!("TRIGGER {} ", ident(&t.name)?));
            s.push_str(match t.timing {
                TriggerTiming::Before => "BEFORE ",
                TriggerTiming::After => "AFTER ",
                TriggerTiming::InsteadOf => "INSTEAD OF ",
            });
            let events = t
                .events
                .iter()
                .map(|e| {
                    Ok(match e {
                        TriggerEvent::Insert => "INSERT".to_owned(),
                        TriggerEvent::Update { columns } if columns.is_empty() => {
                            "UPDATE".to_owned()
                        }
                        TriggerEvent::Update { columns } => {
                            format!("UPDATE OF {}", ident_list(columns)?)
                        }
                        TriggerEvent::Delete => "DELETE".to_owned(),
                        TriggerEvent::Truncate => "TRUNCATE".to_owned(),
                    })
                })
                .collect::<CatalogResult<Vec<_>>>()?;
            s.push_str(&events.join(" OR "));
            s.push_str(&format!("\nON {}", t.table.sql()?));
            if t.deferrable {
                s.push_str(" DEFERRABLE");
                if t.initially_deferred {
                    s.push_str(" INITIALLY DEFERRED");
                }
            }
            if t.old_table.is_some() || t.new_table.is_some() {
                s.push_str("\nREFERENCING");
                if let Some(o) = &t.old_table {
                    s.push_str(&format!(" OLD TABLE AS {}", ident(o)?));
                }
                if let Some(n) = &t.new_table {
                    s.push_str(&format!(" NEW TABLE AS {}", ident(n)?));
                }
            }
            s.push_str(if row {
                "\nFOR EACH ROW"
            } else {
                "\nFOR EACH STATEMENT"
            });
            if let Some(c) = &t.condition {
                s.push_str(&format!("\nWHEN ({})", expr(c, "a trigger condition")?));
            }
            s.push_str(&format!(
                "\nEXECUTE FUNCTION {}",
                function_call(&t.function, &t.arguments)?
            ));
            plan.push(s);
            if let Some(comment) = &t.comment {
                plan.push(comment_on(
                    &format!("TRIGGER {} ON {}", ident(&t.name)?, t.table.sql()?),
                    Some(comment),
                )?);
            }
        }
        TriggerChange::SetState { name, table, state } => {
            plan.warn(
                "enabling or disabling a trigger takes a SHARE ROW EXCLUSIVE lock on the table",
            );
            plan.push(format!(
                "ALTER TABLE {} {} TRIGGER {}",
                table.sql()?,
                state.sql(),
                ident(name)?
            ));
        }
        TriggerChange::Rename {
            name,
            table,
            new_name,
        } => plan.push(format!(
            "ALTER TRIGGER {} ON {} RENAME TO {}",
            ident(name)?,
            table.sql()?,
            ident(new_name)?
        )),
        TriggerChange::Drop {
            name,
            table,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            plan.push(format!(
                "DROP TRIGGER {}{} ON {} {}",
                if *if_exists { "IF EXISTS " } else { "" },
                ident(name)?,
                table.sql()?,
                behavior.sql()
            ));
        }
    }
    plan.finish()
}

/// The event an event trigger fires on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum EventTriggerEvent {
    /// `ddl_command_start`.
    DdlCommandStart,
    /// `ddl_command_end`.
    DdlCommandEnd,
    /// `table_rewrite`.
    TableRewrite,
    /// `sql_drop`.
    SqlDrop,
    /// `login` (17+).
    Login,
}

/// `CREATE EVENT TRIGGER`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateEventTrigger {
    /// Name.
    pub name: String,
    /// Event.
    pub event: EventTriggerEvent,
    /// `WHEN TAG IN (...)`: command tags such as `CREATE TABLE`; empty
    /// fires for every command.
    #[serde(default)]
    pub tags: Vec<String>,
    /// The function (returns `event_trigger`).
    pub function: QualifiedName,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// Changes to event triggers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EventTriggerChange {
    /// Create an event trigger.
    Create(CreateEventTrigger),
    /// Enable or disable.
    SetState {
        /// Name.
        name: String,
        /// New state.
        state: FiringState,
    },
    /// Rename.
    Rename {
        /// Name.
        name: String,
        /// New name.
        new_name: String,
    },
    /// Change the owner (must be a superuser).
    Owner {
        /// Name.
        name: String,
        /// New owner.
        owner: RoleSpec,
    },
    /// Drop.
    Drop {
        /// Name.
        name: String,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent objects.
        #[serde(default)]
        behavior: DropBehavior,
    },
}

pub(crate) fn plan_event_trigger(
    server: &ServerInfo,
    change: &EventTriggerChange,
) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        EventTriggerChange::Create(t) => {
            let event = match t.event {
                EventTriggerEvent::DdlCommandStart => "ddl_command_start",
                EventTriggerEvent::DdlCommandEnd => "ddl_command_end",
                EventTriggerEvent::TableRewrite => "table_rewrite",
                EventTriggerEvent::SqlDrop => "sql_drop",
                EventTriggerEvent::Login => {
                    require(server, 17, "login event triggers")?;
                    plan.warn("a login event trigger that fails blocks every new connection to the database; test it before enabling it");
                    if !t.tags.is_empty() {
                        return Err(invalid("login event triggers have no command tags"));
                    }
                    "login"
                }
            };
            let mut s = format!("CREATE EVENT TRIGGER {} ON {event}", ident(&t.name)?);
            if !t.tags.is_empty() {
                let tags = t
                    .tags
                    .iter()
                    .map(|tag| lit(tag))
                    .collect::<CatalogResult<Vec<_>>>()?;
                s.push_str(&format!("\nWHEN TAG IN ({})", tags.join(", ")));
            }
            s.push_str(&format!(
                "\nEXECUTE FUNCTION {}",
                function_call(&t.function, &[])?
            ));
            plan.warn("an event trigger fires for every matching command in the whole database");
            plan.push(s);
            if let Some(comment) = &t.comment {
                plan.push(comment_on(
                    &format!("EVENT TRIGGER {}", ident(&t.name)?),
                    Some(comment),
                )?);
            }
        }
        EventTriggerChange::SetState { name, state } => plan.push(format!(
            "ALTER EVENT TRIGGER {} {}",
            ident(name)?,
            state.sql()
        )),
        EventTriggerChange::Rename { name, new_name } => plan.push(format!(
            "ALTER EVENT TRIGGER {} RENAME TO {}",
            ident(name)?,
            ident(new_name)?
        )),
        EventTriggerChange::Owner { name, owner } => plan.push(format!(
            "ALTER EVENT TRIGGER {} OWNER TO {}",
            ident(name)?,
            owner.owner_sql(server)?
        )),
        EventTriggerChange::Drop {
            name,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            plan.push(format!(
                "DROP EVENT TRIGGER {}{} {}",
                if *if_exists { "IF EXISTS " } else { "" },
                ident(name)?,
                behavior.sql()
            ));
        }
    }
    plan.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::edit::common::tests::pg;

    fn base() -> CreateTrigger {
        CreateTrigger {
            name: "trg".into(),
            table: QualifiedName::new("s", "t"),
            or_replace: false,
            constraint: false,
            timing: TriggerTiming::Before,
            events: vec![TriggerEvent::Insert],
            level: TriggerLevel::Row,
            old_table: None,
            new_table: None,
            condition: None,
            function: QualifiedName::new("s", "f"),
            arguments: vec![],
            deferrable: false,
            initially_deferred: false,
            comment: None,
        }
    }

    #[test]
    fn create_triggers() -> CatalogResult<()> {
        let t = CreateTrigger {
            or_replace: true,
            events: vec![
                TriggerEvent::Insert,
                TriggerEvent::Update {
                    columns: vec!["a".into(), "b".into()],
                },
                TriggerEvent::Delete,
            ],
            condition: Some("NEW.a IS NOT NULL".into()),
            arguments: vec!["it's".into(), "2".into()],
            comment: Some("c".into()),
            ..base()
        };
        assert!(plan_trigger(&pg(13), &TriggerChange::Create(t.clone())).is_err());
        let plan = plan_trigger(&pg(14), &TriggerChange::Create(t))?;
        assert_eq!(
            plan.statements,
            vec![
                "CREATE OR REPLACE TRIGGER \"trg\" BEFORE INSERT OR UPDATE OF \"a\", \"b\" OR DELETE\nON \"s\".\"t\"\nFOR EACH ROW\nWHEN (NEW.a IS NOT NULL)\nEXECUTE FUNCTION \"s\".\"f\"('it''s', '2')",
                r#"COMMENT ON TRIGGER "trg" ON "s"."t" IS 'c'"#,
            ]
        );
        let constraint = plan_trigger(
            &pg(13),
            &TriggerChange::Create(CreateTrigger {
                constraint: true,
                timing: TriggerTiming::After,
                deferrable: true,
                initially_deferred: true,
                ..base()
            }),
        )?;
        assert_eq!(
            constraint.statements,
            vec![
                "CREATE CONSTRAINT TRIGGER \"trg\" AFTER INSERT\nON \"s\".\"t\" DEFERRABLE INITIALLY DEFERRED\nFOR EACH ROW\nEXECUTE FUNCTION \"s\".\"f\"()"
            ]
        );
        let transition = plan_trigger(
            &pg(13),
            &TriggerChange::Create(CreateTrigger {
                timing: TriggerTiming::After,
                level: TriggerLevel::Statement,
                new_table: Some("added".into()),
                ..base()
            }),
        )?;
        assert_eq!(
            transition.statements,
            vec![
                "CREATE TRIGGER \"trg\" AFTER INSERT\nON \"s\".\"t\"\nREFERENCING NEW TABLE AS \"added\"\nFOR EACH STATEMENT\nEXECUTE FUNCTION \"s\".\"f\"()"
            ]
        );
        let bad = |t: CreateTrigger| plan_trigger(&pg(18), &TriggerChange::Create(t)).is_err();
        assert!(bad(CreateTrigger {
            events: vec![],
            ..base()
        }));
        assert!(bad(CreateTrigger {
            timing: TriggerTiming::InsteadOf,
            level: TriggerLevel::Statement,
            ..base()
        }));
        assert!(bad(CreateTrigger {
            events: vec![TriggerEvent::Truncate],
            ..base()
        }));
        assert!(bad(CreateTrigger {
            constraint: true,
            ..base()
        }));
        assert!(bad(CreateTrigger {
            deferrable: true,
            ..base()
        }));
        assert!(bad(CreateTrigger {
            old_table: Some("o".into()),
            ..base()
        }));
        let constraint = CreateTrigger {
            constraint: true,
            timing: TriggerTiming::After,
            ..base()
        };
        assert!(bad(CreateTrigger {
            new_table: Some("n".into()),
            ..constraint.clone()
        }));
        assert!(bad(CreateTrigger {
            or_replace: true,
            ..constraint
        }));
        Ok(())
    }

    #[test]
    fn trigger_state_rename_drop() -> CatalogResult<()> {
        let t = QualifiedName::new("s", "t");
        let states = [
            (FiringState::Disabled, "DISABLE"),
            (FiringState::Enabled, "ENABLE"),
            (FiringState::Replica, "ENABLE REPLICA"),
            (FiringState::Always, "ENABLE ALWAYS"),
        ];
        for (state, word) in states {
            let plan = plan_trigger(
                &pg(13),
                &TriggerChange::SetState {
                    name: "trg".into(),
                    table: t.clone(),
                    state,
                },
            )?;
            assert_eq!(
                plan.statements,
                vec![format!(r#"ALTER TABLE "s"."t" {word} TRIGGER "trg""#)]
            );
        }
        let rename = plan_trigger(
            &pg(13),
            &TriggerChange::Rename {
                name: "trg".into(),
                table: t.clone(),
                new_name: "x".into(),
            },
        )?;
        assert_eq!(
            rename.statements,
            vec![r#"ALTER TRIGGER "trg" ON "s"."t" RENAME TO "x""#]
        );
        let drop = plan_trigger(
            &pg(13),
            &TriggerChange::Drop {
                name: "trg".into(),
                table: t,
                if_exists: true,
                behavior: DropBehavior::Restrict,
            },
        )?;
        assert_eq!(
            drop.statements,
            vec![r#"DROP TRIGGER IF EXISTS "trg" ON "s"."t" RESTRICT"#]
        );
        assert!(drop.destructive);
        Ok(())
    }

    #[test]
    fn event_triggers() -> CatalogResult<()> {
        let create = CreateEventTrigger {
            name: "e".into(),
            event: EventTriggerEvent::DdlCommandEnd,
            tags: vec!["CREATE TABLE".into(), "ALTER TABLE".into()],
            function: QualifiedName::new("s", "f"),
            comment: Some("c".into()),
        };
        let plan = plan_event_trigger(&pg(13), &EventTriggerChange::Create(create))?;
        assert_eq!(
            plan.statements,
            vec![
                "CREATE EVENT TRIGGER \"e\" ON ddl_command_end\nWHEN TAG IN ('CREATE TABLE', 'ALTER TABLE')\nEXECUTE FUNCTION \"s\".\"f\"()",
                r#"COMMENT ON EVENT TRIGGER "e" IS 'c'"#,
            ]
        );
        let login = CreateEventTrigger {
            name: "l".into(),
            event: EventTriggerEvent::Login,
            tags: vec![],
            function: QualifiedName::new("s", "f"),
            comment: None,
        };
        assert!(plan_event_trigger(&pg(16), &EventTriggerChange::Create(login.clone())).is_err());
        assert_eq!(
            plan_event_trigger(&pg(17), &EventTriggerChange::Create(login))?.statements,
            vec!["CREATE EVENT TRIGGER \"l\" ON login\nEXECUTE FUNCTION \"s\".\"f\"()"]
        );
        let others = [
            (
                EventTriggerChange::SetState {
                    name: "e".into(),
                    state: FiringState::Disabled,
                },
                r#"ALTER EVENT TRIGGER "e" DISABLE"#,
            ),
            (
                EventTriggerChange::Rename {
                    name: "e".into(),
                    new_name: "f".into(),
                },
                r#"ALTER EVENT TRIGGER "e" RENAME TO "f""#,
            ),
            (
                EventTriggerChange::Owner {
                    name: "e".into(),
                    owner: RoleSpec::name("o"),
                },
                r#"ALTER EVENT TRIGGER "e" OWNER TO "o""#,
            ),
            (
                EventTriggerChange::Drop {
                    name: "e".into(),
                    if_exists: true,
                    behavior: DropBehavior::Cascade,
                },
                r#"DROP EVENT TRIGGER IF EXISTS "e" CASCADE"#,
            ),
        ];
        for (change, sql) in others {
            assert_eq!(plan_event_trigger(&pg(13), &change)?.statements, vec![sql]);
        }
        Ok(())
    }
}
