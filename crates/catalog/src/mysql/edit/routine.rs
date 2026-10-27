//! Stored procedures and functions.

use serde::{Deserialize, Serialize};

use super::types::{DataType, Definer, SqlSecurity};
use super::{Plan, Target, fragment, ident, invalid, object_name, plain_text, unsupported};
use crate::model::CatalogResult;

/// Procedure or function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RoutineKind {
    /// `PROCEDURE`.
    Procedure,
    /// `FUNCTION`.
    Function,
}

impl RoutineKind {
    fn keyword(self) -> &'static str {
        match self {
            Self::Procedure => "PROCEDURE",
            Self::Function => "FUNCTION",
        }
    }
}

/// Direction of a procedure parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParameterMode {
    /// `IN`.
    In,
    /// `OUT`.
    Out,
    /// `INOUT`.
    Inout,
}

/// A routine parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineParameter {
    /// Direction (procedures only; `None` is `IN`).
    #[serde(default)]
    pub mode: Option<ParameterMode>,
    /// Name.
    pub name: String,
    /// Type.
    pub data_type: DataType,
}

/// What a routine does with data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DataAccess {
    /// `CONTAINS SQL`.
    ContainsSql,
    /// `NO SQL`.
    NoSql,
    /// `READS SQL DATA`.
    ReadsSqlData,
    /// `MODIFIES SQL DATA`.
    ModifiesSqlData,
}

impl DataAccess {
    fn clause(self) -> &'static str {
        match self {
            Self::ContainsSql => "CONTAINS SQL",
            Self::NoSql => "NO SQL",
            Self::ReadsSqlData => "READS SQL DATA",
            Self::ModifiesSqlData => "MODIFIES SQL DATA",
        }
    }
}

/// Routine characteristics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineCharacteristics {
    /// `COMMENT`.
    #[serde(default)]
    pub comment: Option<String>,
    /// `DETERMINISTIC` or `NOT DETERMINISTIC`.
    #[serde(default)]
    pub deterministic: Option<bool>,
    /// Data access.
    #[serde(default)]
    pub data_access: Option<DataAccess>,
    /// `SQL SECURITY`.
    #[serde(default)]
    pub security: Option<SqlSecurity>,
}

/// `CREATE PROCEDURE` / `CREATE FUNCTION`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRoutine {
    /// Database.
    pub database: String,
    /// Routine name.
    pub name: String,
    /// Procedure or function.
    pub routine: RoutineKind,
    /// Replace an existing routine of that name: `CREATE OR REPLACE` on
    /// MariaDB, `DROP ... IF EXISTS` then `CREATE` on MySQL.
    #[serde(default)]
    pub replace: bool,
    /// `DEFINER`.
    #[serde(default)]
    pub definer: Option<Definer>,
    /// Parameters in order.
    #[serde(default)]
    pub parameters: Vec<RoutineParameter>,
    /// Return type (functions only).
    #[serde(default)]
    pub returns: Option<DataType>,
    /// Characteristics.
    #[serde(default)]
    pub characteristics: RoutineCharacteristics,
    /// The body: one statement or a `BEGIN ... END` block.
    pub body: String,
}

/// `DROP PROCEDURE` / `DROP FUNCTION`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropRoutine {
    /// Database.
    pub database: String,
    /// Routine name.
    pub name: String,
    /// Procedure or function.
    pub routine: RoutineKind,
    /// `IF EXISTS`.
    #[serde(default)]
    pub if_exists: bool,
}

/// `ALTER PROCEDURE` / `ALTER FUNCTION`: changes the characteristics the
/// server can change in place (comment, data access, SQL security). Unlike
/// [`CreateRoutine`] with `replace` on MySQL, the routine is not dropped, so
/// its `EXECUTE` and `ALTER ROUTINE` grants stay. Fields left `None` are not
/// changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlterRoutine {
    /// Database.
    pub database: String,
    /// Routine name.
    pub name: String,
    /// Procedure or function.
    pub routine: RoutineKind,
    /// New comment; an empty string removes it (MariaDB only: MySQL ignores
    /// `COMMENT ''` in `ALTER`, so the change is refused there).
    #[serde(default)]
    pub comment: Option<String>,
    /// New data access.
    #[serde(default)]
    pub data_access: Option<DataAccess>,
    /// New `SQL SECURITY`.
    #[serde(default)]
    pub security: Option<SqlSecurity>,
}

pub(super) fn alter(t: &Target, c: &AlterRoutine, plan: &mut Plan) -> CatalogResult<()> {
    let mut parts = Vec::new();
    if let Some(comment) = &c.comment {
        if comment.is_empty() && !t.is_mariadb() {
            return Err(unsupported(format!(
                "MySQL ignores COMMENT '' in ALTER {}, so the comment cannot be removed in place; \
                 recreate the routine to remove it (its EXECUTE and ALTER ROUTINE grants are lost)",
                c.routine.keyword()
            )));
        }
        // `COMMENT ''` is what MariaDB stores for "no comment".
        parts.push(format!("COMMENT {}", plain_text(comment, "a comment")?));
    }
    if let Some(access) = c.data_access {
        parts.push(access.clause().to_owned());
    }
    if let Some(security) = c.security {
        parts.push(security.clause().to_owned());
    }
    if parts.is_empty() {
        return Err(invalid(format!(
            "ALTER {} needs at least one change",
            c.routine.keyword()
        )));
    }
    if c.routine == RoutineKind::Function {
        // The plan is made offline, so it states the server's rule instead
        // of reading log_bin.
        plan.warn(
            "with binary logging on and log_bin_trust_function_creators off, the server accepts ALTER FUNCTION only from an account with the SUPER privilege",
        );
    }
    plan.push(format!(
        "ALTER {} {} {}",
        c.routine.keyword(),
        object_name(&c.database, &c.name)?,
        parts.join(" ")
    ));
    Ok(())
}

pub(super) fn create(t: &Target, c: &CreateRoutine, plan: &mut Plan) -> CatalogResult<()> {
    let keyword = c.routine.keyword();
    let name = object_name(&c.database, &c.name)?;
    let mut params = Vec::with_capacity(c.parameters.len());
    for p in &c.parameters {
        let mode = match (c.routine, p.mode) {
            (RoutineKind::Function, Some(_)) => {
                return Err(invalid("function parameters have no IN, OUT or INOUT"));
            }
            (_, None) => "",
            (RoutineKind::Procedure, Some(ParameterMode::In)) => "IN ",
            (RoutineKind::Procedure, Some(ParameterMode::Out)) => "OUT ",
            (RoutineKind::Procedure, Some(ParameterMode::Inout)) => "INOUT ",
        };
        params.push(format!(
            "{mode}{} {}",
            ident(&p.name)?,
            p.data_type.render(t, plan)?
        ));
    }
    let returns = match (c.routine, &c.returns) {
        (RoutineKind::Function, Some(ty)) => format!(" RETURNS {}", ty.render(t, plan)?),
        (RoutineKind::Function, None) => return Err(invalid("a function needs a return type")),
        (RoutineKind::Procedure, Some(_)) => {
            return Err(invalid("a procedure has no return type"));
        }
        (RoutineKind::Procedure, None) => String::new(),
    };
    let mut sql = String::from("CREATE");
    if c.replace {
        if t.is_mariadb() {
            sql.push_str(" OR REPLACE");
        } else {
            plan.destructive();
            plan.warn(format!(
                "MySQL has no CREATE OR REPLACE {keyword}: the routine is dropped and created again, its EXECUTE and ALTER ROUTINE grants are removed and calls in between fail"
            ));
            plan.push(format!("DROP {keyword} IF EXISTS {name}"));
        }
    }
    if let Some(definer) = &c.definer {
        sql.push(' ');
        sql.push_str(&definer.clause()?);
    }
    sql.push_str(&format!(
        " {keyword} {name}({}){returns}",
        params.join(", ")
    ));
    let ch = &c.characteristics;
    if let Some(comment) = &ch.comment {
        sql.push_str(&format!(" COMMENT {}", plain_text(comment, "a comment")?));
    }
    if let Some(deterministic) = ch.deterministic {
        sql.push_str(if deterministic {
            " DETERMINISTIC"
        } else {
            " NOT DETERMINISTIC"
        });
    }
    if let Some(access) = ch.data_access {
        sql.push(' ');
        sql.push_str(access.clause());
    }
    if let Some(security) = ch.security {
        sql.push(' ');
        sql.push_str(security.clause());
    }
    if c.routine == RoutineKind::Function
        && !t.is_mariadb()
        && ch.deterministic != Some(true)
        && !matches!(
            ch.data_access,
            Some(DataAccess::NoSql | DataAccess::ReadsSqlData)
        )
    {
        plan.warn(
            "with binary logging on, MySQL refuses functions that are not DETERMINISTIC, NO SQL or READS SQL DATA unless log_bin_trust_function_creators is set",
        );
    }
    sql.push_str(&format!("\n{}", fragment(&c.body, "a routine body")?));
    plan.push(sql);
    Ok(())
}

pub(super) fn drop(c: &DropRoutine, plan: &mut Plan) -> CatalogResult<()> {
    plan.destructive();
    plan.push(format!(
        "DROP {} {}{}",
        c.routine.keyword(),
        if c.if_exists { "IF EXISTS " } else { "" },
        object_name(&c.database, &c.name)?
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::model::CatalogResult;

    const PROC: &str = r#"{"change": "createRoutine", "database": "d", "name": "p", "routine": "procedure", "replace": true,
        "definer": {"kind": "currentUser"},
        "parameters": [
            {"mode": "in", "name": "a", "dataType": {"name": "int"}},
            {"mode": "out", "name": "b", "dataType": {"name": "varchar", "length": 10, "charset": "utf8mb4"}}
        ],
        "characteristics": {"comment": "demo", "deterministic": true, "dataAccess": "containsSql", "security": "invoker"},
        "body": "BEGIN SET b = CONCAT('x', a); END;"}"#;

    #[test]
    fn procedures_replace_per_engine() -> CatalogResult<()> {
        let body = " PROCEDURE `d`.`p`(IN `a` INT, OUT `b` VARCHAR(10) CHARACTER SET `utf8mb4`) COMMENT 'demo' DETERMINISTIC CONTAINS SQL SQL SECURITY INVOKER\nBEGIN SET b = CONCAT('x', a); END";
        let mysql = plan_json(&my80(), PROC)?;
        assert_eq!(
            mysql.statements,
            vec![
                "DROP PROCEDURE IF EXISTS `d`.`p`".to_string(),
                format!("CREATE DEFINER = CURRENT_USER{body}"),
            ]
        );
        assert!(mysql.destructive);
        assert_eq!(mysql.warnings.len(), 1);
        let maria = plan_json(&ma106(), PROC)?;
        assert_eq!(
            maria.statements,
            vec![format!("CREATE OR REPLACE DEFINER = CURRENT_USER{body}")]
        );
        assert!(!maria.destructive && maria.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn functions() -> CatalogResult<()> {
        let json = r#"{"change": "createRoutine", "database": "d", "name": "f", "routine": "function",
            "parameters": [{"name": "x", "dataType": {"name": "decimal", "length": 10, "scale": 2}}],
            "returns": {"name": "decimal", "length": 12, "scale": 2},
            "characteristics": {"deterministic": true, "dataAccess": "noSql"},
            "body": "RETURN x * 1.21"}"#;
        let plan = plan_json(&my97(), json)?;
        assert_eq!(
            sql(&plan),
            "CREATE FUNCTION `d`.`f`(`x` DECIMAL(10,2)) RETURNS DECIMAL(12,2) DETERMINISTIC NO SQL\nRETURN x * 1.21"
        );
        assert!(plan.warnings.is_empty());
        let no_returns = json.replace(
            r#""returns": {"name": "decimal", "length": 12, "scale": 2},"#,
            "",
        );
        assert!(is_invalid(&plan_json(&my97(), &no_returns)));
        let with_mode = json.replace(r#"[{"name": "x""#, r#"[{"mode": "in", "name": "x""#);
        assert!(is_invalid(&plan_json(&my97(), &with_mode)));
        let loose = json.replace(
            r#""characteristics": {"deterministic": true, "dataAccess": "noSql"},"#,
            "",
        );
        assert_eq!(plan_json(&my80(), &loose)?.warnings.len(), 1);
        Ok(())
    }

    #[test]
    fn alter_routine_keeps_the_routine() -> CatalogResult<()> {
        for server in [my80(), my97(), ma106(), ma123()] {
            let plan = plan_json(
                &server,
                r#"{"change": "alterRoutine", "database": "d", "name": "p", "routine": "procedure",
                    "comment": "it's p", "dataAccess": "readsSqlData", "security": "invoker"}"#,
            )?;
            assert_eq!(
                plan.statements,
                vec![
                    "ALTER PROCEDURE `d`.`p` COMMENT 'it''s p' READS SQL DATA SQL SECURITY INVOKER"
                        .to_string()
                ]
            );
            assert!(!plan.destructive && plan.warnings.is_empty());
        }
        let clear = r#"{"change": "alterRoutine", "database": "d", "name": "f", "routine": "function", "comment": ""}"#;
        let cleared = plan_json(&ma106(), clear)?;
        assert_eq!(sql(&cleared), "ALTER FUNCTION `d`.`f` COMMENT ''");
        // Functions state the binary log rule; procedures have none.
        assert_eq!(cleared.warnings.len(), 1, "{cleared:?}");
        assert!(cleared.warnings[0].contains("log_bin_trust_function_creators"));
        let function = plan_json(
            &my80(),
            r#"{"change": "alterRoutine", "database": "d", "name": "f", "routine": "function", "comment": "x"}"#,
        )?;
        assert_eq!(function.warnings.len(), 1, "{function:?}");
        for server in [my80(), my97()] {
            assert!(is_unsupported(&plan_json(&server, clear)));
        }
        assert!(is_invalid(&plan_json(
            &my80(),
            r#"{"change": "alterRoutine", "database": "d", "name": "f", "routine": "function"}"#,
        )));
        Ok(())
    }

    #[test]
    fn drop_routine() -> CatalogResult<()> {
        let plan = plan_json(
            &ma123(),
            r#"{"change": "dropRoutine", "database": "d", "name": "f", "routine": "function", "ifExists": true}"#,
        )?;
        assert_eq!(sql(&plan), "DROP FUNCTION IF EXISTS `d`.`f`");
        assert!(plan.destructive);
        Ok(())
    }
}
