//! Databases and schemas.

use serde::{Deserialize, Serialize};
use varve_driver::ServerInfo;

use super::common::{
    ConfigSetting, DropBehavior, PlanBuilder, RoleSpec, comment_on, config_assignment, config_name,
    ident, invalid, lit, require,
};
use crate::model::{CatalogResult, EditPlan};

/// Locale provider of a new database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum LocaleProvider {
    /// The C library (the default).
    Libc,
    /// ICU (PostgreSQL 15 and later).
    Icu,
    /// The built-in provider (PostgreSQL 17 and later).
    Builtin,
}

/// How `CREATE DATABASE` copies the template (PostgreSQL 15 and later).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum CreateStrategy {
    /// Copy block by block through the WAL (the default since 15).
    WalLog,
    /// Copy files with a checkpoint before and after.
    FileCopy,
}

/// `CREATE DATABASE`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateDatabase {
    /// Name of the new database.
    pub name: String,
    /// Owner, as a role name ([`RoleSpec::Name`]); the current user when
    /// absent.
    #[serde(default)]
    pub owner: Option<RoleSpec>,
    /// Template database (`template1` when absent).
    #[serde(default)]
    pub template: Option<String>,
    /// Encoding name, for example `UTF8`.
    #[serde(default)]
    pub encoding: Option<String>,
    /// Copy strategy (15+).
    #[serde(default)]
    pub strategy: Option<CreateStrategy>,
    /// `LOCALE`: sets both collation and character classification.
    #[serde(default)]
    pub locale: Option<String>,
    /// `LC_COLLATE`.
    #[serde(default)]
    pub lc_collate: Option<String>,
    /// `LC_CTYPE`.
    #[serde(default)]
    pub lc_ctype: Option<String>,
    /// Locale provider (15+; `builtin` 17+).
    #[serde(default)]
    pub locale_provider: Option<LocaleProvider>,
    /// ICU locale (15+).
    #[serde(default)]
    pub icu_locale: Option<String>,
    /// ICU collation rules (16+).
    #[serde(default)]
    pub icu_rules: Option<String>,
    /// Built-in provider locale, for example `C.UTF-8` (17+).
    #[serde(default)]
    pub builtin_locale: Option<String>,
    /// Tablespace.
    #[serde(default)]
    pub tablespace: Option<String>,
    /// `ALLOW_CONNECTIONS`.
    #[serde(default)]
    pub allow_connections: Option<bool>,
    /// `CONNECTION LIMIT` (`-1` is unlimited).
    #[serde(default)]
    pub connection_limit: Option<i32>,
    /// `IS_TEMPLATE`.
    #[serde(default)]
    pub is_template: Option<bool>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// One change of `ALTER DATABASE`, applied in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum DatabaseOperation {
    /// Rename (no one may be connected to it, including this session).
    Rename {
        /// New name.
        new_name: String,
    },
    /// Change the owner.
    Owner {
        /// New owner.
        owner: RoleSpec,
    },
    /// Connection limit (`-1` is unlimited).
    ConnectionLimit {
        /// Limit.
        limit: i32,
    },
    /// Whether connections are allowed.
    AllowConnections {
        /// Allowed.
        allow: bool,
    },
    /// Whether the database can be cloned as a template.
    IsTemplate {
        /// Template.
        is_template: bool,
    },
    /// `SET name = value`: a default for sessions in this database.
    Set {
        /// The setting.
        setting: ConfigSetting,
    },
    /// `RESET name`.
    Reset {
        /// Parameter name.
        name: String,
    },
    /// `RESET ALL`.
    ResetAll,
    /// Move to another tablespace (cannot run in a transaction).
    Tablespace {
        /// Tablespace.
        tablespace: String,
    },
}

/// Changes to databases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DatabaseChange {
    /// Create a database.
    Create(Box<CreateDatabase>),
    /// Alter a database.
    Alter {
        /// The database.
        name: String,
        /// Changes in order.
        operations: Vec<DatabaseOperation>,
    },
    /// Drop a database.
    Drop {
        /// The database.
        name: String,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// `WITH (FORCE)`: terminate other sessions first (13+).
        #[serde(default)]
        force: bool,
    },
}

pub(crate) fn plan_database(
    server: &ServerInfo,
    change: &DatabaseChange,
) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        DatabaseChange::Create(create) => create_database(server, create, &mut plan)?,
        DatabaseChange::Alter { name, operations } => {
            if operations.is_empty() {
                return Err(invalid("no database changes given"));
            }
            let mut current = name.clone();
            for op in operations {
                if matches!(op, DatabaseOperation::Tablespace { .. }) && operations.len() > 1 {
                    return Err(invalid(
                        "moving a database to another tablespace cannot run in a transaction; make it a change of its own",
                    ));
                }
                let db = ident(&current)?;
                let statement = match op {
                    DatabaseOperation::Rename { new_name } => {
                        plan.warn("renaming a database needs every other session to disconnect from it, and this session must be connected to another database");
                        let s = format!("ALTER DATABASE {db} RENAME TO {}", ident(new_name)?);
                        current.clone_from(new_name);
                        s
                    }
                    DatabaseOperation::Owner { owner } => {
                        format!("ALTER DATABASE {db} OWNER TO {}", owner.owner_sql(server)?)
                    }
                    DatabaseOperation::ConnectionLimit { limit } => {
                        check_limit(*limit)?;
                        format!("ALTER DATABASE {db} WITH CONNECTION LIMIT {limit}")
                    }
                    DatabaseOperation::AllowConnections { allow } => {
                        format!(
                            "ALTER DATABASE {db} WITH ALLOW_CONNECTIONS {}",
                            bool_sql(*allow)
                        )
                    }
                    DatabaseOperation::IsTemplate { is_template } => {
                        format!(
                            "ALTER DATABASE {db} WITH IS_TEMPLATE {}",
                            bool_sql(*is_template)
                        )
                    }
                    DatabaseOperation::Set { setting } => {
                        format!("ALTER DATABASE {db} SET {}", config_assignment(setting)?)
                    }
                    DatabaseOperation::Reset { name } => {
                        format!("ALTER DATABASE {db} RESET {}", config_name(name)?)
                    }
                    DatabaseOperation::ResetAll => format!("ALTER DATABASE {db} RESET ALL"),
                    DatabaseOperation::Tablespace { tablespace } => {
                        plan.non_transactional();
                        plan.warn("moving a database copies all its files and needs every session to disconnect from it");
                        format!("ALTER DATABASE {db} SET TABLESPACE {}", ident(tablespace)?)
                    }
                };
                plan.push(statement);
            }
        }
        DatabaseChange::Drop {
            name,
            if_exists,
            force,
        } => {
            plan.non_transactional();
            plan.destructive();
            let mut s = String::from("DROP DATABASE ");
            if *if_exists {
                s.push_str("IF EXISTS ");
            }
            s.push_str(&ident(name)?);
            if *force {
                require(server, 13, "DROP DATABASE ... WITH (FORCE)")?;
                plan.warn("FORCE terminates every other session connected to the database");
                s.push_str(" WITH (FORCE)");
            }
            plan.push(s);
        }
    }
    plan.finish()
}

fn create_database(
    server: &ServerInfo,
    create: &CreateDatabase,
    plan: &mut PlanBuilder,
) -> CatalogResult<()> {
    plan.non_transactional();
    let mut options = Vec::new();
    if let Some(owner) = &create.owner {
        // The CREATE DATABASE grammar takes a role name here, not
        // CURRENT_USER and friends (ALTER DATABASE ... OWNER TO does).
        let RoleSpec::Name { name } = owner else {
            return Err(invalid(
                "CREATE DATABASE needs the owner as a role name; leave it out to make the current user the owner",
            ));
        };
        options.push(format!("OWNER = {}", ident(name)?));
    }
    if let Some(template) = &create.template {
        options.push(format!("TEMPLATE = {}", ident(template)?));
    }
    if let Some(encoding) = &create.encoding {
        options.push(format!("ENCODING = {}", lit(encoding)?));
    }
    if let Some(strategy) = create.strategy {
        require(server, 15, "STRATEGY")?;
        options.push(format!(
            "STRATEGY = {}",
            match strategy {
                CreateStrategy::WalLog => "WAL_LOG",
                CreateStrategy::FileCopy => "FILE_COPY",
            }
        ));
    }
    if let Some(locale) = &create.locale {
        options.push(format!("LOCALE = {}", lit(locale)?));
    }
    if let Some(v) = &create.lc_collate {
        options.push(format!("LC_COLLATE = {}", lit(v)?));
    }
    if let Some(v) = &create.lc_ctype {
        options.push(format!("LC_CTYPE = {}", lit(v)?));
    }
    if let Some(provider) = create.locale_provider {
        require(server, 15, "LOCALE_PROVIDER")?;
        let name = match provider {
            LocaleProvider::Libc => "libc",
            LocaleProvider::Icu => "icu",
            LocaleProvider::Builtin => {
                require(server, 17, "the builtin locale provider")?;
                "builtin"
            }
        };
        options.push(format!("LOCALE_PROVIDER = {name}"));
    }
    let provider = create.locale_provider.unwrap_or(LocaleProvider::Libc);
    if let Some(v) = &create.icu_locale {
        require(server, 15, "ICU_LOCALE")?;
        if provider != LocaleProvider::Icu {
            return Err(invalid("an ICU locale needs the icu locale provider"));
        }
        options.push(format!("ICU_LOCALE = {}", lit(v)?));
    }
    if let Some(v) = &create.icu_rules {
        require(server, 16, "ICU_RULES")?;
        if provider != LocaleProvider::Icu {
            return Err(invalid("ICU rules need the icu locale provider"));
        }
        options.push(format!("ICU_RULES = {}", lit(v)?));
    }
    if let Some(v) = &create.builtin_locale {
        require(server, 17, "BUILTIN_LOCALE")?;
        if provider != LocaleProvider::Builtin {
            return Err(invalid(
                "a builtin locale needs the builtin locale provider",
            ));
        }
        options.push(format!("BUILTIN_LOCALE = {}", lit(v)?));
    }
    if provider == LocaleProvider::Icu && create.icu_locale.is_none() && create.locale.is_none() {
        return Err(invalid("the icu locale provider needs an ICU locale"));
    }
    if let Some(v) = &create.tablespace {
        options.push(format!("TABLESPACE = {}", ident(v)?));
    }
    if let Some(v) = create.allow_connections {
        options.push(format!("ALLOW_CONNECTIONS = {}", bool_sql(v)));
    }
    if let Some(v) = create.connection_limit {
        check_limit(v)?;
        options.push(format!("CONNECTION LIMIT = {v}"));
    }
    if let Some(v) = create.is_template {
        options.push(format!("IS_TEMPLATE = {}", bool_sql(v)));
    }
    let name = ident(&create.name)?;
    let mut s = format!("CREATE DATABASE {name}");
    if !options.is_empty() {
        s.push_str(" WITH ");
        s.push_str(&options.join(" "));
    }
    plan.push(s);
    if let Some(comment) = &create.comment {
        plan.push(comment_on(&format!("DATABASE {name}"), Some(comment))?);
    }
    Ok(())
}

fn check_limit(limit: i32) -> CatalogResult<()> {
    if limit < -1 {
        return Err(invalid("a connection limit is -1 (unlimited) or more"));
    }
    Ok(())
}

pub(crate) fn bool_sql(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// One change of `ALTER SCHEMA`, applied in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum SchemaOperation {
    /// Rename.
    Rename {
        /// New name.
        new_name: String,
    },
    /// Change the owner.
    Owner {
        /// New owner.
        owner: RoleSpec,
    },
}

/// Changes to schemas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SchemaChange {
    /// Create a schema.
    Create {
        /// Name.
        name: String,
        /// `AUTHORIZATION`: the owner.
        #[serde(default)]
        owner: Option<RoleSpec>,
        /// `IF NOT EXISTS`.
        #[serde(default)]
        if_not_exists: bool,
        /// Comment.
        #[serde(default)]
        comment: Option<String>,
    },
    /// Alter a schema.
    Alter {
        /// The schema.
        name: String,
        /// Changes in order.
        operations: Vec<SchemaOperation>,
    },
    /// Drop a schema.
    Drop {
        /// The schema.
        name: String,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// `CASCADE` drops everything in it.
        #[serde(default)]
        behavior: DropBehavior,
    },
}

pub(crate) fn plan_schema(server: &ServerInfo, change: &SchemaChange) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        SchemaChange::Create {
            name,
            owner,
            if_not_exists,
            comment,
        } => {
            let schema = ident(name)?;
            let mut s = String::from("CREATE SCHEMA ");
            if *if_not_exists {
                s.push_str("IF NOT EXISTS ");
            }
            s.push_str(&schema);
            if let Some(owner) = owner {
                s.push_str(" AUTHORIZATION ");
                s.push_str(&owner.owner_sql(server)?);
            }
            plan.push(s);
            if let Some(comment) = comment {
                plan.push(comment_on(&format!("SCHEMA {schema}"), Some(comment))?);
            }
        }
        SchemaChange::Alter { name, operations } => {
            if operations.is_empty() {
                return Err(invalid("no schema changes given"));
            }
            let mut current = name.clone();
            for op in operations {
                let schema = ident(&current)?;
                match op {
                    SchemaOperation::Rename { new_name } => {
                        plan.push(format!(
                            "ALTER SCHEMA {schema} RENAME TO {}",
                            ident(new_name)?
                        ));
                        current.clone_from(new_name);
                    }
                    SchemaOperation::Owner { owner } => plan.push(format!(
                        "ALTER SCHEMA {schema} OWNER TO {}",
                        owner.owner_sql(server)?
                    )),
                }
            }
        }
        SchemaChange::Drop {
            name,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            if *behavior == DropBehavior::Cascade {
                plan.warn(
                    "CASCADE drops every object in the schema and everything that depends on them",
                );
            }
            plan.push(format!(
                "DROP SCHEMA {}{} {}",
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
    fn create_database_with_every_option() -> CatalogResult<()> {
        let create = CreateDatabase {
            name: "shop".into(),
            owner: Some(RoleSpec::name("app")),
            template: Some("template0".into()),
            encoding: Some("UTF8".into()),
            strategy: Some(CreateStrategy::FileCopy),
            locale_provider: Some(LocaleProvider::Icu),
            icu_locale: Some("nl-NL".into()),
            icu_rules: Some("&a < b".into()),
            lc_collate: Some("C".into()),
            lc_ctype: Some("C".into()),
            connection_limit: Some(10),
            allow_connections: Some(true),
            is_template: Some(false),
            tablespace: Some("pg_default".into()),
            comment: Some("it's ours".into()),
            ..CreateDatabase::default()
        };
        let plan = plan_database(&pg(18), &DatabaseChange::Create(Box::new(create.clone())))?;
        assert_eq!(
            plan.statements,
            vec![
                r#"CREATE DATABASE "shop" WITH OWNER = "app" TEMPLATE = "template0" ENCODING = 'UTF8' STRATEGY = FILE_COPY LC_COLLATE = 'C' LC_CTYPE = 'C' LOCALE_PROVIDER = icu ICU_LOCALE = 'nl-NL' ICU_RULES = '&a < b' TABLESPACE = "pg_default" ALLOW_CONNECTIONS = true CONNECTION LIMIT = 10 IS_TEMPLATE = false"#,
                r#"COMMENT ON DATABASE "shop" IS 'it''s ours'"#,
            ]
        );
        assert!(!plan.transactional);
        assert!(!plan.destructive);
        assert!(plan_database(&pg(13), &DatabaseChange::Create(Box::new(create))).is_err());
        let builtin = CreateDatabase {
            name: "b".into(),
            locale_provider: Some(LocaleProvider::Builtin),
            builtin_locale: Some("C.UTF-8".into()),
            template: Some("template0".into()),
            ..CreateDatabase::default()
        };
        assert!(
            plan_database(&pg(16), &DatabaseChange::Create(Box::new(builtin.clone()))).is_err()
        );
        assert_eq!(
            plan_database(&pg(17), &DatabaseChange::Create(Box::new(builtin)))?.statements,
            vec![
                r#"CREATE DATABASE "b" WITH TEMPLATE = "template0" LOCALE_PROVIDER = builtin BUILTIN_LOCALE = 'C.UTF-8'"#
            ]
        );
        let current = CreateDatabase {
            name: "x".into(),
            owner: Some(RoleSpec::CurrentUser),
            ..CreateDatabase::default()
        };
        assert!(plan_database(&pg(18), &DatabaseChange::Create(Box::new(current))).is_err());
        let bad = CreateDatabase {
            name: "x".into(),
            icu_locale: Some("en".into()),
            ..CreateDatabase::default()
        };
        assert!(plan_database(&pg(18), &DatabaseChange::Create(Box::new(bad))).is_err());
        Ok(())
    }

    #[test]
    fn alter_and_drop_database() -> CatalogResult<()> {
        let plan = plan_database(
            &pg(13),
            &DatabaseChange::Alter {
                name: "a".into(),
                operations: vec![
                    DatabaseOperation::Rename {
                        new_name: "b".into(),
                    },
                    DatabaseOperation::Owner {
                        owner: RoleSpec::CurrentUser,
                    },
                    DatabaseOperation::ConnectionLimit { limit: 5 },
                    DatabaseOperation::AllowConnections { allow: true },
                    DatabaseOperation::IsTemplate { is_template: false },
                    DatabaseOperation::Set {
                        setting: ConfigSetting {
                            name: "work_mem".into(),
                            values: vec!["64MB".into()],
                        },
                    },
                    DatabaseOperation::Reset {
                        name: "work_mem".into(),
                    },
                    DatabaseOperation::ResetAll,
                ],
            },
        )?;
        assert_eq!(
            plan.statements,
            vec![
                r#"ALTER DATABASE "a" RENAME TO "b""#,
                r#"ALTER DATABASE "b" OWNER TO CURRENT_USER"#,
                r#"ALTER DATABASE "b" WITH CONNECTION LIMIT 5"#,
                r#"ALTER DATABASE "b" WITH ALLOW_CONNECTIONS true"#,
                r#"ALTER DATABASE "b" WITH IS_TEMPLATE false"#,
                r#"ALTER DATABASE "b" SET "work_mem" = '64MB'"#,
                r#"ALTER DATABASE "b" RESET "work_mem""#,
                r#"ALTER DATABASE "b" RESET ALL"#,
            ]
        );
        assert!(plan.transactional);
        let ts = plan_database(
            &pg(13),
            &DatabaseChange::Alter {
                name: "a".into(),
                operations: vec![DatabaseOperation::Tablespace {
                    tablespace: "fast".into(),
                }],
            },
        )?;
        assert_eq!(
            ts.statements,
            vec![r#"ALTER DATABASE "a" SET TABLESPACE "fast""#]
        );
        assert!(!ts.transactional);
        assert!(
            plan_database(
                &pg(13),
                &DatabaseChange::Alter {
                    name: "a".into(),
                    operations: vec![
                        DatabaseOperation::ResetAll,
                        DatabaseOperation::Tablespace {
                            tablespace: "fast".into()
                        },
                    ],
                },
            )
            .is_err()
        );
        let drop = plan_database(
            &pg(13),
            &DatabaseChange::Drop {
                name: "a".into(),
                if_exists: true,
                force: true,
            },
        )?;
        assert_eq!(
            drop.statements,
            vec![r#"DROP DATABASE IF EXISTS "a" WITH (FORCE)"#]
        );
        assert!(drop.destructive && !drop.transactional);
        assert_eq!(drop.warnings.len(), 1);
        Ok(())
    }

    #[test]
    fn schemas() -> CatalogResult<()> {
        let create = plan_schema(
            &pg(13),
            &SchemaChange::Create {
                name: "app".into(),
                owner: Some(RoleSpec::name("o")),
                if_not_exists: true,
                comment: Some("c".into()),
            },
        )?;
        assert_eq!(
            create.statements,
            vec![
                r#"CREATE SCHEMA IF NOT EXISTS "app" AUTHORIZATION "o""#,
                r#"COMMENT ON SCHEMA "app" IS 'c'"#,
            ]
        );
        let alter = plan_schema(
            &pg(13),
            &SchemaChange::Alter {
                name: "app".into(),
                operations: vec![
                    SchemaOperation::Rename {
                        new_name: "app2".into(),
                    },
                    SchemaOperation::Owner {
                        owner: RoleSpec::SessionUser,
                    },
                ],
            },
        )?;
        assert_eq!(
            alter.statements,
            vec![
                r#"ALTER SCHEMA "app" RENAME TO "app2""#,
                r#"ALTER SCHEMA "app2" OWNER TO SESSION_USER"#,
            ]
        );
        let drop = plan_schema(
            &pg(13),
            &SchemaChange::Drop {
                name: "app".into(),
                if_exists: false,
                behavior: DropBehavior::Cascade,
            },
        )?;
        assert_eq!(drop.statements, vec![r#"DROP SCHEMA "app" CASCADE"#]);
        assert!(drop.destructive && drop.transactional);
        Ok(())
    }
}
