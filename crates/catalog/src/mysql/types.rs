//! Details of MySQL and MariaDB objects, as [`super::MyObject`] carries
//! them. Fields that one engine or version does not have are `None` (or
//! `false`, or empty).

use serde::{Deserialize, Serialize};

/// A database (schema).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyDatabase {
    /// Name.
    pub name: String,
    /// Default character set.
    pub charset: Option<String>,
    /// Default collation.
    pub collation: Option<String>,
    /// Default table encryption (MySQL 8.0.16+).
    pub encryption: Option<bool>,
    /// Read-only schema (MySQL 8.0.22+).
    pub read_only: Option<bool>,
    /// Comment (MariaDB 10.5+).
    pub comment: Option<String>,
    /// Data and index size of its tables in bytes (server estimate).
    /// Filled by `describe`; `None` in lists.
    pub size_bytes: Option<u64>,
    /// Number of tables. Filled by `describe`; `None` in lists.
    pub table_count: Option<u64>,
    /// `mysql`, `sys`, `information_schema` or `performance_schema`.
    pub system: bool,
}

/// A base table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyTable {
    /// Database.
    pub database: String,
    /// Name.
    pub name: String,
    /// `BASE TABLE`, or MariaDB `SYSTEM VERSIONED`.
    pub table_type: String,
    /// Storage engine.
    pub engine: Option<String>,
    /// Row format.
    pub row_format: Option<String>,
    /// Next `AUTO_INCREMENT` value.
    pub auto_increment: Option<u64>,
    /// Table collation.
    pub collation: Option<String>,
    /// Character set of the table collation.
    pub charset: Option<String>,
    /// Comment.
    pub comment: Option<String>,
    /// Other table options as the server lists them (`CREATE_OPTIONS`).
    pub create_options: Option<String>,
    /// Estimated number of rows.
    pub row_estimate: Option<u64>,
    /// Average row length in bytes.
    pub avg_row_length: Option<u64>,
    /// Data size in bytes.
    pub data_length: Option<u64>,
    /// Index size in bytes.
    pub index_length: Option<u64>,
    /// Allocated but unused bytes.
    pub data_free: Option<u64>,
    /// Creation time.
    pub created: Option<String>,
    /// Last update time, when the engine tracks it.
    pub updated: Option<String>,
    /// MariaDB system versioning (`WITH SYSTEM VERSIONING`).
    pub system_versioning: Option<MySystemVersioning>,
    /// MariaDB application-time periods.
    pub periods: Vec<MyPeriod>,
    /// Columns, in order.
    pub columns: Vec<MyColumn>,
    /// Indexes.
    pub indexes: Vec<MyIndex>,
    /// Constraints.
    pub constraints: Vec<MyConstraint>,
    /// Partitioning, for partitioned tables.
    pub partitioning: Option<MyPartitioning>,
    /// Names of the table's triggers.
    pub triggers: Vec<String>,
    /// True for tables in a system database.
    pub system: bool,
}

/// MariaDB system versioning of a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MySystemVersioning {
    /// The `ROW START` column, when declared explicitly.
    pub row_start: Option<String>,
    /// The `ROW END` column, when declared explicitly.
    pub row_end: Option<String>,
}

/// A MariaDB application-time period (`PERIOD FOR name (start, end)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyPeriod {
    /// Period name.
    pub name: String,
    /// Start column.
    pub start_column: String,
    /// End column.
    pub end_column: String,
}

/// A column default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum MyDefault {
    /// `DEFAULT NULL`.
    Null,
    /// A literal value, unquoted (`abc` for `DEFAULT 'abc'`, `1.50`,
    /// `b'101'`).
    Literal(String),
    /// An expression (`CURRENT_TIMESTAMP`, `(now())`, `uuid()`).
    Expression(String),
}

/// How a generated column is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MyGeneratedKind {
    /// `VIRTUAL`: computed when read.
    Virtual,
    /// `STORED` (MariaDB also `PERSISTENT`): computed when written.
    Stored,
    /// MariaDB `GENERATED ALWAYS AS ROW START`.
    RowStart,
    /// MariaDB `GENERATED ALWAYS AS ROW END`.
    RowEnd,
}

/// A generated column's definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyGenerated {
    /// Virtual, stored, row start or row end.
    pub generated_kind: MyGeneratedKind,
    /// The expression (`None` for row start and row end).
    pub expression: Option<String>,
}

/// A column of a table or view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyColumn {
    /// Table or view.
    pub table: String,
    /// Name.
    pub name: String,
    /// 1-based position.
    pub position: u32,
    /// Base type (`varchar`, `int`, `longtext`).
    pub data_type: String,
    /// Full type (`varchar(10)`, `int unsigned`, `enum('a','b')`).
    pub column_type: String,
    /// Accepts `NULL`.
    pub nullable: bool,
    /// Default value; `None` when the column has none.
    pub default: Option<MyDefault>,
    /// `AUTO_INCREMENT`.
    pub auto_increment: bool,
    /// `ON UPDATE` expression (`CURRENT_TIMESTAMP`).
    pub on_update: Option<String>,
    /// Generated column definition.
    pub generated: Option<MyGenerated>,
    /// `INVISIBLE` (MySQL 8.0.23+, MariaDB 10.3+).
    pub invisible: bool,
    /// Character set.
    pub charset: Option<String>,
    /// Collation.
    pub collation: Option<String>,
    /// Comment.
    pub comment: Option<String>,
    /// Spatial reference system id (MySQL `SRID`, MariaDB `REF_SYSTEM_ID`).
    pub srid: Option<u32>,
    /// A JSON column: MySQL `json`, or MariaDB's `JSON` alias (`longtext`
    /// with a `json_valid` check).
    pub json: bool,
    /// MariaDB column-level `CHECK` (other than the JSON alias check).
    pub check: Option<String>,
    /// Key membership as the server reports it (`PRI`, `UNI`, `MUL`).
    pub key: Option<String>,
}

/// Index kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MyIndexKind {
    /// The primary key.
    Primary,
    /// A unique index.
    Unique,
    /// A plain index.
    Index,
    /// A `FULLTEXT` index.
    Fulltext,
    /// A `SPATIAL` index.
    Spatial,
}

/// One part of an index key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyKeyPart {
    /// Column, for column key parts.
    pub column: Option<String>,
    /// Expression, for MySQL functional key parts (8.0.13+).
    pub expression: Option<String>,
    /// Prefix length (`col(10)`).
    pub prefix_length: Option<u32>,
    /// `DESC` (MySQL 8.0, MariaDB 10.8+).
    pub descending: bool,
}

/// An index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyIndex {
    /// Table.
    pub table: String,
    /// Name (`PRIMARY` for the primary key).
    pub name: String,
    /// Primary, unique, plain, fulltext or spatial.
    pub index_kind: MyIndexKind,
    /// Method (`BTREE`, `HASH`) for primary, unique and plain indexes.
    pub method: Option<String>,
    /// Key parts in order.
    pub parts: Vec<MyKeyPart>,
    /// MySQL `INVISIBLE` index.
    pub invisible: bool,
    /// MariaDB `IGNORED` index (10.6+).
    pub ignored: bool,
    /// Comment.
    pub comment: Option<String>,
    /// Full-text parser plugin (`WITH PARSER ngram`); read in table details
    /// only (from `SHOW CREATE TABLE`).
    pub parser: Option<String>,
    /// The definition names the method (`USING BTREE` on a MEMORY table,
    /// where HASH is the default); read in table details only. A
    /// redefinition repeats `method` only then.
    pub method_explicit: bool,
}

/// Constraint types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MyConstraintType {
    /// `PRIMARY KEY`.
    PrimaryKey,
    /// `UNIQUE`.
    Unique,
    /// `FOREIGN KEY`.
    ForeignKey,
    /// `CHECK`.
    Check,
}

/// Where a MariaDB check constraint was declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MyCheckLevel {
    /// In the column definition (named after the column).
    Column,
    /// As a table constraint.
    Table,
}

/// The referenced side of a foreign key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyReference {
    /// Referenced database.
    pub database: String,
    /// Referenced table.
    pub table: String,
    /// Referenced columns, in key order.
    pub columns: Vec<String>,
    /// `ON UPDATE` action.
    pub on_update: Option<String>,
    /// `ON DELETE` action.
    pub on_delete: Option<String>,
    /// Match option.
    pub match_option: Option<String>,
}

/// A table constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyConstraint {
    /// Table.
    pub table: String,
    /// Name.
    pub name: String,
    /// Type.
    pub constraint_type: MyConstraintType,
    /// Columns, in key order (empty for checks).
    pub columns: Vec<String>,
    /// Referenced side, for foreign keys.
    pub references: Option<MyReference>,
    /// The check expression, for checks.
    pub check_clause: Option<String>,
    /// MySQL `ENFORCED` / `NOT ENFORCED` (8.0.16+).
    pub enforced: Option<bool>,
    /// MariaDB: where a check was declared.
    pub level: Option<MyCheckLevel>,
    /// MariaDB: the check that makes a `longtext` column the `JSON` alias.
    pub json: bool,
    /// MariaDB: the check (`start < end`) that an application-time period
    /// adds, named after the period; its DDL is the period's.
    pub period: bool,
}

/// Partitioning of a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyPartitioning {
    /// `RANGE`, `RANGE COLUMNS`, `LIST`, `LIST COLUMNS`, `HASH`,
    /// `LINEAR HASH`, `KEY`, `LINEAR KEY`, MariaDB `SYSTEM_TIME`.
    pub method: String,
    /// True for `LINEAR HASH` and `LINEAR KEY`.
    pub linear: bool,
    /// Partitioning expression or column list.
    pub expression: Option<String>,
    /// Subpartitioning method.
    pub subpartition_method: Option<String>,
    /// Subpartitioning expression.
    pub subpartition_expression: Option<String>,
    /// Partitions in order.
    pub partitions: Vec<MyPartition>,
}

/// A partition or subpartition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyPartition {
    /// Table.
    pub table: String,
    /// Name.
    pub name: String,
    /// For a subpartition: its partition.
    pub partition: Option<String>,
    /// 1-based position among its siblings.
    pub position: u32,
    /// Method of its level (`RANGE`, `HASH`, ...).
    pub method: Option<String>,
    /// Expression of its level.
    pub expression: Option<String>,
    /// The server's description of its values (`10`, `1,2,3`,
    /// `MAXVALUE`, MariaDB `CURRENT`).
    pub description: Option<String>,
    /// The values clause (`VALUES LESS THAN (10)`, `VALUES IN (1,2)`,
    /// `HISTORY`, `CURRENT`).
    pub values: Option<String>,
    /// Estimated rows.
    pub rows: Option<u64>,
    /// Data size in bytes.
    pub data_length: Option<u64>,
    /// Index size in bytes.
    pub index_length: Option<u64>,
    /// Comment.
    pub comment: Option<String>,
    /// Tablespace.
    pub tablespace: Option<String>,
    /// Subpartitions in order.
    pub subpartitions: Vec<MyPartition>,
}

/// A view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyView {
    /// Database.
    pub database: String,
    /// Name.
    pub name: String,
    /// The query, with qualified names.
    pub definition: Option<String>,
    /// `UNDEFINED`, `MERGE` or `TEMPTABLE`.
    pub algorithm: Option<String>,
    /// `DEFINER` or `INVOKER`.
    pub security: Option<String>,
    /// Definer account.
    pub definer: Option<String>,
    /// `NONE`, `LOCAL` or `CASCADED`.
    pub check_option: Option<String>,
    /// Whether the view is updatable.
    pub updatable: Option<bool>,
    /// Client character set when created.
    pub charset_client: Option<String>,
    /// Connection collation when created.
    pub collation_connection: Option<String>,
    /// Columns, in order.
    pub columns: Vec<MyColumn>,
    /// True for system views.
    pub system: bool,
}

/// A routine parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyParameter {
    /// 1-based position.
    pub position: u32,
    /// `IN`, `OUT` or `INOUT` (procedures).
    pub mode: Option<String>,
    /// Name.
    pub name: Option<String>,
    /// Full type.
    pub data_type: String,
}

/// A stored procedure or function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyRoutine {
    /// Database.
    pub database: String,
    /// Name.
    pub name: String,
    /// Parameters in order.
    pub parameters: Vec<MyParameter>,
    /// Return type, for functions.
    pub returns: Option<String>,
    /// `DETERMINISTIC`.
    pub deterministic: bool,
    /// `CONTAINS SQL`, `NO SQL`, `READS SQL DATA`, `MODIFIES SQL DATA`.
    pub data_access: Option<String>,
    /// `DEFINER` or `INVOKER`.
    pub security: Option<String>,
    /// Definer account.
    pub definer: Option<String>,
    /// SQL mode the routine was created with.
    pub sql_mode: Option<String>,
    /// Comment.
    pub comment: Option<String>,
    /// Body.
    pub body: Option<String>,
    /// Creation time.
    pub created: Option<String>,
    /// Last change.
    pub altered: Option<String>,
    /// Client character set when created.
    pub charset_client: Option<String>,
    /// Connection collation when created.
    pub collation_connection: Option<String>,
    /// Database collation when created.
    pub database_collation: Option<String>,
}

/// A trigger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyTrigger {
    /// Database.
    pub database: String,
    /// Table.
    pub table: String,
    /// Name.
    pub name: String,
    /// `BEFORE` or `AFTER`.
    pub timing: String,
    /// `INSERT`, `UPDATE` or `DELETE`.
    pub event: String,
    /// 1-based order among the table's triggers with the same timing and
    /// event.
    pub action_order: u32,
    /// The trigger that runs right before this one (same table and timing,
    /// a shared event), when there is one.
    pub follows: Option<String>,
    /// The trigger that runs right after this one, when there is one. The
    /// DDL re-creates the trigger with `PRECEDES` this trigger (or, for a
    /// MariaDB trigger with more events than this one has, `FOLLOWS` the
    /// previous one).
    pub precedes: Option<String>,
    /// Definer account.
    pub definer: Option<String>,
    /// SQL mode it was created with.
    pub sql_mode: Option<String>,
    /// Body.
    pub body: Option<String>,
    /// Creation time.
    pub created: Option<String>,
    /// Client character set when created.
    pub charset_client: Option<String>,
    /// Connection collation when created.
    pub collation_connection: Option<String>,
    /// Database collation when created.
    pub database_collation: Option<String>,
}

/// When an event runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum MySchedule {
    /// Once, `AT` a time.
    #[serde(rename_all = "camelCase")]
    At {
        /// The time, in the event's time zone.
        at: Option<String>,
    },
    /// Repeatedly, `EVERY` interval.
    #[serde(rename_all = "camelCase")]
    Every {
        /// Interval amount (`1`, `'1:30'`).
        interval_value: Option<String>,
        /// Interval unit (`DAY`, `HOUR_MINUTE`).
        interval_field: Option<String>,
        /// `STARTS` time.
        starts: Option<String>,
        /// `ENDS` time.
        ends: Option<String>,
    },
}

/// Whether an event runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MyEventStatus {
    /// `ENABLE`.
    Enabled,
    /// `DISABLE`.
    Disabled,
    /// `DISABLE ON SLAVE` / `DISABLE ON REPLICA`: created on a source and
    /// replicated, not run here.
    SlavesideDisabled,
}

/// State of the server's event scheduler (`@@event_scheduler`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MyEventScheduler {
    /// Running: enabled events run.
    On,
    /// Stopped; can be started with `SET GLOBAL event_scheduler = ON`.
    Off,
    /// Disabled at server start; cannot be started at runtime.
    Disabled,
}

/// A scheduled event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyEvent {
    /// Database.
    pub database: String,
    /// Name.
    pub name: String,
    /// Definer account.
    pub definer: Option<String>,
    /// Time zone of the schedule.
    pub time_zone: Option<String>,
    /// Schedule.
    pub schedule: MySchedule,
    /// Enabled, disabled or disabled on the replica.
    pub status: MyEventStatus,
    /// `ON COMPLETION PRESERVE`.
    pub preserve: bool,
    /// SQL mode it was created with.
    pub sql_mode: Option<String>,
    /// Comment.
    pub comment: Option<String>,
    /// Body.
    pub body: Option<String>,
    /// Creation time.
    pub created: Option<String>,
    /// Last change.
    pub altered: Option<String>,
    /// Last run.
    pub last_executed: Option<String>,
    /// Server id of the server that created it.
    pub originator: Option<u64>,
    /// The server's event scheduler state: events only run while it is on.
    pub scheduler: MyEventScheduler,
}

/// A MariaDB sequence. Values are decimal text, because MariaDB 11.5+
/// sequences can be unsigned 64-bit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MySequence {
    /// Database.
    pub database: String,
    /// Name.
    pub name: String,
    /// Value type (MariaDB 11.5+); `bigint` before.
    pub data_type: Option<String>,
    /// `START WITH`.
    pub start: String,
    /// `INCREMENT BY`.
    pub increment: String,
    /// `MINVALUE`.
    pub minimum: String,
    /// `MAXVALUE`.
    pub maximum: String,
    /// `CACHE` size.
    pub cache: String,
    /// `CYCLE`.
    pub cycle: bool,
    /// The next value not yet handed out to any cache: what `NEXTVAL`
    /// returns next when no server cache holds values.
    pub next_value: String,
    /// How often the sequence has wrapped around.
    pub cycle_count: String,
    /// `LASTVAL()` in this session: the last value this session drew, or
    /// `None` when it drew none.
    pub last_value: Option<String>,
    /// Comment.
    pub comment: Option<String>,
    /// Storage engine.
    pub engine: Option<String>,
}

/// A MariaDB package specification or package body (Oracle mode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyPackage {
    /// Database.
    pub database: String,
    /// Name.
    pub name: String,
    /// Definer account.
    pub definer: Option<String>,
    /// `DEFINER` or `INVOKER`.
    pub security: Option<String>,
    /// SQL mode it was created with.
    pub sql_mode: Option<String>,
    /// Comment.
    pub comment: Option<String>,
    /// The specification or body text.
    pub body: Option<String>,
    /// Creation time.
    pub created: Option<String>,
    /// Last change.
    pub altered: Option<String>,
}
