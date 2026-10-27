//! `ALTER TABLE` from the table designer: an ordered list of operations
//! combined into as few statements as is correct.
//!
//! Operations go into one `ALTER TABLE` until one of them depends on a
//! name an earlier operation of the same statement renamed (or the other
//! way round): the server resolves every clause of a statement against the
//! old definition, so `RENAME COLUMN a TO b, ADD INDEX (b)` fails while two
//! statements succeed. Partition maintenance and system-versioning changes
//! cannot share a statement with other changes and always get their own.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use varve_driver::Dialect;

use super::cost::{Level, column_change};
use super::table::{
    ApplicationPeriod, PartitionDef, Partitioning, TableConstraint, TableOptions,
    VersioningColumns, system_period,
};
use super::types::{
    ColumnDef, ColumnDefault, IndexDef, IndexKind, RowPeriod, TypeName, visibility_keyword,
};
use super::{Plan, Target, fragment, ident, ident_list, invalid, object_name};
use crate::model::CatalogResult;
use crate::sql;

/// Where an added, modified or changed column goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ColumnPosition {
    /// `FIRST`.
    First,
    /// `AFTER column`.
    After {
        /// The column it follows.
        column: String,
    },
}

impl ColumnPosition {
    fn sql(&self) -> CatalogResult<String> {
        Ok(match self {
            Self::First => " FIRST".into(),
            Self::After { column } => format!(" AFTER {}", ident(column)?),
        })
    }
}

/// `ALGORITHM=` of an `ALTER TABLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AlgorithmOption {
    /// `DEFAULT`: the server picks the cheapest.
    Default,
    /// `INSTANT`: metadata only.
    Instant,
    /// `INPLACE`: no table copy.
    Inplace,
    /// MariaDB: `NOCOPY`, in place without rebuilding the clustered index.
    Nocopy,
    /// `COPY`: copy the table.
    Copy,
}

/// `LOCK=` of an `ALTER TABLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LockOption {
    /// `DEFAULT`.
    Default,
    /// `NONE`: reads and writes continue.
    None,
    /// `SHARED`: reads continue.
    Shared,
    /// `EXCLUSIVE`.
    Exclusive,
}

/// One operation of an `ALTER TABLE`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum AlterOp {
    /// `ADD COLUMN`.
    AddColumn {
        /// The column.
        column: ColumnDef,
        /// Position; at the end when `None`.
        #[serde(default)]
        position: Option<ColumnPosition>,
    },
    /// `DROP COLUMN`.
    DropColumn {
        /// Column name.
        name: String,
        /// An index uses the column (it is then dropped or shrunk too, and
        /// the drop is no longer metadata only).
        #[serde(default)]
        indexed: bool,
    },
    /// `MODIFY COLUMN`: a new definition under the same name.
    ModifyColumn {
        /// The current definition, as the catalog describes it. For
        /// character columns give the character set on both definitions
        /// so the byte length is known (`utf8mb4` is assumed otherwise).
        previous: ColumnDef,
        /// The full new definition.
        column: ColumnDef,
        /// New position; unchanged when `None`.
        #[serde(default)]
        position: Option<ColumnPosition>,
        /// An index uses the column.
        #[serde(default)]
        indexed: bool,
    },
    /// `CHANGE COLUMN`: a new name and definition.
    ChangeColumn {
        /// The current definition (its name is the column's current name).
        previous: ColumnDef,
        /// The full new definition, with the new name.
        column: ColumnDef,
        /// New position; unchanged when `None`.
        #[serde(default)]
        position: Option<ColumnPosition>,
        /// An index uses the column.
        #[serde(default)]
        indexed: bool,
    },
    /// `RENAME COLUMN`.
    RenameColumn {
        /// Current name.
        old_name: String,
        /// New name.
        new_name: String,
    },
    /// `ALTER COLUMN ... SET DEFAULT` or `DROP DEFAULT`.
    SetColumnDefault {
        /// Column name.
        name: String,
        /// The column's type: MySQL takes defaults on `TEXT`, `BLOB`,
        /// `JSON` and spatial columns only as expressions.
        column_type: TypeName,
        /// The new default; `None` drops it.
        #[serde(default)]
        default: Option<ColumnDefault>,
    },
    /// MySQL 8.0.23+: `ALTER COLUMN ... SET [IN]VISIBLE`.
    SetColumnVisibility {
        /// Column name.
        name: String,
        /// Invisible when true.
        invisible: bool,
    },
    /// `ADD INDEX` (also unique, full-text and spatial).
    AddIndex {
        /// The index.
        index: IndexDef,
    },
    /// `DROP INDEX`.
    DropIndex {
        /// Index name.
        name: String,
    },
    /// `RENAME INDEX`.
    RenameIndex {
        /// Current name.
        old_name: String,
        /// New name.
        new_name: String,
    },
    /// `ALTER INDEX ... INVISIBLE` (MySQL) or `IGNORED` (MariaDB).
    SetIndexVisibility {
        /// Index name.
        name: String,
        /// Invisible when true.
        invisible: bool,
    },
    /// `ADD` a primary key, foreign key or check.
    AddConstraint {
        /// The constraint.
        constraint: TableConstraint,
    },
    /// `DROP PRIMARY KEY`.
    DropPrimaryKey,
    /// `DROP FOREIGN KEY`.
    DropForeignKey {
        /// Constraint name.
        name: String,
    },
    /// Drops a check constraint (`DROP CHECK` on MySQL, `DROP CONSTRAINT`
    /// on MariaDB).
    DropCheck {
        /// Constraint name.
        name: String,
    },
    /// MySQL 8.0.16+: `ALTER CHECK ... [NOT] ENFORCED`.
    SetCheckEnforced {
        /// Constraint name.
        name: String,
        /// Enforced when true.
        enforced: bool,
    },
    /// `RENAME TO`, optionally into another database.
    Rename {
        /// Target database; the table's own when `None`.
        #[serde(default)]
        database: Option<String>,
        /// New name.
        name: String,
    },
    /// Changes table options (engine, row format, comment, ...).
    SetOptions {
        /// The options to set; `None` fields stay as they are.
        options: TableOptions,
    },
    /// `CONVERT TO CHARACTER SET`: converts every character column.
    ConvertCharset {
        /// Character set.
        charset: String,
        /// Collation.
        #[serde(default)]
        collation: Option<String>,
    },
    /// MariaDB 10.4+: `ADD PERIOD FOR name (start, end)`.
    AddPeriod {
        /// The period.
        period: ApplicationPeriod,
    },
    /// MariaDB 10.4+: `DROP PERIOD FOR name`.
    DropPeriod {
        /// Period name.
        name: String,
    },
    /// `PARTITION BY ...` on an existing table.
    PartitionBy {
        /// The new partitioning.
        partitioning: Partitioning,
    },
    /// `REMOVE PARTITIONING`.
    RemovePartitioning,
    /// `ADD PARTITION (...)` for range and list partitioning.
    AddPartitions {
        /// The new partitions.
        partitions: Vec<PartitionDef>,
    },
    /// `ADD PARTITION PARTITIONS n` for hash and key partitioning.
    AddPartitionCount {
        /// Number of partitions to add.
        count: u32,
    },
    /// `DROP PARTITION`: deletes the rows in them.
    DropPartitions {
        /// Partition names.
        names: Vec<String>,
    },
    /// `TRUNCATE PARTITION`; every partition (`ALL`) when `names` is empty.
    TruncatePartitions {
        /// Partition names.
        #[serde(default)]
        names: Vec<String>,
    },
    /// `REORGANIZE PARTITION ... INTO (...)`.
    ReorganizePartitions {
        /// Partitions to split or merge.
        names: Vec<String>,
        /// Their replacements.
        into: Vec<PartitionDef>,
    },
    /// `COALESCE PARTITION n` for hash and key partitioning.
    CoalescePartitions {
        /// Number of partitions to remove.
        count: u32,
    },
    /// MariaDB: `ADD SYSTEM VERSIONING`, optionally with explicit row start
    /// and end columns.
    AddSystemVersioning {
        /// Explicit row start and end columns (with
        /// [`ColumnDef::row_period`] set); implicit hidden columns when
        /// `None`.
        #[serde(default)]
        columns: Option<Box<(ColumnDef, ColumnDef)>>,
    },
    /// MariaDB: `DROP SYSTEM VERSIONING`, deleting the history.
    DropSystemVersioning {
        /// The explicit row start and end columns to drop with it; `None`
        /// for implicit columns.
        #[serde(default)]
        columns: Option<VersioningColumns>,
    },
}

/// `ALTER TABLE` from the table designer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlterTable {
    /// Database.
    pub database: String,
    /// Table name.
    pub name: String,
    /// Operations in the order the user made them.
    pub operations: Vec<AlterOp>,
    /// `ALGORITHM=` for the column, index, constraint and option changes.
    #[serde(default)]
    pub algorithm: Option<AlgorithmOption>,
    /// `LOCK=` for the column, index, constraint and option changes.
    #[serde(default)]
    pub lock: Option<LockOption>,
    /// MariaDB: the table is system-versioned. Its statements then run
    /// with `system_versioning_alter_history=KEEP`, without which MariaDB
    /// refuses to alter it.
    #[serde(default)]
    pub system_versioned: bool,
}

/// What an operation means for grouping and warnings.
struct OpInfo {
    /// Names (lowercase) the operation reads or writes.
    touches: Vec<String>,
    /// Names (lowercase) whose meaning the operation changes.
    renames: Vec<String>,
    /// The operation carries user SQL that may name columns.
    expressions: bool,
    /// Short description for warnings.
    label: String,
    /// The cheapest algorithm the operation runs with.
    level: Level,
    /// The table's new name, for `RENAME TO`.
    renamed_to: Option<String>,
    /// MariaDB 10.6 engine change: the server copies the table whatever
    /// the algorithm says and refuses only `ALGORITHM=INPLACE` (seen on
    /// 10.6.28; 12.3 refuses INSTANT and NOCOPY like any copy).
    copies_regardless: bool,
}

fn lower(name: &str) -> String {
    name.to_lowercase()
}

/// `ALTER COLUMN ... SET DEFAULT ...` value.
fn set_default_sql(
    t: &Target,
    column_type: TypeName,
    default: &ColumnDefault,
) -> CatalogResult<String> {
    let literal = match default {
        ColumnDefault::Null => "NULL".to_string(),
        ColumnDefault::Literal { value } => sql::literal(Dialect::Mysql, value)?,
        ColumnDefault::Expression { sql } => {
            if !(t.is_mariadb() || t.mysql_at(8, 0, 13)) {
                return Err(t.lacks("expression defaults (MySQL 8.0.13+)"));
            }
            return Ok(format!(
                "SET DEFAULT ({})",
                fragment(sql, "a default expression")?
            ));
        }
        ColumnDefault::CurrentTimestamp { .. } => {
            return Err(invalid(
                "set a CURRENT_TIMESTAMP default with modifyColumn, which carries the fractional seconds",
            ));
        }
    };
    if !t.is_mariadb() && column_type.mysql_expression_default_only() && literal != "NULL" {
        // MySQL takes constants on these types only as `DEFAULT (constant)`
        // in a column definition: `ALTER COLUMN ... SET DEFAULT ('x')`
        // folds the parentheses away and fails with error 1101.
        return Err(super::unsupported(format!(
            "{} sets a constant default on TEXT, BLOB, JSON and spatial columns only through the column definition; use modifyColumn (it runs INSTANT) or an expression default",
            t.describe()
        )));
    }
    Ok(format!("SET DEFAULT {literal}"))
}

fn position_sql(position: Option<&ColumnPosition>) -> CatalogResult<String> {
    position.map_or_else(|| Ok(String::new()), ColumnPosition::sql)
}

fn position_name(position: Option<&ColumnPosition>) -> Option<String> {
    match position {
        Some(ColumnPosition::After { column }) => Some(lower(column)),
        _ => None,
    }
}

/// Destructive flag and warnings of a column definition change; returns
/// its level.
fn column_change_effects(
    t: &Target,
    previous: &ColumnDef,
    column: &ColumnDef,
    position: Option<&ColumnPosition>,
    indexed: bool,
    plan: &mut Plan,
) -> Level {
    let change = column_change(t, previous, column, position.is_some(), indexed);
    if change.lossy {
        plan.destructive();
    }
    for warning in change.warnings {
        plan.warn(warning);
    }
    change.level
}

/// The level of table options on InnoDB.
fn options_level(t: &Target, options: &TableOptions) -> Level {
    if options.changes_engine() {
        // A real engine change copies; the same engine again rebuilds.
        Level::Copy
    } else if options.rebuilds() {
        Level::Inplace
    } else {
        Level::pick(t, Level::Inplace, Level::Instant)
    }
}

/// The clause of a regular (groupable) operation and what it means.
fn clause(
    t: &Target,
    table_db: &str,
    op: &AlterOp,
    plan: &mut Plan,
) -> CatalogResult<(String, OpInfo)> {
    let info = |touches: Vec<String>, label: &str, level: Level| OpInfo {
        touches,
        renames: Vec::new(),
        expressions: false,
        label: label.to_string(),
        level,
        renamed_to: None,
        copies_regardless: false,
    };
    Ok(match op {
        AlterOp::AddColumn { column, position } => {
            let sql = format!(
                "ADD COLUMN {}{}",
                column.render(t, plan)?,
                position_sql(position.as_ref())?
            );
            let level = if column.is_stored_generated() || column.auto_increment {
                Level::Copy
            } else if t.is_mariadb()
                || t.mysql_at(8, 0, 29)
                || (t.mysql_at(8, 0, 12) && position.is_none())
            {
                Level::Instant
            } else {
                Level::Inplace
            };
            let mut i = info(
                std::iter::once(lower(&column.name))
                    .chain(position_name(position.as_ref()))
                    .collect(),
                "ADD COLUMN",
                level,
            );
            i.expressions = column.has_expressions();
            (sql, i)
        }
        AlterOp::DropColumn { name, indexed } => {
            plan.destructive();
            plan.warn(format!("drops column {} and its data", ident(name)?));
            let level = match (t.is_mariadb(), indexed) {
                (true, true) => Level::Nocopy,
                (true, false) => Level::Instant,
                (false, true) => Level::Inplace,
                (false, false) if t.mysql_at(8, 0, 29) => Level::Instant,
                (false, false) => Level::Inplace,
            };
            let i = info(vec![lower(name)], "DROP COLUMN", level);
            (format!("DROP COLUMN {}", ident(name)?), i)
        }
        AlterOp::ModifyColumn {
            previous,
            column,
            position,
            indexed,
        } => {
            if lower(&previous.name) != lower(&column.name) {
                return Err(invalid(
                    "modifyColumn keeps the name; use changeColumn to rename",
                ));
            }
            let level =
                column_change_effects(t, previous, column, position.as_ref(), *indexed, plan);
            let sql = format!(
                "MODIFY COLUMN {}{}",
                column.render(t, plan)?,
                position_sql(position.as_ref())?
            );
            let mut i = info(
                std::iter::once(lower(&column.name))
                    .chain(position_name(position.as_ref()))
                    .collect(),
                "MODIFY COLUMN",
                level,
            );
            i.expressions = column.has_expressions();
            (sql, i)
        }
        AlterOp::ChangeColumn {
            previous,
            column,
            position,
            indexed,
        } => {
            let mut level =
                column_change_effects(t, previous, column, position.as_ref(), *indexed, plan);
            let old_name = &previous.name;
            let renamed = lower(old_name) != lower(&column.name);
            if renamed && !t.is_mariadb() && !t.mysql_at(8, 0, 28) {
                level = level.max(Level::Inplace);
            }
            let sql = format!(
                "CHANGE COLUMN {} {}{}",
                ident(old_name)?,
                column.render(t, plan)?,
                position_sql(position.as_ref())?
            );
            let mut i = info(
                [lower(old_name), lower(&column.name)]
                    .into_iter()
                    .chain(position_name(position.as_ref()))
                    .collect(),
                "CHANGE COLUMN",
                level,
            );
            if renamed {
                i.renames = vec![lower(old_name), lower(&column.name)];
            }
            i.expressions = column.has_expressions();
            (sql, i)
        }
        AlterOp::RenameColumn { old_name, new_name } => {
            let level = if t.is_mariadb() || t.mysql_at(8, 0, 28) {
                Level::Instant
            } else {
                Level::Inplace
            };
            let mut i = info(
                vec![lower(old_name), lower(new_name)],
                "RENAME COLUMN",
                level,
            );
            i.renames = i.touches.clone();
            (
                format!("RENAME COLUMN {} TO {}", ident(old_name)?, ident(new_name)?),
                i,
            )
        }
        AlterOp::SetColumnDefault {
            name,
            column_type,
            default,
        } => {
            let action = match default {
                None => {
                    if !t.is_mariadb() {
                        plan.warn(format!(
                            "without a default MySQL rejects INSERTs that leave out {} in strict mode, even when it is nullable; set DEFAULT NULL to keep them working",
                            ident(name)?
                        ));
                    }
                    "DROP DEFAULT".to_string()
                }
                Some(default) => set_default_sql(t, *column_type, default)?,
            };
            let mut i = info(
                vec![lower(name)],
                "ALTER COLUMN ... DEFAULT",
                Level::Instant,
            );
            i.expressions = matches!(default, Some(ColumnDefault::Expression { .. }));
            (format!("ALTER COLUMN {} {action}", ident(name)?), i)
        }
        AlterOp::SetColumnVisibility { name, invisible } => {
            if !t.mysql_at(8, 0, 23) {
                return Err(t.lacks(
                    "ALTER COLUMN ... SET INVISIBLE (MySQL 8.0.23+); change the column with modifyColumn",
                ));
            }
            let i = info(
                vec![lower(name)],
                "ALTER COLUMN ... VISIBLE",
                Level::Instant,
            );
            (
                format!(
                    "ALTER COLUMN {} SET {}",
                    ident(name)?,
                    if *invisible { "INVISIBLE" } else { "VISIBLE" }
                ),
                i,
            )
        }
        AlterOp::AddIndex { index } => {
            let sql = format!("ADD {}", index.inline(t, plan)?);
            let level = match index.kind {
                IndexKind::Fulltext | IndexKind::Spatial => Level::Inplace,
                IndexKind::Index | IndexKind::Unique => {
                    Level::pick(t, Level::Inplace, Level::Nocopy)
                }
            };
            let mut i = info(
                index
                    .name
                    .iter()
                    .map(|n| lower(n))
                    .chain(index.column_names().map(lower))
                    .collect(),
                "ADD INDEX",
                level,
            );
            i.expressions = index.has_expressions();
            if index.kind == IndexKind::Fulltext {
                plan.warn("the first full-text index of an InnoDB table rebuilds the table");
            }
            (sql, i)
        }
        AlterOp::DropIndex { name } => {
            plan.destructive();
            let i = info(
                vec![lower(name)],
                "DROP INDEX",
                Level::pick(t, Level::Inplace, Level::Nocopy),
            );
            (format!("DROP INDEX {}", ident(name)?), i)
        }
        AlterOp::RenameIndex { old_name, new_name } => {
            let mut i = info(
                vec![lower(old_name), lower(new_name)],
                "RENAME INDEX",
                Level::pick(t, Level::Inplace, Level::Instant),
            );
            i.renames = i.touches.clone();
            (
                format!("RENAME INDEX {} TO {}", ident(old_name)?, ident(new_name)?),
                i,
            )
        }
        AlterOp::SetIndexVisibility { name, invisible } => {
            let i = info(
                vec![lower(name)],
                "ALTER INDEX",
                Level::pick(t, Level::Inplace, Level::Instant),
            );
            (
                format!(
                    "ALTER INDEX {} {}",
                    ident(name)?,
                    visibility_keyword(t, *invisible)?
                ),
                i,
            )
        }
        AlterOp::AddConstraint { constraint } => {
            let sql = format!("ADD {}", constraint.render(t, table_db, plan)?);
            let (label, level) = match constraint {
                TableConstraint::Check(_) => ("ADD CHECK", Level::Copy),
                TableConstraint::ForeignKey(_) => ("ADD FOREIGN KEY", Level::Copy),
                TableConstraint::PrimaryKey { .. } => {
                    plan.warn("adding a primary key rebuilds the table");
                    ("ADD PRIMARY KEY", Level::Inplace)
                }
            };
            let mut i = info(
                constraint.names().iter().map(|n| lower(n)).collect(),
                label,
                level,
            );
            i.expressions = matches!(constraint, TableConstraint::Check(_));
            (sql, i)
        }
        AlterOp::DropPrimaryKey => {
            plan.destructive();
            // Copy unless the same statement adds a new primary key (see
            // Builder::flush).
            let i = info(vec!["primary".into()], "DROP PRIMARY KEY", Level::Inplace);
            ("DROP PRIMARY KEY".into(), i)
        }
        AlterOp::DropForeignKey { name } => {
            plan.destructive();
            let mut i = info(
                vec![lower(name)],
                "DROP FOREIGN KEY",
                Level::pick(t, Level::Inplace, Level::Instant),
            );
            // Dropping and adding a foreign key of the same name in one
            // statement is refused with ALGORITHM=COPY.
            i.renames = vec![lower(name)];
            (format!("DROP FOREIGN KEY {}", ident(name)?), i)
        }
        AlterOp::DropCheck { name } => {
            plan.destructive();
            let mut i = info(vec![lower(name)], "DROP CHECK", Level::Instant);
            i.renames = vec![lower(name)];
            let sql = if t.is_mariadb() {
                format!("DROP CONSTRAINT {}", ident(name)?)
            } else {
                if !t.mysql_at(8, 0, 16) {
                    return Err(t.lacks("DROP CHECK (MySQL 8.0.16+)"));
                }
                format!("DROP CHECK {}", ident(name)?)
            };
            (sql, i)
        }
        AlterOp::SetCheckEnforced { name, enforced } => {
            if !t.mysql_at(8, 0, 16) {
                return Err(t.lacks("ALTER CHECK ... ENFORCED (MySQL 8.0.16+)"));
            }
            let (label, level) = if *enforced {
                (
                    "ALTER CHECK ... ENFORCED (validates every row)",
                    Level::Copy,
                )
            } else {
                ("ALTER CHECK ... NOT ENFORCED", Level::Instant)
            };
            let i = info(vec![lower(name)], label, level);
            (
                format!(
                    "ALTER CHECK {} {}",
                    ident(name)?,
                    if *enforced {
                        "ENFORCED"
                    } else {
                        "NOT ENFORCED"
                    }
                ),
                i,
            )
        }
        AlterOp::Rename { database, name } => {
            let target = object_name(database.as_deref().unwrap_or(table_db), name)?;
            let mut i = info(vec![lower(name)], "RENAME TO", Level::Instant);
            i.renames = vec![lower(name)];
            i.renamed_to = Some(target.clone());
            (format!("RENAME TO {target}"), i)
        }
        AlterOp::SetOptions { options } => {
            let clauses = options.clauses(t)?;
            if clauses.is_empty() && options.runtime_comment()?.is_none() {
                return Err(invalid("setOptions needs at least one option"));
            }
            if options.changes_engine() {
                plan.destructive();
                match options
                    .engine
                    .as_deref()
                    .and_then(crate::mysql::engine_warning)
                {
                    Some(warning) => plan.warn(warning),
                    None => plan.warn(
                        "changing the storage engine rewrites the table; features the new engine lacks (foreign keys, transactions, full-text indexes) are lost",
                    ),
                }
            } else if options.rebuilds() {
                plan.warn("these table options rebuild the table");
            }
            let label = if options.changes_engine() {
                "ENGINE"
            } else {
                "table options"
            };
            let mut i = info(Vec::new(), label, options_level(t, options));
            i.copies_regardless =
                t.is_mariadb() && !t.mariadb_at(10, 7) && options.changes_engine();
            (clauses.join(", "), i)
        }
        AlterOp::ConvertCharset { charset, collation } => {
            plan.destructive();
            plan.warn(
                "converting the character set rewrites every character column; characters the new set lacks are lost",
            );
            let mut sql = format!("CONVERT TO CHARACTER SET {}", ident(charset)?);
            if let Some(collation) = collation {
                sql.push_str(&format!(" COLLATE {}", ident(collation)?));
            }
            let i = info(Vec::new(), "CONVERT TO CHARACTER SET", Level::Copy);
            (sql, i)
        }
        AlterOp::AddPeriod { period } => {
            let sql = format!("ADD {}", period.render(t)?);
            let i = info(
                vec![
                    lower(&period.name),
                    lower(&period.start),
                    lower(&period.end),
                ],
                "ADD PERIOD",
                Level::Copy,
            );
            (sql, i)
        }
        AlterOp::DropPeriod { name } => {
            if !t.mariadb_at(10, 4) {
                return Err(t.lacks("application-time periods"));
            }
            plan.destructive();
            let i = info(vec![lower(name)], "DROP PERIOD", Level::Instant);
            (format!("DROP PERIOD FOR {}", ident(name)?), i)
        }
        AlterOp::PartitionBy { .. }
        | AlterOp::RemovePartitioning
        | AlterOp::AddPartitions { .. }
        | AlterOp::AddPartitionCount { .. }
        | AlterOp::DropPartitions { .. }
        | AlterOp::TruncatePartitions { .. }
        | AlterOp::ReorganizePartitions { .. }
        | AlterOp::CoalescePartitions { .. }
        | AlterOp::AddSystemVersioning { .. }
        | AlterOp::DropSystemVersioning { .. } => {
            return Err(invalid("this operation runs in its own statement"));
        }
    })
}

/// Partition maintenance: the clause, the reason it rewrites data, and
/// whether it deletes rows.
fn partition_clause(t: &Target, op: &AlterOp, plan: &mut Plan) -> CatalogResult<Option<String>> {
    let names = |names: &[String]| -> CatalogResult<String> {
        if names.is_empty() {
            return Err(invalid("name at least one partition"));
        }
        ident_list(names)
    };
    let defs = |defs: &[PartitionDef]| -> CatalogResult<String> {
        if defs.is_empty() {
            return Err(invalid("give at least one partition"));
        }
        Ok(defs
            .iter()
            .map(|d| d.render(t, None))
            .collect::<CatalogResult<Vec<_>>>()?
            .join(", "))
    };
    Ok(Some(match op {
        AlterOp::PartitionBy { partitioning } => {
            plan.warn("partitioning a table copies all its rows into the new partitions");
            partitioning.render(t)?
        }
        AlterOp::RemovePartitioning => {
            plan.warn("removing partitioning copies all rows into one table");
            "REMOVE PARTITIONING".into()
        }
        AlterOp::AddPartitions { partitions } => format!("ADD PARTITION ({})", defs(partitions)?),
        AlterOp::AddPartitionCount { count } => {
            if *count == 0 {
                return Err(invalid("add at least one partition"));
            }
            plan.warn("adding hash or key partitions redistributes the rows");
            format!("ADD PARTITION PARTITIONS {count}")
        }
        AlterOp::DropPartitions { names: list } => {
            plan.destructive();
            plan.warn(format!(
                "dropping partitions {} deletes every row in them",
                names(list)?
            ));
            format!("DROP PARTITION {}", names(list)?)
        }
        AlterOp::TruncatePartitions { names: list } => {
            plan.destructive();
            if list.is_empty() {
                plan.warn("truncating every partition deletes every row of the table");
                "TRUNCATE PARTITION ALL".into()
            } else {
                plan.warn(format!(
                    "truncating partitions {} deletes every row in them",
                    names(list)?
                ));
                format!("TRUNCATE PARTITION {}", names(list)?)
            }
        }
        AlterOp::ReorganizePartitions { names: list, into } => {
            // The server keeps the total range of RANGE partitions, but on
            // LIST partitioning it deletes, without an error, every row
            // whose value no new partition lists; the plan does not know
            // the old values, so the confirmation always appears.
            plan.destructive();
            plan.warn(format!(
                "reorganizing partitions {} copies their rows into the new partitions; on LIST partitioning, rows whose values no new partition lists are deleted",
                names(list)?
            ));
            format!(
                "REORGANIZE PARTITION {} INTO ({})",
                names(list)?,
                defs(into)?
            )
        }
        AlterOp::CoalescePartitions { count } => {
            if *count == 0 {
                return Err(invalid("coalesce at least one partition"));
            }
            plan.warn("coalescing partitions redistributes the rows");
            format!("COALESCE PARTITION {count}")
        }
        _ => return Ok(None),
    }))
}

/// System versioning add or drop: the clause list.
fn versioning_clauses(
    t: &Target,
    op: &AlterOp,
    plan: &mut Plan,
) -> CatalogResult<Option<Vec<String>>> {
    match op {
        AlterOp::AddSystemVersioning { columns } => {
            if !t.is_mariadb() {
                return Err(t.lacks("system-versioned tables"));
            }
            plan.warn("adding system versioning copies the table");
            let mut clauses = Vec::new();
            if let Some(columns) = columns {
                let (start, end) = columns.as_ref();
                if start.row_period != Some(RowPeriod::Start)
                    || end.row_period != Some(RowPeriod::End)
                {
                    return Err(invalid(
                        "the explicit versioning columns are a ROW START and a ROW END column",
                    ));
                }
                clauses.push(format!("ADD COLUMN {}", start.render(t, plan)?));
                clauses.push(format!("ADD COLUMN {}", end.render(t, plan)?));
                clauses.push(format!(
                    "ADD {}",
                    system_period(&VersioningColumns {
                        start: start.name.clone(),
                        end: end.name.clone(),
                    })?
                ));
            }
            clauses.push("ADD SYSTEM VERSIONING".into());
            Ok(Some(clauses))
        }
        AlterOp::DropSystemVersioning { columns } => {
            if !t.is_mariadb() {
                return Err(t.lacks("system-versioned tables"));
            }
            plan.destructive();
            plan.warn("dropping system versioning deletes the table's history rows");
            let mut clauses = vec!["DROP SYSTEM VERSIONING".to_string()];
            if let Some(columns) = columns {
                clauses.push("DROP PERIOD FOR SYSTEM_TIME".into());
                clauses.push(format!("DROP COLUMN {}", ident(&columns.start)?));
                clauses.push(format!("DROP COLUMN {}", ident(&columns.end)?));
            }
            Ok(Some(clauses))
        }
        _ => Ok(None),
    }
}

/// Clauses collected for one statement.
#[derive(Default)]
struct Group {
    clauses: Vec<String>,
    infos: Vec<OpInfo>,
    touched: HashSet<String>,
    renamed: HashSet<String>,
    expressions: bool,
}

impl Group {
    fn conflicts(&self, info: &OpInfo) -> bool {
        info.touches.iter().any(|n| self.renamed.contains(n))
            || info.renames.iter().any(|n| self.touched.contains(n))
            || (info.expressions && !self.renamed.is_empty())
            || (!info.renames.is_empty() && self.expressions)
    }

    fn add(&mut self, clause: String, info: OpInfo) {
        self.touched.extend(info.touches.iter().cloned());
        self.renamed.extend(info.renames.iter().cloned());
        self.expressions |= info.expressions;
        self.clauses.push(clause);
        self.infos.push(info);
    }
}

/// Writes the statements, following the table through `RENAME TO`.
struct Builder<'a> {
    t: &'a Target,
    /// `SET STATEMENT ... FOR ` or empty.
    keep_history: &'static str,
    /// The table's name as the next statement must call it.
    table: String,
    modifiers: Vec<String>,
    algorithm: Option<AlgorithmOption>,
    lock: Option<LockOption>,
}

impl Builder<'_> {
    fn flush(&mut self, group: &mut Group, plan: &mut Plan) {
        if group.clauses.is_empty() {
            return;
        }
        let group = std::mem::take(group);
        self.cost_warnings(&group, plan);
        let mut clauses = group.clauses;
        clauses.extend(self.modifiers.iter().cloned());
        plan.push(format!(
            "{}ALTER TABLE {} {}",
            self.keep_history,
            self.table,
            clauses.join(", ")
        ));
        if let Some(name) = group.infos.iter().rev().find_map(|i| i.renamed_to.clone()) {
            self.table = name;
        }
    }

    /// A statement of its own (partition maintenance, versioning).
    fn standalone(&self, prefix: &str, clauses: &str, plan: &mut Plan) {
        plan.push(format!("{prefix}ALTER TABLE {} {clauses}", self.table));
    }

    fn cost_warnings(&self, group: &Group, plan: &mut Plan) {
        let adds_pk = group.infos.iter().any(|i| i.label == "ADD PRIMARY KEY");
        for info in group.infos.iter().filter(|i| i.copies_regardless) {
            match self.algorithm {
                Some(AlgorithmOption::Inplace) => plan.warn(format!(
                    "{} copies the table on {}, so the server rejects ALGORITHM=INPLACE",
                    info.label,
                    self.t.describe()
                )),
                _ => plan.warn(format!(
                    "{} copies the whole table on {} whatever ALGORITHM says; writes wait until it finishes",
                    info.label,
                    self.t.describe()
                )),
            }
        }
        let levels: Vec<(String, Level)> = group
            .infos
            .iter()
            .filter(|i| !i.copies_regardless)
            .map(|i| {
                let level = if i.label == "DROP PRIMARY KEY" && !adds_pk {
                    Level::Copy
                } else {
                    i.level
                };
                let label = if i.label == "DROP PRIMARY KEY" && !adds_pk {
                    "DROP PRIMARY KEY without a new primary key".to_string()
                } else {
                    i.label.clone()
                };
                (label, level)
            })
            .collect();
        let server = self.t.describe();
        let requested = match self.algorithm {
            Some(AlgorithmOption::Instant) => Some(Level::Instant),
            Some(AlgorithmOption::Nocopy) => Some(Level::Nocopy),
            // MySQL counts NOCOPY-level operations as INPLACE.
            Some(AlgorithmOption::Inplace) => Some(Level::Inplace),
            Some(AlgorithmOption::Default | AlgorithmOption::Copy) | None => None,
        };
        for (label, level) in &levels {
            if let Some(requested) = requested
                && *level > requested
            {
                plan.warn(format!(
                    "{label} needs ALGORITHM={} on {server}, so the server rejects ALGORITHM={}",
                    level.keyword(),
                    requested.keyword()
                ));
            } else if *level == Level::Copy {
                plan.warn(format!(
                    "{label} copies the whole table; writes wait until it finishes"
                ));
            }
            if *level == Level::Copy && self.lock == Some(LockOption::None) {
                plan.warn(format!(
                    "{label} copies the table, so the server rejects LOCK=NONE"
                ));
            }
        }
    }
}

fn algorithm_sql(algorithm: AlgorithmOption) -> &'static str {
    match algorithm {
        AlgorithmOption::Default => "DEFAULT",
        AlgorithmOption::Instant => "INSTANT",
        AlgorithmOption::Inplace => "INPLACE",
        AlgorithmOption::Nocopy => "NOCOPY",
        AlgorithmOption::Copy => "COPY",
    }
}

pub(super) fn alter(t: &Target, c: &AlterTable, plan: &mut Plan) -> CatalogResult<()> {
    if c.operations.is_empty() {
        return Err(invalid("ALTER TABLE needs at least one operation"));
    }
    let keep_history = if c.system_versioned {
        if !t.is_mariadb() {
            return Err(t.lacks("system-versioned tables"));
        }
        plan.warn(
            "the table is system-versioned: the change runs with system_versioning_alter_history=KEEP and history rows take the new structure",
        );
        KEEP_HISTORY
    } else {
        ""
    };
    let mut modifiers = Vec::new();
    if let Some(algorithm) = c.algorithm {
        match algorithm {
            AlgorithmOption::Nocopy if !t.mariadb_at(10, 3) => {
                return Err(t.lacks("ALGORITHM=NOCOPY"));
            }
            AlgorithmOption::Instant if !(t.mariadb_at(10, 3) || t.mysql_at(8, 0, 12)) => {
                return Err(t.lacks("ALGORITHM=INSTANT"));
            }
            _ => {}
        }
        modifiers.push(format!("ALGORITHM={}", algorithm_sql(algorithm)));
    }
    if let Some(lock) = c.lock {
        modifiers.push(format!(
            "LOCK={}",
            match lock {
                LockOption::Default => "DEFAULT",
                LockOption::None => "NONE",
                LockOption::Shared => "SHARED",
                LockOption::Exclusive => "EXCLUSIVE",
            }
        ));
    }
    let mut builder = Builder {
        t,
        keep_history,
        table: object_name(&c.database, &c.name)?,
        modifiers,
        algorithm: c.algorithm,
        lock: c.lock,
    };
    let mut group = Group::default();
    let mut runtime_comments: Vec<String> = Vec::new();
    for op in &c.operations {
        if let Some(clause) = partition_clause(t, op, plan)? {
            builder.flush(&mut group, plan);
            if !builder.modifiers.is_empty() {
                plan.warn(
                    "ALGORITHM and LOCK apply to column, index, constraint and option changes, not to partition maintenance",
                );
            }
            builder.standalone(keep_history, &clause, plan);
            continue;
        }
        if let Some(clauses) = versioning_clauses(t, op, plan)? {
            builder.flush(&mut group, plan);
            let needs_keep = matches!(op, AlterOp::DropSystemVersioning { columns: Some(_) });
            let prefix = if needs_keep {
                KEEP_HISTORY
            } else {
                keep_history
            };
            let mut all = clauses;
            all.extend(builder.modifiers.iter().cloned());
            builder.standalone(prefix, &all.join(", "), plan);
            continue;
        }
        if let AlterOp::SetOptions { options } = op
            && let Some(comment) = options.runtime_comment()?
        {
            // Set after the ALTER, on the table's final name (see `runtime_comment`).
            runtime_comments.push(comment.to_owned());
            if options.clauses(t)?.is_empty() {
                continue;
            }
        }
        let (clause, info) = clause(t, &c.database, op, plan)?;
        if group.conflicts(&info) {
            builder.flush(&mut group, plan);
        }
        group.add(clause, info);
    }
    builder.flush(&mut group, plan);
    for comment in &runtime_comments {
        super::runtime_comment(&builder.table, comment, plan);
    }
    Ok(())
}

const KEEP_HISTORY: &str = "SET STATEMENT system_versioning_alter_history='KEEP' FOR ";

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::model::CatalogResult;

    fn alter(ops: &str, extra: &str) -> String {
        format!(
            r#"{{"change": "alterTable", "database": "d", "name": "t", "operations": [{ops}]{extra}}}"#
        )
    }

    #[test]
    fn combines_independent_operations() -> CatalogResult<()> {
        let json = alter(
            r#"{"op": "addColumn", "column": {"name": "a", "dataType": {"name": "int"}}, "position": {"kind": "first"}},
               {"op": "addColumn", "column": {"name": "b", "dataType": {"name": "int"}}, "position": {"kind": "after", "column": "a"}},
               {"op": "addIndex", "index": {"name": "ix_b", "kind": "index", "parts": [{"kind": "column", "name": "b"}]}},
               {"op": "setOptions", "options": {"comment": "c"}}"#,
            r#", "algorithm": "inplace", "lock": "none""#,
        );
        let plan = plan_json(&my80(), &json)?;
        assert_eq!(
            sql(&plan),
            "ALTER TABLE `d`.`t` ADD COLUMN `a` INT NULL FIRST, ADD COLUMN `b` INT NULL AFTER `a`, ADD INDEX `ix_b` (`b`), COMMENT='c', ALGORITHM=INPLACE, LOCK=NONE"
        );
        assert!(!plan.destructive && plan.warnings.is_empty() && !plan.transactional);
        Ok(())
    }

    #[test]
    fn engine_changes_say_what_happens_to_the_rows() -> CatalogResult<()> {
        let to = |engine: &str| {
            alter(
                &format!(r#"{{"op": "setOptions", "options": {{"engine": "{engine}"}}}}"#),
                "",
            )
        };
        let blackhole = plan_json(&my80(), &to("BLACKHOLE"))?;
        assert!(blackhole.destructive);
        assert!(
            blackhole
                .warnings
                .iter()
                .any(|w| w.contains("every row of the table is discarded")
                    && w.contains("rows will be lost"))
        );
        let memory = plan_json(&ma106(), &to("MEMORY"))?;
        assert!(
            memory.destructive
                && memory
                    .warnings
                    .iter()
                    .any(|w| w.contains("lost when the server restarts"))
        );
        let innodb = plan_json(&my97(), &to("InnoDB"))?;
        assert!(
            innodb.destructive
                && innodb
                    .warnings
                    .iter()
                    .any(|w| w.contains("features the new engine lacks"))
        );
        Ok(())
    }

    #[test]
    fn a_comment_with_a_backslash_is_set_at_run_time_for_the_sql_mode() -> CatalogResult<()> {
        let json = alter(
            r#"{"op": "setOptions", "options": {"comment": "C:\\data"}}, {"op": "rename", "name": "u"}"#,
            "",
        );
        let plan = plan_json(&my80(), &json)?;
        assert_eq!(plan.statements[0], "ALTER TABLE `d`.`t` RENAME TO `d`.`u`");
        let name = plan.statements[1]
            .strip_prefix("SET @")
            .and_then(|rest| rest.split(' ').next())
            .ok_or("no variable")
            .map_err(|e| crate::model::CatalogError::Invalid { message: e.into() })?
            .to_owned();
        // A random suffix keeps the names apart from the user's own.
        assert!(
            name.starts_with("varve_ddl_") && name.len() == "varve_ddl_".len() + 8,
            "{name}"
        );
        assert!(plan.statements[1].contains(" = CONCAT(_utf8mb4 X'"));
        // The comment travels as hex (C:\data), the prefix names the renamed table.
        assert!(plan.statements[1].contains("X'433a5c64617461'"));
        assert!(plan.statements[1].contains(&format!(
            "X'{}'",
            "ALTER TABLE `d`.`u` COMMENT="
                .bytes()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )));
        // The variable is cleared before EXECUTE can fail; EXECUTE shows the ALTER readably.
        assert_eq!(
            &plan.statements[2..],
            [
                format!("PREPARE {name} FROM @{name}"),
                format!("SET @{name} = NULL"),
                format!("EXECUTE {name} /* ALTER TABLE `d`.`u` COMMENT='C:\\data' */"),
                format!("DEALLOCATE PREPARE {name}"),
            ]
        );
        // A second plan gets other names.
        let again = plan_json(&my80(), &json)?;
        assert_ne!(again.statements[2], plan.statements[2]);
        // A comment that would end the readable comment early is spaced out there (the stored text is the hex).
        let tricky = plan_json(
            &my80(),
            &alter(
                r#"{"op": "setOptions", "options": {"comment": "a\\b */ it's; x"}}"#,
                "",
            ),
        )?;
        assert!(
            tricky.statements[3].ends_with("/* ALTER TABLE `d`.`t` COMMENT='a\\b * / it''s; x' */"),
            "{}",
            tricky.statements[3]
        );
        let plain = plan_json(
            &my80(),
            &alter(
                r#"{"op": "setOptions", "options": {"comment": "it's"}}"#,
                "",
            ),
        )?;
        assert_eq!(plain.statements, ["ALTER TABLE `d`.`t` COMMENT='it''s'"]);
        assert!(
            plan_json(
                &my80(),
                &alter(
                    r#"{"op": "setOptions", "options": {"comment": "a\u0000"}}"#,
                    ""
                )
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn splits_after_renames() -> CatalogResult<()> {
        let json = alter(
            r#"{"op": "renameColumn", "oldName": "a", "newName": "b"},
               {"op": "renameIndex", "oldName": "ix", "newName": "ix2"},
               {"op": "setIndexVisibility", "name": "ix2", "invisible": true},
               {"op": "addIndex", "index": {"kind": "index", "parts": [{"kind": "column", "name": "B"}]}},
               {"op": "dropColumn", "name": "c"}"#,
            "",
        );
        assert_eq!(
            sql(&plan_json(&my97(), &json)?),
            "ALTER TABLE `d`.`t` RENAME COLUMN `a` TO `b`, RENAME INDEX `ix` TO `ix2`;\nALTER TABLE `d`.`t` ALTER INDEX `ix2` INVISIBLE, ADD INDEX (`B`), DROP COLUMN `c`"
        );
        let maria = plan_json(&ma106(), &json)?;
        assert!(sql(&maria).contains("ALTER INDEX `ix2` IGNORED"));
        assert!(maria.destructive);

        let fk = alter(
            r#"{"op": "dropForeignKey", "name": "fk"},
               {"op": "addConstraint", "constraint": {"kind": "foreignKey", "name": "fk", "columns": ["a"], "referencesTable": "p", "referencesColumns": ["id"]}}"#,
            "",
        );
        assert_eq!(
            plan_json(&my80(), &fk)?.statements,
            vec![
                "ALTER TABLE `d`.`t` DROP FOREIGN KEY `fk`".to_string(),
                "ALTER TABLE `d`.`t` ADD CONSTRAINT `fk` FOREIGN KEY (`a`) REFERENCES `d`.`p` (`id`)".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn column_changes() -> CatalogResult<()> {
        let json = alter(
            r#"{"op": "modifyColumn", "previous": {"name": "a", "dataType": {"name": "int"}, "nullable": false},
                "column": {"name": "a", "dataType": {"name": "bigint"}, "nullable": false}, "position": {"kind": "after", "column": "z"}},
               {"op": "changeColumn", "previous": {"name": "b", "dataType": {"name": "varchar", "length": 30}},
                "column": {"name": "c", "dataType": {"name": "varchar", "length": 20}}},
               {"op": "setColumnDefault", "name": "d", "columnType": "int", "default": {"kind": "literal", "value": {"t": "int", "v": 5}}},
               {"op": "setColumnDefault", "name": "e", "columnType": "int"}"#,
            "",
        );
        let plan = plan_json(&ma123(), &json)?;
        assert_eq!(
            sql(&plan),
            "ALTER TABLE `d`.`t` MODIFY COLUMN `a` BIGINT NOT NULL AFTER `z`, CHANGE COLUMN `b` `c` VARCHAR(20) NULL, ALTER COLUMN `d` SET DEFAULT 5, ALTER COLUMN `e` DROP DEFAULT"
        );
        // VARCHAR(30) to (20) can cut values; INT to BIGINT copies but
        // keeps every value.
        assert!(plan.destructive);
        for expected in [
            "MODIFY COLUMN copies the whole table; writes wait until it finishes",
            "changing the type of c converts its values; values that do not fit are cut or rejected",
            "CHANGE COLUMN copies the whole table; writes wait until it finishes",
        ] {
            assert!(
                plan.warnings.iter().any(|w| w == expected),
                "{:?}",
                plan.warnings
            );
        }
        assert_eq!(plan.warnings.len(), 3);

        let comment_only = alter(
            r#"{"op": "modifyColumn", "previous": {"name": "c", "dataType": {"name": "varchar", "length": 10}},
                "column": {"name": "c", "dataType": {"name": "varchar", "length": 10}, "comment": "k"}}"#,
            r#", "algorithm": "instant""#,
        );
        for server in [my80(), my97(), ma106(), ma123()] {
            let plan = plan_json(&server, &comment_only)?;
            assert!(!plan.destructive && plan.warnings.is_empty(), "{plan:?}");
        }
        let widen = alter(
            r#"{"op": "modifyColumn", "previous": {"name": "c", "dataType": {"name": "varchar", "length": 10}},
                "column": {"name": "c", "dataType": {"name": "varchar", "length": 20}}}"#,
            r#", "algorithm": "instant""#,
        );
        let mysql = plan_json(&my80(), &widen)?;
        assert!(!mysql.destructive);
        assert_eq!(
            mysql.warnings,
            vec!["MODIFY COLUMN needs ALGORITHM=INPLACE on MySQL 8.0.46, so the server rejects ALGORITHM=INSTANT".to_string()]
        );
        let maria = plan_json(&ma106(), &widen)?;
        assert!(!maria.destructive && maria.warnings.is_empty());
        let renamed = alter(
            r#"{"op": "modifyColumn", "previous": {"name": "x", "dataType": {"name": "int"}},
                "column": {"name": "y", "dataType": {"name": "int"}}}"#,
            "",
        );
        assert!(is_invalid(&plan_json(&my80(), &renamed)));

        let vis = alter(
            r#"{"op": "setColumnVisibility", "name": "a", "invisible": true}"#,
            "",
        );
        assert_eq!(
            sql(&plan_json(&my80(), &vis)?),
            "ALTER TABLE `d`.`t` ALTER COLUMN `a` SET INVISIBLE"
        );
        assert!(is_unsupported(&plan_json(&ma123(), &vis)));
        Ok(())
    }

    #[test]
    fn set_default_on_text_per_engine() -> CatalogResult<()> {
        let json = alter(
            r#"{"op": "setColumnDefault", "name": "tx", "columnType": "text", "default": {"kind": "literal", "value": {"t": "text", "v": "x"}}}"#,
            "",
        );
        for server in [my80(), my97()] {
            assert!(is_unsupported(&plan_json(&server, &json)));
        }
        for server in [ma106(), ma123()] {
            assert_eq!(
                sql(&plan_json(&server, &json)?),
                "ALTER TABLE `d`.`t` ALTER COLUMN `tx` SET DEFAULT 'x'"
            );
        }
        let expression = alter(
            r#"{"op": "setColumnDefault", "name": "j", "columnType": "json", "default": {"kind": "expression", "sql": "json_array()"}}"#,
            "",
        );
        assert_eq!(
            sql(&plan_json(&my97(), &expression)?),
            "ALTER TABLE `d`.`t` ALTER COLUMN `j` SET DEFAULT (json_array())"
        );
        let modify = alter(
            r#"{"op": "modifyColumn", "previous": {"name": "tx", "dataType": {"name": "text"}},
                "column": {"name": "tx", "dataType": {"name": "text"}, "default": {"kind": "literal", "value": {"t": "text", "v": "x"}}}}"#,
            r#", "algorithm": "instant""#,
        );
        let plan = plan_json(&my80(), &modify)?;
        assert_eq!(
            sql(&plan),
            "ALTER TABLE `d`.`t` MODIFY COLUMN `tx` TEXT NULL DEFAULT ('x'), ALGORITHM=INSTANT"
        );
        assert!(plan.warnings.is_empty() && !plan.destructive);
        let int = alter(
            r#"{"op": "setColumnDefault", "name": "n", "columnType": "int", "default": {"kind": "null"}}"#,
            "",
        );
        assert!(sql(&plan_json(&my80(), &int)?).ends_with("SET DEFAULT NULL"));
        Ok(())
    }

    #[test]
    fn statements_follow_a_table_rename() -> CatalogResult<()> {
        let json = alter(
            r#"{"op": "rename", "name": "t2"},
               {"op": "renameColumn", "oldName": "a", "newName": "b"},
               {"op": "addIndex", "index": {"name": "ixb", "kind": "index", "parts": [{"kind": "column", "name": "b"}]}},
               {"op": "partitionBy", "partitioning": {"method": {"kind": "key", "columns": ["id"]}, "count": 2}},
               {"op": "rename", "database": "e", "name": "t3"},
               {"op": "truncatePartitions"},
               {"op": "addSystemVersioning"}"#,
            "",
        );
        assert_eq!(
            plan_json(&ma123(), &json)?.statements,
            vec![
                "ALTER TABLE `d`.`t` RENAME TO `d`.`t2`, RENAME COLUMN `a` TO `b`",
                "ALTER TABLE `d`.`t2` ADD INDEX `ixb` (`b`)",
                "ALTER TABLE `d`.`t2` PARTITION BY KEY (`id`) PARTITIONS 2",
                "ALTER TABLE `d`.`t2` RENAME TO `e`.`t3`",
                "ALTER TABLE `e`.`t3` TRUNCATE PARTITION ALL",
                "ALTER TABLE `e`.`t3` ADD SYSTEM VERSIONING",
            ]
        );
        Ok(())
    }

    #[test]
    fn constraints_per_engine() -> CatalogResult<()> {
        let json = alter(
            r#"{"op": "dropCheck", "name": "ck"},
               {"op": "dropPrimaryKey"},
               {"op": "addConstraint", "constraint": {"kind": "primaryKey", "parts": [{"kind": "column", "name": "id"}]}}"#,
            "",
        );
        assert_eq!(
            sql(&plan_json(&my80(), &json)?),
            "ALTER TABLE `d`.`t` DROP CHECK `ck`, DROP PRIMARY KEY, ADD PRIMARY KEY (`id`)"
        );
        assert_eq!(
            sql(&plan_json(&ma106(), &json)?),
            "ALTER TABLE `d`.`t` DROP CONSTRAINT `ck`, DROP PRIMARY KEY, ADD PRIMARY KEY (`id`)"
        );
        let enforce = alter(
            r#"{"op": "setCheckEnforced", "name": "ck", "enforced": false}"#,
            "",
        );
        assert_eq!(
            sql(&plan_json(&my97(), &enforce)?),
            "ALTER TABLE `d`.`t` ALTER CHECK `ck` NOT ENFORCED"
        );
        assert!(is_unsupported(&plan_json(&ma123(), &enforce)));
        Ok(())
    }

    #[test]
    fn algorithm_warnings_follow_live_evidence() -> CatalogResult<()> {
        let check = r#"{"op": "addConstraint", "constraint": {"kind": "check", "name": "ck", "expression": "a > 0"}}"#;
        let inplace = plan_json(&my80(), &alter(check, r#", "algorithm": "inplace""#))?;
        assert_eq!(
            inplace.warnings,
            vec!["ADD CHECK needs ALGORITHM=COPY on MySQL 8.0.46, so the server rejects ALGORITHM=INPLACE".to_string()]
        );
        let default = plan_json(&my80(), &alter(check, r#", "lock": "none""#))?;
        assert_eq!(
            default.warnings,
            vec![
                "ADD CHECK copies the whole table; writes wait until it finishes".to_string(),
                "ADD CHECK copies the table, so the server rejects LOCK=NONE".to_string(),
            ]
        );
        let instant = |op: &str| alter(op, r#", "algorithm": "instant""#);
        let rename_index = instant(r#"{"op": "renameIndex", "oldName": "ix", "newName": "ix2"}"#);
        assert_eq!(
            plan_json(&my97(), &rename_index)?.warnings,
            vec!["RENAME INDEX needs ALGORITHM=INPLACE on MySQL 9.7.2, so the server rejects ALGORITHM=INSTANT".to_string()]
        );
        assert!(plan_json(&ma106(), &rename_index)?.warnings.is_empty());
        let visibility =
            instant(r#"{"op": "setIndexVisibility", "name": "ix", "invisible": true}"#);
        assert_eq!(plan_json(&my80(), &visibility)?.warnings.len(), 1);
        assert!(plan_json(&ma123(), &visibility)?.warnings.is_empty());
        let drop_check = instant(r#"{"op": "dropCheck", "name": "ck"}"#);
        for server in [my80(), my97(), ma106(), ma123()] {
            assert!(plan_json(&server, &drop_check)?.warnings.is_empty());
        }
        let not_enforced =
            instant(r#"{"op": "setCheckEnforced", "name": "ck", "enforced": false}"#);
        assert!(plan_json(&my80(), &not_enforced)?.warnings.is_empty());
        let comment = instant(r#"{"op": "setOptions", "options": {"comment": "x"}}"#);
        assert_eq!(plan_json(&my80(), &comment)?.warnings.len(), 1);
        assert!(plan_json(&ma106(), &comment)?.warnings.is_empty());
        let drop_indexed = instant(r#"{"op": "dropColumn", "name": "a", "indexed": true}"#);
        assert_eq!(
            plan_json(&my80(), &drop_indexed)?.warnings.last().cloned(),
            Some("DROP COLUMN needs ALGORITHM=INPLACE on MySQL 8.0.46, so the server rejects ALGORITHM=INSTANT".to_string())
        );
        assert_eq!(
            plan_json(&ma106(), &drop_indexed)?.warnings.last().cloned(),
            Some("DROP COLUMN needs ALGORITHM=NOCOPY on MariaDB 10.6.28, so the server rejects ALGORITHM=INSTANT".to_string())
        );
        let drop_plain = instant(r#"{"op": "dropColumn", "name": "a"}"#);
        assert_eq!(plan_json(&my80(), &drop_plain)?.warnings.len(), 1);
        assert_eq!(plan_json(&mysql(8, 0, 28), &drop_plain)?.warnings.len(), 2);
        let rename_column = instant(r#"{"op": "renameColumn", "oldName": "a", "newName": "b"}"#);
        assert!(
            plan_json(&mysql(8, 0, 28), &rename_column)?
                .warnings
                .is_empty()
        );
        assert_eq!(
            plan_json(&mysql(8, 0, 27), &rename_column)?.warnings.len(),
            1
        );
        let nocopy = alter(
            r#"{"op": "dropColumn", "name": "a"}"#,
            r#", "algorithm": "nocopy""#,
        );
        assert!(is_unsupported(&plan_json(&my97(), &nocopy)));
        assert!(sql(&plan_json(&ma106(), &nocopy)?).ends_with("DROP COLUMN `a`, ALGORITHM=NOCOPY"));
        let engine = plan_json(
            &ma123(),
            &alter(r#"{"op": "setOptions", "options": {"engine": "Aria"}}"#, ""),
        )?;
        assert_eq!(sql(&engine), "ALTER TABLE `d`.`t` ENGINE=`Aria`");
        assert!(engine.destructive);
        let engine_json = |algorithm: &str| {
            alter(
                r#"{"op": "setOptions", "options": {"engine": "MyISAM"}}"#,
                &format!(r#", "algorithm": "{algorithm}""#),
            )
        };
        let maria_instant = plan_json(&ma106(), &engine_json("instant"))?;
        assert!(!maria_instant.warnings.iter().any(|w| w.contains("rejects")));
        assert!(
            maria_instant
                .warnings
                .iter()
                .any(|w| w.contains("whatever ALGORITHM says"))
        );
        let maria_inplace = plan_json(&ma106(), &engine_json("inplace"))?;
        assert!(
            maria_inplace
                .warnings
                .iter()
                .any(|w| w.contains("rejects ALGORITHM=INPLACE"))
        );
        let maria_12 = plan_json(&ma123(), &engine_json("nocopy"))?;
        assert!(
            maria_12
                .warnings
                .iter()
                .any(|w| w.contains("rejects ALGORITHM=NOCOPY"))
        );
        let mysql_instant = plan_json(&my80(), &engine_json("instant"))?;
        assert!(
            mysql_instant
                .warnings
                .iter()
                .any(|w| w.contains("rejects ALGORITHM=INSTANT"))
        );
        let drop_period = alter(
            r#"{"op": "dropPeriod", "name": "p"}"#,
            r#", "algorithm": "instant""#,
        );
        assert!(plan_json(&ma123(), &drop_period)?.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn reorganize_is_destructive() -> CatalogResult<()> {
        // LIST: a value no new partition lists loses its rows without an error.
        let json = alter(
            r#"{"op": "reorganizePartitions", "names": ["a"], "into": [
                    {"name": "a1", "values": {"kind": "in", "rows": [[{"kind": "value", "value": {"t": "int", "v": 1}}]]}}]}"#,
            "",
        );
        for server in [my80(), ma123()] {
            let plan = plan_json(&server, &json)?;
            assert!(plan.destructive);
            assert!(
                plan.warnings.iter().any(|w| w.contains(
                    "on LIST partitioning, rows whose values no new partition lists are deleted"
                )),
                "{:?}",
                plan.warnings
            );
        }
        Ok(())
    }

    #[test]
    fn partition_operations_run_alone() -> CatalogResult<()> {
        let json = alter(
            r#"{"op": "addColumn", "column": {"name": "a", "dataType": {"name": "int"}}},
               {"op": "reorganizePartitions", "names": ["pmax"], "into": [
                    {"name": "p2030", "values": {"kind": "lessThan", "bounds": [{"kind": "value", "value": {"t": "int", "v": 2030}}]}},
                    {"name": "pmax", "values": {"kind": "lessThan", "bounds": [{"kind": "maxValue"}]}}]},
               {"op": "truncatePartitions", "names": ["p0"]},
               {"op": "dropPartitions", "names": ["p0"]},
               {"op": "truncatePartitions"},
               {"op": "coalescePartitions", "count": 2},
               {"op": "addPartitionCount", "count": 1},
               {"op": "removePartitioning"}"#,
            r#", "algorithm": "copy""#,
        );
        let plan = plan_json(&my80(), &json)?;
        assert_eq!(
            plan.statements,
            vec![
                "ALTER TABLE `d`.`t` ADD COLUMN `a` INT NULL, ALGORITHM=COPY",
                "ALTER TABLE `d`.`t` REORGANIZE PARTITION `pmax` INTO (PARTITION `p2030` VALUES LESS THAN (2030), PARTITION `pmax` VALUES LESS THAN (MAXVALUE))",
                "ALTER TABLE `d`.`t` TRUNCATE PARTITION `p0`",
                "ALTER TABLE `d`.`t` DROP PARTITION `p0`",
                "ALTER TABLE `d`.`t` TRUNCATE PARTITION ALL",
                "ALTER TABLE `d`.`t` COALESCE PARTITION 2",
                "ALTER TABLE `d`.`t` ADD PARTITION PARTITIONS 1",
                "ALTER TABLE `d`.`t` REMOVE PARTITIONING",
            ]
        );
        assert!(plan.destructive);
        let by = alter(
            r#"{"op": "partitionBy", "partitioning": {"method": {"kind": "key", "columns": ["id"]}, "count": 3}}"#,
            "",
        );
        assert_eq!(
            sql(&plan_json(&ma106(), &by)?),
            "ALTER TABLE `d`.`t` PARTITION BY KEY (`id`) PARTITIONS 3"
        );
        Ok(())
    }

    #[test]
    fn system_versioning() -> CatalogResult<()> {
        let add = alter(
            r#"{"op": "addSystemVersioning", "columns": [
                {"name": "rs", "dataType": {"name": "timestamp", "length": 6}, "rowPeriod": "start", "invisible": true},
                {"name": "re", "dataType": {"name": "timestamp", "length": 6}, "rowPeriod": "end", "invisible": true}]}"#,
            "",
        );
        assert_eq!(
            sql(&plan_json(&ma106(), &add)?),
            "ALTER TABLE `d`.`t` ADD COLUMN `rs` TIMESTAMP(6) GENERATED ALWAYS AS ROW START INVISIBLE, ADD COLUMN `re` TIMESTAMP(6) GENERATED ALWAYS AS ROW END INVISIBLE, ADD PERIOD FOR SYSTEM_TIME (`rs`, `re`), ADD SYSTEM VERSIONING"
        );
        assert!(is_unsupported(&plan_json(&my97(), &add)));
        let drop = alter(
            r#"{"op": "dropSystemVersioning", "columns": {"start": "rs", "end": "re"}}"#,
            "",
        );
        let plan = plan_json(&ma123(), &drop)?;
        assert_eq!(
            sql(&plan),
            "SET STATEMENT system_versioning_alter_history='KEEP' FOR ALTER TABLE `d`.`t` DROP SYSTEM VERSIONING, DROP PERIOD FOR SYSTEM_TIME, DROP COLUMN `rs`, DROP COLUMN `re`"
        );
        assert!(plan.destructive);
        assert_eq!(
            sql(&plan_json(
                &ma123(),
                &alter(r#"{"op": "dropSystemVersioning"}"#, "")
            )?),
            "ALTER TABLE `d`.`t` DROP SYSTEM VERSIONING"
        );
        let versioned = alter(
            r#"{"op": "addColumn", "column": {"name": "z", "dataType": {"name": "int"}}},
               {"op": "dropPeriod", "name": "app"},
               {"op": "addPeriod", "period": {"name": "app2", "start": "s", "end": "e"}}"#,
            r#", "systemVersioned": true"#,
        );
        assert_eq!(
            sql(&plan_json(&ma106(), &versioned)?),
            "SET STATEMENT system_versioning_alter_history='KEEP' FOR ALTER TABLE `d`.`t` ADD COLUMN `z` INT NULL, DROP PERIOD FOR `app`, ADD PERIOD FOR `app2` (`s`, `e`)"
        );
        assert!(is_unsupported(&plan_json(&my80(), &versioned)));
        Ok(())
    }

    #[test]
    fn rename_and_charset() -> CatalogResult<()> {
        let json = alter(
            r#"{"op": "rename", "database": "e", "name": "u"},
               {"op": "convertCharset", "charset": "utf8mb4", "collation": "utf8mb4_bin"}"#,
            "",
        );
        let plan = plan_json(&my97(), &json)?;
        assert_eq!(
            sql(&plan),
            "ALTER TABLE `d`.`t` RENAME TO `e`.`u`, CONVERT TO CHARACTER SET `utf8mb4` COLLATE `utf8mb4_bin`"
        );
        assert!(plan.destructive);
        assert!(is_invalid(&plan_json(&my97(), &alter("", ""))));
        Ok(())
    }
}
