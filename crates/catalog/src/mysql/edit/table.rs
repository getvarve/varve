//! The table designer's `CREATE TABLE`, constraints, table options,
//! partitioning and MariaDB system versioning and periods.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use varve_driver::{Dialect, Value};

use super::types::{ColumnDef, IndexDef, IndexKind, IndexMethod, KeyPart, RowPeriod};
use super::{Plan, Target, fragment, ident, ident_list, invalid, object_name, plain_text};
use crate::model::CatalogResult;
use crate::sql;

/// What a foreign key does when the referenced row changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReferenceAction {
    /// `RESTRICT`.
    Restrict,
    /// `CASCADE`.
    Cascade,
    /// `SET NULL`.
    SetNull,
    /// `NO ACTION`.
    NoAction,
    /// `SET DEFAULT` (parsed by InnoDB but rejected).
    SetDefault,
}

impl ReferenceAction {
    fn sql(self) -> &'static str {
        match self {
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::NoAction => "NO ACTION",
            Self::SetDefault => "SET DEFAULT",
        }
    }
}

/// A foreign key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKeyDef {
    /// Constraint name; the server picks one when `None`.
    #[serde(default)]
    pub name: Option<String>,
    /// Referencing columns.
    pub columns: Vec<String>,
    /// Database of the referenced table; the table's own when `None`.
    #[serde(default)]
    pub references_database: Option<String>,
    /// Referenced table.
    pub references_table: String,
    /// Referenced columns, one per referencing column.
    pub references_columns: Vec<String>,
    /// `ON DELETE`.
    #[serde(default)]
    pub on_delete: Option<ReferenceAction>,
    /// `ON UPDATE`.
    #[serde(default)]
    pub on_update: Option<ReferenceAction>,
}

impl ForeignKeyDef {
    pub(super) fn render(&self, database: &str) -> CatalogResult<String> {
        if self.columns.is_empty() || self.columns.len() != self.references_columns.len() {
            return Err(invalid(
                "a foreign key needs as many referenced columns as referencing columns, at least one",
            ));
        }
        let mut out = String::new();
        if let Some(name) = &self.name {
            out.push_str(&format!("CONSTRAINT {} ", ident(name)?));
        }
        let ref_db = self.references_database.as_deref().unwrap_or(database);
        out.push_str(&format!(
            "FOREIGN KEY ({}) REFERENCES {} ({})",
            ident_list(&self.columns)?,
            object_name(ref_db, &self.references_table)?,
            ident_list(&self.references_columns)?
        ));
        if let Some(action) = self.on_delete {
            out.push_str(&format!(" ON DELETE {}", action.sql()));
        }
        if let Some(action) = self.on_update {
            out.push_str(&format!(" ON UPDATE {}", action.sql()));
        }
        Ok(out)
    }
}

/// A `CHECK` constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckDef {
    /// Constraint name; the server picks one when `None`.
    #[serde(default)]
    pub name: Option<String>,
    /// The condition.
    pub expression: String,
    /// MySQL 8.0.16+: `ENFORCED` (true) or `NOT ENFORCED` (false). MariaDB
    /// always enforces checks.
    #[serde(default)]
    pub enforced: Option<bool>,
}

impl CheckDef {
    pub(super) fn render(&self, t: &Target, plan: &mut Plan) -> CatalogResult<String> {
        let mut out = String::new();
        if let Some(name) = &self.name {
            out.push_str(&format!("CONSTRAINT {} ", ident(name)?));
        }
        out.push_str(&format!(
            "CHECK ({})",
            fragment(&self.expression, "a check expression")?
        ));
        if !t.is_mariadb() && !t.mysql_at(8, 0, 16) {
            plan.warn("MySQL before 8.0.16 parses CHECK constraints but does not enforce them");
        }
        match self.enforced {
            Some(false) if t.is_mariadb() => {
                return Err(t.lacks("NOT ENFORCED check constraints"));
            }
            Some(false) => out.push_str(" NOT ENFORCED"),
            Some(true) if !t.is_mariadb() => out.push_str(" ENFORCED"),
            _ => {}
        }
        Ok(out)
    }
}

/// A table constraint other than a unique key (unique keys are indexes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TableConstraint {
    /// `PRIMARY KEY`.
    PrimaryKey {
        /// Key columns (no functional key parts).
        parts: Vec<KeyPart>,
        /// B-tree or hash.
        #[serde(default)]
        method: Option<IndexMethod>,
        /// Comment on the primary key index.
        #[serde(default)]
        comment: Option<String>,
    },
    /// `FOREIGN KEY`.
    ForeignKey(ForeignKeyDef),
    /// `CHECK`.
    Check(CheckDef),
}

impl TableConstraint {
    pub(super) fn render(
        &self,
        t: &Target,
        database: &str,
        plan: &mut Plan,
    ) -> CatalogResult<String> {
        match self {
            Self::PrimaryKey {
                parts,
                method,
                comment,
            } => {
                if parts
                    .iter()
                    .any(|p| matches!(p, KeyPart::Expression { .. }))
                {
                    return Err(invalid("a primary key cannot have functional key parts"));
                }
                let index = IndexDef {
                    name: None,
                    kind: IndexKind::Index,
                    method: *method,
                    parts: parts.clone(),
                    parser: None,
                    comment: None,
                    invisible: false,
                };
                let mut out = String::from("PRIMARY KEY");
                if let Some(method) = method {
                    out.push(' ');
                    out.push_str(method.clause());
                }
                out.push(' ');
                out.push_str(&index.parts_sql(t, plan)?);
                if let Some(comment) = comment {
                    out.push_str(&format!(" COMMENT {}", plain_text(comment, "a comment")?));
                }
                Ok(out)
            }
            Self::ForeignKey(fk) => fk.render(database),
            Self::Check(check) => check.render(t, plan),
        }
    }

    /// Column names the constraint names directly.
    pub(super) fn names(&self) -> Vec<String> {
        match self {
            Self::PrimaryKey { parts, .. } => parts
                .iter()
                .filter_map(KeyPart::column_name)
                .map(str::to_string)
                .chain(std::iter::once("PRIMARY".to_string()))
                .collect(),
            Self::ForeignKey(fk) => fk
                .columns
                .iter()
                .cloned()
                .chain(fk.name.clone())
                .chain(std::iter::once(fk.references_table.clone()))
                .collect(),
            Self::Check(check) => check.name.clone().into_iter().collect(),
        }
    }
}

/// `DEFAULT`, `0` or `1` for options with a server default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tristate {
    /// `DEFAULT`: follow the server setting.
    Default,
    /// `0`.
    Off,
    /// `1`.
    On,
}

impl Tristate {
    fn sql(self) -> &'static str {
        match self {
            Self::Default => "DEFAULT",
            Self::Off => "0",
            Self::On => "1",
        }
    }
}

/// `ROW_FORMAT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RowFormat {
    /// `DEFAULT`.
    Default,
    /// `DYNAMIC`.
    Dynamic,
    /// `FIXED`.
    Fixed,
    /// `COMPRESSED`.
    Compressed,
    /// `REDUNDANT`.
    Redundant,
    /// `COMPACT`.
    Compact,
    /// `PAGE` (MariaDB Aria).
    Page,
}

impl RowFormat {
    fn sql(self) -> &'static str {
        match self {
            Self::Default => "DEFAULT",
            Self::Dynamic => "DYNAMIC",
            Self::Fixed => "FIXED",
            Self::Compressed => "COMPRESSED",
            Self::Redundant => "REDUNDANT",
            Self::Compact => "COMPACT",
            Self::Page => "PAGE",
        }
    }
}

/// MySQL InnoDB page compression (`COMPRESSION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TableCompression {
    /// `'zlib'`.
    Zlib,
    /// `'lz4'`.
    Lz4,
    /// `'None'`.
    None,
}

/// Table options. Fields left `None` are not written (the server default
/// on create, unchanged on alter).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableOptions {
    /// Storage engine, for example `InnoDB`, `MyISAM`, `MEMORY`, `Aria`.
    #[serde(default)]
    pub engine: Option<String>,
    /// Next `AUTO_INCREMENT` value.
    #[serde(default)]
    pub auto_increment: Option<u64>,
    /// Default character set.
    #[serde(default)]
    pub charset: Option<String>,
    /// Default collation.
    #[serde(default)]
    pub collation: Option<String>,
    /// Table comment.
    #[serde(default)]
    pub comment: Option<String>,
    /// `ROW_FORMAT`.
    #[serde(default)]
    pub row_format: Option<RowFormat>,
    /// `KEY_BLOCK_SIZE`.
    #[serde(default)]
    pub key_block_size: Option<u32>,
    /// `STATS_PERSISTENT`.
    #[serde(default)]
    pub stats_persistent: Option<Tristate>,
    /// `STATS_AUTO_RECALC`.
    #[serde(default)]
    pub stats_auto_recalc: Option<Tristate>,
    /// `STATS_SAMPLE_PAGES`.
    #[serde(default)]
    pub stats_sample_pages: Option<u32>,
    /// `CHECKSUM` (MyISAM, Aria).
    #[serde(default)]
    pub checksum: Option<bool>,
    /// `DELAY_KEY_WRITE` (MyISAM, Aria).
    #[serde(default)]
    pub delay_key_write: Option<bool>,
    /// `PACK_KEYS` (MyISAM, Aria).
    #[serde(default)]
    pub pack_keys: Option<Tristate>,
    /// `MAX_ROWS`.
    #[serde(default)]
    pub max_rows: Option<u64>,
    /// `MIN_ROWS`.
    #[serde(default)]
    pub min_rows: Option<u64>,
    /// `AVG_ROW_LENGTH`.
    #[serde(default)]
    pub avg_row_length: Option<u64>,
    /// MySQL: `COMPRESSION`.
    #[serde(default)]
    pub compression: Option<TableCompression>,
    /// Encryption: `ENCRYPTION='Y'|'N'` on MySQL, `ENCRYPTED=YES|NO` on
    /// MariaDB.
    #[serde(default)]
    pub encryption: Option<bool>,
    /// MySQL: `TABLESPACE`.
    #[serde(default)]
    pub tablespace: Option<String>,
    /// `DATA DIRECTORY`.
    #[serde(default)]
    pub data_directory: Option<String>,
    /// `INDEX DIRECTORY`.
    #[serde(default)]
    pub index_directory: Option<String>,
    /// MariaDB Aria: `PAGE_CHECKSUM`.
    #[serde(default)]
    pub page_checksum: Option<bool>,
    /// MariaDB Aria: `TRANSACTIONAL`.
    #[serde(default)]
    pub transactional: Option<bool>,
    /// MariaDB InnoDB: `PAGE_COMPRESSED`.
    #[serde(default)]
    pub page_compressed: Option<bool>,
    /// MariaDB InnoDB: `PAGE_COMPRESSION_LEVEL` (1 to 9).
    #[serde(default)]
    pub page_compression_level: Option<u8>,
}

fn flag(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

impl TableOptions {
    /// The options as separate `NAME=value` clauses.
    pub(super) fn clauses(&self, t: &Target) -> CatalogResult<Vec<String>> {
        let mut out = Vec::new();
        let mysql_only = |what: &str| -> CatalogResult<()> {
            if t.is_mariadb() {
                Err(t.lacks(what))
            } else {
                Ok(())
            }
        };
        let mariadb_only = |what: &str| -> CatalogResult<()> {
            if t.is_mariadb() {
                Ok(())
            } else {
                Err(t.lacks(what))
            }
        };
        if let Some(engine) = &self.engine {
            out.push(format!("ENGINE={}", ident(engine)?));
        }
        if let Some(value) = self.auto_increment {
            out.push(format!("AUTO_INCREMENT={value}"));
        }
        if let Some(value) = self.avg_row_length {
            out.push(format!("AVG_ROW_LENGTH={value}"));
        }
        if let Some(charset) = &self.charset {
            out.push(format!("DEFAULT CHARSET={}", ident(charset)?));
        }
        if let Some(collation) = &self.collation {
            out.push(format!("COLLATE={}", ident(collation)?));
        }
        if let Some(value) = self.checksum {
            out.push(format!("CHECKSUM={}", flag(value)));
        }
        if let Some(comment) = &self.comment {
            // A comment with a backslash is set by its own statement, built at run
            // time for the session's sql_mode (see `runtime_comment`).
            if self.runtime_comment()?.is_none() {
                out.push(format!("COMMENT={}", plain_text(comment, "a comment")?));
            }
        }
        if let Some(compression) = self.compression {
            mysql_only("the COMPRESSION table option")?;
            out.push(format!(
                "COMPRESSION='{}'",
                match compression {
                    TableCompression::Zlib => "zlib",
                    TableCompression::Lz4 => "lz4",
                    TableCompression::None => "None",
                }
            ));
        }
        if let Some(dir) = &self.data_directory {
            out.push(format!(
                "DATA DIRECTORY={}",
                plain_text(dir, "a directory")?
            ));
        }
        if let Some(dir) = &self.index_directory {
            out.push(format!(
                "INDEX DIRECTORY={}",
                plain_text(dir, "a directory")?
            ));
        }
        if let Some(value) = self.delay_key_write {
            out.push(format!("DELAY_KEY_WRITE={}", flag(value)));
        }
        if let Some(value) = self.encryption {
            if t.is_mariadb() {
                out.push(format!("ENCRYPTED={}", if value { "YES" } else { "NO" }));
            } else {
                out.push(format!("ENCRYPTION='{}'", if value { "Y" } else { "N" }));
            }
        }
        if let Some(value) = self.key_block_size {
            out.push(format!("KEY_BLOCK_SIZE={value}"));
        }
        if let Some(value) = self.max_rows {
            out.push(format!("MAX_ROWS={value}"));
        }
        if let Some(value) = self.min_rows {
            out.push(format!("MIN_ROWS={value}"));
        }
        if let Some(value) = self.pack_keys {
            out.push(format!("PACK_KEYS={}", value.sql()));
        }
        if let Some(format) = self.row_format {
            if format == RowFormat::Page {
                mariadb_only("ROW_FORMAT=PAGE")?;
            }
            out.push(format!("ROW_FORMAT={}", format.sql()));
        }
        if let Some(value) = self.stats_auto_recalc {
            out.push(format!("STATS_AUTO_RECALC={}", value.sql()));
        }
        if let Some(value) = self.stats_persistent {
            out.push(format!("STATS_PERSISTENT={}", value.sql()));
        }
        if let Some(value) = self.stats_sample_pages {
            if value == 0 {
                return Err(invalid("STATS_SAMPLE_PAGES must be at least 1"));
            }
            out.push(format!("STATS_SAMPLE_PAGES={value}"));
        }
        if let Some(tablespace) = &self.tablespace {
            mysql_only("the TABLESPACE table option")?;
            out.push(format!("TABLESPACE {}", ident(tablespace)?));
        }
        if let Some(value) = self.page_checksum {
            mariadb_only("the PAGE_CHECKSUM table option")?;
            out.push(format!("PAGE_CHECKSUM={}", flag(value)));
        }
        if let Some(value) = self.transactional {
            mariadb_only("the TRANSACTIONAL table option")?;
            out.push(format!("TRANSACTIONAL={}", flag(value)));
        }
        if let Some(value) = self.page_compressed {
            mariadb_only("the PAGE_COMPRESSED table option")?;
            out.push(format!("PAGE_COMPRESSED={}", flag(value)));
        }
        if let Some(level) = self.page_compression_level {
            mariadb_only("the PAGE_COMPRESSION_LEVEL table option")?;
            if !(1..=9).contains(&level) {
                return Err(invalid("PAGE_COMPRESSION_LEVEL must be 1 to 9"));
            }
            out.push(format!("PAGE_COMPRESSION_LEVEL={level}"));
        }
        Ok(out)
    }

    /// True when an option needs the table rebuilt by copying it (engine
    /// change).
    /// The table comment when it holds a backslash: `NO_BACKSLASH_ESCAPES`
    /// decides how the server reads a backslash in a quoted string, and the
    /// grammar accepts only a quoted string there, so such a comment is set
    /// by [`super::runtime_comment`] instead of a `COMMENT=` clause.
    ///
    /// # Errors
    ///
    /// `Invalid` for a NUL character, which a comment cannot hold.
    pub(super) fn runtime_comment(&self) -> CatalogResult<Option<&str>> {
        match &self.comment {
            Some(c) if c.contains('\0') => Err(invalid("a comment cannot contain a NUL character")),
            Some(c) if c.contains('\\') => Ok(Some(c.as_str())),
            _ => Ok(None),
        }
    }

    pub(super) fn changes_engine(&self) -> bool {
        self.engine.is_some()
    }

    /// True when the options rebuild the table (in place or by copy).
    pub(super) fn rebuilds(&self) -> bool {
        self.engine.is_some()
            || self.row_format.is_some()
            || self.key_block_size.is_some()
            || self.compression.is_some()
            || self.encryption.is_some()
            || self.page_compressed.is_some()
            || self.page_compression_level.is_some()
    }
}

/// How rows are assigned to partitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PartitionMethod {
    /// `RANGE (expr)`.
    Range {
        /// Integer expression.
        expression: String,
    },
    /// `RANGE COLUMNS (columns)`.
    RangeColumns {
        /// Columns.
        columns: Vec<String>,
    },
    /// `LIST (expr)`.
    List {
        /// Integer expression.
        expression: String,
    },
    /// `LIST COLUMNS (columns)`.
    ListColumns {
        /// Columns.
        columns: Vec<String>,
    },
    /// `[LINEAR] HASH (expr)`.
    Hash {
        /// Integer expression.
        expression: String,
        /// `LINEAR`.
        #[serde(default)]
        linear: bool,
    },
    /// `[LINEAR] KEY [ALGORITHM={1|2}] (columns)`; no columns means the
    /// primary key.
    Key {
        /// Columns.
        #[serde(default)]
        columns: Vec<String>,
        /// `LINEAR`.
        #[serde(default)]
        linear: bool,
        /// Key hashing algorithm, 1 or 2.
        #[serde(default)]
        algorithm: Option<u8>,
    },
}

fn hash_sql(expression: &str, linear: bool) -> CatalogResult<String> {
    Ok(format!(
        "{}HASH ({})",
        if linear { "LINEAR " } else { "" },
        fragment(expression, "a partitioning expression")?
    ))
}

fn key_sql(columns: &[String], linear: bool, algorithm: Option<u8>) -> CatalogResult<String> {
    let mut out = String::new();
    if linear {
        out.push_str("LINEAR ");
    }
    out.push_str("KEY ");
    if let Some(algorithm) = algorithm {
        if !(1..=2).contains(&algorithm) {
            return Err(invalid("the KEY partitioning algorithm is 1 or 2"));
        }
        out.push_str(&format!("ALGORITHM={algorithm} "));
    }
    if columns.is_empty() {
        out.push_str("()");
    } else {
        out.push_str(&format!("({})", ident_list(columns)?));
    }
    Ok(out)
}

impl PartitionMethod {
    fn sql(&self) -> CatalogResult<String> {
        Ok(match self {
            Self::Range { expression } => format!(
                "RANGE ({})",
                fragment(expression, "a partitioning expression")?
            ),
            Self::RangeColumns { columns } => format!("RANGE COLUMNS ({})", ident_list(columns)?),
            Self::List { expression } => format!(
                "LIST ({})",
                fragment(expression, "a partitioning expression")?
            ),
            Self::ListColumns { columns } => format!("LIST COLUMNS ({})", ident_list(columns)?),
            Self::Hash { expression, linear } => hash_sql(expression, *linear)?,
            Self::Key {
                columns,
                linear,
                algorithm,
            } => key_sql(columns, *linear, *algorithm)?,
        })
    }

    fn is_range(&self) -> bool {
        matches!(self, Self::Range { .. } | Self::RangeColumns { .. })
    }

    fn is_list(&self) -> bool {
        matches!(self, Self::List { .. } | Self::ListColumns { .. })
    }

    /// Values per bound: the column count for `COLUMNS` methods, else 1.
    fn arity(&self) -> usize {
        match self {
            Self::RangeColumns { columns } | Self::ListColumns { columns } => columns.len(),
            _ => 1,
        }
    }
}

/// How partitions are split into subpartitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SubPartitionMethod {
    /// `[LINEAR] HASH (expr)`.
    Hash {
        /// Integer expression.
        expression: String,
        /// `LINEAR`.
        #[serde(default)]
        linear: bool,
    },
    /// `[LINEAR] KEY [ALGORITHM={1|2}] (columns)`.
    Key {
        /// Columns.
        #[serde(default)]
        columns: Vec<String>,
        /// `LINEAR`.
        #[serde(default)]
        linear: bool,
        /// Key hashing algorithm, 1 or 2.
        #[serde(default)]
        algorithm: Option<u8>,
    },
}

/// `SUBPARTITION BY ...`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubPartitioning {
    /// Hash or key.
    pub method: SubPartitionMethod,
    /// `SUBPARTITIONS n`, when the subpartitions are not named one by one.
    #[serde(default)]
    pub count: Option<u32>,
}

/// One bound value of a partition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PartitionBound {
    /// `MAXVALUE` (range partitions).
    MaxValue,
    /// A constant.
    Value {
        /// The value.
        value: Value,
    },
    /// An expression.
    Expression {
        /// The expression.
        sql: String,
    },
}

impl PartitionBound {
    fn sql(&self) -> CatalogResult<String> {
        Ok(match self {
            Self::MaxValue => "MAXVALUE".into(),
            Self::Value { value } => sql::literal(Dialect::Mysql, value)?,
            Self::Expression { sql } => fragment(sql, "a partition bound")?,
        })
    }
}

/// The values a partition holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PartitionValues {
    /// `VALUES LESS THAN (...)`, one bound per partitioning column.
    LessThan {
        /// Bounds.
        bounds: Vec<PartitionBound>,
    },
    /// `VALUES IN (...)`: each row has one value per partitioning column.
    In {
        /// Value rows.
        rows: Vec<Vec<PartitionBound>>,
    },
    /// MariaDB: `DEFAULT` list partition for every other value.
    Default,
}

/// A subpartition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubPartitionDef {
    /// Name.
    pub name: String,
    /// Storage engine.
    #[serde(default)]
    pub engine: Option<String>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
    /// `DATA DIRECTORY`.
    #[serde(default)]
    pub data_directory: Option<String>,
}

/// A partition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionDef {
    /// Name.
    pub name: String,
    /// Values (range and list partitions; `None` for hash and key).
    #[serde(default)]
    pub values: Option<PartitionValues>,
    /// Storage engine.
    #[serde(default)]
    pub engine: Option<String>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
    /// `DATA DIRECTORY`.
    #[serde(default)]
    pub data_directory: Option<String>,
    /// Named subpartitions.
    #[serde(default)]
    pub subpartitions: Vec<SubPartitionDef>,
}

fn partition_options(
    engine: Option<&String>,
    comment: Option<&String>,
    data_directory: Option<&String>,
) -> CatalogResult<String> {
    let mut out = String::new();
    if let Some(engine) = engine {
        out.push_str(&format!(" ENGINE={}", ident(engine)?));
    }
    if let Some(comment) = comment {
        out.push_str(&format!(" COMMENT={}", plain_text(comment, "a comment")?));
    }
    if let Some(dir) = data_directory {
        out.push_str(&format!(
            " DATA DIRECTORY={}",
            plain_text(dir, "a directory")?
        ));
    }
    Ok(out)
}

impl PartitionDef {
    /// `PARTITION name [VALUES ...] [options] [(subpartitions)]`. `method`
    /// is known when the whole partitioning is written and checks the
    /// values against it.
    pub(super) fn render(
        &self,
        t: &Target,
        method: Option<&PartitionMethod>,
    ) -> CatalogResult<String> {
        let mut out = format!("PARTITION {}", ident(&self.name)?);
        if let Some(method) = method {
            let fits = match (&self.values, method) {
                (None, m) => !m.is_range() && !m.is_list(),
                (Some(PartitionValues::LessThan { .. }), m) => m.is_range(),
                (Some(PartitionValues::In { .. } | PartitionValues::Default), m) => m.is_list(),
            };
            if !fits {
                return Err(invalid(format!(
                    "partition {}: range partitions need VALUES LESS THAN, list partitions VALUES IN, hash and key partitions no values",
                    self.name
                )));
            }
        }
        let arity = method.map(PartitionMethod::arity);
        match &self.values {
            None => {}
            Some(PartitionValues::LessThan { bounds }) => {
                if bounds.is_empty() || arity.is_some_and(|a| a != bounds.len()) {
                    return Err(invalid(format!(
                        "partition {}: VALUES LESS THAN needs one bound per partitioning column",
                        self.name
                    )));
                }
                let bounds = bounds
                    .iter()
                    .map(PartitionBound::sql)
                    .collect::<CatalogResult<Vec<_>>>()?;
                out.push_str(&format!(" VALUES LESS THAN ({})", bounds.join(", ")));
            }
            Some(PartitionValues::In { rows }) => {
                if rows.is_empty() {
                    return Err(invalid(format!(
                        "partition {}: VALUES IN needs at least one value",
                        self.name
                    )));
                }
                let width = arity.unwrap_or_else(|| rows.first().map_or(1, Vec::len));
                let mut items = Vec::with_capacity(rows.len());
                for row in rows {
                    if row.len() != width || row.is_empty() {
                        return Err(invalid(format!(
                            "partition {}: every VALUES IN row needs one value per partitioning column",
                            self.name
                        )));
                    }
                    if row.iter().any(|b| matches!(b, PartitionBound::MaxValue)) {
                        return Err(invalid("MAXVALUE is only a range partition bound"));
                    }
                    let values = row
                        .iter()
                        .map(PartitionBound::sql)
                        .collect::<CatalogResult<Vec<_>>>()?;
                    items.push(if width == 1 {
                        values.join(", ")
                    } else {
                        format!("({})", values.join(", "))
                    });
                }
                out.push_str(&format!(" VALUES IN ({})", items.join(", ")));
            }
            Some(PartitionValues::Default) => {
                if !t.is_mariadb() {
                    return Err(t.lacks("DEFAULT list partitions"));
                }
                out.push_str(" DEFAULT");
            }
        }
        out.push_str(&partition_options(
            self.engine.as_ref(),
            self.comment.as_ref(),
            self.data_directory.as_ref(),
        )?);
        if !self.subpartitions.is_empty() {
            let subs = self
                .subpartitions
                .iter()
                .map(|s| {
                    Ok(format!(
                        "SUBPARTITION {}{}",
                        ident(&s.name)?,
                        partition_options(
                            s.engine.as_ref(),
                            s.comment.as_ref(),
                            s.data_directory.as_ref()
                        )?
                    ))
                })
                .collect::<CatalogResult<Vec<_>>>()?;
            out.push_str(&format!(" ({})", subs.join(", ")));
        }
        Ok(out)
    }
}

/// Partitioning of a table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Partitioning {
    /// How rows are assigned.
    pub method: PartitionMethod,
    /// `PARTITIONS n` for hash and key partitioning without named
    /// partitions.
    #[serde(default)]
    pub count: Option<u32>,
    /// Subpartitioning (range and list partitioning only).
    #[serde(default)]
    pub subpartition: Option<SubPartitioning>,
    /// Named partitions (required for range and list).
    #[serde(default)]
    pub partitions: Vec<PartitionDef>,
}

impl Partitioning {
    /// `PARTITION BY ...`.
    pub(super) fn render(&self, t: &Target) -> CatalogResult<String> {
        let method = &self.method;
        if (method.is_range() || method.is_list()) && self.partitions.is_empty() {
            return Err(invalid("range and list partitioning need named partitions"));
        }
        if (method.is_range() || method.is_list()) && self.count.is_some() {
            return Err(invalid(
                "PARTITIONS n applies to hash and key partitioning; name range and list partitions instead",
            ));
        }
        if let Some(count) = self.count {
            if count == 0 {
                return Err(invalid("PARTITIONS needs at least 1"));
            }
            if !self.partitions.is_empty() && count as usize != self.partitions.len() {
                return Err(invalid("PARTITIONS n must match the named partitions"));
            }
        }
        let mut out = format!("PARTITION BY {}", method.sql()?);
        if let Some(count) = self.count {
            out.push_str(&format!(" PARTITIONS {count}"));
        }
        if let Some(sub) = &self.subpartition {
            if !(method.is_range() || method.is_list()) {
                return Err(invalid(
                    "only range and list partitions can be subpartitioned",
                ));
            }
            out.push_str(" SUBPARTITION BY ");
            out.push_str(&match &sub.method {
                SubPartitionMethod::Hash { expression, linear } => hash_sql(expression, *linear)?,
                SubPartitionMethod::Key {
                    columns,
                    linear,
                    algorithm,
                } => key_sql(columns, *linear, *algorithm)?,
            });
            if let Some(count) = sub.count {
                if count == 0 {
                    return Err(invalid("SUBPARTITIONS needs at least 1"));
                }
                out.push_str(&format!(" SUBPARTITIONS {count}"));
            }
        }
        let named_subs: HashSet<usize> = self
            .partitions
            .iter()
            .map(|p| p.subpartitions.len())
            .collect();
        if named_subs.len() > 1 {
            return Err(invalid(
                "every partition needs the same number of named subpartitions",
            ));
        }
        if self.subpartition.is_none() && named_subs.iter().any(|n| *n > 0) {
            return Err(invalid("named subpartitions need SUBPARTITION BY"));
        }
        if !self.partitions.is_empty() {
            let parts = self
                .partitions
                .iter()
                .map(|p| p.render(t, Some(method)))
                .collect::<CatalogResult<Vec<_>>>()?;
            out.push_str(&format!(" ({})", parts.join(", ")));
        }
        Ok(out)
    }
}

/// Names of a MariaDB system-versioning table's row start and end columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersioningColumns {
    /// Row start column.
    pub start: String,
    /// Row end column.
    pub end: String,
}

/// MariaDB `WITH SYSTEM VERSIONING`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemVersioning {
    /// Explicit row start and end columns (declared among the columns with
    /// [`ColumnDef::row_period`]); `None` uses MariaDB's implicit hidden
    /// columns.
    #[serde(default)]
    pub period: Option<VersioningColumns>,
}

/// MariaDB 10.4+ application-time period, `PERIOD FOR name(start, end)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationPeriod {
    /// Period name.
    pub name: String,
    /// Start column (`DATE`, `DATETIME` or `TIMESTAMP`).
    pub start: String,
    /// End column.
    pub end: String,
}

impl ApplicationPeriod {
    pub(super) fn render(&self, t: &Target) -> CatalogResult<String> {
        if !t.mariadb_at(10, 4) {
            return Err(t.lacks("application-time periods"));
        }
        if self.name.eq_ignore_ascii_case("SYSTEM_TIME") {
            return Err(invalid(
                "SYSTEM_TIME is the system-versioning period; use system versioning for it",
            ));
        }
        Ok(format!(
            "PERIOD FOR {} ({}, {})",
            ident(&self.name)?,
            ident(&self.start)?,
            ident(&self.end)?
        ))
    }
}

/// `PERIOD FOR SYSTEM_TIME (start, end)`.
pub(super) fn system_period(columns: &VersioningColumns) -> CatalogResult<String> {
    Ok(format!(
        "PERIOD FOR SYSTEM_TIME ({}, {})",
        ident(&columns.start)?,
        ident(&columns.end)?
    ))
}

/// `CREATE TABLE` from the table designer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTable {
    /// Database.
    pub database: String,
    /// Table name.
    pub name: String,
    /// `IF NOT EXISTS`.
    #[serde(default)]
    pub if_not_exists: bool,
    /// Columns in order.
    pub columns: Vec<ColumnDef>,
    /// Primary key, foreign keys and checks.
    #[serde(default)]
    pub constraints: Vec<TableConstraint>,
    /// Indexes and unique keys.
    #[serde(default)]
    pub indexes: Vec<IndexDef>,
    /// Table options.
    #[serde(default)]
    pub options: TableOptions,
    /// Partitioning.
    #[serde(default)]
    pub partitioning: Option<Partitioning>,
    /// MariaDB: system versioning.
    #[serde(default)]
    pub system_versioning: Option<SystemVersioning>,
    /// MariaDB 10.4+: application-time periods.
    #[serde(default)]
    pub periods: Vec<ApplicationPeriod>,
}

/// `DROP TABLE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropTable {
    /// Database.
    pub database: String,
    /// Table name.
    pub name: String,
    /// `IF EXISTS`.
    #[serde(default)]
    pub if_exists: bool,
}

pub(super) fn create(t: &Target, c: &CreateTable, plan: &mut Plan) -> CatalogResult<()> {
    if c.columns.is_empty() {
        return Err(invalid("a table needs at least one column"));
    }
    let mut seen = HashSet::new();
    for column in &c.columns {
        if !seen.insert(column.name.to_lowercase()) {
            return Err(invalid(format!("column {} appears twice", column.name)));
        }
    }
    let primary_keys = c
        .constraints
        .iter()
        .filter(|k| matches!(k, TableConstraint::PrimaryKey { .. }))
        .count();
    if primary_keys > 1 {
        return Err(invalid("a table has at most one primary key"));
    }
    let mut elements = Vec::new();
    for column in &c.columns {
        elements.push(column.render(t, plan)?);
    }
    for constraint in &c.constraints {
        if let TableConstraint::PrimaryKey { .. } = constraint {
            elements.push(constraint.render(t, &c.database, plan)?);
        }
    }
    for index in &c.indexes {
        elements.push(index.inline(t, plan)?);
    }
    for constraint in &c.constraints {
        if !matches!(constraint, TableConstraint::PrimaryKey { .. }) {
            elements.push(constraint.render(t, &c.database, plan)?);
        }
    }
    let row_columns: Vec<(&str, RowPeriod)> = c
        .columns
        .iter()
        .filter_map(|col| col.row_period.map(|p| (col.name.as_str(), p)))
        .collect();
    match &c.system_versioning {
        None => {
            if !row_columns.is_empty() {
                return Err(invalid("row start and end columns need system versioning"));
            }
        }
        Some(versioning) => {
            if !t.is_mariadb() {
                return Err(t.lacks("system-versioned tables"));
            }
            match &versioning.period {
                None => {
                    if !row_columns.is_empty() {
                        return Err(invalid(
                            "row start and end columns need the SYSTEM_TIME period that names them",
                        ));
                    }
                }
                Some(period) => {
                    let has = |name: &str, kind: RowPeriod| {
                        row_columns
                            .iter()
                            .any(|(n, p)| n.eq_ignore_ascii_case(name) && *p == kind)
                    };
                    if row_columns.len() != 2
                        || !has(&period.start, RowPeriod::Start)
                        || !has(&period.end, RowPeriod::End)
                    {
                        return Err(invalid(
                            "the SYSTEM_TIME period must name the row start and row end columns",
                        ));
                    }
                    elements.push(system_period(period)?);
                }
            }
        }
    }
    for period in &c.periods {
        elements.push(period.render(t)?);
    }
    let mut sql = format!(
        "CREATE TABLE {}{} (\n  {}\n)",
        if c.if_not_exists {
            "IF NOT EXISTS "
        } else {
            ""
        },
        object_name(&c.database, &c.name)?,
        elements.join(",\n  ")
    );
    let mut options = c.options.clauses(t)?;
    if c.system_versioning.is_some() {
        options.push("WITH SYSTEM VERSIONING".into());
    }
    if !options.is_empty() {
        sql.push(' ');
        sql.push_str(&options.join(" "));
    }
    if let Some(partitioning) = &c.partitioning {
        sql.push('\n');
        sql.push_str(&partitioning.render(t)?);
    }
    plan.push(sql);
    if let Some(comment) = c.options.runtime_comment()? {
        super::runtime_comment(&object_name(&c.database, &c.name)?, comment, plan);
    }
    if let Some(warning) = c
        .options
        .engine
        .as_deref()
        .and_then(crate::mysql::engine_warning)
    {
        plan.warn(warning);
    }
    Ok(())
}

pub(super) fn drop(c: &DropTable, plan: &mut Plan) -> CatalogResult<()> {
    let name = object_name(&c.database, &c.name)?;
    plan.destructive();
    plan.warn(format!("drops table {name} and all its rows"));
    plan.push(format!(
        "DROP TABLE {}{name}",
        if c.if_exists { "IF EXISTS " } else { "" }
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::model::CatalogResult;

    const ORDERS: &str = r#"{
        "change": "createTable", "database": "shop", "name": "orders",
        "columns": [
            {"name": "id", "dataType": {"name": "bigint", "unsigned": true}, "nullable": false, "autoIncrement": true},
            {"name": "customer", "dataType": {"name": "int"}, "nullable": false},
            {"name": "status", "dataType": {"name": "enum", "values": ["new", "paid"]}, "nullable": false,
             "default": {"kind": "literal", "value": {"t": "text", "v": "new"}}},
            {"name": "total", "dataType": {"name": "decimal", "length": 10, "scale": 2}, "nullable": false,
             "default": {"kind": "literal", "value": {"t": "decimal", "v": "0.00"}}, "comment": "gross"}
        ],
        "constraints": [
            {"kind": "primaryKey", "parts": [{"kind": "column", "name": "id"}]},
            {"kind": "foreignKey", "name": "fk_customer", "columns": ["customer"], "referencesTable": "customers",
             "referencesColumns": ["id"], "onDelete": "cascade"},
            {"kind": "check", "name": "ck_total", "expression": "total >= 0"}
        ],
        "indexes": [{"name": "ix_status", "kind": "index", "parts": [{"kind": "column", "name": "status"}]}],
        "options": {"engine": "InnoDB", "charset": "utf8mb4", "collation": "utf8mb4_bin", "comment": "orders", "autoIncrement": 1000, "rowFormat": "dynamic"}
    }"#;

    #[test]
    fn create_table_same_sql_on_both_engines() -> CatalogResult<()> {
        let expected = "CREATE TABLE `shop`.`orders` (\n  `id` BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,\n  `customer` INT NOT NULL,\n  `status` ENUM('new','paid') NOT NULL DEFAULT 'new',\n  `total` DECIMAL(10,2) NOT NULL DEFAULT 0.00 COMMENT 'gross',\n  PRIMARY KEY (`id`),\n  INDEX `ix_status` (`status`),\n  CONSTRAINT `fk_customer` FOREIGN KEY (`customer`) REFERENCES `shop`.`customers` (`id`) ON DELETE CASCADE,\n  CONSTRAINT `ck_total` CHECK (total >= 0)\n) ENGINE=`InnoDB` AUTO_INCREMENT=1000 DEFAULT CHARSET=`utf8mb4` COLLATE=`utf8mb4_bin` COMMENT='orders' ROW_FORMAT=DYNAMIC";
        for server in [my80(), my97(), ma106(), ma123()] {
            let plan = plan_json(&server, ORDERS)?;
            assert_eq!(sql(&plan), expected);
            assert!(!plan.destructive && !plan.transactional);
        }
        let old = plan_json(&mysql(8, 0, 15), ORDERS)?;
        assert_eq!(old.warnings.len(), 1);
        Ok(())
    }

    #[test]
    fn partitioned_table() -> CatalogResult<()> {
        let json = r#"{
            "change": "createTable", "database": "d", "name": "events",
            "columns": [
                {"name": "id", "dataType": {"name": "int"}, "nullable": false},
                {"name": "at", "dataType": {"name": "date"}, "nullable": false}
            ],
            "constraints": [{"kind": "primaryKey", "parts": [{"kind": "column", "name": "id"}, {"kind": "column", "name": "at"}]}],
            "partitioning": {
                "method": {"kind": "range", "expression": "YEAR(at)"},
                "subpartition": {"method": {"kind": "key", "columns": ["id"], "linear": true, "algorithm": 2}, "count": 2},
                "partitions": [
                    {"name": "p2020", "values": {"kind": "lessThan", "bounds": [{"kind": "value", "value": {"t": "int", "v": 2020}}]}, "comment": "old"},
                    {"name": "pmax", "values": {"kind": "lessThan", "bounds": [{"kind": "maxValue"}]}}
                ]
            }
        }"#;
        let plan = plan_json(&my80(), json)?;
        assert_eq!(
            sql(&plan),
            "CREATE TABLE `d`.`events` (\n  `id` INT NOT NULL,\n  `at` DATE NOT NULL,\n  PRIMARY KEY (`id`, `at`)\n)\nPARTITION BY RANGE (YEAR(at)) SUBPARTITION BY LINEAR KEY ALGORITHM=2 (`id`) SUBPARTITIONS 2 (PARTITION `p2020` VALUES LESS THAN (2020) COMMENT='old', PARTITION `pmax` VALUES LESS THAN (MAXVALUE))"
        );

        let list = r#"{
            "change": "createTable", "database": "d", "name": "l",
            "columns": [{"name": "a", "dataType": {"name": "int"}}, {"name": "b", "dataType": {"name": "varchar", "length": 5}}],
            "partitioning": {
                "method": {"kind": "listColumns", "columns": ["a", "b"]},
                "partitions": [
                    {"name": "x", "values": {"kind": "in", "rows": [[{"kind": "value", "value": {"t": "int", "v": 1}}, {"kind": "value", "value": {"t": "text", "v": "a"}}]]}},
                    {"name": "rest", "values": {"kind": "default"}}
                ]
            }
        }"#;
        assert_eq!(
            sql(&plan_json(&ma123(), list)?),
            "CREATE TABLE `d`.`l` (\n  `a` INT NULL,\n  `b` VARCHAR(5) NULL\n)\nPARTITION BY LIST COLUMNS (`a`, `b`) (PARTITION `x` VALUES IN ((1, 'a')), PARTITION `rest` DEFAULT)"
        );
        assert!(is_unsupported(&plan_json(&my97(), list)));

        let hash = r#"{
            "change": "createTable", "database": "d", "name": "h",
            "columns": [{"name": "a", "dataType": {"name": "int"}}],
            "partitioning": {"method": {"kind": "hash", "expression": "a", "linear": true}, "count": 4}
        }"#;
        assert_eq!(
            sql(&plan_json(&ma106(), hash)?),
            "CREATE TABLE `d`.`h` (\n  `a` INT NULL\n)\nPARTITION BY LINEAR HASH (a) PARTITIONS 4"
        );
        let bad = hash.replace(
            r#""count": 4"#,
            r#""partitions": [{"name": "p", "values": {"kind": "default"}}]"#,
        );
        assert!(is_invalid(&plan_json(&ma106(), &bad)));
        Ok(())
    }

    #[test]
    fn system_versioning_and_periods() -> CatalogResult<()> {
        let json = r#"{
            "change": "createTable", "database": "d", "name": "v",
            "columns": [
                {"name": "id", "dataType": {"name": "int"}, "nullable": false},
                {"name": "valid_from", "dataType": {"name": "date"}, "nullable": false},
                {"name": "valid_to", "dataType": {"name": "date"}, "nullable": false},
                {"name": "note", "dataType": {"name": "text"}, "withoutSystemVersioning": true},
                {"name": "rs", "dataType": {"name": "timestamp", "length": 6}, "rowPeriod": "start", "invisible": true},
                {"name": "re", "dataType": {"name": "timestamp", "length": 6}, "rowPeriod": "end", "invisible": true}
            ],
            "systemVersioning": {"period": {"start": "rs", "end": "re"}},
            "periods": [{"name": "validity", "start": "valid_from", "end": "valid_to"}]
        }"#;
        assert_eq!(
            sql(&plan_json(&ma106(), json)?),
            "CREATE TABLE `d`.`v` (\n  `id` INT NOT NULL,\n  `valid_from` DATE NOT NULL,\n  `valid_to` DATE NOT NULL,\n  `note` TEXT NULL WITHOUT SYSTEM VERSIONING,\n  `rs` TIMESTAMP(6) GENERATED ALWAYS AS ROW START INVISIBLE,\n  `re` TIMESTAMP(6) GENERATED ALWAYS AS ROW END INVISIBLE,\n  PERIOD FOR SYSTEM_TIME (`rs`, `re`),\n  PERIOD FOR `validity` (`valid_from`, `valid_to`)\n) WITH SYSTEM VERSIONING"
        );
        assert!(is_unsupported(&plan_json(&my97(), json)));
        let implicit = r#"{"change": "createTable", "database": "d", "name": "v",
            "columns": [{"name": "id", "dataType": {"name": "int"}}], "systemVersioning": {}}"#;
        assert_eq!(
            sql(&plan_json(&ma123(), implicit)?),
            "CREATE TABLE `d`.`v` (\n  `id` INT NULL\n) WITH SYSTEM VERSIONING"
        );
        let wrong = json.replace(r#""end": "re"}"#, r#""end": "rs"}"#);
        assert!(is_invalid(&plan_json(&ma106(), &wrong)));
        Ok(())
    }

    #[test]
    fn engine_specific_options() -> CatalogResult<()> {
        let json = r#"{"change": "createTable", "database": "d", "name": "t",
            "columns": [{"name": "id", "dataType": {"name": "int"}}],
            "options": {"encryption": false, "statsPersistent": "on", "packKeys": "default", "keyBlockSize": 8}}"#;
        assert!(
            sql(&plan_json(&my80(), json)?).ends_with(
                ") ENCRYPTION='N' KEY_BLOCK_SIZE=8 PACK_KEYS=DEFAULT STATS_PERSISTENT=1"
            )
        );
        assert!(
            sql(&plan_json(&ma106(), json)?)
                .ends_with(") ENCRYPTED=NO KEY_BLOCK_SIZE=8 PACK_KEYS=DEFAULT STATS_PERSISTENT=1")
        );
        let aria = r#"{"change": "createTable", "database": "d", "name": "t",
            "columns": [{"name": "id", "dataType": {"name": "int"}}],
            "options": {"engine": "Aria", "transactional": true, "pageChecksum": true, "rowFormat": "page"}}"#;
        assert!(
            sql(&plan_json(&ma123(), aria)?)
                .ends_with(") ENGINE=`Aria` ROW_FORMAT=PAGE PAGE_CHECKSUM=1 TRANSACTIONAL=1")
        );
        assert!(is_unsupported(&plan_json(&my97(), aria)));
        let compression = r#"{"change": "createTable", "database": "d", "name": "t",
            "columns": [{"name": "id", "dataType": {"name": "int"}}], "options": {"compression": "lz4"}}"#;
        assert!(sql(&plan_json(&my97(), compression)?).ends_with(") COMPRESSION='lz4'"));
        assert!(is_unsupported(&plan_json(&ma106(), compression)));
        Ok(())
    }

    #[test]
    fn invalid_tables() {
        assert!(is_invalid(&plan_json(
            &my80(),
            r#"{"change": "createTable", "database": "d", "name": "t", "columns": []}"#
        )));
        assert!(is_invalid(&plan_json(
            &my80(),
            r#"{"change": "createTable", "database": "d", "name": "t",
               "columns": [{"name": "a", "dataType": {"name": "int"}}, {"name": "A", "dataType": {"name": "int"}}]}"#
        )));
        assert!(is_invalid(&plan_json(
            &my80(),
            r#"{"change": "createTable", "database": "d", "name": "t",
               "columns": [{"name": "a", "dataType": {"name": "int"}}],
               "constraints": [{"kind": "foreignKey", "columns": ["a"], "referencesTable": "x", "referencesColumns": []}]}"#
        )));
    }

    #[test]
    fn drop_table() -> CatalogResult<()> {
        let plan = plan_json(
            &my80(),
            r#"{"change": "dropTable", "database": "d", "name": "t", "ifExists": true}"#,
        )?;
        assert_eq!(sql(&plan), "DROP TABLE IF EXISTS `d`.`t`");
        assert!(plan.destructive);
        Ok(())
    }
}
