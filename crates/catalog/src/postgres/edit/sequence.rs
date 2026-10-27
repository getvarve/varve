//! Sequences, and the sequence options shared with identity columns.

use serde::{Deserialize, Serialize};
use varve_driver::ServerInfo;

use super::common::{
    DropBehavior, PlanBuilder, QualifiedName, RoleSpec, comment_on, ident, invalid, lit, require,
};
use crate::model::{CatalogResult, EditPlan};

/// Integer type of a sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum SequenceType {
    /// `smallint`.
    Smallint,
    /// `integer`.
    Integer,
    /// `bigint` (the default).
    Bigint,
}

impl SequenceType {
    fn sql(self) -> &'static str {
        match self {
            Self::Smallint => "smallint",
            Self::Integer => "integer",
            Self::Bigint => "bigint",
        }
    }
}

/// A minimum or maximum value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SequenceLimit {
    /// `NO MINVALUE` / `NO MAXVALUE`: the type's limit.
    Default,
    /// An explicit limit.
    Value {
        /// The limit (decimal text in JSON).
        #[serde(with = "super::int_text")]
        value: i64,
    },
}

/// Sequence options. For a new sequence or identity column, `None` leaves
/// the default; for an alter, `None` leaves the option unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct SequenceOptions {
    /// `AS type` (not allowed for identity columns, which use the column type).
    #[serde(default)]
    pub data_type: Option<SequenceType>,
    /// `INCREMENT BY`, never 0 (decimal text in JSON).
    #[serde(default, with = "super::int_text::option")]
    pub increment: Option<i64>,
    /// `MINVALUE`.
    #[serde(default)]
    pub min_value: Option<SequenceLimit>,
    /// `MAXVALUE`.
    #[serde(default)]
    pub max_value: Option<SequenceLimit>,
    /// `START WITH` (decimal text in JSON).
    #[serde(default, with = "super::int_text::option")]
    pub start: Option<i64>,
    /// `CACHE`, at least 1 (decimal text in JSON).
    #[serde(default, with = "super::int_text::option")]
    pub cache: Option<i64>,
    /// `CYCLE` / `NO CYCLE`.
    #[serde(default)]
    pub cycle: Option<bool>,
}

impl SequenceOptions {
    /// True when no option is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The options as `AS bigint INCREMENT BY 2 ...` (empty when none).
    pub(crate) fn sql(&self) -> CatalogResult<Vec<String>> {
        let mut parts = Vec::new();
        if let Some(t) = self.data_type {
            parts.push(format!("AS {}", t.sql()));
        }
        if let Some(i) = self.increment {
            if i == 0 {
                return Err(invalid("a sequence increment cannot be 0"));
            }
            parts.push(format!("INCREMENT BY {i}"));
        }
        match self.min_value {
            Some(SequenceLimit::Default) => parts.push("NO MINVALUE".into()),
            Some(SequenceLimit::Value { value }) => parts.push(format!("MINVALUE {value}")),
            None => {}
        }
        match self.max_value {
            Some(SequenceLimit::Default) => parts.push("NO MAXVALUE".into()),
            Some(SequenceLimit::Value { value }) => parts.push(format!("MAXVALUE {value}")),
            None => {}
        }
        if let (
            Some(SequenceLimit::Value { value: min }),
            Some(SequenceLimit::Value { value: max }),
        ) = (self.min_value, self.max_value)
            && min >= max
        {
            return Err(invalid("a sequence's minimum must be below its maximum"));
        }
        if let Some(s) = self.start {
            parts.push(format!("START WITH {s}"));
        }
        if let Some(c) = self.cache {
            if c < 1 {
                return Err(invalid("a sequence cache is at least 1"));
            }
            parts.push(format!("CACHE {c}"));
        }
        if let Some(c) = self.cycle {
            parts.push(if c { "CYCLE" } else { "NO CYCLE" }.into());
        }
        Ok(parts)
    }
}

/// A table column (for `OWNED BY`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct ColumnRef {
    /// The table.
    pub table: QualifiedName,
    /// The column.
    pub column: String,
}

/// `CREATE SEQUENCE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateSequence {
    /// Name.
    pub name: QualifiedName,
    /// `IF NOT EXISTS`.
    #[serde(default)]
    pub if_not_exists: bool,
    /// `UNLOGGED` (15+).
    #[serde(default)]
    pub unlogged: bool,
    /// Options.
    #[serde(default)]
    pub options: SequenceOptions,
    /// `OWNED BY`: dropped with this column.
    #[serde(default)]
    pub owned_by: Option<ColumnRef>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// One change of `ALTER SEQUENCE`, applied in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum SequenceOperation {
    /// Change options.
    Options {
        /// The options to change.
        options: SequenceOptions,
    },
    /// `RESTART [WITH value]`: without a value, restarts at the start value.
    Restart {
        /// The value the next `nextval` returns (decimal text in JSON).
        #[serde(default, with = "super::int_text::option")]
        with: Option<i64>,
    },
    /// `setval`: sets the current value.
    SetValue {
        /// The value (decimal text in JSON).
        #[serde(with = "super::int_text")]
        value: i64,
        /// True: the next `nextval` returns value + increment; false: it
        /// returns `value`.
        #[serde(default = "yes")]
        is_called: bool,
    },
    /// `OWNED BY column` or `OWNED BY NONE`.
    OwnedBy {
        /// The column, `None` for no owner.
        #[serde(default)]
        column: Option<ColumnRef>,
    },
    /// Rename.
    Rename {
        /// New name.
        new_name: String,
    },
    /// Move to another schema.
    SetSchema {
        /// The schema.
        schema: String,
    },
    /// `SET LOGGED` / `SET UNLOGGED` (15+).
    Logged {
        /// True for logged.
        logged: bool,
    },
    /// Change the owner.
    Owner {
        /// New owner.
        owner: RoleSpec,
    },
}

/// Warning for moving a sequence's position.
pub(crate) const RESTART_WARNING: &str = "moving a sequence back makes nextval return values it returned before, which breaks unique keys that use them";

fn yes() -> bool {
    true
}

/// Changes to sequences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SequenceChange {
    /// Create a sequence.
    Create(CreateSequence),
    /// Alter a sequence.
    Alter {
        /// The sequence.
        name: QualifiedName,
        /// Changes in order.
        operations: Vec<SequenceOperation>,
    },
    /// Drop a sequence.
    Drop {
        /// The sequence.
        name: QualifiedName,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent objects (column defaults).
        #[serde(default)]
        behavior: DropBehavior,
    },
}

pub(crate) fn owned_by_sql(column: Option<&ColumnRef>) -> CatalogResult<String> {
    Ok(match column {
        Some(c) => format!("OWNED BY {}.{}", c.table.sql()?, ident(&c.column)?),
        None => "OWNED BY NONE".into(),
    })
}

pub(crate) fn plan_sequence(
    server: &ServerInfo,
    change: &SequenceChange,
) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        SequenceChange::Create(create) => {
            let name = create.name.sql()?;
            let mut s = String::from("CREATE ");
            if create.unlogged {
                require(server, 15, "unlogged sequences")?;
                s.push_str("UNLOGGED ");
            }
            s.push_str("SEQUENCE ");
            if create.if_not_exists {
                s.push_str("IF NOT EXISTS ");
            }
            s.push_str(&name);
            let mut parts = create.options.sql()?;
            if let Some(owner) = &create.owned_by {
                parts.push(owned_by_sql(Some(owner))?);
            }
            for part in parts {
                s.push(' ');
                s.push_str(&part);
            }
            plan.push(s);
            if let Some(comment) = &create.comment {
                plan.push(comment_on(&format!("SEQUENCE {name}"), Some(comment))?);
            }
        }
        SequenceChange::Alter { name, operations } => {
            if operations.is_empty() {
                return Err(invalid("no sequence changes given"));
            }
            let mut current = name.clone();
            for op in operations {
                let seq = current.sql()?;
                match op {
                    SequenceOperation::Options { options } => {
                        if options.is_empty() {
                            return Err(invalid("no sequence options given"));
                        }
                        plan.push(format!("ALTER SEQUENCE {seq} {}", options.sql()?.join(" ")));
                    }
                    SequenceOperation::Restart { with } => {
                        plan.destructive();
                        plan.warn(RESTART_WARNING);
                        plan.push(match with {
                            Some(v) => format!("ALTER SEQUENCE {seq} RESTART WITH {v}"),
                            None => format!("ALTER SEQUENCE {seq} RESTART"),
                        });
                    }
                    SequenceOperation::SetValue { value, is_called } => {
                        plan.destructive();
                        plan.warn(RESTART_WARNING);
                        plan.warn("setval is not undone when the transaction rolls back");
                        plan.push(format!(
                            "SELECT pg_catalog.setval({}::pg_catalog.regclass, {value}, {is_called})",
                            lit(&seq)?
                        ));
                    }
                    SequenceOperation::OwnedBy { column } => plan.push(format!(
                        "ALTER SEQUENCE {seq} {}",
                        owned_by_sql(column.as_ref())?
                    )),
                    SequenceOperation::Rename { new_name } => {
                        plan.push(format!(
                            "ALTER SEQUENCE {seq} RENAME TO {}",
                            ident(new_name)?
                        ));
                        current = current.renamed(new_name);
                    }
                    SequenceOperation::SetSchema { schema } => {
                        plan.push(format!(
                            "ALTER SEQUENCE {seq} SET SCHEMA {}",
                            ident(schema)?
                        ));
                        current.schema = Some(schema.clone());
                    }
                    SequenceOperation::Logged { logged } => {
                        require(server, 15, "SET LOGGED / UNLOGGED on a sequence")?;
                        plan.push(format!(
                            "ALTER SEQUENCE {seq} SET {}",
                            if *logged { "LOGGED" } else { "UNLOGGED" }
                        ));
                    }
                    SequenceOperation::Owner { owner } => plan.push(format!(
                        "ALTER SEQUENCE {seq} OWNER TO {}",
                        owner.owner_sql(server)?
                    )),
                }
            }
        }
        SequenceChange::Drop {
            name,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            plan.push(format!(
                "DROP SEQUENCE {}{} {}",
                if *if_exists { "IF EXISTS " } else { "" },
                name.sql()?,
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

    #[test]
    fn bigint_values_travel_as_decimal_text() -> CatalogResult<()> {
        let change: super::super::PgChange = serde_json::from_value(serde_json::json!({
            "object": "sequence", "action": "alter", "name": {"schema": "s", "name": "q"},
            "operations": [
                {"op": "options", "options": {
                    "increment": "-1", "start": "9223372036854775806", "cache": "1",
                    "minValue": {"kind": "value", "value": "-9223372036854775808"},
                    "maxValue": {"kind": "value", "value": "9223372036854775806"}}},
                {"op": "restart", "with": "9007199254740993"},
                {"op": "setValue", "value": "9007199254740993", "isCalled": false}
            ]
        }))
        .map_err(|e| invalid(e.to_string()))?;
        let plan = super::super::plan(&pg(13), &change)?;
        assert_eq!(
            plan.statements,
            vec![
                r#"ALTER SEQUENCE "s"."q" INCREMENT BY -1 MINVALUE -9223372036854775808 MAXVALUE 9223372036854775806 START WITH 9223372036854775806 CACHE 1"#,
                r#"ALTER SEQUENCE "s"."q" RESTART WITH 9007199254740993"#,
                r#"SELECT pg_catalog.setval('"s"."q"'::pg_catalog.regclass, 9007199254740993, false)"#,
            ]
        );
        // Written back as text, so the UI never sees a rounded number.
        let json = serde_json::to_value(&change).map_err(|e| invalid(e.to_string()))?;
        assert_eq!(
            json["operations"][0]["options"]["maxValue"]["value"],
            serde_json::json!("9223372036854775806")
        );
        assert_eq!(
            json["operations"][1]["with"],
            serde_json::json!("9007199254740993")
        );
        Ok(())
    }

    fn all_options() -> SequenceOptions {
        SequenceOptions {
            data_type: Some(SequenceType::Integer),
            increment: Some(2),
            min_value: Some(SequenceLimit::Value { value: 1 }),
            max_value: Some(SequenceLimit::Default),
            start: Some(10),
            cache: Some(5),
            cycle: Some(true),
        }
    }

    #[test]
    fn create_sequence() -> CatalogResult<()> {
        let create = CreateSequence {
            name: QualifiedName::new("s", "q"),
            if_not_exists: true,
            unlogged: true,
            options: all_options(),
            owned_by: Some(ColumnRef {
                table: QualifiedName::new("s", "t"),
                column: "id".into(),
            }),
            comment: Some("ids".into()),
        };
        let plan = plan_sequence(&pg(15), &SequenceChange::Create(create.clone()))?;
        assert_eq!(
            plan.statements,
            vec![
                r#"CREATE UNLOGGED SEQUENCE IF NOT EXISTS "s"."q" AS integer INCREMENT BY 2 MINVALUE 1 NO MAXVALUE START WITH 10 CACHE 5 CYCLE OWNED BY "s"."t"."id""#,
                r#"COMMENT ON SEQUENCE "s"."q" IS 'ids'"#,
            ]
        );
        assert!(plan.transactional && !plan.destructive);
        assert!(plan_sequence(&pg(14), &SequenceChange::Create(create)).is_err());
        let bad = SequenceOptions {
            increment: Some(0),
            ..SequenceOptions::default()
        };
        assert!(bad.sql().is_err());
        let bad = SequenceOptions {
            min_value: Some(SequenceLimit::Value { value: 5 }),
            max_value: Some(SequenceLimit::Value { value: 5 }),
            ..SequenceOptions::default()
        };
        assert!(bad.sql().is_err());
        Ok(())
    }

    #[test]
    fn alter_and_drop_sequence() -> CatalogResult<()> {
        let plan = plan_sequence(
            &pg(15),
            &SequenceChange::Alter {
                name: QualifiedName::new("s", "q"),
                operations: vec![
                    SequenceOperation::Options {
                        options: SequenceOptions {
                            increment: Some(5),
                            min_value: Some(SequenceLimit::Default),
                            cycle: Some(false),
                            ..SequenceOptions::default()
                        },
                    },
                    SequenceOperation::Restart { with: None },
                    SequenceOperation::Restart { with: Some(100) },
                    SequenceOperation::SetValue {
                        value: 42,
                        is_called: false,
                    },
                    SequenceOperation::OwnedBy { column: None },
                    SequenceOperation::Rename {
                        new_name: "r".into(),
                    },
                    SequenceOperation::SetSchema { schema: "t".into() },
                    SequenceOperation::Logged { logged: false },
                    SequenceOperation::Owner {
                        owner: RoleSpec::name("o"),
                    },
                ],
            },
        )?;
        assert_eq!(
            plan.statements,
            vec![
                r#"ALTER SEQUENCE "s"."q" INCREMENT BY 5 NO MINVALUE NO CYCLE"#,
                r#"ALTER SEQUENCE "s"."q" RESTART"#,
                r#"ALTER SEQUENCE "s"."q" RESTART WITH 100"#,
                r#"SELECT pg_catalog.setval('"s"."q"'::pg_catalog.regclass, 42, false)"#,
                r#"ALTER SEQUENCE "s"."q" OWNED BY NONE"#,
                r#"ALTER SEQUENCE "s"."q" RENAME TO "r""#,
                r#"ALTER SEQUENCE "s"."r" SET SCHEMA "t""#,
                r#"ALTER SEQUENCE "t"."r" SET UNLOGGED"#,
                r#"ALTER SEQUENCE "t"."r" OWNER TO "o""#,
            ]
        );
        assert!(plan.destructive, "RESTART and setval reuse values");
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("moving a sequence back"))
        );
        assert!(plan.warnings.iter().any(|w| w.contains("setval")));
        assert!(
            plan_sequence(
                &pg(14),
                &SequenceChange::Alter {
                    name: QualifiedName::new("s", "q"),
                    operations: vec![SequenceOperation::Logged { logged: true }],
                },
            )
            .is_err()
        );
        let drop = plan_sequence(
            &pg(13),
            &SequenceChange::Drop {
                name: QualifiedName::new("s", "q"),
                if_exists: true,
                behavior: DropBehavior::Cascade,
            },
        )?;
        assert_eq!(
            drop.statements,
            vec![r#"DROP SEQUENCE IF EXISTS "s"."q" CASCADE"#]
        );
        assert!(drop.destructive);
        Ok(())
    }
}
