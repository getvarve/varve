//! Casts (`pg_cast`) and operators (`pg_operator`).

use serde::{Deserialize, Serialize};
use varve_driver::Session;

use super::Scope;
use super::common::{
    PgMeta, bad_code, comment_on, extension_sql, fetch_one, flag, ident, lit, meta, owner_to,
    schema_of, script, summary, system_schema_sql, with_system,
};
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef, ObjectSummary};
use crate::rows::Row;

/// When a cast applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgCastContext {
    /// Only with an explicit `CAST` or `::`.
    Explicit,
    /// Also in assignments (`AS ASSIGNMENT`).
    Assignment,
    /// Also implicitly in expressions (`AS IMPLICIT`).
    Implicit,
}

/// How a cast converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgCastMethod {
    /// `WITH FUNCTION`.
    Function,
    /// `WITH INOUT`.
    InOut,
    /// `WITHOUT FUNCTION` (binary coercible).
    Binary,
}

/// A cast. Casts the server creates with another object (range to
/// multirange casts on 14+) are system objects. Its [`ObjectRef::name`] and [`ObjectRef::signature`] are both
/// `source AS target`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgCast {
    /// Source type as SQL.
    pub source: String,
    /// Target type as SQL.
    pub target: String,
    /// Function with argument types as SQL (`regprocedure`).
    pub function: Option<String>,
    /// Context.
    pub context: PgCastContext,
    /// Method.
    pub method: PgCastMethod,
    /// Comment and system status (casts have no owner).
    pub meta: PgMeta,
}

/// An operator. Function fields are SQL; operator references are
/// `schema.operator`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgOperator {
    /// Schema.
    pub schema: String,
    /// Operator symbol.
    pub name: String,
    /// Left operand type; `None` for a prefix operator.
    pub left_type: Option<String>,
    /// Right operand type; `None` for a postfix operator (13 only).
    pub right_type: Option<String>,
    /// Result type.
    pub result_type: String,
    /// Implementing function.
    pub function: String,
    /// Commutator.
    pub commutator: Option<String>,
    /// Negator.
    pub negator: Option<String>,
    /// Restriction selectivity estimator.
    pub restrict: Option<String>,
    /// Join selectivity estimator.
    pub join: Option<String>,
    /// `HASHES`.
    pub hashes: bool,
    /// `MERGES`.
    pub merges: bool,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

const CAST_NAME: &str = "pg_catalog.format_type(c.castsource, NULL) OPERATOR(pg_catalog.||) ' AS ' OPERATOR(pg_catalog.||) pg_catalog.format_type(c.casttarget, NULL)";

fn cast_base(filter: &str, extra: &str) -> String {
    format!(
        "SELECT c.oid, pg_catalog.current_database() AS db, {CAST_NAME} AS name, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) c.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_cast'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {ext} AS extension, \
         EXISTS (SELECT 1 FROM pg_catalog.pg_depend idep WHERE idep.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_cast'::pg_catalog.regclass \
         AND idep.objid OPERATOR(pg_catalog.=) c.oid AND idep.deptype OPERATOR(pg_catalog.=) 'i') AS system_schema, \
         pg_catalog.format_type(c.castsource, NULL) AS source, \
         pg_catalog.format_type(c.casttarget, NULL) AS target, c.castcontext::pg_catalog.text AS context, \
         c.castmethod::pg_catalog.text AS method, \
         (CASE WHEN c.castfunc OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN c.castfunc END)::pg_catalog.regprocedure::pg_catalog.text AS function{extra} \
         FROM pg_catalog.pg_cast c WHERE true{filter}",
        ext = extension_sql("pg_catalog.pg_cast", "c.oid"),
    )
}

/// SQL for `schema.operator` of the operator `oid_expr`.
fn operator_ref(oid_expr: &str) -> String {
    format!(
        "(SELECT pg_catalog.quote_ident(rn.nspname) OPERATOR(pg_catalog.||) '.' OPERATOR(pg_catalog.||) r.oprname FROM pg_catalog.pg_operator r \
         JOIN pg_catalog.pg_namespace rn ON rn.oid OPERATOR(pg_catalog.=) r.oprnamespace WHERE r.oid OPERATOR(pg_catalog.=) {oid_expr})"
    )
}

const OPERATOR_SIGNATURE: &str = "COALESCE(pg_catalog.format_type((CASE WHEN o.oprleft OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN o.oprleft END), NULL), 'NONE') \
     OPERATOR(pg_catalog.||) ', ' OPERATOR(pg_catalog.||) COALESCE(pg_catalog.format_type((CASE WHEN o.oprright OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN o.oprright END), NULL), 'NONE')";

fn operator_base(filter: &str, extra: &str) -> String {
    format!(
        "SELECT o.oid, pg_catalog.current_database() AS db, n.nspname AS schema, o.oprname AS name, \
         {OPERATOR_SIGNATURE} AS signature, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) o.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_operator'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {ext} AS extension, \
         {sys} AS system_schema, pg_catalog.format_type(o.oprresult, NULL) AS result_type{extra} \
         FROM pg_catalog.pg_operator o JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) o.oprnamespace WHERE true{filter}",
        ext = extension_sql("pg_catalog.pg_operator", "o.oid"),
        sys = system_schema_sql("n.nspname"),
    )
}

/// Inner list query (see [`super::common::with_system`]).
pub(super) fn inner(kind: ObjectKind, scope: &Scope) -> String {
    if kind == ObjectKind::Operator {
        operator_base(&scope.schema_filter("n.nspname"), "")
    } else {
        cast_base("", "")
    }
}

/// Order of the list.
pub(super) fn order(kind: ObjectKind) -> &'static str {
    if kind == ObjectKind::Operator {
        "q.name, q.signature"
    } else {
        "q.name"
    }
}

fn parse_context(code: &str) -> CatalogResult<PgCastContext> {
    match code {
        "e" => Ok(PgCastContext::Explicit),
        "a" => Ok(PgCastContext::Assignment),
        "i" => Ok(PgCastContext::Implicit),
        other => Err(bad_code("castcontext", other)),
    }
}

fn parse_method(code: &str) -> CatalogResult<PgCastMethod> {
    match code {
        "f" => Ok(PgCastMethod::Function),
        "i" => Ok(PgCastMethod::InOut),
        "b" => Ok(PgCastMethod::Binary),
        other => Err(bad_code("castmethod", other)),
    }
}

/// A tree entry from a row of [`inner`].
pub(super) fn summarize(kind: ObjectKind, row: &Row<'_>) -> CatalogResult<ObjectSummary> {
    if kind == ObjectKind::Operator {
        let detail = Some(format!("returns {}", row.req_text("result_type")?));
        return summary(
            kind,
            row,
            true,
            Some(row.req_text("signature")?),
            detail,
            Vec::new(),
        );
    }
    let flags = match parse_context(&row.req_text("context")?)? {
        PgCastContext::Explicit => Vec::new(),
        PgCastContext::Assignment => vec!["assignment".to_owned()],
        PgCastContext::Implicit => vec!["implicit".to_owned()],
    };
    let detail = match parse_method(&row.req_text("method")?)? {
        PgCastMethod::Function => row.text("function")?,
        PgCastMethod::InOut => Some("with inout".to_owned()),
        PgCastMethod::Binary => Some("binary coercible".to_owned()),
    };
    summary(kind, row, false, Some(row.req_text("name")?), detail, flags)
}

/// Details of a cast, found by its `source AS target` signature (or name).
pub(super) async fn describe_cast(
    session: &dyn Session,
    object: &ObjectRef,
) -> CatalogResult<PgCast> {
    let signature = object.signature.as_deref().unwrap_or(&object.name);
    let main = cast_base(
        &format!(
            " AND {CAST_NAME} OPERATOR(pg_catalog.=) {}",
            lit(signature)?
        ),
        "",
    );
    let rows = fetch_one(session, &with_system(&main, true), "q.name").await?;
    if rows.is_empty() {
        return Err(CatalogError::NotFound {
            message: format!("cast ({signature}) was not found"),
        });
    }
    let row = rows.single("the cast")?;
    Ok(PgCast {
        source: row.req_text("source")?,
        target: row.req_text("target")?,
        function: row.text("function")?,
        context: parse_context(&row.req_text("context")?)?,
        method: parse_method(&row.req_text("method")?)?,
        meta: meta_without_owner(&row)?,
    })
}

fn meta_without_owner(row: &Row<'_>) -> CatalogResult<PgMeta> {
    Ok(PgMeta {
        owner: None,
        comment: row.text("comment")?,
        extension: row.text("extension")?,
        system: flag(row, "system")?,
    })
}

/// `CREATE CAST` with its comment.
pub(super) fn cast_ddl(cast: &PgCast) -> CatalogResult<String> {
    let types = format!("({} AS {})", cast.source, cast.target);
    let mut create = format!("CREATE CAST {types} ");
    match (cast.method, &cast.function) {
        (PgCastMethod::Function, Some(function)) => {
            create.push_str(&format!("WITH FUNCTION {function}"));
        }
        (PgCastMethod::Function, None) => {
            return Err(CatalogError::Unexpected {
                message: "a function cast has no function".into(),
            });
        }
        (PgCastMethod::InOut, _) => create.push_str("WITH INOUT"),
        (PgCastMethod::Binary, _) => create.push_str("WITHOUT FUNCTION"),
    }
    match cast.context {
        PgCastContext::Explicit => {}
        PgCastContext::Assignment => create.push_str(" AS ASSIGNMENT"),
        PgCastContext::Implicit => create.push_str(" AS IMPLICIT"),
    }
    let mut statements = vec![create];
    statements.extend(comment_on(
        &format!("CAST {types}"),
        cast.meta.comment.as_deref(),
    )?);
    Ok(script(statements))
}

/// Details of an operator, found by its `left, right` signature.
pub(super) async fn describe_operator(
    session: &dyn Session,
    object: &ObjectRef,
) -> CatalogResult<PgOperator> {
    let mut filter = format!(
        " AND n.nspname OPERATOR(pg_catalog.=) {} AND o.oprname OPERATOR(pg_catalog.=) {}",
        lit(schema_of(object)?)?,
        lit(&object.name)?
    );
    if let Some(signature) = &object.signature {
        filter.push_str(&format!(
            " AND {OPERATOR_SIGNATURE} OPERATOR(pg_catalog.=) {}",
            lit(signature)?
        ));
    }
    let main = operator_base(
        &filter,
        &format!(
            ", pg_catalog.pg_get_userbyid(o.oprowner) AS owner, \
             pg_catalog.format_type((CASE WHEN o.oprleft OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN o.oprleft END), NULL) AS left_type, \
             pg_catalog.format_type((CASE WHEN o.oprright OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN o.oprright END), NULL) AS right_type, \
             o.oprcode::pg_catalog.text AS function, {com} AS commutator, {neg} AS negator, \
             (CASE WHEN o.oprrest::pg_catalog.oid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN o.oprrest::pg_catalog.oid END)::pg_catalog.regproc::pg_catalog.text AS restrict_fn, \
             (CASE WHEN o.oprjoin::pg_catalog.oid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN o.oprjoin::pg_catalog.oid END)::pg_catalog.regproc::pg_catalog.text AS join_fn, \
             o.oprcanhash AS hashes, o.oprcanmerge AS merges",
            com = operator_ref("o.oprcom"),
            neg = operator_ref("o.oprnegate"),
        ),
    );
    let rows = fetch_one(session, &with_system(&main, true), "q.signature").await?;
    if rows.is_empty() {
        return Err(CatalogError::NotFound {
            message: format!("operator {} was not found", object.name),
        });
    }
    let row = rows.single("the operator")?;
    Ok(PgOperator {
        schema: row.req_text("schema")?,
        name: row.req_text("name")?,
        left_type: row.text("left_type")?,
        right_type: row.text("right_type")?,
        result_type: row.req_text("result_type")?,
        function: row.req_text("function")?,
        commutator: row.text("commutator")?,
        negator: row.text("negator")?,
        restrict: row.text("restrict_fn")?,
        join: row.text("join_fn")?,
        hashes: flag(&row, "hashes")?,
        merges: flag(&row, "merges")?,
        meta: meta(&row)?,
    })
}

/// True for a valid operator symbol, which cannot be quoted in SQL.
fn is_operator_symbol(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| "+-*/<>=~!@#%^&|`?".contains(c))
}

/// `CREATE OPERATOR` with owner and comment.
pub(super) fn operator_ddl(operator: &PgOperator) -> CatalogResult<String> {
    if !is_operator_symbol(&operator.name) {
        return Err(CatalogError::Invalid {
            message: format!("{:?} is not an operator symbol", operator.name),
        });
    }
    let name = format!("{}.{}", ident(&operator.schema)?, operator.name);
    let mut options = vec![format!("FUNCTION = {}", operator.function)];
    if let Some(left) = &operator.left_type {
        options.push(format!("LEFTARG = {left}"));
    }
    if let Some(right) = &operator.right_type {
        options.push(format!("RIGHTARG = {right}"));
    }
    if let Some(commutator) = &operator.commutator {
        options.push(format!("COMMUTATOR = OPERATOR({commutator})"));
    }
    if let Some(negator) = &operator.negator {
        options.push(format!("NEGATOR = OPERATOR({negator})"));
    }
    if let Some(restrict) = &operator.restrict {
        options.push(format!("RESTRICT = {restrict}"));
    }
    if let Some(join) = &operator.join {
        options.push(format!("JOIN = {join}"));
    }
    if operator.hashes {
        options.push("HASHES".to_owned());
    }
    if operator.merges {
        options.push("MERGES".to_owned());
    }
    let target = format!(
        "OPERATOR {name} ({}, {})",
        operator.left_type.as_deref().unwrap_or("NONE"),
        operator.right_type.as_deref().unwrap_or("NONE")
    );
    let mut statements = vec![format!(
        "CREATE OPERATOR {name} (\n    {}\n)",
        options.join(",\n    ")
    )];
    statements.extend(owner_to(&target, operator.meta.owner.as_deref())?);
    statements.extend(comment_on(&target, operator.meta.comment.as_deref())?);
    Ok(script(statements))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_operators_have_no_left_argument() -> CatalogResult<()> {
        let operator = PgOperator {
            schema: "s".into(),
            name: "~~~".into(),
            left_type: None,
            right_type: Some("integer".into()),
            result_type: "integer".into(),
            function: "s.neg".into(),
            commutator: None,
            negator: None,
            restrict: None,
            join: None,
            hashes: false,
            merges: false,
            meta: PgMeta {
                owner: Some("o".into()),
                ..PgMeta::default()
            },
        };
        assert_eq!(
            operator_ddl(&operator)?,
            "CREATE OPERATOR \"s\".~~~ (\n    FUNCTION = s.neg,\n    RIGHTARG = integer\n);\n\n\
             ALTER OPERATOR \"s\".~~~ (NONE, integer) OWNER TO \"o\";\n"
        );
        let mut bad = operator;
        bad.name = "a;b".into();
        assert!(operator_ddl(&bad).is_err());
        Ok(())
    }

    #[test]
    fn casts_name_their_method_and_context() -> CatalogResult<()> {
        let cast = PgCast {
            source: "s.t".into(),
            target: "text".into(),
            function: None,
            context: PgCastContext::Assignment,
            method: PgCastMethod::InOut,
            meta: PgMeta {
                comment: Some("c".into()),
                ..PgMeta::default()
            },
        };
        assert_eq!(
            cast_ddl(&cast)?,
            "CREATE CAST (s.t AS text) WITH INOUT AS ASSIGNMENT;\n\n\
             COMMENT ON CAST (s.t AS text) IS 'c';\n"
        );
        Ok(())
    }
}
