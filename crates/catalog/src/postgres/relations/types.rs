//! Detail structures of the PostgreSQL relations catalog. Everything here is
//! read from the system catalogs as the server reports it; SQL fragments
//! (types, expressions, definitions) are the server's own `format_type` and
//! `pg_get_*` output, schema qualified.

use serde::{Deserialize, Serialize};

/// A schema qualified name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgQualifiedName {
    /// Schema.
    pub schema: String,
    /// Name in the schema.
    pub name: String,
}

/// One `name=value` entry of an option list (storage parameters, foreign
/// data options, tablespace options, attribute options).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgOption {
    /// Option name; table storage parameters of the TOAST table are
    /// prefixed with `toast.`.
    pub name: String,
    /// Option value as stored.
    pub value: String,
}

/// A configuration setting stored for a database (`ALTER DATABASE ... SET`)
/// or for a role in a database (`ALTER ROLE ... IN DATABASE ... SET`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgSetting {
    /// The role the setting is for; `None` for every role.
    pub role: Option<String>,
    /// Parameter name.
    pub name: String,
    /// Value as stored by the server (list parameters such as
    /// `search_path` keep their element quoting).
    pub value: String,
}

/// A database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgDatabase {
    /// Name.
    pub name: String,
    /// Owner role.
    pub owner: String,
    /// Character encoding, for example `UTF8`.
    pub encoding: String,
    /// `LC_COLLATE`.
    pub collation: String,
    /// `LC_CTYPE`.
    pub ctype: String,
    /// Locale provider: `libc`, `icu` (15+) or `builtin` (17+). Always
    /// `libc` before PostgreSQL 15.
    pub locale_provider: String,
    /// ICU locale (15+) or builtin locale (17+), when the provider uses one.
    pub locale: Option<String>,
    /// ICU tailoring rules (16+).
    pub icu_rules: Option<String>,
    /// Collation version recorded for the database (15+).
    pub collation_version: Option<String>,
    /// True when the database can be used as a `TEMPLATE`.
    pub is_template: bool,
    /// False when connections are refused (`ALLOW_CONNECTIONS false`).
    pub allow_connections: bool,
    /// Connection limit, `-1` for none (`-2` marks a database left invalid
    /// by an interrupted `DROP DATABASE`, 15+).
    pub connection_limit: i64,
    /// Default tablespace.
    pub tablespace: String,
    /// Size on disk in bytes; `None` without `CONNECT` privilege.
    pub size_bytes: Option<i64>,
    /// Stored configuration settings.
    pub settings: Vec<PgSetting>,
    /// Comment.
    pub comment: Option<String>,
    /// True for `template0` and `template1`.
    pub system: bool,
}

/// A schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgSchema {
    /// Name.
    pub name: String,
    /// Owner role.
    pub owner: String,
    /// Extension that owns the schema.
    pub extension: Option<String>,
    /// Comment.
    pub comment: Option<String>,
    /// True for `pg_catalog`, `information_schema`, TOAST and temporary
    /// schemas and extension-owned schemas.
    pub system: bool,
}

/// A tablespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgTablespace {
    /// Name.
    pub name: String,
    /// Owner role.
    pub owner: String,
    /// Directory; `None` for the built-in `pg_default` and `pg_global`.
    pub location: Option<String>,
    /// Options (`seq_page_cost`, `random_page_cost`, ...).
    pub options: Vec<PgOption>,
    /// Size on disk in bytes; `None` without the privilege to read it.
    pub size_bytes: Option<i64>,
    /// Comment.
    pub comment: Option<String>,
    /// True for `pg_default` and `pg_global`.
    pub system: bool,
}

/// How a partitioned table is partitioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgPartitioning {
    /// `range`, `list` or `hash`.
    pub strategy: String,
    /// The partition key as `pg_get_partkeydef` prints it, for example
    /// `RANGE (created_at)`.
    pub key: String,
    /// The partitions, ordered by name.
    pub partitions: Vec<PgPartitionEntry>,
    /// The default partition, when there is one.
    pub default_partition: Option<PgQualifiedName>,
}

/// One partition of a partitioned table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgPartitionEntry {
    /// Schema of the partition.
    pub schema: String,
    /// Name of the partition.
    pub name: String,
    /// Bound as `pg_get_expr` prints it: `FOR VALUES ...` or `DEFAULT`.
    pub bound: String,
    /// True for the default partition.
    pub is_default: bool,
    /// True when the partition is itself partitioned.
    pub partitioned: bool,
    /// True when the partition is a foreign table.
    pub foreign: bool,
    /// True while `DETACH PARTITION ... CONCURRENTLY` is pending (14+).
    pub detach_pending: bool,
}

/// Where a partition is attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgPartitionOf {
    /// The partitioned table.
    pub parent: PgQualifiedName,
    /// Bound as `pg_get_expr` prints it: `FOR VALUES ...` or `DEFAULT`.
    pub bound: String,
    /// True for the default partition.
    pub is_default: bool,
    /// True while `DETACH PARTITION ... CONCURRENTLY` is pending (14+).
    pub detach_pending: bool,
}

/// A foreign data wrapper option of a foreign table or column, shared with
/// the programs catalog: `value` is `None` when it is hidden because it
/// may hold a secret (see [`crate::postgres::programs::redact_option`]).
pub use crate::postgres::programs::PgOption as PgFdwOption;

/// Foreign table specifics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgForeignInfo {
    /// Foreign server.
    pub server: String,
    /// Table options for the foreign data wrapper; values that may hold a
    /// secret are hidden.
    pub options: Vec<PgFdwOption>,
}

/// A table: a regular or unlogged table, a partitioned table, a partition
/// or a foreign table, with everything that hangs off it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgTable {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Owner role.
    pub owner: String,
    /// True for an `UNLOGGED` table.
    pub unlogged: bool,
    /// The composite type of a typed table (`CREATE TABLE ... OF type`).
    pub of_type: Option<String>,
    /// Table access method (`heap`, ...); `None` when the relation has none
    /// (partitioned tables before 17, foreign tables).
    pub access_method: Option<String>,
    /// Tablespace; `None` for the database default.
    pub tablespace: Option<String>,
    /// Storage parameters, TOAST ones prefixed with `toast.`.
    pub storage_parameters: Vec<PgOption>,
    /// Parents in `INHERITS`, in inheritance order (not the partition
    /// parent).
    pub inherits: Vec<PgQualifiedName>,
    /// Partitioning, for a partitioned table.
    pub partitioning: Option<PgPartitioning>,
    /// Partition parent and bound, for a partition.
    pub partition_of: Option<PgPartitionOf>,
    /// Server and options, for a foreign table.
    pub foreign: Option<PgForeignInfo>,
    /// Planner row estimate (`reltuples`); `None` before the first
    /// `ANALYZE` (14+ reports that explicitly).
    pub row_estimate: Option<i64>,
    /// Total size including TOAST and indexes, in bytes: exact
    /// (`pg_total_relation_size`) in `describe`, `None` with
    /// [`PgTable::size_note`] when the table is locked.
    pub total_bytes: Option<i64>,
    /// Size of the table and its TOAST table, in bytes (as `total_bytes`).
    pub table_bytes: Option<i64>,
    /// Size of the indexes, in bytes (as `total_bytes`).
    pub index_bytes: Option<i64>,
    /// Why the sizes are missing: another session holds the table in a
    /// lock mode the size functions would wait for.
    pub size_note: Option<String>,
    /// Row level security is enabled.
    pub rls_enabled: bool,
    /// Row level security also applies to the owner.
    pub rls_forced: bool,
    /// Replica identity: `default`, `nothing`, `full` or `index`.
    pub replica_identity: String,
    /// Comment.
    pub comment: Option<String>,
    /// True for system and extension-owned tables.
    pub system: bool,
    /// Columns in order.
    pub columns: Vec<PgColumn>,
    /// Constraints.
    pub constraints: Vec<PgConstraint>,
    /// Indexes.
    pub indexes: Vec<PgIndex>,
    /// Triggers (internal ones excluded).
    pub triggers: Vec<PgTrigger>,
    /// Rewrite rules.
    pub rules: Vec<PgRule>,
    /// Row level security policies.
    pub policies: Vec<PgPolicy>,
    /// Sequences owned by columns of this table (`serial` columns and
    /// `OWNED BY`), identity sequences excluded.
    pub owned_sequences: Vec<PgSequence>,
}

/// A view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgView {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Owner role.
    pub owner: String,
    /// The query (`pg_get_viewdef`), without a trailing semicolon.
    pub definition: String,
    /// `local` or `cascaded` when the view has a check option.
    pub check_option: Option<String>,
    /// `security_barrier` is set.
    pub security_barrier: bool,
    /// `security_invoker` is set (15+).
    pub security_invoker: bool,
    /// Every view option as stored.
    pub options: Vec<PgOption>,
    /// Comment.
    pub comment: Option<String>,
    /// True for system and extension-owned views.
    pub system: bool,
    /// Columns in order.
    pub columns: Vec<PgColumn>,
    /// `INSTEAD OF` and statement triggers.
    pub triggers: Vec<PgTrigger>,
    /// Rewrite rules other than the view's own `_RETURN` rule.
    pub rules: Vec<PgRule>,
}

/// A materialized view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgMaterializedView {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Owner role.
    pub owner: String,
    /// The query (`pg_get_viewdef`), without a trailing semicolon.
    pub definition: String,
    /// False after `WITH NO DATA` until the first refresh.
    pub populated: bool,
    /// Table access method.
    pub access_method: Option<String>,
    /// Tablespace; `None` for the database default.
    pub tablespace: Option<String>,
    /// Storage parameters, TOAST ones prefixed with `toast.`.
    pub storage_parameters: Vec<PgOption>,
    /// Planner row estimate; `None` before the first `ANALYZE`.
    pub row_estimate: Option<i64>,
    /// Total size including TOAST and indexes, in bytes; `None` with
    /// [`PgMaterializedView::size_note`] when the view is locked.
    pub total_bytes: Option<i64>,
    /// Why the size is missing (the view is locked by another session).
    pub size_note: Option<String>,
    /// PostgreSQL does not record when a materialized view was last
    /// refreshed; this says so for the detail view.
    pub last_refresh_note: String,
    /// True when `REFRESH ... CONCURRENTLY` is possible: the view is
    /// populated and has a valid unique index without a predicate on
    /// plain columns only.
    pub refresh_concurrently: bool,
    /// Comment.
    pub comment: Option<String>,
    /// True for system and extension-owned views.
    pub system: bool,
    /// Columns in order.
    pub columns: Vec<PgColumn>,
    /// Indexes.
    pub indexes: Vec<PgIndex>,
}

/// An identity column's generation and sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgIdentity {
    /// `always` or `byDefault`.
    pub generation: String,
    /// The identity sequence, when the server has one for the column (a
    /// partition's inherited identity uses the parent's sequence).
    pub sequence: Option<PgQualifiedName>,
    /// `START WITH` (64-bit values travel as decimal text).
    #[serde(with = "crate::postgres::edit::int_text::option")]
    pub start: Option<i64>,
    /// `INCREMENT BY` (decimal text).
    #[serde(with = "crate::postgres::edit::int_text::option")]
    pub increment: Option<i64>,
    /// `MINVALUE` (decimal text).
    #[serde(with = "crate::postgres::edit::int_text::option")]
    pub min: Option<i64>,
    /// `MAXVALUE` (decimal text).
    #[serde(with = "crate::postgres::edit::int_text::option")]
    pub max: Option<i64>,
    /// `CACHE` (decimal text).
    #[serde(with = "crate::postgres::edit::int_text::option")]
    pub cache: Option<i64>,
    /// `CYCLE`.
    pub cycle: Option<bool>,
}

/// A generated column's expression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgGenerated {
    /// The generation expression.
    pub expression: String,
    /// True for `STORED`, false for `VIRTUAL` (18+).
    pub stored: bool,
}

/// A column of a table, view, materialized view or foreign table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgColumn {
    /// Schema of the relation.
    pub schema: String,
    /// The relation.
    pub table: String,
    /// Column name.
    pub name: String,
    /// Position (`attnum`), 1-based; gaps mark dropped columns.
    pub position: i64,
    /// Type with modifiers as `format_type` prints it, for example
    /// `character varying(20)` or `numeric(10,2)`.
    pub data_type: String,
    /// `NOT NULL`.
    pub not_null: bool,
    /// The not-null constraint's name (18+, where not-null constraints are
    /// catalogued).
    pub not_null_constraint: Option<String>,
    /// Default expression (not for generated columns).
    pub default: Option<String>,
    /// Identity settings for an identity column.
    pub identity: Option<PgIdentity>,
    /// Generation expression for a generated column.
    pub generated: Option<PgGenerated>,
    /// Collation, when it differs from the type's default.
    pub collation: Option<PgQualifiedName>,
    /// Storage: `plain`, `external`, `extended` or `main`.
    pub storage: String,
    /// The type's default storage.
    pub default_storage: String,
    /// Compression method `pglz` or `lz4` (14+); `None` for the default.
    pub compression: Option<String>,
    /// Statistics target; `None` for the default.
    pub statistics_target: Option<i64>,
    /// Attribute options (`n_distinct`, ...).
    pub options: Vec<PgOption>,
    /// Foreign data wrapper options of a foreign table column; values that
    /// may hold a secret are hidden.
    pub fdw_options: Vec<PgFdwOption>,
    /// Defined locally (false for a purely inherited column).
    pub local: bool,
    /// Number of parents the column is inherited from.
    pub inherited_count: i64,
    /// Comment.
    pub comment: Option<String>,
}

/// One key column or expression of an index, with everything needed to
/// build the same key again (`CREATE INDEX` element).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgIndexKey {
    /// The key as `pg_get_indexdef` writes it, for display: the quoted
    /// column name, or the expression (parenthesized unless it is a
    /// function call).
    pub definition: String,
    /// The column name as stored (not quoted) for a column key; `None` for
    /// an expression. Send this back as the column, never `definition`.
    pub column: Option<String>,
    /// True for an expression.
    pub expression: bool,
    /// `DESC`.
    pub descending: bool,
    /// `NULLS FIRST`.
    pub nulls_first: bool,
    /// `COLLATE`, when it is not the collation the key has anyway (the
    /// column's collation, or the default collation of the expression's
    /// type).
    pub collation: Option<PgQualifiedName>,
    /// Operator class, when it is not the default one for the key's type.
    /// For an expression the default can only be told for operator classes
    /// that store their input type, so a default class may be listed; naming
    /// it again builds the same index. Always set when `opclass_options` is
    /// not empty.
    pub opclass: Option<PgQualifiedName>,
    /// Operator class parameters (13+, for example `siglen`).
    pub opclass_options: Vec<PgOption>,
    /// Per-column statistics target (`ALTER INDEX ... ALTER COLUMN n SET
    /// STATISTICS`, expression columns); `None` for the default.
    pub statistics_target: Option<i64>,
}

/// An index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgIndex {
    /// Schema of the table (and the index).
    pub schema: String,
    /// The indexed relation.
    pub table: String,
    /// Index name.
    pub name: String,
    /// Access method: `btree`, `hash`, `gin`, `gist`, `spgist`, `brin`, ...
    pub method: String,
    /// `UNIQUE`.
    pub unique: bool,
    /// Backs the primary key.
    pub primary: bool,
    /// Backs an exclusion constraint.
    pub exclusion: bool,
    /// Uniqueness is checked immediately (false for deferrable
    /// constraints).
    pub immediate: bool,
    /// `NULLS NOT DISTINCT` (15+; `None` before).
    pub nulls_not_distinct: Option<bool>,
    /// The table is clustered on this index.
    pub clustered: bool,
    /// Valid for queries (false after a failed `CREATE INDEX CONCURRENTLY`
    /// or on a partitioned index with missing partitions).
    pub valid: bool,
    /// Ready for inserts.
    pub ready: bool,
    /// Used as the replica identity.
    pub replica_identity: bool,
    /// Index of a partitioned table.
    pub partitioned: bool,
    /// The partitioned index this index is attached to.
    pub parent_index: Option<PgQualifiedName>,
    /// Key columns and expressions.
    pub keys: Vec<PgIndexKey>,
    /// `INCLUDE` columns: names as stored (not quoted).
    pub include_columns: Vec<String>,
    /// Predicate of a partial index.
    pub predicate: Option<String>,
    /// Storage parameters.
    pub storage_parameters: Vec<PgOption>,
    /// Tablespace; `None` for the database default.
    pub tablespace: Option<String>,
    /// Size in bytes: exact in `describe` of the index, estimated from
    /// `pg_class.relpages` in lists and in the details of its table.
    pub size_bytes: Option<i64>,
    /// Why the exact size is missing (the index is locked by another
    /// session).
    pub size_note: Option<String>,
    /// The constraint the index backs.
    pub constraint: Option<String>,
    /// `pg_get_indexdef`.
    pub definition: String,
    /// Foreign keys whose referenced key is this index
    /// (`schema.table.constraint`): the index cannot be dropped while they
    /// exist.
    pub foreign_keys: Vec<String>,
    /// Comment.
    pub comment: Option<String>,
    /// True when the table is a system or extension-owned table.
    pub system: bool,
}

/// The foreign key part of a constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgForeignKey {
    /// Referenced table.
    pub referenced: PgQualifiedName,
    /// Referenced columns.
    pub referenced_columns: Vec<String>,
    /// `noAction`, `restrict`, `cascade`, `setNull` or `setDefault`.
    pub on_update: String,
    /// As `on_update`.
    pub on_delete: String,
    /// `simple`, `full` or `partial`.
    pub match_type: String,
    /// Columns of `ON DELETE SET NULL (...)` / `SET DEFAULT (...)` (15+).
    pub on_delete_set_columns: Vec<String>,
}

/// A table constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgConstraint {
    /// Schema of the table.
    pub schema: String,
    /// The table.
    pub table: String,
    /// Constraint name.
    pub name: String,
    /// `primaryKey`, `unique`, `check`, `exclusion`, `foreignKey` or
    /// `notNull` (18+).
    pub constraint_type: String,
    /// Constrained columns.
    pub columns: Vec<String>,
    /// `pg_get_constraintdef`.
    pub definition: String,
    /// `DEFERRABLE`.
    pub deferrable: bool,
    /// `INITIALLY DEFERRED`.
    pub deferred: bool,
    /// False for a `NOT VALID` constraint that was not validated yet.
    pub validated: bool,
    /// False for `NOT ENFORCED` (18+).
    pub enforced: bool,
    /// The server's `connoinherit`: `NO INHERIT` for CHECK and NOT NULL
    /// constraints; the server also sets it on primary key, unique,
    /// exclusion and foreign key constraints of tables that are no
    /// partition, where it means nothing the user wrote.
    pub no_inherit: bool,
    /// Defined locally (false for a purely inherited constraint).
    pub local: bool,
    /// Number of parents the constraint is inherited from.
    pub inherited_count: i64,
    /// True when the constraint was cloned from a partitioned table's
    /// constraint.
    pub has_parent_constraint: bool,
    /// The index of a primary key, unique or exclusion constraint.
    pub index: Option<String>,
    /// Foreign key details.
    pub foreign_key: Option<PgForeignKey>,
    /// Comment.
    pub comment: Option<String>,
}

/// Who owns a sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgOwnedBy {
    /// Schema of the table.
    pub schema: String,
    /// The table.
    pub table: String,
    /// The column.
    pub column: String,
}

/// A sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgSequence {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Owner role.
    pub owner: String,
    /// `smallint`, `integer` or `bigint`.
    pub data_type: String,
    /// `START WITH` (64-bit values travel as decimal text).
    #[serde(with = "crate::postgres::edit::int_text")]
    pub start: i64,
    /// `INCREMENT BY` (decimal text).
    #[serde(with = "crate::postgres::edit::int_text")]
    pub increment: i64,
    /// `MINVALUE` (decimal text).
    #[serde(with = "crate::postgres::edit::int_text")]
    pub min: i64,
    /// `MAXVALUE` (decimal text).
    #[serde(with = "crate::postgres::edit::int_text")]
    pub max: i64,
    /// `CACHE` (decimal text).
    #[serde(with = "crate::postgres::edit::int_text")]
    pub cache: i64,
    /// `CYCLE`.
    pub cycle: bool,
    /// `last_value` of the sequence relation: with `is_called` the last
    /// value handed out (written ahead by `CACHE` above 1), without it the
    /// value the next `nextval` returns (a new sequence, after `RESTART` or
    /// `setval(.., false)`). Read only in details (reading it waits behind
    /// a lock on the sequence), so always `None` in lists; also `None`
    /// without privilege to read it. Decimal text.
    #[serde(with = "crate::postgres::edit::int_text::option")]
    pub last_value: Option<i64>,
    /// `is_called` of the sequence relation (see `last_value`); `None`
    /// when `last_value` is.
    pub is_called: Option<bool>,
    /// `UNLOGGED` (15+).
    pub unlogged: bool,
    /// The column that owns the sequence (`OWNED BY`, `serial`, identity).
    pub owned_by: Option<PgOwnedBy>,
    /// `always` or `byDefault` for the sequence of an identity column.
    pub identity: Option<String>,
    /// Comment.
    pub comment: Option<String>,
    /// True for system and extension-owned sequences.
    pub system: bool,
}

/// A trigger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgTrigger {
    /// Schema of the table.
    pub schema: String,
    /// The table or view.
    pub table: String,
    /// Trigger name.
    pub name: String,
    /// `before`, `after` or `insteadOf`.
    pub timing: String,
    /// `insert`, `update`, `delete`, `truncate`.
    pub events: Vec<String>,
    /// Columns of `UPDATE OF`.
    pub update_columns: Vec<String>,
    /// `row` or `statement`.
    pub level: String,
    /// The `WHEN` condition.
    pub condition: Option<String>,
    /// The trigger function with its argument types.
    pub function: String,
    /// `origin` (enabled), `disabled`, `replica` or `always`.
    pub enabled: String,
    /// A constraint trigger.
    pub constraint_trigger: bool,
    /// `DEFERRABLE` (constraint triggers).
    pub deferrable: bool,
    /// `INITIALLY DEFERRED` (constraint triggers).
    pub deferred: bool,
    /// Transition table name for old rows (`REFERENCING OLD TABLE AS`).
    pub old_table: Option<String>,
    /// Transition table name for new rows (`REFERENCING NEW TABLE AS`).
    pub new_table: Option<String>,
    /// Cloned from a partitioned table's trigger.
    pub inherited: bool,
    /// `pg_get_triggerdef`.
    pub definition: String,
    /// Comment.
    pub comment: Option<String>,
}

/// A rewrite rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgRule {
    /// Schema of the table.
    pub schema: String,
    /// The table or view.
    pub table: String,
    /// Rule name.
    pub name: String,
    /// `select`, `insert`, `update` or `delete`.
    pub event: String,
    /// `DO INSTEAD`.
    pub instead: bool,
    /// `origin` (enabled), `disabled`, `replica` or `always`.
    pub enabled: String,
    /// `pg_get_ruledef`, without a trailing semicolon.
    pub definition: String,
    /// Comment.
    pub comment: Option<String>,
}

/// A row level security policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgPolicy {
    /// Schema of the table.
    pub schema: String,
    /// The table.
    pub table: String,
    /// Policy name.
    pub name: String,
    /// `all`, `select`, `insert`, `update` or `delete`.
    pub command: String,
    /// True for `PERMISSIVE`, false for `RESTRICTIVE`.
    pub permissive: bool,
    /// Roles; `public` for `PUBLIC`.
    pub roles: Vec<String>,
    /// `USING` expression.
    pub using: Option<String>,
    /// `WITH CHECK` expression.
    pub with_check: Option<String>,
    /// Comment.
    pub comment: Option<String>,
}
