//! PostgreSQL databases, schemas, tables, partitioned tables, partitions,
//! views, materialized views, columns, indexes, constraints, sequences,
//! triggers, rules, policies, foreign tables and tablespaces.
//!
//! The dispatch in `super` calls the functions below. Objects are
//! addressed by [`ObjectRef`]:
//!
//! - databases by `name`; tablespaces by `name` with `database: None`;
//! - schemas by `name`;
//! - tables, partitions, views, materialized views, foreign tables and
//!   sequences by `schema` and `name` (a partition's `parent` names its
//!   partitioned table, but the partition is found by its own schema and
//!   name);
//! - columns, indexes, constraints, triggers, rules and policies by
//!   `schema`, `parent` (the table or view) and `name`.
//!
//! The tree: server level holds databases and tablespaces, a database
//! holds schemas, a schema holds tables, partitioned tables, views,
//! materialized views, sequences and foreign tables, and a relation
//! ([`Container::parent`]) holds its columns, indexes, constraints,
//! triggers, rules, policies and partitions. Partitions are listed under
//! their partitioned table, not in the schema.
//!
//! Lists read catalog columns only: the server's deparsing functions
//! (`pg_get_expr`, `pg_get_indexdef`, `pg_get_constraintdef`,
//! `pg_get_triggerdef`, `pg_get_ruledef`) open the relation and would wait
//! behind a session holding an `ACCESS EXCLUSIVE` lock, so definitions are
//! left to `describe` and `ddl`, which wait at most the catalog's lock
//! timeout and then fail with SQLSTATE `55P03`.

use serde::{Deserialize, Serialize};
use varve_driver::{ServerInfo, Session};

use crate::model::{
    CatalogError, CatalogResult, Container, KindCount, ObjectKind, ObjectRef, ObjectSummary,
};

mod ddl;
mod load;
mod support;
mod types;

pub use types::{
    PgColumn, PgConstraint, PgDatabase, PgFdwOption, PgForeignInfo, PgForeignKey, PgGenerated,
    PgIdentity, PgIndex, PgIndexKey, PgMaterializedView, PgOption, PgOwnedBy, PgPartitionEntry,
    PgPartitionOf, PgPartitioning, PgPolicy, PgQualifiedName, PgRule, PgSchema, PgSequence,
    PgSetting, PgTable, PgTablespace, PgTrigger, PgView,
};

use load::{Filter, RelKinds, RelRow, SeqFilter};
use support::{Pin, fetch, lit, pretty_bytes, system_relation, system_schema};

/// The kinds this module serves.
pub const KINDS: &[ObjectKind] = &[
    ObjectKind::Database,
    ObjectKind::Schema,
    ObjectKind::Table,
    ObjectKind::PartitionedTable,
    ObjectKind::Partition,
    ObjectKind::View,
    ObjectKind::MaterializedView,
    ObjectKind::Column,
    ObjectKind::Index,
    ObjectKind::Constraint,
    ObjectKind::Sequence,
    ObjectKind::Trigger,
    ObjectKind::Rule,
    ObjectKind::Policy,
    ObjectKind::ForeignTable,
    ObjectKind::Tablespace,
];

/// Kinds listed inside a relation ([`Container::parent`]).
const CHILD_KINDS: &[ObjectKind] = &[
    ObjectKind::Column,
    ObjectKind::Index,
    ObjectKind::Constraint,
    ObjectKind::Trigger,
    ObjectKind::Rule,
    ObjectKind::Policy,
    ObjectKind::Partition,
];

/// Details of one object of [`KINDS`], tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PgRelationObject {
    /// A database.
    Database(PgDatabase),
    /// A schema.
    Schema(PgSchema),
    /// A regular or unlogged table.
    Table(PgTable),
    /// A partitioned table.
    PartitionedTable(PgTable),
    /// A partition (table, partitioned table or foreign table).
    Partition(PgTable),
    /// A view.
    View(PgView),
    /// A materialized view.
    MaterializedView(PgMaterializedView),
    /// A column.
    Column(PgColumn),
    /// An index.
    Index(PgIndex),
    /// A constraint.
    Constraint(PgConstraint),
    /// A sequence.
    Sequence(PgSequence),
    /// A trigger.
    Trigger(PgTrigger),
    /// A rewrite rule.
    Rule(PgRule),
    /// A row level security policy.
    Policy(PgPolicy),
    /// A foreign table.
    ForeignTable(PgTable),
    /// A tablespace.
    Tablespace(PgTablespace),
}

/// Kinds of [`KINDS`] the server version supports. Every kind exists on
/// PostgreSQL 13, the oldest supported version.
#[must_use]
pub fn kinds(_server: &ServerInfo) -> Vec<ObjectKind> {
    KINDS.to_vec()
}

/// Kinds of [`KINDS`] with a dedicated editor on this server version
/// (design doc 4.2: foreign tables and tablespaces are browse only).
#[must_use]
pub fn editable(_server: &ServerInfo) -> Vec<ObjectKind> {
    KINDS
        .iter()
        .copied()
        .filter(|k| !matches!(k, ObjectKind::ForeignTable | ObjectKind::Tablespace))
        .collect()
}

/// Named features (see [`crate::model::Capabilities::features`]).
#[must_use]
pub fn features(server: &ServerInfo) -> Vec<String> {
    let mut features = vec![
        "refreshConcurrently",
        "createIndexConcurrently",
        "reindexConcurrently",
        "includeColumns",
        "generatedColumns",
        "hashPartitioning",
        "defaultPartition",
        "attachPartition",
        "rowLevelSecurity",
        "unloggedTables",
    ];
    if server.at_least(14, 0) {
        features.extend(["columnCompression", "detachPartitionConcurrently"]);
    }
    if server.at_least(15, 0) {
        features.extend([
            "nullsNotDistinct",
            "securityInvokerViews",
            "unloggedSequences",
            "icuLocale",
            "foreignKeySetColumns",
        ]);
    }
    if server.at_least(16, 0) {
        features.push("icuRules");
    }
    if server.at_least(17, 0) {
        features.extend(["builtinLocale", "partitionedTableAccessMethod"]);
    }
    if server.at_least(18, 0) {
        features.extend([
            "virtualGeneratedColumns",
            "notNullConstraints",
            "notEnforcedConstraints",
        ]);
    }
    features.into_iter().map(str::to_owned).collect()
}

fn invalid(message: impl Into<String>) -> CatalogError {
    CatalogError::Invalid {
        message: message.into(),
    }
}

fn not_found(what: &str) -> CatalogError {
    CatalogError::NotFound {
        message: format!("{what} was not found"),
    }
}

fn require<'a>(value: Option<&'a str>, what: &str) -> CatalogResult<&'a str> {
    value.ok_or_else(|| invalid(format!("{what} is required")))
}

fn count(kind: ObjectKind, value: Option<i64>) -> KindCount {
    KindCount {
        kind,
        count: u64::try_from(value.unwrap_or(0)).unwrap_or(0),
    }
}

/// Counts of the kinds of [`KINDS`] in a container.
///
/// # Errors
///
/// Driver errors; [`CatalogError::NotFound`] when the parent relation does
/// not exist.
pub async fn counts(
    session: &dyn Session,
    _server: &ServerInfo,
    container: &Container,
    include_system: bool,
) -> CatalogResult<Vec<KindCount>> {
    if let Some(parent) = &container.parent {
        return child_counts(session, parent).await;
    }
    let Some(_database) = &container.database else {
        let db_filter = if include_system {
            String::new()
        } else {
            "WHERE datname OPERATOR(pg_catalog.<>) ALL (ARRAY['template0', 'template1']::pg_catalog.name[])".to_owned()
        };
        let ts_filter = if include_system {
            String::new()
        } else {
            "WHERE spcname OPERATOR(pg_catalog.<>) ALL (ARRAY['pg_default', 'pg_global']::pg_catalog.name[])".to_owned()
        };
        let rows = fetch(
            session,
            Pin::Catalog,
            &format!(
                "SELECT (SELECT pg_catalog.count(*) FROM pg_catalog.pg_database {db_filter})::pg_catalog.int8 AS databases, \
                 (SELECT pg_catalog.count(*) FROM pg_catalog.pg_tablespace {ts_filter})::pg_catalog.int8 AS tablespaces"
            ),
            "1",
        )
        .await?;
        let row = rows.single("the server counts")?;
        return Ok(vec![
            count(ObjectKind::Database, row.int("databases")?),
            count(ObjectKind::Tablespace, row.int("tablespaces")?),
        ]);
    };
    let Some(schema) = &container.schema else {
        let filter = if include_system {
            String::new()
        } else {
            format!("WHERE NOT {}", system_schema("n"))
        };
        let rows = fetch(
            session,
            Pin::Catalog,
            &format!("SELECT pg_catalog.count(*)::pg_catalog.int8 AS schemas FROM pg_catalog.pg_namespace n {filter}"),
            "1",
        )
        .await?;
        let row = rows.single("the schema count")?;
        return Ok(vec![count(ObjectKind::Schema, row.int("schemas")?)]);
    };
    let system = if include_system {
        String::new()
    } else {
        format!(" AND NOT {}", system_relation("c", "n"))
    };
    let sql = format!(
        "SELECT \
           pg_catalog.count(*) FILTER (WHERE c.relkind OPERATOR(pg_catalog.=) 'r' AND NOT c.relispartition)::pg_catalog.int8 AS tables, \
           pg_catalog.count(*) FILTER (WHERE c.relkind OPERATOR(pg_catalog.=) 'p' AND NOT c.relispartition)::pg_catalog.int8 AS partitioned, \
           pg_catalog.count(*) FILTER (WHERE c.relkind OPERATOR(pg_catalog.=) 'f' AND NOT c.relispartition)::pg_catalog.int8 AS foreign_tables, \
           pg_catalog.count(*) FILTER (WHERE c.relkind OPERATOR(pg_catalog.=) 'v')::pg_catalog.int8 AS views, \
           pg_catalog.count(*) FILTER (WHERE c.relkind OPERATOR(pg_catalog.=) 'm')::pg_catalog.int8 AS matviews, \
           pg_catalog.count(*) FILTER (WHERE c.relkind OPERATOR(pg_catalog.=) 'S' AND NOT EXISTS ( \
             SELECT 1 FROM pg_catalog.pg_depend d \
             WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass AND d.objid OPERATOR(pg_catalog.=) c.oid \
               AND d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass \
               AND d.deptype OPERATOR(pg_catalog.=) 'i'))::pg_catalog.int8 AS sequences \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
         WHERE n.nspname OPERATOR(pg_catalog.=) {}{system}",
        lit(schema)?
    );
    let rows = fetch(session, Pin::Catalog, &sql, "1").await?;
    let row = rows.single("the schema object counts")?;
    Ok(vec![
        count(ObjectKind::Table, row.int("tables")?),
        count(ObjectKind::PartitionedTable, row.int("partitioned")?),
        count(ObjectKind::ForeignTable, row.int("foreign_tables")?),
        count(ObjectKind::View, row.int("views")?),
        count(ObjectKind::MaterializedView, row.int("matviews")?),
        count(ObjectKind::Sequence, row.int("sequences")?),
    ])
}

async fn child_counts(session: &dyn Session, parent: &ObjectRef) -> CatalogResult<Vec<KindCount>> {
    let schema = require(parent.schema.as_deref(), "the parent's schema")?;
    let sql = format!(
        "WITH rel AS (SELECT c.oid FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
           WHERE n.nspname OPERATOR(pg_catalog.=) {} AND c.relname OPERATOR(pg_catalog.=) {}) \
         SELECT (SELECT pg_catalog.count(*) FROM rel)::pg_catalog.int8 AS found, \
           (SELECT pg_catalog.count(*) FROM pg_catalog.pg_attribute a, rel \
            WHERE a.attrelid OPERATOR(pg_catalog.=) rel.oid AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped)::pg_catalog.int8 AS columns, \
           (SELECT pg_catalog.count(*) FROM pg_catalog.pg_index i, rel WHERE i.indrelid OPERATOR(pg_catalog.=) rel.oid)::pg_catalog.int8 AS indexes, \
           (SELECT pg_catalog.count(*) FROM pg_catalog.pg_constraint k, rel \
            WHERE k.conrelid OPERATOR(pg_catalog.=) rel.oid AND k.contype OPERATOR(pg_catalog.<>) 't' \
              AND {not_a_clone})::pg_catalog.int8 AS constraints, \
           (SELECT pg_catalog.count(*) FROM pg_catalog.pg_trigger g, rel \
            WHERE g.tgrelid OPERATOR(pg_catalog.=) rel.oid AND NOT g.tgisinternal)::pg_catalog.int8 AS triggers, \
           (SELECT pg_catalog.count(*) FROM pg_catalog.pg_rewrite r, rel \
            WHERE r.ev_class OPERATOR(pg_catalog.=) rel.oid AND r.rulename OPERATOR(pg_catalog.<>) '_RETURN')::pg_catalog.int8 AS rules, \
           (SELECT pg_catalog.count(*) FROM pg_catalog.pg_policy p, rel WHERE p.polrelid OPERATOR(pg_catalog.=) rel.oid)::pg_catalog.int8 AS policies, \
           (SELECT pg_catalog.count(*) FROM pg_catalog.pg_inherits h JOIN pg_catalog.pg_class pc ON pc.oid OPERATOR(pg_catalog.=) h.inhrelid, rel \
            WHERE h.inhparent OPERATOR(pg_catalog.=) rel.oid AND pc.relispartition)::pg_catalog.int8 AS partitions",
        lit(schema)?,
        lit(&parent.name)?,
        not_a_clone = load::NOT_A_CLONE,
    );
    let rows = fetch(session, Pin::Catalog, &sql, "1").await?;
    let row = rows.single("the child counts")?;
    if row.int("found")?.unwrap_or(0) == 0 {
        return Err(not_found(&format!("relation {schema}.{}", parent.name)));
    }
    Ok(vec![
        count(ObjectKind::Column, row.int("columns")?),
        count(ObjectKind::Index, row.int("indexes")?),
        count(ObjectKind::Constraint, row.int("constraints")?),
        count(ObjectKind::Trigger, row.int("triggers")?),
        count(ObjectKind::Rule, row.int("rules")?),
        count(ObjectKind::Policy, row.int("policies")?),
        count(ObjectKind::Partition, row.int("partitions")?),
    ])
}

fn summary(
    object: ObjectRef,
    comment: Option<String>,
    detail: Option<String>,
    flags: Vec<String>,
    has_children: bool,
    system: bool,
) -> ObjectSummary {
    ObjectSummary {
        object,
        comment,
        detail,
        flags,
        has_children,
        system,
        row_estimate: None,
        size_bytes: None,
    }
}

fn object_ref(
    kind: ObjectKind,
    database: Option<&str>,
    schema: Option<&str>,
    parent: Option<&str>,
    name: &str,
) -> ObjectRef {
    ObjectRef {
        kind,
        database: database.map(str::to_owned),
        schema: schema.map(str::to_owned),
        parent: parent.map(str::to_owned),
        name: name.to_owned(),
        signature: None,
    }
}

fn flags(pairs: &[(bool, &str)]) -> Vec<String> {
    pairs
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| (*name).to_owned())
        .collect()
}

/// The kind a relation row is shown as.
fn rel_kind(rel: &RelRow) -> ObjectKind {
    match rel.relkind.as_str() {
        "v" => ObjectKind::View,
        "m" => ObjectKind::MaterializedView,
        _ if rel.is_partition => ObjectKind::Partition,
        "p" => ObjectKind::PartitionedTable,
        "f" => ObjectKind::ForeignTable,
        _ => ObjectKind::Table,
    }
}

fn rel_summary(rel: &RelRow, database: Option<&str>) -> ObjectSummary {
    let kind = rel_kind(rel);
    let parent = rel.partition_of.as_ref().map(|p| p.parent.name.as_str());
    let size = rel.total_bytes.map(pretty_bytes);
    let (detail, flag_list) = match kind {
        ObjectKind::View => (
            None,
            flags(&[
                (
                    rel.options.iter().any(|o| o.name == "check_option"),
                    "check option",
                ),
                (
                    rel.options
                        .iter()
                        .any(|o| o.name == "security_barrier" && o.value == "true"),
                    "security barrier",
                ),
                (
                    rel.options
                        .iter()
                        .any(|o| o.name == "security_invoker" && o.value == "true"),
                    "security invoker",
                ),
            ]),
        ),
        ObjectKind::MaterializedView => (size, flags(&[(!rel.populated, "not populated")])),
        ObjectKind::PartitionedTable => (
            Some(format!(
                "{}, {} partitions",
                rel.part_strategy.clone().unwrap_or_default(),
                rel.child_count
            )),
            flags(&[(rel.rls_enabled, "rls")]),
        ),
        ObjectKind::Partition => {
            let partition = rel.partition_of.as_ref();
            (
                partition.map(|p| p.bound.clone()),
                flags(&[
                    (partition.is_some_and(|p| p.is_default), "default"),
                    (rel.relkind == "p", "partitioned"),
                    (rel.relkind == "f", "foreign"),
                    (
                        partition.is_some_and(|p| p.detach_pending),
                        "detach pending",
                    ),
                    (rel.unlogged, "unlogged"),
                ]),
            )
        }
        ObjectKind::ForeignTable => (
            rel.foreign.as_ref().map(|f| format!("server {}", f.server)),
            Vec::new(),
        ),
        _ => {
            let rows = rel
                .row_estimate
                .map(|r| format!("~{r} rows"))
                .unwrap_or_else(|| "not analyzed".to_owned());
            (
                Some(match size {
                    Some(size) => format!("{rows}, {size}"),
                    None => rows,
                }),
                flags(&[
                    (rel.unlogged, "unlogged"),
                    (rel.rls_enabled, "rls"),
                    (!rel.inherits.is_empty(), "inherits"),
                    (rel.of_type.is_some(), "typed"),
                ]),
            )
        }
    };
    let mut summary = summary(
        object_ref(kind, database, Some(&rel.schema), parent, &rel.name),
        rel.comment.clone(),
        detail,
        flag_list,
        true,
        rel.system,
    );
    // Only relations that hold rows themselves: a table, a leaf partition,
    // a materialized view (a partitioned table's rows are its partitions').
    if matches!(rel.relkind.as_str(), "r" | "m") {
        summary.row_estimate = rel.row_estimate.and_then(|r| u64::try_from(r).ok());
        summary.size_bytes = rel.total_bytes.and_then(|b| u64::try_from(b).ok());
    }
    summary
}

/// The objects of one kind in a container.
///
/// # Errors
///
/// Driver errors; [`CatalogError::Invalid`] when the container does not
/// hold that kind.
pub async fn list(
    session: &dyn Session,
    server: &ServerInfo,
    container: &Container,
    kind: ObjectKind,
    include_system: bool,
) -> CatalogResult<Vec<ObjectSummary>> {
    let database = container.database.as_deref();
    let misplaced = |level: &str| {
        Err(invalid(format!(
            "{kind:?} objects are not listed {level}; the levels are: server (databases, \
             tablespaces), database (schemas), schema (tables, partitioned tables, foreign \
             tables, views, materialized views, sequences), relation (columns, indexes, \
             constraints, triggers, rules, policies, partitions)"
        )))
    };
    // The same levels as `counts`: every kind is listed exactly where it is
    // counted.
    if let Some(parent) = &container.parent {
        if !CHILD_KINDS.contains(&kind) {
            return misplaced("in a relation");
        }
        return list_children(session, server, database, parent, kind, include_system).await;
    }
    let Some(_) = database else {
        return match kind {
            ObjectKind::Database => {
                let mut all = databases(session, server).await?;
                all.retain(|s| include_system || !s.system);
                Ok(all)
            }
            ObjectKind::Tablespace => {
                Ok(
                    load::tablespaces(session, Pin::Catalog, None, include_system)
                        .await?
                        .into_iter()
                        .map(|t| {
                            let detail = t
                                .location
                                .clone()
                                .or_else(|| t.size_bytes.map(pretty_bytes));
                            summary(
                                object_ref(ObjectKind::Tablespace, None, None, None, &t.name),
                                t.comment,
                                detail,
                                Vec::new(),
                                false,
                                t.system,
                            )
                        })
                        .collect(),
                )
            }
            _ => misplaced("at server level"),
        };
    };
    let Some(schema) = container.schema.as_deref() else {
        return match kind {
            ObjectKind::Schema => {
                let name = require(database, "the database")?;
                let mut all = schemas(session, server, name).await?;
                all.retain(|s| include_system || !s.system);
                Ok(all)
            }
            _ => misplaced("at database level"),
        };
    };
    let filter = Filter::Schema(schema);
    let rel_kinds = match kind {
        ObjectKind::Table | ObjectKind::PartitionedTable | ObjectKind::ForeignTable => {
            RelKinds::TopLevelTables
        }
        ObjectKind::View => RelKinds::Views,
        ObjectKind::MaterializedView => RelKinds::MaterializedViews,
        ObjectKind::Sequence => {
            return Ok(load::sequences(
                session,
                Pin::Catalog,
                SeqFilter::Schema(schema),
                include_system,
            )
            .await?
            .into_iter()
            .map(|s| sequence_summary(&s, database))
            .collect());
        }
        _ => return misplaced("in a schema"),
    };
    let rels = load::relations(
        session,
        Pin::Catalog,
        server,
        filter,
        rel_kinds,
        include_system,
        false,
    )
    .await?;
    Ok(rels
        .iter()
        .filter(|r| rel_kind(r) == kind)
        .map(|r| rel_summary(r, database))
        .collect())
}

fn sequence_summary(seq: &PgSequence, database: Option<&str>) -> ObjectSummary {
    let detail = match (seq.last_value, seq.is_called) {
        (Some(last), Some(true)) => format!("{}, last {last}", seq.data_type),
        _ => seq.data_type.clone(),
    };
    summary(
        object_ref(
            ObjectKind::Sequence,
            database,
            Some(&seq.schema),
            None,
            &seq.name,
        ),
        seq.comment.clone(),
        Some(detail),
        flags(&[
            (seq.unlogged, "unlogged"),
            (seq.owned_by.is_some() && seq.identity.is_none(), "owned"),
            (seq.identity.is_some(), "identity"),
        ]),
        false,
        seq.system,
    )
}

async fn list_children(
    session: &dyn Session,
    server: &ServerInfo,
    database: Option<&str>,
    parent: &ObjectRef,
    kind: ObjectKind,
    include_system: bool,
) -> CatalogResult<Vec<ObjectSummary>> {
    let schema = require(parent.schema.as_deref(), "the parent's schema")?;
    let table = parent.name.as_str();
    let filter = Filter::Relation {
        schema,
        name: table,
    };
    let child = |kind, name: &str| object_ref(kind, database, Some(schema), Some(table), name);
    Ok(match kind {
        ObjectKind::Column
        | ObjectKind::Index
        | ObjectKind::Constraint
        | ObjectKind::Trigger
        | ObjectKind::Rule
        | ObjectKind::Policy => load::child_summaries(session, server, filter, kind)
            .await?
            .into_iter()
            .map(|c| {
                summary(
                    child(kind, &c.name),
                    c.comment,
                    c.detail,
                    c.flags,
                    false,
                    c.system,
                )
            })
            .collect(),
        ObjectKind::Partition => load::relations(
            session,
            Pin::Catalog,
            server,
            Filter::PartitionsOf {
                schema,
                name: table,
            },
            RelKinds::Partitions,
            include_system,
            false,
        )
        .await?
        .iter()
        .map(|r| rel_summary(r, database))
        .collect(),
        _ => {
            return Err(invalid(format!(
                "{kind:?} objects are not listed in a relation"
            )));
        }
    })
}

/// Details of one object.
///
/// # Errors
///
/// Driver errors, [`CatalogError::NotFound`].
pub async fn describe(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<PgRelationObject> {
    describe_inner(session, server, object).await
}

async fn describe_inner(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<PgRelationObject> {
    let name = object.name.as_str();
    match object.kind {
        ObjectKind::Database => Ok(PgRelationObject::Database(single(
            load::databases(session, Pin::Catalog, server, Some(name)).await?,
            "database",
            name,
        )?)),
        ObjectKind::Schema => Ok(PgRelationObject::Schema(single(
            load::schemas(session, Pin::Catalog, Some(name), true).await?,
            "schema",
            name,
        )?)),
        ObjectKind::Tablespace => Ok(PgRelationObject::Tablespace(single(
            load::tablespaces(session, Pin::Catalog, Some(name), true).await?,
            "tablespace",
            name,
        )?)),
        ObjectKind::Sequence => {
            let schema = require(object.schema.as_deref(), "the schema")?;
            Ok(PgRelationObject::Sequence(single(
                load::sequences(session, Pin::Catalog, SeqFilter::One(schema, name), true).await?,
                "sequence",
                name,
            )?))
        }
        ObjectKind::Table
        | ObjectKind::PartitionedTable
        | ObjectKind::Partition
        | ObjectKind::ForeignTable
        | ObjectKind::View
        | ObjectKind::MaterializedView => {
            let schema = require(object.schema.as_deref(), "the schema")?;
            relation_details(session, server, schema, name, object.kind).await
        }
        ObjectKind::Column
        | ObjectKind::Index
        | ObjectKind::Constraint
        | ObjectKind::Trigger
        | ObjectKind::Rule
        | ObjectKind::Policy => {
            let schema = require(object.schema.as_deref(), "the schema")?;
            let table = require(object.parent.as_deref(), "the parent relation")?;
            let filter = Filter::Child {
                schema,
                table,
                name,
            };
            let what = format!("{:?} {name} on {schema}.{table}", object.kind);
            Ok(match object.kind {
                ObjectKind::Column => PgRelationObject::Column(single(
                    load::columns(session, Pin::Catalog, server, filter).await?,
                    &what,
                    "",
                )?),
                ObjectKind::Index => {
                    let mut index = single(
                        load::indexes(session, Pin::Catalog, server, filter).await?,
                        &what,
                        "",
                    )?;
                    let sizes = described_sizes(session, schema, &index.name).await?;
                    index.size_bytes = sizes.table;
                    index.size_note = sizes.note;
                    PgRelationObject::Index(index)
                }
                ObjectKind::Constraint => PgRelationObject::Constraint(single(
                    load::constraints(session, Pin::Catalog, server, filter).await?,
                    &what,
                    "",
                )?),
                ObjectKind::Trigger => PgRelationObject::Trigger(single(
                    load::triggers(session, Pin::Catalog, filter).await?,
                    &what,
                    "",
                )?),
                ObjectKind::Rule => PgRelationObject::Rule(single(
                    load::rules(session, Pin::Catalog, filter).await?,
                    &what,
                    "",
                )?),
                _ => PgRelationObject::Policy(single(
                    load::policies(session, Pin::Catalog, filter).await?,
                    &what,
                    "",
                )?),
            })
        }
        other => Err(CatalogError::Unsupported {
            message: format!("{other:?} is not a relations object"),
        }),
    }
}

fn single<T>(items: Vec<T>, what: &str, name: &str) -> CatalogResult<T> {
    let label = if name.is_empty() {
        what.to_owned()
    } else {
        format!("{what} {name}")
    };
    let mut items = items.into_iter();
    match (items.next(), items.next()) {
        (Some(item), None) => Ok(item),
        (None, _) => Err(not_found(&label)),
        (Some(_), Some(_)) => Err(CatalogError::Unexpected {
            message: format!("{label} matched more than one object"),
        }),
    }
}

fn is_table_family(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Table
            | ObjectKind::PartitionedTable
            | ObjectKind::Partition
            | ObjectKind::ForeignTable
    )
}

async fn relation_details(
    session: &dyn Session,
    server: &ServerInfo,
    schema: &str,
    name: &str,
    requested: ObjectKind,
) -> CatalogResult<PgRelationObject> {
    let filter = Filter::Relation { schema, name };
    let rel = single(
        load::relations(
            session,
            Pin::Catalog,
            server,
            filter,
            RelKinds::Any,
            true,
            true,
        )
        .await?,
        "relation",
        &format!("{schema}.{name}"),
    )?;
    let actual = rel_kind(&rel);
    let compatible = actual == requested || (is_table_family(actual) && is_table_family(requested));
    if !compatible {
        return Err(not_found(&format!("{requested:?} {schema}.{name}")));
    }
    let columns = load::columns(session, Pin::Catalog, server, filter).await?;
    match actual {
        ObjectKind::View => {
            let check_option = rel
                .options
                .iter()
                .find(|o| o.name == "check_option")
                .map(|o| o.value.clone());
            let enabled = |option: &str| {
                rel.options
                    .iter()
                    .any(|o| o.name == option && matches!(o.value.as_str(), "true" | "on" | "1"))
            };
            Ok(PgRelationObject::View(PgView {
                definition: rel.definition.clone().unwrap_or_default(),
                check_option,
                security_barrier: enabled("security_barrier"),
                security_invoker: enabled("security_invoker"),
                options: rel.options.clone(),
                triggers: load::triggers(session, Pin::Catalog, filter).await?,
                rules: load::rules(session, Pin::Catalog, filter).await?,
                schema: rel.schema,
                name: rel.name,
                owner: rel.owner,
                comment: rel.comment,
                system: rel.system,
                columns,
            }))
        }
        ObjectKind::MaterializedView => {
            let sizes = described_sizes(session, &rel.schema, &rel.name).await?;
            let mut storage_parameters = rel.options.clone();
            storage_parameters.extend(rel.toast_options.clone());
            Ok(PgRelationObject::MaterializedView(PgMaterializedView {
                definition: rel.definition.clone().unwrap_or_default(),
                populated: rel.populated,
                access_method: rel.access_method.clone(),
                tablespace: rel.tablespace.clone(),
                storage_parameters,
                row_estimate: rel.row_estimate,
                total_bytes: sizes.total,
                size_note: sizes.note,
                last_refresh_note: "PostgreSQL does not record when a materialized view was \
                                    last refreshed"
                    .to_owned(),
                refresh_concurrently: rel.populated && rel.has_plain_unique_index,
                indexes: load::indexes(session, Pin::Catalog, server, filter).await?,
                schema: rel.schema,
                name: rel.name,
                owner: rel.owner,
                comment: rel.comment,
                system: rel.system,
                columns,
            }))
        }
        _ => {
            let table = table_details(session, server, rel, columns).await?;
            Ok(match actual {
                ObjectKind::PartitionedTable => PgRelationObject::PartitionedTable(table),
                ObjectKind::Partition => PgRelationObject::Partition(table),
                ObjectKind::ForeignTable => PgRelationObject::ForeignTable(table),
                _ => PgRelationObject::Table(table),
            })
        }
    }
}

/// Exact sizes for `describe`, or the reason they are missing.
struct DescribedSizes {
    table: Option<i64>,
    indexes: Option<i64>,
    total: Option<i64>,
    note: Option<String>,
}

async fn described_sizes(
    session: &dyn Session,
    schema: &str,
    name: &str,
) -> CatalogResult<DescribedSizes> {
    Ok(match load::exact_sizes(session, schema, name).await? {
        load::Sizes::Exact {
            table,
            indexes,
            total,
        } => DescribedSizes {
            table: Some(table),
            indexes: Some(indexes),
            total: Some(total),
            note: None,
        },
        load::Sizes::Locked => DescribedSizes {
            table: None,
            indexes: None,
            total: None,
            note: Some(SIZE_LOCKED.to_owned()),
        },
    })
}

/// [`PgTable::size_note`] when the sizes cannot be read without waiting.
const SIZE_LOCKED: &str = "size unavailable: the relation is locked by another session";

async fn table_details(
    session: &dyn Session,
    server: &ServerInfo,
    rel: RelRow,
    columns: Vec<PgColumn>,
) -> CatalogResult<PgTable> {
    let filter = Filter::Relation {
        schema: &rel.schema,
        name: &rel.name,
    };
    let partitioning = match (&rel.part_strategy, &rel.part_key) {
        (Some(strategy), Some(key)) => {
            let partitions = load::relations(
                session,
                Pin::Catalog,
                server,
                Filter::PartitionsOf {
                    schema: &rel.schema,
                    name: &rel.name,
                },
                RelKinds::Partitions,
                true,
                false,
            )
            .await?
            .into_iter()
            .filter_map(|p| {
                let of = p.partition_of?;
                Some(PgPartitionEntry {
                    schema: p.schema,
                    name: p.name,
                    bound: of.bound,
                    is_default: of.is_default,
                    partitioned: p.relkind == "p",
                    foreign: p.relkind == "f",
                    detach_pending: of.detach_pending,
                })
            })
            .collect();
            Some(PgPartitioning {
                strategy: strategy.clone(),
                key: key.clone(),
                partitions,
                default_partition: rel.default_partition.clone(),
            })
        }
        _ => None,
    };
    let constraints = load::constraints(session, Pin::Catalog, server, filter).await?;
    let indexes = load::indexes(session, Pin::Catalog, server, filter).await?;
    let triggers = load::triggers(session, Pin::Catalog, filter).await?;
    let rules = load::rules(session, Pin::Catalog, filter).await?;
    let policies = load::policies(session, Pin::Catalog, filter).await?;
    let owned_sequences = load::sequences(
        session,
        Pin::Catalog,
        SeqFilter::OwnedBy(&rel.schema, &rel.name),
        true,
    )
    .await?;
    let sizes = described_sizes(session, &rel.schema, &rel.name).await?;
    let mut storage_parameters = rel.options.clone();
    storage_parameters.extend(rel.toast_options.clone());
    Ok(PgTable {
        schema: rel.schema,
        name: rel.name,
        owner: rel.owner,
        unlogged: rel.unlogged,
        of_type: rel.of_type,
        access_method: rel.access_method,
        tablespace: rel.tablespace,
        storage_parameters,
        inherits: rel.inherits,
        partitioning,
        partition_of: rel.partition_of,
        foreign: rel.foreign,
        row_estimate: rel.row_estimate,
        total_bytes: sizes.total,
        table_bytes: sizes.table,
        index_bytes: sizes.indexes,
        size_note: sizes.note,
        rls_enabled: rel.rls_enabled,
        rls_forced: rel.rls_forced,
        replica_identity: rel.replica_identity,
        comment: rel.comment,
        system: rel.system,
        columns,
        constraints,
        indexes,
        triggers,
        rules,
        policies,
        owned_sequences,
    })
}

/// The `ALTER`/`COMMENT` target of the relation a child belongs to.
async fn parent_target(
    session: &dyn Session,
    server: &ServerInfo,
    schema: &str,
    table: &str,
) -> CatalogResult<ddl::Target> {
    let rel = single(
        load::relations(
            session,
            Pin::Catalog,
            server,
            Filter::Relation {
                schema,
                name: table,
            },
            RelKinds::Any,
            true,
            false,
        )
        .await?,
        "relation",
        &format!("{schema}.{table}"),
    )?;
    let name = support::qname(schema, table)?;
    Ok(match rel.relkind.as_str() {
        "v" => ddl::Target::view(schema, table)?,
        "m" => ddl::Target::materialized_view(schema, table)?,
        "f" => ddl::Target {
            name,
            keyword: "FOREIGN TABLE",
            only: true,
        },
        "p" => ddl::Target {
            name,
            keyword: "TABLE",
            only: false,
        },
        _ => ddl::Target {
            name,
            keyword: "TABLE",
            only: true,
        },
    })
}

/// DDL of one object.
///
/// # Errors
///
/// Driver errors, [`CatalogError::NotFound`], and
/// [`CatalogError::Invalid`] for built-in tablespaces.
pub async fn ddl(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<String> {
    ddl_inner(session, server, object).await
}

async fn ddl_inner(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<String> {
    if matches!(
        object.kind,
        ObjectKind::Column
            | ObjectKind::Index
            | ObjectKind::Constraint
            | ObjectKind::Trigger
            | ObjectKind::Rule
            | ObjectKind::Policy
    ) {
        let schema = require(object.schema.as_deref(), "the schema")?;
        let table = require(object.parent.as_deref(), "the parent relation")?;
        let target = parent_target(session, server, schema, table).await?;
        let details = describe_inner(session, server, object).await?;
        return match details {
            PgRelationObject::Column(column) => {
                let constraints = load::constraints(
                    session,
                    Pin::Catalog,
                    server,
                    Filter::Relation {
                        schema,
                        name: table,
                    },
                )
                .await?;
                let sequences: Vec<PgSequence> = load::sequences(
                    session,
                    Pin::Catalog,
                    SeqFilter::OwnedBy(schema, table),
                    true,
                )
                .await?
                .into_iter()
                .filter(|s| s.owned_by.as_ref().is_some_and(|o| o.column == column.name))
                .collect();
                ddl::column(server, &target, &column, &constraints, &sequences)
            }
            PgRelationObject::Index(index) => {
                let backing = match &index.constraint {
                    Some(name) => Some(single(
                        load::constraints(
                            session,
                            Pin::Catalog,
                            server,
                            Filter::Child {
                                schema,
                                table,
                                name,
                            },
                        )
                        .await?,
                        "constraint",
                        name,
                    )?),
                    None => None,
                };
                let children = if index.partitioned && backing.is_none() {
                    load::index_children(session, Pin::Catalog, schema, &index.name).await?
                } else {
                    Vec::new()
                };
                ddl::index(&target, &index, backing.as_ref(), &children)
            }
            PgRelationObject::Constraint(constraint) => ddl::constraint(&target, &constraint),
            PgRelationObject::Trigger(trigger) => ddl::trigger(&target, &trigger),
            PgRelationObject::Rule(rule) => ddl::rule(&target, &rule),
            PgRelationObject::Policy(policy) => ddl::policy(&target, &policy),
            _ => Err(not_found(&object.name)),
        };
    }
    match describe_inner(session, server, object).await? {
        PgRelationObject::Database(db) => ddl::database(server, &db),
        PgRelationObject::Schema(schema) => ddl::schema(&schema),
        PgRelationObject::Tablespace(ts) => ddl::tablespace(&ts),
        PgRelationObject::Sequence(seq) => ddl::sequence(&seq),
        PgRelationObject::View(view) => ddl::view(&view),
        PgRelationObject::MaterializedView(view) => ddl::materialized_view(server, &view),
        PgRelationObject::Table(table)
        | PgRelationObject::PartitionedTable(table)
        | PgRelationObject::Partition(table)
        | PgRelationObject::ForeignTable(table) => ddl::table(server, &table),
        _ => Err(not_found(&object.name)),
    }
}

/// Databases on the server, system databases (`template0`, `template1`)
/// included and marked.
///
/// # Errors
///
/// Driver errors.
pub async fn databases(
    session: &dyn Session,
    server: &ServerInfo,
) -> CatalogResult<Vec<ObjectSummary>> {
    Ok(load::databases(session, Pin::Catalog, server, None)
        .await?
        .into_iter()
        .map(|d| {
            let detail = d.size_bytes.map(pretty_bytes);
            let flag_list = flags(&[
                (d.is_template, "template"),
                (!d.allow_connections, "no connections"),
                (d.connection_limit == -2, "invalid"),
            ]);
            summary(
                object_ref(ObjectKind::Database, Some(&d.name), None, None, &d.name),
                d.comment,
                detail,
                flag_list,
                true,
                d.system,
            )
        })
        .collect())
}

/// Schemas of the session's database, system schemas included and marked.
///
/// # Errors
///
/// Driver errors.
pub async fn schemas(
    session: &dyn Session,
    _server: &ServerInfo,
    database: &str,
) -> CatalogResult<Vec<ObjectSummary>> {
    Ok(load::schemas(session, Pin::Catalog, None, true)
        .await?
        .into_iter()
        .map(|s| {
            let flag_list = s
                .extension
                .as_ref()
                .map(|e| vec![format!("extension {e}")])
                .unwrap_or_default();
            summary(
                object_ref(ObjectKind::Schema, Some(database), None, None, &s.name),
                s.comment,
                Some(s.owner),
                flag_list,
                true,
                s.system,
            )
        })
        .collect())
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
    fn features_follow_the_version() {
        let old = features(&server(13));
        assert!(old.contains(&"refreshConcurrently".to_owned()));
        assert!(!old.contains(&"columnCompression".to_owned()));
        assert!(!old.contains(&"nullsNotDistinct".to_owned()));
        let new = features(&server(18));
        assert!(new.contains(&"columnCompression".to_owned()));
        assert!(new.contains(&"nullsNotDistinct".to_owned()));
        assert!(new.contains(&"virtualGeneratedColumns".to_owned()));
        assert_eq!(kinds(&server(13)), KINDS.to_vec());
        let editable = editable(&server(18));
        assert!(editable.contains(&ObjectKind::MaterializedView));
        assert!(!editable.contains(&ObjectKind::Tablespace));
    }

    #[test]
    fn details_are_tagged_by_kind() -> Result<(), serde_json::Error> {
        let rule = PgRelationObject::Rule(PgRule {
            schema: "s".into(),
            table: "t".into(),
            name: "r".into(),
            event: "insert".into(),
            instead: false,
            enabled: "origin".into(),
            definition: "CREATE RULE r AS ON INSERT TO s.t DO NOTHING".into(),
            comment: None,
        });
        let json = serde_json::to_value(&rule)?;
        assert_eq!(json["kind"], "rule");
        assert_eq!(json["table"], "t");
        let back: PgRelationObject = serde_json::from_value(json)?;
        assert_eq!(back, rule);
        Ok(())
    }
}
