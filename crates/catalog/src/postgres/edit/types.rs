//! User-defined types: enums, composite types, range types and domains.

use serde::{Deserialize, Serialize};
use varve_driver::ServerInfo;

use super::common::{
    DropBehavior, PlanBuilder, QualifiedName, RoleSpec, comment_on, data_type, expr, ident,
    invalid, lit, paren_expr, require,
};
use crate::model::{CatalogResult, EditPlan};

/// An attribute of a composite type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CompositeAttribute {
    /// Name.
    pub name: String,
    /// Type as SQL.
    pub data_type: String,
    /// `COLLATE`.
    #[serde(default)]
    pub collation: Option<String>,
}

impl CompositeAttribute {
    fn sql(&self) -> CatalogResult<String> {
        let mut s = format!(
            "{} {}",
            ident(&self.name)?,
            data_type(&self.data_type, "an attribute type")?
        );
        if let Some(c) = &self.collation {
            s.push_str(&format!(" COLLATE {}", ident(c)?));
        }
        Ok(s)
    }
}

/// Where a new enum value goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EnumPosition {
    /// `BEFORE label`.
    Before {
        /// Existing label.
        label: String,
    },
    /// `AFTER label`.
    After {
        /// Existing label.
        label: String,
    },
}

/// One change of a composite type, applied in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum CompositeOperation {
    /// `ADD ATTRIBUTE`.
    AddAttribute {
        /// The attribute.
        attribute: CompositeAttribute,
        /// `CASCADE`: also alter typed tables using the type.
        #[serde(default)]
        cascade: bool,
    },
    /// `DROP ATTRIBUTE`.
    DropAttribute {
        /// Attribute.
        name: String,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// `CASCADE`: also alter typed tables using the type.
        #[serde(default)]
        cascade: bool,
    },
    /// `RENAME ATTRIBUTE`.
    RenameAttribute {
        /// Attribute.
        name: String,
        /// New name.
        new_name: String,
        /// `CASCADE`: also alter typed tables using the type.
        #[serde(default)]
        cascade: bool,
    },
    /// `ALTER ATTRIBUTE ... TYPE`.
    AlterAttributeType {
        /// Attribute.
        name: String,
        /// New type.
        data_type: String,
        /// `COLLATE`.
        #[serde(default)]
        collation: Option<String>,
        /// `CASCADE`: also alter typed tables using the type.
        #[serde(default)]
        cascade: bool,
    },
}

/// `CREATE TYPE ... AS RANGE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateRangeType {
    /// Name.
    pub name: QualifiedName,
    /// Element type as SQL.
    pub subtype: String,
    /// Operator class of the subtype.
    #[serde(default)]
    pub subtype_opclass: Option<QualifiedName>,
    /// Collation of the subtype.
    #[serde(default)]
    pub collation: Option<String>,
    /// Canonical function (discrete ranges).
    #[serde(default)]
    pub canonical: Option<QualifiedName>,
    /// Difference function (for GiST efficiency).
    #[serde(default)]
    pub subtype_diff: Option<QualifiedName>,
    /// Name of the multirange type (14+).
    #[serde(default)]
    pub multirange_type_name: Option<QualifiedName>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// Changes to enum, composite and range types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TypeChange {
    /// `CREATE TYPE ... AS ENUM`.
    CreateEnum {
        /// Name.
        name: QualifiedName,
        /// Labels in order.
        labels: Vec<String>,
        /// Comment.
        #[serde(default)]
        comment: Option<String>,
    },
    /// `ALTER TYPE ... ADD VALUE`.
    AddEnumValue {
        /// The enum.
        name: QualifiedName,
        /// New label.
        value: String,
        /// `IF NOT EXISTS`.
        #[serde(default)]
        if_not_exists: bool,
        /// Position; at the end when absent.
        #[serde(default)]
        position: Option<EnumPosition>,
    },
    /// `ALTER TYPE ... RENAME VALUE`.
    RenameEnumValue {
        /// The enum.
        name: QualifiedName,
        /// Existing label.
        value: String,
        /// New label.
        new_value: String,
    },
    /// `CREATE TYPE ... AS (...)`.
    CreateComposite {
        /// Name.
        name: QualifiedName,
        /// Attributes.
        attributes: Vec<CompositeAttribute>,
        /// Comment.
        #[serde(default)]
        comment: Option<String>,
    },
    /// Alter a composite type.
    AlterComposite {
        /// The type.
        name: QualifiedName,
        /// Changes in order.
        operations: Vec<CompositeOperation>,
    },
    /// `CREATE TYPE ... AS RANGE`.
    CreateRange(Box<CreateRangeType>),
    /// Rename the type.
    Rename {
        /// The type.
        name: QualifiedName,
        /// New name.
        new_name: String,
    },
    /// Move the type to another schema.
    SetSchema {
        /// The type.
        name: QualifiedName,
        /// Schema.
        schema: String,
    },
    /// Change the owner.
    Owner {
        /// The type.
        name: QualifiedName,
        /// New owner.
        owner: RoleSpec,
    },
    /// Drop the type.
    Drop {
        /// The type.
        name: QualifiedName,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent columns and functions.
        #[serde(default)]
        behavior: DropBehavior,
    },
}

fn cascade(c: bool) -> &'static str {
    if c { " CASCADE" } else { "" }
}

pub(crate) fn plan_type(server: &ServerInfo, change: &TypeChange) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        TypeChange::CreateEnum {
            name,
            labels,
            comment,
        } => {
            let labels = labels
                .iter()
                .map(|l| lit(l))
                .collect::<CatalogResult<Vec<_>>>()?;
            let n = name.sql()?;
            plan.push(format!("CREATE TYPE {n} AS ENUM ({})", labels.join(", ")));
            if let Some(c) = comment {
                plan.push(comment_on(&format!("TYPE {n}"), Some(c))?);
            }
        }
        TypeChange::AddEnumValue {
            name,
            value,
            if_not_exists,
            position,
        } => {
            plan.warn(
                "a value added inside a transaction cannot be used until that transaction commits",
            );
            let mut s = format!(
                "ALTER TYPE {} ADD VALUE {}{}",
                name.sql()?,
                if *if_not_exists { "IF NOT EXISTS " } else { "" },
                lit(value)?
            );
            match position {
                Some(EnumPosition::Before { label }) => {
                    s.push_str(&format!(" BEFORE {}", lit(label)?));
                }
                Some(EnumPosition::After { label }) => {
                    s.push_str(&format!(" AFTER {}", lit(label)?));
                }
                None => {}
            }
            plan.push(s);
        }
        TypeChange::RenameEnumValue {
            name,
            value,
            new_value,
        } => plan.push(format!(
            "ALTER TYPE {} RENAME VALUE {} TO {}",
            name.sql()?,
            lit(value)?,
            lit(new_value)?
        )),
        TypeChange::CreateComposite {
            name,
            attributes,
            comment,
        } => {
            let attrs = attributes
                .iter()
                .map(CompositeAttribute::sql)
                .collect::<CatalogResult<Vec<_>>>()?;
            let n = name.sql()?;
            plan.push(format!("CREATE TYPE {n} AS ({})", attrs.join(", ")));
            if let Some(c) = comment {
                plan.push(comment_on(&format!("TYPE {n}"), Some(c))?);
            }
        }
        TypeChange::AlterComposite { name, operations } => {
            if operations.is_empty() {
                return Err(invalid("no type changes given"));
            }
            let alter = format!("ALTER TYPE {}", name.sql()?);
            for op in operations {
                let statement = match op {
                    CompositeOperation::AddAttribute {
                        attribute,
                        cascade: c,
                    } => {
                        format!("{alter} ADD ATTRIBUTE {}{}", attribute.sql()?, cascade(*c))
                    }
                    CompositeOperation::DropAttribute {
                        name,
                        if_exists,
                        cascade: c,
                    } => {
                        plan.destructive();
                        format!(
                            "{alter} DROP ATTRIBUTE {}{}{}",
                            if *if_exists { "IF EXISTS " } else { "" },
                            ident(name)?,
                            cascade(*c)
                        )
                    }
                    CompositeOperation::RenameAttribute {
                        name,
                        new_name,
                        cascade: c,
                    } => format!(
                        "{alter} RENAME ATTRIBUTE {} TO {}{}",
                        ident(name)?,
                        ident(new_name)?,
                        cascade(*c)
                    ),
                    CompositeOperation::AlterAttributeType {
                        name,
                        data_type,
                        collation,
                        cascade: c,
                    } => {
                        plan.destructive();
                        plan.warn("changing an attribute type can lose data in typed tables that use the type");
                        let mut s = format!(
                            "{alter} ALTER ATTRIBUTE {} TYPE {}",
                            ident(name)?,
                            super::common::data_type(data_type, "an attribute type")?
                        );
                        if let Some(col) = collation {
                            s.push_str(&format!(" COLLATE {}", ident(col)?));
                        }
                        s.push_str(cascade(*c));
                        s
                    }
                };
                plan.push(statement);
            }
        }
        TypeChange::CreateRange(r) => {
            let mut options = vec![format!("SUBTYPE = {}", data_type(&r.subtype, "a subtype")?)];
            if let Some(o) = &r.subtype_opclass {
                options.push(format!("SUBTYPE_OPCLASS = {}", o.sql()?));
            }
            if let Some(c) = &r.collation {
                options.push(format!("COLLATION = {}", ident(c)?));
            }
            if let Some(f) = &r.canonical {
                options.push(format!("CANONICAL = {}", f.sql()?));
            }
            if let Some(f) = &r.subtype_diff {
                options.push(format!("SUBTYPE_DIFF = {}", f.sql()?));
            }
            if let Some(m) = &r.multirange_type_name {
                require(server, 14, "multirange types")?;
                options.push(format!("MULTIRANGE_TYPE_NAME = {}", m.sql()?));
            }
            let n = r.name.sql()?;
            plan.push(format!("CREATE TYPE {n} AS RANGE ({})", options.join(", ")));
            if let Some(c) = &r.comment {
                plan.push(comment_on(&format!("TYPE {n}"), Some(c))?);
            }
        }
        TypeChange::Rename { name, new_name } => plan.push(format!(
            "ALTER TYPE {} RENAME TO {}",
            name.sql()?,
            ident(new_name)?
        )),
        TypeChange::SetSchema { name, schema } => plan.push(format!(
            "ALTER TYPE {} SET SCHEMA {}",
            name.sql()?,
            ident(schema)?
        )),
        TypeChange::Owner { name, owner } => plan.push(format!(
            "ALTER TYPE {} OWNER TO {}",
            name.sql()?,
            owner.owner_sql(server)?
        )),
        TypeChange::Drop {
            name,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            if *behavior == DropBehavior::Cascade {
                plan.warn("CASCADE also drops columns and functions that use the type");
            }
            plan.push(format!(
                "DROP TYPE {}{} {}",
                if *if_exists { "IF EXISTS " } else { "" },
                name.sql()?,
                behavior.sql()
            ));
        }
    }
    plan.finish()
}

/// A check constraint of a domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct DomainCheck {
    /// Name; `None` lets PostgreSQL choose one.
    #[serde(default)]
    pub name: Option<String>,
    /// Condition on `VALUE`, without parentheses.
    pub expression: String,
}

impl DomainCheck {
    fn sql(&self) -> CatalogResult<String> {
        let mut s = String::new();
        if let Some(n) = &self.name {
            s.push_str(&format!("CONSTRAINT {} ", ident(n)?));
        }
        s.push_str(&format!(
            "CHECK ({})",
            expr(&self.expression, "a check expression")?
        ));
        Ok(s)
    }
}

/// `CREATE DOMAIN`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateDomain {
    /// Name.
    pub name: QualifiedName,
    /// Base type as SQL.
    pub data_type: String,
    /// `COLLATE`.
    #[serde(default)]
    pub collation: Option<String>,
    /// `DEFAULT`.
    #[serde(default)]
    pub default: Option<String>,
    /// `NOT NULL`.
    #[serde(default)]
    pub not_null: bool,
    /// Check constraints.
    #[serde(default)]
    pub checks: Vec<DomainCheck>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// One change of `ALTER DOMAIN`, applied in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum DomainOperation {
    /// `SET DEFAULT`.
    SetDefault {
        /// Expression.
        expression: String,
    },
    /// `DROP DEFAULT`.
    DropDefault,
    /// `SET NOT NULL` (checks every column using the domain).
    SetNotNull,
    /// `DROP NOT NULL`.
    DropNotNull,
    /// `ADD CONSTRAINT ... CHECK`.
    AddCheck {
        /// The check.
        check: DomainCheck,
        /// `NOT VALID`: existing values are not checked.
        #[serde(default)]
        not_valid: bool,
    },
    /// `DROP CONSTRAINT`.
    DropConstraint {
        /// Constraint.
        name: String,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent objects.
        #[serde(default)]
        behavior: DropBehavior,
    },
    /// `RENAME CONSTRAINT`.
    RenameConstraint {
        /// Constraint.
        name: String,
        /// New name.
        new_name: String,
    },
    /// `VALIDATE CONSTRAINT`.
    ValidateConstraint {
        /// Constraint.
        name: String,
    },
    /// `COMMENT ON CONSTRAINT ... ON DOMAIN`: sets (or with `None`, removes) a constraint's comment.
    CommentConstraint {
        /// Constraint.
        name: String,
        /// The comment; `None` removes it.
        #[serde(default)]
        comment: Option<String>,
    },
    /// Rename the domain.
    Rename {
        /// New name.
        new_name: String,
    },
    /// Move to another schema.
    SetSchema {
        /// Schema.
        schema: String,
    },
    /// Change the owner.
    Owner {
        /// New owner.
        owner: RoleSpec,
    },
}

/// Changes to domains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DomainChange {
    /// Create a domain.
    Create(CreateDomain),
    /// Alter a domain.
    Alter {
        /// The domain.
        name: QualifiedName,
        /// Changes in order.
        operations: Vec<DomainOperation>,
    },
    /// Drop a domain.
    Drop {
        /// The domain.
        name: QualifiedName,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent columns.
        #[serde(default)]
        behavior: DropBehavior,
    },
}

pub(crate) fn plan_domain(server: &ServerInfo, change: &DomainChange) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        DomainChange::Create(d) => {
            let n = d.name.sql()?;
            let mut s = format!(
                "CREATE DOMAIN {n} AS {}",
                data_type(&d.data_type, "a base type")?
            );
            if let Some(c) = &d.collation {
                s.push_str(&format!(" COLLATE {}", ident(c)?));
            }
            if let Some(e) = &d.default {
                s.push_str(&format!(
                    " DEFAULT {}",
                    paren_expr(e, "a default expression")?
                ));
            }
            if d.not_null {
                s.push_str(" NOT NULL");
            }
            for c in &d.checks {
                s.push(' ');
                s.push_str(&c.sql()?);
            }
            plan.push(s);
            if let Some(c) = &d.comment {
                plan.push(comment_on(&format!("DOMAIN {n}"), Some(c))?);
            }
        }
        DomainChange::Alter { name, operations } => {
            if operations.is_empty() {
                return Err(invalid("no domain changes given"));
            }
            let mut current = name.clone();
            for op in operations {
                let alter = format!("ALTER DOMAIN {}", current.sql()?);
                let statement = match op {
                    DomainOperation::SetDefault { expression } => format!(
                        "{alter} SET DEFAULT {}",
                        paren_expr(expression, "a default expression")?
                    ),
                    DomainOperation::DropDefault => format!("{alter} DROP DEFAULT"),
                    DomainOperation::SetNotNull => {
                        plan.warn("SET NOT NULL checks every column that uses the domain");
                        format!("{alter} SET NOT NULL")
                    }
                    DomainOperation::DropNotNull => format!("{alter} DROP NOT NULL"),
                    DomainOperation::AddCheck { check, not_valid } => {
                        if !not_valid {
                            plan.warn("adding a domain check checks every column that uses the domain; add it NOT VALID and validate it separately on large tables");
                        }
                        format!(
                            "{alter} ADD {}{}",
                            check.sql()?,
                            if *not_valid { " NOT VALID" } else { "" }
                        )
                    }
                    DomainOperation::DropConstraint {
                        name,
                        if_exists,
                        behavior,
                    } => {
                        plan.destructive();
                        format!(
                            "{alter} DROP CONSTRAINT {}{} {}",
                            if *if_exists { "IF EXISTS " } else { "" },
                            ident(name)?,
                            behavior.sql()
                        )
                    }
                    DomainOperation::RenameConstraint { name, new_name } => format!(
                        "{alter} RENAME CONSTRAINT {} TO {}",
                        ident(name)?,
                        ident(new_name)?
                    ),
                    DomainOperation::ValidateConstraint { name } => {
                        format!("{alter} VALIDATE CONSTRAINT {}", ident(name)?)
                    }
                    DomainOperation::CommentConstraint { name, comment } => comment_on(
                        &format!("CONSTRAINT {} ON DOMAIN {}", ident(name)?, current.sql()?),
                        comment.as_deref(),
                    )?,
                    DomainOperation::Rename { new_name } => {
                        let s = format!("{alter} RENAME TO {}", ident(new_name)?);
                        current = current.renamed(new_name);
                        s
                    }
                    DomainOperation::SetSchema { schema } => {
                        let s = format!("{alter} SET SCHEMA {}", ident(schema)?);
                        current.schema = Some(schema.clone());
                        s
                    }
                    DomainOperation::Owner { owner } => {
                        format!("{alter} OWNER TO {}", owner.owner_sql(server)?)
                    }
                };
                plan.push(statement);
            }
        }
        DomainChange::Drop {
            name,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            plan.push(format!(
                "DROP DOMAIN {}{} {}",
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

    fn n() -> QualifiedName {
        QualifiedName::new("s", "ty")
    }

    #[test]
    fn enums() -> CatalogResult<()> {
        let create = plan_type(
            &pg(13),
            &TypeChange::CreateEnum {
                name: n(),
                labels: vec!["a".into(), "it's".into()],
                comment: Some("e".into()),
            },
        )?;
        assert_eq!(
            create.statements,
            vec![
                r#"CREATE TYPE "s"."ty" AS ENUM ('a', 'it''s')"#,
                r#"COMMENT ON TYPE "s"."ty" IS 'e'"#,
            ]
        );
        let add = plan_type(
            &pg(13),
            &TypeChange::AddEnumValue {
                name: n(),
                value: "b".into(),
                if_not_exists: true,
                position: Some(EnumPosition::Before { label: "a".into() }),
            },
        )?;
        assert_eq!(
            add.statements,
            vec![r#"ALTER TYPE "s"."ty" ADD VALUE IF NOT EXISTS 'b' BEFORE 'a'"#]
        );
        assert!(add.transactional);
        assert_eq!(add.warnings.len(), 1);
        let after = plan_type(
            &pg(13),
            &TypeChange::AddEnumValue {
                name: n(),
                value: "c".into(),
                if_not_exists: false,
                position: Some(EnumPosition::After { label: "a".into() }),
            },
        )?;
        assert_eq!(
            after.statements,
            vec![r#"ALTER TYPE "s"."ty" ADD VALUE 'c' AFTER 'a'"#]
        );
        let rename = plan_type(
            &pg(13),
            &TypeChange::RenameEnumValue {
                name: n(),
                value: "c".into(),
                new_value: "d".into(),
            },
        )?;
        assert_eq!(
            rename.statements,
            vec![r#"ALTER TYPE "s"."ty" RENAME VALUE 'c' TO 'd'"#]
        );
        Ok(())
    }

    #[test]
    fn composites_ranges_and_common() -> CatalogResult<()> {
        let create = plan_type(
            &pg(13),
            &TypeChange::CreateComposite {
                name: n(),
                attributes: vec![
                    CompositeAttribute {
                        name: "x".into(),
                        data_type: "int".into(),
                        collation: None,
                    },
                    CompositeAttribute {
                        name: "y".into(),
                        data_type: "text".into(),
                        collation: Some("C".into()),
                    },
                ],
                comment: None,
            },
        )?;
        assert_eq!(
            create.statements,
            vec![r#"CREATE TYPE "s"."ty" AS ("x" int, "y" text COLLATE "C")"#]
        );
        let alter = plan_type(
            &pg(13),
            &TypeChange::AlterComposite {
                name: n(),
                operations: vec![
                    CompositeOperation::AddAttribute {
                        attribute: CompositeAttribute {
                            name: "z".into(),
                            data_type: "date".into(),
                            collation: None,
                        },
                        cascade: true,
                    },
                    CompositeOperation::RenameAttribute {
                        name: "z".into(),
                        new_name: "w".into(),
                        cascade: false,
                    },
                    CompositeOperation::AlterAttributeType {
                        name: "w".into(),
                        data_type: "timestamptz".into(),
                        collation: None,
                        cascade: true,
                    },
                    CompositeOperation::DropAttribute {
                        name: "w".into(),
                        if_exists: true,
                        cascade: false,
                    },
                ],
            },
        )?;
        assert_eq!(
            alter.statements,
            vec![
                r#"ALTER TYPE "s"."ty" ADD ATTRIBUTE "z" date CASCADE"#,
                r#"ALTER TYPE "s"."ty" RENAME ATTRIBUTE "z" TO "w""#,
                r#"ALTER TYPE "s"."ty" ALTER ATTRIBUTE "w" TYPE timestamptz CASCADE"#,
                r#"ALTER TYPE "s"."ty" DROP ATTRIBUTE IF EXISTS "w""#,
            ]
        );
        assert!(alter.destructive);
        let range = CreateRangeType {
            name: n(),
            subtype: "float8".into(),
            subtype_opclass: Some(QualifiedName::bare("float8_ops")),
            collation: None,
            canonical: None,
            subtype_diff: Some(QualifiedName::new("pg_catalog", "float8mi")),
            multirange_type_name: Some(QualifiedName::new("s", "tys")),
            comment: Some("r".into()),
        };
        assert!(plan_type(&pg(13), &TypeChange::CreateRange(Box::new(range.clone()))).is_err());
        assert_eq!(
            plan_type(&pg(14), &TypeChange::CreateRange(Box::new(range)))?.statements,
            vec![
                r#"CREATE TYPE "s"."ty" AS RANGE (SUBTYPE = float8, SUBTYPE_OPCLASS = "float8_ops", SUBTYPE_DIFF = "pg_catalog"."float8mi", MULTIRANGE_TYPE_NAME = "s"."tys")"#,
                r#"COMMENT ON TYPE "s"."ty" IS 'r'"#,
            ]
        );
        let cases = [
            (
                TypeChange::Rename {
                    name: n(),
                    new_name: "u".into(),
                },
                r#"ALTER TYPE "s"."ty" RENAME TO "u""#,
            ),
            (
                TypeChange::SetSchema {
                    name: n(),
                    schema: "t".into(),
                },
                r#"ALTER TYPE "s"."ty" SET SCHEMA "t""#,
            ),
            (
                TypeChange::Owner {
                    name: n(),
                    owner: RoleSpec::name("o"),
                },
                r#"ALTER TYPE "s"."ty" OWNER TO "o""#,
            ),
            (
                TypeChange::Drop {
                    name: n(),
                    if_exists: true,
                    behavior: DropBehavior::Cascade,
                },
                r#"DROP TYPE IF EXISTS "s"."ty" CASCADE"#,
            ),
        ];
        for (change, sql) in cases {
            assert_eq!(plan_type(&pg(13), &change)?.statements, vec![sql]);
        }
        Ok(())
    }

    #[test]
    fn domains() -> CatalogResult<()> {
        let create = plan_domain(
            &pg(13),
            &DomainChange::Create(CreateDomain {
                name: n(),
                data_type: "text".into(),
                collation: Some("C".into()),
                default: Some("'x'".into()),
                not_null: true,
                checks: vec![
                    DomainCheck {
                        name: Some("len".into()),
                        expression: "length(VALUE) < 10".into(),
                    },
                    DomainCheck {
                        name: None,
                        expression: "VALUE <> ''".into(),
                    },
                ],
                comment: Some("d".into()),
            }),
        )?;
        assert_eq!(
            create.statements,
            vec![
                r#"CREATE DOMAIN "s"."ty" AS text COLLATE "C" DEFAULT ('x') NOT NULL CONSTRAINT "len" CHECK (length(VALUE) < 10) CHECK (VALUE <> '')"#,
                r#"COMMENT ON DOMAIN "s"."ty" IS 'd'"#,
            ]
        );
        let alter = plan_domain(
            &pg(13),
            &DomainChange::Alter {
                name: n(),
                operations: vec![
                    DomainOperation::SetDefault {
                        expression: "'y'".into(),
                    },
                    DomainOperation::DropDefault,
                    DomainOperation::SetNotNull,
                    DomainOperation::DropNotNull,
                    DomainOperation::AddCheck {
                        check: DomainCheck {
                            name: Some("c".into()),
                            expression: "VALUE <> 'z'".into(),
                        },
                        not_valid: true,
                    },
                    DomainOperation::ValidateConstraint { name: "c".into() },
                    DomainOperation::CommentConstraint {
                        name: "c".into(),
                        comment: Some("it's".into()),
                    },
                    DomainOperation::CommentConstraint {
                        name: "c".into(),
                        comment: None,
                    },
                    DomainOperation::RenameConstraint {
                        name: "c".into(),
                        new_name: "c2".into(),
                    },
                    DomainOperation::DropConstraint {
                        name: "c2".into(),
                        if_exists: true,
                        behavior: DropBehavior::Restrict,
                    },
                    DomainOperation::Rename {
                        new_name: "d2".into(),
                    },
                    DomainOperation::SetSchema { schema: "t".into() },
                    DomainOperation::Owner {
                        owner: RoleSpec::CurrentUser,
                    },
                ],
            },
        )?;
        let d = r#"ALTER DOMAIN "s"."ty""#;
        assert_eq!(
            alter.statements,
            vec![
                format!("{d} SET DEFAULT ('y')"),
                format!("{d} DROP DEFAULT"),
                format!("{d} SET NOT NULL"),
                format!("{d} DROP NOT NULL"),
                format!(r#"{d} ADD CONSTRAINT "c" CHECK (VALUE <> 'z') NOT VALID"#),
                format!(r#"{d} VALIDATE CONSTRAINT "c""#),
                r#"COMMENT ON CONSTRAINT "c" ON DOMAIN "s"."ty" IS 'it''s'"#.to_owned(),
                r#"COMMENT ON CONSTRAINT "c" ON DOMAIN "s"."ty" IS NULL"#.to_owned(),
                format!(r#"{d} RENAME CONSTRAINT "c" TO "c2""#),
                format!(r#"{d} DROP CONSTRAINT IF EXISTS "c2" RESTRICT"#),
                format!(r#"{d} RENAME TO "d2""#),
                r#"ALTER DOMAIN "s"."d2" SET SCHEMA "t""#.to_owned(),
                r#"ALTER DOMAIN "t"."d2" OWNER TO CURRENT_USER"#.to_owned(),
            ]
        );
        let drop = plan_domain(
            &pg(13),
            &DomainChange::Drop {
                name: n(),
                if_exists: false,
                behavior: DropBehavior::Cascade,
            },
        )?;
        assert_eq!(drop.statements, vec![r#"DROP DOMAIN "s"."ty" CASCADE"#]);
        assert!(drop.destructive);
        Ok(())
    }
}
