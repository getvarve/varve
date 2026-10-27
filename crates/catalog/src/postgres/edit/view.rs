//! Views and materialized views.

use serde::{Deserialize, Serialize};
use varve_driver::ServerInfo;

use super::common::{
    ACCESS_EXCLUSIVE, DropBehavior, PlanBuilder, QualifiedName, RoleSpec, StorageParam, comment_on,
    ident, ident_list, invalid, paren_expr, query, require, storage_param_names, storage_params,
};
use super::database::bool_sql;
use crate::model::{CatalogResult, EditPlan};

/// `WITH CHECK OPTION` of an updatable view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum CheckOption {
    /// Checks this view's condition only.
    Local,
    /// Checks the conditions of underlying views too.
    Cascaded,
}

impl CheckOption {
    fn sql(self) -> &'static str {
        match self {
            Self::Local => "LOCAL",
            Self::Cascaded => "CASCADED",
        }
    }
}

/// `CREATE [OR REPLACE] VIEW`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateView {
    /// Name.
    pub name: QualifiedName,
    /// `OR REPLACE`: the new query must keep the existing columns (names,
    /// types, order) and may only add columns at the end.
    #[serde(default)]
    pub or_replace: bool,
    /// Column names; empty takes them from the query.
    #[serde(default)]
    pub columns: Vec<String>,
    /// The `SELECT` (or `VALUES`) query.
    pub query: String,
    /// `WITH ... CHECK OPTION`.
    #[serde(default)]
    pub check_option: Option<CheckOption>,
    /// `security_barrier`.
    #[serde(default)]
    pub security_barrier: Option<bool>,
    /// `security_invoker` (15+): check permissions as the querying user.
    #[serde(default)]
    pub security_invoker: Option<bool>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// A view option for `RESET`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum ViewOption {
    /// `check_option`.
    CheckOption,
    /// `security_barrier`.
    SecurityBarrier,
    /// `security_invoker` (15+).
    SecurityInvoker,
}

/// One change of `ALTER VIEW`, applied in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ViewOperation {
    /// Rename.
    Rename {
        /// New name.
        new_name: String,
    },
    /// Move to another schema.
    SetSchema {
        /// Schema.
        schema: String,
    },
    /// Rename a column.
    RenameColumn {
        /// Column.
        name: String,
        /// New name.
        new_name: String,
    },
    /// Default for inserts through the view.
    SetColumnDefault {
        /// Column.
        column: String,
        /// Expression.
        expression: String,
    },
    /// Drop a column default.
    DropColumnDefault {
        /// Column.
        column: String,
    },
    /// `SET (...)`: the options given.
    SetOptions {
        /// `check_option`.
        #[serde(default)]
        check_option: Option<CheckOption>,
        /// `security_barrier`.
        #[serde(default)]
        security_barrier: Option<bool>,
        /// `security_invoker` (15+).
        #[serde(default)]
        security_invoker: Option<bool>,
    },
    /// `RESET (...)`.
    ResetOptions {
        /// Options to reset.
        options: Vec<ViewOption>,
    },
    /// Change the owner.
    Owner {
        /// New owner.
        owner: RoleSpec,
    },
}

/// Changes to views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ViewChange {
    /// Create or replace a view.
    Create(CreateView),
    /// Alter a view.
    Alter {
        /// The view.
        name: QualifiedName,
        /// Changes in order.
        operations: Vec<ViewOperation>,
    },
    /// Drop a view.
    Drop {
        /// The view.
        name: QualifiedName,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent views.
        #[serde(default)]
        behavior: DropBehavior,
    },
}

fn view_options(
    server: &ServerInfo,
    check_option: Option<CheckOption>,
    security_barrier: Option<bool>,
    security_invoker: Option<bool>,
) -> CatalogResult<Vec<String>> {
    let mut options = Vec::new();
    if let Some(c) = check_option {
        options.push(format!("check_option = {}", c.sql().to_lowercase()));
    }
    if let Some(b) = security_barrier {
        options.push(format!("security_barrier = {}", bool_sql(b)));
    }
    if let Some(i) = security_invoker {
        require(server, 15, "security_invoker views")?;
        options.push(format!("security_invoker = {}", bool_sql(i)));
    }
    Ok(options)
}

pub(crate) fn plan_view(server: &ServerInfo, change: &ViewChange) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        ViewChange::Create(create) => {
            let name = create.name.sql()?;
            let mut s = String::from("CREATE ");
            if create.or_replace {
                s.push_str("OR REPLACE ");
            }
            s.push_str("VIEW ");
            s.push_str(&name);
            if !create.columns.is_empty() {
                s.push_str(&format!(" ({})", ident_list(&create.columns)?));
            }
            let options = view_options(
                server,
                None,
                create.security_barrier,
                create.security_invoker,
            )?;
            if !options.is_empty() {
                s.push_str(&format!(" WITH ({})", options.join(", ")));
            }
            s.push_str(" AS\n");
            s.push_str(&query(&create.query, "a view query")?);
            if let Some(c) = create.check_option {
                s.push_str(&format!("\nWITH {} CHECK OPTION", c.sql()));
            }
            plan.push(s);
            if let Some(comment) = &create.comment {
                plan.push(comment_on(&format!("VIEW {name}"), Some(comment))?);
            }
        }
        ViewChange::Alter { name, operations } => {
            if operations.is_empty() {
                return Err(invalid("no view changes given"));
            }
            let mut current = name.clone();
            for op in operations {
                let alter = format!("ALTER VIEW {}", current.sql()?);
                let statement = match op {
                    ViewOperation::Rename { new_name } => {
                        let s = format!("{alter} RENAME TO {}", ident(new_name)?);
                        current = current.renamed(new_name);
                        s
                    }
                    ViewOperation::SetSchema { schema } => {
                        let s = format!("{alter} SET SCHEMA {}", ident(schema)?);
                        current.schema = Some(schema.clone());
                        s
                    }
                    ViewOperation::RenameColumn { name, new_name } => format!(
                        "{alter} RENAME COLUMN {} TO {}",
                        ident(name)?,
                        ident(new_name)?
                    ),
                    ViewOperation::SetColumnDefault { column, expression } => format!(
                        "{alter} ALTER COLUMN {} SET DEFAULT {}",
                        ident(column)?,
                        paren_expr(expression, "a default expression")?
                    ),
                    ViewOperation::DropColumnDefault { column } => {
                        format!("{alter} ALTER COLUMN {} DROP DEFAULT", ident(column)?)
                    }
                    ViewOperation::SetOptions {
                        check_option,
                        security_barrier,
                        security_invoker,
                    } => {
                        let options = view_options(
                            server,
                            *check_option,
                            *security_barrier,
                            *security_invoker,
                        )?;
                        if options.is_empty() {
                            return Err(invalid("no view options given"));
                        }
                        format!("{alter} SET ({})", options.join(", "))
                    }
                    ViewOperation::ResetOptions { options } => {
                        if options.is_empty() {
                            return Err(invalid("no view options given"));
                        }
                        let names = options
                            .iter()
                            .map(|o| {
                                Ok(match o {
                                    ViewOption::CheckOption => "check_option",
                                    ViewOption::SecurityBarrier => "security_barrier",
                                    ViewOption::SecurityInvoker => {
                                        require(server, 15, "security_invoker views")?;
                                        "security_invoker"
                                    }
                                })
                            })
                            .collect::<CatalogResult<Vec<_>>>()?;
                        format!("{alter} RESET ({})", names.join(", "))
                    }
                    ViewOperation::Owner { owner } => {
                        format!("{alter} OWNER TO {}", owner.owner_sql(server)?)
                    }
                };
                plan.push(statement);
            }
        }
        ViewChange::Drop {
            name,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            plan.push(format!(
                "DROP VIEW {}{} {}",
                if *if_exists { "IF EXISTS " } else { "" },
                name.sql()?,
                behavior.sql()
            ));
        }
    }
    plan.finish()
}

/// `CREATE MATERIALIZED VIEW`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateMaterializedView {
    /// Name.
    pub name: QualifiedName,
    /// `IF NOT EXISTS`.
    #[serde(default)]
    pub if_not_exists: bool,
    /// Column names; empty takes them from the query.
    #[serde(default)]
    pub columns: Vec<String>,
    /// The query.
    pub query: String,
    /// `WITH DATA` (true, the default) fills it now; `WITH NO DATA` leaves
    /// it unscannable until the first refresh.
    #[serde(default = "yes")]
    pub with_data: bool,
    /// Storage parameters.
    #[serde(default)]
    pub storage_params: Vec<StorageParam>,
    /// Tablespace.
    #[serde(default)]
    pub tablespace: Option<String>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

fn yes() -> bool {
    true
}

/// One change of `ALTER MATERIALIZED VIEW`, applied in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum MaterializedViewOperation {
    /// Rename.
    Rename {
        /// New name.
        new_name: String,
    },
    /// Move to another schema.
    SetSchema {
        /// Schema.
        schema: String,
    },
    /// Rename a column.
    RenameColumn {
        /// Column.
        name: String,
        /// New name.
        new_name: String,
    },
    /// Move to another tablespace.
    SetTablespace {
        /// Tablespace.
        tablespace: String,
    },
    /// `SET (...)`.
    SetStorageParams {
        /// Parameters.
        params: Vec<StorageParam>,
    },
    /// `RESET (...)`.
    ResetStorageParams {
        /// Names.
        names: Vec<String>,
    },
    /// Change the owner.
    Owner {
        /// New owner.
        owner: RoleSpec,
    },
}

/// Changes to materialized views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MaterializedViewChange {
    /// Create a materialized view.
    Create(CreateMaterializedView),
    /// `REFRESH MATERIALIZED VIEW`.
    Refresh {
        /// The view.
        name: QualifiedName,
        /// `CONCURRENTLY`: readers are not blocked; needs a unique index
        /// on plain columns covering all rows, and a populated view.
        #[serde(default)]
        concurrently: bool,
        /// Whether the view has such a unique index (from the catalog).
        #[serde(default)]
        has_unique_index: bool,
        /// `WITH NO DATA` when false: empties the view.
        #[serde(default = "yes")]
        with_data: bool,
    },
    /// Alter a materialized view.
    Alter {
        /// The view.
        name: QualifiedName,
        /// Changes in order.
        operations: Vec<MaterializedViewOperation>,
    },
    /// Drop a materialized view.
    Drop {
        /// The view.
        name: QualifiedName,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent objects.
        #[serde(default)]
        behavior: DropBehavior,
    },
}

pub(crate) fn plan_materialized_view(
    server: &ServerInfo,
    change: &MaterializedViewChange,
) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        MaterializedViewChange::Create(create) => {
            let name = create.name.sql()?;
            let mut s = String::from("CREATE MATERIALIZED VIEW ");
            if create.if_not_exists {
                s.push_str("IF NOT EXISTS ");
            }
            s.push_str(&name);
            if !create.columns.is_empty() {
                s.push_str(&format!(" ({})", ident_list(&create.columns)?));
            }
            if !create.storage_params.is_empty() {
                s.push_str(&format!(
                    " WITH ({})",
                    storage_params(&create.storage_params)?
                ));
            }
            if let Some(ts) = &create.tablespace {
                s.push_str(&format!(" TABLESPACE {}", ident(ts)?));
            }
            s.push_str(" AS\n");
            s.push_str(&query(&create.query, "a materialized view query")?);
            s.push_str(if create.with_data {
                "\nWITH DATA"
            } else {
                "\nWITH NO DATA"
            });
            plan.push(s);
            if let Some(comment) = &create.comment {
                plan.push(comment_on(
                    &format!("MATERIALIZED VIEW {name}"),
                    Some(comment),
                )?);
            }
        }
        MaterializedViewChange::Refresh {
            name,
            concurrently,
            has_unique_index,
            with_data,
        } => {
            let mut s = String::from("REFRESH MATERIALIZED VIEW ");
            if *concurrently {
                if !has_unique_index {
                    return Err(invalid(
                        "REFRESH MATERIALIZED VIEW CONCURRENTLY needs a unique index on plain columns of the view; create one or refresh without CONCURRENTLY",
                    ));
                }
                if !with_data {
                    return Err(invalid("CONCURRENTLY and WITH NO DATA cannot be combined"));
                }
                plan.warn("CONCURRENTLY only works on a view that is already populated, and takes longer than a plain refresh");
                s.push_str("CONCURRENTLY ");
            } else {
                plan.warn(format!("a refresh without CONCURRENTLY {ACCESS_EXCLUSIVE}"));
            }
            s.push_str(&name.sql()?);
            if !with_data {
                plan.destructive();
                plan.warn(
                    "WITH NO DATA empties the view; it cannot be queried until the next refresh",
                );
                s.push_str(" WITH NO DATA");
            }
            plan.push(s);
        }
        MaterializedViewChange::Alter { name, operations } => {
            if operations.is_empty() {
                return Err(invalid("no materialized view changes given"));
            }
            let mut current = name.clone();
            for op in operations {
                let alter = format!("ALTER MATERIALIZED VIEW {}", current.sql()?);
                let statement = match op {
                    MaterializedViewOperation::Rename { new_name } => {
                        let s = format!("{alter} RENAME TO {}", ident(new_name)?);
                        current = current.renamed(new_name);
                        s
                    }
                    MaterializedViewOperation::SetSchema { schema } => {
                        let s = format!("{alter} SET SCHEMA {}", ident(schema)?);
                        current.schema = Some(schema.clone());
                        s
                    }
                    MaterializedViewOperation::RenameColumn { name, new_name } => format!(
                        "{alter} RENAME COLUMN {} TO {}",
                        ident(name)?,
                        ident(new_name)?
                    ),
                    MaterializedViewOperation::SetTablespace { tablespace } => {
                        plan.warn(format!(
                            "moving a materialized view copies it and {ACCESS_EXCLUSIVE}"
                        ));
                        format!("{alter} SET TABLESPACE {}", ident(tablespace)?)
                    }
                    MaterializedViewOperation::SetStorageParams { params } => {
                        format!("{alter} SET ({})", storage_params(params)?)
                    }
                    MaterializedViewOperation::ResetStorageParams { names } => {
                        format!("{alter} RESET ({})", storage_param_names(names)?)
                    }
                    MaterializedViewOperation::Owner { owner } => {
                        format!("{alter} OWNER TO {}", owner.owner_sql(server)?)
                    }
                };
                plan.push(statement);
            }
        }
        MaterializedViewChange::Drop {
            name,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            plan.push(format!(
                "DROP MATERIALIZED VIEW {}{} {}",
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

    fn v() -> QualifiedName {
        QualifiedName::new("s", "v")
    }

    #[test]
    fn views() -> CatalogResult<()> {
        let create = CreateView {
            name: v(),
            or_replace: true,
            columns: vec!["a".into()],
            query: "SELECT 1".into(),
            check_option: Some(CheckOption::Cascaded),
            security_barrier: Some(true),
            security_invoker: Some(true),
            comment: Some("c".into()),
        };
        let plan = plan_view(&pg(15), &ViewChange::Create(create.clone()))?;
        assert_eq!(
            plan.statements,
            vec![
                "CREATE OR REPLACE VIEW \"s\".\"v\" (\"a\") WITH (security_barrier = true, security_invoker = true) AS\nSELECT 1\nWITH CASCADED CHECK OPTION",
                r#"COMMENT ON VIEW "s"."v" IS 'c'"#,
            ]
        );
        assert!(plan.transactional && !plan.destructive);
        assert!(plan_view(&pg(14), &ViewChange::Create(create)).is_err());

        let alter = plan_view(
            &pg(13),
            &ViewChange::Alter {
                name: v(),
                operations: vec![
                    ViewOperation::SetOptions {
                        check_option: Some(CheckOption::Local),
                        security_barrier: Some(false),
                        security_invoker: None,
                    },
                    ViewOperation::ResetOptions {
                        options: vec![ViewOption::CheckOption, ViewOption::SecurityBarrier],
                    },
                    ViewOperation::RenameColumn {
                        name: "a".into(),
                        new_name: "b".into(),
                    },
                    ViewOperation::SetColumnDefault {
                        column: "b".into(),
                        expression: "1".into(),
                    },
                    ViewOperation::DropColumnDefault { column: "b".into() },
                    ViewOperation::Rename {
                        new_name: "w".into(),
                    },
                    ViewOperation::SetSchema { schema: "t".into() },
                    ViewOperation::Owner {
                        owner: RoleSpec::CurrentUser,
                    },
                ],
            },
        )?;
        assert_eq!(
            alter.statements,
            vec![
                r#"ALTER VIEW "s"."v" SET (check_option = local, security_barrier = false)"#,
                r#"ALTER VIEW "s"."v" RESET (check_option, security_barrier)"#,
                r#"ALTER VIEW "s"."v" RENAME COLUMN "a" TO "b""#,
                r#"ALTER VIEW "s"."v" ALTER COLUMN "b" SET DEFAULT (1)"#,
                r#"ALTER VIEW "s"."v" ALTER COLUMN "b" DROP DEFAULT"#,
                r#"ALTER VIEW "s"."v" RENAME TO "w""#,
                r#"ALTER VIEW "s"."w" SET SCHEMA "t""#,
                r#"ALTER VIEW "t"."w" OWNER TO CURRENT_USER"#,
            ]
        );
        let drop = plan_view(
            &pg(13),
            &ViewChange::Drop {
                name: v(),
                if_exists: false,
                behavior: DropBehavior::Cascade,
            },
        )?;
        assert_eq!(drop.statements, vec![r#"DROP VIEW "s"."v" CASCADE"#]);
        assert!(drop.destructive);
        Ok(())
    }

    #[test]
    fn materialized_views() -> CatalogResult<()> {
        let plan = plan_materialized_view(
            &pg(13),
            &MaterializedViewChange::Create(CreateMaterializedView {
                name: v(),
                if_not_exists: true,
                columns: vec!["a".into()],
                query: "SELECT 1".into(),
                with_data: false,
                storage_params: vec![StorageParam::new("fillfactor", "50")],
                tablespace: Some("pg_default".into()),
                comment: Some("m".into()),
            }),
        )?;
        assert_eq!(
            plan.statements,
            vec![
                "CREATE MATERIALIZED VIEW IF NOT EXISTS \"s\".\"v\" (\"a\") WITH (\"fillfactor\" = 50) TABLESPACE \"pg_default\" AS\nSELECT 1\nWITH NO DATA",
                r#"COMMENT ON MATERIALIZED VIEW "s"."v" IS 'm'"#,
            ]
        );
        let refresh = |concurrently, has_unique_index, with_data| {
            plan_materialized_view(
                &pg(13),
                &MaterializedViewChange::Refresh {
                    name: v(),
                    concurrently,
                    has_unique_index,
                    with_data,
                },
            )
        };
        let r = refresh(true, true, true)?;
        assert_eq!(
            r.statements,
            vec![r#"REFRESH MATERIALIZED VIEW CONCURRENTLY "s"."v""#]
        );
        assert!(r.transactional && !r.destructive);
        assert!(matches!(
            refresh(true, false, true),
            Err(crate::model::CatalogError::Invalid { .. })
        ));
        assert!(refresh(true, true, false).is_err());
        let empty = refresh(false, false, false)?;
        assert_eq!(
            empty.statements,
            vec![r#"REFRESH MATERIALIZED VIEW "s"."v" WITH NO DATA"#]
        );
        assert!(empty.destructive);
        assert!(
            empty
                .warnings
                .iter()
                .any(|w| w.contains("ACCESS EXCLUSIVE"))
        );

        let alter = plan_materialized_view(
            &pg(13),
            &MaterializedViewChange::Alter {
                name: v(),
                operations: vec![
                    MaterializedViewOperation::RenameColumn {
                        name: "a".into(),
                        new_name: "b".into(),
                    },
                    MaterializedViewOperation::SetStorageParams {
                        params: vec![StorageParam::new("fillfactor", "60")],
                    },
                    MaterializedViewOperation::ResetStorageParams {
                        names: vec!["fillfactor".into()],
                    },
                    MaterializedViewOperation::SetTablespace {
                        tablespace: "pg_default".into(),
                    },
                    MaterializedViewOperation::Rename {
                        new_name: "w".into(),
                    },
                    MaterializedViewOperation::SetSchema { schema: "t".into() },
                    MaterializedViewOperation::Owner {
                        owner: RoleSpec::name("o"),
                    },
                ],
            },
        )?;
        assert_eq!(
            alter.statements,
            vec![
                r#"ALTER MATERIALIZED VIEW "s"."v" RENAME COLUMN "a" TO "b""#,
                r#"ALTER MATERIALIZED VIEW "s"."v" SET ("fillfactor" = 60)"#,
                r#"ALTER MATERIALIZED VIEW "s"."v" RESET ("fillfactor")"#,
                r#"ALTER MATERIALIZED VIEW "s"."v" SET TABLESPACE "pg_default""#,
                r#"ALTER MATERIALIZED VIEW "s"."v" RENAME TO "w""#,
                r#"ALTER MATERIALIZED VIEW "s"."w" SET SCHEMA "t""#,
                r#"ALTER MATERIALIZED VIEW "t"."w" OWNER TO "o""#,
            ]
        );
        let drop = plan_materialized_view(
            &pg(13),
            &MaterializedViewChange::Drop {
                name: v(),
                if_exists: true,
                behavior: DropBehavior::Restrict,
            },
        )?;
        assert_eq!(
            drop.statements,
            vec![r#"DROP MATERIALIZED VIEW IF EXISTS "s"."v" RESTRICT"#]
        );
        assert!(drop.destructive);
        Ok(())
    }
}
