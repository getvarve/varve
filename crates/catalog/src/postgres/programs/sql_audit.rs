//! Source check: every PostgreSQL statement Varve writes itself, in the
//! catalog crate (all modules) and the live stats crate, resolves its
//! names in `pg_catalog` only.
//!
//! PostgreSQL resolves names when it parses a statement, before any
//! `set_config('search_path', ...)` in that statement runs. An unqualified
//! function, operator, type or relation can therefore be taken over by a
//! better matching (or, with `pg_catalog` listed late in `search_path`, an
//! identical) object a user created in another schema, which then runs
//! with the browsing user's rights. This test reads every string literal
//! of `crates/catalog/src` and `crates/live-stats/src` (outside
//! `#[cfg(test)]` items and the files in [`NOT_PG_SQL_FILES`]), tokenizes
//! it as SQL and fails on:
//!
//! - a function call that is not `pg_catalog.name(...)` (SQL constructs
//!   such as `COALESCE(`, `EXISTS (` and `ARRAY(` are no calls),
//! - an operator not written as `OPERATOR(pg_catalog.op)`, and the
//!   constructs that look operators up by name: `IN`, `LIKE`, `ILIKE`,
//!   `SIMILAR TO`, `BETWEEN`, `NULLIF`, `IS [NOT] DISTINCT FROM`, a simple
//!   `CASE x WHEN`, `JOIN ... USING` and `NATURAL JOIN`,
//! - a type after `::`, `CAST(... AS` or `COLLATE` that is not
//!   `pg_catalog.name` (SQL keyword types such as `bigint` are always
//!   `pg_catalog`'s),
//! - a `pg_*` catalog relation without `pg_catalog.`,
//! - a `'name'::pg_catalog.regclass` (or other name-resolving `reg*`
//!   type) literal whose name is not schema-qualified,
//! - an unterminated quote, which would hide the rest of the literal.
//!
//! Literals that are no PostgreSQL SQL Varve resolves names in (generated
//! DDL with `name = value` options, MySQL statements in files shared by
//! both engines, messages, connection strings) are listed in
//! [`NOT_CATALOG_SQL`] by file and exact text; an entry that matches
//! nothing fails the test too, so the list cannot go stale. The same holds
//! for [`NOT_PG_SQL_FILES`].

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Source files (or directories, ending in `/`) that hold no PostgreSQL
/// SQL, relative to `crates/`, with the reason.
const NOT_PG_SQL_FILES: &[(&str, &str)] = &[
    ("catalog/src/mysql/", "MySQL and MariaDB only"),
    ("catalog/src/explain/mysql.rs", "MySQL and MariaDB only"),
    ("catalog/src/explain/tests.rs", "a #[cfg(test)] module"),
    ("catalog/src/postgres/programs/sql_audit.rs", "this check"),
    ("live-stats/src/redis/", "Redis only"),
    (
        "live-stats/src/sql/mysql_readonly.rs",
        "MySQL and MariaDB only",
    ),
];

/// Literals that are no PostgreSQL SQL with names to resolve, by file
/// (relative to `crates/`) and exact text.
const NOT_CATALOG_SQL: &[(&str, &str)] = &[
    // Generated DDL: CREATE OPERATOR options.
    ("catalog/src/postgres/programs/casts.rs", "FUNCTION = {}"),
    ("catalog/src/postgres/programs/casts.rs", "LEFTARG = {left}"),
    (
        "catalog/src/postgres/programs/casts.rs",
        "RIGHTARG = {right}",
    ),
    (
        "catalog/src/postgres/programs/casts.rs",
        "COMMUTATOR = OPERATOR({commutator})",
    ),
    (
        "catalog/src/postgres/programs/casts.rs",
        "NEGATOR = OPERATOR({negator})",
    ),
    (
        "catalog/src/postgres/programs/casts.rs",
        "RESTRICT = {restrict}",
    ),
    ("catalog/src/postgres/programs/casts.rs", "JOIN = {join}"),
    // The characters an operator name is made of.
    (
        "catalog/src/postgres/programs/casts.rs",
        "+-*/<>=~!@#%^&|`?",
    ),
    // Generated DDL: CREATE COLLATION options.
    (
        "catalog/src/postgres/programs/collations.rs",
        "provider = {}",
    ),
    (
        "catalog/src/postgres/programs/collations.rs",
        "lc_collate = {}",
    ),
    (
        "catalog/src/postgres/programs/collations.rs",
        "lc_ctype = {}",
    ),
    ("catalog/src/postgres/programs/collations.rs", "locale = {}"),
    (
        "catalog/src/postgres/programs/collations.rs",
        "deterministic = false",
    ),
    ("catalog/src/postgres/programs/collations.rs", "rules = {}"),
    // The search_path value, written inside a string literal.
    (
        "catalog/src/postgres/programs/common.rs",
        "pg_catalog, pg_temp",
    ),
    // Messages.
    (
        "catalog/src/postgres/programs/common.rs",
        "an object is locked by another session (waited {lock_timeout}): try again when that session has finished",
    ),
    (
        "catalog/src/postgres/programs/common.rs",
        "the catalog cannot be read inside an open transaction: finish the transaction first",
    ),
    // The placeholder generated DDL writes for hidden option values.
    ("catalog/src/postgres/programs/common.rs", "<redacted>"),
    (
        "catalog/src/postgres/programs/common.rs",
        "-- Option values that may hold secrets are written as '<redacted>': replace them before running this.",
    ),
    // Generated DDL: an OPTIONS clause.
    ("catalog/src/postgres/programs/common.rs", " OPTIONS ({})"),
    // Credential detection in option values.
    ("catalog/src/postgres/programs/common.rs", "pwd="),
    // Connection strings (libpq syntax, not SQL).
    ("catalog/src/postgres/programs/conninfo.rs", "{key}={}"),
    ("catalog/src/postgres/programs/conninfo.rs", "postgresql://"),
    ("catalog/src/postgres/programs/conninfo.rs", "postgres://"),
    ("catalog/src/postgres/programs/conninfo.rs", "\\'"),
    // Generated DDL: CREATE EVENT TRIGGER.
    (
        "catalog/src/postgres/programs/events.rs",
        "\n    WHEN TAG IN ({})",
    ),
    // Generated DDL: CREATE PUBLICATION and CREATE SUBSCRIPTION options.
    (
        "catalog/src/postgres/programs/replication.rs",
        "publish = {}",
    ),
    (
        "catalog/src/postgres/programs/replication.rs",
        "publish_via_partition_root = {}",
    ),
    (
        "catalog/src/postgres/programs/replication.rs",
        "publish_generated_columns = {}",
    ),
    (
        "catalog/src/postgres/programs/replication.rs",
        "connect = false",
    ),
    (
        "catalog/src/postgres/programs/replication.rs",
        "slot_name = {}",
    ),
    (
        "catalog/src/postgres/programs/replication.rs",
        "slot_name = NONE",
    ),
    (
        "catalog/src/postgres/programs/replication.rs",
        "synchronous_commit = {}",
    ),
    (
        "catalog/src/postgres/programs/replication.rs",
        "{option} = {value}",
    ),
    (
        "catalog/src/postgres/programs/replication.rs",
        "streaming = {}",
    ),
    (
        "catalog/src/postgres/programs/replication.rs",
        "origin = {}",
    ),
    (
        "catalog/src/postgres/programs/replication.rs",
        "the connection string of this subscription is not readable (it needs superuser rights) or not valid, so no DDL can be built",
    ),
    // Display text of an extension: version and schema.
    ("catalog/src/postgres/programs/extensions.rs", "{} in {}"),
    // Display text of an aggregate kind.
    ("catalog/src/postgres/programs/routines.rs", "ordered-set"),
    // Generated DDL: CREATE AGGREGATE options.
    ("catalog/src/postgres/programs/routines.rs", "SFUNC = {}"),
    ("catalog/src/postgres/programs/routines.rs", "STYPE = {}"),
    ("catalog/src/postgres/programs/routines.rs", "SSPACE = {}"),
    (
        "catalog/src/postgres/programs/routines.rs",
        "FINALFUNC = {final_function}",
    ),
    (
        "catalog/src/postgres/programs/routines.rs",
        "FINALFUNC_MODIFY = {}",
    ),
    (
        "catalog/src/postgres/programs/routines.rs",
        "{keyword} = {function}",
    ),
    ("catalog/src/postgres/programs/routines.rs", "INITCOND = {}"),
    (
        "catalog/src/postgres/programs/routines.rs",
        "MSFUNC = {moving}",
    ),
    (
        "catalog/src/postgres/programs/routines.rs",
        "MINVFUNC = {inverse}",
    ),
    (
        "catalog/src/postgres/programs/routines.rs",
        "MSTYPE = {state_type}",
    ),
    ("catalog/src/postgres/programs/routines.rs", "MSSPACE = {}"),
    (
        "catalog/src/postgres/programs/routines.rs",
        "MFINALFUNC = {final_function}",
    ),
    (
        "catalog/src/postgres/programs/routines.rs",
        "MFINALFUNC_MODIFY = {}",
    ),
    (
        "catalog/src/postgres/programs/routines.rs",
        "MINITCOND = {}",
    ),
    (
        "catalog/src/postgres/programs/routines.rs",
        "SORTOP = OPERATOR({operator})",
    ),
    ("catalog/src/postgres/programs/routines.rs", "PARALLEL = {}"),
    // Generated DDL: CREATE TYPE.
    (
        "catalog/src/postgres/programs/types.rs",
        "CREATE TYPE {name} AS ENUM ()",
    ),
    (
        "catalog/src/postgres/programs/types.rs",
        "CREATE TYPE {name} AS ENUM (\n    {}\n)",
    ),
    (
        "catalog/src/postgres/programs/types.rs",
        " COLLATE {collation}",
    ),
    ("catalog/src/postgres/programs/types.rs", "SUBTYPE = {}"),
    (
        "catalog/src/postgres/programs/types.rs",
        "SUBTYPE_OPCLASS = {}",
    ),
    (
        "catalog/src/postgres/programs/types.rs",
        "COLLATION = {collation}",
    ),
    (
        "catalog/src/postgres/programs/types.rs",
        "CANONICAL = {canonical}",
    ),
    (
        "catalog/src/postgres/programs/types.rs",
        "SUBTYPE_DIFF = {diff}",
    ),
    (
        "catalog/src/postgres/programs/types.rs",
        "MULTIRANGE_TYPE_NAME = {multirange}",
    ),
    (
        "catalog/src/postgres/programs/types.rs",
        "CREATE TYPE {name} AS RANGE (\n    {}\n)",
    ),
    // Messages and display text.
    ("catalog/src/postgres/relations.rs", "the parent's schema"),
    ("catalog/src/postgres/relations.rs", "~{r} rows"),
    (
        "catalog/src/postgres/relations.rs",
        "{kind:?} objects are not listed {level}; the levels are: server (databases, tablespaces), database (schemas), schema (tables, partitioned tables, foreign tables, views, materialized views, sequences), relation (columns, indexes, constraints, triggers, rules, policies, partitions)",
    ),
    // Generated DDL: CREATE DATABASE options.
    (
        "catalog/src/postgres/relations/ddl.rs",
        "TEMPLATE = template0",
    ),
    ("catalog/src/postgres/relations/ddl.rs", "OWNER = {}"),
    ("catalog/src/postgres/relations/ddl.rs", "ENCODING = {}"),
    (
        "catalog/src/postgres/relations/ddl.rs",
        "LOCALE_PROVIDER = {}",
    ),
    ("catalog/src/postgres/relations/ddl.rs", "LC_COLLATE = {}"),
    ("catalog/src/postgres/relations/ddl.rs", "LC_CTYPE = {}"),
    ("catalog/src/postgres/relations/ddl.rs", "ICU_LOCALE = {}"),
    (
        "catalog/src/postgres/relations/ddl.rs",
        "BUILTIN_LOCALE = {}",
    ),
    ("catalog/src/postgres/relations/ddl.rs", "ICU_RULES = {}"),
    ("catalog/src/postgres/relations/ddl.rs", "TABLESPACE = {}"),
    (
        "catalog/src/postgres/relations/ddl.rs",
        "ALLOW_CONNECTIONS = {}",
    ),
    (
        "catalog/src/postgres/relations/ddl.rs",
        "CONNECTION LIMIT = {}",
    ),
    ("catalog/src/postgres/relations/ddl.rs", "IS_TEMPLATE = {}"),
    // Generated DDL: the separator of `name = value` storage options.
    ("catalog/src/postgres/relations/ddl.rs", " = "),
    // Generated DDL: other clauses.
    (
        "catalog/src/postgres/relations/ddl.rs",
        "ALTER TABLE {} ALTER COLUMN {} ADD GENERATED {} AS IDENTITY (SEQUENCE NAME {} {})",
    ),
    ("catalog/src/postgres/relations/ddl.rs", "OPTIONS ({})"),
    ("catalog/src/postgres/relations/ddl.rs", "\nOPTIONS ({})"),
    ("catalog/src/postgres/relations/ddl.rs", "COLLATE {}"),
    (
        "catalog/src/postgres/relations/ddl.rs",
        " WITH CHECK ({check})",
    ),
    ("catalog/src/postgres/relations/ddl.rs", "\nINHERITS ({})"),
    ("catalog/src/postgres/relations/ddl.rs", "{alter} SET ({})"),
    ("catalog/src/postgres/relations/ddl.rs", " USING ({using})"),
    // Message.
    (
        "catalog/src/postgres/relations/ddl.rs",
        "{} is a built-in tablespace and cannot be created",
    ),
    // Display text of constraints in the tree.
    (
        "catalog/src/postgres/relations/load.rs",
        "primary key ({columns})",
    ),
    (
        "catalog/src/postgres/relations/load.rs",
        "unique ({columns})",
    ),
    (
        "catalog/src/postgres/relations/load.rs",
        "exclusion ({columns})",
    ),
    (
        "catalog/src/postgres/relations/load.rs",
        "not null ({columns})",
    ),
    (
        "catalog/src/postgres/relations/load.rs",
        "foreign key ({columns}) references {}.{} ({})",
    ),
    // catalog/src/data_edit/apply.rs: MySQL and MariaDB statements.
    (
        "catalog/src/data_edit/apply.rs",
        "SELECT @@in_transaction AS open",
    ),
    // catalog/src/data_edit/apply.rs: Messages and display text.
    (
        "catalog/src/data_edit/apply.rs",
        "the edit plan was made for another engine than this session's",
    ),
    // catalog/src/data_edit/expr.rs: Messages and display text.
    (
        "catalog/src/data_edit/expr.rs",
        "has a backslash inside a string, which MySQL reads differently depending on the NO_BACKSLASH_ESCAPES SQL mode; use a hex literal (X'...') instead",
    ),
    (
        "catalog/src/data_edit/expr.rs",
        "has a dollar-quoted string that is not closed",
    ),
    // catalog/src/data_edit/key.rs: MySQL and MariaDB statements.
    (
        "catalog/src/data_edit/key.rs",
        "SELECT TABLE_TYPE AS table_type FROM information_schema.TABLES WHERE TABLE_SCHEMA = {database_lit} AND TABLE_NAME = {table_lit}",
    ),
    (
        "catalog/src/data_edit/key.rs",
        "SELECT COLUMN_NAME AS name, DATA_TYPE AS data_type, COLUMN_TYPE AS column_type, IS_NULLABLE AS nullable, COLUMN_DEFAULT AS column_default, EXTRA AS extra FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {database_lit} AND TABLE_NAME = {table_lit} ORDER BY ORDINAL_POSITION",
    ),
    (
        "catalog/src/data_edit/key.rs",
        "SELECT INDEX_NAME AS index_name, COLUMN_NAME AS column_name FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = {database_lit} AND TABLE_NAME = {table_lit} AND NON_UNIQUE = 0 ORDER BY INDEX_NAME, SEQ_IN_INDEX",
    ),
    // catalog/src/data_edit/plan.rs: MySQL and MariaDB statements.
    (
        "catalog/src/data_edit/plan.rs",
        "IF(FIND_IN_SET('NO_AUTO_VALUE_ON_ZERO', @@SESSION.sql_mode) > 0, 0, LAST_INSERT_ID())",
    ),
    ("catalog/src/data_edit/plan.rs", "LAST_INSERT_ID()"),
    ("catalog/src/data_edit/plan.rs", "{ident} = {value}"),
    ("catalog/src/data_edit/plan.rs", "="),
    (
        "catalog/src/data_edit/plan.rs",
        "CONVERT({} USING utf8mb4) COLLATE {collation}",
    ),
    ("catalog/src/data_edit/plan.rs", "{ident} <=> {right}"),
    ("catalog/src/data_edit/plan.rs", "CAST({literal} AS JSON)"),
    // catalog/src/data_edit/plan.rs: An operator named with its schema read from the catalog.
    ("catalog/src/data_edit/plan.rs", "OPERATOR({schema}.{name})"),
    // catalog/src/data_edit/plan.rs: A literal cast to the column's type, named with its schema read from the catalog.
    ("catalog/src/data_edit/plan.rs", "CAST({} AS {ty})"),
    // catalog/src/data_edit/plan.rs: Generated DML: `RETURNING *` and `SET` assignments (no operators).
    ("catalog/src/data_edit/plan.rs", " RETURNING *"),
    ("catalog/src/data_edit/plan.rs", "{} = {}"),
    (
        "catalog/src/data_edit/plan.rs",
        "UPDATE {} SET {set_sql} WHERE {filter} RETURNING *",
    ),
    // catalog/src/data_edit/plan.rs: No SQL: words and characters the code compares text with.
    ("catalog/src/data_edit/plan.rs", "+-*/<>=~!@#%^&|`?"),
    ("catalog/src/data_edit/plan.rs", "pg_catalog"),
    ("catalog/src/data_edit/plan.rs", "+00:00"),
    // catalog/src/explain/mod.rs: MySQL and MariaDB statements.
    ("catalog/src/explain/mod.rs", "ANALYZE FORMAT=JSON"),
    ("catalog/src/explain/mod.rs", "EXPLAIN FORMAT=JSON"),
    ("catalog/src/explain/mod.rs", "EXPLAIN FORMAT=TREE"),
    ("catalog/src/explain/mod.rs", "EXPLAIN ANALYZE FORMAT=JSON"),
    ("catalog/src/explain/mod.rs", "EXPLAIN ANALYZE FORMAT=TREE"),
    (
        "catalog/src/explain/mod.rs",
        "SELECT @@in_transaction AS in_transaction",
    ),
    (
        "catalog/src/explain/mod.rs",
        "SELECT @@autocommit AS autocommit",
    ),
    (
        "catalog/src/explain/mod.rs",
        "SET @@transaction_isolation = @@transaction_isolation",
    ),
    // catalog/src/explain/mod.rs: The EXPLAIN option list (keywords only).
    ("catalog/src/explain/mod.rs", "EXPLAIN ({})"),
    // catalog/src/explain/mod.rs: Messages and display text.
    (
        "catalog/src/explain/mod.rs",
        "{} {} has no EXPLAIN ANALYZE (MySQL needs 8.0.18 or newer)",
    ),
    (
        "catalog/src/explain/mod.rs",
        "{} {} has no {what} in {name} format",
    ),
    (
        "catalog/src/explain/mod.rs",
        "EXPLAIN does not take text with executable comments (/*! ... */ or /*M! ... */): the server runs their contents as SQL",
    ),
    // catalog/src/explain/postgres.rs: No SQL: words and characters the code compares text with.
    ("catalog/src/explain/postgres.rs", "?"),
    // catalog/src/explain/safety.rs: No SQL: words and characters the code compares text with.
    ("catalog/src/explain/safety.rs", "pg_notify"),
    ("catalog/src/explain/safety.rs", "pg_terminate_backend"),
    ("catalog/src/explain/safety.rs", "pg_cancel_backend"),
    ("catalog/src/explain/safety.rs", "pg_reload_conf"),
    ("catalog/src/explain/safety.rs", "pg_rotate_logfile"),
    ("catalog/src/explain/safety.rs", "pg_switch_wal"),
    ("catalog/src/explain/safety.rs", "pg_promote"),
    ("catalog/src/explain/safety.rs", "pg_create_restore_point"),
    ("catalog/src/explain/safety.rs", "pg_sleep"),
    ("catalog/src/explain/safety.rs", "pg_sleep_for"),
    ("catalog/src/explain/safety.rs", "pg_sleep_until"),
    ("catalog/src/explain/safety.rs", "pg_current_xact_id"),
    ("catalog/src/explain/safety.rs", "pg_advisory"),
    ("catalog/src/explain/safety.rs", "pg_try_advisory"),
    ("catalog/src/explain/safety.rs", "pg_file_"),
    ("catalog/src/explain/safety.rs", "pg_logfile_"),
    ("catalog/src/explain/safety.rs", "pg_create_"),
    ("catalog/src/explain/safety.rs", "pg_drop_replication"),
    ("catalog/src/explain/safety.rs", "pg_replication_"),
    ("catalog/src/explain/safety.rs", "pg_stat_reset"),
    ("catalog/src/explain/safety.rs", "pg_wal_replay"),
    ("catalog/src/explain/safety.rs", "pg_log_"),
    ("catalog/src/explain/safety.rs", "pg_import_"),
    ("catalog/src/explain/safety.rs", "pg_catalog"),
    ("catalog/src/explain/safety.rs", "pg_typeof"),
    ("catalog/src/explain/safety.rs", "CASE"),
    ("catalog/src/explain/safety.rs", "LIKE"),
    ("catalog/src/explain/safety.rs", "ILIKE"),
    ("catalog/src/explain/safety.rs", "BETWEEN"),
    ("catalog/src/explain/safety.rs", "SIMILAR"),
    ("catalog/src/explain/safety.rs", "COLLATE"),
    // catalog/src/explain/safety.rs: operators and keywords the EXPLAIN ANALYZE safety check
    // looks for in user SQL (no SQL of its own).
    ("catalog/src/explain/safety.rs", "+-*/<>=~!@#%^&|`?"),
    ("catalog/src/explain/safety.rs", "~!@#%^&|`?"),
    // catalog/src/explain/safety.rs: Messages and display text.
    (
        "catalog/src/explain/safety.rs",
        "Sequence values it takes (nextval, serial and identity columns) are not given back by the rollback.",
    ),
    (
        "catalog/src/explain/safety.rs",
        "AUTO_INCREMENT values and sequence values it takes are not given back by the rollback, and changes to non-transactional tables (MyISAM, MEMORY, Aria, CSV, ...) are not rolled back at all.",
    ),
    (
        "catalog/src/explain/safety.rs",
        "MySQL does not measure single-table UPDATE and DELETE (it reports them as not executable by the iterator executor), so the plan has no actual values; a SELECT with the same WHERE clause shows the access path.",
    ),
    (
        "catalog/src/explain/safety.rs",
        "{name}() has effects a rollback does not undo (locks, sequences, files, other sessions or server state).",
    ),
    (
        "catalog/src/explain/safety.rs",
        "{name}() is not a built-in function known to be free of side effects; a user-defined function can change things a rollback does not undo.",
    ),
    // catalog/src/explain/tree.rs: No SQL: words and characters the code compares text with.
    (
        "catalog/src/explain/tree.rs",
        "<not executable by iterator executor>",
    ),
    ("catalog/src/explain/tree.rs", "->"),
    (
        "catalog/src/explain/tree.rs",
        "MySQL could not measure this statement (single-table UPDATE and DELETE are not run by EXPLAIN ANALYZE); the plan has no actual values",
    ),
    ("catalog/src/explain/tree.rs", "cost="),
    ("catalog/src/explain/tree.rs", "rows="),
    ("catalog/src/explain/tree.rs", "Select #"),
    // catalog/src/postgres/edit/common.rs: Generated DDL: `name = value` options and `SET` assignments (no operators).
    ("catalog/src/postgres/edit/common.rs", "{} = {}"),
    ("catalog/src/postgres/edit/common.rs", "{} = {values}"),
    // catalog/src/postgres/edit/common.rs: An operator named with its schema read from the catalog.
    ("catalog/src/postgres/edit/common.rs", "OPERATOR({}.{name})"),
    // catalog/src/postgres/edit/common.rs: No SQL: words and characters the code compares text with.
    ("catalog/src/postgres/edit/common.rs", "+-*/<>=~!@#%^&|`?"),
    ("catalog/src/postgres/edit/common.rs", "case"),
    ("catalog/src/postgres/edit/common.rs", "collate"),
    // catalog/src/postgres/edit/common.rs: Messages and display text.
    (
        "catalog/src/postgres/edit/common.rs",
        "{name:?} is not an operator name (it starts a comment)",
    ),
    (
        "catalog/src/postgres/edit/common.rs",
        "{what} cannot contain `;`",
    ),
    (
        "catalog/src/postgres/edit/common.rs",
        "{what} has an unbalanced `{c}`",
    ),
    (
        "catalog/src/postgres/edit/common.rs",
        "{what} must be a single expression (no `,` outside parentheses)",
    ),
    (
        "catalog/src/postgres/edit/common.rs",
        "{what} has an unclosed `{open}`",
    ),
    (
        "catalog/src/postgres/edit/common.rs",
        "{what} must be argument types separated by commas, for an ordered-set aggregate `[direct arguments] ORDER BY arguments`",
    ),
    (
        "catalog/src/postgres/edit/common.rs",
        "{what} must be `source AS target`",
    ),
    (
        "catalog/src/postgres/edit/common.rs",
        "{what} must be argument types separated by commas, each `[mode] [name] type`",
    ),
    (
        "catalog/src/postgres/edit/common.rs",
        "{what} must be one type name: a name (optionally schema-qualified or quoted) or a multi-word SQL type such as `double precision`, then optional `(modifiers)` and `[]`, with nothing after it",
    ),
    // catalog/src/postgres/edit/database.rs: Generated DDL: `name = value` options and `SET` assignments (no operators).
    ("catalog/src/postgres/edit/database.rs", "OWNER = {}"),
    ("catalog/src/postgres/edit/database.rs", "TEMPLATE = {}"),
    ("catalog/src/postgres/edit/database.rs", "ENCODING = {}"),
    ("catalog/src/postgres/edit/database.rs", "STRATEGY = {}"),
    ("catalog/src/postgres/edit/database.rs", "LOCALE = {}"),
    ("catalog/src/postgres/edit/database.rs", "LC_COLLATE = {}"),
    ("catalog/src/postgres/edit/database.rs", "LC_CTYPE = {}"),
    (
        "catalog/src/postgres/edit/database.rs",
        "LOCALE_PROVIDER = {name}",
    ),
    ("catalog/src/postgres/edit/database.rs", "ICU_LOCALE = {}"),
    ("catalog/src/postgres/edit/database.rs", "ICU_RULES = {}"),
    (
        "catalog/src/postgres/edit/database.rs",
        "BUILTIN_LOCALE = {}",
    ),
    ("catalog/src/postgres/edit/database.rs", "TABLESPACE = {}"),
    (
        "catalog/src/postgres/edit/database.rs",
        "ALLOW_CONNECTIONS = {}",
    ),
    (
        "catalog/src/postgres/edit/database.rs",
        "CONNECTION LIMIT = {v}",
    ),
    ("catalog/src/postgres/edit/database.rs", "IS_TEMPLATE = {}"),
    // catalog/src/postgres/edit/database.rs: Messages and display text.
    (
        "catalog/src/postgres/edit/database.rs",
        "DROP DATABASE ... WITH (FORCE)",
    ),
    (
        "catalog/src/postgres/edit/database.rs",
        "a connection limit is -1 (unlimited) or more",
    ),
    // catalog/src/postgres/edit/extension.rs: Messages and display text.
    (
        "catalog/src/postgres/edit/extension.rs",
        "CASCADE also drops columns, indexes and functions that use the extension's types and functions",
    ),
    // catalog/src/postgres/edit/index.rs: Generated DDL clauses around names, collations and expressions the user chose (resolved as the user wrote them).
    ("catalog/src/postgres/edit/index.rs", " COLLATE "),
    ("catalog/src/postgres/edit/index.rs", " INCLUDE ({})"),
    (
        "catalog/src/postgres/edit/index.rs",
        "ALTER INDEX {index} SET ({})",
    ),
    (
        "catalog/src/postgres/edit/index.rs",
        "ALTER INDEX {index} RESET ({})",
    ),
    ("catalog/src/postgres/edit/index.rs", "NULLS NOT DISTINCT"),
    ("catalog/src/postgres/edit/index.rs", " NULLS NOT DISTINCT"),
    // catalog/src/postgres/edit/index.rs: Messages and display text.
    (
        "catalog/src/postgres/edit/index.rs",
        "{} indexes do not sort, so ASC, DESC and NULLS FIRST/LAST do not apply",
    ),
    (
        "catalog/src/postgres/edit/index.rs",
        "INCLUDE on SP-GiST indexes",
    ),
    (
        "catalog/src/postgres/edit/index.rs",
        "NULLS NOT DISTINCT only applies to unique indexes",
    ),
    // catalog/src/postgres/edit/object.rs: Messages and display text.
    (
        "catalog/src/postgres/edit/object.rs",
        "{:?} {} needs its table in `parent`",
    ),
    (
        "catalog/src/postgres/edit/object.rs",
        "a cast needs `source AS target` in `signature`",
    ),
    (
        "catalog/src/postgres/edit/object.rs",
        "an operator needs its operand types (`left, right`, NONE for a prefix operator) in `signature`",
    ),
    (
        "catalog/src/postgres/edit/object.rs",
        "a {kind:?} has no owner of its own; it belongs to its table, extension or the cast's types",
    ),
    // catalog/src/postgres/edit/routine.rs: Generated DDL clauses around names, collations and expressions the user chose (resolved as the user wrote them).
    ("catalog/src/postgres/edit/routine.rs", "RETURNS TABLE ({})"),
    // catalog/src/postgres/edit/routine.rs: Messages and display text.
    (
        "catalog/src/postgres/edit/routine.rs",
        "a SECURITY DEFINER routine runs with its owner's rights; set search_path on it so callers cannot redirect its lookups",
    ),
    (
        "catalog/src/postgres/edit/routine.rs",
        "ROWS only applies to set-returning functions",
    ),
    (
        "catalog/src/postgres/edit/routine.rs",
        "SQL-standard routine bodies",
    ),
    // catalog/src/postgres/edit/rule.rs: Generated DDL clauses around names, collations and expressions the user chose (resolved as the user wrote them).
    ("catalog/src/postgres/edit/rule.rs", "\nUSING ({})"),
    ("catalog/src/postgres/edit/rule.rs", "\nWITH CHECK ({})"),
    ("catalog/src/postgres/edit/rule.rs", " USING ({})"),
    ("catalog/src/postgres/edit/rule.rs", " WITH CHECK ({})"),
    // catalog/src/postgres/edit/rule.rs: Messages and display text.
    (
        "catalog/src/postgres/edit/rule.rs",
        "a policy needs at least one role (or PUBLIC)",
    ),
    // catalog/src/postgres/edit/sequence.rs: Messages and display text.
    (
        "catalog/src/postgres/edit/sequence.rs",
        "a sequence's minimum must be below its maximum",
    ),
    (
        "catalog/src/postgres/edit/sequence.rs",
        "SET LOGGED / UNLOGGED on a sequence",
    ),
    // catalog/src/postgres/edit/table.rs: Generated DDL clauses around names, collations and expressions the user chose (resolved as the user wrote them).
    ("catalog/src/postgres/edit/table.rs", " COLLATE "),
    ("catalog/src/postgres/edit/table.rs", "PRIMARY KEY ({})"),
    ("catalog/src/postgres/edit/table.rs", "NULLS NOT DISTINCT"),
    ("catalog/src/postgres/edit/table.rs", " NULLS NOT DISTINCT"),
    ("catalog/src/postgres/edit/table.rs", "CHECK ({})"),
    (
        "catalog/src/postgres/edit/table.rs",
        "FOREIGN KEY ({}) REFERENCES {}",
    ),
    ("catalog/src/postgres/edit/table.rs", " COLLATE {}"),
    ("catalog/src/postgres/edit/table.rs", "FOR VALUES IN ({})"),
    ("catalog/src/postgres/edit/table.rs", " INHERITS ({})"),
    ("catalog/src/postgres/edit/table.rs", "{alter} SET ({})"),
    ("catalog/src/postgres/edit/table.rs", "{alter} RESET ({})"),
    // catalog/src/postgres/edit/table.rs: Messages and display text.
    (
        "catalog/src/postgres/edit/table.rs",
        "an identity sequence takes the column's type; leave the sequence type out",
    ),
    (
        "catalog/src/postgres/edit/table.rs",
        "a column list for SET NULL / SET DEFAULT",
    ),
    (
        "catalog/src/postgres/edit/table.rs",
        "a column list for SET NULL / SET DEFAULT is only allowed in ON DELETE",
    ),
    (
        "catalog/src/postgres/edit/table.rs",
        "a column added with a volatile default (for example clock_timestamp() or random()) rewrites the whole table; a constant default does not",
    ),
    (
        "catalog/src/postgres/edit/table.rs",
        "changing a column type rewrites the whole table and rebuilds its indexes, unless the old values fit the new type unchanged (for example a longer varchar)",
    ),
    (
        "catalog/src/postgres/edit/table.rs",
        "changing a column type can lose data (shorter text, fewer decimals)",
    ),
    (
        "catalog/src/postgres/edit/table.rs",
        "SET NOT NULL scans the whole table while holding the lock, unless a valid CHECK (column IS NOT NULL) constraint already proves it",
    ),
    (
        "catalog/src/postgres/edit/table.rs",
        "SET LOGGED / UNLOGGED rewrites the whole table",
    ),
    (
        "catalog/src/postgres/edit/table.rs",
        "ATTACH PARTITION scans the new partition (and the default partition, if any) to check its rows, unless a matching CHECK constraint proves it; it takes an ACCESS EXCLUSIVE lock on the partition",
    ),
    (
        "catalog/src/postgres/edit/table.rs",
        "DETACH PARTITION CONCURRENTLY / FINALIZE",
    ),
    // catalog/src/postgres/edit/trigger.rs: Generated DDL clauses around names, collations and expressions the user chose (resolved as the user wrote them).
    ("catalog/src/postgres/edit/trigger.rs", "\nWHEN TAG IN ({})"),
    // catalog/src/postgres/edit/types.rs: Generated DDL clauses around names, collations and expressions the user chose (resolved as the user wrote them).
    ("catalog/src/postgres/edit/types.rs", " COLLATE {}"),
    ("catalog/src/postgres/edit/types.rs", "CHECK ({})"),
    // catalog/src/postgres/edit/types.rs: Generated DDL: `name = value` options and `SET` assignments (no operators).
    ("catalog/src/postgres/edit/types.rs", "SUBTYPE = {}"),
    ("catalog/src/postgres/edit/types.rs", "SUBTYPE_OPCLASS = {}"),
    ("catalog/src/postgres/edit/types.rs", "COLLATION = {}"),
    ("catalog/src/postgres/edit/types.rs", "CANONICAL = {}"),
    ("catalog/src/postgres/edit/types.rs", "SUBTYPE_DIFF = {}"),
    (
        "catalog/src/postgres/edit/types.rs",
        "MULTIRANGE_TYPE_NAME = {}",
    ),
    // catalog/src/postgres/edit/view.rs: Generated DDL clauses around names, collations and expressions the user chose (resolved as the user wrote them).
    ("catalog/src/postgres/edit/view.rs", "{alter} SET ({})"),
    ("catalog/src/postgres/edit/view.rs", "{alter} RESET ({})"),
    // catalog/src/postgres/edit/view.rs: Generated DDL: `name = value` options and `SET` assignments (no operators).
    ("catalog/src/postgres/edit/view.rs", "check_option = {}"),
    ("catalog/src/postgres/edit/view.rs", "security_barrier = {}"),
    ("catalog/src/postgres/edit/view.rs", "security_invoker = {}"),
    // catalog/src/sql.rs: No SQL: words and characters the code compares text with.
    ("catalog/src/sql.rs", "-Infinity"),
    ("catalog/src/sql.rs", "\\\""),
    // live-stats/src/sql/mod.rs: Messages and display text.
    (
        "live-stats/src/sql/mod.rs",
        "connection {id} is this session's own connection and cannot be killed from here",
    ),
    (
        "live-stats/src/sql/mod.rs",
        "transactional plans are run by the editor's transaction runner",
    ),
    // live-stats/src/sql/process.rs: MySQL and MariaDB statements.
    (
        "live-stats/src/sql/process.rs",
        "SELECT ID AS id, ID = CONNECTION_ID() AS own, USER AS user_name, HOST AS host, DB AS db, COMMAND AS command, TIME AS time_s, STATE AS state, INFO AS info FROM {table} ORDER BY ID",
    ),
    (
        "live-stats/src/sql/process.rs",
        "SELECT ID AS id, ID = CONNECTION_ID() AS own, USER AS user_name, HOST AS host, DB AS db, COMMAND AS command, TIME AS time_s, TIME_MS AS time_ms, STATE AS state, INFO AS info, PROGRESS AS progress, STAGE AS stage, MAX_STAGE AS max_stage, MEMORY_USED AS memory_used, EXAMINED_ROWS AS examined_rows, QUERY_ID AS query_id FROM information_schema.PROCESSLIST ORDER BY ID",
    ),
    (
        "live-stats/src/sql/process.rs",
        "SELECT CONNECTION_ID() AS id",
    ),
    // live-stats/src/sql/status.rs: MySQL and MariaDB statements.
    (
        "live-stats/src/sql/status.rs",
        "SHOW GLOBAL VARIABLES WHERE Variable_name IN ('wsrep_on', 'wsrep_provider')",
    ),
    (
        "live-stats/src/sql/status.rs",
        "SHOW GLOBAL STATUS LIKE 'wsrep\\_%'",
    ),
    (
        "live-stats/src/sql/status.rs",
        "SHOW GLOBAL STATUS WHERE Variable_name IN ('Uptime', 'Threads_connected', 'Threads_running', 'Max_used_connections', 'Innodb_buffer_pool_reads', 'Innodb_buffer_pool_read_requests')",
    ),
    (
        "live-stats/src/sql/status.rs",
        "SHOW GLOBAL VARIABLES WHERE Variable_name IN ('max_connections', 'version_comment', 'wsrep_on', 'wsrep_provider')",
    ),
    // live-stats/src/sql/status.rs: Messages and display text.
    (
        "live-stats/src/sql/status.rs",
        "WAL receiver not streaming (state {}); lag unknown",
    ),
    (
        "live-stats/src/sql/status.rs",
        "caught up: all received WAL is replayed (the primary may simply be idle)",
    ),
    // live-stats/src/sql/variables.rs: MySQL and MariaDB statements.
    (
        "live-stats/src/sql/variables.rs",
        "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'performance_schema' AND TABLE_NAME = 'variables_metadata'",
    ),
    (
        "live-stats/src/sql/variables.rs",
        " LEFT JOIN performance_schema.variables_metadata m ON m.VARIABLE_NAME = i.VARIABLE_NAME",
    ),
    (
        "live-stats/src/sql/variables.rs",
        " WHERE LOWER(i.VARIABLE_NAME) = LOWER({literal})",
    ),
    (
        "live-stats/src/sql/variables.rs",
        "SELECT i.VARIABLE_NAME AS name, s.VARIABLE_VALUE AS session_value, g.VARIABLE_NAME IS NOT NULL AS has_global, g.VARIABLE_VALUE AS global_value, i.VARIABLE_SOURCE AS source, i.VARIABLE_PATH AS path, i.MIN_VALUE AS min_value, i.MAX_VALUE AS max_value, i.SET_TIME AS set_time, i.SET_USER AS set_user, i.SET_HOST AS set_host, p.VARIABLE_VALUE AS persisted_value, {metadata_columns} FROM performance_schema.variables_info i LEFT JOIN performance_schema.session_variables s ON s.VARIABLE_NAME = i.VARIABLE_NAME LEFT JOIN performance_schema.global_variables g ON g.VARIABLE_NAME = i.VARIABLE_NAME LEFT JOIN performance_schema.persisted_variables p ON p.VARIABLE_NAME = i.VARIABLE_NAME{metadata_join}{filter} ORDER BY i.VARIABLE_NAME",
    ),
    (
        "live-stats/src/sql/variables.rs",
        " AND LOWER(VARIABLE_NAME) = LOWER({literal})",
    ),
    (
        "live-stats/src/sql/variables.rs",
        "SELECT VARIABLE_NAME AS name FROM performance_schema.variables_by_thread WHERE THREAD_ID = PS_CURRENT_THREAD_ID(){filter}",
    ),
    (
        "live-stats/src/sql/variables.rs",
        " WHERE VARIABLE_NAME = UPPER({literal})",
    ),
    (
        "live-stats/src/sql/variables.rs",
        "SELECT LOWER(VARIABLE_NAME) AS name, SESSION_VALUE AS session_value, GLOBAL_VALUE AS global_value, GLOBAL_VALUE_ORIGIN AS source, DEFAULT_VALUE AS default_value, VARIABLE_SCOPE AS scope, VARIABLE_TYPE AS data_type, VARIABLE_COMMENT AS description, NUMERIC_MIN_VALUE AS min_value, NUMERIC_MAX_VALUE AS max_value, ENUM_VALUE_LIST AS enum_values, READ_ONLY AS read_only, GLOBAL_VALUE_PATH AS path FROM information_schema.SYSTEM_VARIABLES{filter} ORDER BY VARIABLE_NAME",
    ),
    // live-stats/src/sql/variables.rs: Generated DDL: `name = value` options and `SET` assignments (no operators).
    ("live-stats/src/sql/variables.rs", "SET {name} = {}"),
    (
        "live-stats/src/sql/variables.rs",
        "ALTER SYSTEM SET {name} = {}",
    ),
    ("live-stats/src/sql/variables.rs", "SET {} {name} = {value}"),
    ("live-stats/src/sql/variables.rs", "SET {} {name} = DEFAULT"),
    // live-stats/src/sql/variables.rs: No SQL: words and characters the code compares text with.
    ("live-stats/src/sql/variables.rs", "{user}@{host}"),
    ("live-stats/src/sql/variables.rs", "postgresql://"),
    ("live-stats/src/sql/variables.rs", "postgres://"),
    ("live-stats/src/sql/variables.rs", "://"),
    ("live-stats/src/sql/variables.rs", "superuser-backend"),
    ("live-stats/src/sql/variables.rs", "?"),
    // live-stats/src/sql/variables.rs: Messages and display text.
    (
        "live-stats/src/sql/variables.rs",
        "SET PERSIST is a MySQL feature; use the global scope (ALTER SYSTEM)",
    ),
    ("live-stats/src/sql/variables.rs", "it is read-only"),
    (
        "live-stats/src/sql/variables.rs",
        "the global value returns to the compiled-in default",
    ),
    (
        "live-stats/src/sql/variables.rs",
        "RESET PERSIST removes the value from mysqld-auto.cnf; the running value does not change",
    ),
    (
        "live-stats/src/sql/variables.rs",
        "SET PERSIST changes the running value and writes mysqld-auto.cnf, so it survives a restart",
    ),
    (
        "live-stats/src/sql/variables.rs",
        "SET PERSIST_ONLY only writes mysqld-auto.cnf: it takes effect at the next restart",
    ),
    (
        "live-stats/src/sql/variables.rs",
        " (in {u} unless a unit is given)",
    ),
    (
        "live-stats/src/sql/variables.rs",
        "{} takes the units {} (case-sensitive), not {unit:?}",
    ),
    (
        "live-stats/src/sql/variables.rs",
        "{name}: expected a comma between list elements, found {c:?}",
    ),
    (
        "live-stats/src/sql/variables.rs",
        "{} must be between {} and {}",
    ),
    // catalog/src/data_edit/expr.rs: Messages and display text.
    ("catalog/src/data_edit/expr.rs", "cannot contain ';'"),
    // catalog/src/postgres/edit/object.rs: Generated DDL: a name the code quotes, followed by its argument or column list.
    ("catalog/src/postgres/edit/object.rs", "{}({})"),
    ("catalog/src/postgres/edit/object.rs", "{name} ({sig})"),
    // catalog/src/postgres/edit/routine.rs: Generated DDL: a name the code quotes, followed by its argument or column list.
    ("catalog/src/postgres/edit/routine.rs", "{}({args})"),
    (
        "catalog/src/postgres/edit/routine.rs",
        "CREATE {}{} {name}({})",
    ),
    (
        "catalog/src/postgres/edit/routine.rs",
        "{} {name}({signature})",
    ),
    // catalog/src/postgres/edit/table.rs: Generated DDL: a name the code quotes, followed by its argument or column list.
    ("catalog/src/postgres/edit/table.rs", "{keyword} ({})"),
    (
        "catalog/src/postgres/edit/table.rs",
        "EXCLUDE USING {} ({})",
    ),
    (
        "catalog/src/postgres/edit/table.rs",
        "PARTITION BY {strategy} ({})",
    ),
    // catalog/src/postgres/edit/trigger.rs: Generated DDL: a name the code quotes, followed by its argument or column list.
    ("catalog/src/postgres/edit/trigger.rs", "{}({})"),
    // catalog/src/postgres/programs/casts.rs: Generated DDL: a name the code quotes, followed by its argument or column list.
    (
        "catalog/src/postgres/programs/casts.rs",
        "OPERATOR {name} ({}, {})",
    ),
    (
        "catalog/src/postgres/programs/casts.rs",
        "CREATE OPERATOR {name} (\n    {}\n)",
    ),
    // catalog/src/postgres/programs/collations.rs: Generated DDL: a name the code quotes, followed by its argument or column list.
    (
        "catalog/src/postgres/programs/collations.rs",
        "CREATE {target} ({})",
    ),
    // catalog/src/postgres/programs/events.rs: Generated DDL: a name the code quotes, followed by its argument or column list.
    (
        "catalog/src/postgres/programs/events.rs",
        "\n    EXECUTE FUNCTION {}()",
    ),
    // catalog/src/postgres/programs/routines.rs: Generated DDL: a name the code quotes, followed by its argument or column list.
    (
        "catalog/src/postgres/programs/routines.rs",
        "{keyword} {}({})",
    ),
    (
        "catalog/src/postgres/programs/routines.rs",
        "AGGREGATE {name}({arguments})",
    ),
    (
        "catalog/src/postgres/programs/routines.rs",
        "CREATE {target} (\n    {}\n)",
    ),
    // catalog/src/sql.rs: MySQL and MariaDB hex string literals (no type name).
    ("catalog/src/sql.rs", "_utf8mb4 X'{}'"),
    ("catalog/src/sql.rs", "X'{}'"),
    // catalog/src/data_edit/plan.rs: Generated DML: the key index's collation, named with its schema read from the catalog.
    ("catalog/src/data_edit/plan.rs", "{left} COLLATE {}"),
    // catalog/src/explain/safety.rs: No SQL: names the safety check looks for.
    ("catalog/src/explain/safety.rs", "pg_read_"),
    ("catalog/src/explain/safety.rs", "pg_ls_"),
    // catalog/src/explain/safety.rs: No SQL: names the safety check looks for.
    (
        "catalog/src/explain/safety.rs",
        "The statement assigns a user variable (:= or INTO @var), which the rollback does not undo.",
    ),
    ("catalog/src/explain/safety.rs", "pg_input_"),
    // catalog/src/postgres/edit/int_text.rs: Messages and display text.
    (
        "catalog/src/postgres/edit/int_text.rs",
        "{v} is outside the 64-bit integer range",
    ),
    (
        "catalog/src/postgres/edit/int_text.rs",
        "{t} is outside the 64-bit integer range",
    ),
    // catalog/src/postgres/edit/sequence.rs: Messages and display text.
    ("catalog/src/postgres/edit/sequence.rs", "super::int_text"),
    (
        "catalog/src/postgres/edit/sequence.rs",
        "super::int_text::option",
    ),
    // catalog/src/postgres/relations/types.rs: Messages and display text.
    (
        "catalog/src/postgres/relations/types.rs",
        "crate::postgres::edit::int_text::option",
    ),
    (
        "catalog/src/postgres/relations/types.rs",
        "crate::postgres::edit::int_text",
    ),
    // live-stats/src/sql/process.rs: Messages and display text.
    (
        "live-stats/src/sql/process.rs",
        "{t:?} is not a 64-bit integer",
    ),
    // catalog/src/explain/probe.rs: PL/pgSQL syntax of Varve's own DO block (`:=`, `ERRCODE =`, `SQLSTATE` codes).
    (
        "catalog/src/explain/probe.rs",
        "  BEGIN\n    IF EXISTS (SELECT FROM pg_catalog.pg_event_trigger e WHERE e.evtenabled OPERATOR(pg_catalog.<>) 'D' AND e.evtevent OPERATOR(pg_catalog.=) ANY (ARRAY['ddl_command_start', 'ddl_command_end', 'sql_drop']::pg_catalog.name[]) AND (e.evttags IS NULL OR e.evttags OPERATOR(pg_catalog.&&) ARRAY['CREATE VIEW', 'DROP VIEW']::pg_catalog.text[])) THEN\n      failed := '{EVENT_TRIGGER_SQLSTATE} event triggers';\n    END IF;\n    BEGIN\n      IF failed IS NULL THEN\n        EXECUTE {view};\n      END IF;\n    EXCEPTION WHEN OTHERS THEN\n      IF SQLSTATE OPERATOR(pg_catalog.=) ANY (ARRAY['{READ_ONLY_SQLSTATE}', '42P07']::pg_catalog.text[]) OR (SQLSTATE OPERATOR(pg_catalog.=) '42501' AND SQLERRM OPERATOR(pg_catalog.~~) '%temporary%') THEN\n        failed := pg_catalog.concat(SQLSTATE, ' ', SQLERRM);\n      ELSE\n        RAISE;\n      END IF;\n    END;\n    IF failed IS NULL THEN\n      BEGIN\n        SET LOCAL search_path = pg_catalog, pg_temp;\n        SET LOCAL jit = off;\n        SET LOCAL plan_cache_mode = force_generic_plan;\n        EXECUTE {execute} INTO reasons;\n      EXCEPTION WHEN OTHERS THEN\n        failed := pg_catalog.concat(SQLSTATE, ' ', SQLERRM);\n      END;\n    END IF;\n    RAISE EXCEPTION USING ERRCODE = 'VA003';\n  EXCEPTION\n    WHEN SQLSTATE 'VA003' THEN NULL;\n  END;\n",
    ),
    (
        "catalog/src/explain/probe.rs",
        "  IF reasons IS NULL AND failed IS NULL THEN\n    BEGIN\n      {read_only}EXECUTE {} INTO output;\n      RAISE EXCEPTION USING ERRCODE = 'VA001';\n    EXCEPTION WHEN SQLSTATE 'VA001' THEN NULL;\n    END;\n    IF pg_catalog.octet_length(output) OPERATOR(pg_catalog.>) {OUTPUT_NOTICE_LIMIT} THEN\n      EXECUTE pg_catalog.format('SET {OUTPUT_SETTING} = %L', output);\n      output := NULL;\n    END IF;\n  END IF;\n",
    ),
    (
        "catalog/src/explain/probe.rs",
        "DECLARE\n  output pg_catalog.text;\n  reasons pg_catalog.text;\n  failed pg_catalog.text;\nBEGIN\n  RESET {OUTPUT_SETTING};\n{check}{run}  BEGIN\n    SET LOCAL client_min_messages = notice;\n    RAISE NOTICE USING MESSAGE = pg_catalog.concat('{OUTPUT_NOTICE}', CASE\n      WHEN failed IS NOT NULL THEN pg_catalog.concat('F', failed)\n      WHEN reasons IS NOT NULL THEN pg_catalog.concat('R', reasons)\n      WHEN NOT {ran} THEN 'R[]'\n      WHEN output IS NULL THEN 'S'\n      ELSE pg_catalog.concat('N', output) END);\n    RAISE EXCEPTION USING ERRCODE = 'VA002';\n  EXCEPTION WHEN SQLSTATE 'VA002' THEN NULL;\n  END;\nEND",
    ),
    // catalog/src/explain/probe.rs: Regular expressions (POSIX classes) over the server's deparse of a view, a domain check, a policy or a generated column.
    (
        "catalog/src/explain/probe.rs",
        "[[:alpha:]_][[:alnum:]_$]*|[-+*/<>=~!@#%^&|`?]+",
    ),
    (
        "catalog/src/explain/probe.rs",
        "(\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*)[[:space:]]*[(]",
    ),
    (
        "catalog/src/explain/probe.rs",
        "(^|[^[:alnum:]_])ONLY[[:space:]]+((\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*)([.](\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*))?)",
    ),
    (
        "catalog/src/explain/probe.rs",
        "(^|[^[:alnum:]_])RETURNING[[:space:]]+(\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*)([.](\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*))?",
    ),
    (
        "catalog/src/explain/probe.rs",
        "(^|[^[:alnum:]_])TABLESAMPLE[[:space:]]+(\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*)",
    ),
    // catalog/src/explain/probe.rs: MySQL and MariaDB statements and their fragments.
    (
        "catalog/src/explain/probe.rs",
        "CONCAT(TABLE_SCHEMA, '.', TABLE_NAME)",
    ),
    ("catalog/src/explain/probe.rs", "EXPLAIN FORMAT=TRADITIONAL"),
    ("catalog/src/explain/probe.rs", "LOWER({expression})"),
    (
        "catalog/src/explain/probe.rs",
        "SELECT @@SESSION.sql_mode AS mode, @@lower_case_table_names AS lctn",
    ),
    (
        "catalog/src/explain/probe.rs",
        "SELECT CONCAT(TABLE_SCHEMA, '.', TABLE_NAME) AS name, TABLE_TYPE AS kind, COALESCE(ENGINE, '') AS engine FROM information_schema.TABLES WHERE {full_name} IN ({qualified_list}) OR (TABLE_SCHEMA = DATABASE() AND {table_name} IN ({identifier_list}))",
    ),
    ("catalog/src/explain/probe.rs", "SELECT DATABASE() AS db"),
    (
        "catalog/src/explain/probe.rs",
        "SELECT ENGINE AS engine FROM information_schema.ENGINES WHERE SUPPORT IN ('YES', 'DEFAULT') AND UPPER(ENGINE) NOT IN ({local_engines})",
    ),
    (
        "catalog/src/explain/probe.rs",
        "SELECT LOWER(CONCAT(TABLE_SCHEMA, '.', TABLE_NAME)) AS name FROM information_schema.TABLES WHERE TABLE_TYPE = 'VIEW' AND ((TABLE_SCHEMA = DATABASE() AND LOWER(TABLE_NAME) IN ({identifier_list})) OR LOWER(CONCAT(TABLE_SCHEMA, '.', TABLE_NAME)) IN ({qualified_list}))",
    ),
    (
        "catalog/src/explain/probe.rs",
        "SELECT LOWER(ROUTINE_NAME) AS name FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = DATABASE() AND ROUTINE_TYPE = 'FUNCTION' AND LOWER(ROUTINE_NAME) IN ({call_list})",
    ),
    (
        "catalog/src/explain/probe.rs",
        "SELECT LOWER(name) AS name FROM mysql.func WHERE LOWER(name) IN ({call_list})",
    ),
    (
        "catalog/src/explain/probe.rs",
        "SELECT SCHEMA_NAME AS name FROM information_schema.SCHEMATA WHERE {schema_name} IN ({schema_list})",
    ),
    // catalog/src/explain/probe.rs: No SQL: function names and name patterns the check compares by text.
    ("catalog/src/explain/probe.rs", "pg_stat_get_"),
    (
        "catalog/src/explain/probe.rs",
        "timezone random random_normal clock_timestamp timeofday gen_random_uuid uuidv4 uuidv7 array_sample array_shuffle current_query pg_blocking_pids pg_safe_snapshot_blocking_pids pg_lock_status pg_prepared_xact pg_is_in_recovery pg_is_wal_replay_paused pg_get_wal_replay_pause_state pg_current_wal_lsn pg_current_wal_insert_lsn pg_current_wal_flush_lsn pg_last_wal_receive_lsn pg_last_wal_replay_lsn pg_last_xact_replay_timestamp pg_database_size pg_relation_size pg_table_size pg_indexes_size pg_total_relation_size pg_tablespace_size pg_get_backend_memory_contexts pg_get_shmem_allocations pg_get_shmem_allocations_numa pg_get_wait_events pg_get_loaded_modules pg_get_aios pg_jit_available pg_notification_queue_usage pg_partition_tree pg_partition_ancestors pg_sequence_last_value pg_get_sequence_data pg_stat_have_stats pg_xact_status txid_status pg_xact_commit_timestamp pg_xact_commit_timestamp_origin pg_last_committed_xact pg_collation_actual_version pg_database_collation_actual_version pg_control_checkpoint pg_control_init pg_control_recovery pg_control_system pg_get_multixact_members pg_get_wal_resource_managers pg_get_wal_summarizer_state",
    ),
    ("catalog/src/explain/probe.rs", "{}%"),
    // catalog/src/explain/probe.rs: Extension names (`pg_extension.extname`), deliberately not SQL names.
    ("catalog/src/explain/probe.rs", "pg_trgm"),
    ("catalog/src/explain/probe.rs", "uuid-ossp"),
    // catalog/src/explain/probe.rs: Messages and display text.
    (
        "catalog/src/explain/probe.rs",
        "A schema-qualified call ({}{name}()) reaches a stored function.",
    ),
    (
        "catalog/src/explain/probe.rs",
        "An event trigger on this server would run for the temporary view the check creates (CREATE VIEW), so what the statement references cannot be resolved without running that trigger.",
    ),
    (
        "catalog/src/explain/probe.rs",
        "The check was cancelled ({error}).",
    ),
    (
        "catalog/src/explain/probe.rs",
        "The connection is read-only, where the server cannot resolve what the statement references (that takes a temporary view); the confirmed run is still read-only.",
    ),
    (
        "catalog/src/explain/probe.rs",
        "The definition of {schema}.{name} could not be read ({message}).",
    ),
    (
        "catalog/src/explain/probe.rs",
        "The server did not report how it reads the statement (EXPLAIN left no rewritten statement in SHOW WARNINGS).",
    ),
    (
        "catalog/src/explain/probe.rs",
        "The session is inside a transaction, where the run cannot be made read-only.",
    ),
    (
        "catalog/src/explain/probe.rs",
        "The statement changes data, so it cannot run in a read-only transaction.",
    ),
    (
        "catalog/src/explain/probe.rs",
        "The statement could not be planned ({error}).",
    ),
    (
        "catalog/src/explain/probe.rs",
        "The statement locks rows (FOR UPDATE / FOR SHARE), which a read-only run does not allow.",
    ),
    (
        "catalog/src/explain/probe.rs",
        "The statement locks rows (FOR UPDATE, FOR SHARE, LOCK IN SHARE MODE), which other sessions wait for.",
    ),
    (
        "catalog/src/explain/probe.rs",
        "Whether the statement calls stored functions cannot be checked inside a MariaDB transaction (the check would read non-transactional tables).",
    ),
    (
        "catalog/src/explain/probe.rs",
        "Whether the statement is free of side effects could not be checked ({error}).",
    ),
    (
        "catalog/src/explain/probe.rs",
        "the catalog lookup's answer is not a JSON list: {e}",
    ),
    ("catalog/src/explain/probe.rs", "varve-explain-output:"),
    (
        "catalog/src/explain/probe.rs",
        "{name}() is a loadable function on this server; the statement may call it, and it can change things a rollback does not undo.",
    ),
    (
        "catalog/src/explain/probe.rs",
        "{name}() is a stored function of the default database the statement reaches (also through views); it can change things a rollback does not undo.",
    ),
    (
        "catalog/src/explain/probe.rs",
        "{name}() is a stored function of the default database; the statement may call it, and it can change things a rollback does not undo.",
    ),
    (
        "catalog/src/explain/probe.rs",
        "{schema}.{name}() is a stored function the statement reaches (also through views); it can change things a rollback does not undo.",
    ),
];

/// SQL keywords that may stand before `(` without being a function call.
const PAREN_KEYWORDS: &[&str] = &[
    "all",
    "and",
    "any",
    "array",
    "as",
    "by",
    "case",
    "cast",
    "coalesce",
    "distinct",
    "else",
    "end",
    "except",
    "exists",
    "filter",
    "from",
    "greatest",
    "group",
    "intersect",
    "is",
    "join",
    "lateral",
    "least",
    "limit",
    "not",
    "offset",
    "on",
    "or",
    "ordinality",
    "over",
    "row",
    "select",
    "some",
    "then",
    "union",
    "values",
    "when",
    "where",
    "with",
];

/// Type names that are SQL keywords: the grammar maps them to
/// `pg_catalog` types itself (`bigint` is `pg_catalog.int8`).
const KEYWORD_TYPES: &[&str] = &[
    "bigint",
    "bit",
    "boolean",
    "char",
    "character",
    "dec",
    "decimal",
    "double",
    "float",
    "int",
    "integer",
    "interval",
    "national",
    "nchar",
    "numeric",
    "real",
    "smallint",
    "time",
    "timestamp",
    "varchar",
];

/// SQL keywords that may stand right before a string constant without
/// naming its type.
const STRING_KEYWORDS: &[&str] = &[
    "all",
    "and",
    "any",
    "as",
    "between",
    "by",
    "case",
    "default",
    "else",
    "escape",
    "from",
    "ilike",
    "in",
    "is",
    "like",
    "limit",
    "not",
    "offset",
    "or",
    "return",
    "returns",
    "select",
    "set",
    "similar",
    "some",
    "symmetric",
    "then",
    "to",
    "using",
    "values",
    "when",
    "where",
    "with",
];

/// True when the token before the placeholder at `at` can only end an
/// operand: a qualified name (`t.a`), a constant or `)`.
fn is_operand_end(tokens: &[Token], at: usize) -> bool {
    let Some(before) = at.checked_sub(1).and_then(|p| tokens.get(p)) else {
        return false;
    };
    let qualified_name = matches!(before, Token::Word(_) | Token::Quoted(_))
        && at >= 2
        && tokens[at - 2].is_punct('.');
    qualified_name || matches!(before, Token::Str(_) | Token::Number) || before.is_punct(')')
}

/// True when the token after the placeholder at `at` can only start an
/// operand: a qualified name (`t.a`), a constant or `(`.
fn is_operand_start(tokens: &[Token], at: usize) -> bool {
    let Some(after) = tokens.get(at + 1) else {
        return false;
    };
    let qualified_name = matches!(after, Token::Word(_) | Token::Quoted(_))
        && tokens.get(at + 2).is_some_and(|t| t.is_punct('.'));
    qualified_name || matches!(after, Token::Str(_) | Token::Number) || after.is_punct('(')
}

/// Object identifier types whose input looks a name up in `search_path`
/// (unless it is schema-qualified).
const NAME_LOOKUP_TYPES: &[&str] = &[
    "regclass",
    "regcollation",
    "regconfig",
    "regdictionary",
    "regoper",
    "regoperator",
    "regproc",
    "regprocedure",
    "regtype",
];

/// Characters PostgreSQL operators are made of.
const OPERATOR_CHARS: &str = "+-*/<>=~!@#%^&|`?";

/// One string literal of a Rust source file.
#[derive(Debug)]
struct Literal {
    line: usize,
    text: String,
}

/// Collects the string literals of a Rust source file, skipping comments,
/// character literals and every item annotated `#[cfg(test)]`.
fn literals(source: &str) -> Result<Vec<Literal>, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1;
    // Brace depth, and the depth at which a skipped `#[cfg(test)]` item
    // started (literals are not collected while it is set).
    let mut depth = 0usize;
    let mut skip_from: Option<usize> = None;
    let mut pending_test_item = false;
    let at = |i: usize, text: &str| -> bool {
        text.chars()
            .enumerate()
            .all(|(k, c)| chars.get(i + k) == Some(&c))
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if at(i, "//") {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if at(i, "/*") {
            let mut nesting = 0usize;
            while i < chars.len() {
                if at(i, "/*") {
                    nesting += 1;
                    i += 2;
                } else if at(i, "*/") {
                    nesting -= 1;
                    i += 2;
                    if nesting == 0 {
                        break;
                    }
                } else {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }
        if at(i, "#[cfg(test)]") {
            pending_test_item = true;
            i += "#[cfg(test)]".len();
            continue;
        }
        let ident_before = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        // Raw strings: r"...", r#"..."#, br"...".
        let raw_start = if !ident_before && at(i, "br") {
            Some(i + 2)
        } else if !ident_before && c == 'r' {
            Some(i + 1)
        } else {
            None
        };
        if let Some(mut j) = raw_start {
            let mut hashes = 0;
            while chars.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if chars.get(j) == Some(&'"') {
                let start_line = line;
                let close: String = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let mut k = j + 1;
                let mut text = String::new();
                while k < chars.len() && !at(k, &close) {
                    if chars[k] == '\n' {
                        line += 1;
                    }
                    text.push(chars[k]);
                    k += 1;
                }
                if k >= chars.len() {
                    return Err(format!("unterminated raw string at line {start_line}"));
                }
                if skip_from.is_none() {
                    out.push(Literal {
                        line: start_line,
                        text,
                    });
                }
                i = k + close.chars().count();
                continue;
            }
        }
        if c == '\'' {
            // A character literal ('x', '\n', '\'', '\u{..}') or a lifetime.
            let end = if chars.get(i + 1) == Some(&'\\') {
                (i + 2..chars.len().min(i + 12)).find(|&k| chars[k] == '\'' && k > i + 2)
            } else if chars.get(i + 2) == Some(&'\'') {
                Some(i + 2)
            } else {
                None
            };
            i = end.map_or(i + 1, |k| k + 1);
            continue;
        }
        if c == '"' {
            let start_line = line;
            let mut text = String::new();
            let mut k = i + 1;
            loop {
                let Some(&ch) = chars.get(k) else {
                    return Err(format!("unterminated string at line {start_line}"));
                };
                match ch {
                    '"' => break,
                    '\\' => {
                        let next = chars.get(k + 1).copied().unwrap_or(' ');
                        k += 2;
                        match next {
                            'n' => text.push('\n'),
                            't' => text.push('\t'),
                            'r' => text.push('\r'),
                            '0' => text.push('\0'),
                            '\\' => text.push('\\'),
                            '"' => text.push('"'),
                            '\'' => text.push('\''),
                            'x' => {
                                let hex: String = chars[k..k + 2].iter().collect();
                                let code = u32::from_str_radix(&hex, 16)
                                    .map_err(|e| format!("line {line}: {e}"))?;
                                text.push(char::from_u32(code).unwrap_or('?'));
                                k += 2;
                            }
                            'u' => {
                                let close = (k..chars.len())
                                    .find(|&m| chars[m] == '}')
                                    .ok_or_else(|| format!("line {line}: bad \\u escape"))?;
                                let hex: String = chars[k + 1..close].iter().collect();
                                let code = u32::from_str_radix(&hex, 16)
                                    .map_err(|e| format!("line {line}: {e}"))?;
                                text.push(char::from_u32(code).unwrap_or('?'));
                                k = close + 1;
                            }
                            '\n' => {
                                // Line continuation: skip the newline and
                                // the next line's leading white space.
                                line += 1;
                                while let Some(&w) = chars.get(k) {
                                    if w == '\n' {
                                        line += 1;
                                    } else if !w.is_whitespace() {
                                        break;
                                    }
                                    k += 1;
                                }
                            }
                            other => return Err(format!("line {line}: unknown escape \\{other}")),
                        }
                    }
                    '\n' => {
                        line += 1;
                        text.push('\n');
                        k += 1;
                    }
                    other => {
                        text.push(other);
                        k += 1;
                    }
                }
            }
            if skip_from.is_none() {
                out.push(Literal {
                    line: start_line,
                    text,
                });
            }
            i = k + 1;
            continue;
        }
        match c {
            '{' => {
                if pending_test_item && skip_from.is_none() {
                    skip_from = Some(depth);
                    pending_test_item = false;
                }
                depth += 1;
            }
            '}' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("unbalanced braces at line {line}"))?;
                if skip_from == Some(depth) {
                    skip_from = None;
                }
            }
            ';' if pending_test_item && skip_from.is_none() => {
                // `#[cfg(test)] mod name;` or a test-only `use`.
                pending_test_item = false;
            }
            _ => {}
        }
        i += 1;
    }
    Ok(out)
}

/// A token of SQL text inside a literal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// An unquoted identifier or keyword.
    Word(String),
    /// A `"quoted"` identifier.
    Quoted(String),
    /// A string constant (the text between the quotes, undecoded).
    Str(String),
    /// A number.
    Number,
    /// A Rust format placeholder `{...}`.
    Placeholder,
    /// `::`.
    Cast,
    /// An operator.
    Operator(String),
    /// Any other single character: `( ) , . ; [ ] :` and escaped braces.
    Punct(char),
    /// A quote that is not closed inside the literal.
    Unterminated,
}

impl Token {
    fn word(&self) -> Option<String> {
        match self {
            Self::Word(word) => Some(word.to_ascii_lowercase()),
            _ => None,
        }
    }

    fn is_word(&self, keyword: &str) -> bool {
        self.word().as_deref() == Some(keyword)
    }

    fn is_punct(&self, punct: char) -> bool {
        *self == Self::Punct(punct)
    }
}

/// Tokenizes SQL text; `{...}` is a format placeholder and `{{` / `}}`
/// escaped braces.
fn tokens(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let at = |i: usize, s: &str| -> bool {
        s.chars()
            .enumerate()
            .all(|(k, c)| chars.get(i + k) == Some(&c))
    };
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if at(i, "--") {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if at(i, "/*") {
            i = (i + 2..chars.len().saturating_sub(1))
                .find(|&k| at(k, "*/"))
                .map_or(chars.len(), |k| k + 2);
        } else if at(i, "{{") || at(i, "}}") {
            out.push(Token::Punct(c));
            i += 2;
        } else if c == '{' {
            match (i..chars.len()).find(|&k| chars[k] == '}') {
                Some(end) => {
                    out.push(Token::Placeholder);
                    i = end + 1;
                }
                None => {
                    out.push(Token::Punct('{'));
                    i += 1;
                }
            }
        } else if c == '\''
            || ((c == 'E' || c == 'e')
                && chars.get(i + 1) == Some(&'\'')
                && !(i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')))
        {
            let escapes = c != '\'';
            let mut k = if escapes { i + 2 } else { i + 1 };
            let start = k;
            let mut closed = None;
            while k < chars.len() {
                if escapes && chars[k] == '\\' {
                    k += 2;
                } else if chars[k] == '\'' {
                    if chars.get(k + 1) == Some(&'\'') {
                        k += 2;
                    } else {
                        closed = Some(k);
                        break;
                    }
                } else {
                    k += 1;
                }
            }
            match closed {
                Some(end) => {
                    out.push(Token::Str(chars[start..end].iter().collect()));
                    i = end + 1;
                }
                None => {
                    out.push(Token::Unterminated);
                    i = chars.len();
                }
            }
        } else if c == '"' {
            let mut k = i + 1;
            let mut closed = None;
            while k < chars.len() {
                if chars[k] == '"' {
                    if chars.get(k + 1) == Some(&'"') {
                        k += 2;
                    } else {
                        closed = Some(k);
                        break;
                    }
                } else {
                    k += 1;
                }
            }
            match closed {
                Some(end) => {
                    out.push(Token::Quoted(chars[i + 1..end].iter().collect()));
                    i = end + 1;
                }
                None => {
                    out.push(Token::Unterminated);
                    i = chars.len();
                }
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            out.push(Token::Word(chars[start..i].iter().collect()));
        } else if c.is_ascii_digit() {
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            out.push(Token::Number);
        } else if at(i, "::") {
            out.push(Token::Cast);
            i += 2;
        } else if OPERATOR_CHARS.contains(c) {
            let start = i;
            while i < chars.len()
                && OPERATOR_CHARS.contains(chars[i])
                && !(i > start && (at(i, "--") || at(i, "/*")))
            {
                i += 1;
            }
            out.push(Token::Operator(chars[start..i].iter().collect()));
        } else {
            out.push(Token::Punct(c));
            i += 1;
        }
    }
    out
}

/// True when tokens `at..` are `pg_catalog .`.
fn qualified_at(tokens: &[Token], at: usize) -> bool {
    tokens.get(at).is_some_and(|t| t.is_word("pg_catalog"))
        && tokens.get(at + 1).is_some_and(|t| t.is_punct('.'))
}

/// True when the type name starting at `at` is acceptable: a keyword type
/// or `pg_catalog.name`. A bare placeholder (`::{ty}`) is not: the code
/// that fills it in is not checked, so type names are written out
/// (`::pg_catalog.{ty}`).
fn type_ok(tokens: &[Token], at: usize) -> bool {
    match tokens.get(at) {
        Some(Token::Word(word)) => {
            KEYWORD_TYPES.contains(&word.to_ascii_lowercase().as_str()) || qualified_at(tokens, at)
        }
        _ => false,
    }
}

/// The problems of one literal, as short descriptions.
fn problems(text: &str) -> Vec<String> {
    let tokens = tokens(text);
    let mut found = Vec::new();
    // Paren depth of `CAST(` constructs still open.
    let mut cast_depths: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    // Index just after the last `OPERATOR(pg_catalog.op)` construct.
    let mut operator_end = None;
    for (k, token) in tokens.iter().enumerate() {
        let prev = k.checked_sub(1).and_then(|p| tokens.get(p));
        let next = tokens.get(k + 1);
        let after_dot = prev.is_some_and(|p| p.is_punct('.'));
        match token {
            Token::Unterminated => found.push("an unterminated quote".to_owned()),
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => {
                if cast_depths.last() == Some(&depth) {
                    cast_depths.pop();
                }
                depth = depth.saturating_sub(1);
            }
            Token::Word(word) => {
                let lower = word.to_ascii_lowercase();
                let calls = next.is_some_and(|n| n.is_punct('('));
                if lower == "operator" && calls {
                    let qualified_operator = qualified_at(&tokens, k + 2)
                        && matches!(tokens.get(k + 4), Some(Token::Operator(_)))
                        && tokens.get(k + 5).is_some_and(|t| t.is_punct(')'));
                    if qualified_operator {
                        operator_end = Some(k + 6);
                    } else {
                        found.push("OPERATOR(...) without pg_catalog".to_owned());
                    }
                    continue;
                }
                if calls {
                    // A column alias list (`AS x(a, b)`, `WITH ORDINALITY
                    // x(a, b)`, `f(...) x(a, b)`) is no call.
                    let alias = prev.is_some_and(|p| {
                        p.is_word("as") || p.is_word("ordinality") || p.is_punct(')')
                    });
                    let qualifier_ok = k >= 2 && after_dot && tokens[k - 2].is_word("pg_catalog");
                    if lower == "cast" {
                        cast_depths.push(depth + 1);
                    } else if lower == "nullif" {
                        found.push("NULLIF (looks = up by name)".to_owned());
                    } else if !alias
                        && !qualifier_ok
                        && (after_dot || !PAREN_KEYWORDS.contains(&lower.as_str()))
                    {
                        found.push(format!("the call {word}(...) without pg_catalog"));
                    }
                }
                // `type 'literal'` resolves the type through search_path.
                if matches!(next, Some(Token::Str(_)))
                    && !STRING_KEYWORDS.contains(&lower.as_str())
                    && !KEYWORD_TYPES.contains(&lower.as_str())
                    && !(k >= 2 && after_dot && tokens[k - 2].is_word("pg_catalog"))
                {
                    found.push(format!("the typed literal {word} '...' without pg_catalog"));
                }
                if lower.starts_with("pg_")
                    && lower != "pg_catalog"
                    && !after_dot
                    && !calls
                    && !next.is_some_and(|n| n.is_punct('.'))
                {
                    found.push(format!("the relation {word} without pg_catalog"));
                }
                if lower == "pg_catalog" && !next.is_some_and(|n| n.is_punct('.')) {
                    found.push("pg_catalog without a name".to_owned());
                }
                match lower.as_str() {
                    // `x IN (...)`, `x IN {list}`; prose such as "in a
                    // schema" or `TABLES IN SCHEMA` is no operator.
                    "in" if next.is_some_and(|n| n.is_punct('(') || *n == Token::Placeholder) => {
                        found.push(format!("{word} (looks an operator up by name)"));
                    }
                    "like" | "ilike" | "similar" | "between" | "natural" => {
                        found.push(format!("{word} (looks an operator up by name)"));
                    }
                    "using" if next.is_some_and(|n| n.is_punct('(')) => {
                        found.push("JOIN ... USING (looks = up by name)".to_owned());
                    }
                    "distinct" if prev.is_some_and(|p| p.is_word("is") || p.is_word("not")) => {
                        found.push("IS DISTINCT FROM (looks = up by name)".to_owned());
                    }
                    "case" if !next.is_some_and(|n| n.is_word("when")) => {
                        found.push("a simple CASE (looks = up by name)".to_owned());
                    }
                    "collate" if !next.is_some_and(|_| type_ok(&tokens, k + 1)) => {
                        found.push("COLLATE without pg_catalog".to_owned());
                    }
                    "as" if cast_depths.last() == Some(&depth) && !type_ok(&tokens, k + 1) => {
                        found.push("CAST(... AS type) without pg_catalog".to_owned());
                    }
                    _ => {}
                }
            }
            Token::Quoted(name) => {
                let qualifier_ok = k >= 2 && after_dot && tokens[k - 2].is_word("pg_catalog");
                if next.is_some_and(|n| n.is_punct('(')) && !qualifier_ok {
                    found.push(format!("the call \"{name}\"(...) without pg_catalog"));
                }
            }
            Token::Placeholder => {
                // `{f}(...)`: a name the code fills in is not checked. A
                // column list (`AS {x}(a, b)`, `INSERT INTO {t} (a)`,
                // `ON {t} (a)`, `REFERENCES {t} (a)`) is no call.
                let column_list = prev.is_some_and(|p| {
                    [
                        "as",
                        "ordinality",
                        "into",
                        "on",
                        "references",
                        "table",
                        "key",
                    ]
                    .iter()
                    .any(|w| p.is_word(w))
                });
                if next.is_some_and(|n| n.is_punct('(')) && !after_dot && !column_list {
                    found.push("a placeholder used as a function name".to_owned());
                }
                // `t.a {op} 'x'`: an operator the code fills in, between
                // two things that can only be operands.
                if is_operand_end(&tokens, k) && is_operand_start(&tokens, k) {
                    found.push("a placeholder used as an operator".to_owned());
                }
            }
            Token::Cast => {
                if !type_ok(&tokens, k + 1) {
                    found.push("a cast to a type without pg_catalog".to_owned());
                }
            }
            Token::Str(content) => {
                let lookup = matches!(tokens.get(k + 1), Some(Token::Cast))
                    && qualified_at(&tokens, k + 2)
                    && tokens
                        .get(k + 4)
                        .and_then(Token::word)
                        .is_some_and(|ty| NAME_LOOKUP_TYPES.contains(&ty.as_str()));
                // `'{catalog}'`: the name comes from code and is checked
                // where it is written.
                let placeholder = content.starts_with('{')
                    && content.ends_with('}')
                    && content.matches('{').count() == 1
                    && content.matches('}').count() == 1;
                if lookup && !content.contains('.') && !placeholder {
                    found.push(format!("'{content}' resolved through search_path"));
                }
            }
            Token::Operator(op) => {
                if operator_end.is_some_and(|end| k + 1 < end) {
                    continue;
                }
                let operand_before = prev.is_some_and(|p| {
                    matches!(
                        p,
                        Token::Word(_)
                            | Token::Quoted(_)
                            | Token::Str(_)
                            | Token::Number
                            | Token::Placeholder
                    ) || p.is_punct(')')
                        || p.is_punct(']')
                }) && !prev.is_some_and(|p| {
                    p.word().is_some_and(|w| {
                        matches!(
                            w.as_str(),
                            "select"
                                | "distinct"
                                | "then"
                                | "else"
                                | "when"
                                | "and"
                                | "or"
                                | "not"
                                | "in"
                                | "any"
                                | "all"
                                | "by"
                                | "returning"
                        )
                    })
                });
                let after_operator = prev.is_some_and(|p| matches!(p, Token::Operator(_)))
                    || operator_end == Some(k);
                // `*` as "every column": `SELECT *`, `q.*`, `count(*)`.
                let star = op == "*"
                    && prev.is_none_or(|p| {
                        p.is_punct('.')
                            || p.is_punct('(')
                            || p.is_punct(',')
                            || p.is_word("select")
                            || p.is_word("distinct")
                    });
                // A negative number constant (the grammar negates it
                // without an operator).
                let negative = op == "-"
                    && matches!(next, Some(Token::Number))
                    && (!operand_before || after_operator);
                if !star && !negative {
                    found.push(format!(
                        "the operator {op} without OPERATOR(pg_catalog.{op})"
                    ));
                }
            }
            _ => {}
        }
    }
    found
}

/// The crate source directories checked, relative to `crates/`.
const ROOTS: &[&str] = &["catalog/src", "live-stats/src"];

/// Every `.rs` file under `dir`, sorted.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The path of a source file relative to `crates/`, with `/` separators.
fn relative(crates: &Path, path: &Path) -> String {
    path.strip_prefix(crates)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// True when the exclusion `entry` (a file, or a directory ending in `/`)
/// covers `file`.
fn excludes(entry: &str, file: &str) -> bool {
    if entry.ends_with('/') {
        file.starts_with(entry)
    } else {
        file == entry
    }
}

#[test]
fn catalog_sql_resolves_every_name_in_pg_catalog() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for root in ROOTS {
        rust_files(&crates.join(root), &mut files);
    }
    assert!(files.len() > 40, "found only {files:?}");
    let mut report = String::new();
    let mut used = vec![false; NOT_CATALOG_SQL.len()];
    let mut excluded = vec![false; NOT_PG_SQL_FILES.len()];
    let mut checked = 0usize;
    for path in &files {
        let relative = relative(&crates, path);
        if let Some(index) = NOT_PG_SQL_FILES
            .iter()
            .position(|(entry, _)| excludes(entry, &relative))
        {
            excluded[index] = true;
            continue;
        }
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let literals = literals(&source).unwrap_or_else(|error| panic!("{relative}: {error}"));
        for literal in literals {
            if let Some(index) = NOT_CATALOG_SQL
                .iter()
                .position(|(file, text)| *file == relative && *text == literal.text)
            {
                used[index] = true;
                continue;
            }
            checked += 1;
            for problem in problems(&literal.text) {
                let _ = writeln!(
                    report,
                    "{relative}:{}: {problem} in {:?}",
                    literal.line, literal.text
                );
            }
        }
    }
    for (index, (file, text)) in NOT_CATALOG_SQL.iter().enumerate() {
        if !used[index] {
            let _ = writeln!(report, "{file}: the exemption {text:?} matches no literal");
        }
    }
    for (index, (entry, _)) in NOT_PG_SQL_FILES.iter().enumerate() {
        if !excluded[index] {
            let _ = writeln!(report, "{entry}: the file exclusion matches no file");
        }
    }
    assert!(checked > 1000, "only {checked} literals were checked");
    assert!(
        report.is_empty(),
        "PostgreSQL SQL that resolves names through search_path:\n{report}"
    );
}

#[test]
fn the_check_finds_what_it_must() {
    let bad = [
        (
            "SELECT obj_description(c.oid, 'pg_class')",
            "the call obj_description",
        ),
        ("SELECT public.f(1)", "the call f"),
        (
            "SELECT \"format_type\"(1, NULL)",
            "the call \"format_type\"",
        ),
        ("WHERE a.x = b.y", "the operator ="),
        ("SELECT a || 'x'", "the operator ||"),
        ("SELECT a OPERATOR(=) b", "OPERATOR(...) without pg_catalog"),
        ("WHERE k - 1 > 0", "the operator -"),
        ("WHERE x IN ('a', 'b')", "IN"),
        ("WHERE p.prokind IN {kinds}", "IN"),
        ("WHERE n.nspname LIKE 'pg\\_%'", "LIKE"),
        ("SELECT NULLIF(a, '')", "NULLIF"),
        ("WHERE a IS DISTINCT FROM b", "IS DISTINCT FROM"),
        ("SELECT CASE x WHEN 1 THEN 2 END", "a simple CASE"),
        ("FROM a JOIN b USING (id)", "JOIN ... USING"),
        ("SELECT x::text", "a cast to a type"),
        ("SELECT x::\"char\"", "a cast to a type"),
        ("SELECT NULL::{null} AS x", "a cast to a type"),
        ("SELECT CAST(x AS regclass)", "CAST(... AS type)"),
        ("ORDER BY x COLLATE \"C\"", "COLLATE"),
        ("FROM pg_class c", "the relation pg_class"),
        (
            "WHERE c.classid = 'pg_proc'::pg_catalog.regclass",
            "'pg_proc' resolved",
        ),
        ("SELECT 'it", "an unterminated quote"),
        (
            "SELECT public.\"format_type\"(1, NULL)",
            "the call \"format_type\"",
        ),
        ("SELECT {f}(x)", "a placeholder used as a function name"),
        ("WHERE t.a {op} 'b'", "a placeholder used as an operator"),
        (
            "WHERE c.oid = regclass 'pg_class'",
            "the typed literal regclass",
        ),
        ("SELECT text 'x'", "the typed literal text"),
    ];
    for (sql, expected) in bad {
        let found = problems(sql);
        assert!(
            found.iter().any(|p| p.contains(expected)),
            "{sql:?}: expected {expected:?}, found {found:?}"
        );
    }
    let good = [
        "SELECT pg_catalog.obj_description(c.oid, 'pg_class') FROM pg_catalog.pg_class c",
        "WHERE a.x OPERATOR(pg_catalog.=) b.y AND k OPERATOR(pg_catalog.>) -1",
        "SELECT q.*, pg_catalog.count(*) FROM (SELECT * FROM t) q",
        "SELECT x::pg_catalog.text, y::bigint, CAST(z AS pg_catalog.\"char\")",
        "SELECT COALESCE(a, b), EXISTS (SELECT 1), ARRAY(SELECT 1)",
        "FROM pg_catalog.unnest(a) WITH ORDINALITY x(n, o)",
        "WHERE c.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass",
        "WHERE x OPERATOR(pg_catalog.=) ANY (ARRAY['a', 'b']::pg_catalog.name[])",
        "SELECT CASE WHEN a THEN -1 ELSE 2 END, 'it''s', E'a\\'b'",
        "SELECT {expr} AS x, '{catalog}'::pg_catalog.regclass",
        "the objects in a schema, TABLES IN SCHEMA s",
        "SELECT pg_catalog.\"format_type\"(1, NULL), pg_catalog.text 'x', interval '1 day'",
        "SELECT {columns} FROM {table} t WHERE {filter} AND x OPERATOR(pg_catalog.~~) 'a' ORDER BY {order}",
    ];
    for sql in good {
        assert_eq!(problems(sql), Vec::<String>::new(), "{sql:?}");
    }
}

#[test]
fn literals_are_read_like_rustc_reads_them() {
    let source = "let a = \"x \\\n    y\";\n// \"comment\"\nlet c = 'q';\nlet r = r#\"a\"b\"#;\n\
                  #[cfg(test)]\nmod tests {\n    const T: &str = \"hidden\";\n}\nlet z = \"seen\";\n";
    let found: Vec<String> = literals(source)
        .unwrap_or_default()
        .into_iter()
        .map(|literal| literal.text)
        .collect();
    assert_eq!(found, vec!["x y", "a\"b", "seen"]);
}
