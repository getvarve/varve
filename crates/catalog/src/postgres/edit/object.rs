//! Comments on every object kind and owner changes for every owned kind,
//! addressed through the browser's [`ObjectRef`].

use serde::{Deserialize, Serialize};
use varve_driver::ServerInfo;

use super::common::{
    PlanBuilder, RoleSpec, aggregate_signature, cast_signature, comment_on, ident, invalid,
    operator, type_list,
};
use crate::model::{CatalogError, CatalogResult, EditPlan, ObjectKind, ObjectRef};
use crate::sql;

/// `COMMENT ON`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CommentChange {
    /// The object. Columns, constraints, triggers, rules and policies name
    /// their table in `parent`; routines, aggregates and operators give
    /// their argument types in `signature`, casts `source AS target`.
    pub target: ObjectRef,
    /// The comment; `None` removes it.
    #[serde(default)]
    pub comment: Option<String>,
}

/// `ALTER ... OWNER TO`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct OwnerChange {
    /// The object (see [`CommentChange::target`]).
    pub target: ObjectRef,
    /// New owner.
    pub owner: RoleSpec,
}

fn schema_name(object: &ObjectRef) -> CatalogResult<String> {
    sql::qualified(
        varve_driver::Dialect::Postgres,
        &[object.schema.as_deref(), Some(object.name.as_str())],
    )
}

fn parent(object: &ObjectRef) -> CatalogResult<String> {
    let Some(table) = &object.parent else {
        return Err(invalid(format!(
            "{:?} {} needs its table in `parent`",
            object.kind, object.name
        )));
    };
    sql::qualified(
        varve_driver::Dialect::Postgres,
        &[object.schema.as_deref(), Some(table.as_str())],
    )
}

fn signature(object: &ObjectRef, aggregate: bool) -> CatalogResult<String> {
    match object.signature.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() && aggregate => aggregate_signature(s, "an argument list"),
        Some(s) if !s.is_empty() => type_list(s, "an argument list"),
        _ if aggregate => Ok("*".into()),
        _ => Ok(String::new()),
    }
}

/// The object as written after `COMMENT ON` / `ALTER`: keyword and name.
fn target(object: &ObjectRef) -> CatalogResult<(&'static str, String)> {
    use ObjectKind as K;
    Ok(match object.kind {
        K::Database => ("DATABASE", ident(&object.name)?),
        K::Schema => ("SCHEMA", ident(&object.name)?),
        K::Table | K::PartitionedTable | K::Partition => ("TABLE", schema_name(object)?),
        K::View => ("VIEW", schema_name(object)?),
        K::MaterializedView => ("MATERIALIZED VIEW", schema_name(object)?),
        K::ForeignTable => ("FOREIGN TABLE", schema_name(object)?),
        K::Sequence => ("SEQUENCE", schema_name(object)?),
        K::Index => ("INDEX", schema_name(object)?),
        K::Column => (
            "COLUMN",
            format!("{}.{}", parent(object)?, ident(&object.name)?),
        ),
        K::Constraint => (
            "CONSTRAINT",
            format!("{} ON {}", ident(&object.name)?, parent(object)?),
        ),
        K::Trigger => (
            "TRIGGER",
            format!("{} ON {}", ident(&object.name)?, parent(object)?),
        ),
        K::Rule => (
            "RULE",
            format!("{} ON {}", ident(&object.name)?, parent(object)?),
        ),
        K::Policy => (
            "POLICY",
            format!("{} ON {}", ident(&object.name)?, parent(object)?),
        ),
        K::Function => (
            "FUNCTION",
            format!("{}({})", schema_name(object)?, signature(object, false)?),
        ),
        K::Procedure => (
            "PROCEDURE",
            format!("{}({})", schema_name(object)?, signature(object, false)?),
        ),
        K::Aggregate => (
            "AGGREGATE",
            format!("{}({})", schema_name(object)?, signature(object, true)?),
        ),
        K::EventTrigger => ("EVENT TRIGGER", ident(&object.name)?),
        K::EnumType | K::CompositeType | K::RangeType => ("TYPE", schema_name(object)?),
        K::Domain => ("DOMAIN", schema_name(object)?),
        K::Extension => ("EXTENSION", ident(&object.name)?),
        K::Publication => ("PUBLICATION", ident(&object.name)?),
        K::Subscription => ("SUBSCRIPTION", ident(&object.name)?),
        K::ForeignDataWrapper => ("FOREIGN DATA WRAPPER", ident(&object.name)?),
        K::ForeignServer => ("SERVER", ident(&object.name)?),
        K::Tablespace => ("TABLESPACE", ident(&object.name)?),
        K::Collation => ("COLLATION", schema_name(object)?),
        K::Cast => {
            let Some(sig) = object.signature.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Err(invalid("a cast needs `source AS target` in `signature`"));
            };
            (
                "CAST",
                format!("({})", cast_signature(sig, "a cast signature")?),
            )
        }
        K::Operator => {
            let sig = signature(object, false)?;
            if sig.is_empty() {
                return Err(invalid(
                    "an operator needs its operand types (`left, right`, NONE for a prefix operator) in `signature`",
                ));
            }
            let op = operator(&object.name, None)?;
            let name = match &object.schema {
                Some(s) => format!("{}.{op}", ident(s)?),
                None => op,
            };
            ("OPERATOR", format!("{name} ({sig})"))
        }
        K::Event | K::Package | K::PackageBody => {
            return Err(CatalogError::Unsupported {
                message: format!("PostgreSQL has no {:?} objects", object.kind),
            });
        }
    })
}

pub(crate) fn plan_comment(
    _server: &ServerInfo,
    change: &CommentChange,
) -> CatalogResult<EditPlan> {
    let (keyword, name) = target(&change.target)?;
    let mut plan = PlanBuilder::new();
    plan.push(comment_on(
        &format!("{keyword} {name}"),
        change.comment.as_deref(),
    )?);
    plan.finish()
}

pub(crate) fn plan_owner(server: &ServerInfo, change: &OwnerChange) -> CatalogResult<EditPlan> {
    use ObjectKind as K;
    let kind = change.target.kind;
    if matches!(
        kind,
        K::Column
            | K::Index
            | K::Constraint
            | K::Trigger
            | K::Rule
            | K::Policy
            | K::Extension
            | K::Cast
    ) {
        return Err(invalid(format!(
            "a {kind:?} has no owner of its own; it belongs to its table, extension or the cast's types"
        )));
    }
    let (keyword, name) = target(&change.target)?;
    let mut plan = PlanBuilder::new();
    if matches!(
        kind,
        K::Table | K::PartitionedTable | K::Partition | K::ForeignTable
    ) {
        plan.warn(format!(
            "ALTER {keyword} {}",
            super::common::ACCESS_EXCLUSIVE
        ));
    }
    plan.push(format!(
        "ALTER {keyword} {name} OWNER TO {}",
        change.owner.owner_sql(server)?
    ));
    plan.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::edit::common::tests::pg;

    fn obj(kind: ObjectKind, name: &str) -> ObjectRef {
        ObjectRef {
            kind,
            database: Some("d".into()),
            schema: Some("s".into()),
            parent: Some("t".into()),
            name: name.into(),
            signature: None,
        }
    }

    fn comment(o: ObjectRef) -> CatalogResult<String> {
        let plan = plan_comment(
            &pg(13),
            &CommentChange {
                target: o,
                comment: Some("x".into()),
            },
        )?;
        Ok(plan.statements[0].clone())
    }

    #[test]
    fn comments_on_every_kind() -> CatalogResult<()> {
        use ObjectKind as K;
        let sig = |kind, name: &str, s: &str| ObjectRef {
            signature: Some(s.into()),
            ..obj(kind, name)
        };
        let cases = [
            (obj(K::Database, "n"), r#"DATABASE "n""#),
            (obj(K::Schema, "n"), r#"SCHEMA "n""#),
            (obj(K::Table, "n"), r#"TABLE "s"."n""#),
            (obj(K::PartitionedTable, "n"), r#"TABLE "s"."n""#),
            (obj(K::Partition, "n"), r#"TABLE "s"."n""#),
            (obj(K::View, "n"), r#"VIEW "s"."n""#),
            (
                obj(K::MaterializedView, "n"),
                r#"MATERIALIZED VIEW "s"."n""#,
            ),
            (obj(K::ForeignTable, "n"), r#"FOREIGN TABLE "s"."n""#),
            (obj(K::Sequence, "n"), r#"SEQUENCE "s"."n""#),
            (obj(K::Index, "n"), r#"INDEX "s"."n""#),
            (obj(K::Column, "n"), r#"COLUMN "s"."t"."n""#),
            (obj(K::Constraint, "n"), r#"CONSTRAINT "n" ON "s"."t""#),
            (obj(K::Trigger, "n"), r#"TRIGGER "n" ON "s"."t""#),
            (obj(K::Rule, "n"), r#"RULE "n" ON "s"."t""#),
            (obj(K::Policy, "n"), r#"POLICY "n" ON "s"."t""#),
            (
                sig(K::Function, "n", "integer, text"),
                r#"FUNCTION "s"."n"(integer, text)"#,
            ),
            (obj(K::Procedure, "n"), r#"PROCEDURE "s"."n"()"#),
            (obj(K::Aggregate, "n"), r#"AGGREGATE "s"."n"(*)"#),
            (
                sig(K::Aggregate, "n", "integer"),
                r#"AGGREGATE "s"."n"(integer)"#,
            ),
            (obj(K::EventTrigger, "n"), r#"EVENT TRIGGER "n""#),
            (obj(K::EnumType, "n"), r#"TYPE "s"."n""#),
            (obj(K::CompositeType, "n"), r#"TYPE "s"."n""#),
            (obj(K::RangeType, "n"), r#"TYPE "s"."n""#),
            (obj(K::Domain, "n"), r#"DOMAIN "s"."n""#),
            (obj(K::Extension, "n"), r#"EXTENSION "n""#),
            (obj(K::Publication, "n"), r#"PUBLICATION "n""#),
            (obj(K::Subscription, "n"), r#"SUBSCRIPTION "n""#),
            (
                obj(K::ForeignDataWrapper, "n"),
                r#"FOREIGN DATA WRAPPER "n""#,
            ),
            (obj(K::ForeignServer, "n"), r#"SERVER "n""#),
            (obj(K::Tablespace, "n"), r#"TABLESPACE "n""#),
            (obj(K::Collation, "n"), r#"COLLATION "s"."n""#),
            (sig(K::Cast, "c", "text AS s.t"), r#"CAST (text AS s.t)"#),
            (
                sig(K::Operator, "===", "int, int"),
                r#"OPERATOR "s".=== (int, int)"#,
            ),
        ];
        for (o, sql) in cases {
            assert_eq!(comment(o)?, format!("COMMENT ON {sql} IS 'x'"));
        }
        let none = plan_comment(
            &pg(13),
            &CommentChange {
                target: obj(K::Table, "n"),
                comment: None,
            },
        )?;
        assert_eq!(none.statements, vec![r#"COMMENT ON TABLE "s"."n" IS NULL"#]);
        assert!(comment(obj(K::Event, "n")).is_err());
        assert!(comment(obj(K::Cast, "n")).is_err());
        assert!(
            comment(ObjectRef {
                parent: None,
                ..obj(K::Column, "n")
            })
            .is_err()
        );
        assert!(comment(sig(K::Function, "n", "int); DROP TABLE x; --")).is_err());
        Ok(())
    }

    #[test]
    fn owners() -> CatalogResult<()> {
        use ObjectKind as K;
        let owner = |o: ObjectRef| {
            plan_owner(
                &pg(13),
                &OwnerChange {
                    target: o,
                    owner: RoleSpec::name("o"),
                },
            )
        };
        let p = owner(obj(K::Table, "n"))?;
        assert_eq!(p.statements, vec![r#"ALTER TABLE "s"."n" OWNER TO "o""#]);
        assert!(p.warnings[0].contains("ACCESS EXCLUSIVE"));
        assert_eq!(
            owner(ObjectRef {
                signature: Some("int".into()),
                ..obj(K::Function, "f")
            })?
            .statements,
            vec![r#"ALTER FUNCTION "s"."f"(int) OWNER TO "o""#]
        );
        for (kind, sql) in [
            (K::Database, r#"ALTER DATABASE "n""#),
            (K::Schema, r#"ALTER SCHEMA "n""#),
            (K::MaterializedView, r#"ALTER MATERIALIZED VIEW "s"."n""#),
            (K::Domain, r#"ALTER DOMAIN "s"."n""#),
            (K::EnumType, r#"ALTER TYPE "s"."n""#),
            (K::ForeignServer, r#"ALTER SERVER "n""#),
            (K::EventTrigger, r#"ALTER EVENT TRIGGER "n""#),
        ] {
            assert_eq!(
                owner(obj(kind, "n"))?.statements,
                vec![format!(r#"{sql} OWNER TO "o""#)]
            );
        }
        for kind in [
            K::Column,
            K::Index,
            K::Constraint,
            K::Trigger,
            K::Rule,
            K::Policy,
            K::Extension,
        ] {
            assert!(owner(obj(kind, "n")).is_err());
        }
        assert!(
            plan_owner(
                &pg(13),
                &OwnerChange {
                    target: obj(K::Table, "n"),
                    owner: RoleSpec::Public
                }
            )
            .is_err()
        );
        Ok(())
    }
}
