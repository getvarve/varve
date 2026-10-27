//! Functions and procedures.

use serde::{Deserialize, Serialize};
use varve_driver::ServerInfo;

use super::common::{
    ConfigSetting, DropBehavior, PlanBuilder, QualifiedName, RoleSpec, comment_on,
    config_assignment, config_name, func_type, ident, invalid, lit, paren_expr, require, type_list,
};
use crate::model::{CatalogResult, EditPlan};

/// Function or procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum RoutineKind {
    /// `FUNCTION`.
    Function,
    /// `PROCEDURE`.
    Procedure,
}

impl RoutineKind {
    fn sql(self) -> &'static str {
        match self {
            Self::Function => "FUNCTION",
            Self::Procedure => "PROCEDURE",
        }
    }
}

/// Argument mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum ArgMode {
    /// `IN` (the default).
    #[default]
    In,
    /// `OUT` (procedures: 14+).
    Out,
    /// `INOUT`.
    InOut,
    /// `VARIADIC`.
    Variadic,
}

/// One argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct RoutineArg {
    /// Mode.
    #[serde(default)]
    pub mode: ArgMode,
    /// Name (optional).
    #[serde(default)]
    pub name: Option<String>,
    /// Type as SQL.
    pub data_type: String,
    /// Default expression (`IN`, `INOUT` and `VARIADIC` only).
    #[serde(default)]
    pub default: Option<String>,
}

impl RoutineArg {
    /// An `IN` argument.
    #[must_use]
    pub fn new(name: impl Into<String>, data_type: impl Into<String>) -> Self {
        Self {
            mode: ArgMode::In,
            name: Some(name.into()),
            data_type: data_type.into(),
            default: None,
        }
    }
}

/// A column of `RETURNS TABLE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct ReturnColumn {
    /// Name.
    pub name: String,
    /// Type as SQL.
    pub data_type: String,
}

/// What a function returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Returns {
    /// `RETURNS [SETOF] type` (also `void`, `trigger`, `event_trigger`).
    Type {
        /// Type as SQL.
        data_type: String,
        /// `SETOF`.
        #[serde(default)]
        setof: bool,
    },
    /// `RETURNS TABLE (...)`.
    Table {
        /// Columns.
        columns: Vec<ReturnColumn>,
    },
}

/// The body of a routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RoutineBody {
    /// `AS $$...$$`: the source text for the language (dollar-quoted with
    /// a tag that does not occur in the text).
    Definition {
        /// Source.
        source: String,
    },
    /// `AS 'file', 'symbol'` for C functions.
    Object {
        /// Shared library.
        file: String,
        /// Link symbol.
        symbol: String,
    },
    /// SQL-standard `RETURN expression` (14+, `LANGUAGE sql` functions).
    Return {
        /// Expression.
        expression: String,
    },
    /// SQL-standard `BEGIN ATOMIC ... END` (14+, `LANGUAGE sql`).
    Atomic {
        /// Statements, each without a trailing `;`.
        statements: Vec<String>,
    },
}

/// Volatility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum Volatility {
    /// `IMMUTABLE`.
    Immutable,
    /// `STABLE`.
    Stable,
    /// `VOLATILE`.
    Volatile,
}

impl Volatility {
    fn sql(self) -> &'static str {
        match self {
            Self::Immutable => "IMMUTABLE",
            Self::Stable => "STABLE",
            Self::Volatile => "VOLATILE",
        }
    }
}

/// Parallel safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum Parallel {
    /// `PARALLEL UNSAFE`.
    Unsafe,
    /// `PARALLEL RESTRICTED`.
    Restricted,
    /// `PARALLEL SAFE`.
    Safe,
}

impl Parallel {
    fn sql(self) -> &'static str {
        match self {
            Self::Unsafe => "PARALLEL UNSAFE",
            Self::Restricted => "PARALLEL RESTRICTED",
            Self::Safe => "PARALLEL SAFE",
        }
    }
}

/// `CREATE [OR REPLACE] FUNCTION | PROCEDURE`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateRoutine {
    /// Function or procedure.
    pub routine: RoutineKind,
    /// Name.
    pub name: QualifiedName,
    /// `OR REPLACE`.
    #[serde(default)]
    pub or_replace: bool,
    /// Arguments.
    #[serde(default)]
    pub arguments: Vec<RoutineArg>,
    /// Return type (functions only; required for them).
    #[serde(default)]
    pub returns: Option<Returns>,
    /// Language, for example `plpgsql`, `sql`, `c`.
    pub language: String,
    /// Body.
    pub body: RoutineBody,
    /// Volatility (functions).
    #[serde(default)]
    pub volatility: Option<Volatility>,
    /// `STRICT` (`RETURNS NULL ON NULL INPUT`) when true, `CALLED ON NULL
    /// INPUT` when false (functions).
    #[serde(default)]
    pub strict: Option<bool>,
    /// `SECURITY DEFINER` when true, `SECURITY INVOKER` when false.
    #[serde(default)]
    pub security_definer: Option<bool>,
    /// `LEAKPROOF` (functions).
    #[serde(default)]
    pub leakproof: Option<bool>,
    /// Parallel safety (functions).
    #[serde(default)]
    pub parallel: Option<Parallel>,
    /// Estimated cost (functions).
    #[serde(default)]
    pub cost: Option<f64>,
    /// Estimated rows (set-returning functions).
    #[serde(default)]
    pub rows: Option<f64>,
    /// `WINDOW` (C window functions).
    #[serde(default)]
    pub window: bool,
    /// `SET` configuration while the routine runs.
    #[serde(default)]
    pub settings: Vec<ConfigSetting>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// One change of `ALTER FUNCTION | PROCEDURE`, applied in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RoutineOperation {
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
    /// Change the owner.
    Owner {
        /// New owner.
        owner: RoleSpec,
    },
    /// `SECURITY DEFINER` / `SECURITY INVOKER`.
    Security {
        /// True for definer.
        definer: bool,
    },
    /// Volatility (functions).
    Volatility {
        /// Volatility.
        volatility: Volatility,
    },
    /// `STRICT` / `CALLED ON NULL INPUT` (functions).
    Strict {
        /// Strict.
        strict: bool,
    },
    /// `LEAKPROOF` / `NOT LEAKPROOF` (functions).
    Leakproof {
        /// Leakproof.
        leakproof: bool,
    },
    /// Parallel safety (functions).
    Parallel {
        /// Safety.
        parallel: Parallel,
    },
    /// `COST` (functions).
    Cost {
        /// Cost.
        cost: f64,
    },
    /// `ROWS` (set-returning functions).
    Rows {
        /// Rows.
        rows: f64,
    },
    /// `SET name = value`.
    Set {
        /// Setting.
        setting: ConfigSetting,
    },
    /// `RESET name`.
    Reset {
        /// Name.
        name: String,
    },
}

/// Changes to functions and procedures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RoutineChange {
    /// Create or replace a routine.
    Create(CreateRoutine),
    /// Alter a routine.
    Alter {
        /// Function or procedure.
        routine: RoutineKind,
        /// Name.
        name: QualifiedName,
        /// Argument types identifying the overload, as
        /// `pg_get_function_identity_arguments` returns them (empty for none).
        #[serde(default)]
        signature: String,
        /// Changes in order.
        operations: Vec<RoutineOperation>,
    },
    /// Drop a routine.
    Drop {
        /// Function or procedure.
        routine: RoutineKind,
        /// Name.
        name: QualifiedName,
        /// Identity arguments (empty for none).
        #[serde(default)]
        signature: String,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent objects (triggers, defaults, views).
        #[serde(default)]
        behavior: DropBehavior,
    },
}

/// A language name as `pg_language` stores it. A name that would be a
/// plain identifier is folded to lower case (as PostgreSQL folds an
/// unquoted name) and written bare unless it is a reserved word; anything
/// else is quoted.
pub(crate) fn language_name(name: &str) -> CatalogResult<String> {
    let simple = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !simple {
        return ident(name);
    }
    let lower = name.to_ascii_lowercase();
    if super::common::RESERVED_WORDS.contains(&lower.as_str()) {
        ident(&lower)
    } else {
        Ok(lower)
    }
}

/// `name(signature)`.
pub(crate) fn with_signature(name: &QualifiedName, signature: &str) -> CatalogResult<String> {
    let args = if signature.trim().is_empty() {
        String::new()
    } else {
        type_list(signature, "an argument list")?
    };
    Ok(format!("{}({args})", name.sql()?))
}

fn number(value: f64, what: &str) -> CatalogResult<String> {
    if !value.is_finite() || value <= 0.0 {
        return Err(invalid(format!("{what} must be a positive number")));
    }
    Ok(format!("{value}"))
}

fn function_only(kind: RoutineKind, what: &str) -> CatalogResult<()> {
    if kind == RoutineKind::Procedure {
        return Err(invalid(format!("{what} only applies to functions")));
    }
    Ok(())
}

pub(crate) fn plan_routine(server: &ServerInfo, change: &RoutineChange) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        RoutineChange::Create(create) => create_routine(server, create, &mut plan)?,
        RoutineChange::Alter {
            routine,
            name,
            signature,
            operations,
        } => {
            if operations.is_empty() {
                return Err(invalid("no routine changes given"));
            }
            let mut current = name.clone();
            for op in operations {
                let alter = format!(
                    "ALTER {} {}",
                    routine.sql(),
                    with_signature(&current, signature)?
                );
                let statement = match op {
                    RoutineOperation::Rename { new_name } => {
                        let s = format!("{alter} RENAME TO {}", ident(new_name)?);
                        current = current.renamed(new_name);
                        s
                    }
                    RoutineOperation::SetSchema { schema } => {
                        let s = format!("{alter} SET SCHEMA {}", ident(schema)?);
                        current.schema = Some(schema.clone());
                        s
                    }
                    RoutineOperation::Owner { owner } => {
                        format!("{alter} OWNER TO {}", owner.owner_sql(server)?)
                    }
                    RoutineOperation::Security { definer } => {
                        if *definer {
                            plan.warn("a SECURITY DEFINER routine runs with its owner's rights; set search_path on it so callers cannot redirect its lookups");
                        }
                        format!(
                            "{alter} SECURITY {}",
                            if *definer { "DEFINER" } else { "INVOKER" }
                        )
                    }
                    RoutineOperation::Volatility { volatility } => {
                        function_only(*routine, "volatility")?;
                        format!("{alter} {}", volatility.sql())
                    }
                    RoutineOperation::Strict { strict } => {
                        function_only(*routine, "STRICT")?;
                        format!(
                            "{alter} {}",
                            if *strict {
                                "STRICT"
                            } else {
                                "CALLED ON NULL INPUT"
                            }
                        )
                    }
                    RoutineOperation::Leakproof { leakproof } => {
                        function_only(*routine, "LEAKPROOF")?;
                        format!(
                            "{alter} {}",
                            if *leakproof {
                                "LEAKPROOF"
                            } else {
                                "NOT LEAKPROOF"
                            }
                        )
                    }
                    RoutineOperation::Parallel { parallel } => {
                        function_only(*routine, "parallel safety")?;
                        format!("{alter} {}", parallel.sql())
                    }
                    RoutineOperation::Cost { cost } => {
                        function_only(*routine, "COST")?;
                        format!("{alter} COST {}", number(*cost, "a cost")?)
                    }
                    RoutineOperation::Rows { rows } => {
                        function_only(*routine, "ROWS")?;
                        format!("{alter} ROWS {}", number(*rows, "a row estimate")?)
                    }
                    RoutineOperation::Set { setting } => {
                        format!("{alter} SET {}", config_assignment(setting)?)
                    }
                    RoutineOperation::Reset { name } => {
                        format!("{alter} RESET {}", config_name(name)?)
                    }
                };
                plan.push(statement);
            }
        }
        RoutineChange::Drop {
            routine,
            name,
            signature,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            plan.push(format!(
                "DROP {} {}{} {}",
                routine.sql(),
                if *if_exists { "IF EXISTS " } else { "" },
                with_signature(name, signature)?,
                behavior.sql()
            ));
        }
    }
    plan.finish()
}

fn create_routine(
    server: &ServerInfo,
    create: &CreateRoutine,
    plan: &mut PlanBuilder,
) -> CatalogResult<()> {
    let kind = create.routine;
    let procedure = kind == RoutineKind::Procedure;
    let mut args = Vec::new();
    let mut seen_default = false;
    for arg in &create.arguments {
        let mut s = String::new();
        match arg.mode {
            ArgMode::In => {}
            ArgMode::Out => {
                if procedure {
                    require(server, 14, "OUT arguments of procedures")?;
                }
                s.push_str("OUT ");
            }
            ArgMode::InOut => s.push_str("INOUT "),
            ArgMode::Variadic => s.push_str("VARIADIC "),
        }
        if let Some(n) = &arg.name {
            s.push_str(&ident(n)?);
            s.push(' ');
        }
        s.push_str(&func_type(&arg.data_type, "an argument type")?);
        if let Some(d) = &arg.default {
            if arg.mode == ArgMode::Out {
                return Err(invalid("OUT arguments cannot have a default"));
            }
            seen_default = true;
            s.push_str(" DEFAULT ");
            s.push_str(&paren_expr(d, "an argument default")?);
        } else if seen_default && matches!(arg.mode, ArgMode::In | ArgMode::InOut) {
            return Err(invalid(
                "every input argument after one with a default needs a default too",
            ));
        }
        args.push(s);
    }
    let name = create.name.sql()?;
    let mut lines = vec![format!(
        "CREATE {}{} {name}({})",
        if create.or_replace { "OR REPLACE " } else { "" },
        kind.sql(),
        args.join(", ")
    )];
    match (&create.returns, procedure) {
        (Some(_), true) => return Err(invalid("procedures do not return a value")),
        (None, false) => return Err(invalid("a function needs a return type")),
        (Some(Returns::Type { data_type, setof }), false) => lines.push(format!(
            "RETURNS {}{}",
            if *setof { "SETOF " } else { "" },
            func_type(data_type, "a return type")?
        )),
        (Some(Returns::Table { columns }), false) => {
            if columns.is_empty() {
                return Err(invalid("RETURNS TABLE needs at least one column"));
            }
            let cols = columns
                .iter()
                .map(|c| {
                    Ok(format!(
                        "{} {}",
                        ident(&c.name)?,
                        func_type(&c.data_type, "a column type")?
                    ))
                })
                .collect::<CatalogResult<Vec<_>>>()?;
            lines.push(format!("RETURNS TABLE ({})", cols.join(", ")));
        }
        (None, true) => {}
    }
    lines.push(format!("LANGUAGE {}", language_name(&create.language)?));
    if create.window {
        function_only(kind, "WINDOW")?;
        lines.push("WINDOW".into());
    }
    if let Some(v) = create.volatility {
        function_only(kind, "volatility")?;
        lines.push(v.sql().into());
    }
    if let Some(strict) = create.strict {
        function_only(kind, "STRICT")?;
        lines.push(
            if strict {
                "STRICT"
            } else {
                "CALLED ON NULL INPUT"
            }
            .into(),
        );
    }
    if let Some(definer) = create.security_definer {
        if definer {
            plan.warn("a SECURITY DEFINER routine runs with its owner's rights; set search_path on it so callers cannot redirect its lookups");
        }
        lines.push(
            if definer {
                "SECURITY DEFINER"
            } else {
                "SECURITY INVOKER"
            }
            .into(),
        );
    }
    if let Some(l) = create.leakproof {
        function_only(kind, "LEAKPROOF")?;
        lines.push(if l { "LEAKPROOF" } else { "NOT LEAKPROOF" }.into());
    }
    if let Some(p) = create.parallel {
        function_only(kind, "parallel safety")?;
        lines.push(p.sql().into());
    }
    if let Some(c) = create.cost {
        function_only(kind, "COST")?;
        lines.push(format!("COST {}", number(c, "a cost")?));
    }
    if let Some(r) = create.rows {
        function_only(kind, "ROWS")?;
        let set_returning = matches!(
            create.returns,
            Some(Returns::Type { setof: true, .. } | Returns::Table { .. })
        );
        if !set_returning {
            return Err(invalid("ROWS only applies to set-returning functions"));
        }
        lines.push(format!("ROWS {}", number(r, "a row estimate")?));
    }
    for setting in &create.settings {
        lines.push(format!("SET {}", config_assignment(setting)?));
    }
    match &create.body {
        RoutineBody::Definition { source } => {
            lines.push(format!("AS {}", crate::sql::dollar_quote(source)?));
        }
        RoutineBody::Object { file, symbol } => {
            lines.push(format!("AS {}, {}", lit(file)?, lit(symbol)?));
        }
        RoutineBody::Return { expression } => {
            require(server, 14, "SQL-standard routine bodies")?;
            if procedure {
                return Err(invalid(
                    "a procedure body cannot be RETURN; use BEGIN ATOMIC",
                ));
            }
            lines.push(format!(
                "RETURN {}",
                paren_expr(expression, "a RETURN expression")?
            ));
        }
        RoutineBody::Atomic { statements } => {
            require(server, 14, "SQL-standard routine bodies")?;
            let mut body = String::from("BEGIN ATOMIC\n");
            for statement in statements {
                body.push_str("    ");
                body.push_str(&super::common::statement(
                    statement,
                    "a statement of the body",
                )?);
                body.push_str(";\n");
            }
            body.push_str("END");
            lines.push(body);
        }
    }
    plan.push(lines.join("\n"));
    if let Some(comment) = &create.comment {
        let signature = create
            .arguments
            .iter()
            .filter(|a| a.mode != ArgMode::Out || procedure)
            .map(|a| {
                let mode = match a.mode {
                    ArgMode::In => "",
                    ArgMode::Out => "OUT ",
                    ArgMode::InOut => "INOUT ",
                    ArgMode::Variadic => "VARIADIC ",
                };
                Ok(format!(
                    "{mode}{}",
                    func_type(&a.data_type, "an argument type")?
                ))
            })
            .collect::<CatalogResult<Vec<_>>>()?
            .join(", ");
        plan.push(comment_on(
            &format!("{} {name}({signature})", kind.sql()),
            Some(comment),
        )?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::edit::common::tests::pg;

    fn base(kind: RoutineKind) -> CreateRoutine {
        CreateRoutine {
            routine: kind,
            name: QualifiedName::new("s", "f"),
            or_replace: false,
            arguments: vec![],
            returns: None,
            language: "sql".into(),
            body: RoutineBody::Definition {
                source: "SELECT 1".into(),
            },
            volatility: None,
            strict: None,
            security_definer: None,
            leakproof: None,
            parallel: None,
            cost: None,
            rows: None,
            window: false,
            settings: vec![],
            comment: None,
        }
    }

    #[test]
    fn create_function_with_everything() -> CatalogResult<()> {
        let f = CreateRoutine {
            or_replace: true,
            arguments: vec![
                RoutineArg::new("a", "integer"),
                RoutineArg {
                    default: Some("'x'".into()),
                    ..RoutineArg::new("b", "text")
                },
                RoutineArg {
                    mode: ArgMode::Out,
                    ..RoutineArg::new("c", "integer")
                },
            ],
            returns: Some(Returns::Type {
                data_type: "integer".into(),
                setof: true,
            }),
            language: "plpgsql".into(),
            body: RoutineBody::Definition {
                source: "BEGIN c := a; RETURN NEXT; END".into(),
            },
            volatility: Some(Volatility::Stable),
            strict: Some(true),
            security_definer: Some(true),
            leakproof: Some(false),
            parallel: Some(Parallel::Safe),
            cost: Some(10.0),
            rows: Some(5.0),
            settings: vec![ConfigSetting {
                name: "search_path".into(),
                values: vec!["pg_catalog".into(), "s".into()],
            }],
            comment: Some("does it".into()),
            ..base(RoutineKind::Function)
        };
        let plan = plan_routine(&pg(13), &RoutineChange::Create(f))?;
        assert_eq!(
            plan.statements,
            vec![
                "CREATE OR REPLACE FUNCTION \"s\".\"f\"(\"a\" integer, \"b\" text DEFAULT ('x'), OUT \"c\" integer)\nRETURNS SETOF integer\nLANGUAGE plpgsql\nSTABLE\nSTRICT\nSECURITY DEFINER\nNOT LEAKPROOF\nPARALLEL SAFE\nCOST 10\nROWS 5\nSET \"search_path\" = 'pg_catalog', 's'\nAS $$BEGIN c := a; RETURN NEXT; END$$",
                r#"COMMENT ON FUNCTION "s"."f"(integer, text) IS 'does it'"#,
            ]
        );
        assert!(plan.transactional && !plan.destructive);
        assert_eq!(plan.warnings.len(), 1);

        let table = plan_routine(
            &pg(14),
            &RoutineChange::Create(CreateRoutine {
                returns: Some(Returns::Table {
                    columns: vec![ReturnColumn {
                        name: "x".into(),
                        data_type: "int".into(),
                    }],
                }),
                body: RoutineBody::Return {
                    expression: "1".into(),
                },
                ..base(RoutineKind::Function)
            }),
        )?;
        assert_eq!(
            table.statements,
            vec![
                "CREATE FUNCTION \"s\".\"f\"()\nRETURNS TABLE (\"x\" int)\nLANGUAGE sql\nRETURN (1)"
            ]
        );
        let c = plan_routine(
            &pg(13),
            &RoutineChange::Create(CreateRoutine {
                language: "c".into(),
                returns: Some(Returns::Type {
                    data_type: "int".into(),
                    setof: false,
                }),
                body: RoutineBody::Object {
                    file: "$libdir/x".into(),
                    symbol: "f".into(),
                },
                ..base(RoutineKind::Function)
            }),
        )?;
        assert!(c.statements[0].ends_with("AS '$libdir/x', 'f'"));
        let nested = plan_routine(
            &pg(13),
            &RoutineChange::Create(CreateRoutine {
                returns: Some(Returns::Type {
                    data_type: "text".into(),
                    setof: false,
                }),
                body: RoutineBody::Definition {
                    source: "SELECT $$it's$$ || 'x'".into(),
                },
                ..base(RoutineKind::Function)
            }),
        )?;
        assert!(
            nested.statements[0].ends_with("AS $body$SELECT $$it's$$ || 'x'$body$"),
            "{}",
            nested.statements[0]
        );
        Ok(())
    }

    #[test]
    fn language_names() -> CatalogResult<()> {
        assert_eq!(language_name("plpgsql")?, "plpgsql");
        assert_eq!(language_name("SQL")?, "sql");
        assert_eq!(language_name("plpython3u")?, "plpython3u");
        assert_eq!(language_name("table")?, r#""table""#);
        assert_eq!(language_name("my lang")?, r#""my lang""#);
        assert_eq!(language_name("x\"; DROP")?, r#""x""; DROP""#);
        Ok(())
    }

    #[test]
    fn procedures_and_rules() -> CatalogResult<()> {
        let p = CreateRoutine {
            arguments: vec![
                RoutineArg::new("a", "int"),
                RoutineArg {
                    mode: ArgMode::Out,
                    ..RoutineArg::new("b", "int")
                },
            ],
            body: RoutineBody::Atomic {
                statements: vec!["SELECT a INTO b".into(), "SELECT 1".into()],
            },
            security_definer: Some(false),
            comment: Some("p".into()),
            ..base(RoutineKind::Procedure)
        };
        assert!(plan_routine(&pg(13), &RoutineChange::Create(p.clone())).is_err());
        let plan = plan_routine(&pg(14), &RoutineChange::Create(p.clone()))?;
        assert_eq!(
            plan.statements,
            vec![
                "CREATE PROCEDURE \"s\".\"f\"(\"a\" int, OUT \"b\" int)\nLANGUAGE sql\nSECURITY INVOKER\nBEGIN ATOMIC\n    SELECT a INTO b;\n    SELECT 1;\nEND",
                r#"COMMENT ON PROCEDURE "s"."f"(int, OUT int) IS 'p'"#,
            ]
        );
        let bad = |c: CreateRoutine| plan_routine(&pg(18), &RoutineChange::Create(c)).is_err();
        assert!(bad(CreateRoutine {
            volatility: Some(Volatility::Immutable),
            ..base(RoutineKind::Procedure)
        }));
        assert!(bad(CreateRoutine {
            returns: Some(Returns::Type {
                data_type: "int".into(),
                setof: false
            }),
            ..base(RoutineKind::Procedure)
        }));
        assert!(bad(base(RoutineKind::Function)));
        assert!(bad(CreateRoutine {
            returns: Some(Returns::Type {
                data_type: "int".into(),
                setof: false
            }),
            rows: Some(3.0),
            ..base(RoutineKind::Function)
        }));
        assert!(bad(CreateRoutine {
            returns: Some(Returns::Type {
                data_type: "int".into(),
                setof: false
            }),
            arguments: vec![
                RoutineArg {
                    default: Some("1".into()),
                    ..RoutineArg::new("a", "int")
                },
                RoutineArg::new("b", "int"),
            ],
            ..base(RoutineKind::Function)
        }));
        assert!(bad(CreateRoutine {
            returns: Some(Returns::Type {
                data_type: "int".into(),
                setof: false
            }),
            body: RoutineBody::Atomic {
                statements: vec!["SELECT 1; SELECT 2".into()]
            },
            ..base(RoutineKind::Function)
        }));
        Ok(())
    }

    #[test]
    fn alter_and_drop_routines() -> CatalogResult<()> {
        let plan = plan_routine(
            &pg(13),
            &RoutineChange::Alter {
                routine: RoutineKind::Function,
                name: QualifiedName::new("s", "f"),
                signature: "integer, text".into(),
                operations: vec![
                    RoutineOperation::Security { definer: true },
                    RoutineOperation::Volatility {
                        volatility: Volatility::Immutable,
                    },
                    RoutineOperation::Strict { strict: false },
                    RoutineOperation::Leakproof { leakproof: true },
                    RoutineOperation::Parallel {
                        parallel: Parallel::Restricted,
                    },
                    RoutineOperation::Cost { cost: 2.5 },
                    RoutineOperation::Rows { rows: 100.0 },
                    RoutineOperation::Set {
                        setting: ConfigSetting {
                            name: "work_mem".into(),
                            values: vec!["1MB".into()],
                        },
                    },
                    RoutineOperation::Reset {
                        name: "work_mem".into(),
                    },
                    RoutineOperation::Rename {
                        new_name: "g".into(),
                    },
                    RoutineOperation::SetSchema { schema: "t".into() },
                    RoutineOperation::Owner {
                        owner: RoleSpec::name("o"),
                    },
                ],
            },
        )?;
        let f = r#"ALTER FUNCTION "s"."f"(integer, text)"#;
        assert_eq!(
            plan.statements,
            vec![
                format!("{f} SECURITY DEFINER"),
                format!("{f} IMMUTABLE"),
                format!("{f} CALLED ON NULL INPUT"),
                format!("{f} LEAKPROOF"),
                format!("{f} PARALLEL RESTRICTED"),
                format!("{f} COST 2.5"),
                format!("{f} ROWS 100"),
                format!(r#"{f} SET "work_mem" = '1MB'"#),
                format!(r#"{f} RESET "work_mem""#),
                format!(r#"{f} RENAME TO "g""#),
                r#"ALTER FUNCTION "s"."g"(integer, text) SET SCHEMA "t""#.to_owned(),
                r#"ALTER FUNCTION "t"."g"(integer, text) OWNER TO "o""#.to_owned(),
            ]
        );
        assert!(
            plan_routine(
                &pg(13),
                &RoutineChange::Alter {
                    routine: RoutineKind::Procedure,
                    name: QualifiedName::new("s", "p"),
                    signature: String::new(),
                    operations: vec![RoutineOperation::Cost { cost: 1.0 }],
                },
            )
            .is_err()
        );
        let drop = plan_routine(
            &pg(13),
            &RoutineChange::Drop {
                routine: RoutineKind::Procedure,
                name: QualifiedName::new("s", "p"),
                signature: String::new(),
                if_exists: true,
                behavior: DropBehavior::Cascade,
            },
        )?;
        assert_eq!(
            drop.statements,
            vec![r#"DROP PROCEDURE IF EXISTS "s"."p"() CASCADE"#]
        );
        assert!(drop.destructive);
        Ok(())
    }
}
