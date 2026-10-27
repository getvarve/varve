//! The storage engines a MySQL or MariaDB server offers for new tables
//! (design doc 4.2: "engines (InnoDB, MyISAM, MEMORY, ...)" for tables, and
//! MariaDB's Aria, ColumnStore, Spider, CONNECT and S3 "shown when
//! available"). The table designer fills its engine picker from this list.

use serde::{Deserialize, Serialize};
use varve_core::Engine;
use varve_driver::Session;

use crate::model::{CatalogError, CatalogResult};
use crate::rows::Rows;

use super::util;

/// Lists the engines the server can use (`SUPPORT` is `YES` or `DEFAULT`).
const ENGINES_SQL: &str = "SELECT ENGINE AS `engine`, SUPPORT AS `support`, COMMENT AS `comment`, \
     TRANSACTIONS AS `transactions` FROM information_schema.ENGINES \
     WHERE SUPPORT IN ('YES', 'DEFAULT') ORDER BY ENGINE";

/// Engines the server lists as available that cannot hold a table created
/// with `CREATE TABLE ... ENGINE=`: the Performance Schema's own engine and
/// MariaDB's virtual `SEQUENCE` engine (its tables exist without being
/// created).
const NOT_FOR_TABLES: &[&str] = &["PERFORMANCE_SCHEMA", "SEQUENCE"];

/// One storage engine available on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyEngine {
    /// Name as the server writes it (`InnoDB`, `Aria`, `MEMORY`).
    pub name: String,
    /// The server's default engine for new tables.
    pub default: bool,
    /// The engine supports transactions; `None` when the server does not say.
    pub transactions: Option<bool>,
    /// The server's description of the engine.
    pub comment: Option<String>,
    /// What moving a table with rows to this engine does to the rows or the
    /// table ([`engine_warning`]); `None` for engines that keep rows on disk
    /// and take any table.
    pub warning: Option<String>,
}

/// What changing a table to `engine` does beyond the rewrite: rows that are
/// lost (BLACKHOLE discards every row, MEMORY keeps them only until the
/// server restarts) or tables it cannot hold (ARCHIVE, CSV). `None` for
/// engines that keep rows on disk and take any table (InnoDB, MyISAM,
/// Aria, ...). Matched without case.
#[must_use]
pub fn engine_warning(engine: &str) -> Option<&'static str> {
    match engine.to_ascii_uppercase().as_str() {
        "BLACKHOLE" => Some(
            "BLACKHOLE stores nothing: every row of the table is discarded, and rows written later are thrown away. The rows will be lost",
        ),
        "MEMORY" | "HEAP" => Some(
            "MEMORY keeps rows only in memory: they are lost when the server restarts, and BLOB and TEXT columns are not allowed",
        ),
        "ARCHIVE" => Some(
            "ARCHIVE has no indexes except on an AUTO_INCREMENT column and allows no UPDATE or DELETE; the change fails while other indexes exist",
        ),
        "CSV" => Some(
            "CSV has no indexes and no NULL columns; the change fails while the table has either",
        ),
        "FEDERATED" | "FEDERATEDX" | "SPIDER" | "CONNECT" => Some(
            "this engine keeps no rows of its own but reads them from another server or file: the table's current rows are not kept",
        ),
        _ => None,
    }
}

/// True when moving a table to `engine` loses its rows, now (BLACKHOLE,
/// engines that read rows from elsewhere) or at the next restart (MEMORY).
#[must_use]
pub fn engine_loses_rows(engine: &str) -> bool {
    matches!(
        engine.to_ascii_uppercase().as_str(),
        "BLACKHOLE" | "MEMORY" | "HEAP" | "FEDERATED" | "FEDERATEDX" | "SPIDER" | "CONNECT"
    )
}

/// Reads [`ENGINES_SQL`] rows into engines, leaving out [`NOT_FOR_TABLES`].
fn parse(rows: &Rows) -> CatalogResult<Vec<MyEngine>> {
    let mut out = Vec::new();
    for row in rows.iter() {
        let Some(name) = util::text(&row, "engine")? else {
            continue;
        };
        if NOT_FOR_TABLES.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let support = util::text(&row, "support")?.unwrap_or_default();
        let transactions = util::text(&row, "transactions")?.and_then(|t| {
            if t.eq_ignore_ascii_case("YES") {
                Some(true)
            } else if t.eq_ignore_ascii_case("NO") {
                Some(false)
            } else {
                None
            }
        });
        out.push(MyEngine {
            default: support.eq_ignore_ascii_case("DEFAULT"),
            transactions,
            comment: util::text(&row, "comment")?.filter(|c| !c.is_empty()),
            warning: engine_warning(&name).map(str::to_owned),
            name,
        });
    }
    Ok(out)
}

/// The storage engines the server offers for new tables, sorted by name,
/// with the default engine marked.
///
/// # Errors
///
/// `Unsupported` for a session that is not MySQL or MariaDB; driver errors.
pub async fn engines(session: &dyn Session) -> CatalogResult<Vec<MyEngine>> {
    let engine = session.server_info().engine;
    if !matches!(engine, Engine::Mysql | Engine::Mariadb) {
        return Err(CatalogError::Unsupported {
            message: "storage engines exist only on MySQL and MariaDB".into(),
        });
    }
    let rows = crate::rows::query(session, ENGINES_SQL).await?;
    parse(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_driver::{ExecOutput, StatementResult, Value};

    fn rows(data: &[[Option<&str>; 4]]) -> CatalogResult<Rows> {
        let text = |v: Option<&str>| v.map_or(Value::Null, |s| Value::Text(s.into()));
        Rows::first(ExecOutput {
            results: vec![StatementResult::Rows {
                columns: ["engine", "support", "comment", "transactions"]
                    .iter()
                    .map(|c| varve_driver::Column {
                        name: (*c).into(),
                        type_name: "varchar".into(),
                    })
                    .collect(),
                rows: data
                    .iter()
                    .map(|r| r.iter().copied().map(text).collect())
                    .collect(),
                truncated: false,
            }],
            notices: vec![],
            elapsed_ms: 0,
        })
    }

    #[test]
    fn marks_the_default_and_leaves_out_engines_without_tables() -> CatalogResult<()> {
        let list = parse(&rows(&[
            [
                Some("Aria"),
                Some("YES"),
                Some("Crash-safe tables with MyISAM heritage"),
                Some("NO"),
            ],
            [
                Some("InnoDB"),
                Some("DEFAULT"),
                Some("Supports transactions"),
                Some("YES"),
            ],
            [
                Some("PERFORMANCE_SCHEMA"),
                Some("YES"),
                Some("Performance Schema"),
                Some("NO"),
            ],
            [
                Some("SEQUENCE"),
                Some("YES"),
                Some("Generated tables"),
                Some("YES"),
            ],
            [Some("CSV"), Some("YES"), Some(""), None],
        ])?)?;
        assert_eq!(
            list,
            vec![
                MyEngine {
                    name: "Aria".into(),
                    default: false,
                    transactions: Some(false),
                    comment: Some("Crash-safe tables with MyISAM heritage".into()),
                    warning: None,
                },
                MyEngine {
                    name: "InnoDB".into(),
                    default: true,
                    transactions: Some(true),
                    comment: Some("Supports transactions".into()),
                    warning: None,
                },
                MyEngine {
                    name: "CSV".into(),
                    default: false,
                    transactions: None,
                    comment: None,
                    warning: engine_warning("csv").map(str::to_owned),
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn warns_for_engines_that_lose_rows_or_refuse_tables() {
        assert!(engine_warning("blackhole").is_some_and(|w| w.contains("rows will be lost")));
        assert!(engine_warning("MEMORY").is_some_and(|w| w.contains("server restarts")));
        assert!(engine_warning("Archive").is_some());
        assert!(engine_warning("InnoDB").is_none());
        assert!(engine_warning("Aria").is_none());
        assert!(engine_loses_rows("BlackHole") && engine_loses_rows("memory"));
        assert!(!engine_loses_rows("ARCHIVE") && !engine_loses_rows("InnoDB"));
    }
}
