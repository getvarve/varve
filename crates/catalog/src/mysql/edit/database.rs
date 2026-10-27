//! Databases (MySQL and MariaDB schemas).

use serde::{Deserialize, Serialize};

use super::{Plan, Target, ident, invalid, plain_text};
use crate::model::CatalogResult;

/// `CREATE DATABASE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDatabase {
    /// Database name.
    pub name: String,
    /// `IF NOT EXISTS`.
    #[serde(default)]
    pub if_not_exists: bool,
    /// Default character set.
    #[serde(default)]
    pub charset: Option<String>,
    /// Default collation.
    #[serde(default)]
    pub collation: Option<String>,
    /// MySQL 8.0.16+: default table encryption (`ENCRYPTION 'Y'` / `'N'`).
    #[serde(default)]
    pub encryption: Option<bool>,
    /// MariaDB 10.5+: database comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// `ALTER DATABASE`. Fields left `None` are not changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlterDatabase {
    /// Database name.
    pub name: String,
    /// New default character set.
    #[serde(default)]
    pub charset: Option<String>,
    /// New default collation.
    #[serde(default)]
    pub collation: Option<String>,
    /// MySQL 8.0.16+: default table encryption.
    #[serde(default)]
    pub encryption: Option<bool>,
    /// MySQL 8.0.22+: `READ ONLY`.
    #[serde(default)]
    pub read_only: Option<bool>,
    /// MariaDB 10.5+: database comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// `DROP DATABASE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropDatabase {
    /// Database name.
    pub name: String,
    /// `IF EXISTS`.
    #[serde(default)]
    pub if_exists: bool,
}

/// Character set, collation, encryption and comment options shared by
/// create and alter.
fn options(
    t: &Target,
    charset: Option<&String>,
    collation: Option<&String>,
    encryption: Option<bool>,
    comment: Option<&String>,
) -> CatalogResult<Vec<String>> {
    let mut out = Vec::new();
    if let Some(charset) = charset {
        out.push(format!("CHARACTER SET {}", ident(charset)?));
    }
    if let Some(collation) = collation {
        out.push(format!("COLLATE {}", ident(collation)?));
    }
    if let Some(encryption) = encryption {
        if !t.mysql_at(8, 0, 16) {
            return Err(t.lacks("a database default encryption (MySQL 8.0.16+)"));
        }
        out.push(format!(
            "ENCRYPTION '{}'",
            if encryption { "Y" } else { "N" }
        ));
    }
    if let Some(comment) = comment {
        if !t.mariadb_at(10, 5) {
            return Err(t.lacks("database comments (MariaDB 10.5+)"));
        }
        out.push(format!("COMMENT {}", plain_text(comment, "a comment")?));
    }
    Ok(out)
}

pub(super) fn create(t: &Target, c: &CreateDatabase, plan: &mut Plan) -> CatalogResult<()> {
    let mut sql = String::from("CREATE DATABASE ");
    if c.if_not_exists {
        sql.push_str("IF NOT EXISTS ");
    }
    sql.push_str(&ident(&c.name)?);
    for option in options(
        t,
        c.charset.as_ref(),
        c.collation.as_ref(),
        c.encryption,
        c.comment.as_ref(),
    )? {
        sql.push(' ');
        sql.push_str(&option);
    }
    plan.push(sql);
    Ok(())
}

pub(super) fn alter(t: &Target, c: &AlterDatabase, plan: &mut Plan) -> CatalogResult<()> {
    let mut parts = options(
        t,
        c.charset.as_ref(),
        c.collation.as_ref(),
        c.encryption,
        c.comment.as_ref(),
    )?;
    if let Some(read_only) = c.read_only {
        if !t.mysql_at(8, 0, 22) {
            return Err(t.lacks("read-only databases (MySQL 8.0.22+)"));
        }
        parts.push(format!("READ ONLY = {}", u8::from(read_only)));
    }
    if parts.is_empty() {
        return Err(invalid("ALTER DATABASE needs at least one change"));
    }
    if c.charset.is_some() || c.collation.is_some() {
        plan.warn(
            "the new default character set and collation apply to tables created later; existing tables keep theirs",
        );
    }
    if c.encryption.is_some() {
        plan.warn("the default encryption applies to tables created later");
    }
    plan.push(format!(
        "ALTER DATABASE {} {}",
        ident(&c.name)?,
        parts.join(" ")
    ));
    Ok(())
}

pub(super) fn drop(c: &DropDatabase, plan: &mut Plan) -> CatalogResult<()> {
    let name = ident(&c.name)?;
    plan.destructive();
    plan.warn(format!(
        "drops database {name} with every table, view, routine, trigger and event in it"
    ));
    plan.push(format!(
        "DROP DATABASE {}{name}",
        if c.if_exists { "IF EXISTS " } else { "" }
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::model::CatalogResult;

    #[test]
    fn create_per_engine() -> CatalogResult<()> {
        let json = r#"{"change":"createDatabase","name":"shop","ifNotExists":true,"charset":"utf8mb4","collation":"utf8mb4_bin","encryption":false}"#;
        let plan = plan_json(&my80(), json)?;
        assert_eq!(
            sql(&plan),
            "CREATE DATABASE IF NOT EXISTS `shop` CHARACTER SET `utf8mb4` COLLATE `utf8mb4_bin` ENCRYPTION 'N'"
        );
        assert!(!plan.destructive && !plan.transactional);
        assert!(is_unsupported(&plan_json(&ma123(), json)));
        assert!(is_unsupported(&plan_json(&mysql(8, 0, 15), json)));

        let maria = r#"{"change":"createDatabase","name":"shop","comment":"the shop"}"#;
        assert_eq!(
            sql(&plan_json(&ma106(), maria)?),
            "CREATE DATABASE `shop` COMMENT 'the shop'"
        );
        assert!(is_unsupported(&plan_json(&my97(), maria)));
        Ok(())
    }

    #[test]
    fn alter_per_engine() -> CatalogResult<()> {
        let json = r#"{"change":"alterDatabase","name":"shop","charset":"latin1","readOnly":true}"#;
        let plan = plan_json(&my97(), json)?;
        assert_eq!(
            sql(&plan),
            "ALTER DATABASE `shop` CHARACTER SET `latin1` READ ONLY = 1"
        );
        assert_eq!(plan.warnings.len(), 1);
        assert!(is_unsupported(&plan_json(&mysql(8, 0, 21), json)));
        assert!(is_unsupported(&plan_json(&ma123(), json)));
        assert_eq!(
            sql(&plan_json(
                &ma123(),
                r#"{"change":"alterDatabase","name":"shop","collation":"utf8mb4_bin","comment":"x"}"#
            )?),
            "ALTER DATABASE `shop` COLLATE `utf8mb4_bin` COMMENT 'x'"
        );
        assert!(is_invalid(&plan_json(
            &my80(),
            r#"{"change":"alterDatabase","name":"shop"}"#
        )));
        Ok(())
    }

    #[test]
    fn drop_is_destructive() -> CatalogResult<()> {
        let plan = plan_json(&ma106(), r#"{"change":"dropDatabase","name":"a`b"}"#)?;
        assert_eq!(sql(&plan), "DROP DATABASE `a``b`");
        assert!(plan.destructive);
        assert_eq!(plan.warnings.len(), 1);
        Ok(())
    }
}
