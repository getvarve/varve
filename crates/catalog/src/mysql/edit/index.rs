//! The index editor: create, drop, rename, visibility and replacing a
//! definition.

use serde::{Deserialize, Serialize};

use super::alter::{AlgorithmOption, AlterOp, AlterTable, LockOption};
use super::cost::Level;
use super::types::{IndexDef, IndexKind, visibility_keyword};
use super::{Plan, Target, ident, invalid, object_name};
use crate::model::CatalogResult;

/// `CREATE INDEX`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateIndex {
    /// Database.
    pub database: String,
    /// Table.
    pub table: String,
    /// The index; it needs a name.
    pub index: IndexDef,
    /// `ALGORITHM=`.
    #[serde(default)]
    pub algorithm: Option<AlgorithmOption>,
    /// `LOCK=`.
    #[serde(default)]
    pub lock: Option<LockOption>,
}

/// `DROP INDEX`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropIndex {
    /// Database.
    pub database: String,
    /// Table.
    pub table: String,
    /// Index name (`PRIMARY` drops the primary key).
    pub name: String,
    /// `ALGORITHM=`.
    #[serde(default)]
    pub algorithm: Option<AlgorithmOption>,
    /// `LOCK=`.
    #[serde(default)]
    pub lock: Option<LockOption>,
}

/// Renames an index (`ALTER TABLE ... RENAME INDEX`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameIndex {
    /// Database.
    pub database: String,
    /// Table.
    pub table: String,
    /// Current name.
    pub old_name: String,
    /// New name.
    pub new_name: String,
}

/// Makes an index invisible (MySQL) or ignored (MariaDB 10.6+), or
/// visible again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetIndexVisibility {
    /// Database.
    pub database: String,
    /// Table.
    pub table: String,
    /// Index name.
    pub name: String,
    /// Invisible when true.
    pub invisible: bool,
}

/// Replaces an index with a new definition in one `ALTER TABLE` (drop and
/// add), for changes no `ALTER INDEX` covers: columns, kind, comment, ...
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplaceIndex {
    /// Database.
    pub database: String,
    /// Table.
    pub table: String,
    /// Name of the index to replace.
    pub old_name: String,
    /// The new definition; it needs a name (it may be the old one).
    pub index: IndexDef,
    /// `ALGORITHM=`.
    #[serde(default)]
    pub algorithm: Option<AlgorithmOption>,
    /// `LOCK=`.
    #[serde(default)]
    pub lock: Option<LockOption>,
}

/// ` ALGORITHM=... LOCK=...` for `CREATE INDEX` and `DROP INDEX`.
fn index_modifiers(
    t: &Target,
    algorithm: Option<AlgorithmOption>,
    lock: Option<LockOption>,
    level: Level,
    plan: &mut Plan,
) -> CatalogResult<String> {
    let mut out = String::new();
    if let Some(algorithm) = algorithm {
        let word = match algorithm {
            AlgorithmOption::Default => "DEFAULT",
            AlgorithmOption::Instant => "INSTANT",
            AlgorithmOption::Inplace => "INPLACE",
            AlgorithmOption::Nocopy => {
                if !t.mariadb_at(10, 3) {
                    return Err(t.lacks("ALGORITHM=NOCOPY"));
                }
                "NOCOPY"
            }
            AlgorithmOption::Copy => "COPY",
        };
        let requested = match algorithm {
            AlgorithmOption::Instant => Some(Level::Instant),
            AlgorithmOption::Nocopy => Some(Level::Nocopy),
            AlgorithmOption::Inplace => Some(Level::Inplace),
            AlgorithmOption::Default | AlgorithmOption::Copy => None,
        };
        if let Some(requested) = requested
            && level > requested
        {
            plan.warn(format!(
                "the index change needs ALGORITHM={} on {}, so the server rejects ALGORITHM={}",
                level.keyword(),
                t.describe(),
                requested.keyword()
            ));
        }
        out.push_str(&format!(" ALGORITHM={word}"));
    }
    if let Some(lock) = lock {
        out.push_str(&format!(
            " LOCK={}",
            match lock {
                LockOption::Default => "DEFAULT",
                LockOption::None => "NONE",
                LockOption::Shared => "SHARED",
                LockOption::Exclusive => "EXCLUSIVE",
            }
        ));
    }
    Ok(out)
}

fn kind_warnings(index: &IndexDef, lock: Option<LockOption>, plan: &mut Plan) {
    match index.kind {
        IndexKind::Fulltext => {
            plan.warn("the first full-text index of an InnoDB table rebuilds the table");
            if lock == Some(LockOption::None) {
                plan.warn(
                    "full-text indexes cannot be built with LOCK=NONE; the server rejects it",
                );
            }
        }
        IndexKind::Spatial if lock == Some(LockOption::None) => {
            plan.warn("spatial indexes cannot be built with LOCK=NONE; the server rejects it");
        }
        _ => {}
    }
}

pub(super) fn create(t: &Target, c: &CreateIndex, plan: &mut Plan) -> CatalogResult<()> {
    let table = object_name(&c.database, &c.table)?;
    let mut sql = c.index.create_statement(t, &table, plan)?;
    kind_warnings(&c.index, c.lock, plan);
    let level = match c.index.kind {
        IndexKind::Fulltext | IndexKind::Spatial => Level::Inplace,
        IndexKind::Index | IndexKind::Unique => Level::pick(t, Level::Inplace, Level::Nocopy),
    };
    sql.push_str(&index_modifiers(t, c.algorithm, c.lock, level, plan)?);
    plan.push(sql);
    Ok(())
}

pub(super) fn drop(t: &Target, c: &DropIndex, plan: &mut Plan) -> CatalogResult<()> {
    let table = object_name(&c.database, &c.table)?;
    plan.destructive();
    if c.name.eq_ignore_ascii_case("PRIMARY") {
        plan.warn("dropping the primary key without adding another copies the table");
    }
    let modifiers = index_modifiers(
        t,
        c.algorithm,
        c.lock,
        Level::pick(t, Level::Inplace, Level::Nocopy),
        plan,
    )?;
    if t.is_mariadb() && !modifiers.is_empty() {
        // MariaDB's DROP INDEX takes no ALGORITHM or LOCK; ALTER TABLE does.
        let options = modifiers.trim_start().replace(' ', ", ");
        plan.push(format!(
            "ALTER TABLE {table} DROP INDEX {}, {options}",
            ident(&c.name)?
        ));
    } else {
        plan.push(format!(
            "DROP INDEX {} ON {table}{modifiers}",
            ident(&c.name)?
        ));
    }
    Ok(())
}

pub(super) fn rename(c: &RenameIndex, plan: &mut Plan) -> CatalogResult<()> {
    plan.push(format!(
        "ALTER TABLE {} RENAME INDEX {} TO {}",
        object_name(&c.database, &c.table)?,
        ident(&c.old_name)?,
        ident(&c.new_name)?
    ));
    Ok(())
}

pub(super) fn visibility(t: &Target, c: &SetIndexVisibility, plan: &mut Plan) -> CatalogResult<()> {
    if c.name.eq_ignore_ascii_case("PRIMARY") && c.invisible {
        return Err(invalid("the primary key cannot be made invisible"));
    }
    plan.push(format!(
        "ALTER TABLE {} ALTER INDEX {} {}",
        object_name(&c.database, &c.table)?,
        ident(&c.name)?,
        visibility_keyword(t, c.invisible)?
    ));
    Ok(())
}

pub(super) fn replace(t: &Target, c: &ReplaceIndex, plan: &mut Plan) -> CatalogResult<()> {
    if c.index.name.is_none() {
        return Err(invalid("the new index definition needs a name"));
    }
    kind_warnings(&c.index, c.lock, plan);
    super::alter::alter(
        t,
        &AlterTable {
            database: c.database.clone(),
            name: c.table.clone(),
            operations: vec![
                AlterOp::DropIndex {
                    name: c.old_name.clone(),
                },
                AlterOp::AddIndex {
                    index: c.index.clone(),
                },
            ],
            algorithm: c.algorithm,
            lock: c.lock,
            system_versioned: false,
        },
        plan,
    )?;
    // The index is rebuilt, not lost: replacing is not destructive.
    plan.destructive = false;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::model::CatalogResult;

    const CREATE: &str = r#"{"change": "createIndex", "database": "d", "table": "t", "algorithm": "inplace", "lock": "none",
        "index": {"name": "ix", "kind": "unique", "method": "btree",
                  "parts": [{"kind": "column", "name": "a", "length": 8}, {"kind": "column", "name": "b", "descending": true}],
                  "comment": "c", "invisible": true}}"#;

    #[test]
    fn create_per_engine() -> CatalogResult<()> {
        assert_eq!(
            sql(&plan_json(&my80(), CREATE)?),
            "CREATE UNIQUE INDEX `ix` USING BTREE ON `d`.`t` (`a`(8), `b` DESC) COMMENT 'c' INVISIBLE ALGORITHM=INPLACE LOCK=NONE"
        );
        let maria = plan_json(&ma106(), CREATE)?;
        assert_eq!(
            sql(&maria),
            "CREATE UNIQUE INDEX `ix` USING BTREE ON `d`.`t` (`a`(8), `b` DESC) COMMENT 'c' IGNORED ALGORITHM=INPLACE LOCK=NONE"
        );
        assert_eq!(maria.warnings.len(), 1);
        assert!(plan_json(&ma123(), CREATE)?.warnings.is_empty());
        let unnamed = CREATE.replace(r#""name": "ix", "#, "");
        assert!(is_invalid(&plan_json(&my80(), &unnamed)));
        Ok(())
    }

    #[test]
    fn functional_and_fulltext() -> CatalogResult<()> {
        let functional = r#"{"change": "createIndex", "database": "d", "table": "t",
            "index": {"name": "fx", "kind": "index", "parts": [{"kind": "expression", "sql": "lower(email)", "descending": true}]}}"#;
        assert_eq!(
            sql(&plan_json(&my97(), functional)?),
            "CREATE INDEX `fx` ON `d`.`t` ((lower(email)) DESC)"
        );
        assert!(is_unsupported(&plan_json(&ma123(), functional)));
        let fulltext = r#"{"change": "createIndex", "database": "d", "table": "t", "lock": "none",
            "index": {"name": "ft", "kind": "fulltext", "parts": [{"kind": "column", "name": "body"}], "parser": "ngram"}}"#;
        let plan = plan_json(&my80(), fulltext)?;
        assert_eq!(
            sql(&plan),
            "CREATE FULLTEXT INDEX `ft` ON `d`.`t` (`body`) WITH PARSER `ngram` LOCK=NONE"
        );
        assert_eq!(plan.warnings.len(), 2);
        Ok(())
    }

    #[test]
    fn drop_rename_visibility_replace() -> CatalogResult<()> {
        let drop = plan_json(
            &ma106(),
            r#"{"change": "dropIndex", "database": "d", "table": "t", "name": "ix", "algorithm": "nocopy"}"#,
        )?;
        assert_eq!(
            sql(&drop),
            "ALTER TABLE `d`.`t` DROP INDEX `ix`, ALGORITHM=NOCOPY"
        );
        assert!(drop.destructive);
        let mysql_drop = r#"{"change": "dropIndex", "database": "d", "table": "t", "name": "ix", "algorithm": "inplace", "lock": "none"}"#;
        assert_eq!(
            sql(&plan_json(&my80(), mysql_drop)?),
            "DROP INDEX `ix` ON `d`.`t` ALGORITHM=INPLACE LOCK=NONE"
        );
        assert_eq!(
            sql(&plan_json(&ma123(), mysql_drop)?),
            "ALTER TABLE `d`.`t` DROP INDEX `ix`, ALGORITHM=INPLACE, LOCK=NONE"
        );
        assert_eq!(
            sql(&plan_json(
                &my80(),
                r#"{"change": "renameIndex", "database": "d", "table": "t", "oldName": "a", "newName": "b"}"#
            )?),
            "ALTER TABLE `d`.`t` RENAME INDEX `a` TO `b`"
        );
        let vis = r#"{"change": "setIndexVisibility", "database": "d", "table": "t", "name": "ix", "invisible": false}"#;
        assert_eq!(
            sql(&plan_json(&my97(), vis)?),
            "ALTER TABLE `d`.`t` ALTER INDEX `ix` VISIBLE"
        );
        assert_eq!(
            sql(&plan_json(&ma123(), vis)?),
            "ALTER TABLE `d`.`t` ALTER INDEX `ix` NOT IGNORED"
        );
        let replace = plan_json(
            &my80(),
            r#"{"change": "replaceIndex", "database": "d", "table": "t", "oldName": "ix", "lock": "shared",
                "index": {"name": "ix", "kind": "index", "parts": [{"kind": "column", "name": "a"}, {"kind": "column", "name": "b"}]}}"#,
        )?;
        assert_eq!(
            sql(&replace),
            "ALTER TABLE `d`.`t` DROP INDEX `ix`, ADD INDEX `ix` (`a`, `b`), LOCK=SHARED"
        );
        assert!(!replace.destructive);
        Ok(())
    }
}
