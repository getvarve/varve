//! `EXPLAIN` plan trees from each engine's output (design doc 4.2: a visual
//! plan tree with cost and time hot spots and buffers).
//!
//! - [`explain_sql`] builds the engine's `EXPLAIN` statement for one SQL
//!   statement and a set of [`ExplainOptions`].
//! - [`analyze_safety`] reads a statement's text and decides whether it
//!   may run under `ANALYZE`, which executes it; [`analyze_safety_checked`]
//!   asks the server, which resolves every name the statement references
//!   by object id, and fails closed (see its module for the threat model).
//! - [`parse_plan`] turns the server's output into the engine-neutral
//!   [`Plan`]: PostgreSQL `FORMAT JSON`, MySQL `FORMAT=JSON` (format
//!   versions 1 and 2), MySQL `FORMAT=TREE` / `EXPLAIN ANALYZE` text, and
//!   MariaDB `EXPLAIN FORMAT=JSON` / `ANALYZE FORMAT=JSON`.
//! - [`explain`] runs it all on a session. Every `ANALYZE` runs inside a
//!   transaction (or, when the session already is in one, a savepoint) that
//!   is rolled back afterwards, also when the statement is a plain `SELECT`
//!   (functions it calls may write).
//!
//! Every node carries its exclusive time (or, without `ANALYZE`, its
//! exclusive estimated cost) and its share of the whole plan, so the UI can
//! colour hot spots, plus warnings for row misestimates of 10x or more,
//! sorts and hashes that spilled to disk, and full scans of large tables.

mod analysis;
mod cancel;
mod json;
mod mysql;
mod postgres;
mod probe;
mod safety;
mod tree;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use varve_core::Engine;
use varve_driver::{Dialect, ExecOutput, ServerInfo, Session, StatementKind, StatementResult};

use crate::model::{CatalogError, CatalogResult};
use crate::rows;

pub use analysis::{LARGE_TABLE_ROWS, MISESTIMATE_FACTOR};
pub use cancel::{ExplainCancel, RESEND_INTERVAL};
pub use probe::{READ_ONLY_CONNECTION_REASON, analyze_safety_checked};
pub use safety::analyze_safety;

/// Output format of `EXPLAIN`, where an engine has several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExplainFormat {
    /// JSON: PostgreSQL `FORMAT JSON`, MySQL and MariaDB `FORMAT=JSON`.
    Json,
    /// MySQL `FORMAT=TREE` (8.0.16+), the format of MySQL `EXPLAIN ANALYZE`.
    Tree,
}

/// What to ask `EXPLAIN` for.
///
/// Options an engine does not have are refused by [`explain_sql`] rather
/// than ignored; [`explain_capabilities`] tells the UI which ones to offer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainOptions {
    /// Run the statement and report actual rows and times. See
    /// [`analyze_safety`]: this executes the statement.
    pub analyze: bool,
    /// PostgreSQL `BUFFERS`: shared, local and temp block counts.
    pub buffers: bool,
    /// PostgreSQL `VERBOSE`: output columns, schema-qualified names.
    pub verbose: bool,
    /// PostgreSQL `SETTINGS` (12+): planner settings that differ from the
    /// defaults.
    pub settings: bool,
    /// PostgreSQL `WAL` (13+, needs `analyze`): WAL records written.
    pub wal: bool,
    /// Output format; `None` takes the engine default: JSON, except MySQL
    /// `EXPLAIN ANALYZE`, which defaults to the tree format.
    pub format: Option<ExplainFormat>,
    /// The user saw and accepted the warnings of
    /// [`AnalyzeSafety::NeedsConfirmation`]; without it [`explain`] refuses
    /// such statements under `ANALYZE`.
    pub confirmed: bool,
}

/// The `EXPLAIN` features a server offers, for the options dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainCapabilities {
    /// Formats for a plain `EXPLAIN`, default first.
    pub formats: Vec<ExplainFormat>,
    /// True when `ANALYZE` is available.
    pub analyze: bool,
    /// Formats for `ANALYZE`, default first.
    pub analyze_formats: Vec<ExplainFormat>,
    /// `BUFFERS` is available.
    pub buffers: bool,
    /// `VERBOSE` is available.
    pub verbose: bool,
    /// `SETTINGS` is available.
    pub settings: bool,
    /// `WAL` is available (only together with `ANALYZE`).
    pub wal: bool,
}

/// Which parser reads an `EXPLAIN` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanFormat {
    /// PostgreSQL `EXPLAIN (FORMAT JSON)`.
    PostgresJson,
    /// MySQL `EXPLAIN FORMAT=JSON`, format version 1 (`query_block`) or 2
    /// (`query_plan`, including `EXPLAIN ANALYZE FORMAT=JSON`).
    MysqlJson,
    /// MySQL `EXPLAIN FORMAT=TREE` and `EXPLAIN ANALYZE` text.
    MysqlTree,
    /// MariaDB `EXPLAIN FORMAT=JSON` and `ANALYZE FORMAT=JSON`.
    MariadbJson,
}

/// Whether a statement may run under `ANALYZE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AnalyzeSafety {
    /// The statement only reads. [`explain`] still runs it inside a
    /// transaction that is rolled back.
    ReadOnly,
    /// The statement changes data; it runs inside a transaction that is
    /// rolled back. The warnings say what a rollback does not undo.
    RolledBack {
        /// What the user should know before running it.
        warnings: Vec<String>,
    },
    /// The statement calls functions whose effects a rollback may not undo
    /// (advisory locks, server files, other connections, user-defined
    /// functions, ...). It runs inside a transaction that is rolled back,
    /// but only once the user confirmed ([`ExplainOptions::confirmed`]).
    NeedsConfirmation {
        /// What the user must confirm, naming each function.
        warnings: Vec<String>,
    },
    /// The statement must not run under `ANALYZE` (DDL, transaction
    /// control, several statements, ...).
    Refused {
        /// Why.
        reason: String,
    },
}

/// What the hot-spot shares of a plan are measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShareBasis {
    /// Exclusive actual time (`ANALYZE` plans).
    Time,
    /// Exclusive estimated cost (plans without actual times).
    Cost,
    /// The plan has neither times nor costs (MariaDB before 11.0 without
    /// `ANALYZE`).
    None,
}

/// A parsed execution plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// The output format it was read from.
    pub format: PlanFormat,
    /// True when the plan has actual values (the statement ran).
    pub analyzed: bool,
    /// The top node.
    pub root: PlanNode,
    /// Planning time (PostgreSQL `Planning Time`, MariaDB
    /// `query_optimization`).
    pub planning_ms: Option<f64>,
    /// Execution time (PostgreSQL `Execution Time`, otherwise the root's
    /// total actual time).
    pub execution_ms: Option<f64>,
    /// Estimated total cost of the root.
    pub total_cost: Option<f64>,
    /// What [`PlanNode::share`] is measured in.
    pub share_basis: ShareBasis,
    /// Buffers used while planning (PostgreSQL `BUFFERS`).
    pub planning_buffers: Option<Buffers>,
    /// Trigger times (PostgreSQL `ANALYZE` of data-changing statements).
    pub triggers: Vec<TriggerTime>,
    /// Non-default planner settings (PostgreSQL `SETTINGS`).
    pub settings: Vec<PlanProperty>,
    /// Plan-level fields without a place above (PostgreSQL `JIT`, MySQL
    /// rewritten query text, ...).
    pub properties: Vec<PlanProperty>,
    /// Warnings about the plan as a whole (what a rollback did not undo,
    /// parts the server could not measure, ...).
    pub warnings: Vec<String>,
    /// Notes and warnings the server sent with the plan.
    pub notices: Vec<String>,
    /// The server's output, as received.
    pub raw: String,
}

/// One operation of a plan.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanNode {
    /// Position in a depth-first walk, starting at 0 for the root.
    pub id: u32,
    /// What the node does: `Hash Join`, `Seq Scan`, `Filter: (...)`,
    /// `Full table scan on orders`, ...
    pub operation: String,
    /// The fields every engine has in some form.
    pub detail: NodeDetail,
    /// Estimated rows per loop.
    pub estimated_rows: Option<f64>,
    /// Actual rows per loop (an average over the loops).
    pub actual_rows: Option<f64>,
    /// How often the node ran.
    pub loops: Option<u64>,
    /// True when `ANALYZE` reported that the node never ran.
    pub never_executed: bool,
    /// Estimated cost.
    pub estimated_cost: Option<Cost>,
    /// Actual time.
    pub actual_time: Option<ActualTime>,
    /// Buffer usage (PostgreSQL `BUFFERS`).
    pub buffers: Option<Buffers>,
    /// Every other field the server reported, by its own name.
    pub properties: Vec<PlanProperty>,
    /// Child operations; the order is the engine's (outer side first).
    pub children: Vec<PlanNode>,
    /// Share of the whole plan's exclusive time or cost (see
    /// [`Plan::share_basis`]), from 0 to 1.
    pub share: Option<f64>,
    /// Problems found in this node.
    pub warnings: Vec<NodeWarning>,
}

/// Engine fields with a common meaning.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDetail {
    /// Table (or other relation) read.
    pub relation: Option<String>,
    /// Schema of the relation (PostgreSQL `VERBOSE`, MySQL format 2).
    pub schema: Option<String>,
    /// Alias of the relation in the query.
    pub alias: Option<String>,
    /// Index used.
    pub index: Option<String>,
    /// Join type: `Inner`, `Left`, `Semi`, `Anti`, ...
    pub join_type: Option<String>,
    /// Algorithm or strategy: aggregate strategy, join algorithm, ...
    pub strategy: Option<String>,
    /// How the node relates to its parent: `Outer`, `Inner`, `InitPlan`,
    /// `SubPlan`, `Member`, `attached subquery`, ...
    pub relationship: Option<String>,
    /// Name of the subplan this node starts (`InitPlan 1`, `CTE big`).
    pub subplan: Option<String>,
    /// Common table expression scanned.
    pub cte: Option<String>,
    /// Function scanned.
    pub function: Option<String>,
    /// Row filter applied by this node.
    pub filter: Option<String>,
    /// Index, join or lookup condition.
    pub condition: Option<String>,
    /// Sort keys.
    pub sort_key: Vec<String>,
    /// Grouping keys.
    pub group_key: Vec<String>,
    /// True when the node runs in parallel workers.
    pub parallel_aware: bool,
    /// Parallel workers planned.
    pub workers_planned: Option<u64>,
    /// Parallel workers started.
    pub workers_launched: Option<u64>,
    /// Output columns (PostgreSQL `VERBOSE`).
    pub output: Vec<String>,
}

/// Estimated cost, in the engine's own units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    /// Cost before the first row.
    pub startup: Option<f64>,
    /// Cost of the node including its children.
    pub total: Option<f64>,
    /// Cost of the node itself, without its children.
    pub exclusive: Option<f64>,
}

/// Actual time, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActualTime {
    /// Time to the first row, per loop.
    pub first_row_ms: Option<f64>,
    /// Time of one loop, as the server reported it.
    pub per_loop_ms: Option<f64>,
    /// Time over all loops including the children (for nodes in parallel
    /// workers, divided by the number of processes, so it is wall time).
    pub total_ms: Option<f64>,
    /// [`ActualTime::total_ms`] without the children.
    pub exclusive_ms: Option<f64>,
}

/// Block counts (PostgreSQL `BUFFERS`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Buffers {
    /// Shared blocks found in the buffer cache.
    pub shared_hit: u64,
    /// Shared blocks read from disk (or the OS cache).
    pub shared_read: u64,
    /// Shared blocks changed.
    pub shared_dirtied: u64,
    /// Shared blocks written out.
    pub shared_written: u64,
    /// Local (temporary table) blocks found in the cache.
    pub local_hit: u64,
    /// Local blocks read.
    pub local_read: u64,
    /// Local blocks changed.
    pub local_dirtied: u64,
    /// Local blocks written.
    pub local_written: u64,
    /// Temp file blocks read (sorts, hashes, materialization).
    pub temp_read: u64,
    /// Temp file blocks written.
    pub temp_written: u64,
}

/// Time spent in one trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerTime {
    /// Trigger name (or constraint name for foreign key triggers).
    pub name: String,
    /// Table it fired on.
    pub relation: Option<String>,
    /// Total time.
    pub time_ms: Option<f64>,
    /// How often it fired.
    pub calls: Option<u64>,
}

/// A field of a node or plan, by the engine's name for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanProperty {
    /// Field name as the server wrote it.
    pub name: String,
    /// Value as text; lists are joined with `, `, nested structures are
    /// compact JSON.
    pub value: String,
}

/// A problem found in a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum NodeWarning {
    /// Estimated and actual rows differ by [`MISESTIMATE_FACTOR`] or more.
    #[serde(rename_all = "camelCase")]
    RowMisestimate {
        /// Estimated rows per loop.
        estimated: f64,
        /// Actual rows per loop.
        actual: f64,
        /// How many times larger the bigger one is.
        factor: f64,
    },
    /// A sort did not fit in memory and used disk.
    #[serde(rename_all = "camelCase")]
    ExternalSort {
        /// Sort method as reported.
        method: String,
        /// Disk space used, in kB.
        space_kb: Option<u64>,
    },
    /// A hash table did not fit in memory and was split into batches on
    /// disk.
    #[serde(rename_all = "camelCase")]
    DiskSpill {
        /// Number of batches.
        batches: Option<u64>,
        /// Disk space used, in kB.
        disk_kb: Option<u64>,
    },
    /// A full scan of a table with [`LARGE_TABLE_ROWS`] rows or more.
    #[serde(rename_all = "camelCase")]
    SeqScanLargeTable {
        /// The table.
        relation: Option<String>,
        /// Rows scanned (or estimated).
        rows: f64,
    },
}

/// The dialect for a SQL engine.
fn dialect(server: &ServerInfo) -> CatalogResult<Dialect> {
    match server.engine {
        Engine::Postgres => Ok(Dialect::Postgres),
        Engine::Mysql | Engine::Mariadb => Ok(Dialect::Mysql),
        Engine::Redis => Err(CatalogError::Unsupported {
            message: "Redis has no EXPLAIN".into(),
        }),
    }
}

/// True when the server is at least `major.minor.patch` (a missing patch
/// counts as 0).
fn at_least_patch(server: &ServerInfo, major: u32, minor: u32, patch: u32) -> bool {
    (server.major, server.minor, server.patch.unwrap_or(0)) >= (major, minor, patch)
}

/// The `EXPLAIN` features `server` offers.
#[must_use]
pub fn explain_capabilities(server: &ServerInfo) -> ExplainCapabilities {
    match server.engine {
        Engine::Postgres => ExplainCapabilities {
            formats: vec![ExplainFormat::Json],
            analyze: true,
            analyze_formats: vec![ExplainFormat::Json],
            buffers: true,
            verbose: true,
            settings: server.at_least(12, 0),
            wal: server.at_least(13, 0),
        },
        Engine::Mysql => {
            let mut formats = vec![ExplainFormat::Json];
            if at_least_patch(server, 8, 0, 16) {
                formats.push(ExplainFormat::Tree);
            }
            let analyze = at_least_patch(server, 8, 0, 18);
            let mut analyze_formats = Vec::new();
            if analyze {
                analyze_formats.push(ExplainFormat::Tree);
            }
            if server.at_least(8, 3) {
                analyze_formats.push(ExplainFormat::Json);
            }
            ExplainCapabilities {
                formats,
                analyze,
                analyze_formats,
                buffers: false,
                verbose: false,
                settings: false,
                wal: false,
            }
        }
        Engine::Mariadb => ExplainCapabilities {
            formats: vec![ExplainFormat::Json],
            analyze: true,
            analyze_formats: vec![ExplainFormat::Json],
            buffers: false,
            verbose: false,
            settings: false,
            wal: false,
        },
        Engine::Redis => ExplainCapabilities {
            formats: Vec::new(),
            analyze: false,
            analyze_formats: Vec::new(),
            buffers: false,
            verbose: false,
            settings: false,
            wal: false,
        },
    }
}

/// The format `options` resolve to on `server`, checked against what the
/// server offers.
fn resolve_format(server: &ServerInfo, options: &ExplainOptions) -> CatalogResult<ExplainFormat> {
    let capabilities = explain_capabilities(server);
    if options.analyze && !capabilities.analyze {
        return Err(CatalogError::Unsupported {
            message: format!(
                "{} {} has no EXPLAIN ANALYZE (MySQL needs 8.0.18 or newer)",
                server.engine.display_name(),
                server.version
            ),
        });
    }
    let offered = if options.analyze {
        &capabilities.analyze_formats
    } else {
        &capabilities.formats
    };
    let format = match options.format {
        Some(format) => format,
        None => *offered.first().ok_or_else(|| CatalogError::Unsupported {
            message: format!("{} has no EXPLAIN", server.engine.display_name()),
        })?,
    };
    if !offered.contains(&format) {
        let name = match format {
            ExplainFormat::Json => "JSON",
            ExplainFormat::Tree => "TREE",
        };
        let what = if options.analyze {
            "EXPLAIN ANALYZE"
        } else {
            "EXPLAIN"
        };
        return Err(CatalogError::Unsupported {
            message: format!(
                "{} {} has no {what} in {name} format",
                server.engine.display_name(),
                server.version
            ),
        });
    }
    let refuse = |option: &str| CatalogError::Unsupported {
        message: format!(
            "{} {} has no EXPLAIN option {option}",
            server.engine.display_name(),
            server.version
        ),
    };
    if options.buffers && !capabilities.buffers {
        return Err(refuse("BUFFERS"));
    }
    if options.verbose && !capabilities.verbose {
        return Err(refuse("VERBOSE"));
    }
    if options.settings && !capabilities.settings {
        return Err(refuse("SETTINGS"));
    }
    if options.wal && !capabilities.wal {
        return Err(refuse("WAL"));
    }
    if options.wal && !options.analyze {
        return Err(CatalogError::Invalid {
            message: "EXPLAIN option WAL needs ANALYZE".into(),
        });
    }
    Ok(format)
}

/// The parser for what [`explain_sql`] returns with these options.
///
/// # Errors
///
/// As [`explain_sql`] for options the server does not offer.
pub fn plan_format(server: &ServerInfo, options: &ExplainOptions) -> CatalogResult<PlanFormat> {
    let format = resolve_format(server, options)?;
    Ok(match (server.engine, format) {
        (Engine::Postgres, _) => PlanFormat::PostgresJson,
        (Engine::Mariadb, _) => PlanFormat::MariadbJson,
        (_, ExplainFormat::Tree) => PlanFormat::MysqlTree,
        (_, ExplainFormat::Json) => PlanFormat::MysqlJson,
    })
}

/// The one statement of `statement`, refusing scripts and any text whose
/// statement boundaries the server could see differently than the splitter:
/// MySQL text whose split depends on `NO_BACKSLASH_ESCAPES`, and MySQL text
/// with an executable comment (`/*! ... */`, MariaDB `/*M! ... */`), whose
/// contents the server runs as code.
fn single_statement(
    server: &ServerInfo,
    statement: &str,
) -> CatalogResult<(String, StatementKind)> {
    let dialect = dialect(server)?;
    if dialect == Dialect::Mysql && has_executable_comment(statement) {
        return Err(CatalogError::Invalid {
            message: "EXPLAIN does not take text with executable comments (/*! ... */ or \
                      /*M! ... */): the server runs their contents as SQL"
                .into(),
        });
    }
    let spans = varve_driver::split_statements_strict(statement, dialect).map_err(|_| {
        CatalogError::Invalid {
            message: "the statement boundaries of this text depend on whether the server \
                      treats backslashes in strings as escapes (NO_BACKSLASH_ESCAPES); \
                      rewrite the string literals without backslashes before quotes"
                .into(),
        }
    })?;
    match spans.as_slice() {
        [span] => Ok((span.text.to_owned(), span.kind)),
        [] => Err(CatalogError::Invalid {
            message: "there is no statement to explain".into(),
        }),
        _ => Err(CatalogError::Invalid {
            message: format!(
                "EXPLAIN takes one statement; the text holds {}",
                spans.len()
            ),
        }),
    }
}

/// True when MySQL text contains `/*!` or `/*M!` anywhere, string literals
/// included: refusing a literal that merely looks like one is the safe side.
fn has_executable_comment(text: &str) -> bool {
    text.contains("/*!") || text.contains("/*M!") || text.contains("/*m!")
}

/// Builds the `EXPLAIN` statement for `statement` on `server`.
///
/// - PostgreSQL: `EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS, VERBOSE,
///   SETTINGS, WAL) ...` with the options asked for (on 18, where
///   `ANALYZE` includes buffers by default, `BUFFERS FALSE` when they are
///   not wanted).
/// - MySQL: `EXPLAIN FORMAT=JSON`, `EXPLAIN FORMAT=TREE`, `EXPLAIN ANALYZE`
///   (tree text), `EXPLAIN ANALYZE FORMAT=JSON` (8.3+, needs
///   `explain_json_format_version = 2`, the default from 9.0).
/// - MariaDB: `EXPLAIN FORMAT=JSON`, `ANALYZE FORMAT=JSON`.
///
/// This is the bare statement: with `analyze` it executes `statement`, so
/// callers that run it themselves must check [`analyze_safety_checked`] and roll
/// back as [`explain`] does.
///
/// # Errors
///
/// [`CatalogError::Invalid`] for an empty text, more than one statement, a
/// statement that already is an `EXPLAIN`, and `WAL` without `ANALYZE`;
/// [`CatalogError::Unsupported`] for options or formats the server does not
/// have, and for Redis.
pub fn explain_sql(
    server: &ServerInfo,
    statement: &str,
    options: &ExplainOptions,
) -> CatalogResult<String> {
    let format = resolve_format(server, options)?;
    let (text, kind) = single_statement(server, statement)?;
    if kind == StatementKind::Explain {
        return Err(CatalogError::Invalid {
            message: "the statement already is an EXPLAIN, SHOW or DESCRIBE".into(),
        });
    }
    let prefix = match server.engine {
        Engine::Postgres => {
            let mut parts = vec!["FORMAT JSON"];
            if options.analyze {
                parts.push("ANALYZE");
            }
            if options.buffers {
                parts.push("BUFFERS");
            } else if options.analyze && server.at_least(18, 0) {
                parts.push("BUFFERS FALSE");
            }
            if options.verbose {
                parts.push("VERBOSE");
            }
            if options.settings {
                parts.push("SETTINGS");
            }
            if options.wal {
                parts.push("WAL");
            }
            format!("EXPLAIN ({})", parts.join(", "))
        }
        Engine::Mariadb => {
            if options.analyze {
                "ANALYZE FORMAT=JSON".to_owned()
            } else {
                "EXPLAIN FORMAT=JSON".to_owned()
            }
        }
        Engine::Mysql => match (options.analyze, format) {
            (false, ExplainFormat::Json) => "EXPLAIN FORMAT=JSON".to_owned(),
            (false, ExplainFormat::Tree) => "EXPLAIN FORMAT=TREE".to_owned(),
            (true, ExplainFormat::Json) => "EXPLAIN ANALYZE FORMAT=JSON".to_owned(),
            // Before 8.0.32 (the `explain_format` variable) EXPLAIN ANALYZE
            // always printed the tree and older releases do not take the
            // FORMAT clause; from then on the format is named so a session
            // default cannot change it.
            (true, ExplainFormat::Tree) if at_least_patch(server, 8, 0, 32) => {
                "EXPLAIN ANALYZE FORMAT=TREE".to_owned()
            }
            (true, ExplainFormat::Tree) => "EXPLAIN ANALYZE".to_owned(),
        },
        Engine::Redis => {
            return Err(CatalogError::Unsupported {
                message: "Redis has no EXPLAIN".into(),
            });
        }
    };
    // A newline, not a space: the statement may start with a comment.
    Ok(format!("{prefix}\n{text}"))
}

/// Reads an `EXPLAIN` output into a [`Plan`].
///
/// # Errors
///
/// [`CatalogError::Unexpected`] when `raw` is not a plan in `format`.
pub fn parse_plan(format: PlanFormat, raw: &str) -> CatalogResult<Plan> {
    match format {
        PlanFormat::PostgresJson => postgres::parse(raw),
        PlanFormat::MysqlJson => mysql::parse_mysql(raw),
        PlanFormat::MariadbJson => mysql::parse_mariadb(raw),
        PlanFormat::MysqlTree => tree::parse(raw),
    }
}

/// The plan text of an `EXPLAIN` result: the first column of every row of
/// the first result set, one row per line (the JSON formats return one
/// row).
fn plan_text(output: &ExecOutput) -> CatalogResult<String> {
    for result in &output.results {
        if let StatementResult::Rows { rows, .. } = result {
            let lines: Vec<String> = rows
                .iter()
                .filter_map(|row| row.first().and_then(varve_driver::Value::to_plain_text))
                .collect();
            return Ok(lines.join("\n"));
        }
    }
    Err(CatalogError::Unexpected {
        message: "EXPLAIN returned no result set".into(),
    })
}

/// Name of the savepoint used when the session already is in a
/// transaction.
const SAVEPOINT: &str = "varve_explain";

/// How an `ANALYZE` run is undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// A transaction started for the run: `ROLLBACK`.
    Transaction,
    /// The session was in a transaction already: `ROLLBACK TO SAVEPOINT`.
    Savepoint,
}

/// Whether a PostgreSQL session is inside a transaction, asked of the
/// server (for sessions that do not track it).
async fn pg_in_transaction(session: &dyn Session) -> CatalogResult<bool> {
    // Inside a transaction, `transaction_timestamp()` is the time of its
    // first statement; for a statement on its own both are equal.
    // Qualified so no object in the user's search_path can stand in.
    let rows = rows::query(
        session,
        "SELECT pg_catalog.transaction_timestamp() OPERATOR(pg_catalog.<>) \
         pg_catalog.statement_timestamp() AS in_transaction",
    )
    .await?;
    Ok(rows
        .single("transaction state")?
        .bool("in_transaction")?
        .unwrap_or(false))
}

/// Opens the scope an `ANALYZE` run is rolled back in, without touching a
/// transaction the session already has open.
async fn begin_scope(session: &dyn Session, server: &ServerInfo) -> CatalogResult<Scope> {
    let in_transaction = match server.engine {
        Engine::Postgres => pg_in_transaction(session).await?,
        Engine::Mariadb => {
            // MariaDB reports open transactions, implicit ones under
            // autocommit off included.
            let rows = rows::query(session, "SELECT @@in_transaction AS in_transaction").await?;
            rows.single("transaction state")?
                .bool("in_transaction")?
                .unwrap_or(false)
        }
        Engine::Mysql => {
            let rows = rows::query(session, "SELECT @@autocommit AS autocommit").await?;
            let autocommit = rows
                .single("autocommit")?
                .bool("autocommit")?
                .unwrap_or(true);
            if autocommit {
                // MySQL has no unprivileged way to read the transaction
                // state, but refuses to change transaction characteristics
                // inside a transaction (error 1568). Setting the isolation
                // level to its own value changes nothing otherwise.
                match rows::run(
                    session,
                    "SET @@transaction_isolation = @@transaction_isolation",
                )
                .await
                {
                    Ok(_) => false,
                    Err(CatalogError::Driver { error })
                        if error
                            .code
                            .as_deref()
                            .and_then(|c| c.split_whitespace().next())
                            == Some("1568") =>
                    {
                        true
                    }
                    Err(error) => return Err(error),
                }
            } else {
                // With autocommit off a transaction may have started
                // implicitly; a savepoint works either way, where START
                // TRANSACTION would commit it.
                true
            }
        }
        Engine::Redis => {
            return Err(CatalogError::Unsupported {
                message: "Redis has no EXPLAIN".into(),
            });
        }
    };
    if in_transaction {
        let name = crate::sql::ident(dialect(server)?, SAVEPOINT)?;
        rows::run(session, &format!("SAVEPOINT {name}")).await?;
        Ok(Scope::Savepoint)
    } else {
        let begin = match server.engine {
            Engine::Postgres => "BEGIN",
            _ => "START TRANSACTION",
        };
        rows::run(session, begin).await?;
        Ok(Scope::Transaction)
    }
}

/// Rolls back what [`begin_scope`] opened.
async fn end_scope(session: &dyn Session, server: &ServerInfo, scope: Scope) -> CatalogResult<()> {
    match scope {
        Scope::Transaction => {
            rows::run(session, "ROLLBACK").await?;
        }
        Scope::Savepoint => {
            let name = crate::sql::ident(dialect(server)?, SAVEPOINT)?;
            rows::run(session, &format!("ROLLBACK TO SAVEPOINT {name}")).await?;
            rows::run(session, &format!("RELEASE SAVEPOINT {name}")).await?;
        }
    }
    Ok(())
}

/// Adds a failed rollback to the error of the statement.
fn with_rollback_failure(error: CatalogError, rollback: &CatalogError) -> CatalogError {
    let note = format!("rolling back the EXPLAIN ANALYZE run also failed: {rollback}");
    match error {
        CatalogError::Driver { mut error } => {
            error.detail = Some(match error.detail.take() {
                Some(detail) => format!("{detail}\n{note}"),
                None => note,
            });
            CatalogError::Driver { error }
        }
        other => CatalogError::Unexpected {
            message: format!("{other}; {note}"),
        },
    }
}

/// Explains `statement` on `session` and parses the plan.
///
/// With `analyze` the statement runs. Without
/// [`ExplainOptions::confirmed`] it runs only when
/// [`analyze_safety_checked`] proves it safe, and then rolled back:
/// PostgreSQL in a read-only subtransaction of a single `DO` statement, MySQL
/// and MariaDB as one call with a `ROLLBACK` after it (in a MySQL
/// transaction in a savepoint); otherwise it is refused with the reasons.
/// When the proven statement then fails (a run-time error, a cancel, a
/// statement timeout, an error while planning, an output that cannot be
/// read back), that error is returned, never a request for confirmation.
/// With confirmation it runs inside a transaction, or a savepoint when the
/// session already is in a transaction, that is rolled back afterwards,
/// also when it fails (statements [`analyze_safety`] refuses never run);
/// on PostgreSQL that is again a `DO` statement, inside the session's
/// transaction with a savepoint around it, so a failing run leaves that
/// transaction usable. If the rollback fails the call fails, even when the
/// plan was read.
///
/// On PostgreSQL every run outside a transaction is one driver call, so
/// dropping the returned future leaves no transaction of Varve's open.
/// Inside the session's transaction (PostgreSQL) and for a confirmed run
/// on MySQL and MariaDB (and a proven one inside the user's transaction)
/// it takes several calls; dropping the future there can leave Varve's
/// transaction or savepoint open. To stop an explain, use
/// [`explain_with_cancel`] and await the call: the session's cancel handle
/// alone reaches only the statement running when it arrives, and the next
/// call of the explain would start anyway.
///
/// # Errors
///
/// As [`explain_sql`]; [`CatalogError::Invalid`] when `ANALYZE` is refused
/// (statements that must never run under it);
/// [`CatalogError::NeedsConfirmation`] when the statement runs only once
/// the user confirmed (call again with [`ExplainOptions::confirmed`]);
/// driver errors; [`CatalogError::Unexpected`] when the output cannot be
/// parsed.
pub async fn explain(
    session: &dyn Session,
    server: &ServerInfo,
    statement: &str,
    options: &ExplainOptions,
) -> CatalogResult<Plan> {
    explain_with_cancel(session, server, statement, options, &ExplainCancel::new()).await
}

/// [`explain`] that `cancel` stops as a whole: once it is cancelled the
/// step that runs is cancelled (again every [`RESEND_INTERVAL`] until it
/// ends) and no later step starts, so one cancel always stops the explain
/// however its calls fall (the session's cancel handle alone reaches only
/// the statement that runs when it arrives). What the explain opened (a
/// transaction, a savepoint) is still rolled back. The call then fails with
/// a [`ErrorKind::Cancelled`](varve_driver::ErrorKind::Cancelled) driver
/// error, or returns the plan when the run had already ended. The resend
/// uses a tokio timer, so the call must run on a tokio runtime with the
/// time driver enabled.
///
/// # Errors
///
/// As [`explain`].
pub async fn explain_with_cancel(
    session: &dyn Session,
    server: &ServerInfo,
    statement: &str,
    options: &ExplainOptions,
    cancel: &ExplainCancel,
) -> CatalogResult<Plan> {
    // `work` runs the steps; `session` itself only the rollbacks.
    let guarded = cancel::Guarded::new(session, cancel);
    let work: &dyn Session = &guarded;
    let sql = explain_sql(server, statement, options)?;
    let format = plan_format(server, options)?;
    if !options.analyze {
        let output = rows::run(work, &sql).await?;
        return finish(format, &output, Vec::new());
    }
    if !options.confirmed {
        return match probe::server_verdict(work, session, server, statement, Some(&sql)).await {
            probe::Verdict::Refused(reason) => Err(CatalogError::Invalid { message: reason }),
            probe::Verdict::Confirm(warnings) => Err(probe::needs_confirmation(&warnings)),
            probe::Verdict::Safe(Some(output)) => finish(format, &output, Vec::new()),
            probe::Verdict::RunFailed(error) => Err(error),
            // A MySQL transaction: the run happens in a savepoint.
            probe::Verdict::Safe(None) => {
                run_in_scope(work, session, server, &sql, format, Vec::new()).await
            }
        };
    }
    let warnings = match analyze_safety(server, statement) {
        AnalyzeSafety::ReadOnly => Vec::new(),
        AnalyzeSafety::RolledBack { warnings } | AnalyzeSafety::NeedsConfirmation { warnings } => {
            warnings
        }
        AnalyzeSafety::Refused { reason } => {
            return Err(CatalogError::Invalid { message: reason });
        }
    };
    run_in_scope(work, session, server, &sql, format, warnings).await
}

/// Runs an `EXPLAIN ANALYZE` statement in a transaction (or, inside the
/// session's transaction, a savepoint) that is rolled back afterwards.
/// PostgreSQL runs it in a `DO` block with a subtransaction that is always
/// rolled back, so outside a transaction a dropped call leaves nothing
/// open; inside the session's transaction a savepoint around the block
/// keeps that transaction usable when the run fails.
///
/// `work` runs the steps, `session` (the same session without a cancel
/// guard) the rollback.
async fn run_in_scope(
    work: &dyn Session,
    session: &dyn Session,
    server: &ServerInfo,
    sql: &str,
    format: PlanFormat,
    warnings: Vec<String>,
) -> CatalogResult<Plan> {
    if server.engine == Engine::Postgres {
        // Outside a transaction the `DO` block is the whole scope. Inside
        // the session's transaction an error in it (also a cancel or a
        // statement timeout) would abort that transaction, so a savepoint
        // around it keeps the transaction usable.
        let savepoint = match work.in_transaction() {
            Some(known) => known,
            None => pg_in_transaction(work).await?,
        };
        if !savepoint {
            let output = probe::run_isolated(work, session, sql, false).await?;
            return finish(format, &output, warnings);
        }
        let name = crate::sql::ident(Dialect::Postgres, SAVEPOINT)?;
        rows::run(work, &format!("SAVEPOINT {name}")).await?;
        let result = probe::run_isolated(work, session, sql, false).await;
        let rollback = end_scope(session, server, Scope::Savepoint).await;
        return match (result, rollback) {
            (Ok(output), Ok(())) => finish(format, &output, warnings),
            (Ok(_), Err(rollback)) => Err(rollback),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(rollback)) => Err(with_rollback_failure(error, &rollback)),
        };
    }
    let scope = begin_scope(work, server).await?;
    let result = rows::run(work, sql).await;
    let rollback = end_scope(session, server, scope).await;
    match (result, rollback) {
        (Ok(output), Ok(())) => finish(format, &output, warnings),
        (Ok(_), Err(rollback)) => Err(rollback),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(rollback)) => Err(with_rollback_failure(error, &rollback)),
    }
}

/// Parses an execution output and adds warnings and server notices.
fn finish(format: PlanFormat, output: &ExecOutput, warnings: Vec<String>) -> CatalogResult<Plan> {
    let raw = plan_text(output)?;
    let mut plan = parse_plan(format, &raw)?;
    let mut all = warnings;
    all.append(&mut plan.warnings);
    plan.warnings = all;
    plan.notices = output
        .notices
        .iter()
        .map(|n| format!("{}: {}", n.severity, n.message))
        .collect();
    Ok(plan)
}
