//! Engine-neutral catalog model and the [`Catalog`] trait.
//!
//! The object tree is loaded lazily, one level at a time: databases, then
//! (PostgreSQL) schemas, then the object kinds present in a container with
//! their counts, then the objects of one kind, then the children of an
//! object (indexes, triggers, partitions, ...). Details and editors are
//! engine specific on purpose (design doc 4.1): [`ObjectDetails`] carries
//! the engine's own structures.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use varve_core::Engine;
use varve_driver::{DriverError, ServerInfo, Session};

/// Every kind of object the browser can show, across engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectKind {
    /// A database (PostgreSQL database, MySQL/MariaDB schema).
    Database,
    /// A PostgreSQL schema.
    Schema,
    /// A table (for PostgreSQL also unlogged tables).
    Table,
    /// A PostgreSQL partitioned table.
    PartitionedTable,
    /// A partition of a partitioned table.
    Partition,
    /// A view.
    View,
    /// A PostgreSQL materialized view.
    MaterializedView,
    /// A column of a table or view.
    Column,
    /// An index.
    Index,
    /// A table constraint (primary key, unique, check, exclusion, foreign key).
    Constraint,
    /// A sequence (PostgreSQL, MariaDB).
    Sequence,
    /// A function.
    Function,
    /// A procedure.
    Procedure,
    /// A PostgreSQL aggregate function.
    Aggregate,
    /// A trigger.
    Trigger,
    /// A PostgreSQL event trigger.
    EventTrigger,
    /// A PostgreSQL enum type.
    EnumType,
    /// A PostgreSQL composite type.
    CompositeType,
    /// A PostgreSQL domain.
    Domain,
    /// A PostgreSQL range type.
    RangeType,
    /// A PostgreSQL extension.
    Extension,
    /// A PostgreSQL rewrite rule.
    Rule,
    /// A PostgreSQL row level security policy.
    Policy,
    /// A PostgreSQL publication.
    Publication,
    /// A PostgreSQL subscription.
    Subscription,
    /// A PostgreSQL foreign data wrapper.
    ForeignDataWrapper,
    /// A PostgreSQL foreign server.
    ForeignServer,
    /// A PostgreSQL foreign table.
    ForeignTable,
    /// A PostgreSQL tablespace.
    Tablespace,
    /// A PostgreSQL collation.
    Collation,
    /// A PostgreSQL cast.
    Cast,
    /// A PostgreSQL operator.
    Operator,
    /// A MySQL/MariaDB scheduled event.
    Event,
    /// A MariaDB package (Oracle mode).
    Package,
    /// A MariaDB package body (Oracle mode).
    PackageBody,
}

/// Points at one object. Fields that do not apply to an engine or kind are
/// `None`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectRef {
    /// What it is.
    pub kind: ObjectKind,
    /// Database (PostgreSQL database, MySQL/MariaDB schema). `None` only for
    /// server-wide objects such as tablespaces.
    pub database: Option<String>,
    /// PostgreSQL schema.
    pub schema: Option<String>,
    /// Name of the table (or view) the object belongs to, for columns,
    /// indexes, constraints, triggers, rules, policies and partitions.
    pub parent: Option<String>,
    /// The object's own name.
    pub name: String,
    /// Identity arguments of an overloaded routine (PostgreSQL
    /// `pg_get_function_identity_arguments`), or a cast's `source AS target`.
    pub signature: Option<String>,
}

/// A place in the tree whose objects are listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    /// Database; `None` lists server-wide objects.
    pub database: Option<String>,
    /// PostgreSQL schema.
    pub schema: Option<String>,
    /// The object whose children are listed (a table for its indexes, a
    /// partitioned table for its partitions).
    pub parent: Option<ObjectRef>,
}

/// One entry of the object tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectSummary {
    /// The object.
    pub object: ObjectRef,
    /// Comment on the object, when it has one.
    pub comment: Option<String>,
    /// Short secondary text for the tree (row estimate, size, language,
    /// index method, ...).
    pub detail: Option<String>,
    /// Short labels shown as badges (`unlogged`, `invisible`, `disabled`,
    /// `system versioned`, ...).
    pub flags: Vec<String>,
    /// True when the object has children (partitions, indexes, ...).
    pub has_children: bool,
    /// True for objects owned by the system or an extension; hidden unless
    /// the connection shows system objects.
    pub system: bool,
    /// Estimated number of rows, from the same list query (no extra query
    /// per object). PostgreSQL: `pg_class.reltuples` for tables, leaf
    /// partitions and materialized views (`None` before the first `VACUUM`
    /// or `ANALYZE` on 14 and newer; on 13 such a table reports 0, as an
    /// empty analyzed one does); `None` for partitioned tables (their rows
    /// are their partitions'), views, foreign tables and every other kind.
    /// MySQL / MariaDB: `information_schema.TABLES.TABLE_ROWS` for tables
    /// (exact for MyISAM, an estimate for InnoDB; for a partitioned table
    /// the server's sum over its partitions) and `PARTITIONS.TABLE_ROWS`
    /// for partitions; `None` for views and every other kind.
    #[serde(default)]
    pub row_estimate: Option<u64>,
    /// Size on disk in bytes, from the same list query, for the same
    /// objects. PostgreSQL: the pages of the relation, its TOAST table and
    /// its indexes from `pg_class.relpages` times the block size (as of the
    /// last `VACUUM`, `ANALYZE` or `CREATE INDEX`; `pg_total_relation_size`
    /// would wait behind a session holding the table locked). MySQL /
    /// MariaDB: `DATA_LENGTH + INDEX_LENGTH` (for a partitioned table the
    /// server's sum over its partitions). `None` for every other object.
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// How many objects of one kind a container holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KindCount {
    /// The kind.
    pub kind: ObjectKind,
    /// Number of objects (system objects excluded unless requested).
    pub count: u64,
}

/// What the connected server supports (design doc 4.1): the UI renders only
/// what is listed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// Engine the capabilities are for.
    pub engine: Engine,
    /// Kinds the browser can show on this server version.
    pub kinds: Vec<ObjectKind>,
    /// Kinds that have a dedicated editor (create, alter, drop).
    pub editable: Vec<ObjectKind>,
    /// Named features, for example `explainAnalyze`, `explainJson`,
    /// `refreshConcurrently`, `invisibleColumns`, `systemVersioning`,
    /// `setPersist`, `processList`, `serverVariables`, `galera`.
    pub features: Vec<String>,
}

/// Engine-specific details of one object, for the detail view and editors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "engine", content = "object", rename_all = "camelCase")]
pub enum ObjectDetails {
    /// A PostgreSQL object.
    Postgres(Box<crate::postgres::PgObject>),
    /// A MySQL or MariaDB object.
    Mysql(Box<crate::mysql::MyObject>),
}

/// SQL an editor produced for a change, shown to the user before it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditPlan {
    /// Statements in the order they must run, each a complete statement
    /// without a trailing `;`.
    pub statements: Vec<String>,
    /// True when the statements can and should run in one transaction
    /// (PostgreSQL DDL; never MySQL DDL, which commits implicitly, and never
    /// statements such as `CREATE INDEX CONCURRENTLY` or `VACUUM`).
    pub transactional: bool,
    /// True when the plan drops or rewrites data or objects (drop, type
    /// change that can lose data, truncate, ...): the UI asks for
    /// confirmation regardless of the environment.
    pub destructive: bool,
    /// Warnings for the user, for example "rewrites the whole table" or
    /// "takes an ACCESS EXCLUSIVE lock".
    pub warnings: Vec<String>,
}

/// A catalog failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CatalogError {
    /// The server returned an error.
    #[error("{error}")]
    Driver {
        /// The driver error.
        error: DriverError,
    },
    /// The object does not exist (any more).
    #[error("{message}")]
    NotFound {
        /// What was not found.
        message: String,
    },
    /// The engine or server version does not support this.
    #[error("{message}")]
    Unsupported {
        /// What is not supported.
        message: String,
    },
    /// The request is not valid (for example an editor change that cannot
    /// be expressed in SQL).
    #[error("{message}")]
    Invalid {
        /// What is wrong.
        message: String,
    },
    /// `EXPLAIN ANALYZE` would run a statement Varve cannot prove free of
    /// side effects (it changes data, calls functions a rollback may not
    /// undo, runs inside the session's transaction, ...): it runs only once
    /// the user confirmed ([`crate::explain::ExplainOptions::confirmed`]).
    #[error("{message}")]
    NeedsConfirmation {
        /// The whole explanation, for logs and plain error displays.
        message: String,
        /// Each reason on its own, for the confirmation dialog.
        warnings: Vec<String>,
    },
    /// The server answered with something the catalog did not expect.
    #[error("unexpected catalog result: {message}")]
    Unexpected {
        /// What was unexpected.
        message: String,
    },
}

impl From<DriverError> for CatalogError {
    fn from(error: DriverError) -> Self {
        Self::Driver { error }
    }
}

/// Result of catalog calls.
pub type CatalogResult<T> = Result<T, CatalogError>;

/// The catalog of one engine. Calls take the session to run on; the
/// catalog keeps no connection of its own.
#[async_trait]
pub trait Catalog: Send + Sync {
    /// What the server supports.
    fn capabilities(&self) -> &Capabilities;

    /// Databases on the server.
    async fn databases(&self, session: &dyn Session) -> CatalogResult<Vec<ObjectSummary>>;

    /// PostgreSQL schemas of a database (empty for other engines).
    async fn schemas(
        &self,
        session: &dyn Session,
        database: &str,
    ) -> CatalogResult<Vec<ObjectSummary>>;

    /// The kinds present in a container, with counts.
    async fn kinds(
        &self,
        session: &dyn Session,
        container: &Container,
        include_system: bool,
    ) -> CatalogResult<Vec<KindCount>>;

    /// The objects of one kind in a container.
    async fn list(
        &self,
        session: &dyn Session,
        container: &Container,
        kind: ObjectKind,
        include_system: bool,
    ) -> CatalogResult<Vec<ObjectSummary>>;

    /// Full details of one object.
    async fn describe(
        &self,
        session: &dyn Session,
        object: &ObjectRef,
    ) -> CatalogResult<ObjectDetails>;

    /// The DDL that creates the object, as the server would accept it.
    async fn ddl(&self, session: &dyn Session, object: &ObjectRef) -> CatalogResult<String>;
}

/// The catalog for a connected session, including features that depend on
/// server settings rather than the version (MySQL/MariaDB event scheduler
/// state, MariaDB Galera). Prefer this over [`catalog_for`].
///
/// # Errors
///
/// [`CatalogError::Unsupported`] for Redis, and driver errors from the
/// probe.
pub async fn open_catalog(session: &dyn Session) -> CatalogResult<Box<dyn Catalog>> {
    let server = session.server_info();
    match server.engine {
        Engine::Mysql | Engine::Mariadb => {
            Ok(Box::new(crate::mysql::MyCatalog::probe(session).await?))
        }
        _ => catalog_for(server),
    }
}

/// The catalog for a server from its version alone. Settings-dependent
/// features are missing; [`open_catalog`] adds them.
///
/// # Errors
///
/// [`CatalogError::Unsupported`] for Redis.
pub fn catalog_for(server: &ServerInfo) -> CatalogResult<Box<dyn Catalog>> {
    match server.engine {
        Engine::Postgres => Ok(Box::new(crate::postgres::PgCatalog::new(server))),
        Engine::Mysql | Engine::Mariadb => Ok(Box::new(crate::mysql::MyCatalog::new(server))),
        Engine::Redis => Err(CatalogError::Unsupported {
            message: "Redis keys are browsed with the key browser".into(),
        }),
    }
}
