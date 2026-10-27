//! The server-side half of the `EXPLAIN ANALYZE` safety check, and the
//! read-only run of statements it proves safe.
//!
//! # Threat model
//!
//! `EXPLAIN ANALYZE` executes the statement. The confirmation the UI asks
//! for protects users from side effects they did not expect from "just
//! explaining" a query: a function that writes, takes a lock, sleeps, talks
//! to another server or signals a backend. It is not a security boundary
//! against a hostile database: whoever can create objects can also make a
//! confirmed run, or a plain `EXPLAIN`, do harm, and a race between the
//! check and the run (DDL by another session in between) is not closed.
//! Parsing alone runs code, before any check can answer: a literal is
//! converted with its type's input function while the statement is parsed
//! (for a domain, a domain over a composite, or an array of one, that runs
//! the domain's checks, volatile ones included), and planning folds
//! immutable and stable function calls. A plain `EXPLAIN` by the user does
//! exactly the same, and so does the check's own parse of the statement
//! (read-only and rolled back) below, so a session-level effect of such
//! code (an advisory lock, a notice) can outlive the check's answer.
//!
//! The check fails closed: a statement runs without confirmation only when
//! every check below proves it safe; any doubt, parse surprise or failed
//! lookup asks for confirmation. Nothing is decided by the spelling of a
//! name: what the statement references is resolved by the server itself
//! and compared by object id.
//!
//! PostgreSQL. The main guard is the transaction: a statement that skips
//! confirmation runs in a read-only subtransaction that is always rolled
//! back, inside a single `DO` statement (so nothing is left open when the
//! call is dropped), so it cannot write to any table, take a sequence value
//! or keep a setting, whatever the functions it reaches do. The block
//! hands the plan back in a notice, or (a plan over 512 KiB) through a
//! session setting that one more statement reads and resets, so a plan of
//! any size arrives. Inside the user's own transaction the run cannot be
//! made read-only, so there confirmation is always required. The
//! remaining effects a read-only transaction does not stop (advisory locks,
//! `dblink` and foreign data wrappers, `pg_notify`, backend signals,
//! configuration reloads, file access, sleeps) are excluded by asking the
//! server what the statement references, by object id:
//!
//! - Inside the same `DO` block, in a subtransaction that is rolled back
//!   before anything runs, the statement becomes the body of a temporary
//!   view (`CREATE TEMP VIEW ... AS SELECT NULL FROM (statement) AS s`).
//!   The server's own parser resolves every name the way the run would
//!   (same session, same `search_path`), and records in `pg_depend` the
//!   object id of every user-defined relation, function (also the
//!   function behind a cast, implicit or explicit, and an inlined SQL
//!   function), aggregate, window function, operator, type (also the
//!   targets of input/output coercions and typed literals, column
//!   definition lists, `XMLTABLE` and `JSON_TABLE` columns) and collation
//!   the query tree holds. Built-in objects are pinned and leave no
//!   `pg_depend` row; they are read by name from the server's own deparse
//!   (below), which is exact for them: a built-in function is printed by
//!   its own name, and every `pg_catalog` function of that name must pass.
//! - One prepared catalog query (prepared once per session, run with `jit`
//!   off) walks those dependencies transitively: a view to its `SELECT`
//!   rule and on; a relation to its row level security policies, its
//!   virtual generated columns (PostgreSQL 18), its partitions and
//!   inheritance children (not after `FROM ONLY`), its indexes (their
//!   operator classes, expressions and predicates; a GiST index over a
//!   range also its `subtype_diff`), its partition key's operator classes
//!   and expressions, and its columns' types as types in use; a type to
//!   its base type, element type, range subtype and multirange, composite
//!   attributes, the default operator classes for it (sorting, grouping
//!   and comparing use them) and a range's subtype operator class, and,
//!   when a value of it is made, its domain checks and its own input,
//!   output, receive, send, typmod and canonical functions; an operator
//!   class to the support functions and operators of its family; an
//!   operator to its code, restriction and join estimators; an aggregate
//!   to its support functions; and the `pg_depend` rows of every rule,
//!   domain constraint, column default, policy, operator, function and
//!   type reached (of a relation only its functions, operator classes and
//!   operators: a partition key or index expression). The server's own deparse of each reached view (the check's
//!   view included, `pg_get_viewdef`), domain check
//!   (`pg_get_constraintdef`), policy and generated column (`pg_get_expr`)
//!   supplies what `pg_depend` does not hold: the names before `(` (the
//!   built-ins called), the type after `RETURNING` of the SQL/JSON
//!   functions (resolved with `to_regtype`, and followed like any type;
//!   the parser records it nowhere) and the method after `TABLESAMPLE`
//!   (only `bernoulli` and `system` pass).
//! - Everything reached must be allowed: a relation must be a table,
//!   partitioned table, materialized view, sequence, view (walked) or
//!   composite type (a foreign table asks); a function reached as a call
//!   must be trusted and not a procedure, must not take `cstring` or
//!   `internal` (`domain_in`, `record_in`, `array_in`, `range_in`,
//!   `multirange_in` take a type oid and run that type's domain checks;
//!   an operator class's support functions and an operator's estimators
//!   only have to be trusted, they take `internal`),
//!   must not be on the list of built-ins with effects a rollback does not
//!   undo (sequences, advisory locks, `pg_notify`, signals, files, large
//!   objects, replication, `set_config`, sleeps, the `*_to_xml` and json
//!   record functions that run other SQL or a named type's input), must be
//!   immutable or stable (or one of a few volatile built-ins that only
//!   read server state, such as `random()` and `pg_lock_status()`), must
//!   not (outside the built-ins) return a polymorphic type (`hstore`'s
//!   `populate_record` fills a row of the caller's type through its
//!   columns' input functions), and a `pg_catalog` function written in SQL
//!   without a pre-parsed body (its names resolve when it runs) is only
//!   accepted when no untrusted function or operator in the `search_path`
//!   shares a name with its body; a function reached as a type's own I/O
//!   function must be trusted; an operator must be trusted; and an object
//!   of any other catalog than the ones the walk knows (collations,
//!   schemas, extensions, languages, text search objects, access methods
//!   and operator classes run no user code) asks. Trusted means built in
//!   (an oid below `FirstNormalObjectId`, 16384: created by `initdb`) or a
//!   superuser-owned member of one of a few allowlisted extensions
//!   (`citext`, `hstore`, `pg_trgm`, ...: a role that merely owns an
//!   extension can add its own functions to it, but not superuser-owned
//!   ones).
//! - The statement text adds reasons, never safety: a locking clause
//!   (`FOR UPDATE`, `FOR SHARE`, ...) asks, as the read-only run cannot
//!   take row locks.
//!
//! Creating the view is DDL: an enabled event trigger for
//! `ddl_command_start`, `ddl_command_end` or `sql_drop` whose tags include
//! `CREATE VIEW` or `DROP VIEW` (or filter no tags) would run during the
//! check, so the block asks before creating the view when `pg_event_trigger`
//! holds one. Parsing the view's body runs what parsing runs (above): a
//! range literal of a range type with a user operator class runs that
//! class's compare function while the check parses it, and then asks.
//!
//! A read-only connection (the driver keeps every statement in a read-only
//! transaction) cannot create the temporary view, so there every
//! `EXPLAIN ANALYZE` asks for confirmation; the confirmed run is still
//! read-only. A statement the server cannot parse is reported with its own
//! error, never as a request for confirmation.
//!
//! MySQL and MariaDB have no read-only guarantee for every engine (MyISAM
//! and Aria writes are not rolled back) and no catalog that records what a
//! statement references. What the text names is looked up first, before
//! the server parses anything: a word before `(` that is a stored function
//! of the default database or a loadable function, a schema-qualified
//! call, and any identifier that names a view (unqualified in the default
//! database, or `schema.name`) ask; so do a locking clause and a user
//! variable. Views ask whatever they hold: MariaDB's `EXPLAIN` evaluates a
//! deterministic stored function with constant arguments while planning
//! and prints its value, so a view's body cannot be trusted to show one
//! (and that `EXPLAIN` would already have run it, as the user's own
//! `EXPLAIN` does). Only then the server resolves the names: a plain
//! `EXPLAIN` (`EXPLAIN FORMAT=TRADITIONAL` on MySQL, `EXPLAIN EXTENDED` on
//! MariaDB; it opens the tables but runs no statement) leaves in `SHOW
//! WARNINGS` note 1003 the statement as the optimizer rewrote it: every
//! table schema-qualified and quoted (temporary tables, tables after a
//! leading dot, in parentheses, in derived tables, subqueries, joins and
//! `JSON_TABLE` alike), and every stored function call quoted
//! (`` `f`(...) ``, `` `db`.`f`(...) ``) where a built-in is bare. A
//! statement skips confirmation only when, besides the above, the
//! rewritten statement calls no stored function, every table it names is
//! in `information_schema.TABLES` as a base table of an engine known to
//! keep its data on the server (only `InnoDB`, `MyISAM`, `Aria`, `MEMORY`,
//! `CSV`, `ARCHIVE`, `MRG_MyISAM`, `BLACKHOLE`, `SEQUENCE` and
//! `PERFORMANCE_SCHEMA` count; `FEDERATED` / FederatedX, `CONNECT`,
//! `SPIDER`, `S3` and the like ask) or a system view, and no bare call it
//! shows is a loadable function. A table the catalog does not list is a
//! temporary table (or a MariaDB sequence-engine table), whose engine only
//! `SHOW CREATE TABLE` shows: while any engine not known to be local is
//! enabled on the server, `SHOW CREATE TABLE` is run for every table the
//! rewritten statement names and every identifier of the text (MariaDB
//! evaluates an uncorrelated subquery while planning, so the rewritten
//! statement alone can lose a table), with the server's own case rules
//! (`lower_case_table_names`), reading the engine from the table options
//! after the column list; every one must be of a local engine, and a
//! table the rewritten statement names whose definition cannot be shown
//! asks; `dual` and a temporary table of a local engine never ask. The statement then runs
//! as one call followed by `ROLLBACK` (or, in a MySQL transaction, in a
//! savepoint). A statement the `EXPLAIN` rejects is reported with its own
//! error. Inside a MariaDB transaction nothing is looked up (its routine
//! tables are Aria tables, and reading one breaks every later savepoint),
//! so confirmation is required there. A cancel during the lookup is
//! reported as a cancel.

use std::collections::BTreeSet;

use varve_core::Engine;
use varve_driver::{
    Column, Dialect, ErrorKind, ExecOutput, ServerInfo, Session, StatementResult, Value,
};

use super::safety::{self, PG_SIDE_EFFECT_NAMES, PG_SIDE_EFFECT_PREFIXES, Token};
use super::{AnalyzeSafety, single_statement};
use crate::model::{CatalogError, CatalogResult};
use crate::rows::{self, Rows};
use crate::sql;

/// Volatile `pg_catalog` built-ins without effects outside the statement:
/// they read the clock, the random generator or server state (`timezone`:
/// `AT TIME ZONE` on a `time with time zone` reads the current date).
/// Reviewed against the volatile functions of `pg_catalog` in PostgreSQL
/// 13 to 18. Names compared by text, separated by spaces.
const PG_VOLATILE_OK: &str = "timezone random random_normal clock_timestamp \
    timeofday gen_random_uuid uuidv4 uuidv7 \
    array_sample array_shuffle current_query pg_blocking_pids \
    pg_safe_snapshot_blocking_pids pg_lock_status pg_prepared_xact pg_is_in_recovery \
    pg_is_wal_replay_paused pg_get_wal_replay_pause_state pg_current_wal_lsn pg_current_wal_insert_lsn \
    pg_current_wal_flush_lsn pg_last_wal_receive_lsn pg_last_wal_replay_lsn pg_last_xact_replay_timestamp \
    pg_database_size pg_relation_size pg_table_size pg_indexes_size \
    pg_total_relation_size pg_tablespace_size pg_get_backend_memory_contexts pg_get_shmem_allocations \
    pg_get_shmem_allocations_numa pg_get_wait_events pg_get_loaded_modules pg_get_aios \
    pg_jit_available pg_notification_queue_usage pg_partition_tree pg_partition_ancestors \
    pg_sequence_last_value pg_get_sequence_data pg_stat_have_stats pg_xact_status \
    txid_status pg_xact_commit_timestamp pg_xact_commit_timestamp_origin pg_last_committed_xact \
    pg_collation_actual_version pg_database_collation_actual_version pg_control_checkpoint pg_control_init \
    pg_control_recovery pg_control_system pg_get_multixact_members pg_get_wal_resource_managers \
    pg_get_wal_summarizer_state";

/// Name prefixes of volatile `pg_catalog` built-ins that only read
/// statistics (`pg_stat_get_xact_*` and the like), separated by spaces.
const PG_VOLATILE_OK_PREFIXES: &str = "pg_stat_get_";

/// The message of an `EXPLAIN ANALYZE` refused for want of confirmation.
pub(super) fn needs_confirmation(warnings: &[String]) -> CatalogError {
    CatalogError::NeedsConfirmation {
        message: format!(
            "EXPLAIN ANALYZE needs confirmation for this statement: {}",
            warnings.join(" ")
        ),
        warnings: warnings.to_vec(),
    }
}

/// [`super::analyze_safety`] with the server's answer: whether `statement`
/// may run under `EXPLAIN ANALYZE` without confirmation. See the module
/// documentation for what is checked. Returns
/// [`AnalyzeSafety::ReadOnly`] only for statements proven safe,
/// [`AnalyzeSafety::Refused`] as the text check does, and
/// [`AnalyzeSafety::NeedsConfirmation`] for everything else (also for
/// statements that change data, which [`super::analyze_safety`] calls
/// [`AnalyzeSafety::RolledBack`]).
///
/// PostgreSQL: one round trip (the temporary view and the catalog lookup
/// in one `DO` block). MySQL and MariaDB: two (the `EXPLAIN` and the
/// catalog), plus one per table while an engine not known to be local is
/// enabled.
pub async fn analyze_safety_checked(
    session: &dyn Session,
    server: &ServerInfo,
    statement: &str,
) -> AnalyzeSafety {
    match server_verdict(session, session, server, statement, None).await {
        Verdict::Refused(reason) => AnalyzeSafety::Refused { reason },
        Verdict::Confirm(warnings) => AnalyzeSafety::NeedsConfirmation { warnings },
        Verdict::Safe(_) => AnalyzeSafety::ReadOnly,
        // Without a run only the parse can fail: the statement is not
        // valid here, and its error shows once it runs (confirmed or not,
        // it fails the same way).
        Verdict::RunFailed(CatalogError::Driver { error })
            if error.kind == ErrorKind::Cancelled =>
        {
            AnalyzeSafety::NeedsConfirmation {
                warnings: vec![format!("The check was cancelled ({error}).")],
            }
        }
        Verdict::RunFailed(error) => AnalyzeSafety::NeedsConfirmation {
            warnings: vec![format!("The statement could not be planned ({error}).")],
        },
    }
}

/// What the server check decided.
pub(super) enum Verdict {
    /// Never runs under `ANALYZE`.
    Refused(String),
    /// Runs only once the user confirmed; the warnings say why.
    Confirm(Vec<String>),
    /// Proven safe. With `run`, the output of the `EXPLAIN ANALYZE`
    /// statement, already run read-only.
    Safe(Option<ExecOutput>),
    /// Proven safe, but the run itself failed (a run-time error, a cancel,
    /// a statement timeout, or the statement could not be parsed): the
    /// run's own error.
    RunFailed(CatalogError),
}

/// Why the server check ended without a verdict.
enum Failure {
    /// The check itself failed: the statement needs confirmation.
    Check(CatalogError),
    /// The statement's own parse or run failed: its error is the answer.
    Run(CatalogError),
}

/// A failure of the check itself, except a cancel: a cancelled check is
/// the answer (the explain was cancelled), not a reason to confirm.
fn check_failure(error: CatalogError) -> Failure {
    match &error {
        CatalogError::Driver { error: driver } if driver.kind == ErrorKind::Cancelled => {
            Failure::Run(error)
        }
        _ => Failure::Check(error),
    }
}

/// The server check; with `run` (the `EXPLAIN ANALYZE` statement) a
/// statement proven safe is also run, read-only, in the same transaction
/// as the check. `cleanup` is the session that ends what a failed run left
/// open (the same session, without the cancel guard of `session`).
pub(super) async fn server_verdict(
    session: &dyn Session,
    cleanup: &dyn Session,
    server: &ServerInfo,
    statement: &str,
    run: Option<&str>,
) -> Verdict {
    let text_check = safety::text_check(server, statement);
    match text_check.safety {
        AnalyzeSafety::Refused { reason } => return Verdict::Refused(reason),
        AnalyzeSafety::ReadOnly => {}
        AnalyzeSafety::RolledBack { warnings } => {
            let mut warnings = warnings;
            warnings.insert(
                0,
                "The statement changes data, so it cannot run in a read-only transaction."
                    .to_owned(),
            );
            return Verdict::Confirm(warnings);
        }
        // Functions the text check does not know are the server's to
        // resolve (nothing is planned or folded: the server only parses);
        // any other reason of the text check stands, and then nothing is
        // parsed or looked up.
        AnalyzeSafety::NeedsConfirmation { .. } if text_check.only_unknown => {}
        AnalyzeSafety::NeedsConfirmation { warnings } => return Verdict::Confirm(warnings),
    }
    let Ok((text, _)) = single_statement(server, statement) else {
        return Verdict::Confirm(vec!["The statement could not be read.".to_owned()]);
    };
    let result = match server.engine {
        Engine::Postgres => postgres(session, cleanup, server, &text, run).await,
        Engine::Mysql | Engine::Mariadb => mysql(session, cleanup, server, &text, run).await,
        Engine::Redis => {
            return Verdict::Refused("Redis has no EXPLAIN".to_owned());
        }
    };
    match result {
        Err(Failure::Run(error)) => Verdict::RunFailed(error),
        Ok((reasons, output)) => {
            let mut warnings: Vec<String> = Vec::new();
            for reason in reasons {
                if !warnings.contains(&reason) {
                    warnings.push(reason);
                }
            }
            if warnings.is_empty() {
                Verdict::Safe(output)
            } else {
                Verdict::Confirm(warnings)
            }
        }
        Err(Failure::Check(error)) => Verdict::Confirm(vec![format!(
            "Whether the statement is free of side effects could not be checked ({error})."
        )]),
    }
}

/// The reasons to confirm, and the run's output when it ran.
type Checked = Result<(Vec<String>, Option<ExecOutput>), Failure>;

/// The prefix of the notice an isolated block ends with, followed by `N`
/// and the output, `S` (the output is in [`OUTPUT_SETTING`]), `R` and the
/// reasons to confirm as a JSON array, or `F` and the error of the check.
const OUTPUT_NOTICE: &str = "varve-explain-output:";

/// The session setting a [`pg_block`] leaves an output in that is too
/// large for a notice: the driver keeps at most 1 MiB of notices per call.
/// A custom setting holds text of any size and outlives the subtransaction
/// that is rolled back.
const OUTPUT_SETTING: &str = "varve.explain_output";

/// Outputs up to this many bytes come back in the notice, larger ones
/// through [`OUTPUT_SETTING`] (leaving room for the statement's own
/// notices under the driver's 1 MiB).
const OUTPUT_NOTICE_LIMIT: usize = 512 * 1024;

/// The SQLSTATE a check reports when the connection is read-only (the
/// server's `read_only_sql_transaction`, also used for the driver's own
/// refusal).
const READ_ONLY_SQLSTATE: &str = "25006";

/// The marker a check reports when event triggers would fire for the
/// temporary view (a code of Varve's own, not a server SQLSTATE).
const EVENT_TRIGGER_SQLSTATE: &str = "VA004";

/// The one reason a read-only PostgreSQL connection gives for every
/// `EXPLAIN ANALYZE`: the check's temporary view cannot be created in a
/// read-only transaction, so nothing can be proven there. The run a user
/// confirms for this reason alone is still read-only.
pub const READ_ONLY_CONNECTION_REASON: &str = "The connection is read-only, where the server cannot resolve what the statement \
     references (that takes a temporary view); the confirmed run is still read-only.";

/// A `DO` statement that first runs the catalog lookup `check` (when
/// given: the statement that creates the temporary view, and the `EXECUTE`
/// of the prepared lookup) in a subtransaction that is rolled back, and
/// then, when it found nothing, runs `run` (one statement returning one
/// value, such as an `EXPLAIN (FORMAT JSON)`) in a subtransaction that is
/// always rolled back, read-only when `read_only`. It ends with a notice
/// that carries the lookup's reasons or the run's value ([`run_block`]
/// reads it).
///
/// One statement, so dropping the call half way, an error, a cancel or a
/// statement timeout end it with the statement and nothing is left open.
/// Outside a transaction a failure leaves the session as it was; inside
/// the session's own transaction the caller sets a savepoint around it (an
/// error in the block aborts the transaction).
///
/// An error of the statement itself (it cannot be parsed, a relation does
/// not exist) is raised again from the block, so the caller sees the
/// statement's own error; an error of the lookup, a read-only transaction
/// (the view cannot be created) or a view of that name that already exists
/// is caught and reported in the notice; a cancel or a statement timeout
/// ends the block. The lookup runs with `jit` off (it is a large query
/// that runs in milliseconds; compiling it would take seconds) and the
/// notice goes out at `client_min_messages = notice`, each inside a
/// subtransaction that is rolled back, so the session's own settings stay.
/// An output larger than [`OUTPUT_NOTICE_LIMIT`] is kept in
/// [`OUTPUT_SETTING`] instead, which [`run_block`] reads and clears with
/// one more statement; every block first resets the setting, so an output
/// a dropped call left there goes with the next run.
pub(super) fn pg_block(
    run: Option<&str>,
    check: Option<(&str, &str)>,
    read_only: bool,
) -> CatalogResult<String> {
    let check = match check {
        Some((view, execute)) => format!(
            "\x20 BEGIN\n\
             \x20   IF EXISTS (SELECT FROM pg_catalog.pg_event_trigger e \
             WHERE e.evtenabled OPERATOR(pg_catalog.<>) 'D' \
             AND e.evtevent OPERATOR(pg_catalog.=) ANY (ARRAY['ddl_command_start', 'ddl_command_end', 'sql_drop']::pg_catalog.name[]) \
             AND (e.evttags IS NULL OR e.evttags OPERATOR(pg_catalog.&&) ARRAY['CREATE VIEW', 'DROP VIEW']::pg_catalog.text[])) THEN\n\
             \x20     failed := '{EVENT_TRIGGER_SQLSTATE} event triggers';\n\
             \x20   END IF;\n\
             \x20   BEGIN\n\
             \x20     IF failed IS NULL THEN\n\
             \x20       EXECUTE {view};\n\
             \x20     END IF;\n\
             \x20   EXCEPTION WHEN OTHERS THEN\n\
             \x20     IF SQLSTATE OPERATOR(pg_catalog.=) ANY (ARRAY['{READ_ONLY_SQLSTATE}', '42P07']::pg_catalog.text[]) \
             OR (SQLSTATE OPERATOR(pg_catalog.=) '42501' \
             AND SQLERRM OPERATOR(pg_catalog.~~) '%temporary%') THEN\n\
             \x20       failed := pg_catalog.concat(SQLSTATE, ' ', SQLERRM);\n\
             \x20     ELSE\n\
             \x20       RAISE;\n\
             \x20     END IF;\n\
             \x20   END;\n\
             \x20   IF failed IS NULL THEN\n\
             \x20     BEGIN\n\
             \x20       SET LOCAL search_path = pg_catalog, pg_temp;\n\
             \x20       SET LOCAL jit = off;\n\
             \x20       SET LOCAL plan_cache_mode = force_generic_plan;\n\
             \x20       EXECUTE {execute} INTO reasons;\n\
             \x20     EXCEPTION WHEN OTHERS THEN\n\
             \x20       failed := pg_catalog.concat(SQLSTATE, ' ', SQLERRM);\n\
             \x20     END;\n\
             \x20   END IF;\n\
             \x20   RAISE EXCEPTION USING ERRCODE = 'VA003';\n\
             \x20 EXCEPTION\n\
             \x20   WHEN SQLSTATE 'VA003' THEN NULL;\n\
             \x20 END;\n",
            view = sql::dollar_quote(view)?,
            execute = sql::dollar_quote(execute)?,
        ),
        None => String::new(),
    };
    let (run, ran) = match run {
        Some(sql) => {
            let read_only = if read_only {
                "PERFORM pg_catalog.set_config('transaction_read_only', 'on', true);\n      "
            } else {
                ""
            };
            (
                format!(
                    "\x20 IF reasons IS NULL AND failed IS NULL THEN\n\
                     \x20   BEGIN\n\
                     \x20     {read_only}EXECUTE {} INTO output;\n\
                     \x20     RAISE EXCEPTION USING ERRCODE = 'VA001';\n\
                     \x20   EXCEPTION WHEN SQLSTATE 'VA001' THEN NULL;\n\
                     \x20   END;\n\
                     \x20   IF pg_catalog.octet_length(output) OPERATOR(pg_catalog.>) \
                     {OUTPUT_NOTICE_LIMIT} THEN\n\
                     \x20     EXECUTE pg_catalog.format('SET {OUTPUT_SETTING} = %L', output);\n\
                     \x20     output := NULL;\n\
                     \x20   END IF;\n\
                     \x20 END IF;\n",
                    sql::dollar_quote(sql)?
                ),
                "true",
            )
        }
        None => (String::new(), "false"),
    };
    let body = format!(
        "DECLARE\n\
         \x20 output pg_catalog.text;\n\
         \x20 reasons pg_catalog.text;\n\
         \x20 failed pg_catalog.text;\n\
         BEGIN\n\
         \x20 RESET {OUTPUT_SETTING};\n\
         {check}{run}\
         \x20 BEGIN\n\
         \x20   SET LOCAL client_min_messages = notice;\n\
         \x20   RAISE NOTICE USING MESSAGE = pg_catalog.concat('{OUTPUT_NOTICE}', CASE\n\
         \x20     WHEN failed IS NOT NULL THEN pg_catalog.concat('F', failed)\n\
         \x20     WHEN reasons IS NOT NULL THEN pg_catalog.concat('R', reasons)\n\
         \x20     WHEN NOT {ran} THEN 'R[]'\n\
         \x20     WHEN output IS NULL THEN 'S'\n\
         \x20     ELSE pg_catalog.concat('N', output) END);\n\
         \x20   RAISE EXCEPTION USING ERRCODE = 'VA002';\n\
         \x20 EXCEPTION WHEN SQLSTATE 'VA002' THEN NULL;\n\
         \x20 END;\n\
         END"
    );
    Ok(format!("DO {}", sql::dollar_quote(&body)?))
}

/// Reads and clears [`OUTPUT_SETTING`] in one statement: the subquery
/// (`OFFSET 0` keeps it a subquery) reads the value before the lateral
/// `set_config`, whose argument depends on it, clears it.
const READ_OUTPUT_SETTING: &str = "SELECT o.v AS \"QUERY PLAN\" \
     FROM (SELECT pg_catalog.current_setting('varve.explain_output') AS v OFFSET 0) AS o \
     CROSS JOIN LATERAL pg_catalog.set_config('varve.explain_output', \
                                              pg_catalog.\"left\"(o.v, 0), false) AS c(cleared)";

/// What a [`pg_block`] answered.
pub(super) enum Block {
    /// The run's output, as the single row of a `QUERY PLAN` result set,
    /// with the statement's own notices.
    Output(ExecOutput),
    /// The lookup's reasons to confirm (empty when there was no run and
    /// the lookup found nothing); the statement did not run.
    Reasons(Vec<String>),
    /// The lookup failed with this message (`SQLSTATE message`); the
    /// statement did not run.
    CheckFailed(String),
}

/// Runs a [`pg_block`] on `session`: the lookup `check` (when given) and
/// then, when it found nothing, `run`.
///
/// The lookup runs as the session's prepared statement (`EXECUTE` inside
/// the block), prepared first when the session does not have it yet. A
/// read-only connection's driver refuses the block before sending it (it
/// mentions the read-only mode); as the temporary view could not be
/// created there anyway, that is reported as [`Block::CheckFailed`] with
/// [`READ_ONLY_SQLSTATE`].
///
/// # Errors
///
/// The statement's own error (also a cancel or statement timeout during
/// the lookup); [`CatalogError::Unexpected`] when the block ran but its
/// answer could not be read back (callers report it and never run the
/// statement again).
pub(super) async fn run_block(
    session: &dyn Session,
    cleanup: &dyn Session,
    run: Option<&str>,
    check: Option<&Lookup>,
    read_only: bool,
) -> CatalogResult<Block> {
    let execute = check.map(Lookup::execute);
    let mut prepared = false;
    loop {
        let parts = check
            .zip(execute.as_deref())
            .map(|(lookup, execute)| (lookup.view.as_str(), execute));
        let block = pg_block(run, parts, read_only)?;
        let output = match rows::run(session, &block).await {
            Err(CatalogError::Driver { error })
                if error.kind == ErrorKind::ReadOnly && error.code.is_none() && check.is_some() =>
            {
                return Ok(Block::CheckFailed(format!(
                    "{READ_ONLY_SQLSTATE} {}",
                    error.message
                )));
            }
            other => other?,
        };
        match read_block(cleanup, output).await? {
            // SQLSTATE invalid_sql_statement_name: not prepared yet.
            Block::CheckFailed(message) if message.starts_with("26000 ") && !prepared => {
                if let Some(check) = check {
                    rows::run(session, &check.prepare())
                        .await
                        .map_err(|error| CatalogError::Unexpected {
                            message: format!("the catalog lookup could not be prepared: {error}"),
                        })?;
                }
                prepared = true;
            }
            answer => return Ok(answer),
        }
    }
}

/// Reads the answer of a [`pg_block`] from its output. A large output is
/// read (and cleared) on `cleanup`, the session without the explain's
/// cancel guard: the statement already ran, and a cancel must not leave
/// the output in the session setting.
async fn read_block(cleanup: &dyn Session, output: ExecOutput) -> CatalogResult<Block> {
    let lost = || CatalogError::Unexpected {
        message: "the statement ran, but its EXPLAIN output could not be read back; it is not \
                  run again"
            .into(),
    };
    let mut carried = None;
    let mut notices = Vec::with_capacity(output.notices.len());
    for notice in output.notices {
        match notice.message.strip_prefix(OUTPUT_NOTICE) {
            Some(text) if carried.is_none() => carried = Some(text.to_owned()),
            _ => notices.push(notice),
        }
    }
    let Some(carried) = carried else {
        return Err(lost());
    };
    let (kind, rest) = carried.split_at_checked(1).ok_or_else(lost)?;
    let value = match kind {
        "N" => rest.to_owned(),
        "S" => {
            let rows = rows::query(cleanup, READ_OUTPUT_SETTING).await?;
            let row = rows.single("EXPLAIN output")?;
            row.text("QUERY PLAN")?.ok_or_else(lost)?
        }
        "R" => {
            let reasons: Vec<String> =
                serde_json::from_str(rest).map_err(|e| CatalogError::Unexpected {
                    message: format!("the catalog lookup's answer is not a JSON list: {e}"),
                })?;
            return Ok(Block::Reasons(reasons));
        }
        "F" => return Ok(Block::CheckFailed(rest.to_owned())),
        _ => return Err(lost()),
    };
    Ok(Block::Output(ExecOutput {
        results: vec![StatementResult::Rows {
            columns: vec![Column {
                name: "QUERY PLAN".into(),
                type_name: "text".into(),
            }],
            rows: vec![vec![Value::Text(value)]],
            truncated: false,
        }],
        notices,
        elapsed_ms: output.elapsed_ms,
    }))
}

/// Runs `sql` in a [`pg_block`] without a lookup and returns its output.
///
/// # Errors
///
/// As [`run_block`].
pub(super) async fn run_isolated(
    session: &dyn Session,
    cleanup: &dyn Session,
    sql: &str,
    read_only: bool,
) -> CatalogResult<ExecOutput> {
    match run_block(session, cleanup, Some(sql), None, read_only).await? {
        Block::Output(output) => Ok(output),
        Block::Reasons(_) | Block::CheckFailed(_) => Err(CatalogError::Unexpected {
            message: "the EXPLAIN block answered without an output".into(),
        }),
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL

async fn postgres(
    session: &dyn Session,
    cleanup: &dyn Session,
    server: &ServerInfo,
    text: &str,
    run: Option<&str>,
) -> Checked {
    if session.in_transaction() != Some(false) {
        return Ok((
            vec![
                "The session is inside a transaction, where the run cannot be made read-only."
                    .to_owned(),
            ],
            None,
        ));
    }
    let mut reasons = pg_text_reasons(text);
    let lookup = pg_lookup(server, text).map_err(Failure::Check)?;
    // The lookup and the run are one block: the statement runs only when
    // the text showed no problem and the lookup found nothing. An error
    // the block raises is the statement's own (the block catches the
    // lookup's and reports them in its notice); anything else (the lookup
    // could not be prepared, an answer that could not be read) is the
    // check's failure.
    let failure = |error: CatalogError| match error {
        CatalogError::Driver { .. } => Failure::Run(error),
        other => Failure::Check(other),
    };
    let block = match (run, reasons.is_empty()) {
        (Some(sql), true) => run_block(session, cleanup, Some(sql), Some(&lookup), true)
            .await
            .map_err(failure)?,
        _ => run_block(session, cleanup, None, Some(&lookup), false)
            .await
            .map_err(failure)?,
    };
    let output = match block {
        Block::Output(output) => Some(output),
        Block::Reasons(found) => {
            for reason in found {
                if !reasons.contains(&reason) {
                    reasons.push(reason);
                }
            }
            None
        }
        Block::CheckFailed(message) if message.starts_with(READ_ONLY_SQLSTATE) => {
            reasons.push(READ_ONLY_CONNECTION_REASON.to_owned());
            None
        }
        Block::CheckFailed(message) if message.starts_with(EVENT_TRIGGER_SQLSTATE) => {
            reasons.push(
                "An event trigger on this server would run for the temporary view the check \
                 creates (CREATE VIEW), so what the statement references cannot be resolved \
                 without running that trigger."
                    .to_owned(),
            );
            None
        }
        Block::CheckFailed(message) => {
            return Err(Failure::Check(CatalogError::Unexpected { message }));
        }
    };
    Ok((reasons, output))
}

/// What the statement text alone shows: a locking clause, which the
/// read-only run cannot take (the server's view of the statement decides
/// everything else).
pub(super) fn pg_text_reasons(text: &str) -> Vec<String> {
    let tokens = safety::tokens(text, Dialect::Postgres, false);
    let word = |at: usize, expected: &str| matches!(tokens.get(at), Some(Token::Word { text, quoted: false }) if text == expected);
    let locks = (0..tokens.len()).any(|i| {
        word(i, "FOR")
            && (word(i + 1, "UPDATE")
                || word(i + 1, "SHARE")
                || (word(i + 1, "NO") && word(i + 2, "KEY") && word(i + 3, "UPDATE"))
                || (word(i + 1, "KEY") && word(i + 2, "SHARE")))
    });
    let mut reasons = Vec::new();
    if locks {
        reasons.push(
            "The statement locks rows (FOR UPDATE / FOR SHARE), which a read-only run does not \
             allow."
                .to_owned(),
        );
    }
    reasons
}

/// Whether the object (owner column, oid column, catalog) is trusted:
/// built in, or owned by a superuser and a member of an allowlisted
/// extension.
fn trusted(owner: &str, class: &str, oid: &str) -> String {
    let extensions = PG_TRUSTED_EXTENSIONS
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "({oid}::pg_catalog.oid OPERATOR(pg_catalog.<) {FIRST_NORMAL_OBJECT_ID}::pg_catalog.oid \
         OR ({owner} OPERATOR(pg_catalog.=) ANY (ARRAY(SELECT s.oid FROM supers s)) \
             AND EXISTS (SELECT FROM pg_catalog.pg_depend e \
                         JOIN pg_catalog.pg_extension ex ON ex.oid OPERATOR(pg_catalog.=) e.refobjid \
                         WHERE e.classid OPERATOR(pg_catalog.=) '{class}'::pg_catalog.regclass \
                           AND e.objid OPERATOR(pg_catalog.=) {oid} \
                           AND e.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass \
                           AND e.deptype OPERATOR(pg_catalog.=) 'e' \
                           AND ex.extname OPERATOR(pg_catalog.=) ANY (ARRAY[{extensions}]::pg_catalog.name[]))))"
    )
}

/// `FirstNormalObjectId`: objects with a lower oid were created by
/// `initdb` (the built-ins of `pg_catalog` and `information_schema`).
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;

/// Extensions whose objects count as built in when a superuser owns them
/// (installed by a superuser, or as a trusted extension): their functions
/// compute values without effects outside the statement, and the ones that
/// could turn a value into a row of a user type (`hstore`'s
/// `populate_record` and `#=`, polymorphic results) are refused by the
/// polymorphic-result rule. Anything else (`dblink`, `postgres_fdw`,
/// `tablefunc`, an extension a role made itself) is user code. The names
/// are extension names (`pg_extension.extname`), not SQL names.
const PG_TRUSTED_EXTENSIONS: &[&str] = &[
    "btree_gin",
    "btree_gist",
    "citext",
    "cube",
    "earthdistance",
    "fuzzystrmatch",
    "hstore",
    "intarray",
    "isn",
    "ltree",
    "pg_trgm",
    "pgcrypto",
    "seg",
    "unaccent",
    "uuid-ossp",
];

/// The name of the temporary view the check creates (and rolls back) in
/// the session's temporary schema.
const PG_CHECK_VIEW: &str = "varve_explain_check";

/// The catalog lookup of one statement: the statement that creates the
/// temporary view, and the query that walks what the view references.
pub(super) struct Lookup {
    /// `CREATE TEMP VIEW ... AS SELECT NULL FROM (statement) AS s`.
    view: String,
    /// The lookup query (no parameters).
    body: String,
}

impl Lookup {
    /// `EXECUTE` of the session's prepared lookup.
    fn execute(&self) -> String {
        format!("EXECUTE {PG_CHECK_STATEMENT}")
    }

    /// `PREPARE` of the lookup in the session.
    fn prepare(&self) -> String {
        format!("PREPARE {PG_CHECK_STATEMENT} AS {}", self.body)
    }
}

/// The name the lookup is prepared under in a session (once per session;
/// a newer Varve with another lookup uses another name).
const PG_CHECK_STATEMENT: &str = "varve_explain_check_v9";

/// A regular expression (POSIX classes, no backslashes) for the type name
/// after `RETURNING` in a deparsed view: an identifier or a quoted
/// identifier, optionally schema-qualified. Groups 1 and 2 are the parts.
const RETURNING_PATTERN: &str = "(^|[^[:alnum:]_])RETURNING[[:space:]]+(\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*)\
     ([.](\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*))?";

/// The method after `TABLESAMPLE` in a deparsed view.
const TABLESAMPLE_PATTERN: &str =
    "(^|[^[:alnum:]_])TABLESAMPLE[[:space:]]+(\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*)";

/// The relation name after `ONLY` in a deparsed query (`FROM ONLY t`):
/// group 2 is the name, quoted or bare, optionally schema-qualified.
const ONLY_PATTERN: &str = "(^|[^[:alnum:]_])ONLY[[:space:]]+((\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*)\
     ([.](\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*))?)";

/// The words of a function body: identifiers and operator symbols, each a
/// whole token (the body is split once; the pieces are then compared by
/// equality with the names in the `search_path`, which is cheap however
/// many there are).
const WORD_PATTERN: &str = "[[:alpha:]_][[:alnum:]_$]*|[-+*/<>=~!@#%^&|`?]+";

/// A name before `(` in a deparsed expression: a function call (also
/// `pg_catalog.name(`, keywords such as `CAST(`, which name no function).
/// Group 1 is the name, quoted or bare.
const CALL_PATTERN: &str = "(\"([^\"]|\"\")+\"|[[:alpha:]_][[:alnum:]_$]*)[[:space:]]*[(]";

/// Type names of the SQL grammar: unquoted after `RETURNING` they always
/// mean the built-in type, whatever the `search_path` holds (lower case).
const PG_TYPE_KEYWORDS: &[&str] = &[
    "int",
    "integer",
    "smallint",
    "bigint",
    "real",
    "float",
    "double",
    "decimal",
    "dec",
    "numeric",
    "boolean",
    "bit",
    "varchar",
    "character",
    "char",
    "nchar",
    "national",
    "timestamp",
    "time",
    "interval",
];

/// The catalog lookup of `text`: one query returning the reasons to
/// confirm as a JSON array of strings, or `NULL` when there are none. See
/// the module documentation for the walk and the rules.
fn pg_lookup(server: &ServerInfo, text: &str) -> CatalogResult<Lookup> {
    let view = format!(
        "CREATE TEMP VIEW {PG_CHECK_VIEW} AS SELECT NULL::pg_catalog.text FROM (\n{text}\n) \
         AS varve_explain_sub"
    );
    let literal_list = |items: &[&str]| -> CatalogResult<String> {
        Ok(items
            .iter()
            .map(|item| sql::string(Dialect::Postgres, item))
            .collect::<CatalogResult<Vec<_>>>()?
            .join(", "))
    };
    let deny = literal_list(PG_SIDE_EFFECT_NAMES)?;
    let deny_prefixes = literal_list(
        &PG_SIDE_EFFECT_PREFIXES
            .iter()
            .map(|p| format!("{}%", p.replace('_', "\\_")))
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    )?;
    let volatile_ok = literal_list(&PG_VOLATILE_OK.split_whitespace().collect::<Vec<_>>())?;
    let volatile_ok_prefixes = literal_list(
        &PG_VOLATILE_OK_PREFIXES
            .split_whitespace()
            .map(|p| format!("{}%", p.replace('_', "\\_")))
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    )?;
    let type_keywords = literal_list(PG_TYPE_KEYWORDS)?;
    let returning_pattern = sql::string(Dialect::Postgres, RETURNING_PATTERN)?;
    let tablesample_pattern = sql::string(Dialect::Postgres, TABLESAMPLE_PATTERN)?;
    let call_pattern = sql::string(Dialect::Postgres, CALL_PATTERN)?;
    let only_pattern = sql::string(Dialect::Postgres, ONLY_PATTERN)?;
    let word_pattern = sql::string(Dialect::Postgres, WORD_PATTERN)?;
    let view_name = sql::string(Dialect::Postgres, PG_CHECK_VIEW)?;
    let class = |catalog: &str| format!("'{catalog}'::pg_catalog.regclass::pg_catalog.oid");
    let class_class = class("pg_catalog.pg_class");
    let type_class = class("pg_catalog.pg_type");
    let proc_class = class("pg_catalog.pg_proc");
    let rewrite_class = class("pg_catalog.pg_rewrite");
    let constraint_class = class("pg_catalog.pg_constraint");
    let operator_class = class("pg_catalog.pg_operator");
    let attrdef_class = class("pg_catalog.pg_attrdef");
    let policy_class = class("pg_catalog.pg_policy");
    let opclass_class = class("pg_catalog.pg_opclass");
    // Catalogs whose objects run no user code when reached.
    let harmless_classes = [
        "pg_catalog.pg_collation",
        "pg_catalog.pg_namespace",
        "pg_catalog.pg_extension",
        "pg_catalog.pg_language",
        "pg_catalog.pg_ts_config",
        "pg_catalog.pg_ts_dict",
        "pg_catalog.pg_ts_parser",
        "pg_catalog.pg_ts_template",
        "pg_catalog.pg_am",
        "pg_catalog.pg_opfamily",
        "pg_catalog.pg_conversion",
    ]
    .iter()
    .map(|catalog| class(catalog))
    .collect::<Vec<_>>()
    .join(", ");
    // PostgreSQL 14 added pre-parsed SQL-standard bodies and multiranges.
    let (unparsed, multirange) = if server.at_least(14, 0) {
        (
            "p.prosqlbody IS NULL",
            format!(
                "UNION ALL SELECT {type_class}, rg.rngtypid, x.how \
                 FROM pg_catalog.pg_range rg \
                 WHERE x.classid OPERATOR(pg_catalog.=) {type_class} \
                   AND rg.rngmultitypid OPERATOR(pg_catalog.=) x.objid"
            ),
        )
    } else {
        ("true", String::new())
    };
    // A function in the `internal` language is a symbol of the server
    // itself (a range type's constructors, for one), whoever created it:
    // only a superuser can, and it runs no user code. Not an aggregate,
    // whose `internal` entry stands for its support functions.
    let trusted_proc = format!(
        "({} OR (p.prokind OPERATOR(pg_catalog.<>) 'a' \
                AND p.prolang OPERATOR(pg_catalog.=) (SELECT l.oid FROM pg_catalog.pg_language l \
                WHERE l.lanname OPERATOR(pg_catalog.=) 'internal')))",
        trusted("p.proowner", "pg_catalog.pg_proc", "p.oid")
    );
    let trusted_q = trusted("q.proowner", "pg_catalog.pg_proc", "q.oid");
    let trusted_op = trusted("o.oprowner", "pg_catalog.pg_operator", "o.oid");
    let trusted_qo = trusted("qo.oprowner", "pg_catalog.pg_operator", "qo.oid");
    // Polymorphic result types: a function outside the built-ins that
    // returns one can build a value of the caller's type (`hstore`'s
    // `populate_record` and `#=` fill a row of the table's type through
    // its columns' input functions, domain checks included).
    let polymorphic = "ARRAY['pg_catalog.anyelement', 'pg_catalog.anyarray', \
                       'pg_catalog.anynonarray', 'pg_catalog.anyenum', 'pg_catalog.anyrange', \
                       'pg_catalog.anycompatible', 'pg_catalog.anycompatiblearray', \
                       'pg_catalog.anycompatiblenonarray', \
                       'pg_catalog.anycompatiblerange']::pg_catalog.regtype[]::pg_catalog.oid[]";
    let raw_input = "ARRAY['pg_catalog.cstring', 'pg_catalog.internal']::pg_catalog.regtype[]::pg_catalog.oid[]";
    let sql_body_unsafe = format!(
        "(n.nspname OPERATOR(pg_catalog.=) 'pg_catalog' \
          AND p.prolang OPERATOR(pg_catalog.=) (SELECT l.oid FROM pg_catalog.pg_language l \
                WHERE l.lanname OPERATOR(pg_catalog.=) 'sql') \
          AND {unparsed} \
          AND EXISTS (SELECT FROM pg_catalog.regexp_matches(p.prosrc, {word_pattern}, 'g') w \
                      JOIN untrusted u ON u.name OPERATOR(pg_catalog.=) pg_catalog.lower(w[1])))"
    );
    // Why a function reached as a call is not allowed, or NULL.
    let problem = format!(
        "CASE WHEN NOT {trusted_proc} THEN 'is not a trusted built-in' \
              WHEN p.prokind OPERATOR(pg_catalog.=) 'p' THEN 'is a procedure' \
              WHEN p.oid OPERATOR(pg_catalog.>=) {FIRST_NORMAL_OBJECT_ID}::pg_catalog.oid \
                   AND p.prorettype OPERATOR(pg_catalog.=) ANY ({polymorphic}) \
                THEN 'returns a polymorphic type, so it can build values of a type the statement does not name' \
              WHEN p.proargtypes::pg_catalog.oid[] OPERATOR(pg_catalog.&&) {raw_input} \
                THEN 'takes a type''s raw input, which runs that type''s domain checks' \
              WHEN p.proname::pg_catalog.text OPERATOR(pg_catalog.=) ANY (ARRAY[{deny}]::pg_catalog.text[]) \
                   OR p.proname::pg_catalog.text OPERATOR(pg_catalog.~~) ANY (ARRAY[{deny_prefixes}]::pg_catalog.text[]) \
                   OR p.prosrc OPERATOR(pg_catalog.=) ANY (ARRAY[{deny}]::pg_catalog.text[]) \
                   OR p.prosrc OPERATOR(pg_catalog.~~) ANY (ARRAY[{deny_prefixes}]::pg_catalog.text[]) \
                THEN 'has effects a rollback does not undo (locks, sequences, files, other sessions or server state)' \
              WHEN NOT (p.provolatile OPERATOR(pg_catalog.=) ANY (ARRAY['i', 's']::pg_catalog.\"char\"[]) \
                        OR (n.nspname OPERATOR(pg_catalog.=) 'pg_catalog' \
                            AND (p.proname::pg_catalog.text OPERATOR(pg_catalog.=) ANY (ARRAY[{volatile_ok}]::pg_catalog.text[]) \
                                 OR p.proname::pg_catalog.text OPERATOR(pg_catalog.~~) ANY (ARRAY[{volatile_ok_prefixes}]::pg_catalog.text[])))) \
                THEN 'is volatile and not known to be free of side effects' \
              WHEN {sql_body_unsafe} \
                THEN 'is written in SQL and resolves its names when it runs, where an untrusted function of the same name is in the search_path' \
              ELSE NULL END"
    );
    let identity = "(pg_catalog.pg_identify_object(x.classid, x.objid, 0)).identity";
    // The server's own deparse of the object `x` reached: a view's
    // definition (the check's view holds the statement), a domain check, a
    // policy's expressions, a generated column's expression. Built-in
    // functions (pinned objects) leave no `pg_depend` row, so the names
    // they are called by are read here; and so are the type after
    // `RETURNING` and the method after `TABLESAMPLE`, which the parser
    // records nowhere.
    let deparse = format!(
        "CASE WHEN x.classid OPERATOR(pg_catalog.=) {rewrite_class} \
              THEN (SELECT pg_catalog.pg_get_viewdef(r.ev_class) FROM pg_catalog.pg_rewrite r \
                    WHERE r.oid OPERATOR(pg_catalog.=) x.objid) \
              WHEN x.classid OPERATOR(pg_catalog.=) {constraint_class} \
              THEN (SELECT pg_catalog.pg_get_constraintdef(c.oid) FROM pg_catalog.pg_constraint c \
                    WHERE c.oid OPERATOR(pg_catalog.=) x.objid) \
              WHEN x.classid OPERATOR(pg_catalog.=) {policy_class} \
              THEN (SELECT pg_catalog.concat_ws(' ', pg_catalog.pg_get_expr(pol.polqual, pol.polrelid), \
                                                pg_catalog.pg_get_expr(pol.polwithcheck, pol.polrelid)) \
                    FROM pg_catalog.pg_policy pol WHERE pol.oid OPERATOR(pg_catalog.=) x.objid) \
              WHEN x.classid OPERATOR(pg_catalog.=) {attrdef_class} \
              THEN (SELECT pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) FROM pg_catalog.pg_attrdef ad \
                    WHERE ad.oid OPERATOR(pg_catalog.=) x.objid) \
              ELSE NULL END"
    );
    // The type names after `RETURNING` in a deparsed text `src.text`,
    // resolved as the parser would (`to_regtype`; NULL when the name does
    // not resolve): a quoted identifier as written, a bare one folded to
    // lower case, and a bare type keyword of the grammar left out (it
    // always means the built-in type).
    let returning_types = format!(
        "SELECT nm.name, pg_catalog.to_regtype(nm.name)::pg_catalog.oid AS oid \
         FROM pg_catalog.regexp_matches(src.text, {returning_pattern}, 'gi') m \
         CROSS JOIN LATERAL (SELECT \
             CASE WHEN m[2] OPERATOR(pg_catalog.~~) '\"%' \
                  THEN pg_catalog.replace(pg_catalog.substr(m[2], 2, pg_catalog.length(m[2]) OPERATOR(pg_catalog.-) 2), '\"\"', '\"') \
                  ELSE pg_catalog.lower(m[2]) END AS first, \
             CASE WHEN m[5] OPERATOR(pg_catalog.~~) '\"%' \
                  THEN pg_catalog.replace(pg_catalog.substr(m[5], 2, pg_catalog.length(m[5]) OPERATOR(pg_catalog.-) 2), '\"\"', '\"') \
                  ELSE pg_catalog.lower(m[5]) END AS second, \
             m[5] IS NULL AND NOT (m[2] OPERATOR(pg_catalog.~~) '\"%') \
               AND pg_catalog.lower(m[2]) OPERATOR(pg_catalog.=) ANY (ARRAY[{type_keywords}]::pg_catalog.text[]) AS keyword) w \
         CROSS JOIN LATERAL (SELECT CASE WHEN w.second IS NULL THEN pg_catalog.quote_ident(w.first) \
             ELSE pg_catalog.concat(pg_catalog.quote_ident(w.first), '.', pg_catalog.quote_ident(w.second)) END AS name) nm \
         WHERE NOT w.keyword"
    );
    let body = format!(
        "WITH RECURSIVE \
         path AS (SELECT n.oid, n.nspname FROM pg_catalog.pg_namespace n \
                  WHERE n.nspname OPERATOR(pg_catalog.=) ANY (pg_catalog.current_schemas(true))), \
         supers AS (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolsuper), \
         untrusted AS MATERIALIZED (\
           SELECT q.proname::pg_catalog.text AS name FROM pg_catalog.pg_proc q \
           JOIN path qn ON qn.oid OPERATOR(pg_catalog.=) q.pronamespace \
           WHERE qn.nspname OPERATOR(pg_catalog.<>) 'pg_catalog' AND NOT {trusted_q} \
           UNION SELECT qo.oprname::pg_catalog.text FROM pg_catalog.pg_operator qo \
           JOIN path qn ON qn.oid OPERATOR(pg_catalog.=) qo.oprnamespace \
           WHERE qn.nspname OPERATOR(pg_catalog.<>) 'pg_catalog' AND NOT {trusted_qo}), \
         checkview AS (\
           SELECT c.oid FROM pg_catalog.pg_class c \
           WHERE c.relname OPERATOR(pg_catalog.=) {view_name}::pg_catalog.name \
             AND c.relnamespace OPERATOR(pg_catalog.=) pg_catalog.pg_my_temp_schema()), \
         onlyrels AS (\
           SELECT r.oid FROM (SELECT pg_catalog.pg_get_viewdef(v.oid) AS def FROM checkview v) d \
           CROSS JOIN LATERAL pg_catalog.regexp_matches(d.def, {only_pattern}, 'g') m \
           CROSS JOIN LATERAL (SELECT pg_catalog.to_regclass(m[2])::pg_catalog.oid AS oid) r \
           WHERE r.oid IS NOT NULL \
             AND NOT d.def OPERATOR(pg_catalog.~) pg_catalog.concat('(FROM|JOIN|,)[[:space:]]*', \
                   pg_catalog.regexp_replace(m[2], '([^[:alnum:]_])', '\\\\\\1', 'g'), '([^[:alnum:]_.]|$)')), \
         dep AS (\
           SELECT {rewrite_class} AS classid, r.oid AS objid, 'walk'::pg_catalog.text AS how \
           FROM pg_catalog.pg_rewrite r JOIN checkview v ON v.oid OPERATOR(pg_catalog.=) r.ev_class \
           UNION \
           SELECT y.classid, y.objid, y.how FROM dep x CROSS JOIN LATERAL (\
             SELECT d.refclassid AS classid, d.refobjid AS objid, \
               CASE WHEN d.refclassid OPERATOR(pg_catalog.<>) {proc_class} THEN 'walk' \
                    WHEN x.classid OPERATOR(pg_catalog.=) {type_class} THEN 'io' \
                    ELSE 'call' END::pg_catalog.text AS how \
               FROM pg_catalog.pg_depend d \
               WHERE d.classid OPERATOR(pg_catalog.=) x.classid \
                 AND d.objid OPERATOR(pg_catalog.=) x.objid \
                 AND (x.classid OPERATOR(pg_catalog.=) ANY (ARRAY[{rewrite_class}, {constraint_class}, \
                        {attrdef_class}, {policy_class}, {operator_class}, {proc_class}]::pg_catalog.oid[]) \
                      OR (x.classid OPERATOR(pg_catalog.=) {type_class} AND x.how OPERATOR(pg_catalog.=) 'walk' \
                          AND d.refclassid OPERATOR(pg_catalog.<>) {class_class} \
                          AND NOT EXISTS (SELECT FROM pg_catalog.pg_range rg \
                                          WHERE rg.rngtypid OPERATOR(pg_catalog.=) x.objid \
                                            AND d.refclassid OPERATOR(pg_catalog.=) {proc_class} \
                                            AND rg.rngsubdiff::pg_catalog.oid OPERATOR(pg_catalog.=) d.refobjid)) \
                      OR (x.classid OPERATOR(pg_catalog.=) {class_class} \
                          AND d.refclassid OPERATOR(pg_catalog.=) ANY (ARRAY[{proc_class}, {opclass_class}, \
                                {operator_class}]::pg_catalog.oid[]))) \
             UNION ALL SELECT {rewrite_class}, r.oid, 'walk' FROM pg_catalog.pg_rewrite r \
               JOIN pg_catalog.pg_class rc ON rc.oid OPERATOR(pg_catalog.=) r.ev_class \
               WHERE x.classid OPERATOR(pg_catalog.=) {class_class} \
                 AND r.ev_class OPERATOR(pg_catalog.=) x.objid \
                 AND rc.relkind OPERATOR(pg_catalog.=) 'v' \
                 AND r.ev_type OPERATOR(pg_catalog.=) '1' \
             UNION ALL SELECT {policy_class}, pol.oid, 'walk' FROM pg_catalog.pg_policy pol \
               JOIN pg_catalog.pg_class pc ON pc.oid OPERATOR(pg_catalog.=) pol.polrelid \
               WHERE x.classid OPERATOR(pg_catalog.=) {class_class} \
                 AND pol.polrelid OPERATOR(pg_catalog.=) x.objid AND pc.relrowsecurity \
             UNION ALL SELECT {attrdef_class}, ad.oid, 'walk' FROM pg_catalog.pg_attrdef ad \
               JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) ad.adrelid \
                                             AND a.attnum OPERATOR(pg_catalog.=) ad.adnum \
               WHERE x.classid OPERATOR(pg_catalog.=) {class_class} \
                 AND ad.adrelid OPERATOR(pg_catalog.=) x.objid \
                 AND a.attgenerated OPERATOR(pg_catalog.=) 'v' \
             UNION ALL SELECT {class_class}, i.inhrelid, 'walk' FROM pg_catalog.pg_inherits i \
               WHERE x.classid OPERATOR(pg_catalog.=) {class_class} \
                 AND i.inhparent OPERATOR(pg_catalog.=) x.objid \
                 AND NOT EXISTS (SELECT FROM onlyrels o WHERE o.oid OPERATOR(pg_catalog.=) x.objid) \
             UNION ALL SELECT {class_class}, i.indexrelid, 'index' FROM pg_catalog.pg_index i \
               WHERE x.classid OPERATOR(pg_catalog.=) {class_class} \
                 AND x.how OPERATOR(pg_catalog.<>) 'index' \
                 AND i.indrelid OPERATOR(pg_catalog.=) x.objid \
             UNION ALL SELECT {type_class}, a.atttypid, 'use' FROM pg_catalog.pg_attribute a \
               WHERE x.classid OPERATOR(pg_catalog.=) {class_class} \
                 AND x.how OPERATOR(pg_catalog.<>) 'index' \
                 AND a.attrelid OPERATOR(pg_catalog.=) x.objid \
                 AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped \
             UNION ALL SELECT {opclass_class}, c.oid, 'walk' FROM pg_catalog.pg_partitioned_table pt \
               CROSS JOIN LATERAL pg_catalog.unnest(pt.partclass::pg_catalog.oid[]) AS c(oid) \
               WHERE x.classid OPERATOR(pg_catalog.=) {class_class} \
                 AND pt.partrelid OPERATOR(pg_catalog.=) x.objid \
             UNION ALL SELECT {opclass_class}, c.oid, 'walk' FROM pg_catalog.pg_index i \
               CROSS JOIN LATERAL pg_catalog.unnest(i.indclass::pg_catalog.oid[]) AS c(oid) \
               WHERE x.classid OPERATOR(pg_catalog.=) {class_class} \
                 AND x.how OPERATOR(pg_catalog.=) 'index' \
                 AND i.indexrelid OPERATOR(pg_catalog.=) x.objid \
             UNION ALL SELECT {proc_class}, rg.rngsubdiff::pg_catalog.oid, 'call' FROM pg_catalog.pg_index i \
               JOIN pg_catalog.pg_class ic ON ic.oid OPERATOR(pg_catalog.=) i.indexrelid \
               JOIN pg_catalog.pg_am am ON am.oid OPERATOR(pg_catalog.=) ic.relam \
               JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) i.indexrelid \
               JOIN pg_catalog.pg_range rg ON rg.rngtypid OPERATOR(pg_catalog.=) a.atttypid \
               WHERE x.classid OPERATOR(pg_catalog.=) {class_class} \
                 AND x.how OPERATOR(pg_catalog.=) 'index' \
                 AND i.indexrelid OPERATOR(pg_catalog.=) x.objid \
                 AND am.amname OPERATOR(pg_catalog.=) 'gist' \
                 AND rg.rngsubdiff OPERATOR(pg_catalog.<>) 0::pg_catalog.oid \
             UNION ALL SELECT {proc_class}, ap.amproc::pg_catalog.oid, 'support' FROM pg_catalog.pg_opclass oc \
               JOIN pg_catalog.pg_amproc ap ON ap.amprocfamily OPERATOR(pg_catalog.=) oc.opcfamily \
               WHERE x.classid OPERATOR(pg_catalog.=) {opclass_class} \
                 AND oc.oid OPERATOR(pg_catalog.=) x.objid \
             UNION ALL SELECT {operator_class}, ao.amopopr, 'walk' FROM pg_catalog.pg_opclass oc \
               JOIN pg_catalog.pg_amop ao ON ao.amopfamily OPERATOR(pg_catalog.=) oc.opcfamily \
               WHERE x.classid OPERATOR(pg_catalog.=) {opclass_class} \
                 AND oc.oid OPERATOR(pg_catalog.=) x.objid \
             UNION ALL SELECT {opclass_class}, oc.oid, 'walk' FROM pg_catalog.pg_opclass oc \
               WHERE x.classid OPERATOR(pg_catalog.=) {type_class} \
                 AND oc.opcintype OPERATOR(pg_catalog.=) x.objid AND oc.opcdefault \
             UNION ALL SELECT v.classid, v.objid, v.how FROM pg_catalog.pg_type t \
               CROSS JOIN LATERAL (VALUES \
                 ({type_class}, t.typbasetype, x.how), ({type_class}, t.typelem, x.how)) AS v(classid, objid, how) \
               WHERE x.classid OPERATOR(pg_catalog.=) {type_class} \
                 AND t.oid OPERATOR(pg_catalog.=) x.objid \
                 AND v.objid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid \
             UNION ALL SELECT v.classid, v.objid, v.how FROM pg_catalog.pg_type t \
               CROSS JOIN LATERAL (VALUES \
                 ({proc_class}, t.typinput::pg_catalog.oid, 'io'), \
                 ({proc_class}, t.typoutput::pg_catalog.oid, 'io'), \
                 ({proc_class}, t.typreceive::pg_catalog.oid, 'io'), \
                 ({proc_class}, t.typsend::pg_catalog.oid, 'io'), \
                 ({proc_class}, t.typmodin::pg_catalog.oid, 'io'), \
                 ({proc_class}, t.typmodout::pg_catalog.oid, 'io')) AS v(classid, objid, how) \
               WHERE x.classid OPERATOR(pg_catalog.=) {type_class} \
                 AND x.how OPERATOR(pg_catalog.=) 'walk' \
                 AND t.oid OPERATOR(pg_catalog.=) x.objid \
                 AND v.objid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid \
             UNION ALL SELECT v.classid, v.objid, v.how FROM pg_catalog.pg_range rg \
               CROSS JOIN LATERAL (VALUES \
                 ({type_class}, rg.rngsubtype, x.how), \
                 ({opclass_class}, rg.rngsubopc, 'walk'), \
                 ({proc_class}, rg.rngcanonical::pg_catalog.oid, \
                  CASE WHEN x.how OPERATOR(pg_catalog.=) 'walk' THEN 'io' ELSE 'skip' END)) AS v(classid, objid, how) \
               WHERE x.classid OPERATOR(pg_catalog.=) {type_class} \
                 AND rg.rngtypid OPERATOR(pg_catalog.=) x.objid \
                 AND v.objid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid \
                 AND v.how OPERATOR(pg_catalog.<>) 'skip' \
             {multirange} \
             UNION ALL SELECT {constraint_class}, c.oid, 'walk' FROM pg_catalog.pg_constraint c \
               WHERE x.classid OPERATOR(pg_catalog.=) {type_class} \
                 AND x.how OPERATOR(pg_catalog.=) 'walk' \
                 AND c.contypid OPERATOR(pg_catalog.=) x.objid \
             UNION ALL SELECT {type_class}, a.atttypid, x.how FROM pg_catalog.pg_type t \
               JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) t.typrelid \
               WHERE x.classid OPERATOR(pg_catalog.=) {type_class} \
                 AND t.oid OPERATOR(pg_catalog.=) x.objid \
                 AND t.typrelid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid \
                 AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped \
             UNION ALL SELECT {proc_class}, f.oid, f.how FROM pg_catalog.pg_operator o \
               CROSS JOIN LATERAL (VALUES (o.oprcode::pg_catalog.oid, 'call'), \
                 (o.oprrest::pg_catalog.oid, 'io'), (o.oprjoin::pg_catalog.oid, 'io')) AS f(oid, how) \
               WHERE x.classid OPERATOR(pg_catalog.=) {operator_class} \
                 AND o.oid OPERATOR(pg_catalog.=) x.objid \
                 AND f.oid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid \
             UNION ALL SELECT {proc_class}, f.oid, 'call' FROM pg_catalog.pg_aggregate ag \
               CROSS JOIN LATERAL pg_catalog.unnest(ARRAY[ag.aggtransfn, ag.aggfinalfn, \
                 ag.aggcombinefn, ag.aggserialfn, ag.aggdeserialfn, ag.aggmtransfn, \
                 ag.aggminvtransfn, ag.aggmfinalfn]::pg_catalog.oid[]) AS f(oid) \
               WHERE x.classid OPERATOR(pg_catalog.=) {proc_class} \
                 AND ag.aggfnoid OPERATOR(pg_catalog.=) x.objid \
                 AND f.oid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid \
             UNION ALL SELECT {type_class}, rt.oid, 'walk' FROM (SELECT {deparse} AS text) src \
               CROSS JOIN LATERAL ({returning_types}) AS rt \
               WHERE src.text IS NOT NULL AND rt.oid IS NOT NULL) y), \
         texts AS (\
           SELECT x.classid, x.objid, t.text FROM dep x CROSS JOIN LATERAL (SELECT {deparse} AS text) t \
           WHERE t.text IS NOT NULL), \
         bcalls AS (\
           SELECT DISTINCT CASE WHEN m[1] OPERATOR(pg_catalog.~~) '\"%' \
                    THEN pg_catalog.replace(pg_catalog.substr(m[1], 2, pg_catalog.length(m[1]) OPERATOR(pg_catalog.-) 2), '\"\"', '\"') \
                    ELSE pg_catalog.lower(m[1]) END AS name \
           FROM texts t CROSS JOIN LATERAL pg_catalog.regexp_matches(t.text, {call_pattern}, 'g') m), \
         procs AS (\
           SELECT x.how, p.oid, p.proname, n.nspname, {trusted_proc} AS trusted, {problem} AS problem \
           FROM dep x JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) x.objid \
           JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace \
           WHERE x.classid OPERATOR(pg_catalog.=) {proc_class}), \
         reasons AS (\
           SELECT pg_catalog.concat('The statement reads the foreign table ', {identity}, \
                    ', which reaches another server.') AS reason \
           FROM dep x JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) x.objid \
           WHERE x.classid OPERATOR(pg_catalog.=) {class_class} AND c.relkind OPERATOR(pg_catalog.=) 'f' \
           UNION ALL \
           SELECT pg_catalog.concat('The statement reads ', {identity}, \
                    ', which is not a table (relation kind ', c.relkind, ').') \
           FROM dep x JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) x.objid \
           WHERE x.classid OPERATOR(pg_catalog.=) {class_class} \
             AND NOT c.relkind OPERATOR(pg_catalog.=) ANY (ARRAY['r', 'p', 'm', 'S', 'v', 'c', 't', 'f', 'i', 'I']::pg_catalog.\"char\"[]) \
           UNION ALL \
           SELECT pg_catalog.concat('The statement reaches ', pr.nspname, '.', pr.proname, '(), which ', pr.problem, '.') \
           FROM procs pr WHERE pr.how OPERATOR(pg_catalog.=) 'call' AND pr.problem IS NOT NULL \
           UNION ALL \
           SELECT pg_catalog.concat('The statement reaches ', n.nspname, '.', p.proname, '(), which ', b.problem, '.') \
           FROM bcalls c \
           JOIN pg_catalog.pg_proc p ON p.proname OPERATOR(pg_catalog.=) c.name::pg_catalog.name \
           JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace \
           CROSS JOIN LATERAL (SELECT {problem} AS problem) b \
           WHERE n.nspname OPERATOR(pg_catalog.=) 'pg_catalog' AND b.problem IS NOT NULL \
             AND p.prorettype OPERATOR(pg_catalog.<>) 'pg_catalog.tsm_handler'::pg_catalog.regtype \
           UNION ALL \
           SELECT pg_catalog.concat('The statement reaches ', pr.nspname, '.', pr.proname, \
                    '() through a type''s own functions; it is not a trusted built-in.') \
           FROM procs pr WHERE pr.how OPERATOR(pg_catalog.=) 'io' AND NOT pr.trusted \
           UNION ALL \
           SELECT pg_catalog.concat('The statement reaches ', pr.nspname, '.', pr.proname, \
                    '() through an operator class (an index, a partition key, a range or a type''s \
                    default ordering); it is not a trusted built-in.') \
           FROM procs pr WHERE pr.how OPERATOR(pg_catalog.=) 'support' AND NOT pr.trusted \
           UNION ALL \
           SELECT pg_catalog.concat('The operator ', n.nspname, '.', o.oprname, ' is not a trusted built-in.') \
           FROM dep x JOIN pg_catalog.pg_operator o ON o.oid OPERATOR(pg_catalog.=) x.objid \
           JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) o.oprnamespace \
           WHERE x.classid OPERATOR(pg_catalog.=) {operator_class} AND NOT {trusted_op} \
           UNION ALL \
           SELECT pg_catalog.concat('The statement reaches ', \
                    (pg_catalog.pg_identify_object(x.classid, x.objid, 0)).type, ' ', {identity}, \
                    ', which cannot be checked.') \
           FROM dep x WHERE NOT x.classid OPERATOR(pg_catalog.=) ANY (ARRAY[{class_class}, {type_class}, \
                 {proc_class}, {rewrite_class}, {constraint_class}, {operator_class}, {attrdef_class}, \
                 {policy_class}, {opclass_class}, {harmless_classes}]::pg_catalog.oid[]) \
           UNION ALL \
           SELECT pg_catalog.concat('The type ', rt.name, ' after RETURNING could not be resolved.') \
           FROM texts src CROSS JOIN LATERAL ({returning_types}) AS rt WHERE rt.oid IS NULL \
           UNION ALL \
           SELECT pg_catalog.concat('The sampling method ', m[2], ' is not a built-in.') \
           FROM texts t CROSS JOIN LATERAL pg_catalog.regexp_matches(t.text, {tablesample_pattern}, 'gi') m \
           WHERE NOT pg_catalog.lower(m[2]) OPERATOR(pg_catalog.=) ANY (ARRAY['bernoulli', 'system']::pg_catalog.text[])) \
         SELECT pg_catalog.json_agg(DISTINCT r.reason)::pg_catalog.text FROM reasons r"
    );
    Ok(Lookup { view, body })
}

// ---------------------------------------------------------------------------
// MySQL and MariaDB

async fn mysql(
    session: &dyn Session,
    cleanup: &dyn Session,
    server: &ServerInfo,
    text: &str,
    run: Option<&str>,
) -> Checked {
    let mariadb = server.engine == Engine::Mariadb;
    let in_transaction = session.in_transaction();
    if mariadb && in_transaction != Some(false) {
        return Ok((
            vec![
                "Whether the statement calls stored functions cannot be checked inside a \
                 MariaDB transaction (the check would read non-transactional tables)."
                    .to_owned(),
            ],
            None,
        ));
    }
    let mut reasons = Vec::new();
    let mut calls = BTreeSet::new();
    // Per reading of double quotes (index 0: strings, the default SQL
    // mode; 1: identifiers, ANSI_QUOTES): every word, and every
    // `schema.name`. Which reading counts for the tables is decided once
    // the session's sql_mode is known; views are looked up under both.
    let mut identifiers: [BTreeSet<String>; 2] = Default::default();
    let mut qualified: [BTreeSet<(String, String)>; 2] = Default::default();
    // The text under both readings of backslashes and of double quotes:
    // whatever is code in either counts (the server resolves the names
    // below; the text only adds reasons).
    for (backslash_escapes, ansi_quotes) in
        [(true, false), (false, false), (true, true), (false, true)]
    {
        let reading = usize::from(ansi_quotes);
        // Words as written: whether the server folds their case is its
        // own setting, decided in the lookups.
        let tokens =
            safety::tokens_cased(text, Dialect::Mysql, backslash_escapes, ansi_quotes, false);
        let word = |i: usize, expected: &str| {
            matches!(tokens.get(i), Some(Token::Word { text, quoted: false })
                if text.eq_ignore_ascii_case(expected))
        };
        let locks = (0..tokens.len()).any(|i| {
            (word(i, "FOR") && (word(i + 1, "UPDATE") || word(i + 1, "SHARE")))
                || (word(i, "LOCK") && word(i + 1, "IN") && word(i + 2, "SHARE"))
        });
        if locks {
            let message = "The statement locks rows (FOR UPDATE, FOR SHARE, LOCK IN SHARE MODE), \
                           which other sessions wait for."
                .to_owned();
            if !reasons.contains(&message) {
                reasons.push(message);
            }
        }
        for (i, token) in tokens.iter().enumerate() {
            let Token::Word { text: name, .. } = token else {
                continue;
            };
            // Names keep their case: whether the server folds them is its
            // `lower_case_table_names` setting; calls are case-insensitive.
            let name = name.clone();
            let after_dot = i > 0 && tokens.get(i - 1) == Some(&Token::Dot);
            let mut schema = None;
            if after_dot
                && let Some(Token::Word { text: found, .. }) =
                    i.checked_sub(2).and_then(|p| tokens.get(p))
            {
                schema = Some(found.clone());
                qualified[reading].insert((found.clone(), name.clone()));
            }
            if tokens.get(i + 1) == Some(&Token::Open) {
                if after_dot {
                    let message = format!(
                        "A schema-qualified call ({}{name}()) reaches a stored function.",
                        schema.map(|s| format!("{s}.")).unwrap_or_default()
                    );
                    if !reasons.contains(&message) {
                        reasons.push(message);
                    }
                } else {
                    calls.insert(name.to_lowercase());
                }
            }
            identifiers[reading].insert(name);
        }
    }
    let every_identifier: BTreeSet<String> = identifiers.iter().flatten().cloned().collect();
    let every_qualified: BTreeSet<(String, String)> = qualified.iter().flatten().cloned().collect();
    // What the text names decides first, before the server parses
    // anything: a stored function the text calls, a view the text names
    // (MariaDB's EXPLAIN evaluates a deterministic stored function with
    // constant arguments and prints its value, so a view's body cannot be
    // trusted to show one) and a loadable function.
    let (found, ansi_quotes, folds_names) =
        mysql_prefilter(session, server, &calls, &every_identifier, &every_qualified)
            .await
            .map_err(check_failure)?;
    reasons.extend(found);
    if !reasons.is_empty() {
        return Ok((reasons, None));
    }
    let reading = usize::from(ansi_quotes);
    let note = mysql_explain(session, cleanup, server, text, in_transaction).await?;
    let mut names = RewrittenNames::default();
    match note {
        Some(note) => {
            // The server prints string literals with backslash escapes
            // whatever the session's mode; both readings are taken, so a
            // misread cannot hide a name.
            for backslash_escapes in [true, false] {
                let tokens =
                    safety::tokens_quoting(&note, Dialect::Mysql, backslash_escapes, ansi_quotes);
                rewritten_names(&tokens, &mut names);
            }
        }
        None => reasons.push(
            "The server did not report how it reads the statement (EXPLAIN left no rewritten \
             statement in SHOW WARNINGS)."
                .to_owned(),
        ),
    }
    for (schema, name) in &names.functions {
        reasons.push(match schema {
            Some(schema) => format!(
                "{schema}.{name}() is a stored function the statement reaches (also through \
                 views); it can change things a rollback does not undo."
            ),
            None => format!(
                "{name}() is a stored function of the default database the statement reaches \
                 (also through views); it can change things a rollback does not undo."
            ),
        });
    }
    // Built-ins the rewritten statement calls that the text did not name
    // (through a view) may be loadable functions too.
    let rewritten_calls: BTreeSet<String> = names.calls.difference(&calls).cloned().collect();
    reasons.extend(
        mysql_tables(
            session,
            &names.tables,
            &identifiers[reading],
            &qualified[reading],
            &rewritten_calls,
            server,
            folds_names,
        )
        .await
        .map_err(check_failure)?,
    );
    let output = match run {
        // One call, so dropping it half way leaves no transaction of
        // Varve's open: the statement (a read proven to reach no stored
        // code) and a ROLLBACK that ends the implicit transaction it opened
        // when autocommit is off.
        Some(sql) if reasons.is_empty() && in_transaction == Some(false) => {
            match rows::run(session, &format!("{sql}\n;\nROLLBACK")).await {
                Ok(output) => Some(output),
                // With autocommit off the failed statement still opened a
                // transaction, which the skipped ROLLBACK would have ended.
                Err(error) => {
                    return Err(Failure::Run(match rows::run(cleanup, "ROLLBACK").await {
                        Ok(_) => error,
                        Err(rollback) => super::with_rollback_failure(error, &rollback),
                    }));
                }
            }
        }
        // A MySQL transaction: the caller's savepoint scope.
        _ => None,
    };
    Ok((reasons, output))
}

/// `SELECT name` of a list of names, or `NULL` for none.
fn mysql_list(items: &mut dyn Iterator<Item = &String>) -> CatalogResult<String> {
    let items = items
        .map(|item| sql::string(Dialect::Mysql, item))
        .collect::<CatalogResult<Vec<_>>>()?;
    Ok(if items.is_empty() {
        "NULL".to_owned()
    } else {
        items.join(", ")
    })
}

/// The loadable functions named like a call.
fn mysql_loadable(call_list: &str) -> String {
    format!("SELECT LOWER(name) AS name FROM mysql.func WHERE LOWER(name) IN ({call_list})")
}

/// Runs `script`, whose last statement (index `loadable_at`) reads
/// `mysql.func`, which may be unreadable for the user (loadable functions
/// are installed by an administrator, so then the rest still counts).
async fn mysql_script(
    session: &dyn Session,
    script: &str,
    loadable_at: u32,
) -> CatalogResult<Vec<Rows>> {
    match rows::run(session, script).await {
        Ok(output) => Rows::all(output),
        // A query error, not a cancel, a time limit or a lost connection.
        Err(CatalogError::Driver { error })
            if error.kind == ErrorKind::Query
                && error
                    .partial
                    .as_ref()
                    .is_some_and(|p| p.failed_statement == Some(loadable_at)) =>
        {
            let results = error.partial.map(|p| p.results).unwrap_or_default();
            Rows::all(ExecOutput {
                results,
                notices: Vec::new(),
                elapsed_ms: 0,
            })
        }
        Err(error) => Err(error),
    }
}

/// One script, before anything is parsed: the session's `sql_mode`
/// (whether double quotes quote identifiers, `ANSI_QUOTES`, is returned)
/// and `lower_case_table_names` (whether names are folded, returned too),
/// the stored functions of the default database named like a call in the
/// text, the views the text names (unqualified in the default database,
/// or `schema.name`), and the loadable functions named like a call (last,
/// see [`mysql_script`]). On MariaDB each query is followed by `COMMIT`
/// (with `autocommit` off the query opened a transaction holding a read of
/// an Aria table, which would break a later savepoint).
async fn mysql_prefilter(
    session: &dyn Session,
    server: &ServerInfo,
    calls: &BTreeSet<String>,
    identifiers: &BTreeSet<String>,
    qualified: &BTreeSet<(String, String)>,
) -> CatalogResult<(Vec<String>, bool, bool)> {
    let mariadb = server.engine == Engine::Mariadb;
    let call_list = mysql_list(&mut calls.iter())?;
    let lowered: Vec<String> = identifiers.iter().map(|n| n.to_lowercase()).collect();
    let identifier_list = mysql_list(&mut lowered.iter())?;
    let pairs: Vec<String> = qualified
        .iter()
        .map(|(schema, name)| format!("{schema}.{name}").to_lowercase())
        .collect();
    let qualified_list = mysql_list(&mut pairs.iter())?;
    let routines = format!(
        "SELECT LOWER(ROUTINE_NAME) AS name FROM information_schema.ROUTINES \
         WHERE ROUTINE_SCHEMA = DATABASE() AND ROUTINE_TYPE = 'FUNCTION' \
         AND LOWER(ROUTINE_NAME) IN ({call_list})"
    );
    let views = format!(
        "SELECT LOWER(CONCAT(TABLE_SCHEMA, '.', TABLE_NAME)) AS name \
         FROM information_schema.TABLES WHERE TABLE_TYPE = 'VIEW' \
         AND ((TABLE_SCHEMA = DATABASE() AND LOWER(TABLE_NAME) IN ({identifier_list})) \
              OR LOWER(CONCAT(TABLE_SCHEMA, '.', TABLE_NAME)) IN ({qualified_list}))"
    );
    let loadable = mysql_loadable(&call_list);
    let mode = "SELECT @@SESSION.sql_mode AS mode, @@lower_case_table_names AS lctn";
    let (script, loadable_at) = if mariadb {
        (
            format!("{mode};\n{routines};\nCOMMIT;\n{views};\nCOMMIT;\n{loadable};\nCOMMIT"),
            5,
        )
    } else {
        (format!("{mode};\n{routines};\n{views};\n{loadable}"), 3)
    };
    let sets = mysql_script(session, &script, loadable_at).await?;
    if sets.len() < 3 {
        return Err(CatalogError::Unexpected {
            message: "the stored function and view lookup returned too few results".into(),
        });
    }
    // Whether the server folds table and database names to lower case
    // (`lower_case_table_names` 1 or 2); with 0 they are case sensitive.
    let folds_names = sets[0]
        .iter()
        .next()
        .map(|row| row.int("lctn"))
        .transpose()?
        .flatten()
        .is_none_or(|lctn| lctn != 0);
    // An unknown mode takes the reading that finds more names.
    let ansi_quotes = sets[0]
        .iter()
        .next()
        .map(|row| row.text("mode"))
        .transpose()?
        .flatten()
        .is_none_or(|mode| {
            mode.split(',')
                .any(|m| m.eq_ignore_ascii_case("ANSI_QUOTES") || m.eq_ignore_ascii_case("ANSI"))
        });
    let mut reasons = Vec::new();
    for row in sets[1].iter() {
        let name = row.req_text("name")?;
        reasons.push(format!(
            "{name}() is a stored function of the default database; the statement may call \
             it, and it can change things a rollback does not undo."
        ));
    }
    for row in sets[2].iter() {
        let name = row.req_text("name")?;
        reasons.push(format!(
            "{name} is a view, which can call stored functions."
        ));
    }
    for row in sets.get(3).map(Rows::iter).into_iter().flatten() {
        let name = row.req_text("name")?;
        reasons.push(format!(
            "{name}() is a loadable function on this server; the statement may call it, \
             and it can change things a rollback does not undo."
        ));
    }
    Ok((reasons, ansi_quotes, folds_names))
}

/// One script: the plain `EXPLAIN` and its `SHOW WARNINGS` (note 1003 is
/// the rewritten statement, returned when the server left one); outside a
/// transaction a `ROLLBACK` ends what the `EXPLAIN` opened with
/// `autocommit` off. An error of the `EXPLAIN` is the statement's own (it
/// fails the same way under `ANALYZE`).
async fn mysql_explain(
    session: &dyn Session,
    cleanup: &dyn Session,
    server: &ServerInfo,
    text: &str,
    in_transaction: Option<bool>,
) -> Result<Option<String>, Failure> {
    let explain = if server.engine == Engine::Mariadb {
        "EXPLAIN EXTENDED"
    } else {
        "EXPLAIN FORMAT=TRADITIONAL"
    };
    let end = if in_transaction == Some(false) {
        ";\nROLLBACK"
    } else {
        ""
    };
    let script = format!("{explain}\n{text}\n;\nSHOW WARNINGS{end}");
    let check = |error| Failure::Check(error);
    let sets = match rows::run(session, &script).await {
        Ok(output) => Rows::all(output).map_err(check)?,
        // (The first statement failed: with nothing produced before it,
        // the error carries no partial output.)
        Err(CatalogError::Driver { error })
            if error.kind == ErrorKind::Query
                && error
                    .partial
                    .as_ref()
                    .is_none_or(|p| p.failed_statement.is_none_or(|at| at == 0)) =>
        {
            // The EXPLAIN failed, so the ROLLBACK (when any) did not run:
            // end what the EXPLAIN opened with autocommit off.
            let error = CatalogError::Driver { error };
            if in_transaction == Some(false)
                && let Err(rollback) = rows::run(cleanup, "ROLLBACK").await
            {
                return Err(Failure::Run(super::with_rollback_failure(error, &rollback)));
            }
            return Err(Failure::Run(error));
        }
        Err(error) => return Err(check_failure(error)),
    };
    if sets.len() < 2 {
        return Err(Failure::Check(CatalogError::Unexpected {
            message: "the EXPLAIN of the statement returned too few results".into(),
        }));
    }
    // The EXPLAIN's own result set (sets[0]) is of no use; the note follows.
    let mut note = None;
    for row in sets[1].iter() {
        let code = row.int("Code").map_err(check)?;
        let level = row.text("Level").map_err(check)?;
        if code == Some(1003) && level.is_some_and(|l| l.eq_ignore_ascii_case("note")) {
            note = row.text("Message").map_err(check)?;
            break;
        }
    }
    Ok(note)
}

/// What the rewritten statement names.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RewrittenNames {
    /// Every `schema.table` as printed (the server prints names in their
    /// stored case): the server qualifies every table it reads, so a
    /// quoted pair is a table, and the first two parts of a three-part
    /// column are, unless the middle part is an alias of the statement
    /// (`` `db`.`x`.`col` `` of `` `db`.`t` `x` ``).
    pub(super) tables: BTreeSet<(String, String)>,
    /// Stored functions: a quoted name before `(`, with its schema when
    /// qualified (a built-in is printed bare; a CTE with a column list,
    /// `` `c`(`n`) AS (``, is not a call).
    pub(super) functions: BTreeSet<(Option<String>, String)>,
    /// Bare words before `(`: built-ins and loadable functions.
    pub(super) calls: BTreeSet<String>,
}

/// Collects the tables, stored functions and bare calls of a rewritten
/// statement's tokens.
pub(super) fn rewritten_names(tokens: &[Token], names: &mut RewrittenNames) {
    let quoted = |at: usize| match tokens.get(at) {
        Some(Token::Word { text, quoted: true }) => Some(text.clone()),
        _ => None,
    };
    let word = |at: usize, expected: &str| {
        matches!(tokens.get(at), Some(Token::Word { text, quoted: false })
            if text.eq_ignore_ascii_case(expected))
    };
    // The chain of dotted quoted names starting at `at`, and where it ends.
    let chain = |at: usize| {
        let mut parts = Vec::new();
        let mut end = at;
        while let Some(part) = quoted(end) {
            parts.push(part);
            if tokens.get(end + 1) == Some(&Token::Dot) && quoted(end + 2).is_some() {
                end += 2;
            } else {
                break;
            }
        }
        (parts, end)
    };
    // Aliases of the statement: a quoted word right after a table
    // reference (`` `db`.`t` `x` ``) or after a parenthesized derived
    // table or subquery (`` (...) `d` ``). A column of an aliased table is
    // printed `` `db`.`x`.`col` ``, which names no table `db.x`.
    let mut aliases = BTreeSet::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens.get(i) == Some(&Token::Close)
            && let Some(alias) = quoted(i + 1)
        {
            aliases.insert(alias);
        }
        if quoted(i).is_some() {
            let (parts, end) = chain(i);
            if parts.len() == 2
                && let Some(alias) = quoted(end + 1)
            {
                aliases.insert(alias);
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Word {
                text,
                quoted: false,
            } => {
                if tokens.get(i + 1) == Some(&Token::Open) {
                    names.calls.insert(text.to_lowercase());
                }
                i += 1;
            }
            Token::Word { quoted: true, .. } => {
                let (chain, at) = chain(i);
                match chain.as_slice() {
                    // `db`.`t` or `db`.`t`.`col`: a table unless the
                    // middle part is an alias (`x`.`col`, `db`.`x`.`col`).
                    [first, second] if !aliases.contains(first) => {
                        names.tables.insert((first.clone(), second.clone()));
                    }
                    [first, second, _, ..] if !aliases.contains(second) => {
                        names.tables.insert((first.clone(), second.clone()));
                    }
                    _ => {}
                }
                // `WITH c(n) AS (...)`: a CTE with a column list is printed
                // as a quoted name before `(`, like a call, but its list
                // is followed by `AS (`; a call's alias is `AS name`.
                let cte = tokens.get(at + 1) == Some(&Token::Open)
                    && safety::matching_close(tokens, at + 1).is_some_and(|close| {
                        word(close + 1, "AS") && tokens.get(close + 2) == Some(&Token::Open)
                    });
                if tokens.get(at + 1) == Some(&Token::Open) && !cte {
                    match chain.as_slice() {
                        [name] => {
                            names.functions.insert((None, name.clone()));
                        }
                        [schema, name, ..] => {
                            names.functions.insert((Some(schema.clone()), name.clone()));
                        }
                        [] => {}
                    }
                }
                i = at + 1;
            }
            _ => i += 1,
        }
    }
}

/// Storage engines whose tables keep their data on the server itself
/// (compared in upper case). A table of any other engine (MySQL
/// `FEDERATED`, MariaDB FederatedX, `CONNECT`, `SPIDER`, `S3`, `OQGRAPH`,
/// ColumnStore, MyRocks, a plugin) needs confirmation: reading it can talk
/// to another server or run the engine's own code. `MRG_MYISAM` reads local
/// MyISAM tables, `SEQUENCE` and `PERFORMANCE_SCHEMA` serve values the
/// server computes, `BLACKHOLE` returns nothing.
const MYSQL_LOCAL_ENGINES: &[&str] = &[
    "INNODB",
    "MYISAM",
    "ARIA",
    "MEMORY",
    "HEAP",
    "CSV",
    "ARCHIVE",
    "MRG_MYISAM",
    "BLACKHOLE",
    "SEQUENCE",
    "PERFORMANCE_SCHEMA",
];

/// One script: the tables the rewritten statement names (their type and
/// engine), the databases among their schemas, the enabled engines that
/// are not local ([`MYSQL_LOCAL_ENGINES`]) and the loadable functions
/// among the calls only the rewritten statement shows (last, see
/// [`mysql_script`]); while a non-local engine is enabled, `SHOW CREATE
/// TABLE` of every named table follows (a temporary table is listed
/// nowhere else). On MariaDB each query is followed by `COMMIT` (see
/// [`mysql_prefilter`]).
async fn mysql_tables(
    session: &dyn Session,
    tables: &BTreeSet<(String, String)>,
    identifiers: &BTreeSet<String>,
    qualified: &BTreeSet<(String, String)>,
    calls: &BTreeSet<String>,
    server: &ServerInfo,
    folds_names: bool,
) -> CatalogResult<Vec<String>> {
    let mariadb = server.engine == Engine::Mariadb;
    // Names as the server compares them: folded to lower case when it
    // folds them (`lower_case_table_names` 1 or 2), else exactly as
    // written (0), where `tfu` and `TFU` are two tables.
    let fold = |name: &str| {
        if folds_names {
            name.to_lowercase()
        } else {
            name.to_owned()
        }
    };
    // Candidate tables: what the rewritten statement names (those the
    // server reads for sure), every `schema.name` of the text, and every
    // identifier of the text as a table of the default database (MariaDB's
    // EXPLAIN evaluates an uncorrelated subquery while planning and prints
    // its value, so the rewritten statement alone can lose a table).
    let printed: BTreeSet<(String, String)> = tables
        .iter()
        .map(|(schema, name)| (fold(schema), fold(name)))
        .collect();
    let pairs: BTreeSet<(String, String)> = printed
        .iter()
        .cloned()
        .chain(
            qualified
                .iter()
                .map(|(schema, name)| (fold(schema), fold(name))),
        )
        .collect();
    let identifiers: BTreeSet<String> = identifiers.iter().map(|n| fold(n)).collect();
    let pair_names: Vec<String> = pairs
        .iter()
        .map(|(schema, name)| format!("{schema}.{name}"))
        .collect();
    let qualified_list = mysql_list(&mut pair_names.iter())?;
    let identifier_list = mysql_list(&mut identifiers.iter())?;
    let schemas: BTreeSet<String> = pairs.iter().map(|(schema, _)| schema.clone()).collect();
    let schema_list = mysql_list(&mut schemas.iter())?;
    // The comparison the server's own case rules call for.
    let cmp = |expression: &str| {
        if folds_names {
            format!("LOWER({expression})")
        } else {
            format!("BINARY {expression}")
        }
    };
    let full_name = cmp("CONCAT(TABLE_SCHEMA, '.', TABLE_NAME)");
    let table_name = cmp("TABLE_NAME");
    let schema_name = cmp("SCHEMA_NAME");
    let call_list = mysql_list(&mut calls.iter())?;
    let local_engines = MYSQL_LOCAL_ENGINES
        .iter()
        .map(|engine| sql::string(Dialect::Mysql, engine))
        .collect::<CatalogResult<Vec<_>>>()?
        .join(", ");
    let listed = format!(
        "SELECT CONCAT(TABLE_SCHEMA, '.', TABLE_NAME) AS name, TABLE_TYPE AS kind, \
         COALESCE(ENGINE, '') AS engine FROM information_schema.TABLES \
         WHERE {full_name} IN ({qualified_list}) \
            OR (TABLE_SCHEMA = DATABASE() AND {table_name} IN ({identifier_list}))"
    );
    let current = "SELECT DATABASE() AS db";
    let databases = format!(
        "SELECT SCHEMA_NAME AS name FROM information_schema.SCHEMATA \
         WHERE {schema_name} IN ({schema_list})"
    );
    // Enabled engines that are not local.
    let engines = format!(
        "SELECT ENGINE AS engine FROM information_schema.ENGINES \
         WHERE SUPPORT IN ('YES', 'DEFAULT') AND UPPER(ENGINE) NOT IN ({local_engines})"
    );
    let loadable = mysql_loadable(&call_list);
    let (script, loadable_index) = if mariadb {
        (
            format!(
                "{listed};\nCOMMIT;\n{databases};\nCOMMIT;\n{engines};\nCOMMIT;\n{current};\n\
                 {loadable};\nCOMMIT"
            ),
            7,
        )
    } else {
        (
            format!("{listed};\n{databases};\n{engines};\n{current};\n{loadable}"),
            4,
        )
    };
    let sets = mysql_script(session, &script, loadable_index).await?;
    if sets.len() < 4 {
        return Err(CatalogError::Unexpected {
            message: "the table lookup returned too few results".into(),
        });
    }
    let mut reasons = Vec::new();
    let mut remote_engines = Vec::new();
    for row in sets[2].iter() {
        remote_engines.push(row.req_text("engine")?);
    }
    let mut databases = BTreeSet::new();
    for row in sets[1].iter() {
        databases.insert(fold(&row.req_text("name")?));
    }
    // The tables the catalog lists: a view the server did not expand
    // (its body cannot be seen), a table of an engine not known to be
    // local, or a system view / local table (fine). What it does not list
    // is a temporary table or a sequence-engine table, which can only be
    // of a remote engine when one is enabled: then SHOW CREATE TABLE
    // decides, for every table (a temporary table shadows a listed one).
    for row in sets[0].iter() {
        let name = row.req_text("name")?;
        let kind = row.req_text("kind")?;
        let engine = row.req_text("engine")?;
        if kind.eq_ignore_ascii_case("VIEW") {
            reasons.push(format!(
                "{name} is a view the server did not expand, which can call stored functions."
            ));
        } else if !kind.eq_ignore_ascii_case("SYSTEM VIEW")
            && !MYSQL_LOCAL_ENGINES.contains(&engine.to_ascii_uppercase().as_str())
        {
            reasons.push(format!(
                "{name} is a table of the {engine} engine, which is not known to keep its data \
                 on this server; reading it can reach another server."
            ));
        }
    }
    if !remote_engines.is_empty() {
        let current = sets[3]
            .iter()
            .next()
            .map(|row| row.text("db"))
            .transpose()?
            .flatten()
            .map(|db| fold(&db));
        // A candidate the rewritten statement printed is read for sure:
        // its definition must be shown; a word of the text may be no table.
        let mut candidates: Vec<(String, String, bool)> = pairs
            .iter()
            .filter(|(schema, _)| databases.contains(schema))
            .map(|pair| (pair.0.clone(), pair.1.clone(), printed.contains(pair)))
            .collect();
        if let Some(db) = current {
            for name in &identifiers {
                let pair = (db.clone(), name.clone());
                if !candidates
                    .iter()
                    .any(|(s, n, _)| (s, n) == (&pair.0, &pair.1))
                {
                    candidates.push((pair.0, pair.1, false));
                }
            }
        }
        reasons.extend(mysql_show_create(session, &candidates, mariadb).await?);
    }
    if let Some(loadable) = sets.get(4) {
        for row in loadable.iter() {
            let name = row.req_text("name")?;
            reasons.push(format!(
                "{name}() is a loadable function on this server; the statement may call it, \
                 and it can change things a rollback does not undo."
            ));
        }
    }
    Ok(reasons)
}

/// `SHOW CREATE TABLE` of every candidate `(schema, name, printed)`, as
/// one script; a name that is no table (error 1146) is skipped unless the
/// rewritten statement printed it (then the server reads a table whose
/// definition cannot be shown: a reason), a table whose definition cannot
/// be shown otherwise, a view, and a table of an engine not known to be
/// local are reasons. A failed statement ends the script, so the rest is
/// sent again without it.
async fn mysql_show_create(
    session: &dyn Session,
    tables: &[(String, String, bool)],
    mariadb: bool,
) -> CatalogResult<Vec<String>> {
    let mut reasons = Vec::new();
    let mut from = 0usize;
    while from < tables.len() {
        let rest = &tables[from..];
        let statements = rest
            .iter()
            .map(|(schema, name, _)| {
                Ok(format!(
                    "SHOW CREATE TABLE {}",
                    sql::qualified(Dialect::Mysql, &[Some(schema), Some(name)])?
                ))
            })
            .collect::<CatalogResult<Vec<_>>>()?;
        let script = if mariadb {
            format!("{};\nCOMMIT", statements.join(";\nCOMMIT;\n"))
        } else {
            statements.join(";\n")
        };
        let (sets, failed) = match rows::run(session, &script).await {
            Ok(output) => (Rows::all(output)?, None),
            Err(CatalogError::Driver { error }) if error.kind == ErrorKind::Query => {
                let partial = error.partial.as_ref();
                let failed = partial.and_then(|p| p.failed_statement);
                let results = partial.map(|p| p.results.clone()).unwrap_or_default();
                let sets = Rows::all(ExecOutput {
                    results,
                    notices: Vec::new(),
                    elapsed_ms: 0,
                })?;
                let no_such_table = error
                    .code
                    .as_deref()
                    .and_then(|c| c.split_whitespace().next())
                    == Some("1146");
                (sets, Some((failed, error.message, no_such_table)))
            }
            Err(error) => return Err(error),
        };
        for (at, set) in sets.iter().enumerate() {
            let Some((schema, name, _)) = rest.get(at) else {
                break;
            };
            let row = set.single("SHOW CREATE TABLE")?;
            let definition = if set.columns().iter().any(|c| c == "Create Table") {
                row.req_text("Create Table")?
            } else {
                reasons.push(format!(
                    "{schema}.{name} is a view the server did not expand, which can call stored \
                     functions."
                ));
                continue;
            };
            match engine_of(&definition) {
                Some(engine)
                    if MYSQL_LOCAL_ENGINES.contains(&engine.to_ascii_uppercase().as_str()) => {}
                Some(engine) => reasons.push(format!(
                    "{schema}.{name} is a table of the {engine} engine, which is not known to \
                     keep its data on this server; reading it can reach another server."
                )),
                None => reasons.push(format!(
                    "The engine of {schema}.{name} could not be read from its definition."
                )),
            }
        }
        match failed {
            None => break,
            Some((failed_statement, message, no_such_table)) => {
                // On MariaDB every other statement is a COMMIT.
                let index = failed_statement
                    .map(|at| {
                        let at = at as usize;
                        if mariadb { at / 2 } else { at }
                    })
                    .unwrap_or(sets.len());
                let Some((schema, name, printed)) = rest.get(index) else {
                    return Err(CatalogError::Unexpected {
                        message: format!("SHOW CREATE TABLE failed: {message}"),
                    });
                };
                if !no_such_table || *printed {
                    reasons.push(format!(
                        "The definition of {schema}.{name} could not be read ({message})."
                    ));
                }
                from += index + 1;
            }
        }
    }
    Ok(reasons)
}

/// The engine named in the table options of a `SHOW CREATE TABLE`
/// definition (`ENGINE=InnoDB` after the column list's closing
/// parenthesis), in upper case: an `ENGINE=` inside the column list (a
/// column comment or default, a column name) does not count. None when
/// the definition has no column list or no engine option.
pub(super) fn engine_of(definition: &str) -> Option<String> {
    let options = table_options(definition)?;
    let tokens = safety::tokens(options, Dialect::Mysql, true);
    for (i, token) in tokens.iter().enumerate() {
        let Token::Word {
            text,
            quoted: false,
        } = token
        else {
            continue;
        };
        if !text.eq_ignore_ascii_case("ENGINE") || tokens.get(i + 1) != Some(&Token::Punct('=')) {
            continue;
        }
        if let Some(Token::Word { text: engine, .. }) = tokens.get(i + 2) {
            return Some(engine.clone());
        }
    }
    None
}

/// The text after the column list of a `CREATE TABLE` definition: the
/// first parenthesized group outside quotes and comments, which the
/// server prints right after the table name.
fn table_options(definition: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut chars = definition.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                // A quoted string or identifier: ends at an unescaped quote
                // of the same kind (a doubled quote is a character).
                while let Some((_, inner)) = chars.next() {
                    if inner == '\\' && c != '`' {
                        chars.next();
                    } else if inner == c {
                        if chars.peek().is_some_and(|(_, next)| *next == c) {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            }
            '/' if chars.peek().is_some_and(|(_, next)| *next == '*') => {
                chars.next();
                let mut last = ' ';
                for (_, inner) in chars.by_ref() {
                    if last == '*' && inner == '/' {
                        break;
                    }
                    last = inner;
                }
            }
            '#' => {
                for (_, inner) in chars.by_ref() {
                    if inner == '\n' {
                        break;
                    }
                }
            }
            '-' if chars.peek().is_some_and(|(_, next)| *next == '-') => {
                for (_, inner) in chars.by_ref() {
                    if inner == '\n' {
                        break;
                    }
                }
            }
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return definition.get(at + 1..);
                }
            }
            _ => {}
        }
    }
    None
}
