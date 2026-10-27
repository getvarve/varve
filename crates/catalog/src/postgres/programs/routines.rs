//! Functions, procedures and aggregates (`pg_proc`, `pg_aggregate`).

use serde::{Deserialize, Serialize};
use varve_driver::{ServerInfo, Session};

use super::Scope;
use super::common::{
    PgMeta, bad_code, comment_on, extension_sql, fetch_pinned, flag, float, int, lit, meta,
    owner_to, qname, schema_of, script, summary, system_schema_sql, texts, with_system,
};
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef, ObjectSummary};
use crate::rows::Row;

/// How an argument is passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgArgumentMode {
    /// `IN`.
    In,
    /// `OUT`.
    Out,
    /// `INOUT`.
    InOut,
    /// `VARIADIC`.
    Variadic,
    /// A column of `RETURNS TABLE`.
    Table,
}

/// One argument of a function, procedure or aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgArgument {
    /// Argument name, when it has one.
    pub name: Option<String>,
    /// Mode.
    pub mode: PgArgumentMode,
    /// Type as SQL (`format_type`).
    pub data_type: String,
    /// Default expression as SQL.
    pub default: Option<String>,
}

/// Volatility class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgVolatility {
    /// `IMMUTABLE`.
    Immutable,
    /// `STABLE`.
    Stable,
    /// `VOLATILE`.
    Volatile,
}

/// Parallel safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgParallel {
    /// `PARALLEL SAFE`.
    Safe,
    /// `PARALLEL RESTRICTED`.
    Restricted,
    /// `PARALLEL UNSAFE`.
    Unsafe,
}

impl PgParallel {
    fn parse(code: &str) -> CatalogResult<Self> {
        match code {
            "s" => Ok(Self::Safe),
            "r" => Ok(Self::Restricted),
            "u" => Ok(Self::Unsafe),
            other => Err(bad_code("proparallel", other)),
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            Self::Safe => "SAFE",
            Self::Restricted => "RESTRICTED",
            Self::Unsafe => "UNSAFE",
        }
    }
}

/// A function or procedure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgFunction {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Identity arguments (`pg_get_function_identity_arguments`), the
    /// [`ObjectRef::signature`] of the routine.
    pub signature: String,
    /// All arguments in order, including `OUT` and `TABLE` columns.
    pub arguments: Vec<PgArgument>,
    /// Result as SQL (`integer`, `SETOF text`, `TABLE(a integer)`);
    /// `None` for procedures.
    pub result: Option<String>,
    /// The `pg_proc.prorettype` type as SQL; `None` for procedures.
    pub return_type: Option<String>,
    /// True for set-returning functions.
    pub returns_set: bool,
    /// True for window functions.
    pub window: bool,
    /// Language.
    pub language: String,
    /// Volatility.
    pub volatility: PgVolatility,
    /// `STRICT` (returns null on null input).
    pub strict: bool,
    /// `SECURITY DEFINER`.
    pub security_definer: bool,
    /// `LEAKPROOF`.
    pub leakproof: bool,
    /// Parallel safety.
    pub parallel: PgParallel,
    /// Estimated execution cost.
    pub cost: f64,
    /// Estimated rows of a set-returning function.
    pub rows: Option<f64>,
    /// `SET` clauses, as `name=value`.
    pub config: Vec<String>,
    /// Source text (body, or the C symbol); `None` for a SQL-standard body.
    pub source: Option<String>,
    /// Library file of a C function.
    pub library: Option<String>,
    /// True for a SQL-standard body (`BEGIN ATOMIC` or `RETURN`, 14+).
    pub sql_standard_body: bool,
    /// Full `CREATE OR REPLACE` statement (`pg_get_functiondef`).
    pub definition: String,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

/// What an aggregate computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgAggregateKind {
    /// A normal aggregate.
    Normal,
    /// An ordered-set aggregate (`WITHIN GROUP`).
    OrderedSet,
    /// A hypothetical-set aggregate.
    Hypothetical,
}

/// Whether a final function modifies the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgFinalModify {
    /// `READ_ONLY`.
    ReadOnly,
    /// `SHAREABLE`.
    Shareable,
    /// `READ_WRITE`.
    ReadWrite,
}

impl PgFinalModify {
    fn parse(code: &str) -> CatalogResult<Self> {
        match code {
            "r" => Ok(Self::ReadOnly),
            "s" => Ok(Self::Shareable),
            "w" => Ok(Self::ReadWrite),
            other => Err(bad_code("aggfinalmodify", other)),
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            Self::ReadOnly => "READ_ONLY",
            Self::Shareable => "SHAREABLE",
            Self::ReadWrite => "READ_WRITE",
        }
    }
}

/// An aggregate function. Function and type fields are SQL as the server
/// prints them (schema-qualified outside `pg_catalog`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgAggregate {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Identity arguments, the [`ObjectRef::signature`]; empty for `(*)`.
    pub signature: String,
    /// Arguments as SQL (`pg_get_function_arguments`, with `ORDER BY` for
    /// ordered-set aggregates); empty for `(*)`.
    pub arguments: String,
    /// Number of arguments, direct ones included.
    pub argument_count: i64,
    /// Number of direct arguments of an ordered-set aggregate.
    pub direct_argument_count: i64,
    /// Kind.
    pub aggregate_kind: PgAggregateKind,
    /// Result type as SQL.
    pub result: String,
    /// State transition function (`SFUNC`).
    pub transition_function: String,
    /// State type (`STYPE`).
    pub state_type: String,
    /// Average state size (`SSPACE`); 0 for the default.
    pub state_space: i64,
    /// Final function (`FINALFUNC`).
    pub final_function: Option<String>,
    /// `FINALFUNC_EXTRA`.
    pub final_extra: bool,
    /// `FINALFUNC_MODIFY`.
    pub final_modify: PgFinalModify,
    /// Combine function (`COMBINEFUNC`).
    pub combine_function: Option<String>,
    /// Serialization function (`SERIALFUNC`).
    pub serial_function: Option<String>,
    /// Deserialization function (`DESERIALFUNC`).
    pub deserial_function: Option<String>,
    /// Initial condition (`INITCOND`), the state value's text.
    pub initial_condition: Option<String>,
    /// Moving-aggregate transition function (`MSFUNC`).
    pub moving_transition_function: Option<String>,
    /// Moving-aggregate inverse transition function (`MINVFUNC`).
    pub moving_inverse_function: Option<String>,
    /// Moving-aggregate state type (`MSTYPE`).
    pub moving_state_type: Option<String>,
    /// Moving-aggregate state size (`MSSPACE`); 0 for the default.
    pub moving_state_space: i64,
    /// Moving-aggregate final function (`MFINALFUNC`).
    pub moving_final_function: Option<String>,
    /// `MFINALFUNC_EXTRA`.
    pub moving_final_extra: bool,
    /// `MFINALFUNC_MODIFY`.
    pub moving_final_modify: PgFinalModify,
    /// Moving-aggregate initial condition (`MINITCOND`).
    pub moving_initial_condition: Option<String>,
    /// Sort operator (`SORTOP`) as `schema.operator`.
    pub sort_operator: Option<String>,
    /// Parallel safety.
    pub parallel: PgParallel,
    /// Arguments with names and modes.
    pub argument_list: Vec<PgArgument>,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

const FROM: &str = "FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace \
                    JOIN pg_catalog.pg_language l ON l.oid OPERATOR(pg_catalog.=) p.prolang";

fn prokinds(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Procedure => "(ARRAY['p']::pg_catalog.\"char\"[])",
        ObjectKind::Aggregate => "(ARRAY['a']::pg_catalog.\"char\"[])",
        _ => "(ARRAY['f', 'w']::pg_catalog.\"char\"[])",
    }
}

fn common_columns() -> String {
    format!(
        "p.oid, pg_catalog.current_database() AS db, n.nspname AS schema, p.proname AS name, \
         pg_catalog.pg_get_function_identity_arguments(p.oid) AS signature, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) p.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {ext} AS extension, \
         ({sys} OR {internal}) AS system_schema, l.lanname AS language",
        ext = extension_sql("pg_catalog.pg_proc", "p.oid"),
        sys = system_schema_sql("n.nspname"),
        internal = INTERNAL_ROUTINE,
    )
}

/// True for routines the server creates as part of another object, such
/// as range and multirange constructors: they have an internal (`i`)
/// dependency, are dropped with their owner and have no DDL of their own,
/// so they are system objects (`pg_dump` skips them too).
const INTERNAL_ROUTINE: &str = "EXISTS (SELECT 1 FROM pg_catalog.pg_depend idep \
     WHERE idep.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass AND idep.objid OPERATOR(pg_catalog.=) p.oid AND idep.deptype OPERATOR(pg_catalog.=) 'i')";

/// Inner list query (see [`super::common::with_system`]).
pub(super) fn inner(kind: ObjectKind, scope: &Scope) -> String {
    let filter = scope.schema_filter("n.nspname");
    if kind == ObjectKind::Aggregate {
        return format!(
            "SELECT {cols}, a.aggkind::pg_catalog.text AS aggregate_kind, \
             pg_catalog.format_type(a.aggtranstype, NULL) AS state_type \
             {FROM} JOIN pg_catalog.pg_aggregate a ON a.aggfnoid OPERATOR(pg_catalog.=) p.oid \
             WHERE p.prokind OPERATOR(pg_catalog.=) 'a'{filter}",
            cols = common_columns(),
        );
    }
    format!(
        "SELECT {cols}, p.provolatile::pg_catalog.text AS volatility, p.prosecdef AS security_definer, \
         p.proleakproof AS leakproof, p.prokind::pg_catalog.text AS routine_kind, \
         CASE WHEN p.prokind OPERATOR(pg_catalog.=) 'p' THEN NULL ELSE pg_catalog.pg_get_function_result(p.oid) END AS result \
         {FROM} WHERE p.prokind OPERATOR(pg_catalog.=) ANY {kinds}{filter}",
        cols = common_columns(),
        kinds = prokinds(kind),
    )
}

/// Order of the list.
pub(super) const ORDER: &str = "q.name, q.signature";

/// A tree entry from a row of [`inner`].
pub(super) fn summarize(kind: ObjectKind, row: &Row<'_>) -> CatalogResult<ObjectSummary> {
    let language = row.req_text("language")?;
    let signature = Some(row.req_text("signature")?);
    if kind == ObjectKind::Aggregate {
        let mut flags = Vec::new();
        match row.req_text("aggregate_kind")?.as_str() {
            "o" => flags.push("ordered-set".to_owned()),
            "h" => flags.push("hypothetical".to_owned()),
            _ => {}
        }
        let detail = Some(format!("state {}", row.req_text("state_type")?));
        return summary(kind, row, true, signature, detail, flags);
    }
    let mut flags = Vec::new();
    match row.req_text("volatility")?.as_str() {
        "i" => flags.push("immutable".to_owned()),
        "s" => flags.push("stable".to_owned()),
        _ => {}
    }
    if row.req_text("routine_kind")? == "w" {
        flags.push("window".to_owned());
    }
    if flag(row, "security_definer")? {
        flags.push("security definer".to_owned());
    }
    if flag(row, "leakproof")? {
        flags.push("leakproof".to_owned());
    }
    let detail = Some(match row.text("result")? {
        Some(result) => format!("{language}, returns {result}"),
        None => language,
    });
    summary(kind, row, true, signature, detail, flags)
}

fn identity_filter(object: &ObjectRef) -> CatalogResult<String> {
    let mut filter = format!(
        "p.prokind OPERATOR(pg_catalog.=) ANY {kinds} AND n.nspname OPERATOR(pg_catalog.=) {schema} AND p.proname OPERATOR(pg_catalog.=) {name}",
        kinds = prokinds(object.kind),
        schema = lit(schema_of(object)?)?,
        name = lit(&object.name)?,
    );
    if let Some(signature) = &object.signature {
        filter.push_str(&format!(
            " AND pg_catalog.pg_get_function_identity_arguments(p.oid) OPERATOR(pg_catalog.=) {}",
            lit(signature)?
        ));
    }
    Ok(filter)
}

fn arguments_sql(filter: &str) -> String {
    format!(
        "SELECT a.ord, (CASE WHEN p.proargnames[a.ord] OPERATOR(pg_catalog.<>) '' THEN p.proargnames[a.ord] END) AS name, \
         COALESCE(p.proargmodes[a.ord]::pg_catalog.text, 'i') AS mode, \
         pg_catalog.format_type(a.type_oid, NULL) AS data_type, \
         pg_catalog.pg_get_function_arg_default(p.oid, a.ord::integer) AS default_value \
         {FROM} CROSS JOIN LATERAL \
         pg_catalog.unnest(COALESCE(p.proallargtypes, p.proargtypes::pg_catalog.oid[])) WITH ORDINALITY AS a(type_oid, ord) \
         WHERE {filter} ORDER BY a.ord"
    )
}

fn read_arguments(rows: &crate::rows::Rows) -> CatalogResult<Vec<PgArgument>> {
    rows.iter()
        .map(|row| {
            let mode = match row.req_text("mode")?.as_str() {
                "i" => PgArgumentMode::In,
                "o" => PgArgumentMode::Out,
                "b" => PgArgumentMode::InOut,
                "v" => PgArgumentMode::Variadic,
                "t" => PgArgumentMode::Table,
                other => return Err(bad_code("proargmodes", other)),
            };
            Ok(PgArgument {
                name: row.text("name")?,
                mode,
                data_type: row.req_text("data_type")?,
                default: row.text("default_value")?,
            })
        })
        .collect()
}

fn not_found(object: &ObjectRef) -> CatalogError {
    CatalogError::NotFound {
        message: format!("{:?} {} was not found", object.kind, object.name),
    }
}

/// Details of a function or procedure.
pub(super) async fn describe_function(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<PgFunction> {
    let filter = identity_filter(object)?;
    let sql_body = if server.at_least(14, 0) {
        "p.prosqlbody IS NOT NULL"
    } else {
        "false"
    };
    let main = format!(
        "SELECT {cols}, pg_catalog.pg_get_userbyid(p.proowner) AS owner, \
         p.prokind::pg_catalog.text AS routine_kind, p.provolatile::pg_catalog.text AS volatility, \
         p.proisstrict AS strict, p.prosecdef AS security_definer, p.proleakproof AS leakproof, \
         p.proparallel::pg_catalog.text AS parallel, p.proretset AS returns_set, \
         p.procost::pg_catalog.float8 AS cost, \
         CASE WHEN p.proretset THEN p.prorows::pg_catalog.float8 END AS est_rows, \
         p.proconfig::pg_catalog.text[] AS config, \
         CASE WHEN {sql_body} THEN NULL ELSE p.prosrc END AS source, \
         CASE WHEN p.probin OPERATOR(pg_catalog.=) ANY (ARRAY['', '-']::pg_catalog.text[]) THEN NULL ELSE p.probin END AS library, \
         {sql_body} AS sql_standard_body, \
         CASE WHEN p.prokind OPERATOR(pg_catalog.=) 'p' THEN NULL ELSE pg_catalog.pg_get_function_result(p.oid) END AS result, \
         CASE WHEN p.prokind OPERATOR(pg_catalog.=) 'p' THEN NULL ELSE pg_catalog.format_type(p.prorettype, NULL) END AS return_type, \
         pg_catalog.pg_get_functiondef(p.oid) AS definition \
         {FROM} WHERE {filter}",
        cols = common_columns(),
    );
    let sets = fetch_pinned(
        session,
        &[
            (with_system(&main, true), "q.signature".to_owned()),
            (arguments_sql(&filter), "q.ord".to_owned()),
        ],
    )
    .await?;
    let [main_rows, argument_rows] = sets.as_slice() else {
        return Err(CatalogError::Unexpected {
            message: "the routine query returned the wrong number of result sets".into(),
        });
    };
    if main_rows.is_empty() {
        return Err(not_found(object));
    }
    let row = main_rows.single("the routine")?;
    let volatility = match row.req_text("volatility")?.as_str() {
        "i" => PgVolatility::Immutable,
        "s" => PgVolatility::Stable,
        "v" => PgVolatility::Volatile,
        other => return Err(bad_code("provolatile", other)),
    };
    Ok(PgFunction {
        schema: row.req_text("schema")?,
        name: row.req_text("name")?,
        signature: row.req_text("signature")?,
        arguments: read_arguments(argument_rows)?,
        result: row.text("result")?,
        return_type: row.text("return_type")?,
        returns_set: flag(&row, "returns_set")?,
        window: row.req_text("routine_kind")? == "w",
        language: row.req_text("language")?,
        volatility,
        strict: flag(&row, "strict")?,
        security_definer: flag(&row, "security_definer")?,
        leakproof: flag(&row, "leakproof")?,
        parallel: PgParallel::parse(&row.req_text("parallel")?)?,
        cost: float(&row, "cost")?.unwrap_or_default(),
        rows: float(&row, "est_rows")?,
        config: texts(&row, "config")?,
        source: row.text("source")?,
        library: row.text("library")?,
        sql_standard_body: flag(&row, "sql_standard_body")?,
        definition: row.req_text("definition")?,
        meta: meta(&row)?,
    })
}

/// DDL of a function or procedure: its definition, owner and comment.
pub(super) fn function_ddl(kind: ObjectKind, function: &PgFunction) -> CatalogResult<String> {
    let keyword = if kind == ObjectKind::Procedure {
        "PROCEDURE"
    } else {
        "FUNCTION"
    };
    let target = format!(
        "{keyword} {}({})",
        qname(&function.schema, &function.name)?,
        function.signature
    );
    let mut statements = vec![function.definition.trim_end().to_owned()];
    statements.extend(owner_to(&target, function.meta.owner.as_deref())?);
    statements.extend(comment_on(&target, function.meta.comment.as_deref())?);
    Ok(script(statements))
}

/// Details of an aggregate.
pub(super) async fn describe_aggregate(
    session: &dyn Session,
    object: &ObjectRef,
) -> CatalogResult<PgAggregate> {
    let filter = identity_filter(object)?;
    let function = |column: &str| {
        format!(
            "(CASE WHEN a.{column}::pg_catalog.oid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN a.{column}::pg_catalog.oid END)::pg_catalog.regproc::pg_catalog.text"
        )
    };
    let main = format!(
        "SELECT {cols}, pg_catalog.pg_get_userbyid(p.proowner) AS owner, \
         pg_catalog.pg_get_function_arguments(p.oid) AS arguments, p.pronargs AS argument_count, \
         a.aggnumdirectargs AS direct_argument_count, a.aggkind::pg_catalog.text AS aggregate_kind, \
         pg_catalog.pg_get_function_result(p.oid) AS result, \
         a.aggtransfn::pg_catalog.regproc::pg_catalog.text AS transition_function, \
         pg_catalog.format_type(a.aggtranstype, NULL) AS state_type, a.aggtransspace AS state_space, \
         {finalfn} AS final_function, a.aggfinalextra AS final_extra, \
         a.aggfinalmodify::pg_catalog.text AS final_modify, {combinefn} AS combine_function, \
         {serialfn} AS serial_function, {deserialfn} AS deserial_function, \
         a.agginitval AS initial_condition, {mtransfn} AS moving_transition_function, \
         {minvfn} AS moving_inverse_function, \
         CASE WHEN a.aggmtranstype OPERATOR(pg_catalog.=) 0 THEN NULL ELSE pg_catalog.format_type(a.aggmtranstype, NULL) END \
         AS moving_state_type, a.aggmtransspace AS moving_state_space, \
         {mfinalfn} AS moving_final_function, a.aggmfinalextra AS moving_final_extra, \
         a.aggmfinalmodify::pg_catalog.text AS moving_final_modify, \
         a.aggminitval AS moving_initial_condition, \
         (SELECT pg_catalog.quote_ident(opn.nspname) OPERATOR(pg_catalog.||) '.' OPERATOR(pg_catalog.||) o.oprname FROM pg_catalog.pg_operator o \
          JOIN pg_catalog.pg_namespace opn ON opn.oid OPERATOR(pg_catalog.=) o.oprnamespace WHERE o.oid OPERATOR(pg_catalog.=) a.aggsortop) \
         AS sort_operator, p.proparallel::pg_catalog.text AS parallel \
         {FROM} JOIN pg_catalog.pg_aggregate a ON a.aggfnoid OPERATOR(pg_catalog.=) p.oid WHERE {filter}",
        cols = common_columns(),
        finalfn = function("aggfinalfn"),
        combinefn = function("aggcombinefn"),
        serialfn = function("aggserialfn"),
        deserialfn = function("aggdeserialfn"),
        mtransfn = function("aggmtransfn"),
        minvfn = function("aggminvtransfn"),
        mfinalfn = function("aggmfinalfn"),
    );
    let sets = fetch_pinned(
        session,
        &[
            (with_system(&main, true), "q.signature".to_owned()),
            (arguments_sql(&filter), "q.ord".to_owned()),
        ],
    )
    .await?;
    let [main_rows, argument_rows] = sets.as_slice() else {
        return Err(CatalogError::Unexpected {
            message: "the aggregate query returned the wrong number of result sets".into(),
        });
    };
    if main_rows.is_empty() {
        return Err(not_found(object));
    }
    let row = main_rows.single("the aggregate")?;
    let aggregate_kind = match row.req_text("aggregate_kind")?.as_str() {
        "n" => PgAggregateKind::Normal,
        "o" => PgAggregateKind::OrderedSet,
        "h" => PgAggregateKind::Hypothetical,
        other => return Err(bad_code("aggkind", other)),
    };
    Ok(PgAggregate {
        schema: row.req_text("schema")?,
        name: row.req_text("name")?,
        signature: row.req_text("signature")?,
        arguments: row.req_text("arguments")?,
        argument_count: int(&row, "argument_count")?,
        direct_argument_count: int(&row, "direct_argument_count")?,
        aggregate_kind,
        result: row.req_text("result")?,
        transition_function: row.req_text("transition_function")?,
        state_type: row.req_text("state_type")?,
        state_space: int(&row, "state_space")?,
        final_function: row.text("final_function")?,
        final_extra: flag(&row, "final_extra")?,
        final_modify: PgFinalModify::parse(&row.req_text("final_modify")?)?,
        combine_function: row.text("combine_function")?,
        serial_function: row.text("serial_function")?,
        deserial_function: row.text("deserial_function")?,
        initial_condition: row.text("initial_condition")?,
        moving_transition_function: row.text("moving_transition_function")?,
        moving_inverse_function: row.text("moving_inverse_function")?,
        moving_state_type: row.text("moving_state_type")?,
        moving_state_space: int(&row, "moving_state_space")?,
        moving_final_function: row.text("moving_final_function")?,
        moving_final_extra: flag(&row, "moving_final_extra")?,
        moving_final_modify: PgFinalModify::parse(&row.req_text("moving_final_modify")?)?,
        moving_initial_condition: row.text("moving_initial_condition")?,
        sort_operator: row.text("sort_operator")?,
        parallel: PgParallel::parse(&row.req_text("parallel")?)?,
        argument_list: read_arguments(argument_rows)?,
        meta: meta(&row)?,
    })
}

/// `CREATE AGGREGATE`, built from the catalog (PostgreSQL has no
/// `pg_get_aggregatedef`), with owner and comment.
pub(super) fn aggregate_ddl(aggregate: &PgAggregate) -> CatalogResult<String> {
    let arguments = if aggregate.argument_count == 0 {
        "*"
    } else {
        aggregate.arguments.as_str()
    };
    let name = qname(&aggregate.schema, &aggregate.name)?;
    let ordered = aggregate.aggregate_kind != PgAggregateKind::Normal;
    let default_modify = if ordered {
        PgFinalModify::ReadWrite
    } else {
        PgFinalModify::ReadOnly
    };
    let mut options = vec![
        format!("SFUNC = {}", aggregate.transition_function),
        format!("STYPE = {}", aggregate.state_type),
    ];
    if aggregate.state_space != 0 {
        options.push(format!("SSPACE = {}", aggregate.state_space));
    }
    if let Some(final_function) = &aggregate.final_function {
        options.push(format!("FINALFUNC = {final_function}"));
        if aggregate.final_extra {
            options.push("FINALFUNC_EXTRA".to_owned());
        }
        if aggregate.final_modify != default_modify {
            options.push(format!(
                "FINALFUNC_MODIFY = {}",
                aggregate.final_modify.keyword()
            ));
        }
    }
    for (keyword, function) in [
        ("COMBINEFUNC", &aggregate.combine_function),
        ("SERIALFUNC", &aggregate.serial_function),
        ("DESERIALFUNC", &aggregate.deserial_function),
    ] {
        if let Some(function) = function {
            options.push(format!("{keyword} = {function}"));
        }
    }
    if let Some(condition) = &aggregate.initial_condition {
        options.push(format!("INITCOND = {}", lit(condition)?));
    }
    if let Some(moving) = &aggregate.moving_transition_function {
        options.push(format!("MSFUNC = {moving}"));
        if let Some(inverse) = &aggregate.moving_inverse_function {
            options.push(format!("MINVFUNC = {inverse}"));
        }
        if let Some(state_type) = &aggregate.moving_state_type {
            options.push(format!("MSTYPE = {state_type}"));
        }
        if aggregate.moving_state_space != 0 {
            options.push(format!("MSSPACE = {}", aggregate.moving_state_space));
        }
        if let Some(final_function) = &aggregate.moving_final_function {
            options.push(format!("MFINALFUNC = {final_function}"));
            if aggregate.moving_final_extra {
                options.push("MFINALFUNC_EXTRA".to_owned());
            }
            if aggregate.moving_final_modify != PgFinalModify::ReadOnly {
                options.push(format!(
                    "MFINALFUNC_MODIFY = {}",
                    aggregate.moving_final_modify.keyword()
                ));
            }
        }
        if let Some(condition) = &aggregate.moving_initial_condition {
            options.push(format!("MINITCOND = {}", lit(condition)?));
        }
    }
    if let Some(operator) = &aggregate.sort_operator {
        options.push(format!("SORTOP = OPERATOR({operator})"));
    }
    if aggregate.parallel != PgParallel::Unsafe {
        options.push(format!("PARALLEL = {}", aggregate.parallel.keyword()));
    }
    if aggregate.aggregate_kind == PgAggregateKind::Hypothetical {
        options.push("HYPOTHETICAL".to_owned());
    }
    let target = format!("AGGREGATE {name}({arguments})");
    let mut statements = vec![format!(
        "CREATE {target} (\n    {}\n)",
        options.join(",\n    ")
    )];
    statements.extend(owner_to(&target, aggregate.meta.owner.as_deref())?);
    statements.extend(comment_on(&target, aggregate.meta.comment.as_deref())?);
    Ok(script(statements))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aggregate() -> PgAggregate {
        PgAggregate {
            schema: "s".into(),
            name: "agg".into(),
            signature: String::new(),
            arguments: String::new(),
            argument_count: 0,
            direct_argument_count: 0,
            aggregate_kind: PgAggregateKind::Normal,
            result: "bigint".into(),
            transition_function: "int8inc".into(),
            state_type: "bigint".into(),
            state_space: 0,
            final_function: None,
            final_extra: false,
            final_modify: PgFinalModify::ReadOnly,
            combine_function: None,
            serial_function: None,
            deserial_function: None,
            initial_condition: Some("0".into()),
            moving_transition_function: None,
            moving_inverse_function: None,
            moving_state_type: None,
            moving_state_space: 0,
            moving_final_function: None,
            moving_final_extra: false,
            moving_final_modify: PgFinalModify::ReadOnly,
            moving_initial_condition: None,
            sort_operator: None,
            parallel: PgParallel::Unsafe,
            argument_list: Vec::new(),
            meta: PgMeta {
                owner: Some("me".into()),
                comment: Some("it's".into()),
                extension: None,
                system: false,
            },
        }
    }

    #[test]
    fn zero_argument_aggregates_use_star() -> CatalogResult<()> {
        assert_eq!(
            aggregate_ddl(&aggregate())?,
            "CREATE AGGREGATE \"s\".\"agg\"(*) (\n    SFUNC = int8inc,\n    STYPE = bigint,\n    \
             INITCOND = '0'\n);\n\nALTER AGGREGATE \"s\".\"agg\"(*) OWNER TO \"me\";\n\n\
             COMMENT ON AGGREGATE \"s\".\"agg\"(*) IS 'it''s';\n"
        );
        Ok(())
    }

    #[test]
    fn hypothetical_aggregates_keep_their_options() -> CatalogResult<()> {
        let mut agg = aggregate();
        agg.argument_count = 2;
        agg.arguments = "integer ORDER BY integer".into();
        agg.aggregate_kind = PgAggregateKind::Hypothetical;
        agg.final_function = Some("s.f".into());
        agg.final_extra = true;
        agg.final_modify = PgFinalModify::ReadWrite;
        agg.sort_operator = Some("pg_catalog.<".into());
        agg.parallel = PgParallel::Safe;
        agg.meta = PgMeta::default();
        let ddl = aggregate_ddl(&agg)?;
        assert!(ddl.starts_with("CREATE AGGREGATE \"s\".\"agg\"(integer ORDER BY integer) ("));
        assert!(ddl.contains("FINALFUNC = s.f,\n    FINALFUNC_EXTRA,\n    INITCOND"));
        assert!(!ddl.contains("FINALFUNC_MODIFY"));
        assert!(ddl.contains("SORTOP = OPERATOR(pg_catalog.<)"));
        assert!(ddl.contains("PARALLEL = SAFE,\n    HYPOTHETICAL\n)"));
        Ok(())
    }
}
