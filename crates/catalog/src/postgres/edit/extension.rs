//! Extensions.

use serde::{Deserialize, Serialize};
use varve_driver::ServerInfo;

use super::common::{DropBehavior, PlanBuilder, comment_on, ident, lit};
use crate::model::{CatalogResult, EditPlan};

/// `CREATE EXTENSION`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateExtension {
    /// Name, from `pg_available_extensions`.
    pub name: String,
    /// `IF NOT EXISTS`.
    #[serde(default)]
    pub if_not_exists: bool,
    /// Schema for the extension's objects.
    #[serde(default)]
    pub schema: Option<String>,
    /// Version; the default version when absent.
    #[serde(default)]
    pub version: Option<String>,
    /// `CASCADE`: also install the extensions it needs.
    #[serde(default)]
    pub cascade: bool,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// Changes to extensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ExtensionChange {
    /// Install an extension.
    Create(CreateExtension),
    /// `ALTER EXTENSION ... UPDATE [TO version]`.
    Update {
        /// Extension.
        name: String,
        /// Target version; the default version when absent.
        #[serde(default)]
        version: Option<String>,
    },
    /// Move the extension's objects to another schema (relocatable
    /// extensions only).
    SetSchema {
        /// Extension.
        name: String,
        /// Schema.
        schema: String,
    },
    /// Uninstall.
    Drop {
        /// Extension.
        name: String,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Objects that use the extension's objects.
        #[serde(default)]
        behavior: DropBehavior,
    },
}

pub(crate) fn plan_extension(
    _server: &ServerInfo,
    change: &ExtensionChange,
) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        ExtensionChange::Create(e) => {
            let name = ident(&e.name)?;
            let mut s = format!(
                "CREATE EXTENSION {}{name}",
                if e.if_not_exists {
                    "IF NOT EXISTS "
                } else {
                    ""
                }
            );
            if let Some(schema) = &e.schema {
                s.push_str(&format!(" SCHEMA {}", ident(schema)?));
            }
            if let Some(v) = &e.version {
                s.push_str(&format!(" VERSION {}", lit(v)?));
            }
            if e.cascade {
                s.push_str(" CASCADE");
            }
            plan.push(s);
            if let Some(c) = &e.comment {
                plan.push(comment_on(&format!("EXTENSION {name}"), Some(c))?);
            }
        }
        ExtensionChange::Update { name, version } => {
            let mut s = format!("ALTER EXTENSION {} UPDATE", ident(name)?);
            if let Some(v) = version {
                s.push_str(&format!(" TO {}", lit(v)?));
            }
            plan.push(s);
        }
        ExtensionChange::SetSchema { name, schema } => plan.push(format!(
            "ALTER EXTENSION {} SET SCHEMA {}",
            ident(name)?,
            ident(schema)?
        )),
        ExtensionChange::Drop {
            name,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            if *behavior == DropBehavior::Cascade {
                plan.warn("CASCADE also drops columns, indexes and functions that use the extension's types and functions");
            }
            plan.push(format!(
                "DROP EXTENSION {}{} {}",
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

    #[test]
    fn extensions() -> CatalogResult<()> {
        let create = plan_extension(
            &pg(13),
            &ExtensionChange::Create(CreateExtension {
                name: "hstore".into(),
                if_not_exists: true,
                schema: Some("ext".into()),
                version: Some("1.4".into()),
                cascade: true,
                comment: Some("kv".into()),
            }),
        )?;
        assert_eq!(
            create.statements,
            vec![
                r#"CREATE EXTENSION IF NOT EXISTS "hstore" SCHEMA "ext" VERSION '1.4' CASCADE"#,
                r#"COMMENT ON EXTENSION "hstore" IS 'kv'"#,
            ]
        );
        assert!(create.transactional && !create.destructive);
        let cases = [
            (
                ExtensionChange::Update {
                    name: "hstore".into(),
                    version: Some("1.5".into()),
                },
                r#"ALTER EXTENSION "hstore" UPDATE TO '1.5'"#,
            ),
            (
                ExtensionChange::Update {
                    name: "hstore".into(),
                    version: None,
                },
                r#"ALTER EXTENSION "hstore" UPDATE"#,
            ),
            (
                ExtensionChange::SetSchema {
                    name: "hstore".into(),
                    schema: "x".into(),
                },
                r#"ALTER EXTENSION "hstore" SET SCHEMA "x""#,
            ),
            (
                ExtensionChange::Drop {
                    name: "hstore".into(),
                    if_exists: true,
                    behavior: DropBehavior::Cascade,
                },
                r#"DROP EXTENSION IF EXISTS "hstore" CASCADE"#,
            ),
        ];
        for (change, sql) in cases {
            assert_eq!(plan_extension(&pg(13), &change)?.statements, vec![sql]);
        }
        Ok(())
    }
}
