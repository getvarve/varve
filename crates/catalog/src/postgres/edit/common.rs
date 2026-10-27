//! Building blocks shared by the PostgreSQL editors: names, roles, storage
//! parameters, the plan builder and the checks for SQL fragments the user
//! typed (expressions, data types, queries).

use serde::{Deserialize, Serialize};
use varve_driver::{
    Dialect, LexKind, Lexeme, ServerInfo, StatementKind, lex, split_statements_strict,
};

use super::typename;
use crate::model::{CatalogError, CatalogResult, EditPlan};
use crate::sql;

/// A possibly schema-qualified object name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct QualifiedName {
    /// Schema; `None` resolves through the session's `search_path`.
    #[serde(default)]
    pub schema: Option<String>,
    /// The object's name.
    pub name: String,
}

impl QualifiedName {
    /// A name in a schema.
    #[must_use]
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    /// A name resolved through the `search_path`.
    #[must_use]
    pub fn bare(name: impl Into<String>) -> Self {
        Self {
            schema: None,
            name: name.into(),
        }
    }

    /// The quoted, qualified name.
    pub(crate) fn sql(&self) -> CatalogResult<String> {
        sql::qualified(
            Dialect::Postgres,
            &[self.schema.as_deref(), Some(self.name.as_str())],
        )
    }

    /// The same schema with another name.
    pub(crate) fn renamed(&self, name: &str) -> Self {
        Self {
            schema: self.schema.clone(),
            name: name.to_owned(),
        }
    }
}

/// A role as written in `OWNER TO`, `TO` (policies) and `AUTHORIZATION`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RoleSpec {
    /// A named role.
    Name {
        /// The role's name.
        name: String,
    },
    /// `PUBLIC` (every role); only valid where privileges or policies apply.
    Public,
    /// `CURRENT_USER`.
    CurrentUser,
    /// `SESSION_USER`.
    SessionUser,
    /// `CURRENT_ROLE` (PostgreSQL 14 and later).
    CurrentRole,
}

impl RoleSpec {
    /// A named role.
    #[must_use]
    pub fn name(name: impl Into<String>) -> Self {
        Self::Name { name: name.into() }
    }

    pub(crate) fn sql(&self, server: &ServerInfo) -> CatalogResult<String> {
        Ok(match self {
            Self::Name { name } => ident(name)?,
            Self::Public => "PUBLIC".into(),
            Self::CurrentUser => "CURRENT_USER".into(),
            Self::SessionUser => "SESSION_USER".into(),
            Self::CurrentRole => {
                require(server, 14, "CURRENT_ROLE as a role specification")?;
                "CURRENT_ROLE".into()
            }
        })
    }

    /// The role as an owner: `PUBLIC` cannot own anything.
    pub(crate) fn owner_sql(&self, server: &ServerInfo) -> CatalogResult<String> {
        if matches!(self, Self::Public) {
            return Err(invalid("PUBLIC cannot own an object"));
        }
        self.sql(server)
    }
}

/// What happens to dependent objects on drop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum DropBehavior {
    /// Refuse when other objects depend on it (the default).
    #[default]
    Restrict,
    /// Drop dependent objects too.
    Cascade,
}

impl DropBehavior {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
        }
    }
}

/// A storage parameter (`WITH (name = value)`), for tables, indexes and
/// materialized views. Names may be prefixed (`toast.autovacuum_enabled`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct StorageParam {
    /// Parameter name, for example `fillfactor` or `toast.autovacuum_enabled`.
    pub name: String,
    /// Value as text; numbers are written as they are, anything else as a
    /// string literal (PostgreSQL parses both the same way).
    pub value: String,
}

impl StorageParam {
    /// A parameter.
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// A configuration parameter set on a database or routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSetting {
    /// Parameter name, for example `work_mem` or `search_path`.
    pub name: String,
    /// Value(s): one item for scalar parameters, several for list
    /// parameters such as `search_path`. Each is written as a string literal.
    pub values: Vec<String>,
}

/// Whether a trigger or rule fires (`ALTER TABLE ... ENABLE/DISABLE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum FiringState {
    /// `ENABLE`: fires in origin and local sessions.
    Enabled,
    /// `DISABLE`: never fires.
    Disabled,
    /// `ENABLE REPLICA`: fires only when `session_replication_role` is `replica`.
    Replica,
    /// `ENABLE ALWAYS`: fires regardless of `session_replication_role`.
    Always,
}

impl FiringState {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            Self::Enabled => "ENABLE",
            Self::Disabled => "DISABLE",
            Self::Replica => "ENABLE REPLICA",
            Self::Always => "ENABLE ALWAYS",
        }
    }
}

/// The lock warning shared by most `ALTER TABLE` forms.
pub(crate) const ACCESS_EXCLUSIVE: &str = "takes an ACCESS EXCLUSIVE lock, which blocks every read and write on the object until the transaction ends";

/// Collects the statements and flags of one plan.
#[derive(Debug)]
pub(crate) struct PlanBuilder {
    statements: Vec<String>,
    transactional: bool,
    destructive: bool,
    warnings: Vec<String>,
}

impl PlanBuilder {
    pub(crate) fn new() -> Self {
        Self {
            statements: Vec::new(),
            transactional: true,
            destructive: false,
            warnings: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, statement: String) {
        self.statements.push(statement);
    }

    /// Marks the plan as one that cannot run inside a transaction block.
    pub(crate) fn non_transactional(&mut self) {
        self.transactional = false;
    }

    pub(crate) fn destructive(&mut self) {
        self.destructive = true;
    }

    /// Adds a warning once.
    pub(crate) fn warn(&mut self, warning: impl Into<String>) {
        let warning = warning.into();
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }

    pub(crate) fn finish(self) -> CatalogResult<EditPlan> {
        if self.statements.is_empty() {
            return Err(invalid("the change has nothing to do"));
        }
        Ok(EditPlan {
            statements: self.statements,
            transactional: self.transactional,
            destructive: self.destructive,
            warnings: self.warnings,
        })
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> CatalogError {
    CatalogError::Invalid {
        message: message.into(),
    }
}

/// [`CatalogError::Unsupported`] unless the server is at least `major`.
pub(crate) fn require(server: &ServerInfo, major: u32, what: &str) -> CatalogResult<()> {
    if server.at_least(major, 0) {
        Ok(())
    } else {
        Err(CatalogError::Unsupported {
            message: format!(
                "{what} needs PostgreSQL {major} or later; the server runs {}",
                server.version
            ),
        })
    }
}

/// A quoted identifier.
pub(crate) fn ident(name: &str) -> CatalogResult<String> {
    sql::ident(Dialect::Postgres, name)
}

/// Quoted identifiers separated by `, `.
pub(crate) fn ident_list(names: &[String]) -> CatalogResult<String> {
    Ok(names
        .iter()
        .map(|n| ident(n))
        .collect::<CatalogResult<Vec<_>>>()?
        .join(", "))
}

/// A string literal.
pub(crate) fn lit(text: &str) -> CatalogResult<String> {
    sql::string(Dialect::Postgres, text)
}

/// `COMMENT ON <what> IS '...'` (or `IS NULL`).
pub(crate) fn comment_on(what: &str, comment: Option<&str>) -> CatalogResult<String> {
    let text = match comment {
        Some(c) => lit(c)?,
        None => "NULL".into(),
    };
    Ok(format!("COMMENT ON {what} IS {text}"))
}

/// A storage parameter list without the parentheses: `"fillfactor" = 70`.
pub(crate) fn storage_params(params: &[StorageParam]) -> CatalogResult<String> {
    if params.is_empty() {
        return Err(invalid("no storage parameters given"));
    }
    Ok(params
        .iter()
        .map(|p| {
            Ok(format!(
                "{} = {}",
                param_name(&p.name)?,
                param_value(&p.value)?
            ))
        })
        .collect::<CatalogResult<Vec<_>>>()?
        .join(", "))
}

/// Storage parameter names for `RESET (...)`.
pub(crate) fn storage_param_names(names: &[String]) -> CatalogResult<String> {
    if names.is_empty() {
        return Err(invalid("no storage parameters given"));
    }
    Ok(names
        .iter()
        .map(|n| param_name(n))
        .collect::<CatalogResult<Vec<_>>>()?
        .join(", "))
}

/// A parameter name, prefix and name quoted separately
/// (`"toast"."autovacuum_enabled"`).
fn param_name(name: &str) -> CatalogResult<String> {
    match name.split_once('.') {
        Some((prefix, rest)) => Ok(format!("{}.{}", ident(prefix)?, ident(rest)?)),
        None => ident(name),
    }
}

/// A number stays a number, anything else becomes a string literal.
fn param_value(value: &str) -> CatalogResult<String> {
    if is_number(value) {
        Ok(value.to_owned())
    } else {
        lit(value)
    }
}

fn is_number(text: &str) -> bool {
    let body = text.strip_prefix('-').unwrap_or(text);
    let mut digits = 0;
    let mut dots = 0;
    for c in body.chars() {
        match c {
            '0'..='9' => digits += 1,
            '.' => dots += 1,
            _ => return false,
        }
    }
    digits > 0 && dots <= 1
}

/// `name = 'v1', 'v2'` for `SET` of a configuration parameter.
pub(crate) fn config_assignment(setting: &ConfigSetting) -> CatalogResult<String> {
    if setting.values.is_empty() {
        return Err(invalid(format!(
            "setting {} needs at least one value",
            setting.name
        )));
    }
    let values = setting
        .values
        .iter()
        .map(|v| lit(v))
        .collect::<CatalogResult<Vec<_>>>()?
        .join(", ");
    Ok(format!("{} = {values}", config_name(&setting.name)?))
}

/// A configuration parameter name; custom parameters have a prefix
/// (`myapp.tenant`), quoted part by part.
pub(crate) fn config_name(name: &str) -> CatalogResult<String> {
    name.split('.')
        .map(ident)
        .collect::<CatalogResult<Vec<_>>>()
        .map(|parts| parts.join("."))
}

/// An operator name such as `&&` or `<->`, optionally schema-qualified as
/// `OPERATOR(schema.&&)`. Operator names are not identifiers, so they are
/// checked against the characters PostgreSQL allows instead of quoted.
pub(crate) fn operator(name: &str, schema: Option<&str>) -> CatalogResult<String> {
    const ALLOWED: &str = "+-*/<>=~!@#%^&|`?";
    if name.is_empty() || name.len() > 63 || !name.chars().all(|c| ALLOWED.contains(c)) {
        return Err(invalid(format!("{name:?} is not an operator name")));
    }
    if name.contains("--") || name.contains("/*") {
        return Err(invalid(format!(
            "{name:?} is not an operator name (it starts a comment)"
        )));
    }
    match schema {
        Some(schema) => Ok(format!("OPERATOR({}.{name})", ident(schema)?)),
        None => Ok(name.to_owned()),
    }
}

/// PostgreSQL's fully reserved key words, which cannot be bare names.
pub(crate) const RESERVED_WORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "intersect",
    "into",
    "lateral",
    "leading",
    "limit",
    "localtime",
    "localtimestamp",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "session_user",
    "some",
    "symmetric",
    "system_user",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

/// What a piece of SQL the user typed must look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// One value expression: no top-level `,`.
    Expression,
    /// One type name in PostgreSQL's type-name grammar.
    Type,
    /// Routine arguments separated by commas (`[mode] [name] type`), as
    /// signatures and operator operand lists are written.
    TypeList,
    /// A cast signature: `source AS target`.
    Cast,
    /// A routine argument or return type: a type name or `t.col%TYPE`.
    FuncType,
    /// An aggregate signature: arguments, for ordered-set aggregates
    /// `[direct arguments] ORDER BY aggregated arguments`.
    AggregateList,
    /// One `SELECT` / `VALUES` / `TABLE` / `WITH` query.
    Query,
    /// One data-changing statement (`SELECT`, `INSERT`, `UPDATE`,
    /// `DELETE`, `MERGE`, `NOTIFY`), for rule actions and `BEGIN ATOMIC`.
    Statement,
}

/// A checked fragment: comments removed, every quote, dollar quote and
/// bracket closed, no `;`, and the shape it was asked for. The lexer is
/// the driver's, which follows PostgreSQL's lexical rules, so the server
/// reads the fragment as the same tokens.
fn checked(text: &str, what: &str, shape: Shape) -> CatalogResult<String> {
    let cleaned = strip_comments(text, what)?;
    let tokens = lex_closed(&cleaned, what)?;
    if tokens.iter().any(|t| t.kind == LexKind::Comment) {
        return Err(invalid(format!("{what} cannot contain comments")));
    }
    let mut stack = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            LexKind::Punct(';') => {
                return Err(invalid(format!("{what} cannot contain `;`")));
            }
            LexKind::Punct(c @ ('(' | '[')) => stack.push(c),
            LexKind::Punct(c @ (')' | ']')) => {
                let open = if c == ')' { '(' } else { '[' };
                if stack.pop() != Some(open) {
                    return Err(invalid(format!("{what} has an unbalanced `{c}`")));
                }
            }
            LexKind::Punct(',') if stack.is_empty() && shape == Shape::Expression => {
                return Err(invalid(format!(
                    "{what} must be a single expression (no `,` outside parentheses)"
                )));
            }
            _ => {}
        }
        // `1e'..'` and `$1e'..'` are escape strings on PostgreSQL 13 and
        // 14 and a syntax error later; nobody writes them on purpose.
        if token.kind == LexKind::String && index > 0 {
            let touches = |a: &Lexeme, b: &Lexeme| a.end == b.start;
            let number = |t: &Lexeme| matches!(t.kind, LexKind::Number | LexKind::Parameter);
            let prev = &tokens[index - 1];
            let e_prefix = prev.kind == LexKind::Word
                && cleaned[prev.start..prev.end].eq_ignore_ascii_case("e")
                && touches(prev, token)
                && index > 1
                && number(&tokens[index - 2])
                && touches(&tokens[index - 2], prev);
            if e_prefix || (number(prev) && touches(prev, token)) {
                return Err(invalid(format!(
                    "{what} has a string literal directly after a number or parameter"
                )));
            }
        }
    }
    if let Some(open) = stack.last() {
        return Err(invalid(format!("{what} has an unclosed `{open}`")));
    }
    match shape {
        Shape::Type => {
            if !typename::is_type(&cleaned, &tokens) {
                return Err(type_error(what));
            }
        }
        Shape::FuncType => {
            if !typename::is_func_type(&cleaned, &tokens) {
                return Err(type_error(what));
            }
        }
        Shape::TypeList => {
            if !arguments(&cleaned, &tokens, false) {
                return Err(argument_error(what));
            }
        }
        Shape::AggregateList => {
            let order_by = tokens.windows(2).position(|w| {
                let word = |t: &Lexeme, s: &str| {
                    t.kind == LexKind::Word && cleaned[t.start..t.end].eq_ignore_ascii_case(s)
                };
                word(&w[0], "order") && word(&w[1], "by")
            });
            let ok = match order_by {
                Some(at) => {
                    arguments(&cleaned, &tokens[..at], true)
                        && arguments(&cleaned, &tokens[at + 2..], false)
                }
                None => arguments(&cleaned, &tokens, false),
            };
            if !ok {
                return Err(invalid(format!(
                    "{what} must be argument types separated by commas, for an ordered-set aggregate `[direct arguments] ORDER BY arguments`"
                )));
            }
        }
        Shape::Cast => {
            let parts = top_level_split(&tokens, |t| {
                t.kind == LexKind::Word && cleaned[t.start..t.end].eq_ignore_ascii_case("as")
            });
            let [source, target] = parts.as_slice() else {
                return Err(invalid(format!("{what} must be `source AS target`")));
            };
            if !typename::is_type(&cleaned, source) || !typename::is_type(&cleaned, target) {
                return Err(type_error(what));
            }
        }
        Shape::Query | Shape::Statement => {
            let spans = split_statements_strict(&cleaned, Dialect::Postgres)
                .map_err(|_| invalid(format!("{what} cannot be split unambiguously")))?;
            let [span] = spans.as_slice() else {
                return Err(invalid(format!("{what} must be exactly one statement")));
            };
            let allowed = match shape {
                Shape::Query => span.kind == StatementKind::Select,
                _ => {
                    matches!(
                        span.kind,
                        StatementKind::Select
                            | StatementKind::Insert
                            | StatementKind::Update
                            | StatementKind::UpdateAll
                            | StatementKind::Delete
                            | StatementKind::DeleteAll
                            | StatementKind::Merge
                    ) || (span.kind == StatementKind::Other
                        && tokens.first().is_some_and(|t| {
                            t.kind == LexKind::Word
                                && cleaned[t.start..t.end].eq_ignore_ascii_case("notify")
                        }))
                }
            };
            if !allowed {
                return Err(invalid(match shape {
                    Shape::Query => format!("{what} must be a SELECT, VALUES, TABLE or WITH query"),
                    _ => {
                        format!("{what} must be a SELECT, INSERT, UPDATE, DELETE, MERGE or NOTIFY")
                    }
                }));
            }
        }
        Shape::Expression => {}
    }
    Ok(cleaned)
}

/// Comma-separated `[mode] [name] type` items; `allow_empty` for the
/// direct arguments of an ordered-set aggregate (`ORDER BY anyelement`).
fn arguments(text: &str, tokens: &[Lexeme], allow_empty: bool) -> bool {
    if tokens.is_empty() {
        return allow_empty;
    }
    top_level_split(tokens, |t| t.kind == LexKind::Punct(','))
        .into_iter()
        .all(|item| typename::is_argument(text, item))
}

fn argument_error(what: &str) -> crate::model::CatalogError {
    invalid(format!(
        "{what} must be argument types separated by commas, each `[mode] [name] type`"
    ))
}

fn type_error(what: &str) -> crate::model::CatalogError {
    invalid(format!(
        "{what} must be one type name: a name (optionally schema-qualified or quoted) or a multi-word SQL type such as `double precision`, then optional `(modifiers)` and `[]`, with nothing after it"
    ))
}

/// Splits tokens at separators outside parentheses and brackets.
fn top_level_split(tokens: &[Lexeme], is_separator: impl Fn(&Lexeme) -> bool) -> Vec<&[Lexeme]> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, token) in tokens.iter().enumerate() {
        match token.kind {
            LexKind::Punct('(' | '[') => depth += 1,
            LexKind::Punct(')' | ']') => depth = depth.saturating_sub(1),
            _ if depth == 0 && is_separator(token) => {
                parts.push(&tokens[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&tokens[start..]);
    parts
}

/// Lexes `text` and proves every token is closed: a `;` after a newline
/// must come out as its own token at the very end, which it does not when
/// a quote, dollar quote or block comment is still open.
fn lex_closed(text: &str, what: &str) -> CatalogResult<Vec<Lexeme>> {
    let probe = format!("{text}\n;");
    let mut tokens = lex(&probe, Dialect::Postgres, true);
    match tokens.pop() {
        Some(last) if last.kind == LexKind::Punct(';') && last.start == text.len() + 1 => {
            Ok(tokens)
        }
        _ => Err(invalid(format!(
            "{what} has an unterminated quote, quoted identifier, dollar quote or comment"
        ))),
    }
}

/// The text with every comment replaced by a space, trimmed.
fn strip_comments(text: &str, what: &str) -> CatalogResult<String> {
    let trimmed = text.trim_matches(pg_space);
    if trimmed.contains('\0') {
        return Err(invalid(format!("{what} cannot contain a NUL character")));
    }
    // PostgreSQL 13 to 15 do not read a vertical tab as white space, later
    // versions do; refuse it so every version reads the same tokens.
    if trimmed.contains('\x0b') {
        return Err(invalid(format!("{what} cannot contain a vertical tab")));
    }
    let tokens = lex_closed(trimmed, what)?;
    let mut out = String::with_capacity(trimmed.len());
    let mut pos = 0;
    for token in tokens.iter().filter(|t| t.kind == LexKind::Comment) {
        out.push_str(&trimmed[pos..token.start]);
        out.push(' ');
        pos = token.end;
    }
    out.push_str(&trimmed[pos..]);
    let out = out.trim_matches(pg_space).to_owned();
    if out.is_empty() {
        return Err(invalid(format!("{what} cannot be empty")));
    }
    Ok(out)
}

/// Whitespace as every supported PostgreSQL version reads it (not Unicode
/// white space: U+2028 and friends are identifier characters there; the
/// vertical tab is refused before this is used).
fn pg_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

/// One expression (a default, check, predicate, `USING`, generation
/// expression, condition). Callers place it where nothing can follow it or
/// wrap it with [`paren_expr`].
pub(crate) fn expr(text: &str, what: &str) -> CatalogResult<String> {
    checked(text, what, Shape::Expression)
}

/// One expression in parentheses, for open-ended places such as
/// `DEFAULT` and `WHERE`: the closing parenthesis ends it for certain.
pub(crate) fn paren_expr(text: &str, what: &str) -> CatalogResult<String> {
    Ok(format!("({})", expr(text, what)?))
}

/// Like [`paren_expr`] for an optional value.
pub(crate) fn opt_paren_expr(text: Option<&str>, what: &str) -> CatalogResult<Option<String>> {
    text.map(|t| paren_expr(t, what)).transpose()
}

/// A type name such as `numeric(10,2)`, `character varying(20)[]` or
/// `"my schema"."my type"`.
pub(crate) fn data_type(text: &str, what: &str) -> CatalogResult<String> {
    checked(text, what, Shape::Type)
}

/// Routine arguments separated by commas (a routine signature, an
/// operator's operand types). Argument modes and names are words, so
/// `pg_get_function_identity_arguments` output passes.
pub(crate) fn type_list(text: &str, what: &str) -> CatalogResult<String> {
    checked(text, what, Shape::TypeList)
}

/// A routine argument or return type: a type name or `t.col%TYPE`.
pub(crate) fn func_type(text: &str, what: &str) -> CatalogResult<String> {
    checked(text, what, Shape::FuncType)
}

/// An aggregate's argument list, ordered-set aggregates included.
pub(crate) fn aggregate_signature(text: &str, what: &str) -> CatalogResult<String> {
    checked(text, what, Shape::AggregateList)
}

/// A cast signature, `source AS target`.
pub(crate) fn cast_signature(text: &str, what: &str) -> CatalogResult<String> {
    checked(text, what, Shape::Cast)
}

/// One query for a view.
pub(crate) fn query(text: &str, what: &str) -> CatalogResult<String> {
    checked(text, what, Shape::Query)
}

/// One data-changing statement (rule actions, `BEGIN ATOMIC` bodies).
pub(crate) fn statement(text: &str, what: &str) -> CatalogResult<String> {
    checked(text, what, Shape::Statement)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use varve_core::Engine;

    pub(crate) fn pg(major: u32) -> ServerInfo {
        ServerInfo {
            engine: Engine::Postgres,
            version: format!("{major}.0"),
            major,
            minor: 0,
            patch: None,
        }
    }

    #[test]
    fn expressions_follow_postgres_lexing() {
        // Plain cases.
        assert_eq!(expr("  now() ", "x").ok(), Some("now()".into()));
        assert_eq!(paren_expr("a > 0", "x").ok(), Some("(a > 0)".into()));
        assert!(expr("", "x").is_err());
        assert!(expr("/* only */ -- a comment", "x").is_err());
        assert!(expr("a\0", "x").is_err());
        assert!(expr("1; drop table t", "x").is_err());
        assert!(expr("$1 + 2; x", "x").is_err());
        // Unterminated tokens.
        for open in [
            "'a",
            "\"a",
            "/* a",
            "$$ a",
            "$q$ a $Q$",
            "E'\\'",
            "/* /* */",
        ] {
            assert!(expr(open, "x").is_err(), "{open}");
        }
        // Quotes and nested comments hide `;` and `,` from the checks, and
        // comments are removed.
        assert_eq!(
            expr("'a;b' || \"c;d\" || $q$ e; $q$ /* ; /* ; */ */", "x").ok(),
            Some("'a;b' || \"c;d\" || $q$ e; $q$".into())
        );
        assert!(expr("E'it\\'s; ,'", "x").is_ok());
        assert!(expr("'it''s;'", "x").is_ok());
        assert!(expr("a$b$c", "x").is_ok());
        assert_eq!(expr("1 -- one", "x").ok(), Some("1".into()));
        assert!(expr("1 -- one\n; x", "x").is_err());
        // Top-level commas and unbalanced brackets.
        assert!(expr("0, DROP COLUMN b", "x").is_err());
        assert!(expr("x), DROP COLUMN b", "x").is_err());
        assert!(expr("(x", "x").is_err());
        assert!(expr("a[1)", "x").is_err());
        assert!(expr("coalesce(a, b) + arr[1:2]", "x").is_ok());
    }

    /// Every injection payload of the audit, as typed into an expression,
    /// predicate or query field.
    #[test]
    fn audit_payloads_are_refused() {
        let payloads = [
            // `\r` ends a line comment in PostgreSQL.
            "'x' --\r; DROP TABLE m2_audit_pgedit.victim",
            "b > 0) --\r; DROP TABLE m2_audit_pgedit.victim; SELECT (1",
            // Non-ASCII dollar tag.
            "a <> $\u{20ac}$'$\u{20ac}$; DROP TABLE m2_audit_pgedit.victim; --'",
            "SELECT $\u{20ac}$'$\u{20ac}$ AS a; DROP TABLE m2_audit_pgedit.victim; --'",
            // Non-ASCII identifier characters: `\u{20ac}$x$` is one word.
            "SELECT 1 AS \u{20ac}$x$; DROP TABLE m2_audit_pgedit.victim; --$x$",
            // `\u{20ac}e'..'` is not an E-string.
            "m2_audit_pgedit.\u{20ac}e'\\'; DROP TABLE m2_audit_pgedit.victim; --'",
            // A real E-string with an escaped quote, then a statement.
            "E'\\''; DROP TABLE m2_audit_pgedit.victim; --'",
            // Unbalanced nested comment.
            "'x' /* /* */ ; drop table x",
            // Destructive bypass through a comma.
            "0, DROP COLUMN b",
        ];
        for payload in payloads {
            assert!(expr(payload, "x").is_err(), "expression {payload:?}");
            assert!(query(payload, "x").is_err(), "query {payload:?}");
            assert!(statement(payload, "x").is_err(), "statement {payload:?}");
        }
        // The harmless audit case still works.
        assert_eq!(expr("'x' /* a /* ; */ ; */", "x").ok(), Some("'x'".into()));
        assert_eq!(
            expr("E'\\'; DROP TABLE m2_audit_pgedit.victim; --'", "x").ok(),
            Some("E'\\'; DROP TABLE m2_audit_pgedit.victim; --'".into()),
            "one E-string literal"
        );
        // A non-ASCII word next to a dollar quote is lexed as one word, as
        // PostgreSQL does, so no quote opens and the `;` is seen.
        assert!(expr("\u{20ac}$x$ ; $x$", "x").is_err());
        assert!(expr("\u{2028}", "x").is_ok());
        // U+2028 does not end a line comment, so the rest is comment text
        // and is removed.
        assert_eq!(
            expr("1 --\u{2028}; DROP TABLE x", "x").ok(),
            Some("1".into())
        );
    }

    #[test]
    fn type_names() {
        for ok in [
            "int",
            "numeric(10,2)",
            "numeric(10, -2)",
            "character varying(20)[]",
            "int[3][]",
            "timestamp(3) with time zone",
            "\"my schema\".\"my type\"",
            "public.citext",
        ] {
            assert!(data_type(ok, "t").is_ok(), "{ok}");
        }
        for bad in [
            "int; drop table x",
            "int, text",
            "int DEFAULT 1",
            "int)",
            "int CHECK (false)",
            "int --\r, x int",
            "(int)",
            "int[",
        ] {
            assert!(data_type(bad, "t").is_err(), "{bad}");
        }
        assert_eq!(
            type_list("a integer, VARIADIC b text[], numeric(3,1)", "t").ok(),
            Some("a integer, VARIADIC b text[], numeric(3,1)".into())
        );
        assert!(type_list("int,", "t").is_err());
        assert!(type_list("int, 1", "t").is_err());
        assert!(type_list("int) ; DROP TABLE x; --", "t").is_err());
    }

    /// Every multi-word SQL type name and modifier form the grammar takes.
    pub(crate) const ACCEPTED_TYPES: &[&str] = &[
        "integer",
        "pg_catalog.int8",
        "\"char\"",
        "\"my schema\".\"my type\"",
        "varve.public.citext",
        "double precision",
        "character varying(20)",
        "char varying(5)",
        "character(3)",
        "national character(3)",
        "national char varying(4)",
        "national character varying",
        "nchar varying(6)",
        "nchar(2)",
        "bit varying(8)",
        "bit(3)",
        "float(10)",
        "numeric(10, 2)",
        "numeric(10, -2)",
        "time",
        "time(3) with time zone",
        "time without time zone",
        "timestamp(2) without time zone",
        "timestamp with time zone",
        "interval",
        "interval(4)",
        "interval year",
        "interval month",
        "interval day",
        "interval hour",
        "interval minute",
        "interval second(2)",
        "interval year to month",
        "interval day to hour",
        "interval day to minute",
        "interval day to second(3)",
        "interval hour to minute",
        "interval hour to second",
        "interval minute to second",
        "numeric(10,2)[]",
        "int[3][]",
        "integer ARRAY",
        "integer ARRAY[3]",
        "text[]",
    ];

    #[test]
    fn type_name_grammar() {
        for ok in ACCEPTED_TYPES {
            assert!(data_type(ok, "t").is_ok(), "{ok}");
        }
        for bad in [
            // Trailing clauses the old word check let through.
            "int SECURITY DEFINER",
            "int LEAKPROOF",
            "int PRIMARY KEY",
            "int REFERENCES t ON DELETE CASCADE",
            "int GENERATED ALWAYS AS IDENTITY",
            "int USING x",
            "int NOT NULL",
            "text COLLATE \"C\"",
            "int int",
            "double precision precision",
            // Only return types may be set-returning, through their own field.
            "SETOF int",
            "TABLE (a int)",
            // Malformed modifiers, multi-word names and dimensions.
            "numeric()",
            "varchar(10",
            "int[x]",
            "int[3",
            "int ARRAY[]",
            "a.b.c.d",
            "a.",
            "timestamp with zone",
            "time with time zone(3)",
            "interval year to second",
            "interval month(3)",
            "interval to second",
            "national varying",
            "national",
        ] {
            assert!(data_type(bad, "t").is_err(), "{bad}");
        }
        // Modifiers are constants or names, one per item, as PostgreSQL's
        // grammar passes them to the type's modifier function.
        for ok in [
            "geometry(Point,4326)",
            "public.geometry(MultiPolygon, 4326)",
            "x('a')",
            "x(\"Q\", -1.5)",
            "numeric(1.5)",
            "varchar('x')",
        ] {
            assert!(data_type(ok, "t").is_ok(), "{ok}");
        }
        for bad in [
            "numeric(1+2)",
            "numeric((1))",
            "x(f(1))",
            "x(a b)",
            "x(select)",
            "x(default)",
            "x(-a)",
            "x(1,)",
            "x(,1)",
            "x($1)",
            "x(1) y",
        ] {
            assert!(data_type(bad, "t").is_err(), "{bad}");
        }
        // `%TYPE` only where routines take types.
        for ok in ["t.col%TYPE", "s.t.col%TYPE", "\"S\".\"T\".\"c\"%type"] {
            assert!(func_type(ok, "f").is_ok(), "{ok}");
            assert!(data_type(ok, "t").is_err(), "{ok}");
        }
        for bad in [
            "col%TYPE",
            "t.col%TYPE[]",
            "t.col%ROWTYPE",
            "t.col % TYPE x",
            "a.b.c.d.e%TYPE",
        ] {
            assert!(func_type(bad, "f").is_err(), "{bad}");
        }
        assert!(func_type("double precision", "f").is_ok());
        assert!(type_list("a s.t.col%TYPE, OUT b integer", "l").is_ok());
        // Ordered-set and hypothetical-set aggregate signatures.
        for ok in [
            "double precision ORDER BY double precision",
            "ORDER BY anyelement",
            "integer, text ORDER BY double precision, numeric(3,1)",
            "VARIADIC \"any\" ORDER BY VARIADIC \"any\"",
            "integer",
        ] {
            assert!(aggregate_signature(ok, "a").is_ok(), "{ok}");
        }
        for bad in [
            "ORDER BY",
            "integer ORDER BY",
            "integer ORDER BY int ORDER BY int",
            "int ORDER int",
        ] {
            assert!(aggregate_signature(bad, "a").is_err(), "{bad}");
        }
        assert!(type_list("double precision ORDER BY double precision", "l").is_err());
        assert!(cast_signature("text AS public.citext", "c").is_ok());
        assert!(cast_signature("double precision AS character varying(3)", "c").is_ok());
        assert!(cast_signature("text", "c").is_err());
        assert!(cast_signature("text AS int AS bigint", "c").is_err());
        assert!(cast_signature("text AS int WITH FUNCTION f(text)", "c").is_err());
        assert!(type_list("OUT x double precision, interval day to second", "l").is_ok());
        assert!(type_list("integer SECURITY DEFINER", "l").is_err());
        assert!(type_list("a int PRIMARY KEY", "l").is_err());
    }

    #[test]
    fn lexer_regressions_are_refused_or_contained() {
        // Continuation of an escape string keeps escape mode: one literal.
        let one = expr("E'a'\n'b\\'; c'", "x");
        assert_eq!(one.ok(), Some("E'a'\n'b\\'; c'".into()));
        // `--` comments in the gap: stripped, the newline (and so the
        // continuation) stays, still one literal.
        let commented = expr("E'a' -- note\n  -- more\n'b\\'; c'", "x");
        assert!(commented.is_ok(), "{commented:?}");
        // A standard string continued keeps standard rules: the `;` is real.
        assert!(expr("'a'\n'b\\'; SELECT 2", "x").is_err());
        // No newline, no continuation.
        assert!(expr("'a' 'b'; SELECT 2", "x").is_err());
        // `1e'..'` / `$1e'..'`: refused on every version.
        for prefix in ["1e'\\'; x'", "$1e'\\'; x'", "1E'x'", "1'x'"] {
            assert!(expr(prefix, "x").is_err(), "{prefix}");
        }
        // `abc1e` is a name, so the string after it is standard.
        assert!(expr("abc1e'\\'; SELECT 2", "x").is_err());
        // Vertical tab: refused (PostgreSQL 13 to 15 do not read it as space).
        assert!(expr("1\x0b+ 1", "x").is_err());
        assert!(data_type("int\x0b", "x").is_err());
    }

    /// The same forms through [`crate::postgres::edit::plan`]: whatever is
    /// accepted ends up as exactly one statement.
    #[test]
    fn lexer_regressions_through_plan() -> CatalogResult<()> {
        use crate::postgres::edit::{PgChange, TableChange, TableOperation, plan};
        let set_default = |expression: &str| {
            plan(
                &pg(13),
                &PgChange::Table(TableChange::Alter {
                    name: QualifiedName::new("s", "t"),
                    operations: vec![TableOperation::SetDefault {
                        column: "a".into(),
                        expression: expression.into(),
                    }],
                }),
            )
        };
        for contained in ["E'a'\n'b\\'; c'", "E'a' -- note\n'b\\'; c'"] {
            let p = set_default(contained)?;
            for statement in &p.statements {
                assert_eq!(
                    varve_driver::split_statements(statement, Dialect::Postgres).len(),
                    1,
                    "{statement}"
                );
            }
        }
        // A block comment ends a continuation in PostgreSQL; with comments
        // stripped the parts would join, so the result is refused as an
        // unterminated literal rather than read two ways.
        for refused in [
            "'a'\n'b\\'; SELECT 2",
            "E'a'\n/* x */'b\\'; SELECT 2",
            "1e'\\'; x'",
            "$1e'\\'; x'",
            "0\x0b, DROP COLUMN b",
        ] {
            assert!(set_default(refused).is_err(), "{refused:?}");
        }
        Ok(())
    }

    #[test]
    fn queries_and_statements() {
        assert!(query("SELECT 1", "q").is_ok());
        assert!(query("WITH a AS (SELECT 1) SELECT * FROM a", "q").is_ok());
        assert!(query("VALUES (1), (2)", "q").is_ok());
        assert!(query("SELECT 1;", "q").is_err());
        for bad in [
            "COMMIT",
            "DELETE FROM t",
            "SET search_path = x",
            "VACUUM t",
            "DROP TABLE t",
        ] {
            assert!(query(bad, "q").is_err(), "{bad}");
        }
        for ok in [
            "INSERT INTO log VALUES (OLD.a)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "NOTIFY chan",
            "SELECT 1",
        ] {
            assert!(statement(ok, "s").is_ok(), "{ok}");
        }
        for bad in [
            "COMMIT",
            "BEGIN",
            "DROP TABLE t",
            "TRUNCATE t",
            "SET a = 1",
            "VACUUM",
            "INSERT INTO t VALUES (1); DROP TABLE t",
        ] {
            assert!(statement(bad, "s").is_err(), "{bad}");
        }
    }

    #[test]
    fn storage_params_and_settings() -> CatalogResult<()> {
        assert_eq!(
            storage_params(&[
                StorageParam::new("fillfactor", "70"),
                StorageParam::new("toast.autovacuum_enabled", "off"),
            ])?,
            r#""fillfactor" = 70, "toast"."autovacuum_enabled" = 'off'"#
        );
        assert!(storage_params(&[]).is_err());
        assert_eq!(
            config_assignment(&ConfigSetting {
                name: "search_path".into(),
                values: vec!["app".into(), "public".into()],
            })?,
            r#""search_path" = 'app', 'public'"#
        );
        assert_eq!(config_name("myapp.tenant")?, r#""myapp"."tenant""#);
        Ok(())
    }

    #[test]
    fn operators_and_roles() -> CatalogResult<()> {
        assert_eq!(operator("&&", None)?, "&&");
        assert_eq!(
            operator("=", Some("pg_catalog"))?,
            r#"OPERATOR("pg_catalog".=)"#
        );
        assert!(operator("=; drop", None).is_err());
        assert!(operator("--", None).is_err());
        assert_eq!(RoleSpec::name("bob").sql(&pg(13))?, r#""bob""#);
        assert!(RoleSpec::CurrentRole.sql(&pg(13)).is_err());
        assert_eq!(RoleSpec::CurrentRole.sql(&pg(14))?, "CURRENT_ROLE");
        assert!(RoleSpec::Public.owner_sql(&pg(18)).is_err());
        Ok(())
    }
}
