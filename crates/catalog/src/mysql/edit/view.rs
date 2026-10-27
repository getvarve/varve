//! Views.

use serde::{Deserialize, Serialize};

use super::types::{Definer, SqlSecurity};
use super::{Plan, fragment, ident_list, object_name};
use crate::model::CatalogResult;

/// `ALGORITHM` of a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ViewAlgorithm {
    /// `UNDEFINED`: the server picks.
    Undefined,
    /// `MERGE`.
    Merge,
    /// `TEMPTABLE`.
    Temptable,
}

/// `WITH ... CHECK OPTION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckOption {
    /// `WITH CASCADED CHECK OPTION`.
    Cascaded,
    /// `WITH LOCAL CHECK OPTION`.
    Local,
}

/// `CREATE [OR REPLACE] VIEW`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateView {
    /// Database.
    pub database: String,
    /// View name.
    pub name: String,
    /// `OR REPLACE`: replaces an existing view in one statement.
    #[serde(default)]
    pub replace: bool,
    /// `ALGORITHM`.
    #[serde(default)]
    pub algorithm: Option<ViewAlgorithm>,
    /// `DEFINER`.
    #[serde(default)]
    pub definer: Option<Definer>,
    /// `SQL SECURITY`.
    #[serde(default)]
    pub security: Option<SqlSecurity>,
    /// Column names; the query's own when empty.
    #[serde(default)]
    pub columns: Vec<String>,
    /// The `SELECT`.
    pub query: String,
    /// `WITH CHECK OPTION`.
    #[serde(default)]
    pub check_option: Option<CheckOption>,
}

/// `DROP VIEW`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropView {
    /// Database.
    pub database: String,
    /// View name.
    pub name: String,
    /// `IF EXISTS`.
    #[serde(default)]
    pub if_exists: bool,
}

pub(super) fn create(c: &CreateView, plan: &mut Plan) -> CatalogResult<()> {
    let mut sql = String::from("CREATE");
    if c.replace {
        sql.push_str(" OR REPLACE");
    }
    if let Some(algorithm) = c.algorithm {
        sql.push_str(match algorithm {
            ViewAlgorithm::Undefined => " ALGORITHM = UNDEFINED",
            ViewAlgorithm::Merge => " ALGORITHM = MERGE",
            ViewAlgorithm::Temptable => " ALGORITHM = TEMPTABLE",
        });
        if algorithm == ViewAlgorithm::Temptable && c.check_option.is_some() {
            plan.warn("a TEMPTABLE view is not updatable, so its CHECK OPTION never applies");
        }
    }
    if let Some(definer) = &c.definer {
        sql.push(' ');
        sql.push_str(&definer.clause()?);
    }
    if let Some(security) = c.security {
        sql.push(' ');
        sql.push_str(security.clause());
    }
    sql.push_str(&format!(" VIEW {}", object_name(&c.database, &c.name)?));
    if !c.columns.is_empty() {
        sql.push_str(&format!(" ({})", ident_list(&c.columns)?));
    }
    sql.push_str(&format!(" AS {}", fragment(&c.query, "a view query")?));
    if let Some(option) = c.check_option {
        sql.push_str(match option {
            CheckOption::Cascaded => " WITH CASCADED CHECK OPTION",
            CheckOption::Local => " WITH LOCAL CHECK OPTION",
        });
    }
    plan.push(sql);
    Ok(())
}

pub(super) fn drop(c: &DropView, plan: &mut Plan) -> CatalogResult<()> {
    plan.destructive();
    plan.push(format!(
        "DROP VIEW {}{}",
        if c.if_exists { "IF EXISTS " } else { "" },
        object_name(&c.database, &c.name)?
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::model::CatalogResult;

    #[test]
    fn create_view_with_every_option() -> CatalogResult<()> {
        let json = r#"{"change": "createView", "database": "d", "name": "v", "replace": true, "algorithm": "merge",
            "definer": {"kind": "account", "user": "root", "host": "%"}, "security": "invoker",
            "columns": ["a", "b"], "query": "SELECT x, y FROM t WHERE x > 0;", "checkOption": "local"}"#;
        let expected = "CREATE OR REPLACE ALGORITHM = MERGE DEFINER = 'root'@'%' SQL SECURITY INVOKER VIEW `d`.`v` (`a`, `b`) AS SELECT x, y FROM t WHERE x > 0 WITH LOCAL CHECK OPTION";
        for server in [my80(), my97(), ma106(), ma123()] {
            let plan = plan_json(&server, json)?;
            assert_eq!(sql(&plan), expected);
            assert!(!plan.destructive);
        }
        let plain =
            r#"{"change": "createView", "database": "d", "name": "v", "query": "select 1"}"#;
        assert_eq!(
            sql(&plan_json(&my80(), plain)?),
            "CREATE VIEW `d`.`v` AS select 1"
        );
        assert!(is_invalid(&plan_json(
            &my80(),
            r#"{"change": "createView", "database": "d", "name": "v", "query": " ; "}"#
        )));
        Ok(())
    }

    #[test]
    fn drop_view() -> CatalogResult<()> {
        let plan = plan_json(
            &ma123(),
            r#"{"change": "dropView", "database": "d", "name": "v", "ifExists": true}"#,
        )?;
        assert_eq!(sql(&plan), "DROP VIEW IF EXISTS `d`.`v`");
        assert!(plan.destructive);
        Ok(())
    }
}
