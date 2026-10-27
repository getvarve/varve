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
    let mut plan = Plan::default();
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
    Ok(plan.finish())
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

    fn finish(self) -> EditPlan {
        EditPlan {
            statements: self.statements,
            transactional: false,
            destructive: self.destructive,
            warnings: self.warnings,
        }
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

/// A string literal where the grammar accepts only a plain quoted string
/// (comments, `ENUM` members, account names, paths). A backslash is read
/// differently with and without `NO_BACKSLASH_ESCAPES` and the
/// mode-independent hex form is not accepted in these places, so such text
/// is refused rather than stored differently from what the user typed.
fn plain_text(text: &str, what: &str) -> CatalogResult<String> {
    if text.contains('\\') || text.contains('\0') {
        return Err(invalid(format!(
            "{what} cannot contain a backslash or NUL character"
        )));
    }
    sql::string(Dialect::Mysql, text)
}

/// Sets a table comment that holds a backslash. How the server reads a
/// backslash in a quoted string depends on the session's `sql_mode`
/// (`NO_BACKSLASH_ESCAPES`), the plan does not know that mode, and
/// `COMMENT` takes only a quoted string. So the `ALTER TABLE ... COMMENT=`
/// statement is built at run time: the text travels as a hex literal (the
/// same in every mode), is quoted for the mode the session has when it
/// runs (`QUOTE()` escapes backslashes, the other branch only doubles
/// quotes), then prepared and executed. `table` is the quoted table name.
///
/// The user variable and the prepared statement get a name with a random
/// suffix, so they never meet the user's own. The variable is cleared right
/// after `PREPARE`, before anything can fail; only when `EXECUTE` itself
/// fails (the table went away, a lock wait timed out) does the prepared
/// statement stay in the session until it closes, unused. The `EXECUTE`
/// line carries the statement in readable form as a comment, for the plan
/// preview and the activity log.
pub(super) fn runtime_comment(table: &str, comment: &str, plan: &mut Plan) {
    let prefix = hex_text(&format!("ALTER TABLE {table} COMMENT="));
    let text = hex_text(comment);
    let name = format!("varve_ddl_{}", runtime_suffix());
    plan.warn(
        "the comment holds a backslash, which the server reads differently by sql_mode: it is set by a statement prepared at run time, so it is stored exactly as typed",
    );
    plan.push(format!(
        "SET @{name} = CONCAT({prefix}, IF(@@SESSION.sql_mode LIKE '%NO_BACKSLASH_ESCAPES%', \
         CONCAT('''', REPLACE({text}, '''', ''''''), ''''), QUOTE({text})))"
    ));
    plan.push(format!("PREPARE {name} FROM @{name}"));
    plan.push(format!("SET @{name} = NULL"));
    plan.push(format!(
        "EXECUTE {name} /* ALTER TABLE {} COMMENT='{}' */",
        readable(table),
        readable(&comment.replace('\'', "''"))
    ));
    plan.push(format!("DEALLOCATE PREPARE {name}"));
}

/// Text shown inside a `/* ... */` comment: a `*/` in it would end the
/// comment early, so its slash is spaced out.
fn readable(text: &str) -> String {
    text.replace("*/", "* /")
}

/// Eight hex digits that differ per call (a counter mixed with the clock),
/// for names that must not collide with the user's.
fn runtime_suffix() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    format!(
        "{:08x}",
        nanos.rotate_left(13) ^ count.wrapping_mul(0x9e37_79b9)
    )
}

/// Text as a `_utf8mb4 X'...'` literal, read the same in every `sql_mode`.
fn hex_text(text: &str) -> String {
    let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();
    format!("_utf8mb4 X'{hex}'")
}

/// SQL written by the user (an expression, a query or a body): trimmed,
/// trailing semicolons removed, and never empty.
fn fragment(text: &str, what: &str) -> CatalogResult<String> {
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
