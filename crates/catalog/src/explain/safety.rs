//! Whether a statement may run under `EXPLAIN ANALYZE`, which executes it.

use varve_core::Engine;
use varve_driver::{Dialect, LexKind, ServerInfo, StatementKind};

use super::{AnalyzeSafety, dialect, single_statement};

/// Decides whether `statement` may run under `ANALYZE` on `server`.
///
/// - Statements that only read (`SELECT`, `VALUES`, `TABLE`, `WITH ...
///   SELECT`) and call only built-in functions without side effects are
///   [`AnalyzeSafety::ReadOnly`]. A PostgreSQL `WITH` holding `INSERT` /
///   `UPDATE` / `DELETE` / `MERGE` and `SELECT INTO` (creates a table) are
///   [`AnalyzeSafety::RolledBack`]; MySQL `SELECT ... INTO OUTFILE /
///   DUMPFILE` writes a server file and is refused.
/// - `INSERT`, `REPLACE`, `UPDATE`, `DELETE` and `MERGE` run inside a
///   transaction that is rolled back: [`AnalyzeSafety::RolledBack`].
/// - Any of these that calls a function with effects a rollback does not
///   undo (advisory and user locks, sequences, server files, other
///   connections, server signals, `SLEEP` / `BENCHMARK`, ...) or a function
///   that is not a known side-effect-free built-in (user-defined functions
///   can do anything) is [`AnalyzeSafety::NeedsConfirmation`], with a
///   warning naming each function. So are MariaDB `NEXT VALUE FOR` /
///   `PREVIOUS VALUE FOR` sequence reads.
/// - Everything else (DDL, `TRUNCATE`, `GRANT`, transaction control,
///   `CALL`, `COPY`, maintenance, session settings, ...) and scripts with
///   more than one statement are [`AnalyzeSafety::Refused`]: MySQL commits
///   DDL implicitly, and procedures can commit on their own.
///
/// This reads the text only, and text cannot show what a statement really
/// runs (views, casts, overloads in other schemas, keywords that are also
/// function names). It is a first filter: [`super::analyze_safety_checked`]
/// asks the server and is what [`super::explain`] uses; a call of a
/// function this check does not know is the one reason it leaves to the
/// server ([`TextCheck::only_unknown`]).
#[must_use]
pub fn analyze_safety(server: &ServerInfo, statement: &str) -> AnalyzeSafety {
    text_check(server, statement).safety
}

/// [`analyze_safety`] with what the server check needs to know about the
/// answer.
pub(super) struct TextCheck {
    /// The text's verdict.
    pub(super) safety: AnalyzeSafety,
    /// The verdict is [`AnalyzeSafety::NeedsConfirmation`] only because the
    /// statement calls functions the text check does not know (which the
    /// server resolves by object id); nothing else in the text asks.
    pub(super) only_unknown: bool,
}

/// [`analyze_safety`], and whether unknown functions are its only reason
/// to ask.
pub(super) fn text_check(server: &ServerInfo, statement: &str) -> TextCheck {
    let known = |safety: AnalyzeSafety| TextCheck {
        safety,
        only_unknown: false,
    };
    let dialect = match dialect(server) {
        Ok(dialect) => dialect,
        Err(error) => {
            return known(AnalyzeSafety::Refused {
                reason: error.to_string(),
            });
        }
    };
    let (text, kind) = match single_statement(server, statement) {
        Ok(found) => found,
        Err(error) => {
            return known(AnalyzeSafety::Refused {
                reason: error.to_string(),
            });
        }
    };
    let postgres = dialect == Dialect::Postgres;
    // MySQL text reads differently with NO_BACKSLASH_ESCAPES; whatever is
    // code in either reading counts.
    let readings: Vec<Vec<Token>> = if postgres {
        vec![tokens(&text, dialect, false)]
    } else {
        vec![tokens(&text, dialect, true), tokens(&text, dialect, false)]
    };
    let words: Vec<String> = readings
        .iter()
        .flat_map(|reading| words_of(reading))
        .collect();
    let has = |word: &str| words.iter().any(|w| w == word);
    let mut functions: Vec<String> = Vec::new();
    for reading in &readings {
        for call in calls(reading) {
            if !functions.contains(&call) {
                functions.push(call);
            }
        }
    }

    let undo = if postgres {
        "Sequence values it takes (nextval, serial and identity columns) are not given back by \
         the rollback."
    } else {
        "AUTO_INCREMENT values and sequence values it takes are not given back by the rollback, \
         and changes to non-transactional tables (MyISAM, MEMORY, Aria, CSV, ...) are not \
         rolled back at all."
    };
    let (mut confirm, unknown) = function_warnings(&functions, dialect);
    // MySQL and MariaDB: `@x := ...` and `... INTO @x` change a user
    // variable of the session, which no rollback restores.
    let assigns = !postgres
        && readings.iter().any(|reading| {
            reading.windows(2).any(|pair| {
                matches!(pair, [Token::Punct(':'), Token::Punct('=')])
                    || matches!(
                        pair,
                        [Token::Word { text, quoted: false }, Token::Punct('@')] if text == "INTO"
                    )
            })
        });
    if assigns {
        confirm.push(
            "The statement assigns a user variable (:= or INTO @var), which the rollback does \
             not undo."
                .to_owned(),
        );
    }
    if !postgres && has("VALUE") && has("FOR") && (has("NEXT") || has("PREVIOUS") || has("NEXTVAL"))
    {
        confirm.push(
            "NEXT VALUE FOR takes a sequence value, which the rollback does not give back."
                .to_owned(),
        );
    }

    let mut warnings = Vec::new();
    match kind {
        StatementKind::Select => {
            if !postgres && (has("OUTFILE") || has("DUMPFILE")) {
                return known(AnalyzeSafety::Refused {
                    reason: "SELECT ... INTO OUTFILE or DUMPFILE writes a file on the server, \
                             which a rollback cannot undo"
                        .into(),
                });
            }
            let modifying_cte = postgres
                && words.first().is_some_and(|w| w == "WITH")
                && ["INSERT", "UPDATE", "DELETE", "MERGE"]
                    .iter()
                    .any(|w| has(w));
            if modifying_cte {
                warnings.push(
                    "The statement changes data in its WITH clause; it runs and is rolled back \
                     afterwards, holding row locks until then."
                        .to_owned(),
                );
                warnings.push(undo.to_owned());
            }
            if postgres && !modifying_cte && has("INTO") {
                warnings.push(
                    "SELECT INTO creates a table; it runs and is rolled back afterwards."
                        .to_owned(),
                );
            }
            if !postgres && !confirm.is_empty() {
                warnings.push(undo.to_owned());
            }
        }
        StatementKind::Insert
        | StatementKind::Update
        | StatementKind::UpdateAll
        | StatementKind::Delete
        | StatementKind::DeleteAll
        | StatementKind::Merge => {
            warnings.push(
                "The statement runs, including its triggers, and is rolled back afterwards; \
                 the rows it touches stay locked until then."
                    .to_owned(),
            );
            warnings.push(undo.to_owned());
            if matches!(kind, StatementKind::UpdateAll | StatementKind::DeleteAll) {
                warnings.push(
                    "It has no WHERE clause, so it changes every row of the table before the \
                     rollback."
                        .to_owned(),
                );
            }
            if server.engine == Engine::Mysql
                && matches!(
                    kind,
                    StatementKind::Update
                        | StatementKind::UpdateAll
                        | StatementKind::Delete
                        | StatementKind::DeleteAll
                )
            {
                warnings.push(
                    "MySQL does not measure single-table UPDATE and DELETE (it reports them as \
                     not executable by the iterator executor), so the plan has no actual \
                     values; a SELECT with the same WHERE clause shows the access path."
                        .to_owned(),
                );
            }
        }
        other => {
            return known(AnalyzeSafety::Refused {
                reason: format!(
                    "EXPLAIN ANALYZE executes the statement, and a {} statement cannot be safely \
                     rolled back; use EXPLAIN without ANALYZE",
                    kind_name(other)
                ),
            });
        }
    }
    // Only unknown functions ask: the text's hard reasons are the
    // confirmations other than those, plus any warning of a change (the
    // note on what a rollback does not undo, which follows the unknown
    // calls on MySQL and MariaDB, is not one).
    let only_unknown = !unknown.is_empty()
        && warnings.iter().all(|warning| warning == undo)
        && confirm.iter().all(|reason| unknown.contains(reason));
    if !confirm.is_empty() {
        confirm.extend(warnings);
        TextCheck {
            safety: AnalyzeSafety::NeedsConfirmation { warnings: confirm },
            only_unknown,
        }
    } else if warnings.is_empty() {
        known(AnalyzeSafety::ReadOnly)
    } else {
        known(AnalyzeSafety::RolledBack { warnings })
    }
}

fn kind_name(kind: StatementKind) -> &'static str {
    match kind {
        StatementKind::Ddl | StatementKind::AlterDrop | StatementKind::Drop => "DDL",
        StatementKind::Truncate => "TRUNCATE",
        StatementKind::Grant => "GRANT or REVOKE",
        StatementKind::Transaction => "transaction control",
        StatementKind::Explain => "EXPLAIN, SHOW or DESCRIBE",
        StatementKind::Session => "session setting",
        StatementKind::Call => "CALL, DO or EXECUTE",
        StatementKind::Copy => "COPY or LOAD DATA",
        StatementKind::Maintenance => "maintenance",
        _ => "statement of this kind",
    }
}

/// Warnings for the called functions that are not known side-effect-free
/// built-ins, and among them the warnings for functions the text check
/// merely does not know (the server resolves those).
fn function_warnings(functions: &[String], dialect: Dialect) -> (Vec<String>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut unknown = Vec::new();
    for name in functions.iter().filter(|name| !pure_builtin(name, dialect)) {
        if side_effecting(name, dialect) {
            warnings.push(format!(
                "{name}() has effects a rollback does not undo (locks, sequences, files, \
                 other sessions or server state)."
            ));
        } else {
            let warning = format!(
                "{name}() is not a built-in function known to be free of side effects; a \
                 user-defined function can change things a rollback does not undo."
            );
            unknown.push(warning.clone());
            warnings.push(warning);
        }
    }
    (warnings, unknown)
}

/// PostgreSQL built-ins whose effects outlive a rollback.
pub(super) const PG_SIDE_EFFECT_NAMES: &[&str] = &[
    "nextval",
    "setval",
    "currval",
    "lastval",
    "pg_notify",
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_switch_wal",
    "pg_promote",
    "pg_create_restore_point",
    "pg_sleep",
    "pg_sleep_for",
    "pg_sleep_until",
    "set_config",
    "setseed",
    "txid_current",
    "pg_current_xact_id",
];

/// Name prefixes of PostgreSQL built-ins whose effects outlive a rollback.
pub(super) const PG_SIDE_EFFECT_PREFIXES: &[&str] = &[
    "pg_advisory",
    "pg_try_advisory",
    "lo_",
    "dblink",
    "pg_file_",
    "pg_logfile_",
    "pg_create_",
    "pg_drop_replication",
    "pg_replication_",
    "pg_stat_reset",
    "pg_wal_replay",
    "pg_log_",
    "pg_import_",
    "pg_read_",
    "pg_ls_",
    // Built-ins that run other SQL or user code by name, so neither the
    // plan nor the name of the call shows what runs: they execute a query
    // or read whole relations through SPI (`query_to_xml`, `cursor_to_xml`,
    // `table_to_xml`, `schema_to_xml`, `database_to_xml` and their
    // `_xmlschema` / `_and_xmlschema` variants, `ts_stat`, `ts_rewrite`
    // with a query), or call a named type's input function, which runs a
    // domain's checks (`pg_input_is_valid`, `pg_input_error_info`, the
    // json / jsonb `populate_record` functions, which build a row of their
    // argument's type; `json_to_record` and friends take their column
    // types from a column definition list, which the server check sees).
    // Reviewed against the stable and immutable functions of `pg_catalog`
    // in PostgreSQL 13 to 18; `xpath`, `xmlexists`, the `pg_get_*def`
    // deparsers and `format` run no SQL.
    "query_to_xml",
    "cursor_to_xml",
    "table_to_xml",
    "schema_to_xml",
    "database_to_xml",
    "ts_stat",
    "ts_rewrite",
    "pg_input_",
    "json_populate_record",
    "jsonb_populate_record",
];

/// Built-ins whose effects outlive a rollback, by exact name or prefix.
pub(super) fn side_effecting(name: &str, dialect: Dialect) -> bool {
    let bare = name.rsplit('.').next().unwrap_or(name);
    match dialect {
        Dialect::Postgres => {
            PG_SIDE_EFFECT_NAMES.contains(&bare)
                || PG_SIDE_EFFECT_PREFIXES.iter().any(|p| bare.starts_with(p))
        }
        Dialect::Mysql => {
            const NAMES: &[&str] = &[
                "get_lock",
                "release_lock",
                "release_all_locks",
                "sleep",
                "benchmark",
                "nextval",
                "setval",
                "lastval",
                "load_file",
                "last_insert_id",
                "master_pos_wait",
                "master_gtid_wait",
                "source_pos_wait",
                "wait_for_executed_gtid_set",
                "wait_until_sql_thread_after_gtids",
                "uuid_short",
            ];
            NAMES.contains(&bare)
        }
    }
}

/// Built-in functions without side effects that statements commonly call.
/// Anything else, including every schema-qualified function outside the
/// system schema, needs confirmation.
pub(super) fn pure_builtin(name: &str, dialect: Dialect) -> bool {
    let bare = match name.rsplit_once('.') {
        None => name,
        Some((schema, bare)) => {
            let system = match dialect {
                Dialect::Postgres => schema == "pg_catalog",
                Dialect::Mysql => false,
            };
            if !system {
                return false;
            }
            bare
        }
    };
    const COMMON: &[&str] = &[
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "stddev",
        "stddev_pop",
        "stddev_samp",
        "variance",
        "var_pop",
        "var_samp",
        "coalesce",
        "nullif",
        "greatest",
        "least",
        "lower",
        "upper",
        "length",
        "char_length",
        "character_length",
        "octet_length",
        "bit_length",
        "substr",
        "substring",
        "position",
        "trim",
        "ltrim",
        "rtrim",
        "lpad",
        "rpad",
        "replace",
        "concat",
        "concat_ws",
        "left",
        "right",
        "repeat",
        "reverse",
        "format",
        "md5",
        "abs",
        "ceil",
        "ceiling",
        "floor",
        "round",
        "sign",
        "sqrt",
        "power",
        "pow",
        "exp",
        "ln",
        "log",
        "log10",
        "mod",
        "pi",
        "degrees",
        "radians",
        "sin",
        "cos",
        "tan",
        "asin",
        "acos",
        "atan",
        "atan2",
        "cot",
        "extract",
        "cast",
        "now",
        "row_number",
        "rank",
        "dense_rank",
        "ntile",
        "lag",
        "lead",
        "first_value",
        "last_value",
        "nth_value",
        "cume_dist",
        "percent_rank",
        "regexp_replace",
        "regexp_like",
        "regexp_substr",
        "regexp_instr",
        "ascii",
        "chr",
        "char",
        "version",
        "random",
        "rand",
        "date",
        "time",
    ];
    const POSTGRES: &[&str] = &[
        "bool_and",
        "bool_or",
        "every",
        "array_agg",
        "string_agg",
        "json_agg",
        "jsonb_agg",
        "json_object_agg",
        "jsonb_object_agg",
        "percentile_cont",
        "percentile_disc",
        "mode",
        "strpos",
        "btrim",
        "translate",
        "split_part",
        "initcap",
        "starts_with",
        "overlay",
        "to_hex",
        "encode",
        "decode",
        "sha224",
        "sha256",
        "sha384",
        "sha512",
        "to_char",
        "to_date",
        "to_timestamp",
        "to_number",
        "date_trunc",
        "date_part",
        "date_bin",
        "age",
        "clock_timestamp",
        "statement_timestamp",
        "transaction_timestamp",
        "make_date",
        "make_time",
        "make_timestamp",
        "make_timestamptz",
        "make_interval",
        "justify_days",
        "justify_hours",
        "justify_interval",
        "trunc",
        "cbrt",
        "div",
        "gcd",
        "lcm",
        "width_bucket",
        "regexp_matches",
        "regexp_match",
        "regexp_split_to_array",
        "regexp_split_to_table",
        "regexp_count",
        "array_length",
        "array_upper",
        "array_lower",
        "array_position",
        "array_positions",
        "array_append",
        "array_prepend",
        "array_cat",
        "array_remove",
        "array_replace",
        "array_to_string",
        "string_to_array",
        "cardinality",
        "unnest",
        "generate_series",
        "generate_subscripts",
        "json_build_object",
        "jsonb_build_object",
        "json_build_array",
        "jsonb_build_array",
        "to_json",
        "to_jsonb",
        "row_to_json",
        "json_each",
        "jsonb_each",
        "json_each_text",
        "jsonb_each_text",
        "json_array_elements",
        "jsonb_array_elements",
        "json_array_elements_text",
        "jsonb_array_elements_text",
        "json_extract_path",
        "jsonb_extract_path",
        "json_extract_path_text",
        "jsonb_extract_path_text",
        "json_object_keys",
        "jsonb_object_keys",
        "jsonb_set",
        "jsonb_insert",
        "jsonb_strip_nulls",
        "jsonb_pretty",
        "json_typeof",
        "jsonb_typeof",
        "json_array_length",
        "jsonb_array_length",
        "jsonb_path_query",
        "jsonb_path_exists",
        "jsonb_path_match",
        "jsonb_path_query_array",
        "jsonb_path_query_first",
        "num_nonnulls",
        "num_nulls",
        "quote_ident",
        "quote_literal",
        "quote_nullable",
        "gen_random_uuid",
        "pg_typeof",
        "current_setting",
        "current_database",
        "current_schema",
        "timezone",
    ];
    const MYSQL: &[&str] = &[
        "group_concat",
        "json_arrayagg",
        "json_objectagg",
        "std",
        "bit_and",
        "bit_or",
        "bit_xor",
        "ifnull",
        "if",
        "isnull",
        "lcase",
        "ucase",
        "substring_index",
        "mid",
        "locate",
        "instr",
        "field",
        "find_in_set",
        "elt",
        "hex",
        "unhex",
        "sha",
        "sha1",
        "sha2",
        "crc32",
        "year",
        "month",
        "day",
        "dayofmonth",
        "dayofweek",
        "dayofyear",
        "dayname",
        "monthname",
        "hour",
        "minute",
        "second",
        "microsecond",
        "week",
        "weekday",
        "weekofyear",
        "yearweek",
        "quarter",
        "curdate",
        "curtime",
        "current_date",
        "current_time",
        "current_timestamp",
        "localtime",
        "localtimestamp",
        "sysdate",
        "utc_date",
        "utc_time",
        "utc_timestamp",
        "date_add",
        "date_sub",
        "adddate",
        "subdate",
        "addtime",
        "subtime",
        "datediff",
        "timediff",
        "timestampdiff",
        "timestampadd",
        "date_format",
        "time_format",
        "str_to_date",
        "from_unixtime",
        "unix_timestamp",
        "from_days",
        "to_days",
        "last_day",
        "makedate",
        "maketime",
        "truncate",
        "log2",
        "convert",
        "json_extract",
        "json_unquote",
        "json_object",
        "json_array",
        "json_contains",
        "json_contains_path",
        "json_length",
        "json_keys",
        "json_value",
        "json_valid",
        "json_type",
        "json_search",
        "json_depth",
        "json_quote",
        "uuid",
        "database",
        "schema",
        "bin",
        "oct",
        "conv",
        "space",
        "strcmp",
        "soundex",
        "quote",
        "inet_aton",
        "inet_ntoa",
        "bit_count",
    ];
    COMMON.contains(&bare)
        || match dialect {
            Dialect::Postgres => POSTGRES.contains(&bare),
            Dialect::Mysql => MYSQL.contains(&bare),
        }
}

/// Words that may stand before `(` without calling a function.
fn syntax_before_paren(word: &str) -> bool {
    const SYNTAX: &[&str] = &[
        "IN",
        "EXISTS",
        "AS",
        "VALUES",
        "OVER",
        "FILTER",
        "ANY",
        "ALL",
        "SOME",
        "ARRAY",
        "ROW",
        "ON",
        "USING",
        "FROM",
        "JOIN",
        "AND",
        "OR",
        "NOT",
        "WHERE",
        "SELECT",
        "WITH",
        "INTO",
        "LATERAL",
        "SETS",
        "ROLLUP",
        "CUBE",
        "WITHIN",
        "BY",
        "RECURSIVE",
        "MATERIALIZED",
        "THEN",
        "ELSE",
        "WHEN",
        "CASE",
        "RETURNING",
        "TABLE",
        "UNION",
        "INTERSECT",
        "EXCEPT",
        "HAVING",
        "LIMIT",
        "OFFSET",
        "IS",
        "LIKE",
        "ILIKE",
        "BETWEEN",
        "SIMILAR",
        "DISTINCT",
        "KEY",
        "INDEX",
        "FOR",
        "OF",
        "MATCH",
        "AGAINST",
        "SET",
        "UPDATE",
        "DELETE",
        "INSERT",
        "MERGE",
        "DO",
        "NOTHING",
        "CONFLICT",
        "PARTITION",
        "WINDOW",
        "GROUPING",
        "ORDER",
        "GROUP",
        "VARCHAR",
        "CHARACTER",
        "VARYING",
        "NUMERIC",
        "DECIMAL",
        "DEC",
        "FLOAT",
        "DOUBLE",
        "PRECISION",
        "TIMESTAMP",
        "INTERVAL",
        "BIT",
        "VARBINARY",
        "BINARY",
        "NCHAR",
        "NVARCHAR",
        "UNSIGNED",
        "SIGNED",
        "INTEGER",
        "INT",
        "BIGINT",
        "SMALLINT",
        "TINYINT",
        "MEDIUMINT",
        "REAL",
        "TEXT",
        "BLOB",
        "ENUM",
        "DATETIME",
        "ZONE",
        "ESCAPE",
        "RANGE",
        "ROWS",
        "PRECEDING",
        "FOLLOWING",
        "ASC",
        "DESC",
        "NULLS",
        "FIRST",
        "LAST",
        "ONLY",
        "STRAIGHT_JOIN",
        "IGNORE",
        "FORCE",
        "PRIMARY",
        "UNIQUE",
        "CHECK",
        "DEFAULT",
        "GENERATED",
        "IF",
        "ELSEIF",
        "WHILE",
        "TRIM",
        "LEADING",
        "TRAILING",
        "BOTH",
        "COLLATE",
        "SKIP",
        "LOCKED",
        "NOWAIT",
        "SHARE",
        "LOCK",
        "MODE",
        "FETCH",
        "NEXT",
        // PostgreSQL `OPERATOR(schema.op)`: checked as an operator by
        // `analyze_safety_checked`.
        "OPERATOR",
    ];
    SYNTAX.contains(&word)
}

/// A lexical token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Token {
    /// A word; upper-cased unless quoted.
    Word { text: String, quoted: bool },
    /// `(`.
    Open,
    /// `)`.
    Close,
    /// `.`.
    Dot,
    /// Any other punctuation character.
    Punct(char),
    /// A PostgreSQL operator: adjacent operator characters, split the way
    /// PostgreSQL's lexer splits them.
    Operator(String),
}

/// The tokens of `sql` outside literals and comments, from the driver's
/// lexer ([`varve_driver::lex`]), so this check reads the text exactly as
/// the statement splitter does. `backslash_escapes` is MySQL's default
/// `sql_mode` (off means `NO_BACKSLASH_ESCAPES`). A string constant becomes
/// the token `'`, a number or parameter the token `0`; quoted identifiers keep their name; a MySQL `"..."`
/// counts as a quoted identifier (as under `ANSI_QUOTES`), so a call
/// written that way is still found. The contents of MySQL executable
/// comments are code and are lexed as such.
pub(super) fn tokens(sql: &str, dialect: Dialect, backslash_escapes: bool) -> Vec<Token> {
    tokens_quoting(sql, dialect, backslash_escapes, true)
}

/// [`tokens`], with MySQL / MariaDB double quotes read as identifier
/// quotes (`ansi_quotes`: the `ANSI_QUOTES` SQL mode) or as string quotes
/// (the default mode). PostgreSQL always quotes identifiers with them.
pub(super) fn tokens_quoting(
    sql: &str,
    dialect: Dialect,
    backslash_escapes: bool,
    ansi_quotes: bool,
) -> Vec<Token> {
    tokens_cased(sql, dialect, backslash_escapes, ansi_quotes, true)
}

/// [`tokens_quoting`] with unquoted words kept as written (`fold` false)
/// instead of upper-cased: a MySQL / MariaDB server with
/// `lower_case_table_names = 0` compares table names as written.
pub(super) fn tokens_cased(
    sql: &str,
    dialect: Dialect,
    backslash_escapes: bool,
    ansi_quotes: bool,
    fold: bool,
) -> Vec<Token> {
    let lexemes = varve_driver::lex(sql, dialect, backslash_escapes);
    let positioned: Vec<(Token, usize, usize)> = lexemes
        .iter()
        .enumerate()
        .filter_map(|(i, lexeme)| {
            let text = sql.get(lexeme.start..lexeme.end).unwrap_or("");
            Some(match lexeme.kind {
                // The `E` of a PostgreSQL `E'...'` string is a prefix.
                LexKind::Word
                    if dialect == Dialect::Postgres
                        && text.eq_ignore_ascii_case("e")
                        && lexemes.get(i + 1).is_some_and(|next| {
                            next.kind == LexKind::String && next.start == lexeme.end
                        }) =>
                {
                    return None;
                }
                LexKind::Word => Token::Word {
                    text: if fold {
                        text.to_ascii_uppercase()
                    } else {
                        text.to_owned()
                    },
                    quoted: false,
                },
                LexKind::DoubleQuoted if dialect == Dialect::Mysql && !ansi_quotes => {
                    Token::Punct('\'')
                }
                LexKind::QuotedIdent | LexKind::DoubleQuoted => Token::Word {
                    text: unquote(text),
                    quoted: true,
                },
                // A string constant; numbers and parameters are `0`.
                LexKind::String | LexKind::DollarString => Token::Punct('\''),
                LexKind::Number | LexKind::Parameter => Token::Punct('0'),
                LexKind::Comment
                | LexKind::ExecutableCommentStart
                | LexKind::ExecutableCommentEnd => return None,
                LexKind::Punct('(') => Token::Open,
                LexKind::Punct(')') => Token::Close,
                LexKind::Punct('.') => Token::Dot,
                LexKind::Punct(c) => Token::Punct(c),
            })
            .map(|token| (token, lexeme.start, lexeme.end))
        })
        .collect();
    if dialect == Dialect::Postgres {
        merge_operators(positioned)
    } else {
        positioned.into_iter().map(|(token, _, _)| token).collect()
    }
}

/// Characters PostgreSQL operators are made of.
const OPERATOR_CHARS: &str = "+-*/<>=~!@#%^&|`?";

/// Joins adjacent PostgreSQL operator characters into operators. As in
/// PostgreSQL's lexer, a multi-character operator ending in `+` or `-`
/// loses those characters (they become operators of their own) unless it
/// contains one of `~ ! @ # % ^ & | ` ?`.
fn merge_operators(positioned: Vec<(Token, usize, usize)>) -> Vec<Token> {
    let mut out = Vec::with_capacity(positioned.len());
    let mut pending = String::new();
    let mut pending_end = None;
    let flush = |pending: &mut String, out: &mut Vec<Token>| {
        if pending.is_empty() {
            return;
        }
        let keeps_sign = pending.chars().any(|c| "~!@#%^&|`?".contains(c));
        let mut body = pending.clone();
        let mut trailing = Vec::new();
        if !keeps_sign {
            while body.len() > 1 && (body.ends_with('+') || body.ends_with('-')) {
                if let Some(sign) = body.pop() {
                    trailing.push(sign.to_string());
                }
            }
        }
        out.push(Token::Operator(body));
        out.extend(trailing.into_iter().rev().map(Token::Operator));
        pending.clear();
    };
    for (token, start, end) in positioned {
        match token {
            Token::Punct(c) if OPERATOR_CHARS.contains(c) => {
                if pending_end != Some(start) {
                    flush(&mut pending, &mut out);
                }
                pending.push(c);
                pending_end = Some(end);
            }
            other => {
                flush(&mut pending, &mut out);
                pending_end = None;
                out.push(other);
            }
        }
    }
    flush(&mut pending, &mut out);
    out
}

/// The name inside a quoted identifier: without the quotes, doubled quote
/// characters made single. An unterminated one runs to the end.
fn unquote(text: &str) -> String {
    let mut chars = text.chars();
    let Some(quote) = chars.next() else {
        return String::new();
    };
    let inner = chars.as_str();
    let inner = inner.strip_suffix(quote).unwrap_or(inner);
    let doubled: String = [quote, quote].iter().collect();
    inner.replace(&doubled, &quote.to_string())
}

/// The unquoted words of a token list.
fn words_of(tokens: &[Token]) -> Vec<String> {
    tokens
        .iter()
        .filter_map(|t| match t {
            Token::Word {
                text,
                quoted: false,
            } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The upper-cased words of `sql` outside string literals, quoted
/// identifiers and comments.
#[cfg(test)]
pub(crate) fn words(sql: &str, dialect: Dialect, backslash_escapes: bool) -> Vec<String> {
    words_of(&tokens(sql, dialect, backslash_escapes))
}

/// Functions called in a token list, lower-cased (quoted names as
/// written), schema-qualified when written so. A word before `(` is not a
/// call when it is syntax (`IN (`, `AS (`, a type such as `varchar(10)`),
/// follows `AS` or `::` (alias column lists and casts) or `INTO` (an
/// insert column list), or it names a CTE (`WITH name (a, b) AS (...)`).
/// `f(...) AS (a int)` (a column definition list) is a call.
fn calls(tokens: &[Token]) -> Vec<String> {
    let cte_names = cte_names(tokens);
    let mut found = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let Token::Word { text, quoted } = token else {
            continue;
        };
        if tokens.get(i + 1) != Some(&Token::Open) {
            continue;
        }
        if !quoted && syntax_before_paren(text) {
            continue;
        }
        let previous = i.checked_sub(1).and_then(|p| tokens.get(p));
        // An alias column list: `AS g(i)`, `WITH ORDINALITY u(x, n)`.
        if matches!(previous, Some(Token::Word { text, quoted: false })
            if text == "AS" || text == "ORDINALITY")
        {
            continue;
        }
        let cast = i >= 2
            && tokens.get(i - 1) == Some(&Token::Punct(':'))
            && tokens.get(i - 2) == Some(&Token::Punct(':'));
        if cast {
            continue;
        }
        // `INSERT INTO t (a, b)` / `REPLACE INTO s.t (a)`: a column list.
        let before_name = if previous == Some(&Token::Dot) {
            i.checked_sub(3)
        } else {
            i.checked_sub(1)
        };
        if matches!(
            before_name.and_then(|p| tokens.get(p)),
            Some(Token::Word { text, quoted: false }) if text == "INTO"
        ) {
            continue;
        }
        // A CTE column list: `WITH name (a, b) AS (...)`.
        if cte_names.contains(&i) {
            continue;
        }
        // An alias column list right after a call or subquery,
        // `f(...) g(i)`: a name cannot follow `)` as a call.
        if previous == Some(&Token::Close) {
            continue;
        }
        // `TABLESAMPLE BERNOULLI (50)` / `SYSTEM (50)`: the built-in
        // sampling methods (others stay calls).
        if !quoted
            && matches!(text.as_str(), "BERNOULLI" | "SYSTEM")
            && matches!(previous, Some(Token::Word { text, quoted: false }) if text == "TABLESAMPLE")
        {
            continue;
        }
        let name = |text: &str, quoted: bool| {
            if quoted {
                text.to_owned()
            } else {
                text.to_ascii_lowercase()
            }
        };
        let mut full = name(text, *quoted);
        if previous == Some(&Token::Dot)
            && let Some(Token::Word {
                text: schema,
                quoted: schema_quoted,
            }) = i.checked_sub(2).and_then(|p| tokens.get(p))
        {
            full = format!("{}.{full}", name(schema, *schema_quoted));
        }
        if !found.contains(&full) {
            found.push(full);
        }
    }
    found
}

/// The indexes of the CTE names of every `WITH` list: after `WITH` (and
/// `RECURSIVE`) a name, an optional column list, `AS`, an optional
/// `[NOT] MATERIALIZED`, the parenthesized query, and then either `,` and
/// the next CTE or the end of the list.
fn cte_names(tokens: &[Token]) -> Vec<usize> {
    let word = |at: usize, expected: &str| {
        matches!(
            tokens.get(at),
            Some(Token::Word { text, quoted: false }) if text == expected
        )
    };
    let mut names = Vec::new();
    for (start, token) in tokens.iter().enumerate() {
        if !matches!(token, Token::Word { text, quoted: false } if text == "WITH") {
            continue;
        }
        let mut at = start + 1;
        if word(at, "RECURSIVE") {
            at += 1;
        }
        loop {
            if !matches!(tokens.get(at), Some(Token::Word { .. })) {
                break;
            }
            let name = at;
            at += 1;
            if tokens.get(at) == Some(&Token::Open) {
                let Some(close) = matching_close(tokens, at) else {
                    break;
                };
                at = close + 1;
            }
            if !word(at, "AS") {
                break;
            }
            at += 1;
            if word(at, "NOT") {
                at += 1;
            }
            if word(at, "MATERIALIZED") {
                at += 1;
            }
            if tokens.get(at) != Some(&Token::Open) {
                break;
            }
            names.push(name);
            let Some(close) = matching_close(tokens, at) else {
                break;
            };
            at = close + 1;
            if tokens.get(at) != Some(&Token::Punct(',')) {
                break;
            }
            at += 1;
        }
    }
    names
}

pub(super) fn matching_close(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, token) in tokens.iter().enumerate().skip(open) {
        match token {
            Token::Open => depth += 1,
            Token::Close => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
pub(crate) fn calls_in(sql: &str, dialect: Dialect) -> Vec<String> {
    calls(&tokens(sql, dialect, true))
}
