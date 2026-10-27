//! PostgreSQL functions, procedures, aggregates, event triggers, enum/composite/range types, domains, extensions, publications, subscriptions, foreign data wrappers, foreign servers, collations, casts, operators.
//!
//! Owned by the PostgreSQL programs worker. The dispatch in `super` calls
//! the functions below; keep their signatures.
//!
//! Containers: functions, procedures, aggregates, types, domains,
//! collations and operators are listed per schema ([`Container::schema`]
//! set); event triggers, extensions, publications, subscriptions, foreign
//! data wrappers, foreign servers and casts are listed per database
//! ([`Container::schema`] `None`). None of these kinds has children.
//!
//! Every catalog statement pins `search_path` to `pg_catalog, pg_temp` and
//! `lock_timeout` to [`LOCK_TIMEOUT`] for itself only (see
//! [`pinned_statement`]); the session's own settings are never changed. Type
//! and function names in details and DDL are therefore schema-qualified
//! outside `pg_catalog` and do not depend on the session's `search_path`.
//! Every call (lists, counts, details, DDL) refuses to run inside a
//! transaction block with [`crate::model::CatalogError::Invalid`]. Objects created by
//! `initdb`, objects in system schemas, routines that only exist as part of
//! another object (range constructors) and extension members are `system`.

use serde::{Deserialize, Serialize};
use varve_driver::{ServerInfo, Session};

use crate::model::{
    CatalogError, CatalogResult, Container, KindCount, ObjectKind, ObjectRef, ObjectSummary,
};

mod casts;
mod collations;
mod common;
mod conninfo;
mod events;
mod extensions;
mod foreign;
mod replication;
mod routines;
#[cfg(test)]
mod sql_audit;
mod types;

pub use casts::{PgCast, PgCastContext, PgCastMethod, PgOperator};
pub use collations::{PgCollation, PgCollationProvider};
pub(crate) use common::fetch_pinned_with;
pub use common::{
    IN_TRANSACTION, LOCK_TIMEOUT, PgMeta, PgOption, REDACTED, fdw_extension_sql, fetch_pinned,
    pinned_statement, redact_option,
};
pub use conninfo::strip_passwords;
pub use events::{PgEventTrigger, PgTriggerState};
pub use extensions::{PgAvailableExtension, PgExtension, PgExtensionVersion};
pub use foreign::{PgForeignDataWrapper, PgForeignServer, PgUserMapping};
pub use replication::{PgPublication, PgPublicationTable, PgSubscription};
pub use routines::{
    PgAggregate, PgAggregateKind, PgArgument, PgArgumentMode, PgFinalModify, PgFunction,
    PgParallel, PgVolatility,
};
pub use types::{
    PgAttribute, PgCompositeType, PgDomain, PgDomainConstraint, PgEnumType, PgRangeType,
};

/// The kinds this module serves.
pub const KINDS: &[ObjectKind] = &[
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Aggregate,
    ObjectKind::EventTrigger,
    ObjectKind::EnumType,
    ObjectKind::CompositeType,
    ObjectKind::Domain,
    ObjectKind::RangeType,
    ObjectKind::Extension,
    ObjectKind::Publication,
    ObjectKind::Subscription,
    ObjectKind::ForeignDataWrapper,
    ObjectKind::ForeignServer,
    ObjectKind::Collation,
    ObjectKind::Cast,
    ObjectKind::Operator,
];

/// Kinds of [`KINDS`] listed per schema.
pub const SCHEMA_KINDS: &[ObjectKind] = &[
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Aggregate,
    ObjectKind::EnumType,
    ObjectKind::CompositeType,
    ObjectKind::Domain,
    ObjectKind::RangeType,
    ObjectKind::Collation,
    ObjectKind::Operator,
];

/// Kinds of [`KINDS`] listed per database.
pub const DATABASE_KINDS: &[ObjectKind] = &[
    ObjectKind::EventTrigger,
    ObjectKind::Extension,
    ObjectKind::Publication,
    ObjectKind::Subscription,
    ObjectKind::ForeignDataWrapper,
    ObjectKind::ForeignServer,
    ObjectKind::Cast,
];

/// Details of one object of [`KINDS`], tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PgProgramObject {
    /// A function (normal or window).
    Function(PgFunction),
    /// A procedure.
    Procedure(PgFunction),
    /// An aggregate.
    Aggregate(PgAggregate),
    /// An event trigger.
    EventTrigger(PgEventTrigger),
    /// An enum type.
    EnumType(PgEnumType),
    /// A composite type.
    CompositeType(PgCompositeType),
    /// A domain.
    Domain(PgDomain),
    /// A range type.
    RangeType(PgRangeType),
    /// An installed extension.
    Extension(PgExtension),
    /// A publication.
    Publication(PgPublication),
    /// A subscription.
    Subscription(PgSubscription),
    /// A foreign data wrapper.
    ForeignDataWrapper(PgForeignDataWrapper),
    /// A foreign server.
    ForeignServer(PgForeignServer),
    /// A collation.
    Collation(PgCollation),
    /// A cast.
    Cast(PgCast),
    /// An operator.
    Operator(PgOperator),
}

/// Kinds of [`KINDS`] the server version supports (all of them exist on
/// every supported version, 13 and newer).
#[must_use]
pub fn kinds(_server: &ServerInfo) -> Vec<ObjectKind> {
    KINDS.to_vec()
}

/// Kinds of [`KINDS`] with a dedicated editor on this server version
/// (design doc 4.2: functions and procedures, event triggers, types,
/// extensions).
#[must_use]
pub fn editable(_server: &ServerInfo) -> Vec<ObjectKind> {
    vec![
        ObjectKind::Function,
        ObjectKind::Procedure,
        ObjectKind::EventTrigger,
        ObjectKind::EnumType,
        ObjectKind::CompositeType,
        ObjectKind::Domain,
        ObjectKind::RangeType,
        ObjectKind::Extension,
    ]
}

/// Named features (see [`crate::model::Capabilities::features`]).
#[must_use]
pub fn features(server: &ServerInfo) -> Vec<String> {
    let mut features = Vec::new();
    let mut add = |major: u32, name: &str| {
        if server.at_least(major, 0) {
            features.push(name.to_owned());
        }
    };
    add(14, "sqlStandardFunctionBody");
    add(14, "procedureOutParameters");
    add(14, "multirangeTypes");
    add(14, "subscriptionBinary");
    add(14, "subscriptionStreaming");
    add(15, "publicationRowFilters");
    add(15, "publicationColumnLists");
    add(15, "publicationSchemas");
    add(15, "subscriptionTwoPhase");
    add(16, "icuCollationRules");
    add(16, "subscriptionOrigin");
    add(16, "subscriptionParallelStreaming");
    add(17, "builtinCollationProvider");
    add(17, "subscriptionFailover");
    add(18, "publicationGeneratedColumns");
    features
}

/// SQL condition, true when `oid_expr` (an object of the system catalog
/// `catalog`, written schema-qualified: for example `pg_catalog.pg_class`
/// and `c.oid`) is owned by an extension.
/// Other catalog lists use it to mark extension members `system`. Both
/// arguments must be code constants, never user text.
#[must_use]
pub fn extension_member_sql(catalog: &str, oid_expr: &str) -> String {
    format!("{} IS NOT NULL", common::extension_sql(catalog, oid_expr))
}

/// Where a list or count looks.
pub(super) enum Scope {
    /// One schema, as a quoted SQL literal.
    Schema(String),
    /// The session's database.
    Database,
}

impl Scope {
    /// `None` for containers without objects of this module: server-wide
    /// ones (no database) and objects with a parent.
    fn of(container: &Container) -> CatalogResult<Option<Self>> {
        if container.parent.is_some() || container.database.is_none() {
            return Ok(None);
        }
        Ok(Some(match &container.schema {
            Some(schema) => Self::Schema(common::lit(schema)?),
            None => Self::Database,
        }))
    }

    fn kinds(&self) -> &'static [ObjectKind] {
        match self {
            Self::Schema(_) => SCHEMA_KINDS,
            Self::Database => DATABASE_KINDS,
        }
    }

    /// ` AND <column> = '<schema>'` for a schema scope.
    pub(super) fn schema_filter(&self, column: &str) -> String {
        match self {
            Self::Schema(literal) => format!(" AND {column} OPERATOR(pg_catalog.=) {literal}"),
            Self::Database => String::new(),
        }
    }
}

/// The inner list query of a kind; `restricted` reads only the
/// subscription columns every role may read.
fn inner_sql(kind: ObjectKind, server: &ServerInfo, scope: &Scope, restricted: bool) -> String {
    match kind {
        ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Aggregate => {
            routines::inner(kind, scope)
        }
        ObjectKind::EnumType
        | ObjectKind::CompositeType
        | ObjectKind::Domain
        | ObjectKind::RangeType => types::inner(kind, scope),
        ObjectKind::EventTrigger => events::inner(),
        ObjectKind::Extension => extensions::inner(),
        ObjectKind::Publication | ObjectKind::Subscription => {
            replication::inner(kind, server, restricted)
        }
        ObjectKind::ForeignDataWrapper | ObjectKind::ForeignServer => foreign::inner(kind),
        ObjectKind::Collation => collations::inner(server, scope),
        _ => casts::inner(kind, scope),
    }
}

fn order(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Aggregate => routines::ORDER,
        ObjectKind::EnumType
        | ObjectKind::CompositeType
        | ObjectKind::Domain
        | ObjectKind::RangeType => types::ORDER,
        ObjectKind::EventTrigger => events::ORDER,
        ObjectKind::Extension => extensions::ORDER,
        ObjectKind::Publication | ObjectKind::Subscription => replication::ORDER,
        ObjectKind::ForeignDataWrapper | ObjectKind::ForeignServer => foreign::ORDER,
        ObjectKind::Collation => collations::ORDER,
        _ => casts::order(kind),
    }
}

fn summarize(kind: ObjectKind, row: &crate::rows::Row<'_>) -> CatalogResult<ObjectSummary> {
    match kind {
        ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Aggregate => {
            routines::summarize(kind, row)
        }
        ObjectKind::EnumType
        | ObjectKind::CompositeType
        | ObjectKind::Domain
        | ObjectKind::RangeType => types::summarize(kind, row),
        ObjectKind::EventTrigger => events::summarize(row),
        ObjectKind::Extension => extensions::summarize(row),
        ObjectKind::Publication | ObjectKind::Subscription => replication::summarize(kind, row),
        ObjectKind::ForeignDataWrapper | ObjectKind::ForeignServer => foreign::summarize(kind, row),
        ObjectKind::Collation => collations::summarize(row),
        _ => casts::summarize(kind, row),
    }
}

/// Stable tag of a kind inside count queries.
fn tag(kind: ObjectKind) -> String {
    format!("{kind:?}")
}

fn counts_sql(
    server: &ServerInfo,
    scope: &Scope,
    kinds: &[ObjectKind],
    include_system: bool,
    restricted: bool,
) -> String {
    let parts: Vec<String> = kinds
        .iter()
        .map(|&kind| {
            format!(
                "SELECT '{tag}'::pg_catalog.text AS kind, i.oid, i.extension, i.system_schema FROM ({inner}) i",
                tag = tag(kind),
                inner = inner_sql(kind, server, scope, restricted),
            )
        })
        .collect();
    let filter = if include_system {
        String::new()
    } else {
        format!(" WHERE NOT {}", common::SYSTEM_EXPR)
    };
    format!(
        "SELECT q.kind, pg_catalog.count(*) AS n FROM ({}) q{filter} GROUP BY q.kind",
        parts.join("\nUNION ALL\n")
    )
}

async fn fetch_counts(
    session: &dyn Session,
    server: &ServerInfo,
    scope: &Scope,
    kinds: &[ObjectKind],
    include_system: bool,
    restricted: bool,
) -> CatalogResult<crate::rows::Rows> {
    common::fetch_one(
        session,
        &counts_sql(server, scope, kinds, include_system, restricted),
        "q.kind",
    )
    .await
}

/// Every extension the server can install (`pg_available_extensions` with
/// the versions of `pg_available_extension_versions`), sorted by name, with
/// the installed version and schema in the session's database. Feeds the
/// install picker of the extensions editor.
///
/// # Errors
///
/// [`CatalogError::Invalid`] inside a transaction block, driver errors.
pub async fn available_extensions(
    session: &dyn Session,
) -> CatalogResult<Vec<PgAvailableExtension>> {
    extensions::available(session).await
}

/// Counts of the kinds of [`KINDS`] in a container, in one query. Without
/// superuser rights subscriptions are counted from the columns every role
/// may read; if even those are denied they count as zero instead of
/// failing the tree.
///
/// # Errors
///
/// Driver errors.
pub async fn counts(
    session: &dyn Session,
    server: &ServerInfo,
    container: &Container,
    include_system: bool,
) -> CatalogResult<Vec<KindCount>> {
    let Some(scope) = Scope::of(container)? else {
        return Ok(Vec::new());
    };
    let kinds = scope.kinds();
    let rows = match fetch_counts(session, server, &scope, kinds, include_system, false).await {
        Err(error)
            if common::is_permission_denied(&error)
                && kinds.contains(&ObjectKind::Subscription) =>
        {
            match fetch_counts(session, server, &scope, kinds, include_system, true).await {
                Err(error) if common::is_permission_denied(&error) => {
                    let readable: Vec<ObjectKind> = kinds
                        .iter()
                        .copied()
                        .filter(|&k| k != ObjectKind::Subscription)
                        .collect();
                    fetch_counts(session, server, &scope, &readable, include_system, true).await?
                }
                other => other?,
            }
        }
        other => other?,
    };
    let mut counts: Vec<KindCount> = kinds
        .iter()
        .map(|&kind| KindCount { kind, count: 0 })
        .collect();
    for row in rows.iter() {
        let kind = row.req_text("kind")?;
        let n = u64::try_from(common::int(&row, "n")?).unwrap_or_default();
        if let Some(entry) = counts.iter_mut().find(|c| tag(c.kind) == kind) {
            entry.count = n;
        }
    }
    Ok(counts)
}

/// The objects of one kind in a container. A kind listed in the other
/// scope (a schema kind in a database container, or the reverse) and any
/// container with a parent give an empty list. Without superuser rights
/// subscriptions are listed from the columns every role may read (no
/// comment on PostgreSQL 13); if even those are denied the list is empty
/// instead of an error.
///
/// # Errors
///
/// Driver errors, [`CatalogError::Unsupported`] for a kind of another
/// module.
pub async fn list(
    session: &dyn Session,
    server: &ServerInfo,
    container: &Container,
    kind: ObjectKind,
    include_system: bool,
) -> CatalogResult<Vec<ObjectSummary>> {
    if !KINDS.contains(&kind) {
        return Err(not_mine(kind));
    }
    let Some(scope) = Scope::of(container)? else {
        return Ok(Vec::new());
    };
    if !scope.kinds().contains(&kind) {
        return Ok(Vec::new());
    }
    let query = |restricted: bool| {
        common::with_system(&inner_sql(kind, server, &scope, restricted), include_system)
    };
    let rows = match common::fetch_one(session, &query(false), order(kind)).await {
        Err(error) if kind == ObjectKind::Subscription && common::is_permission_denied(&error) => {
            match common::fetch_one(session, &query(true), order(kind)).await {
                Err(error) if common::is_permission_denied(&error) => return Ok(Vec::new()),
                other => other?,
            }
        }
        other => other?,
    };
    rows.iter().map(|row| summarize(kind, &row)).collect()
}

fn not_mine(kind: ObjectKind) -> CatalogError {
    CatalogError::Unsupported {
        message: format!("{kind:?} is not a PostgreSQL program object"),
    }
}

/// Details of one object.
///
/// # Errors
///
/// Driver errors, [`CatalogError::NotFound`], [`CatalogError::Invalid`]
/// for a schema-scoped reference without a schema.
pub async fn describe(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<PgProgramObject> {
    Ok(match object.kind {
        ObjectKind::Function => {
            PgProgramObject::Function(routines::describe_function(session, server, object).await?)
        }
        ObjectKind::Procedure => {
            PgProgramObject::Procedure(routines::describe_function(session, server, object).await?)
        }
        ObjectKind::Aggregate => {
            PgProgramObject::Aggregate(routines::describe_aggregate(session, object).await?)
        }
        ObjectKind::EventTrigger => {
            PgProgramObject::EventTrigger(events::describe(session, object).await?)
        }
        ObjectKind::EnumType => {
            PgProgramObject::EnumType(types::describe_enum(session, object).await?)
        }
        ObjectKind::CompositeType => {
            PgProgramObject::CompositeType(types::describe_composite(session, object).await?)
        }
        ObjectKind::Domain => {
            PgProgramObject::Domain(types::describe_domain(session, object).await?)
        }
        ObjectKind::RangeType => {
            PgProgramObject::RangeType(types::describe_range(session, server, object).await?)
        }
        ObjectKind::Extension => {
            PgProgramObject::Extension(extensions::describe(session, object).await?)
        }
        ObjectKind::Publication => PgProgramObject::Publication(
            replication::describe_publication(session, server, object).await?,
        ),
        ObjectKind::Subscription => PgProgramObject::Subscription(
            replication::describe_subscription(session, server, object).await?,
        ),
        ObjectKind::ForeignDataWrapper => {
            PgProgramObject::ForeignDataWrapper(foreign::describe_wrapper(session, object).await?)
        }
        ObjectKind::ForeignServer => {
            PgProgramObject::ForeignServer(foreign::describe_server(session, object).await?)
        }
        ObjectKind::Collation => {
            PgProgramObject::Collation(collations::describe(session, server, object).await?)
        }
        ObjectKind::Cast => PgProgramObject::Cast(casts::describe_cast(session, object).await?),
        ObjectKind::Operator => {
            PgProgramObject::Operator(casts::describe_operator(session, object).await?)
        }
        other => return Err(not_mine(other)),
    })
}

/// The DDL of described details: complete statements, each ended by `;`,
/// that recreate the object with its owner and comment. Names and values
/// are quoted through [`crate::sql`].
///
/// # Errors
///
/// [`CatalogError::Unsupported`] for objects without DDL (the built-in
/// default collation, a subscription whose connection string cannot be
/// read, schema publications on servers before 15); quoting errors.
pub fn ddl_of(server: &ServerInfo, object: &PgProgramObject) -> CatalogResult<String> {
    match object {
        PgProgramObject::Function(f) => routines::function_ddl(ObjectKind::Function, f),
        PgProgramObject::Procedure(p) => routines::function_ddl(ObjectKind::Procedure, p),
        PgProgramObject::Aggregate(a) => routines::aggregate_ddl(a),
        PgProgramObject::EventTrigger(t) => events::ddl(t),
        PgProgramObject::EnumType(e) => types::enum_ddl(e),
        PgProgramObject::CompositeType(c) => types::composite_ddl(c),
        PgProgramObject::Domain(d) => types::domain_ddl(d),
        PgProgramObject::RangeType(r) => types::range_ddl(r),
        PgProgramObject::Extension(e) => extensions::ddl(e),
        PgProgramObject::Publication(p) => replication::publication_ddl(server, p),
        PgProgramObject::Subscription(s) => replication::subscription_ddl(s),
        PgProgramObject::ForeignDataWrapper(w) => foreign::wrapper_ddl(w),
        PgProgramObject::ForeignServer(s) => foreign::server_ddl(s),
        PgProgramObject::Collation(c) => collations::ddl(c),
        PgProgramObject::Cast(c) => casts::cast_ddl(c),
        PgProgramObject::Operator(o) => casts::operator_ddl(o),
    }
}

/// DDL of one object (see [`ddl_of`]).
///
/// # Errors
///
/// Driver errors, [`CatalogError::NotFound`], and as [`ddl_of`].
pub async fn ddl(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<String> {
    let details = describe(session, server, object).await?;
    ddl_of(server, &details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_core::Engine;

    fn server(major: u32) -> ServerInfo {
        ServerInfo {
            engine: Engine::Postgres,
            version: format!("{major}.0"),
            major,
            minor: 0,
            patch: None,
        }
    }

    #[test]
    fn scopes_split_the_kinds() {
        for kind in KINDS {
            assert!(
                SCHEMA_KINDS.contains(kind) ^ DATABASE_KINDS.contains(kind),
                "{kind:?} must be in exactly one scope"
            );
        }
        assert_eq!(SCHEMA_KINDS.len() + DATABASE_KINDS.len(), KINDS.len());
    }

    #[test]
    fn features_follow_the_version() {
        let old = features(&server(13));
        assert!(old.is_empty());
        let new = features(&server(18));
        assert!(new.contains(&"publicationGeneratedColumns".to_owned()));
        assert!(new.contains(&"sqlStandardFunctionBody".to_owned()));
        let mid = features(&server(15));
        assert!(mid.contains(&"publicationSchemas".to_owned()));
        assert!(!mid.contains(&"icuCollationRules".to_owned()));
        assert_eq!(kinds(&server(13)).len(), KINDS.len());
        assert!(editable(&server(13)).iter().all(|k| KINDS.contains(k)));
    }

    #[test]
    fn details_serialize_with_a_kind_tag() -> Result<(), serde_json::Error> {
        let object = PgProgramObject::EnumType(PgEnumType {
            schema: "s".into(),
            name: "e".into(),
            labels: vec!["a".into()],
            meta: PgMeta::default(),
        });
        let json = serde_json::to_value(&object)?;
        assert_eq!(json["kind"], "enumType");
        assert_eq!(json["labels"][0], "a");
        let back: PgProgramObject = serde_json::from_value(json)?;
        assert_eq!(back, object);
        Ok(())
    }

    #[test]
    fn count_queries_cover_every_kind_of_the_scope() -> CatalogResult<()> {
        let sql = counts_sql(&server(18), &Scope::Database, DATABASE_KINDS, false, false);
        for kind in DATABASE_KINDS {
            assert!(sql.contains(&format!("'{kind:?}'::pg_catalog.text")));
        }
        assert!(sql.contains("WHERE NOT"));
        let schema = Scope::of(&Container {
            database: Some("d".into()),
            schema: Some("it's".into()),
            parent: None,
        })?;
        assert!(matches!(&schema, Some(Scope::Schema(l)) if l == "'it''s'"));
        Ok(())
    }
}
