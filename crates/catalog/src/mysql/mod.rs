//! MySQL and MariaDB catalog.
//!
//! The two engines share the wire protocol but are separate engines above
//! it (design doc 2.2): [`ServerInfo::engine`] tells them apart, and the
//! catalog uses separate queries and capabilities where they differ.
//!
//! The tree: databases, then per database tables, views, procedures,
//! functions, triggers, events and (MariaDB) sequences, packages and
//! package bodies; per table its columns, indexes, constraints, triggers
//! and partitions; per view its columns; per partition its
//! subpartitions. Children name their table in [`ObjectRef::parent`].
//! MySQL has no schemas inside a database, so [`Catalog::schemas`] is
//! empty.
//!
//! Objects in the system databases (`mysql`, `sys`, `information_schema`,
//! `performance_schema`) and system views are marked `system` and left out
//! of lists and counts unless asked for.
//!
//! Every call reads `information_schema` (or `SHOW CREATE`) through
//! [`Session::execute`]; lists are one query each, details one round trip.
//! See [`ddl`](self) for how the DDL is made re-runnable.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use varve_driver::{ServerInfo, Session};

use crate::model::{
    Capabilities, Catalog, CatalogError, CatalogResult, Container, KindCount, ObjectDetails,
    ObjectKind, ObjectRef, ObjectSummary,
};

mod ddl;
pub mod edit;
mod engines;
mod programs;
mod row_count;
mod schema;
pub mod session;
mod text;
mod types;
mod util;

pub use engines::{MyEngine, engine_loses_rows, engine_warning, engines};
pub use row_count::{ROW_COUNT_CAP_MS, row_count};
pub use session::{DDL_SESSION_PREFIX, DdlSession, ddl_session_header, strip_ddl_session_header};
pub use types::{
    MyCheckLevel, MyColumn, MyConstraint, MyConstraintType, MyDatabase, MyDefault, MyEvent,
    MyEventScheduler, MyEventStatus, MyGenerated, MyGeneratedKind, MyIndex, MyIndexKind, MyKeyPart,
    MyPackage, MyParameter, MyPartition, MyPartitioning, MyPeriod, MyReference, MyRoutine,
    MySchedule, MySequence, MySystemVersioning, MyTable, MyTrigger, MyView,
};

use util::{at_least, database_of, is_mariadb, is_system_db, lit, parent_of};

/// Details of one MySQL or MariaDB object, tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MyObject {
    /// A database.
    Database(MyDatabase),
    /// A base table with its columns, indexes, constraints and partitions.
    Table(Box<MyTable>),
    /// A view.
    View(MyView),
    /// A column.
    Column(MyColumn),
    /// An index.
    Index(MyIndex),
    /// A constraint.
    Constraint(MyConstraint),
    /// A partition or subpartition.
    Partition(MyPartition),
    /// A stored procedure.
    Procedure(MyRoutine),
    /// A stored function.
    Function(MyRoutine),
    /// A trigger.
    Trigger(MyTrigger),
    /// A scheduled event.
    Event(MyEvent),
    /// A MariaDB sequence.
    Sequence(MySequence),
    /// A MariaDB package specification.
    Package(MyPackage),
    /// A MariaDB package body.
    PackageBody(MyPackage),
}

/// The MySQL and MariaDB catalog.
#[derive(Debug, Clone)]
pub struct MyCatalog {
    server: ServerInfo,
    capabilities: Capabilities,
}

/// Kinds the browser shows on this server.
fn kinds_for(server: &ServerInfo) -> Vec<ObjectKind> {
    let mut kinds = vec![
        ObjectKind::Database,
        ObjectKind::Table,
        ObjectKind::Partition,
        ObjectKind::View,
        ObjectKind::Column,
        ObjectKind::Index,
        ObjectKind::Constraint,
        ObjectKind::Procedure,
        ObjectKind::Function,
        ObjectKind::Trigger,
        ObjectKind::Event,
    ];
    if is_mariadb(server) {
        kinds.extend([
            ObjectKind::Sequence,
            ObjectKind::Package,
            ObjectKind::PackageBody,
        ]);
    }
    kinds.sort();
    kinds
}

/// Kinds with a dedicated editor (design doc 4.2: packages are browse
/// only).
fn editable_for(server: &ServerInfo) -> Vec<ObjectKind> {
    let mut kinds: Vec<ObjectKind> = kinds_for(server)
        .into_iter()
        .filter(|k| !matches!(k, ObjectKind::Package | ObjectKind::PackageBody))
        .collect();
    kinds.sort();
    kinds
}

/// Named features by engine and version.
fn features_for(server: &ServerInfo) -> Vec<String> {
    let mut features: Vec<&str> = vec![
        "processList",
        "serverVariables",
        "explainJson",
        "partitioning",
        "events",
    ];
    if is_mariadb(server) {
        features.extend(["analyzeFormatJson", "jsonAlias", "expressionDefaults"]);
        if at_least(server, 10, 2, 1) {
            features.push("checkConstraints");
        }
        if at_least(server, 10, 3, 3) {
            features.push("invisibleColumns");
        }
        if at_least(server, 10, 3, 4) {
            features.push("systemVersioning");
        }
        if at_least(server, 10, 3, 0) {
            features.extend(["sequences", "packages"]);
        }
        if at_least(server, 10, 4, 3) {
            features.push("applicationTimePeriods");
        }
        if at_least(server, 10, 6, 0) {
            features.push("ignoredIndexes");
        }
        if at_least(server, 10, 8, 0) {
            features.push("descendingIndexes");
        }
    } else {
        features.extend([
            "setPersist",
            "invisibleIndexes",
            "descendingIndexes",
            "jsonType",
        ]);
        if at_least(server, 8, 0, 13) {
            features.extend(["functionalIndexes", "expressionDefaults"]);
        }
        if at_least(server, 8, 0, 16) {
            features.extend(["checkConstraints", "explainFormatTree"]);
        }
        if at_least(server, 8, 0, 18) {
            features.push("explainAnalyze");
        }
        if at_least(server, 8, 0, 22) {
            features.push("databaseReadOnly");
        }
        if at_least(server, 8, 0, 23) {
            features.push("invisibleColumns");
        }
    }
    let mut features: Vec<String> = features.into_iter().map(str::to_owned).collect();
    features.sort();
    features
}

impl MyCatalog {
    /// The catalog for a server, with the capabilities its engine and
    /// version imply. Runtime state (event scheduler, Galera) is added by
    /// [`MyCatalog::probe`].
    #[must_use]
    pub fn new(server: &ServerInfo) -> Self {
        Self {
            server: server.clone(),
            capabilities: Capabilities {
                engine: server.engine,
                kinds: kinds_for(server),
                editable: editable_for(server),
                features: features_for(server),
            },
        }
    }

    /// The catalog for the session's server, with runtime features read
    /// from the server: `eventScheduler` when the event scheduler is on,
    /// and on MariaDB `galera` when `wsrep_on` is on.
    ///
    /// # Errors
    ///
    /// Driver errors.
    pub async fn probe(session: &dyn Session) -> CatalogResult<Self> {
        let mut catalog = Self::new(session.server_info());
        let rows = crate::rows::query(
            session,
            "SHOW GLOBAL VARIABLES WHERE Variable_name IN ('event_scheduler', 'wsrep_on')",
        )
        .await?;
        for row in rows.iter() {
            let name = util::text(&row, "Variable_name")?.unwrap_or_default();
            let on = util::text(&row, "Value")?
                .is_some_and(|v| v.eq_ignore_ascii_case("ON") || v == "1");
            if !on {
                continue;
            }
            if name.eq_ignore_ascii_case("event_scheduler") {
                catalog.capabilities.features.push("eventScheduler".into());
            } else if name.eq_ignore_ascii_case("wsrep_on") && is_mariadb(&catalog.server) {
                catalog.capabilities.features.push("galera".into());
            }
        }
        catalog.capabilities.features.sort();
        catalog.capabilities.features.dedup();
        Ok(catalog)
    }

    /// The state of the server's event scheduler.
    ///
    /// # Errors
    ///
    /// Driver errors.
    pub async fn event_scheduler(&self, session: &dyn Session) -> CatalogResult<MyEventScheduler> {
        let rows = crate::rows::query(session, programs::SCHEDULER_SQL).await?;
        Ok(programs::parse_scheduler(
            programs::single_text(&rows, "scheduler")?.as_deref(),
        ))
    }

    fn unsupported(&self, kind: ObjectKind) -> CatalogError {
        let engine = if is_mariadb(&self.server) {
            "MariaDB"
        } else {
            "MySQL"
        };
        CatalogError::Unsupported {
            message: format!("{engine} has no {kind:?} objects"),
        }
    }

    fn check_kind(&self, kind: ObjectKind) -> CatalogResult<()> {
        if self.capabilities.kinds.contains(&kind) {
            Ok(())
        } else {
            Err(self.unsupported(kind))
        }
    }
}

/// Counts per kind in a container, as `kind` / `n` rows.
fn counts_sql(server: &ServerInfo, container: &Container) -> CatalogResult<Option<String>> {
    let Some(database) = container.database.as_deref() else {
        return Ok(None);
    };
    let db = lit(database)?;
    let Some(parent) = &container.parent else {
        let mut parts = vec![
            format!(
                "SELECT 'table' AS `kind`, COUNT(*) AS `n` FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = {db} AND TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED')"
            ),
            format!(
                "SELECT 'view', COUNT(*) FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = {db} AND TABLE_TYPE IN ('VIEW', 'SYSTEM VIEW')"
            ),
            format!(
                "SELECT CAST(ROUTINE_TYPE AS CHAR), COUNT(*) FROM information_schema.ROUTINES \
                 WHERE ROUTINE_SCHEMA = {db} GROUP BY ROUTINE_TYPE"
            ),
            format!(
                "SELECT 'trigger', COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = {db}"
            ),
            format!(
                "SELECT 'event', COUNT(*) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = {db}"
            ),
        ];
        if is_mariadb(server) {
            parts.push(format!(
                "SELECT 'sequence', COUNT(*) FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = {db} AND TABLE_TYPE = 'SEQUENCE'"
            ));
        }
        return Ok(Some(parts.join(" UNION ALL ")));
    };
    let table = lit(&parent.name)?;
    Ok(match parent.kind {
        ObjectKind::Table => Some(format!(
            "SELECT 'column' AS `kind`, COUNT(*) AS `n` FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {table} \
             UNION ALL SELECT 'index', COUNT(DISTINCT INDEX_NAME) FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {table} \
             UNION ALL SELECT 'constraint', COUNT(*) FROM information_schema.TABLE_CONSTRAINTS \
             WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {table} \
             UNION ALL SELECT 'trigger', COUNT(*) FROM information_schema.TRIGGERS \
             WHERE TRIGGER_SCHEMA = {db} AND EVENT_OBJECT_TABLE = {table} \
             UNION ALL SELECT 'partition', COUNT(DISTINCT PARTITION_NAME) FROM information_schema.PARTITIONS \
             WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {table} AND PARTITION_NAME IS NOT NULL"
        )),
        ObjectKind::View => Some(format!(
            "SELECT 'column' AS `kind`, COUNT(*) AS `n` FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {table}"
        )),
        ObjectKind::Partition => {
            let partition_table = lit(parent_of(parent)?)?;
            Some(format!(
                "SELECT 'partition' AS `kind`, COUNT(*) AS `n` FROM information_schema.PARTITIONS \
                 WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {partition_table} \
                 AND PARTITION_NAME = {table} AND SUBPARTITION_NAME IS NOT NULL"
            ))
        }
        _ => None,
    })
}

fn kind_of_count(name: &str) -> Option<ObjectKind> {
    Some(match name.to_ascii_lowercase().as_str() {
        "table" => ObjectKind::Table,
        "view" => ObjectKind::View,
        "procedure" => ObjectKind::Procedure,
        "function" => ObjectKind::Function,
        "package" => ObjectKind::Package,
        "package body" => ObjectKind::PackageBody,
        "trigger" => ObjectKind::Trigger,
        "event" => ObjectKind::Event,
        "sequence" => ObjectKind::Sequence,
        "column" => ObjectKind::Column,
        "index" => ObjectKind::Index,
        "constraint" => ObjectKind::Constraint,
        "partition" => ObjectKind::Partition,
        _ => return None,
    })
}

#[async_trait]
impl Catalog for MyCatalog {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn databases(&self, session: &dyn Session) -> CatalogResult<Vec<ObjectSummary>> {
        schema::databases(session, &self.server).await
    }

    async fn schemas(
        &self,
        _session: &dyn Session,
        _database: &str,
    ) -> CatalogResult<Vec<ObjectSummary>> {
        Ok(Vec::new())
    }

    async fn kinds(
        &self,
        session: &dyn Session,
        container: &Container,
        include_system: bool,
    ) -> CatalogResult<Vec<KindCount>> {
        let Some(database) = container.database.as_deref() else {
            let count = schema::databases(session, &self.server)
                .await?
                .iter()
                .filter(|d| include_system || !d.system)
                .count();
            return Ok(if count > 0 {
                vec![KindCount {
                    kind: ObjectKind::Database,
                    count: count as u64,
                }]
            } else {
                Vec::new()
            });
        };
        if is_system_db(database) && !include_system {
            return Ok(Vec::new());
        }
        let Some(sql) = counts_sql(&self.server, container)? else {
            return Ok(Vec::new());
        };
        let rows = util::query_one(session, &sql, &format!("database {database}")).await?;
        let mut counts: Vec<KindCount> = Vec::new();
        for row in rows.iter() {
            let Some(kind) = util::text(&row, "kind")?.as_deref().and_then(kind_of_count) else {
                continue;
            };
            let count = util::uint(&row, "n")?.unwrap_or(0);
            if count == 0 || !self.capabilities.kinds.contains(&kind) {
                continue;
            }
            match counts.iter_mut().find(|c| c.kind == kind) {
                Some(existing) => existing.count += count,
                None => counts.push(KindCount { kind, count }),
            }
        }
        counts.sort_by_key(|c| c.kind);
        Ok(counts)
    }

    async fn list(
        &self,
        session: &dyn Session,
        container: &Container,
        kind: ObjectKind,
        include_system: bool,
    ) -> CatalogResult<Vec<ObjectSummary>> {
        self.check_kind(kind)?;
        if kind == ObjectKind::Database {
            let mut all = schema::databases(session, &self.server).await?;
            all.retain(|d| include_system || !d.system);
            return Ok(all);
        }
        let database = container
            .database
            .as_deref()
            .ok_or_else(|| CatalogError::Invalid {
                message: format!("listing {kind:?} objects needs a database"),
            })?;
        if is_system_db(database) && !include_system {
            return Ok(Vec::new());
        }
        let parent = container.parent.as_ref();
        let parent_name = parent.map(|p| p.name.as_str());
        let needs_parent = || {
            parent_name.ok_or_else(|| CatalogError::Invalid {
                message: format!("listing {kind:?} objects needs the table as parent"),
            })
        };
        let server = &self.server;
        let mut out = match kind {
            ObjectKind::Table => schema::list_tables(session, database).await?,
            ObjectKind::View => programs::list_views(session, server, database).await?,
            ObjectKind::Procedure | ObjectKind::Function => {
                programs::list_routines(session, database, kind).await?
            }
            ObjectKind::Package | ObjectKind::PackageBody => {
                programs::list_packages(session, database, kind).await?
            }
            ObjectKind::Event => programs::list_events(session, database).await?,
            ObjectKind::Sequence => programs::list_sequences(session, database).await?,
            ObjectKind::Trigger => schema::load_triggers(session, server, database, parent_name)
                .await?
                .iter()
                .map(schema::trigger_summary)
                .collect(),
            ObjectKind::Column => {
                schema::load_columns(session, server, database, needs_parent()?, None)
                    .await?
                    .iter()
                    .map(|c| schema::column_summary(database, c))
                    .collect()
            }
            ObjectKind::Index => {
                schema::load_indexes(session, server, database, needs_parent()?, None)
                    .await?
                    .iter()
                    .map(|i| schema::index_summary(database, i))
                    .collect()
            }
            ObjectKind::Constraint => {
                schema::load_constraints(session, server, database, needs_parent()?, None)
                    .await?
                    .iter()
                    .map(|c| schema::constraint_summary(database, c))
                    .collect()
            }
            ObjectKind::Partition => {
                // Under a table: its partitions; under a partition: its
                // subpartitions.
                let (table, partition) = match parent {
                    Some(p) if p.kind == ObjectKind::Partition => {
                        (parent_of(p)?, Some(p.name.as_str()))
                    }
                    _ => (needs_parent()?, None),
                };
                let partitioning = schema::load_partitioning(session, database, table).await?;
                let partitions = partitioning.map(|p| p.partitions).unwrap_or_default();
                match partition {
                    None => partitions,
                    Some(name) => partitions
                        .into_iter()
                        .find(|p| p.name == name)
                        .map(|p| p.subpartitions)
                        .unwrap_or_default(),
                }
                .iter()
                .map(|p| schema::partition_summary(database, p))
                .collect()
            }
            other => return Err(self.unsupported(other)),
        };
        out.retain(|o| include_system || !o.system);
        Ok(out)
    }

    async fn describe(
        &self,
        session: &dyn Session,
        object: &ObjectRef,
    ) -> CatalogResult<ObjectDetails> {
        self.check_kind(object.kind)?;
        let server = &self.server;
        let name = object.name.as_str();
        let not_found = |what: &str| CatalogError::NotFound {
            message: format!("{what} {name} was not found"),
        };
        let details = match object.kind {
            ObjectKind::Database => {
                MyObject::Database(schema::describe_database(session, server, name).await?)
            }
            ObjectKind::Table => MyObject::Table(Box::new(
                schema::describe_table(session, server, database_of(object)?, name).await?,
            )),
            ObjectKind::View => MyObject::View(
                programs::describe_view(session, server, database_of(object)?, name).await?,
            ),
            ObjectKind::Column => MyObject::Column(
                schema::load_columns(
                    session,
                    server,
                    database_of(object)?,
                    parent_of(object)?,
                    Some(name),
                )
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| not_found("column"))?,
            ),
            ObjectKind::Index => MyObject::Index(
                schema::load_indexes(
                    session,
                    server,
                    database_of(object)?,
                    parent_of(object)?,
                    Some(name),
                )
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| not_found("index"))?,
            ),
            ObjectKind::Constraint => MyObject::Constraint(
                schema::load_constraints(
                    session,
                    server,
                    database_of(object)?,
                    parent_of(object)?,
                    Some(name),
                )
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| not_found("constraint"))?,
            ),
            ObjectKind::Partition => {
                let partitioning =
                    schema::load_partitioning(session, database_of(object)?, parent_of(object)?)
                        .await?
                        .ok_or_else(|| not_found("partition"))?;
                MyObject::Partition(
                    schema::find_partition(&partitioning, name)
                        .ok_or_else(|| not_found("partition"))?,
                )
            }
            ObjectKind::Procedure => MyObject::Procedure(
                programs::describe_routine(session, database_of(object)?, object.kind, name)
                    .await?,
            ),
            ObjectKind::Function => MyObject::Function(
                programs::describe_routine(session, database_of(object)?, object.kind, name)
                    .await?,
            ),
            ObjectKind::Trigger => MyObject::Trigger(
                schema::load_triggers(
                    session,
                    server,
                    database_of(object)?,
                    object.parent.as_deref(),
                )
                .await?
                .into_iter()
                .find(|t| t.name == name)
                .ok_or_else(|| not_found("trigger"))?,
            ),
            ObjectKind::Event => MyObject::Event(
                programs::describe_event(session, database_of(object)?, name).await?,
            ),
            ObjectKind::Sequence => MyObject::Sequence(
                programs::describe_sequence(session, server, database_of(object)?, name).await?,
            ),
            ObjectKind::Package => MyObject::Package(
                programs::describe_package(session, database_of(object)?, object.kind, name)
                    .await?,
            ),
            ObjectKind::PackageBody => MyObject::PackageBody(
                programs::describe_package(session, database_of(object)?, object.kind, name)
                    .await?,
            ),
            other => return Err(self.unsupported(other)),
        };
        Ok(ObjectDetails::Mysql(Box::new(details)))
    }

    async fn ddl(&self, session: &dyn Session, object: &ObjectRef) -> CatalogResult<String> {
        self.check_kind(object.kind)?;
        ddl::ddl(session, &self.server, object).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_core::Engine;

    fn server(engine: Engine, major: u32, minor: u32, patch: u32) -> ServerInfo {
        ServerInfo {
            engine,
            version: format!("{major}.{minor}.{patch}"),
            major,
            minor,
            patch: Some(patch),
        }
    }

    #[test]
    fn capabilities_follow_engine_and_version() {
        let mysql = MyCatalog::new(&server(Engine::Mysql, 8, 0, 22));
        let caps = mysql.capabilities();
        assert!(!caps.kinds.contains(&ObjectKind::Sequence));
        assert!(caps.features.contains(&"setPersist".to_owned()));
        assert!(caps.features.contains(&"databaseReadOnly".to_owned()));
        assert!(!caps.features.contains(&"invisibleColumns".to_owned()));
        assert!(caps.features.contains(&"invisibleIndexes".to_owned()));
        assert!(!caps.features.contains(&"ignoredIndexes".to_owned()));
        let mysql9 = MyCatalog::new(&server(Engine::Mysql, 9, 7, 0));
        assert!(
            mysql9
                .capabilities()
                .features
                .contains(&"invisibleColumns".to_owned())
        );
        assert!(
            mysql9
                .capabilities()
                .features
                .contains(&"explainAnalyze".to_owned())
        );

        let maria = MyCatalog::new(&server(Engine::Mariadb, 10, 6, 0));
        let caps = maria.capabilities();
        for kind in [
            ObjectKind::Sequence,
            ObjectKind::Package,
            ObjectKind::PackageBody,
        ] {
            assert!(caps.kinds.contains(&kind));
        }
        assert!(caps.editable.contains(&ObjectKind::Sequence));
        assert!(!caps.editable.contains(&ObjectKind::Package));
        for feature in [
            "systemVersioning",
            "ignoredIndexes",
            "invisibleColumns",
            "analyzeFormatJson",
            "sequences",
        ] {
            assert!(caps.features.contains(&feature.to_owned()), "{feature}");
        }
        assert!(!caps.features.contains(&"setPersist".to_owned()));
        assert!(!caps.features.contains(&"invisibleIndexes".to_owned()));
        assert!(!caps.features.contains(&"descendingIndexes".to_owned()));
    }

    #[test]
    fn objects_serialize_with_their_kind() -> Result<(), serde_json::Error> {
        let object = MyObject::Sequence(MySequence {
            database: "d".into(),
            name: "s".into(),
            data_type: None,
            start: "1".into(),
            increment: "1".into(),
            minimum: "1".into(),
            maximum: "9".into(),
            cache: "0".into(),
            cycle: false,
            next_value: "1".into(),
            cycle_count: "0".into(),
            last_value: None,
            comment: None,
            engine: None,
        });
        let json = serde_json::to_value(&object)?;
        assert_eq!(json["kind"], "sequence");
        assert_eq!(json["nextValue"], "1");
        let back: MyObject = serde_json::from_value(json)?;
        assert_eq!(back, object);
        Ok(())
    }

    #[test]
    fn count_names_map_to_kinds() {
        assert_eq!(kind_of_count("PACKAGE BODY"), Some(ObjectKind::PackageBody));
        assert_eq!(kind_of_count("PROCEDURE"), Some(ObjectKind::Procedure));
        assert_eq!(kind_of_count("x"), None);
    }
}
