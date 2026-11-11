//! MySQL and MariaDB editors: typed changes from the UI turned into SQL
//! statements for preview and execution.
//!
//! Every editor takes a [`MyChange`] and returns an [`EditPlan`] through
//! [`plan`]. The plan is computed offline from the change and the server
//! version: nothing is read from the server, so the UI can preview the SQL
//! while the user edits. Names and values are quoted only through
//! [`crate::sql`]; expressions, view queries and routine bodies are SQL the
//! user wrote and are passed through as such (trimmed, without a trailing
//! `;`).
//!
//! MySQL and MariaDB DDL commits implicitly, so plans are never
//! transactional. Where the two engines differ (design doc 4.2) the
//! generators pick the right syntax per engine and version or refuse the
//! change with [`CatalogError::Unsupported`]:
//!
//! - invisible indexes are `INVISIBLE` on MySQL and `IGNORED` on MariaDB;
//! - functional key parts, `NOT ENFORCED` checks, database `READ ONLY` and
//!   `ENCRYPTION` exist only on MySQL;
//! - sequences, system versioning, application-time periods, database
//!   comments, `CREATE OR REPLACE` for routines and triggers and
//!   `ALGORITHM=NOCOPY` exist only on MariaDB;
//! - MySQL replaces routines and triggers by dropping and creating them,
//!   which the plan marks as destructive; a change of only the comment,
//!   data access or SQL security of a routine uses [`AlterRoutine`] on both
//!   engines, which keeps the routine and its grants (MySQL ignores
//!   `COMMENT ''` there, so it cannot remove a routine comment in place).

mod alter;
mod cost;
mod database;
mod event;
mod index;
mod routine;
mod sequence;
mod table;
mod trigger;
mod types;
mod view;

use serde::{Deserialize, Serialize};
use varve_core::Engine;
use varve_driver::{Dialect, ServerInfo};

use crate::model::{CatalogError, CatalogResult, EditPlan};
use crate::sql;

pub use alter::{AlgorithmOption, AlterOp, AlterTable, ColumnPosition, LockOption};
pub use database::{AlterDatabase, CreateDatabase, DropDatabase};
pub use event::{
    AlterEvent, CreateEvent, DropEvent, EventSchedule, EventStatus, EventTime, Interval,
    IntervalUnit, TimeBase,
};
pub use index::{CreateIndex, DropIndex, RenameIndex, ReplaceIndex, SetIndexVisibility};
pub use routine::{
    AlterRoutine, CreateRoutine, DataAccess, DropRoutine, ParameterMode, RoutineCharacteristics,
    RoutineKind, RoutineParameter,
};
pub use sequence::{
    AlterSequence, CreateSequence, DropSequence, SequenceLimit, SequenceNumber, SequenceRestart,
    SequenceType, SetSequenceValue,
};
pub use table::{
    ApplicationPeriod, CheckDef, CreateTable, DropTable, ForeignKeyDef, PartitionBound,
    PartitionDef, PartitionMethod, PartitionValues, Partitioning, ReferenceAction, RowFormat,
    SubPartitionDef, SubPartitionMethod, SubPartitioning, SystemVersioning, TableCompression,
    TableConstraint, TableOptions, Tristate, VersioningColumns,
};
pub use trigger::{CreateTrigger, DropTrigger, TriggerEvent, TriggerOrder, TriggerTiming};
pub use types::{
    ColumnDef, ColumnDefault, DataType, Definer, Generated, GeneratedStorage, IndexDef, IndexKind,
    IndexMethod, KeyPart, RowPeriod, SqlSecurity, TypeName,
};
pub use view::{CheckOption, CreateView, DropView, ViewAlgorithm};

/// One change from a MySQL or MariaDB editor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "change",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MyChange {
    /// `CREATE DATABASE`.
    CreateDatabase(CreateDatabase),
    /// `ALTER DATABASE`.
    AlterDatabase(AlterDatabase),
    /// `DROP DATABASE`.
    DropDatabase(DropDatabase),
    /// `CREATE TABLE` from the table designer.
    CreateTable(Box<CreateTable>),
    /// `ALTER TABLE` from the table designer.
    AlterTable(AlterTable),
    /// `DROP TABLE`.
    DropTable(DropTable),
    /// `CREATE INDEX`.
    CreateIndex(CreateIndex),
    /// `DROP INDEX`.
    DropIndex(DropIndex),
    /// Renames an index.
    RenameIndex(RenameIndex),
    /// Makes an index invisible (MySQL) or ignored (MariaDB), or visible again.
    SetIndexVisibility(SetIndexVisibility),
    /// Replaces an index definition in one statement (drop and add).
    ReplaceIndex(ReplaceIndex),
    /// `CREATE [OR REPLACE] VIEW`.
    CreateView(CreateView),
    /// `DROP VIEW`.
    DropView(DropView),
    /// `CREATE PROCEDURE` or `CREATE FUNCTION`, optionally replacing one.
    CreateRoutine(CreateRoutine),
    /// `DROP PROCEDURE` or `DROP FUNCTION`.
    DropRoutine(DropRoutine),
    /// `ALTER PROCEDURE` or `ALTER FUNCTION`: comment, data access and SQL
    /// security of an existing routine, keeping its grants.
    AlterRoutine(AlterRoutine),
    /// `CREATE TRIGGER`, optionally replacing one.
    CreateTrigger(CreateTrigger),
    /// `DROP TRIGGER`.
    DropTrigger(DropTrigger),
    /// `CREATE EVENT`.
    CreateEvent(CreateEvent),
    /// `ALTER EVENT`.
    AlterEvent(AlterEvent),
    /// `DROP EVENT`.
    DropEvent(DropEvent),
    /// `CREATE SEQUENCE` (MariaDB).
    CreateSequence(CreateSequence),
    /// `ALTER SEQUENCE` (MariaDB).
    AlterSequence(AlterSequence),
    /// `DROP SEQUENCE` (MariaDB).
    DropSequence(DropSequence),
    /// `SETVAL` on a sequence (MariaDB).
    SetSequenceValue(SetSequenceValue),
}

/// The SQL for one change on a MySQL or MariaDB server.
///
/// # Errors
///
/// [`CatalogError::Unsupported`] when the server is not MySQL or MariaDB,
/// or its engine or version lacks a feature the change uses;
/// [`CatalogError::Invalid`] when the change cannot be expressed (an empty
/// name, a length a type does not take, a missing body, ...).
pub fn plan(server: &ServerInfo, change: &MyChange) -> CatalogResult<EditPlan> {
    let target = Target::new(server)?;
    let mut plan = Plan {
        execute_immediate: target.has_execute_immediate(),
        ..Plan::default()
    };
    match change {
        MyChange::CreateDatabase(c) => database::create(&target, c, &mut plan)?,
        MyChange::AlterDatabase(c) => database::alter(&target, c, &mut plan)?,
        MyChange::DropDatabase(c) => database::drop(c, &mut plan)?,
        MyChange::CreateTable(c) => table::create(&target, c, &mut plan)?,
        MyChange::AlterTable(c) => alter::alter(&target, c, &mut plan)?,
        MyChange::DropTable(c) => table::drop(c, &mut plan)?,
        MyChange::CreateIndex(c) => index::create(&target, c, &mut plan)?,
        MyChange::DropIndex(c) => index::drop(&target, c, &mut plan)?,
        MyChange::RenameIndex(c) => index::rename(c, &mut plan)?,
        MyChange::SetIndexVisibility(c) => index::visibility(&target, c, &mut plan)?,
        MyChange::ReplaceIndex(c) => index::replace(&target, c, &mut plan)?,
        MyChange::CreateView(c) => view::create(c, &mut plan)?,
        MyChange::DropView(c) => view::drop(c, &mut plan)?,
        MyChange::CreateRoutine(c) => routine::create(&target, c, &mut plan)?,
        MyChange::DropRoutine(c) => routine::drop(c, &mut plan)?,
        MyChange::AlterRoutine(c) => routine::alter(&target, c, &mut plan)?,
        MyChange::CreateTrigger(c) => trigger::create(&target, c, &mut plan)?,
        MyChange::DropTrigger(c) => trigger::drop(c, &mut plan)?,
        MyChange::CreateEvent(c) => event::create(&target, c, &mut plan)?,
        MyChange::AlterEvent(c) => event::alter(&target, c, &mut plan)?,
        MyChange::DropEvent(c) => event::drop(c, &mut plan)?,
        MyChange::CreateSequence(c) => sequence::create(&target, c, &mut plan)?,
        MyChange::AlterSequence(c) => sequence::alter(&target, c, &mut plan)?,
        MyChange::DropSequence(c) => sequence::drop(&target, c, &mut plan)?,
        MyChange::SetSequenceValue(c) => sequence::set_value(&target, c, &mut plan)?,
    }
    plan.finish()
}

/// The engine and full version the SQL is generated for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target {
    mariadb: bool,
    version: (u32, u32, u32),
}

impl Target {
    fn new(server: &ServerInfo) -> CatalogResult<Self> {
        let mariadb = match server.engine {
            Engine::Mysql => false,
            Engine::Mariadb => true,
            Engine::Postgres | Engine::Redis => {
                return Err(unsupported(
                    "the MySQL editors only generate SQL for MySQL and MariaDB",
                ));
            }
        };
        Ok(Self {
            mariadb,
            // A missing patch level counts as `.0`, so a feature that
            // arrived in a patch release is only used when it is known to
            // be there.
            version: (server.major, server.minor, server.patch.unwrap_or(0)),
        })
    }

    /// True on MySQL `major.minor.patch` or newer (never on MariaDB).
    fn mysql_at(&self, major: u32, minor: u32, patch: u32) -> bool {
        !self.mariadb && self.version >= (major, minor, patch)
    }

    /// True on MariaDB `major.minor` or newer (never on MySQL).
    fn mariadb_at(&self, major: u32, minor: u32) -> bool {
        self.mariadb && (self.version.0, self.version.1) >= (major, minor)
    }

    fn is_mariadb(&self) -> bool {
        self.mariadb
    }

    /// True when the server has `EXECUTE IMMEDIATE` (MariaDB 10.2.3 and
    /// newer, never MySQL).
    fn has_execute_immediate(&self) -> bool {
        self.mariadb && self.version >= (10, 2, 3)
    }

    /// Product and version for messages, for example `MySQL 8.0.12`.
    fn describe(&self) -> String {
        let product = if self.mariadb { "MariaDB" } else { "MySQL" };
        let (major, minor, patch) = self.version;
        format!("{product} {major}.{minor}.{patch}")
    }

    /// Error for a feature this server lacks.
    fn lacks(&self, feature: &str) -> CatalogError {
        unsupported(format!("{} does not support {feature}", self.describe()))
    }
}

/// Statements, flags and warnings collected while generating a plan.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    statements: Vec<String>,
    destructive: bool,
    warnings: Vec<String>,
    /// True when a statement built at run time can run through
    /// `EXECUTE IMMEDIATE` ([`Target::has_execute_immediate`]).
    execute_immediate: bool,
}

impl Plan {
    fn push(&mut self, statement: String) {
        self.statements.push(statement);
    }

    fn destructive(&mut self) {
        self.destructive = true;
    }

    fn warn(&mut self, warning: impl Into<String>) {
        let warning = warning.into();
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }

    /// The plan. A statement holding [`table_text`] marks is replaced by
    /// the statements that build it at run time ([`runtime_statement`]).
    fn finish(mut self) -> CatalogResult<EditPlan> {
        let mut statements = Vec::with_capacity(self.statements.len());
        for statement in std::mem::take(&mut self.statements) {
            if statement.contains(RUNTIME_TEXT) {
                runtime_statement(&statement, &mut statements, &mut self)?;
            } else {
                statements.push(statement);
            }
        }
        Ok(EditPlan {
            statements,
            transactional: false,
            destructive: self.destructive,
            warnings: self.warnings,
        })
    }
}

fn invalid(message: impl Into<String>) -> CatalogError {
    CatalogError::Invalid {
        message: message.into(),
    }
}

fn unsupported(message: impl Into<String>) -> CatalogError {
    CatalogError::Unsupported {
        message: message.into(),
    }
}

/// A quoted identifier.
fn ident(name: &str) -> CatalogResult<String> {
    sql::ident(Dialect::Mysql, name)
}

/// A comma separated list of quoted identifiers.
fn ident_list(names: &[String]) -> CatalogResult<String> {
    if names.is_empty() {
        return Err(invalid("a list of names cannot be empty"));
    }
    Ok(names
        .iter()
        .map(|name| ident(name))
        .collect::<CatalogResult<Vec<_>>>()?
        .join(", "))
}

/// `database`.`name`.
fn object_name(database: &str, name: &str) -> CatalogResult<String> {
    sql::qualified(Dialect::Mysql, &[Some(database), Some(name)])
}

/// A string literal where the grammar accepts any expression (defaults,
/// partition bounds, event times): every text is allowed.
fn value_text(text: &str) -> CatalogResult<String> {
    sql::string(Dialect::Mysql, text)
}

/// A string literal where the grammar accepts only a plain quoted string,
/// in statements MySQL cannot prepare (routine, event, database and
/// sequence comments, account names, intervals). A backslash is read
/// differently with and without `NO_BACKSLASH_ESCAPES` and the
/// mode-independent hex form is not accepted in these places, so such text
/// is refused rather than stored differently from what the user typed.
/// Table statements use [`table_text`], which keeps such text.
fn plain_text(text: &str, what: &str) -> CatalogResult<String> {
    if text.contains('\\') || text.contains('\0') {
        return Err(invalid(format!(
            "{what} cannot contain a backslash or NUL character"
        )));
    }
    sql::string(Dialect::Mysql, text)
}

/// Delimits text that [`table_text`] leaves for [`runtime_statement`] to
/// quote. Nothing else in a plan holds a NUL character: names, literals and
/// SQL fragments refuse it or never write it.
const RUNTIME_TEXT: char = '\0';

/// A string literal where the grammar accepts only a plain quoted string,
/// inside a statement MySQL can prepare (`ALTER TABLE`, `CREATE TABLE`,
/// `CREATE INDEX`): column, index and partition comments, `ENUM` / `SET`
/// members, data and index directories.
///
/// A backslash is read differently with and without `NO_BACKSLASH_ESCAPES`,
/// the plan does not know the mode the session will have when it runs, and
/// the mode-independent hex form is not accepted in these places. So text
/// with a backslash is only marked here, and [`Plan::finish`] has the whole
/// statement built at run time with the text quoted for the session's mode
/// ([`runtime_statement`]). Restating an existing column whose comment or
/// `ENUM` member holds a backslash (every `MODIFY` / `CHANGE COLUMN` does)
/// then keeps that text exactly.
fn table_text(text: &str, what: &str) -> CatalogResult<String> {
    if text.contains('\0') {
        return Err(invalid(format!("{what} cannot contain a NUL character")));
    }
    if text.contains('\\') {
        return Ok(format!("{RUNTIME_TEXT}{text}{RUNTIME_TEXT}"));
    }
    sql::string(Dialect::Mysql, text)
}

/// Statements MySQL and MariaDB can prepare, among those the editors write
/// with [`table_text`] values.
const PREPARABLE: &[&str] = &[
    "ALTER TABLE ",
    "CREATE TABLE ",
    "CREATE INDEX ",
    "CREATE UNIQUE INDEX ",
    "CREATE FULLTEXT INDEX ",
    "CREATE SPATIAL INDEX ",
];

/// The five statements [`runtime_statement`] writes on a server without
/// `EXECUTE IMMEDIATE`: the first starts with `PREPARED_SET`, the fourth
/// with `PREPARED_EXECUTE`. The user variable and the prepared statement
/// have one fixed name for every such statement: see [`runtime_statement`]
/// for why.
const PREPARED_SET: &str = "SET @varve_ddl = ";
const PREPARED_PREPARE: &str = "PREPARE varve_ddl FROM @varve_ddl";
const PREPARED_CLEAR: &str = "SET @varve_ddl = NULL";
const PREPARED_EXECUTE: &str = "EXECUTE varve_ddl /* ";
const PREPARED_DEALLOCATE: &str = "DEALLOCATE PREPARE varve_ddl";
const PREPARED_STATEMENTS: usize = 5;

/// The one statement [`runtime_statement`] writes with `EXECUTE IMMEDIATE`
/// starts with this.
const IMMEDIATE: &str = "EXECUTE IMMEDIATE ";

/// Around the readable form of a statement built at run time, in the line
/// that executes it.
const SHOWN_OPEN: &str = " /* ";
const SHOWN_CLOSE: &str = " */";

/// Adds the statements that build `statement` (holding [`table_text`]
/// marks) at run time and execute it: the SQL around the texts travels as
/// hex literals (read the same in every mode), each text is quoted for the
/// mode the session has when it runs ([`quote_for_mode`]), and the result is
/// executed as a statement. The line that executes it carries the statement
/// in readable form as a comment, which [`shown_statements`] reads back for
/// the confirmation, the preview and the activity log.
///
/// A MariaDB `SET STATEMENT system_versioning_alter_history='KEEP' FOR`
/// prefix (system-versioned tables) is part of the statement built at run
/// time: MariaDB prepares `SET STATEMENT ... FOR` like the statement it
/// wraps, and the setting then covers exactly that `ALTER TABLE`.
///
/// With `EXECUTE IMMEDIATE` (MariaDB 10.2.3 and newer) this is one
/// statement that leaves nothing in the session, whether it fails or not.
///
/// Elsewhere (MySQL) it takes five: set a user variable, `PREPARE` from it,
/// clear it, `EXECUTE`, `DEALLOCATE PREPARE`. A plan is a plain list of
/// statements and its runner stops at the first failure. A failed `PREPARE`
/// (a syntax error in a default or CHECK expression of the same statement)
/// therefore leaves the statement text in the variable, and a failed
/// `EXECUTE` (duplicate key, truncated data, a lock wait timeout) leaves
/// the prepared statement. The runner removes both after a failure with
/// the statements of [`cleanup_statements`]. Should that fail too (a lost
/// connection ends the session anyway), both have one fixed name, so the
/// next statement built at run time in that session cleans up as its first
/// steps: its `SET` overwrites the variable and its `PREPARE` deallocates a
/// prepared statement of the same name before preparing the new one.
fn runtime_statement(statement: &str, out: &mut Vec<String>, plan: &mut Plan) -> CatalogResult<()> {
    let parts: Vec<&str> = statement.split(RUNTIME_TEXT).collect();
    if parts.len().is_multiple_of(2) {
        return Err(CatalogError::Unexpected {
            message: "a statement holds an unterminated run-time text".into(),
        });
    }
    let head = parts.first().copied().unwrap_or_default();
    let head = head.strip_prefix(alter::KEEP_HISTORY).unwrap_or(head);
    if !PREPARABLE.iter().any(|p| head.starts_with(p)) {
        return Err(invalid(
            "a comment, ENUM or SET member or directory with a backslash can only be written in \
             ALTER TABLE, CREATE TABLE and CREATE INDEX statements",
        ));
    }
    let mut pieces = Vec::with_capacity(parts.len());
    let mut shown = String::with_capacity(statement.len());
    for (index, part) in parts.iter().enumerate() {
        if index.is_multiple_of(2) {
            if !part.is_empty() {
                pieces.push(hex_text(part));
            }
            shown.push_str(part);
        } else {
            pieces.push(quote_for_mode(&hex_text(part)));
            shown.push('\'');
            shown.push_str(&part.replace('\'', "''"));
            shown.push('\'');
        }
    }
    plan.warn(
        "a comment, ENUM or SET member or directory holds a backslash, which the server reads differently by sql_mode: its statement is prepared at run time, so the text is stored exactly as typed",
    );
    let built = format!("CONCAT({})", pieces.join(", "));
    let shown = readable(&shown);
    if plan.execute_immediate {
        out.push(format!(
            "{IMMEDIATE}{built}{SHOWN_OPEN}{shown}{SHOWN_CLOSE}"
        ));
        return Ok(());
    }
    out.push(format!("{PREPARED_SET}{built}"));
    out.push(PREPARED_PREPARE.into());
    out.push(PREPARED_CLEAR.into());
    out.push(format!("{PREPARED_EXECUTE}{shown}{SHOWN_CLOSE}"));
    out.push(PREPARED_DEALLOCATE.into());
    Ok(())
}

/// The statements of a plan as the user reads them: the statements that
/// build and execute a statement at run time ([`runtime_statement`]) are
/// replaced by that one statement in readable form, every other statement
/// is kept as it is.
///
/// The readable form is for showing and classifying only. It is not what
/// runs: its texts are quoted with doubled quotes only (right for a session
/// with `NO_BACKSLASH_ESCAPES`, one backslash short per backslash
/// otherwise), and a `*/` in a text reads `* /`.
///
/// No other statement of a plan starts the way the run-time ones do: the
/// editors write `CREATE`, `ALTER`, `DROP`, `RENAME` and `SELECT SETVAL`
/// statements, and SQL the user wrote only ever sits inside those.
pub(crate) fn shown_statements(statements: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(statements.len());
    let mut rest = statements;
    while let Some((first, tail)) = rest.split_first() {
        if let Some(shown) = immediate_shown(first) {
            out.push(shown.to_owned());
            rest = tail;
        } else if let Some(shown) = prepared_shown(rest) {
            out.push(shown.to_owned());
            rest = &rest[PREPARED_STATEMENTS..];
        } else {
            out.push(first.clone());
            rest = tail;
        }
    }
    out
}

/// Where statement `index` of `statements` sits in [`shown_statements`]:
/// the position of the shown statement it belongs to, and whether that
/// shown statement has run by the time `index` runs (true only for the
/// `DEALLOCATE PREPARE` after the `EXECUTE` of a statement built at run
/// time). An index past the end gives the number of shown statements.
pub(crate) fn shown_position(statements: &[String], index: usize) -> (usize, bool) {
    let mut shown = 0;
    let mut at = 0;
    while at < statements.len() {
        let len = if prepared_shown(&statements[at..]).is_some() {
            PREPARED_STATEMENTS
        } else {
            1
        };
        if index < at + len {
            return (shown, index - at == PREPARED_STATEMENTS - 1);
        }
        at += len;
        shown += 1;
    }
    (shown, false)
}

/// The statements that remove what a failed statement built at run time
/// can leave in the session (the prepared statement and the user variable
/// of [`runtime_statement`]), for the runner to execute after a failure
/// with their errors ignored: each fails harmlessly when there is nothing
/// to remove. Empty when no statement of the plan uses `PREPARE` (none is
/// built at run time, or the server has `EXECUTE IMMEDIATE`).
pub(crate) fn cleanup_statements(statements: &[String]) -> Vec<String> {
    let prepared = (0..statements.len()).any(|at| prepared_shown(&statements[at..]).is_some());
    if prepared {
        vec![PREPARED_DEALLOCATE.into(), PREPARED_CLEAR.into()]
    } else {
        Vec::new()
    }
}

/// The readable form carried by an `EXECUTE IMMEDIATE` statement of
/// [`runtime_statement`].
fn immediate_shown(statement: &str) -> Option<&str> {
    // The built expression holds hex literals and fixed SQL only, so the
    // first comment opening is the one written here.
    let (_, shown) = statement.strip_prefix(IMMEDIATE)?.split_once(SHOWN_OPEN)?;
    shown.strip_suffix(SHOWN_CLOSE)
}

/// The readable form carried by the five statements of [`runtime_statement`]
/// at the start of `statements`.
fn prepared_shown(statements: &[String]) -> Option<&str> {
    let [set, prepare, clear, execute, deallocate] = statements.get(..PREPARED_STATEMENTS)? else {
        return None;
    };
    if !set.starts_with(PREPARED_SET)
        || prepare != PREPARED_PREPARE
        || clear != PREPARED_CLEAR
        || deallocate != PREPARED_DEALLOCATE
    {
        return None;
    }
    execute
        .strip_prefix(PREPARED_EXECUTE)?
        .strip_suffix(SHOWN_CLOSE)
}

/// An expression that quotes the text of `literal` (a mode-independent
/// literal) as a string literal for the session's `sql_mode` at run time:
/// with `NO_BACKSLASH_ESCAPES` only quotes are doubled, otherwise `QUOTE()`
/// also escapes backslashes.
fn quote_for_mode(literal: &str) -> String {
    format!(
        "IF(@@SESSION.sql_mode LIKE '%NO_BACKSLASH_ESCAPES%', \
         CONCAT('''', REPLACE({literal}, '''', ''''''), ''''), QUOTE({literal}))"
    )
}

/// Sets a table comment that holds a backslash. How the server reads a
/// backslash in a quoted string depends on the session's `sql_mode`
/// (`NO_BACKSLASH_ESCAPES`), the plan does not know that mode, and
/// `COMMENT` takes only a quoted string. So the comment gets an
/// `ALTER TABLE ... COMMENT=` statement of its own that is built at run
/// time, like every other statement with such a text: [`Plan::finish`]
/// hands it to [`runtime_statement`], which describes what a failed one
/// leaves in the session. `table` is the quoted table name; `comment` holds
/// no NUL character (`TableOptions::runtime_comment` refuses it).
pub(super) fn runtime_comment(table: &str, comment: &str, plan: &mut Plan) {
    plan.push(format!(
        "ALTER TABLE {table} COMMENT={RUNTIME_TEXT}{comment}{RUNTIME_TEXT}"
    ));
}

/// Text shown inside a `/* ... */` comment: a `*/` in it would end the
/// comment early, so its slash is spaced out.
fn readable(text: &str) -> String {
    text.replace("*/", "* /")
}

/// Text as a `_utf8mb4 X'...'` literal, read the same in every `sql_mode`.
fn hex_text(text: &str) -> String {
    let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();
    format!("_utf8mb4 X'{hex}'")
}

/// SQL written by the user (an expression, a query or a body): trimmed,
/// trailing semicolons removed, and never empty.
fn fragment(text: &str, what: &str) -> CatalogResult<String> {
    // No statement holds a NUL character (see `RUNTIME_TEXT`).
    if text.contains('\0') {
        return Err(invalid(format!("{what} cannot contain a NUL character")));
    }
    let mut body = text.trim();
    while let Some(rest) = body.strip_suffix(';') {
        body = rest.trim_end();
    }
    if body.is_empty() {
        return Err(invalid(format!("{what} cannot be empty")));
    }
    Ok(body.to_string())
}

#[cfg(test)]
pub(crate) mod test_support {
    use varve_core::Engine;
    use varve_driver::ServerInfo;

    use super::{MyChange, plan};
    use crate::model::{CatalogError, CatalogResult, EditPlan};

    pub fn mysql(major: u32, minor: u32, patch: u32) -> ServerInfo {
        ServerInfo {
            engine: Engine::Mysql,
            version: format!("{major}.{minor}.{patch}"),
            major,
            minor,
            patch: Some(patch),
        }
    }

    pub fn mariadb(major: u32, minor: u32, patch: u32) -> ServerInfo {
        ServerInfo {
            engine: Engine::Mariadb,
            version: format!("{major}.{minor}.{patch}-MariaDB"),
            major,
            minor,
            patch: Some(patch),
        }
    }

    pub fn my80() -> ServerInfo {
        mysql(8, 0, 46)
    }

    pub fn my97() -> ServerInfo {
        mysql(9, 7, 2)
    }

    pub fn ma106() -> ServerInfo {
        mariadb(10, 6, 28)
    }

    pub fn ma123() -> ServerInfo {
        mariadb(12, 3, 3)
    }

    /// The plan for a change given as JSON, the way the UI sends it.
    pub fn plan_json(server: &ServerInfo, json: &str) -> CatalogResult<EditPlan> {
        let change: MyChange = serde_json::from_str(json).map_err(|e| CatalogError::Invalid {
            message: e.to_string(),
        })?;
        plan(server, &change)
    }

    /// Statements of a plan joined by `;\n` for compact assertions.
    pub fn sql(plan: &EditPlan) -> String {
        plan.statements.join(";\n")
    }

    pub fn is_unsupported<T: std::fmt::Debug>(result: &CatalogResult<T>) -> bool {
        matches!(result, Err(CatalogError::Unsupported { .. }))
    }

    pub fn is_invalid<T: std::fmt::Debug>(result: &CatalogResult<T>) -> bool {
        matches!(result, Err(CatalogError::Invalid { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn refuses_other_engines() {
        let server = ServerInfo {
            engine: Engine::Postgres,
            version: "18.0".into(),
            major: 18,
            minor: 0,
            patch: None,
        };
        let change = MyChange::DropDatabase(DropDatabase {
            name: "x".into(),
            if_exists: false,
        });
        assert!(is_unsupported(&plan(&server, &change)));
    }

    #[test]
    fn versions_compare_with_patch() -> CatalogResult<()> {
        let t = Target::new(&mysql(8, 0, 13))?;
        assert!(t.mysql_at(8, 0, 13));
        assert!(!t.mysql_at(8, 0, 22));
        assert!(!t.mariadb_at(10, 0));
        let mut missing_patch = mysql(8, 0, 30);
        missing_patch.patch = None;
        assert!(!Target::new(&missing_patch)?.mysql_at(8, 0, 1));
        let m = Target::new(&ma106())?;
        assert!(m.mariadb_at(10, 6));
        assert!(!m.mariadb_at(10, 7));
        assert!(!m.mysql_at(5, 0, 0));
        assert_eq!(m.describe(), "MariaDB 10.6.28");
        Ok(())
    }

    #[test]
    fn fragments_and_text() -> CatalogResult<()> {
        assert_eq!(fragment("  select 1 ;; \n", "q")?, "select 1");
        assert!(fragment(" ; ", "q").is_err());
        assert_eq!(plain_text("it's", "c")?, "'it''s'");
        assert!(plain_text("a\\b", "c").is_err());
        assert_eq!(value_text("a\\b")?, "_utf8mb4 X'615c62'");
        assert_eq!(ident_list(&["a".into(), "b`c".into()])?, "`a`, `b``c`");
        assert!(ident_list(&[]).is_err());
        Ok(())
    }

    #[test]
    fn changes_deserialize_from_tagged_json() -> CatalogResult<()> {
        let plan = plan_json(
            &my80(),
            r#"{"change":"dropDatabase","name":"shop","ifExists":true}"#,
        )?;
        assert_eq!(sql(&plan), "DROP DATABASE IF EXISTS `shop`");
        assert!(!plan.transactional);
        Ok(())
    }
}
