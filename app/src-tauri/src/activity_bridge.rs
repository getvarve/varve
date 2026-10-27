//! Turns session events into activity log entries (design doc 9.2) and
//! forwards them to extension hooks.

mod details;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use varve_activity::{
    ActivityEvent, ActivityEventBuilder, Category, ConnectionDetails, ErrorDetails, Recorder,
    RowCounts, SqlDialect,
};
use varve_core::Engine;
use varve_driver::{
    Dialect, DriverError, Durability, ErrorKind, LexKind, MysqlMode, MysqlServer, Notice,
    RanStatement, ServerInfo, StatementKind, TransactionEffect, TransactionTracker, lex_mysql,
    split_statements, transaction_effect,
};
use varve_records::Records;

use crate::sessions::{Outcome, SessionEvent, SessionInfo, SessionPurpose};

/// Records session events in the activity log.
pub struct ActivityBridge {
    recorder: Arc<Recorder>,
    records: Records,
    /// MySQL and MariaDB sessions that ran `USE mysql` (in a script or in
    /// the profile's init SQL): their default database may be `mysql` from
    /// then on (kept until the session closes, whatever later `USE`
    /// statements did).
    in_mysql: Mutex<HashSet<String>>,
    /// PostgreSQL sessions whose user is (`true`) or is not (`false`) a
    /// superuser; absent when unknown.
    superusers: Mutex<HashMap<String, bool>>,
}

/// A `prefer` connection that ended without TLS.
fn downgraded(encryption: &crate::sessions::Encryption) -> bool {
    encryption.mode == varve_driver::TlsMode::Prefer && encryption.tls == Some(false)
}

fn error_details(error: &DriverError) -> ErrorDetails {
    ErrorDetails {
        code: error.code.clone(),
        message: error.message.clone(),
    }
}

/// An exact security event: its action and the statement it records.
type Exact = (&'static str, String);

/// A `privilege.unverified` event: its reason and the statements in doubt.
#[cfg(test)]
type Doubt = (&'static str, String);

/// How a script run ended, as far as privilege logging needs to know.
#[derive(Debug, Clone, Copy)]
struct RunEnd<'a> {
    /// The whole script succeeded.
    succeeded: bool,
    /// The error, when it failed or was cancelled.
    error: Option<&'a DriverError>,
    /// Whether a transaction was open before the run (driver status flag).
    in_transaction_before: Option<bool>,
    /// Whether a transaction was open after the run (driver status flag).
    in_transaction_after: Option<bool>,
    /// The statements that ran, when the driver knows them exactly.
    ran: Option<&'a [RanStatement]>,
    /// The MySQL or MariaDB server, for which executable comments run.
    server: Option<MysqlServer>,
    /// The default database is `mysql` (the session's, or a `USE mysql` in
    /// the script), so unqualified grant table names count.
    in_mysql_database: bool,
    /// The notices and warnings the server sent during the run.
    notices: &'a [Notice],
    /// PostgreSQL 16 and later: a CREATE ROLE grants the new role to its
    /// creator with the admin option when the session user is not a
    /// superuser. `Some(true)` then, `Some(false)` when it does not (a
    /// superuser, an older server, another engine), `None` when unknown.
    creator_admin: Option<bool>,
}

/// The SQL dialect of an engine, `None` for Redis.
fn dialect_of(engine: Engine) -> Option<Dialect> {
    match engine {
        Engine::Postgres => Some(Dialect::Postgres),
        Engine::Mysql | Engine::Mariadb => Some(Dialect::Mysql),
        Engine::Redis => None,
    }
}

/// What became of each statement of a successful PostgreSQL script, from
/// its text and the transaction status before the run: in a script that
/// succeeded the status after each statement follows from the transaction
/// control statements. `None` when the result disagrees with the status the
/// driver reported after the run (then nothing is certain).
fn postgres_success(
    statements: &[varve_driver::StatementSpan<'_>],
    before: Option<bool>,
    after: Option<bool>,
) -> Option<Vec<Durability>> {
    let mut tracker = TransactionTracker::new();
    let mut open = before;
    for statement in statements {
        let mut effect = transaction_effect(statement.text, Dialect::Postgres);
        let verb = first_word(statement.text).map(str::to_ascii_uppercase);
        match &effect {
            TransactionEffect::Begin { .. } => open = Some(true),
            TransactionEffect::Commit { chain } | TransactionEffect::Rollback { chain } => {
                open = Some(*chain);
            }
            // PREPARE TRANSACTION ends the block; CALL and DO never leave
            // one open, and inside a block they cannot end it.
            TransactionEffect::Opaque if verb.as_deref() == Some("PREPARE") => open = Some(false),
            TransactionEffect::Opaque if open == Some(true) => {
                effect = TransactionEffect::Statement;
            }
            _ => {}
        }
        tracker.completed(effect, open);
    }
    (open == after).then(|| tracker.finish(after, Durability::Unknown))
}

/// What became of each statement of the script that completed: one entry
/// per completed statement, in [`split_statements`] order.
fn completed_statements(
    statements: &[varve_driver::StatementSpan<'_>],
    end: RunEnd<'_>,
) -> Vec<Durability> {
    if end.succeeded {
        return postgres_success(
            statements,
            end.in_transaction_before,
            end.in_transaction_after,
        )
        .unwrap_or_default();
    }
    // A failed (or cancelled) script: what the driver reported for the
    // statements that completed before the failure. Without it, nothing.
    match end.error.and_then(|e| e.partial.as_deref()) {
        Some(partial)
            if partial
                .failed_statement
                .and_then(|n| usize::try_from(n).ok())
                == Some(partial.completed.len()) =>
        {
            partial.completed.clone()
        }
        _ => Vec::new(),
    }
}

/// The statements of a SQL script whose effect certainly persisted, with
/// their exact security events (design doc 9.2, security, see
/// [`security_actions`]): statements that completed and were committed, by
/// the driver's report for a failed script, or for a successful one by the
/// engine's transaction rules. Anything uncertain (rolled back, still in an
/// open transaction, unknown) is not logged. The verb is read after leading
/// comments. The recorder redacts the text like any statement.
fn privilege_statements(
    engine: Engine,
    text: &str,
    end: RunEnd<'_>,
) -> Vec<(&'static str, String)> {
    let committed: Vec<&str> = match engine {
        // PostgreSQL runs every statement of Varve's split as a query of its
        // own: the split is the server's.
        Engine::Postgres => {
            let statements = split_statements(text, Dialect::Postgres);
            let completed = completed_statements(&statements, end);
            statements
                .into_iter()
                .zip(completed)
                .filter(|(_, durability)| *durability == Durability::Committed)
                .map(|(span, _)| span.text)
                .collect()
        }
        // MySQL and MariaDB split a multi-statement request themselves:
        // only the driver's exact report of the statements that ran counts
        // (scripts that change privileges run one statement per request).
        Engine::Mysql | Engine::Mariadb => end
            .ran
            .unwrap_or_default()
            .iter()
            .filter(|r| r.durability == Durability::Committed)
            .filter_map(|r| text.get(r.offset..r.offset + r.len))
            .collect(),
        Engine::Redis => Vec::new(),
    };
    committed
        .into_iter()
        .flat_map(|statement| {
            security_actions(statement, engine, end.server)
                .into_iter()
                .map(move |action| (action, statement.to_string()))
        })
        .collect()
}

/// The code of a statement as the server reads it: comments left out,
/// MySQL executable comments that run on `server` read as code (all of
/// them without a server).
fn code_lexemes(
    statement: &str,
    dialect: Dialect,
    server: Option<MysqlServer>,
) -> Vec<varve_driver::Lexeme> {
    let lexemes = match dialect {
        Dialect::Mysql => lex_mysql(
            statement,
            &MysqlMode {
                server,
                ..MysqlMode::default()
            },
        ),
        Dialect::Postgres => varve_driver::lex(statement, dialect, true),
    };
    lexemes
        .into_iter()
        .filter(|l| {
            !matches!(
                l.kind,
                LexKind::Comment | LexKind::ExecutableCommentStart | LexKind::ExecutableCommentEnd
            )
        })
        .collect()
}

/// The code words of a statement, upper-cased, in order (see
/// [`code_lexemes`]; literals and quoted names left out).
fn code_words(statement: &str, dialect: Dialect, server: Option<MysqlServer>) -> Vec<String> {
    code_lexemes(statement, dialect, server)
        .into_iter()
        .filter(|l| l.kind == LexKind::Word)
        .map(|l| statement[l.start..l.end].to_ascii_uppercase())
        .collect()
}

/// The exact security events of a statement once it certainly took effect
/// (design doc 9.2): `privilege.granted` / `privilege.revoked` for GRANT,
/// REVOKE and statements that grant or revoke as part of their work
/// (PostgreSQL `ALTER GROUP ... ADD / DROP USER`, `CREATE ROLE ... IN ROLE
/// / ROLE / USER / ADMIN`, `ALTER DEFAULT PRIVILEGES`, `CREATE SCHEMA ...
/// GRANT`, `DROP OWNED`; MySQL `CREATE USER ... DEFAULT ROLE`, MariaDB
/// `CREATE ROLE`, which grants the new role to its creator or to the WITH
/// ADMIN account), and `user.created` / `user.altered` / `user.dropped` for
/// account and role statements (role attributes, passwords, default roles,
/// renames included). Empty for any other statement, and for one inside a
/// MySQL executable comment (in doubt instead, `executable_comment`).
fn security_actions(
    statement: &str,
    engine: Engine,
    server: Option<MysqlServer>,
) -> Vec<&'static str> {
    let Some(dialect) = dialect_of(engine) else {
        return Vec::new();
    };
    let words = code_words(statement, dialect, server);
    // Only a statement that is code itself.
    let verb = first_word(statement).map(str::to_ascii_uppercase);
    if words.first() != verb.as_ref() {
        return Vec::new();
    }
    match dialect {
        Dialect::Postgres => postgres_security_actions(statement, &words),
        Dialect::Mysql => mysql_security_actions(&words, engine == Engine::Mariadb),
    }
}

/// [`security_actions`] of a PostgreSQL statement with code `words`.
fn postgres_security_actions(statement: &str, words: &[String]) -> Vec<&'static str> {
    let word = |at: usize| words.get(at).map(String::as_str);
    // A role statement, not one about a user mapping (`CREATE USER MAPPING
    // FOR`, `DROP USER MAPPING IF EXISTS FOR`).
    let role = matches!(word(1), Some("ROLE" | "USER" | "GROUP"))
        && !(word(1) == Some("USER")
            && word(2) == Some("MAPPING")
            && matches!(word(3), Some("FOR" | "IF")));
    match word(0) {
        Some("GRANT") => vec!["privilege.granted"],
        Some("REVOKE") => vec!["privilege.revoked"],
        // ALTER GROUP name ADD USER ... / DROP USER ... (the name may be
        // quoted, so not counted as a word); RENAME TO alters the role.
        Some("ALTER") if word(1) == Some("GROUP") => {
            let membership = words
                .get(2..)
                .unwrap_or_default()
                .windows(2)
                .find_map(|pair| match (pair[0].as_str(), pair[1].as_str()) {
                    ("ADD", "USER") => Some("privilege.granted"),
                    ("DROP", "USER") => Some("privilege.revoked"),
                    _ => None,
                });
            vec![membership.unwrap_or("user.altered")]
        }
        Some("ALTER") if role => vec!["user.altered"],
        Some("ALTER") if word(1) == Some("DEFAULT") && word(2) == Some("PRIVILEGES") => words
            .iter()
            .find_map(|w| match w.as_str() {
                "GRANT" => Some("privilege.granted"),
                "REVOKE" => Some("privilege.revoked"),
                _ => None,
            })
            .into_iter()
            .collect(),
        Some("CREATE") if role => {
            // Options after the role's name that make it a member of other
            // roles or other roles members of it.
            let options = code_lexemes(statement, Dialect::Postgres, None)
                .into_iter()
                .skip(3)
                .filter(|l| l.kind == LexKind::Word)
                .any(|l| {
                    matches!(
                        statement[l.start..l.end].to_ascii_uppercase().as_str(),
                        "ROLE" | "USER" | "GROUP" | "ADMIN"
                    )
                });
            if options {
                vec!["user.created", "privilege.granted"]
            } else {
                vec!["user.created"]
            }
        }
        // CREATE SCHEMA ... GRANT ...: the embedded GRANT runs.
        Some("CREATE") if word(1) == Some("SCHEMA") && words.iter().any(|w| w == "GRANT") => {
            vec!["privilege.granted"]
        }
        Some("DROP") if role => vec!["user.dropped"],
        // DROP OWNED BY revokes every privilege of the roles.
        Some("DROP") if word(1) == Some("OWNED") => vec!["privilege.revoked"],
        _ => Vec::new(),
    }
}

/// [`security_actions`] of a MySQL or MariaDB statement with code `words`.
fn mysql_security_actions(words: &[String], mariadb: bool) -> Vec<&'static str> {
    let word = |at: usize| words.get(at).map(String::as_str);
    // CREATE OR REPLACE USER / ROLE (MariaDB).
    let created = if word(1) == Some("OR") && word(2) == Some("REPLACE") {
        word(3)
    } else {
        word(1)
    };
    let default_role = words
        .windows(2)
        .any(|pair| pair[0] == "DEFAULT" && pair[1] == "ROLE");
    match word(0) {
        Some("GRANT") => vec!["privilege.granted"],
        Some("REVOKE") => vec!["privilege.revoked"],
        // CREATE USER ... DEFAULT ROLE grants the roles (MySQL).
        Some("CREATE") if created == Some("USER") && default_role => {
            vec!["user.created", "privilege.granted"]
        }
        Some("CREATE") if created == Some("USER") => vec!["user.created"],
        // MariaDB grants a new role to its creator (or the WITH ADMIN
        // account), with the admin option.
        Some("CREATE") if created == Some("ROLE") && mariadb => {
            vec!["user.created", "privilege.granted"]
        }
        Some("CREATE") if created == Some("ROLE") => vec!["user.created"],
        Some("ALTER") if word(1) == Some("USER") => vec!["user.altered"],
        Some("DROP") if matches!(word(1), Some("USER" | "ROLE")) => vec!["user.dropped"],
        Some("RENAME") if word(1) == Some("USER") => vec!["user.altered"],
        Some("SET") if word(1) == Some("PASSWORD") => vec!["user.altered"],
        Some("SET") if word(1) == Some("DEFAULT") && word(2) == Some("ROLE") => {
            vec!["user.altered"]
        }
        _ => Vec::new(),
    }
}

/// True when an account statement sets a password (PostgreSQL `PASSWORD
/// '...'` or `PASSWORD NULL`, MySQL `IDENTIFIED ...`, `SET PASSWORD`): the
/// event then says "password changed", never the value (design doc 9.2).
fn sets_password(statement: &str, dialect: Dialect) -> bool {
    let code = code_lexemes(statement, dialect, None);
    let word = |at: usize| -> Option<String> {
        code.get(at)
            .filter(|l| l.kind == LexKind::Word)
            .map(|l| statement[l.start..l.end].to_ascii_uppercase())
    };
    if word(0).as_deref() == Some("SET") && word(1).as_deref() == Some("PASSWORD") {
        return true;
    }
    (0..code.len()).any(|at| match word(at).as_deref() {
        Some("IDENTIFIED") => dialect == Dialect::Mysql,
        Some("PASSWORD") => {
            // PostgreSQL string forms: '...', E'...', U&'...', $$...$$.
            let mut next = at + 1;
            if dialect == Dialect::Postgres
                && word(next).is_some_and(|w| w == "E" || w == "U")
                && code
                    .get(next + 1)
                    .is_some_and(|l| matches!(l.kind, LexKind::String | LexKind::Punct('&')))
            {
                next += 1;
                if code
                    .get(next)
                    .is_some_and(|l| l.kind == LexKind::Punct('&'))
                {
                    next += 1;
                }
            }
            match code.get(next).map(|l| l.kind) {
                Some(LexKind::String | LexKind::DoubleQuoted | LexKind::DollarString) => true,
                Some(LexKind::Word) => word(next).as_deref() == Some("NULL"),
                _ => false,
            }
        }
        _ => false,
    })
}

/// The MySQL and MariaDB grant tables: writing them changes privileges once
/// they are reloaded (`FLUSH PRIVILEGES`, a failed account statement on
/// MySQL, a restart).
const GRANT_TABLES: [&str; 11] = [
    "user",
    "db",
    "tables_priv",
    "columns_priv",
    "procs_priv",
    "proxies_priv",
    "global_priv",
    "global_grants",
    "role_edges",
    "roles_mapping",
    "default_roles",
];

/// Verbs of statements that write rows.
fn is_write_verb(word: &str) -> bool {
    matches!(word, "INSERT" | "REPLACE" | "UPDATE" | "DELETE")
}

/// The names (plain or quoted, lower-cased) and dots of a MySQL statement,
/// in order, other tokens as empty strings; and its code words, upper-cased.
fn mysql_tokens(statement: &str, server: Option<MysqlServer>) -> (Vec<String>, Vec<String>) {
    let mode = MysqlMode {
        server,
        ..MysqlMode::default()
    };
    let mut tokens: Vec<String> = Vec::new();
    let mut words = Vec::new();
    for lexeme in lex_mysql(statement, &mode) {
        let piece = &statement[lexeme.start..lexeme.end];
        match lexeme.kind {
            LexKind::Word => {
                words.push(piece.to_ascii_uppercase());
                tokens.push(piece.to_ascii_lowercase());
            }
            LexKind::QuotedIdent | LexKind::DoubleQuoted => {
                tokens.push(unquoted(piece).to_ascii_lowercase());
            }
            LexKind::Punct('.') => tokens.push(".".into()),
            LexKind::Comment | LexKind::ExecutableCommentStart | LexKind::ExecutableCommentEnd => {}
            _ => tokens.push(String::new()),
        }
    }
    (tokens, words)
}

/// True when a MySQL statement writes a grant table (`INSERT`, `REPLACE`,
/// `UPDATE`, `DELETE`, `TRUNCATE`, `LOAD DATA`, MariaDB `ANALYZE` of a write,
/// or table DDL such as `RENAME TABLE` swapping one in, naming
/// `mysql.<table>`, or the bare table name when the default database is
/// `mysql`) or reloads them (`FLUSH ... PRIVILEGES`).
fn writes_grant_tables(statement: &str, server: Option<MysqlServer>, in_mysql: bool) -> bool {
    let (tokens, words) = mysql_tokens(statement, server);
    let word = |at: usize| words.get(at).map(String::as_str);
    let writes = match word(0) {
        Some("FLUSH") => return words.iter().any(|w| w == "PRIVILEGES"),
        Some("INSERT" | "REPLACE" | "UPDATE" | "DELETE" | "TRUNCATE" | "LOAD") => true,
        // WITH ... UPDATE / DELETE (a CTE before a write).
        Some("WITH") => words.iter().any(|w| is_write_verb(w)),
        // MariaDB ANALYZE [FORMAT = JSON] runs the write it analyzes.
        Some("ANALYZE") => {
            let at = if word(1) == Some("FORMAT") { 3 } else { 1 };
            word(at).is_some_and(is_write_verb)
        }
        // Table DDL on a grant table: RENAME TABLE (swapping in another
        // table), ALTER, DROP, CREATE [OR REPLACE] [TEMPORARY] TABLE.
        Some("RENAME" | "ALTER" | "DROP" | "CREATE") => {
            words.iter().skip(1).take(3).any(|w| w == "TABLE")
        }
        _ => false,
    };
    // Any grant table named in the statement counts (also one only read,
    // in a join or a subquery: over-reporting rather than missing one).
    writes && names_grant_table(&tokens, in_mysql)
}

/// True when `tokens` (see [`mysql_tokens`]) name a grant table: as
/// `mysql.<table>`, or bare when the default database may be `mysql`.
fn names_grant_table(tokens: &[String], in_mysql: bool) -> bool {
    tokens
        .windows(3)
        .any(|w| w[0] == "mysql" && w[1] == "." && GRANT_TABLES.contains(&w[2].as_str()))
        || (in_mysql
            && tokens.iter().enumerate().any(|(at, token)| {
                GRANT_TABLES.contains(&token.as_str()) && (at == 0 || tokens[at - 1] != ".")
            }))
}

/// The tables a MySQL statement writes or reads by name, from `tokens` (see
/// [`mysql_tokens`]): the name after `INTO`, `UPDATE`, `INSERT`, `REPLACE`,
/// `FROM`, `JOIN`, `TABLE` or `TRUNCATE` (modifiers such as `IGNORE`
/// skipped), with its schema when qualified.
fn table_refs(tokens: &[String]) -> Vec<(Option<String>, String)> {
    let mut refs = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
        if !matches!(
            token.as_str(),
            "into" | "update" | "insert" | "replace" | "from" | "join" | "table" | "truncate"
        ) {
            continue;
        }
        let mut next = at + 1;
        while tokens.get(next).is_some_and(|t| {
            matches!(
                t.as_str(),
                "low_priority"
                    | "high_priority"
                    | "delayed"
                    | "ignore"
                    | "quick"
                    | "into"
                    | "table"
            )
        }) {
            next += 1;
        }
        let Some(name) = tokens.get(next).filter(|t| !t.is_empty() && *t != ".") else {
            continue;
        };
        if tokens.get(next + 1).is_some_and(|t| t == ".") {
            if let Some(table) = tokens.get(next + 2).filter(|t| !t.is_empty() && *t != ".") {
                refs.push((Some(name.clone()), table.clone()));
            }
        } else {
            refs.push((None, name.clone()));
        }
    }
    refs
}

/// True when a MySQL `CREATE EVENT` / `ALTER EVENT` may change privileges
/// later, when the event runs outside any session: its body calls a
/// procedure, runs dynamic SQL, has a privilege or role setting statement,
/// `FLUSH`, or names a grant table as a table (bare names count when the
/// event's schema is `mysql`); or it is an `ALTER EVENT` without a body
/// (a new schedule, ENABLE, a rename: the body it runs is not in the
/// statement), unless it disables the event.
fn event_may_change_privileges(
    statement: &str,
    server: Option<MysqlServer>,
    in_mysql: bool,
) -> bool {
    let (tokens, words) = mysql_tokens(statement, server);
    let alter = match words.first().map(String::as_str) {
        Some("CREATE") => false,
        Some("ALTER") => true,
        _ => return false,
    };
    // EVENT in the header (after OR REPLACE, DEFINER = ...), not a column
    // or another object named `event`.
    let Some(at) = words.iter().position(|w| w == "EVENT") else {
        return false;
    };
    if at > 6
        || words[..at].iter().any(|w| {
            matches!(
                w.as_str(),
                "TABLE"
                    | "VIEW"
                    | "PROCEDURE"
                    | "FUNCTION"
                    | "TRIGGER"
                    | "USER"
                    | "ROLE"
                    | "DATABASE"
                    | "SCHEMA"
                    | "INDEX"
                    | "SERVER"
                    | "TABLESPACE"
            )
        })
    {
        return false;
    }
    let Some(body) = words[at..]
        .iter()
        .position(|w| w == "DO")
        .and_then(|d| words.get(at + d + 1..))
    else {
        return alter && !words.iter().any(|w| w == "DISABLE");
    };
    // The event's schema: qualified name, else the session's database.
    let event_at = tokens.iter().position(|t| t == "event").unwrap_or(0);
    let mut name_at = event_at + 1;
    while tokens
        .get(name_at)
        .is_some_and(|t| matches!(t.as_str(), "if" | "not" | "exists"))
    {
        name_at += 1;
    }
    let in_mysql_schema = if tokens.get(name_at + 1).is_some_and(|t| t == ".") {
        tokens.get(name_at).is_some_and(|t| t == "mysql")
    } else {
        in_mysql
    };
    let body_start = tokens.iter().position(|t| t == "do").unwrap_or(0);
    let names_grant_table_as_table =
        table_refs(&tokens[body_start..])
            .into_iter()
            .any(|(schema, table)| {
                GRANT_TABLES.contains(&table.as_str())
                    && match schema {
                        Some(schema) => schema == "mysql",
                        None => in_mysql_schema,
                    }
            });
    body.iter().any(|w| {
        matches!(
            w.as_str(),
            "CALL"
                | "EXECUTE"
                | "GRANT"
                | "REVOKE"
                | "FLUSH"
                | "MANDATORY_ROLES"
                | "ACTIVATE_ALL_ROLES_ON_LOGIN"
        )
    }) || privilege_words(body)
        || names_grant_table_as_table
}

/// True when the code lexeme at `at` is a user variable name (`@name`, not
/// a system variable `@@name`).
fn is_user_variable(code: &[varve_driver::Lexeme], at: usize) -> bool {
    at > 0
        && code[at - 1].kind == LexKind::Punct('@')
        && (at < 2 || code[at - 2].kind != LexKind::Punct('@'))
}

/// True when a MySQL `SET` / `RESET` changes the roles every account gets
/// (`mandatory_roles`) or activates (`activate_all_roles_on_login`). User
/// variables of those names do not count.
fn changes_role_settings(statement: &str, server: Option<MysqlServer>) -> bool {
    let code = code_lexemes(statement, Dialect::Mysql, server);
    let first = code
        .first()
        .map(|l| statement[l.start..l.end].to_ascii_uppercase());
    matches!(first.as_deref(), Some("SET" | "RESET"))
        && code.iter().enumerate().any(|(at, l)| {
            matches!(l.kind, LexKind::Word | LexKind::QuotedIdent)
                && !is_user_variable(&code, at)
                && {
                    let name = statement[l.start..l.end].to_ascii_lowercase();
                    name.contains("mandatory_roles") || name.contains("activate_all_roles_on_login")
                }
        })
}

/// True when a MySQL `SET` turns the event scheduler on (`event_scheduler`
/// to anything but `OFF`, `DISABLED` or 0): pending events then run, and
/// their bodies may change privileges.
fn starts_event_scheduler(statement: &str, server: Option<MysqlServer>) -> bool {
    let code = code_lexemes(statement, Dialect::Mysql, server);
    let text = |l: &varve_driver::Lexeme| statement[l.start..l.end].to_ascii_lowercase();
    if code.first().map(text).as_deref() != Some("set") {
        return false;
    }
    code.iter().enumerate().any(|(at, l)| {
        if !matches!(l.kind, LexKind::Word | LexKind::QuotedIdent)
            || unquoted(&text(l)) != "event_scheduler"
            || is_user_variable(&code, at)
        {
            return false;
        }
        // The value after `=`, `:=` or TO.
        let value = code[at + 1..]
            .iter()
            .find(|v| !matches!(v.kind, LexKind::Punct('=' | ':')) && text(v) != "to")
            .map(|v| unquoted(&text(v)).trim_matches('\'').to_string());
        !matches!(value.as_deref(), Some("off" | "disabled" | "0" | "false"))
    })
}

/// True when a MySQL statement creates or drops a stored procedure or
/// function: with `automatic_sp_privileges` (on by default) the server
/// grants the creator EXECUTE and ALTER ROUTINE on it when it could not
/// already run and alter it, and removes those grants on DROP. Loadable
/// functions (`SONAME`) do not count.
fn changes_routine_privileges(statement: &str, server: Option<MysqlServer>) -> bool {
    let words = code_words(statement, Dialect::Mysql, server);
    match words.first().map(String::as_str) {
        Some("DROP") => matches!(
            words.get(1).map(String::as_str),
            Some("PROCEDURE" | "FUNCTION")
        ),
        Some("CREATE") => {
            let Some(at) = words
                .iter()
                .take(8)
                .position(|w| w == "PROCEDURE" || w == "FUNCTION")
            else {
                return false;
            };
            !words[..at].iter().any(|w| {
                matches!(
                    w.as_str(),
                    "TABLE" | "TRIGGER" | "EVENT" | "VIEW" | "USER" | "ROLE" | "INDEX"
                )
            }) && !words.iter().any(|w| w == "SONAME")
        }
        _ => false,
    }
}

/// The PostgreSQL system catalogs whose rows carry privileges (ACL
/// columns, role attributes, memberships, default privileges).
const PRIVILEGE_CATALOGS: [&str; 16] = [
    "pg_authid",
    "pg_auth_members",
    "pg_class",
    "pg_namespace",
    "pg_proc",
    "pg_database",
    "pg_default_acl",
    "pg_largeobject_metadata",
    "pg_attribute",
    "pg_type",
    "pg_language",
    "pg_tablespace",
    "pg_foreign_data_wrapper",
    "pg_foreign_server",
    "pg_parameter_acl",
    "pg_init_privs",
];

/// True when a PostgreSQL statement writes a system catalog that carries
/// privileges (a superuser's `UPDATE pg_catalog.pg_authid`, `pg_class
/// .relacl`, `pg_auth_members`, ...): any INSERT, UPDATE, DELETE, MERGE,
/// TRUNCATE, `COPY ... FROM` or WITH ... write naming one of
/// [`PRIVILEGE_CATALOGS`] (also one only read in it: over-reporting rather
/// than missing one).
fn writes_privilege_catalog(statement: &str) -> bool {
    let code = code_lexemes(statement, Dialect::Postgres, None);
    let words: Vec<String> = code
        .iter()
        .filter(|l| l.kind == LexKind::Word)
        .map(|l| statement[l.start..l.end].to_ascii_uppercase())
        .collect();
    let writes = match words.first().map(String::as_str) {
        Some("INSERT" | "UPDATE" | "DELETE" | "MERGE" | "TRUNCATE") => true,
        // COPY table FROM (COPY (query) TO only reads).
        Some("COPY") => {
            code.get(1).is_some_and(|l| l.kind != LexKind::Punct('('))
                && words.iter().any(|w| w == "FROM")
        }
        Some("WITH") => words
            .iter()
            .any(|w| matches!(w.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE")),
        _ => false,
    };
    writes
        && code.iter().any(|l| {
            let name = match l.kind {
                LexKind::Word => statement[l.start..l.end].to_ascii_lowercase(),
                LexKind::QuotedIdent => unquoted(&statement[l.start..l.end]),
                _ => return false,
            };
            PRIVILEGE_CATALOGS.contains(&name.as_str())
        })
}

/// True when a PostgreSQL statement changes who owns objects (`ALTER ...
/// OWNER TO`, `REASSIGN OWNED`, `CREATE DATABASE ... OWNER`, `CREATE SCHEMA
/// ... AUTHORIZATION`): the owner holds every privilege on them.
fn changes_ownership(words: &[String]) -> bool {
    let word = |at: usize| words.get(at).map(String::as_str);
    match word(0) {
        // OWNER TO as a clause, not a column or constraint named `owner`
        // being renamed (`RENAME [COLUMN] owner TO ...`).
        Some("ALTER") => words.windows(3).any(|p| {
            p[1] == "OWNER"
                && p[2] == "TO"
                && !matches!(
                    p[0].as_str(),
                    "RENAME" | "COLUMN" | "CONSTRAINT" | "ATTRIBUTE"
                )
        }),
        Some("REASSIGN") => word(1) == Some("OWNED"),
        Some("CREATE") => match word(1) {
            Some("DATABASE") => words.iter().any(|w| w == "OWNER"),
            Some("SCHEMA") => words.iter().any(|w| w == "AUTHORIZATION"),
            _ => false,
        },
        _ => false,
    }
}

/// Why a statement may have changed privileges although no exact event
/// records it, or `None`. Every procedure call and DO block is in doubt
/// (the code they run is not read); so are dynamic SQL, MySQL compound
/// statements, events whose body may change privileges, grant table
/// writes, role settings, privilege statements in executable comments that
/// run, PostgreSQL ownership changes and extension scripts, and statements
/// with exact events whose effect is not certain.
fn privilege_doubt(statement: &str, engine: Engine, end: RunEnd<'_>) -> Option<&'static str> {
    let dialect = dialect_of(engine)?;
    let mysql = dialect == Dialect::Mysql;
    let code = code_lexemes(statement, dialect, end.server);
    let words = code_words(statement, dialect, end.server);
    let first = words.first().map(String::as_str);
    // A statement label (`lbl: LOOP`): a name followed by a colon.
    let labelled = matches!(
        code.as_slice(),
        [name, colon, ..]
            if matches!(name.kind, LexKind::Word | LexKind::QuotedIdent)
                && colon.kind == LexKind::Punct(':')
    );
    match first {
        Some("CALL") => Some("procedure_call"),
        Some("DO") if !mysql => Some("do_block"),
        Some("EXECUTE") => Some("dynamic_sql"),
        Some("IF" | "CASE" | "LOOP" | "WHILE" | "REPEAT" | "FOR") if mysql => {
            Some("compound_statement")
        }
        Some("BEGIN") if mysql && words.get(1).map(String::as_str) == Some("NOT") => {
            Some("compound_statement")
        }
        _ if mysql && labelled => Some("compound_statement"),
        _ if mysql
            && (event_may_change_privileges(statement, end.server, end.in_mysql_database)
                || starts_event_scheduler(statement, end.server)) =>
        {
            Some("scheduled_event")
        }
        _ if mysql && changes_routine_privileges(statement, end.server) => {
            Some("routine_privileges")
        }
        _ if mysql && writes_grant_tables(statement, end.server, end.in_mysql_database) => {
            Some("grant_tables")
        }
        _ if mysql && changes_role_settings(statement, end.server) => Some("role_settings"),
        _ if mysql && running_comment_changes_privileges(statement, end.server) => {
            Some("executable_comment")
        }
        _ if !mysql && changes_ownership(&words) => Some("ownership"),
        _ if !mysql && writes_privilege_catalog(statement) => Some("catalog_write"),
        // An extension script may grant (CREATE EXTENSION, ALTER EXTENSION
        // ... UPDATE).
        _ if !mysql
            && matches!(first, Some("CREATE" | "ALTER"))
            && words.get(1).map(String::as_str) == Some("EXTENSION") =>
        {
            Some("extension")
        }
        _ if !security_actions(statement, engine, end.server).is_empty() => Some("not_confirmed"),
        _ => None,
    }
}

/// True when an executable comment that runs on `server` is in the
/// statement and its code mentions privileges. Without a known server every
/// executable comment counts as running.
fn running_comment_changes_privileges(statement: &str, server: Option<MysqlServer>) -> bool {
    let mode = MysqlMode {
        server,
        ..MysqlMode::default()
    };
    let lexemes = lex_mysql(statement, &mode);
    let mut inside = false;
    let mut words = Vec::new();
    for lexeme in &lexemes {
        match lexeme.kind {
            LexKind::ExecutableCommentStart => inside = true,
            LexKind::ExecutableCommentEnd => inside = false,
            LexKind::Word if inside => {
                words.push(statement[lexeme.start..lexeme.end].to_ascii_uppercase());
            }
            _ => {}
        }
    }
    privilege_words(&words)
}

/// True when upper-cased code `words` have a GRANT, REVOKE, SET PASSWORD,
/// DEFAULT ROLE, or a CREATE [OR REPLACE] / ALTER / DROP / RENAME of a USER
/// or ROLE. (Role settings are recognised by statement, so reading
/// `@@mandatory_roles` is no privilege code.)
fn privilege_words(words: &[String]) -> bool {
    words
        .iter()
        .any(|w| matches!(w.as_str(), "GRANT" | "REVOKE"))
        || words.windows(2).any(|pair| {
            (matches!(pair[0].as_str(), "CREATE" | "ALTER" | "DROP" | "RENAME")
                && matches!(pair[1].as_str(), "USER" | "ROLE"))
                || (pair[0] == "SET" && pair[1] == "PASSWORD")
                || (pair[0] == "DEFAULT" && pair[1] == "ROLE")
        })
        || or_replace_account(words)
}

/// True when upper-cased `words` have MariaDB `CREATE OR REPLACE USER` or
/// `CREATE OR REPLACE ROLE`.
fn or_replace_account(words: &[String]) -> bool {
    words.windows(4).any(|w| {
        w[0] == "CREATE"
            && w[1] == "OR"
            && w[2] == "REPLACE"
            && matches!(w[3].as_str(), "USER" | "ROLE")
    })
}

/// The quoting modes a MySQL session may read a script in
/// (`NO_BACKSLASH_ESCAPES` and `ANSI_QUOTES` off or on), for `server`.
fn quoting_modes(server: Option<MysqlServer>) -> [MysqlMode; 4] {
    [(true, false), (false, false), (true, true), (false, true)].map(
        |(backslash_escapes, ansi_quotes)| MysqlMode {
            backslash_escapes,
            ansi_quotes,
            oracle: false,
            server,
        },
    )
}

/// The code words of a MySQL script in each quoting mode (see
/// [`quoting_modes`]), with their byte offsets: words that matter for
/// privileges upper-cased, others as empty strings (so pairs stay pairs).
fn code_words_per_mode(text: &str, server: Option<MysqlServer>) -> Vec<Vec<(usize, String)>> {
    quoting_modes(server)
        .iter()
        .map(|mode| {
            lex_mysql(text, mode)
                .into_iter()
                .filter(|l| l.kind == LexKind::Word)
                .map(|l| {
                    let word = text[l.start..l.end].to_ascii_uppercase();
                    let relevant = matches!(
                        word.as_str(),
                        "GRANT"
                            | "REVOKE"
                            | "CREATE"
                            | "ALTER"
                            | "DROP"
                            | "RENAME"
                            | "USER"
                            | "ROLE"
                            | "SET"
                            | "PASSWORD"
                            | "DEFAULT"
                            | "REPLACE"
                            | "CALL"
                            | "EXECUTE"
                    );
                    (l.start, if relevant { word } else { String::new() })
                })
                .collect()
        })
        .collect()
}

/// True when the part `from..to` of a MySQL script has code that may change
/// privileges (privilege words, CALL, EXECUTE) in any quoting mode the
/// session may be in: the server may split and quote the script otherwise
/// than Varve's default reading. String literals are not code in any mode.
fn privilege_code_in_any_mode(per_mode: &[Vec<(usize, String)>], from: usize, to: usize) -> bool {
    per_mode.iter().any(|words| {
        let start = words.partition_point(|(at, _)| *at < from);
        let stop = words.partition_point(|(at, _)| *at < to);
        let slice: Vec<String> = words[start..stop].iter().map(|(_, w)| w.clone()).collect();
        privilege_words(&slice) || slice.iter().any(|w| w == "CALL" || w == "EXECUTE")
    })
}

/// True when a MySQL script has a `USE mysql` (executable comments read
/// as code, so possibly over-counting).
fn uses_mysql_database(text: &str) -> bool {
    split_statements(text, Dialect::Mysql)
        .iter()
        .any(|statement| {
            let words = code_words(statement.text, Dialect::Mysql, None);
            let mut names = lex_mysql(statement.text, &MysqlMode::default())
                .into_iter()
                .filter(|l| matches!(l.kind, LexKind::Word | LexKind::QuotedIdent))
                .map(|l| unquoted(&statement.text[l.start..l.end]).to_ascii_lowercase());
            words.first().map(String::as_str) == Some("USE")
                && names.nth(1).as_deref() == Some("mysql")
        })
}

/// An identifier without its quotes (`` `a``b` `` gives ``a`b``); other
/// text as it is.
fn unquoted(piece: &str) -> String {
    for quote in ['`', '"'] {
        if let Some(inner) = piece
            .strip_prefix(quote)
            .map(|rest| rest.strip_suffix(quote).unwrap_or(rest))
        {
            let doubled: String = [quote, quote].iter().collect();
            return inner.replace(&doubled, &quote.to_string());
        }
    }
    piece.to_string()
}

/// The server of a MySQL or MariaDB session, for executable comments.
fn mysql_server(info: &SessionInfo) -> Option<MysqlServer> {
    let mariadb = match info.engine {
        Engine::Mysql => false,
        Engine::Mariadb => true,
        Engine::Postgres | Engine::Redis => return None,
    };
    let patch = info.server.patch?;
    Some(MysqlServer::new(
        mariadb,
        info.server.major,
        info.server.minor,
        patch,
    ))
}

/// The statement a MySQL or MariaDB script failed at, when the driver
/// reported the completed ones exactly: the first statement with code after
/// the last reported one, read in every quoting mode the session may be in
/// (the session may have changed `sql_mode`), so possibly more than one
/// reading.
fn failing_mysql_statements<'t>(
    text: &'t str,
    report: &[RanStatement],
    server: Option<MysqlServer>,
) -> Vec<&'t str> {
    let from = report.last().map_or(0, |r| r.offset + r.len);
    let Some(rest) = text.get(from..) else {
        return Vec::new();
    };
    let mut found: Vec<&'t str> = Vec::new();
    for mode in quoting_modes(server) {
        let spans = varve_driver::split_statements_mysql(rest, &mode)
            .unwrap_or_else(|_| split_statements(rest, Dialect::Mysql));
        if let Some(span) = spans
            .into_iter()
            .find(|s| !code_words(s.text, Dialect::Mysql, server).is_empty())
            && !found.contains(&span.text)
        {
            found.push(span.text);
        }
    }
    found
}

/// What became of a PostgreSQL statement, as far as the doubt about it
/// goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// It completed and was committed.
    Committed,
    /// It completed and was rolled back.
    RolledBack,
    /// The server rejected it: a failed statement changes nothing.
    Failed,
    /// Anything else (still in a transaction, cancelled, unknown).
    Uncertain,
}

/// The [`Fate`] of the statement at `index` of a PostgreSQL script.
fn postgres_fate(index: usize, completed: &[Durability], end: RunEnd<'_>) -> Fate {
    match completed.get(index) {
        Some(Durability::Committed) => Fate::Committed,
        Some(Durability::RolledBack) => Fate::RolledBack,
        Some(_) => Fate::Uncertain,
        None => {
            // Without a partial output (nothing completed, the driver
            // keeps none) the first statement failed.
            let failed_here = end.error.is_some_and(|error| {
                error.kind == ErrorKind::Query
                    && match error.partial.as_deref() {
                        Some(partial) => {
                            partial
                                .failed_statement
                                .and_then(|n| usize::try_from(n).ok())
                                == Some(index)
                        }
                        None => index == 0,
                    }
            });
            if !end.succeeded && failed_here {
                Fate::Failed
            } else {
                Fate::Uncertain
            }
        }
    }
}

/// True when a doubt about the statement a MySQL or MariaDB script failed
/// at stands: code that ran in part (a procedure, dynamic SQL, a compound
/// statement) may have changed privileges before the failure, and a MariaDB
/// account statement or grant table write may have applied in part (a
/// GRANT to two accounts of which one is missing grants to the other).
/// MySQL account statements and InnoDB grant table writes are atomic. A
/// cancelled or lost statement may have completed.
fn failing_doubt_stands(
    reason: &str,
    statement: &str,
    engine: Engine,
    server: Option<MysqlServer>,
    error: Option<&DriverError>,
) -> bool {
    if error.is_none_or(|e| e.kind != ErrorKind::Query) {
        return true;
    }
    let mariadb = engine == Engine::Mariadb;
    // A statement the server could not parse never ran.
    let syntax_error = error
        .and_then(|e| e.code.as_deref())
        .and_then(|code| code.split_whitespace().next())
        .is_some_and(|code| matches!(code, "1064" | "1149"));
    if syntax_error {
        return false;
    }
    match reason {
        "procedure_call" | "dynamic_sql" | "compound_statement" => true,
        "grant_tables" | "executable_comment" => mariadb,
        // Only a statement naming several accounts can apply in part.
        "not_confirmed" => mariadb && details::named_accounts(statement, engine, server) > 1,
        _ => false,
    }
}

/// The statements of a run that may have changed privileges without being
/// recorded exactly (design doc 9.2, `privilege.unverified`): see
/// [`privilege_doubt`]; for MySQL and MariaDB also any privilege code in a
/// script whose statements the driver could not report exactly, and the
/// statement an exactly reported script failed at. Returns the reason of
/// the first one and their text. Statements that did not run, failed
/// PostgreSQL statements and rolled back ones are left out.
#[cfg(test)]
fn unverified_privileges(
    engine: Engine,
    text: &str,
    end: RunEnd<'_>,
    logged: &[(&'static str, String)],
) -> Option<(&'static str, String)> {
    unverified_all(engine, text, end, logged).map(|(reasons, texts)| (reasons[0], texts))
}

/// [`unverified_privileges`] with every reason, most telling first.
fn unverified_all(
    engine: Engine,
    text: &str,
    end: RunEnd<'_>,
    logged: &[(&'static str, String)],
) -> Option<(Vec<&'static str>, String)> {
    let dialect = dialect_of(engine)?;
    // MySQL text is split as the server reads it (executable comments that
    // do not run on it are comments), when the server is known.
    let statements = match (dialect, end.server) {
        (Dialect::Mysql, Some(server)) => varve_driver::split_statements_mysql(
            text,
            &MysqlMode {
                server: Some(server),
                ..MysqlMode::default()
            },
        )
        .unwrap_or_else(|_| split_statements(text, dialect)),
        _ => split_statements(text, dialect),
    };
    // How many statements may have run: all after a success, those up to
    // and including the failing one after a failure (it may have run in
    // part, a procedure for example).
    let ran = if end.succeeded {
        statements.len()
    } else {
        match end.error.and_then(|e| e.partial.as_deref()) {
            Some(partial) => partial
                .failed_statement
                .and_then(|n| usize::try_from(n).ok())
                .map_or(statements.len(), |n| n + 1),
            None => 1,
        }
    };
    let exact: Option<&[RanStatement]> = match engine {
        Engine::Mysql | Engine::Mariadb => end.ran,
        _ => None,
    };
    let mut doubts: Vec<(&'static str, &str)> = Vec::new();
    match exact {
        // The driver reported the statements exactly: doubt only those
        // that may change privileges without a record.
        Some(report) => {
            for statement in report {
                let Some(text) = text.get(statement.offset..statement.offset + statement.len)
                else {
                    continue;
                };
                if logged.iter().any(|(_, l)| l == text) {
                    continue;
                }
                let Some(reason) = privilege_doubt(text, engine, end) else {
                    continue;
                };
                // A GRANT or a grant table write that was rolled back
                // changed nothing; a procedure that ran may have committed
                // on its own.
                if statement.durability == Durability::RolledBack
                    && matches!(reason, "not_confirmed" | "grant_tables")
                {
                    continue;
                }
                doubts.push((reason, text));
            }
            // The statement the script failed at is not in the report, but
            // it may have changed privileges before it failed.
            if !end.succeeded {
                for failing in failing_mysql_statements(text, report, end.server) {
                    if let Some(reason) = privilege_doubt(failing, engine, end)
                        && failing_doubt_stands(reason, failing, engine, end.server, end.error)
                    {
                        doubts.push((reason, failing));
                    }
                }
            }
        }
        None => {
            let per_mode = if dialect == Dialect::Mysql {
                code_words_per_mode(text, end.server)
            } else {
                Vec::new()
            };
            for statement in statements.iter().take(ran) {
                if logged.iter().any(|(_, l)| l == statement.text) {
                    continue;
                }
                let reason = match privilege_doubt(statement.text, engine, end) {
                    Some(reason) => Some(reason),
                    // MySQL without an exact report: privilege code in any
                    // quoting mode the server may read the script in
                    // (literals left out: only EXECUTE or CALL runs them,
                    // and those are in doubt themselves).
                    None if dialect == Dialect::Mysql
                        && (statement.kind == StatementKind::Grant
                            || privilege_code_in_any_mode(
                                &per_mode,
                                statement.offset,
                                statement.offset + statement.text.len(),
                            )) =>
                    {
                        Some("not_attributed")
                    }
                    None => None,
                };
                if let Some(reason) = reason {
                    doubts.push((reason, statement.text));
                }
            }
        }
    }
    // PostgreSQL statements that are in doubt for what they are (and not
    // for code they run) only when they may have taken effect: not after
    // the server rejected them or they were rolled back; a statement with
    // exact events only while its transaction state is not certain.
    if dialect == Dialect::Postgres {
        let completed = completed_statements(&statements, end);
        doubts.retain(|(reason, statement)| {
            let fate = statements
                .iter()
                .position(|s| std::ptr::eq(s.text, *statement))
                .map_or(Fate::Uncertain, |index| {
                    postgres_fate(index, &completed, end)
                });
            match *reason {
                "not_confirmed" => fate == Fate::Uncertain,
                "ownership" | "extension" | "catalog_write" => {
                    matches!(fate, Fate::Committed | Fate::Uncertain)
                }
                _ => true,
            }
        });
    }
    if doubts.is_empty() {
        return None;
    }
    let reasons = by_priority(doubts.iter().map(|(reason, _)| *reason));
    let texts: Vec<&str> = doubts.iter().map(|(_, t)| *t).collect();
    Some((reasons, texts.join(";\n")))
}

/// Doubt reasons from the most to the least telling: code that ran first
/// (a reviewer filtering on `procedure_call` must find every CALL), the
/// unattributed doubt last.
const REASON_PRIORITY: [&str; 15] = [
    "procedure_call",
    "do_block",
    "dynamic_sql",
    "compound_statement",
    "executable_comment",
    "scheduled_event",
    "grant_tables",
    "catalog_write",
    "role_settings",
    "ownership",
    "extension",
    "routine_privileges",
    "no_effect",
    "not_confirmed",
    "not_attributed",
];

/// The distinct reasons, ordered by [`REASON_PRIORITY`].
fn by_priority<'a>(reasons: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut list: Vec<&str> = Vec::new();
    for reason in reasons {
        if !list.contains(&reason) {
            list.push(reason);
        }
    }
    list.sort_by_key(|reason| {
        REASON_PRIORITY
            .iter()
            .position(|p| p == reason)
            .unwrap_or(REASON_PRIORITY.len())
    });
    list
}

/// PostgreSQL notices that say a GRANT (or ALTER GROUP ... ADD USER) did
/// not grant everything it names, and those that say a REVOKE (or ALTER
/// GROUP ... DROP USER) did not revoke it. Matched in English.
const POSTGRES_NOT_GRANTED: [&str; 4] = [
    "no privileges were granted",
    "not all privileges were granted",
    "is already a member of role",
    "has already been granted membership",
];

/// See [`POSTGRES_NOT_GRANTED`].
const POSTGRES_NOT_REVOKED: [&str; 4] = [
    "no privileges could be revoked",
    "not all privileges could be revoked",
    "has not been granted membership",
    "is not a member of role",
];

/// MySQL and MariaDB warnings (codes) of an IF [NOT] EXISTS or IGNORE
/// UNKNOWN USER statement that skipped something: no such grant (1141,
/// 1147, 1403), no such account (3162, MariaDB 1974, 1976), the account or
/// role exists already (3163, MariaDB 1973, 1975).
const MYSQL_SKIPPED: [u32; 9] = [1141, 1147, 1403, 3162, 3163, 1973, 1974, 1975, 1976];

/// The first double-quoted name in a server message (`role "u" ...`).
fn first_quoted(message: &str) -> Option<&str> {
    let (_, rest) = message.split_once('"')?;
    rest.split_once('"').map(|(name, _)| name)
}

/// True when the server said during the run that the exact `action` of
/// `statement` may not have done what it says. PostgreSQL: a notice of
/// [`POSTGRES_NOT_GRANTED`] / [`POSTGRES_NOT_REVOKED`] whose first quoted
/// name is in the statement. MySQL and MariaDB: a warning of
/// [`MYSQL_SKIPPED`] in a run with this statement, when the statement has
/// IF [NOT] EXISTS or IGNORE UNKNOWN USER (others fail instead of warning).
fn server_says_no_effect(
    engine: Engine,
    action: &str,
    statement: &str,
    text: &str,
    end: RunEnd<'_>,
) -> bool {
    let notices = end.notices;
    match engine {
        Engine::Postgres => {
            let patterns: &[&str] = match action {
                "privilege.granted" => &POSTGRES_NOT_GRANTED,
                "privilege.revoked" => &POSTGRES_NOT_REVOKED,
                _ => return false,
            };
            // The names in the statement (unquoted words folded to lower
            // case, as PostgreSQL reads them).
            let names: Vec<String> = code_lexemes(statement, Dialect::Postgres, None)
                .into_iter()
                .filter_map(|l| {
                    let piece = &statement[l.start..l.end];
                    match l.kind {
                        LexKind::Word => Some(piece.to_lowercase()),
                        LexKind::QuotedIdent => Some(unquoted(piece)),
                        _ => None,
                    }
                })
                .collect();
            notices.iter().any(|notice| {
                patterns.iter().any(|p| notice.message.contains(p))
                    && first_quoted(&notice.message)
                        .is_none_or(|name| names.iter().any(|n| n == name))
            })
        }
        Engine::Mysql | Engine::Mariadb => {
            // Unknown details: assume the statement may skip.
            let skips = details::security_details(statement, engine, end.server)
                .map_or(true, |d| {
                    d.contains_key("if_exists") || d.contains_key("ignore_unknown_user")
                });
            skips && mysql_statement_warned(statement, text, end)
        }
        Engine::Redis => false,
    }
}

/// True when the warnings of a MySQL run name a skip (see
/// [`MYSQL_SKIPPED`]) for `statement`. The driver's exact report runs one
/// statement per request: the server keeps warning details only for the
/// last one (the run's `code: message` notices), and earlier statements
/// with warnings get a note "Statement N raised ... warning(s)" without
/// codes, which counts as a possible skip.
fn mysql_statement_warned(statement: &str, text: &str, end: RunEnd<'_>) -> bool {
    let Some(report) = end.ran else {
        return false;
    };
    let Some(index) = report
        .iter()
        .position(|r| text.get(r.offset..r.offset + r.len) == Some(statement))
    else {
        return false;
    };
    let coded_skip = end.notices.iter().any(|notice| {
        notice
            .message
            .split_once(':')
            .and_then(|(code, _)| code.trim().parse::<u32>().ok())
            .is_some_and(|code| MYSQL_SKIPPED.contains(&code))
    });
    let noted = end.notices.iter().any(|notice| {
        notice
            .message
            .strip_prefix("Statement ")
            .and_then(|rest| rest.split_once(' '))
            .and_then(|(n, rest)| {
                rest.starts_with("raised")
                    .then(|| n.parse::<usize>().ok())
                    .flatten()
            })
            == Some(index + 1)
    });
    (index + 1 == report.len() && coded_skip) || noted
}

/// True when a PostgreSQL script may switch the current role, so whether
/// it is a superuser may change: `SET [SESSION | LOCAL] ROLE`, `SET ...
/// SESSION AUTHORIZATION`, `RESET ROLE`, `RESET SESSION AUTHORIZATION`,
/// `ALTER ROLE / USER ... [NO]SUPERUSER`, and DO or CALL (their code may do
/// the same).
pub(crate) fn may_switch_role(text: &str) -> bool {
    split_statements(text, Dialect::Postgres).iter().any(|s| {
        let words = code_words(s.text, Dialect::Postgres, None);
        let word = |at: usize| words.get(at).map(String::as_str);
        let rest =
            if matches!(word(1), Some("SESSION" | "LOCAL")) && word(2) != Some("AUTHORIZATION") {
                2
            } else {
                1
            };
        match word(0) {
            Some("SET" | "RESET") => {
                word(rest) == Some("ROLE")
                    || (word(rest) == Some("SESSION") && word(rest + 1) == Some("AUTHORIZATION"))
                    || (word(1) == Some("SESSION") && word(2) == Some("AUTHORIZATION"))
            }
            Some("ALTER") if matches!(word(1), Some("ROLE" | "USER")) => {
                words.iter().any(|w| w == "SUPERUSER" || w == "NOSUPERUSER")
            }
            Some("DO" | "CALL") => true,
            _ => false,
        }
    })
}

/// True when a PostgreSQL statement creates a role (`CREATE ROLE / USER /
/// GROUP`, not a user mapping).
fn creates_postgres_role(statement: &str) -> bool {
    let words = code_words(statement, Dialect::Postgres, None);
    words.first().map(String::as_str) == Some("CREATE")
        && postgres_security_actions(statement, &words).contains(&"user.created")
}

/// The security events of a run: the exact ones (see
/// [`privilege_statements`]) and at most one `privilege.unverified` (see
/// [`unverified_privileges`]). Exact `privilege.*` events of statements the
/// server said may have had no effect (see [`server_says_no_effect`])
/// become a doubt with reason `no_effect`. On PostgreSQL 16 and later a
/// CREATE ROLE by a non-superuser also grants the new role to its creator
/// (`privilege.granted`; in doubt when it is not known whether the session
/// user is a superuser).
#[cfg(test)]
fn security_events(engine: Engine, text: &str, end: RunEnd<'_>) -> (Vec<Exact>, Option<Doubt>) {
    let (exact, doubt) = security_events_all(engine, text, end);
    (exact, doubt.map(|(reasons, texts)| (reasons[0], texts)))
}

/// [`security_events`] with every doubt reason, most telling first.
fn security_events_all(
    engine: Engine,
    text: &str,
    end: RunEnd<'_>,
) -> (Vec<Exact>, Option<(Vec<&'static str>, String)>) {
    let mut exact = privilege_statements(engine, text, end);
    let mut extra: Vec<(&'static str, String)> = Vec::new();
    if engine == Engine::Postgres {
        let created: Vec<String> = exact
            .iter()
            .filter(|(action, statement)| {
                *action == "user.created" && creates_postgres_role(statement)
            })
            .map(|(_, statement)| statement.clone())
            .collect();
        for statement in created {
            let granted = exact
                .iter()
                .any(|(action, s)| *action == "privilege.granted" && *s == statement);
            match end.creator_admin {
                Some(true) if !granted => exact.push(("privilege.granted", statement)),
                None => extra.push(("not_confirmed", statement)),
                _ => {}
            }
        }
    }
    exact.retain(|(action, statement)| {
        let no_effect = action.starts_with("privilege.")
            && server_says_no_effect(engine, action, statement, text, end);
        if no_effect && !extra.iter().any(|(_, s)| s == statement) {
            extra.push(("no_effect", statement.clone()));
        }
        !no_effect
    });
    // Statements in an extra doubt count as recorded for the rest.
    let mut recorded = exact.clone();
    recorded.extend(
        extra
            .iter()
            .map(|(_, statement)| ("privilege.unverified", statement.clone())),
    );
    let doubt = unverified_all(engine, text, end, &recorded);
    if extra.is_empty() {
        return (exact, doubt);
    }
    let mut texts: Vec<String> = extra.iter().map(|(_, s)| s.clone()).collect();
    let mut reasons: Vec<&'static str> = extra.iter().map(|(r, _)| *r).collect();
    if let Some((more_reasons, more)) = doubt {
        reasons.extend(more_reasons);
        texts.push(more);
    }
    (
        exact,
        Some((by_priority(reasons.into_iter()), texts.join(";\n"))),
    )
}

/// The first word of a statement after leading `--`, `#` and `/* */`
/// comments.
fn first_word(statement: &str) -> Option<&str> {
    let mut rest = statement.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--").or_else(|| rest.strip_prefix('#')) {
            rest = after.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    rest.split(|c: char| !c.is_ascii_alphabetic())
        .find(|w| !w.is_empty())
}

fn kind_label(kinds: &[StatementKind]) -> String {
    match kinds {
        [] => "command".into(),
        [one] => format!("{one:?}").to_ascii_lowercase(),
        many => {
            let mut labels: Vec<String> = many
                .iter()
                .map(|k| format!("{k:?}").to_ascii_lowercase())
                .collect();
            labels.dedup();
            format!("script:{}", labels.join(","))
        }
    }
}

/// The init SQL a driver ran while connecting, for [`ActivityBridge::record_init_sql`].
struct InitRun<'a> {
    connection_id: &'a str,
    engine: Engine,
    server: Option<MysqlServer>,
    sql: &'a str,
    /// The transaction status after connecting.
    after: Option<bool>,
    notices: &'a [Notice],
    creator_admin: Option<bool>,
    /// The connection's database is `mysql`.
    in_mysql: bool,
    /// The session, for its purpose (none for a connection test).
    purpose: Option<&'a SessionInfo>,
    extra: &'a [(&'a str, &'a str)],
}

/// True when a failed connect may have run (part of) the init SQL: the
/// driver says the init SQL failed or changed the session, or the connect
/// was cut off (timeout, cancel, lost connection) at a point that may be
/// inside it. A refused login, TLS or network failure never reached it.
fn init_sql_may_have_run(error: Option<&DriverError>) -> bool {
    let Some(error) = error else {
        return true;
    };
    error.message.contains("init SQL")
        || matches!(
            error.kind,
            ErrorKind::Timeout | ErrorKind::Cancelled | ErrorKind::ConnectionLost
        )
}

/// See [`RunEnd::creator_admin`]: from the engine, the server's major
/// version and whether the current role is a superuser.
fn creator_admin_for(engine: Engine, major: u32, superuser: Option<bool>) -> Option<bool> {
    if engine != Engine::Postgres || major < 16 {
        return Some(false);
    }
    superuser.map(|superuser| !superuser)
}

/// The server of a MySQL or MariaDB [`ServerInfo`], for executable comments.
fn mysql_server_of(info: &ServerInfo) -> Option<MysqlServer> {
    let mariadb = match info.engine {
        Engine::Mysql => false,
        Engine::Mariadb => true,
        Engine::Postgres | Engine::Redis => return None,
    };
    Some(MysqlServer::new(
        mariadb,
        info.major,
        info.minor,
        info.patch?,
    ))
}

impl ActivityBridge {
    /// Creates the bridge.
    #[must_use]
    pub fn new(recorder: Arc<Recorder>, records: Records) -> Self {
        Self {
            recorder,
            records,
            in_mysql: Mutex::new(HashSet::new()),
            superusers: Mutex::new(HashMap::new()),
        }
    }

    fn connection_details(
        &self,
        connection_id: &str,
        engine: Engine,
        server_version: Option<&str>,
    ) -> ConnectionDetails {
        match self.records.connection(connection_id) {
            Ok(profile) => ConnectionDetails {
                engine,
                server_version: server_version.map(ToString::to_string),
                host: profile.host,
                port: Some(profile.port),
                database: profile.database,
                username: profile.username,
                via_ssh: profile.ssh.enabled,
                tls_mode: Some(format!("{:?}", profile.tls.mode).to_ascii_lowercase()),
            },
            Err(_) => ConnectionDetails {
                engine,
                server_version: server_version.map(ToString::to_string),
                host: String::new(),
                port: None,
                database: None,
                username: None,
                via_ssh: false,
                tls_mode: None,
            },
        }
    }

    /// Connection details of an open session: the database it is connected
    /// to, which differs from the profile's for per-database sessions.
    fn session_details(&self, info: &SessionInfo) -> ConnectionDetails {
        let mut details =
            self.connection_details(&info.connection_id, info.engine, Some(&info.server.version));
        if info.database.is_some() {
            details.database.clone_from(&info.database);
        }
        details
    }

    /// Remembers a MySQL or MariaDB session whose init SQL runs `USE
    /// mysql` (its default database may be `mysql` from the start), and
    /// whether a PostgreSQL session user is a superuser.
    fn note_session(&self, info: &SessionInfo, init_sql: Option<&str>, superuser: Option<bool>) {
        if matches!(info.engine, Engine::Mysql | Engine::Mariadb)
            && init_sql.is_some_and(uses_mysql_database)
        {
            self.in_mysql
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(info.id.clone());
        }
        if let Some(superuser) = superuser {
            self.superusers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(info.id.clone(), superuser);
        }
    }

    /// See [`RunEnd::creator_admin`].
    fn creator_admin(&self, info: &SessionInfo) -> Option<bool> {
        let superuser = self.superusers.lock().ok()?.get(&info.id).copied();
        creator_admin_for(info.engine, info.server.major, superuser)
    }

    /// Writes the security events of a run (see [`security_events`]):
    /// exact events with their structured details (accounts, privileges,
    /// object, grantees, attributes; see `details`) and `target`, or
    /// `details_omitted` with the reason when they cannot be read safely,
    /// and the doubt with its most telling `reason` and every one in
    /// `reasons`. `extra` details go on every event (the init SQL's
    /// `source`).
    #[allow(clippy::too_many_arguments)]
    fn write_security(
        &self,
        connection_id: &str,
        engine: Engine,
        server: Option<MysqlServer>,
        creator_admin: Option<bool>,
        (exact, unverified): (Vec<Exact>, Option<(Vec<&'static str>, String)>),
        extra: &[(&str, &str)],
        purpose: Option<&SessionInfo>,
    ) {
        let dialect = SqlDialect::from(engine);
        let finish = |mut builder: ActivityEventBuilder| {
            for (key, value) in extra {
                builder = builder.detail(*key, *value);
            }
            match purpose {
                Some(info) => Self::with_purpose(builder, info),
                None => builder,
            }
        };
        if let Some((reasons, statements)) = unverified {
            let reason = reasons.first().copied().unwrap_or("not_confirmed");
            self.write(
                finish(
                    ActivityEvent::builder(Category::Security, "privilege.unverified")
                        .detail("reason", reason)
                        .detail(
                            "reasons",
                            serde_json::Value::Array(reasons.iter().map(|r| (*r).into()).collect()),
                        )
                        .statement(statements, dialect),
                ),
                connection_id,
            );
        }
        for (action, statement) in exact {
            let mut builder = ActivityEvent::builder(Category::Security, action);
            match details::security_details(&statement, engine, server) {
                Ok(mut fields) => {
                    if engine == Engine::Postgres
                        && action == "privilege.granted"
                        && creator_admin == Some(true)
                        && creates_postgres_role(&statement)
                    {
                        fields.insert("creator_admin".into(), true.into());
                    }
                    if let Some(target) = details::target_of(&fields) {
                        builder = builder.target(target);
                    }
                    for (key, value) in fields {
                        builder = builder.detail(key, value);
                    }
                }
                Err(reason) => builder = builder.detail("details_omitted", reason),
            }
            // Passwords are logged as "password changed", never the value
            // (the recorder redacts the text as well).
            if action.starts_with("user.")
                && dialect_of(engine).is_some_and(|d| sets_password(&statement, d))
            {
                builder = builder.detail("password", "changed");
            }
            self.write(finish(builder.statement(statement, dialect)), connection_id);
        }
    }

    /// Records the profile's init SQL that the driver ran while connecting
    /// (a session open or a connection test): a `query.executed` and its
    /// security events, all with `source: init_sql` (and the run's `extra`
    /// details). The driver reports no statements exactly for it, so MySQL
    /// and MariaDB privilege statements in it are in doubt; on PostgreSQL
    /// the transaction status after connecting decides, and the init SQL's
    /// notices can turn a GRANT into `no_effect`.
    fn record_init_sql(&self, run: &InitRun<'_>) {
        let dialect = SqlDialect::from(run.engine);
        let kinds = dialect_of(run.engine)
            .map(|d| varve_driver::classify(run.sql, d))
            .unwrap_or_default();
        let mut query = ActivityEvent::query_executed(
            run.sql,
            dialect,
            kind_label(&kinds),
            RowCounts {
                returned: None,
                affected: None,
            },
            0,
        )
        .detail("source", "init_sql");
        for (key, value) in run.extra {
            query = query.detail(*key, *value);
        }
        if let Some(info) = run.purpose {
            query = Self::with_purpose(query, info);
        }
        self.write(query, run.connection_id);
        let end = RunEnd {
            succeeded: true,
            error: None,
            in_transaction_before: Some(false),
            in_transaction_after: run.after,
            ran: None,
            server: run.server,
            in_mysql_database: run.in_mysql || uses_mysql_database(run.sql),
            notices: run.notices,
            creator_admin: run.creator_admin,
        };
        let mut extra = vec![("source", "init_sql")];
        extra.extend_from_slice(run.extra);
        self.write_security(
            run.connection_id,
            run.engine,
            run.server,
            run.creator_admin,
            security_events_all(run.engine, run.sql, end),
            &extra,
            run.purpose,
        );
    }

    /// The init SQL of a connection that failed (to open, or a test) may
    /// have run in part: every statement in it that may change privileges
    /// is in doubt.
    fn record_failed_init_sql(
        &self,
        connection_id: &str,
        engine: Engine,
        sql: &str,
        error: Option<&DriverError>,
        extra: &[(&str, &str)],
    ) {
        if !init_sql_may_have_run(error) {
            return;
        }
        let end = RunEnd {
            succeeded: true,
            error: None,
            in_transaction_before: Some(false),
            in_transaction_after: None,
            ran: None,
            server: None,
            in_mysql_database: uses_mysql_database(sql),
            notices: &[],
            creator_admin: None,
        };
        let doubt = unverified_all(engine, sql, end, &[]);
        let mut all = vec![("source", "init_sql"), ("open_failed", "true")];
        all.extend_from_slice(extra);
        self.write_security(
            connection_id,
            engine,
            None,
            None,
            (Vec::new(), doubt),
            &all,
            None,
        );
    }

    /// True when the default database of a MySQL or MariaDB session may be
    /// `mysql` for a script: the session's database, a `USE mysql` in an
    /// earlier script of the session, or one in this script.
    fn in_mysql_database(&self, info: &SessionInfo, text: &str) -> bool {
        if !matches!(info.engine, Engine::Mysql | Engine::Mariadb) {
            return false;
        }
        let Ok(mut sessions) = self.in_mysql.lock() else {
            // A poisoned lock: assume the worst.
            return true;
        };
        if uses_mysql_database(text) {
            sessions.insert(info.id.clone());
        }
        sessions.contains(&info.id)
            || info
                .database
                .as_deref()
                .is_some_and(|db| db.eq_ignore_ascii_case("mysql"))
    }

    /// Marks events of metadata sessions with `purpose: metadata`.
    fn with_purpose(builder: ActivityEventBuilder, info: &SessionInfo) -> ActivityEventBuilder {
        match info.purpose {
            SessionPurpose::Metadata => builder.detail("purpose", "metadata"),
            SessionPurpose::Admin => builder.detail("purpose", "admin"),
            SessionPurpose::User => builder,
        }
    }

    fn write(&self, builder: ActivityEventBuilder, info_connection: &str) {
        let builder = if info_connection == "unsaved" {
            builder
        } else {
            builder.connection(info_connection)
        };
        match builder.build() {
            Ok(event) => {
                if let Err(error) = self.recorder.record(event) {
                    tracing::warn!(%error, "could not record an activity event");
                }
            }
            Err(error) => tracing::warn!(%error, "invalid activity event"),
        }
    }

    fn dialect(info: &SessionInfo) -> SqlDialect {
        SqlDialect::from(info.engine)
    }

    /// Records one session event.
    pub fn handle(&self, event: &SessionEvent) {
        match event {
            SessionEvent::Opened {
                info,
                duration_ms,
                init_sql,
                init_in_transaction,
                superuser,
                init_notices,
                ..
            } => {
                let init_sql = init_sql.as_deref().filter(|sql| !sql.trim().is_empty());
                self.note_session(info, init_sql, *superuser);
                let details = self.session_details(info);
                self.write(
                    Self::with_purpose(
                        ActivityEvent::connection_opened(&details).duration_ms(*duration_ms),
                        info,
                    ),
                    &info.connection_id,
                );
                // `prefer` that ended without TLS (the server or port did
                // not do TLS, or the TLS attempt failed and plain TCP was
                // used): the user asked for encryption where possible. Once
                // per user connect: the metadata and admin sessions that
                // follow it reach the same server the same way.
                if info.purpose == SessionPurpose::User && downgraded(&info.encryption) {
                    self.write(
                        ActivityEvent::tls_downgraded(
                            &details,
                            info.encryption.password_unencrypted,
                        ),
                        &info.connection_id,
                    );
                }
                if let Some(sql) = init_sql {
                    self.record_init_sql(&InitRun {
                        connection_id: &info.connection_id,
                        engine: info.engine,
                        server: mysql_server(info),
                        sql,
                        after: *init_in_transaction,
                        notices: init_notices,
                        creator_admin: self.creator_admin(info),
                        in_mysql: info
                            .database
                            .as_deref()
                            .is_some_and(|db| db.eq_ignore_ascii_case("mysql")),
                        purpose: Some(info),
                        extra: &[],
                    });
                }
            }
            SessionEvent::Tested {
                connection_id,
                engine,
                server,
                init_sql,
                init_in_transaction,
                superuser,
                init_notices,
                error,
                encryption,
            } => {
                // A test that sent the password in clear over a `prefer`
                // connection that ended without TLS is recorded too: the
                // password crossed the network as is all the same.
                if let (Some(server), Some(encryption)) = (server, encryption)
                    && downgraded(encryption)
                    && encryption.password_unencrypted
                {
                    let mut details = self.connection_details(
                        connection_id,
                        server.engine,
                        Some(&server.version),
                    );
                    details.tls_mode = Some("prefer".into());
                    self.write(
                        ActivityEvent::tls_downgraded(&details, true)
                            .detail("test_connection", true),
                        connection_id,
                    );
                }
                let Some(sql) = init_sql.as_deref().filter(|sql| !sql.trim().is_empty()) else {
                    return;
                };
                let extra = [("test_connection", "true")];
                match (server, error) {
                    (Some(server), None) => self.record_init_sql(&InitRun {
                        connection_id,
                        engine: server.engine,
                        server: mysql_server_of(server),
                        sql,
                        after: *init_in_transaction,
                        notices: init_notices,
                        creator_admin: creator_admin_for(server.engine, server.major, *superuser),
                        in_mysql: false,
                        purpose: None,
                        extra: &extra,
                    }),
                    _ => self.record_failed_init_sql(
                        connection_id,
                        *engine,
                        sql,
                        error.as_ref(),
                        &extra,
                    ),
                }
            }
            SessionEvent::OpenFailed {
                connection_id,
                engine,
                error,
                duration_ms,
                init_sql,
            } => {
                let details = self.connection_details(connection_id, *engine, None);
                self.write(
                    ActivityEvent::connection_failed(&details, &error_details(error))
                        .duration_ms(*duration_ms),
                    connection_id,
                );
                if let Some(sql) = init_sql.as_deref().filter(|sql| !sql.trim().is_empty()) {
                    self.record_failed_init_sql(connection_id, *engine, sql, Some(error), &[]);
                }
            }
            SessionEvent::Executed {
                info,
                text,
                kinds,
                outcome,
                rows,
                rows_affected,
                duration_ms,
                error,
                confirmed_destructive,
                destructive,
                in_transaction_before,
                ran,
                in_transaction_after,
                notices,
                superuser_after,
            } => {
                let dialect = Self::dialect(info);
                let kind = kind_label(kinds);
                let builder = match (outcome, error) {
                    (Outcome::Success, _) => ActivityEvent::query_executed(
                        text,
                        dialect,
                        kind,
                        RowCounts {
                            returned: *rows,
                            affected: *rows_affected,
                        },
                        *duration_ms,
                    ),
                    (Outcome::Cancelled, _) => {
                        ActivityEvent::query_cancelled(text, dialect, kind, *duration_ms)
                    }
                    (Outcome::Failure, Some(error)) => ActivityEvent::query_failed(
                        text,
                        dialect,
                        kind,
                        &error_details(error),
                        *duration_ms,
                    ),
                    (Outcome::Failure, None) => ActivityEvent::query_failed(
                        text,
                        dialect,
                        kind,
                        &ErrorDetails {
                            code: None,
                            message: "failed".into(),
                        },
                        *duration_ms,
                    ),
                };
                self.write(builder, &info.connection_id);
                let switches = info.engine == Engine::Postgres && may_switch_role(text);
                // Security events are logged exactly only when the statement
                // certainly took effect (never over-logged, see
                // `privilege_statements`); what may have changed privileges
                // otherwise is one honest "may have changed" event.
                let end = RunEnd {
                    succeeded: *outcome == Outcome::Success,
                    error: error.as_deref(),
                    in_transaction_before: *in_transaction_before,
                    in_transaction_after: *in_transaction_after,
                    ran: ran.as_deref(),
                    server: mysql_server(info),
                    in_mysql_database: self.in_mysql_database(info, text),
                    notices,
                    // A script that may switch the current role may change
                    // whether it is a superuser part way through.
                    creator_admin: if switches {
                        creator_admin_for(info.engine, info.server.major, None)
                    } else {
                        self.creator_admin(info)
                    },
                };
                self.write_security(
                    &info.connection_id,
                    info.engine,
                    end.server,
                    end.creator_admin,
                    security_events_all(info.engine, text, end),
                    &[],
                    None,
                );
                // The status the session read again after such a script.
                if switches && let Ok(mut map) = self.superusers.lock() {
                    match superuser_after {
                        Some(superuser) => map.insert(info.id.clone(), *superuser),
                        None => map.remove(&info.id),
                    };
                }
                if *confirmed_destructive {
                    let label = destructive.join(",");
                    self.write(
                        ActivityEvent::destructive_confirmed(
                            label,
                            &format!("{:?}", info.environment).to_ascii_lowercase(),
                        ),
                        &info.connection_id,
                    );
                }
            }
            SessionEvent::PolicyBlocked {
                info,
                policy,
                kinds,
            } => {
                self.write(
                    ActivityEvent::builder(Category::Safety, "policy.blocked")
                        .outcome(varve_activity::Outcome::Cancelled)
                        .detail("policy", policy.clone())
                        .detail("kind", kind_label(kinds)),
                    &info.connection_id,
                );
            }
            SessionEvent::DestructiveBlocked {
                info, statements, ..
            } => {
                let label = statements.join(",");
                self.write(
                    ActivityEvent::destructive_blocked(
                        if label.is_empty() {
                            "redis".to_string()
                        } else {
                            label
                        },
                        &format!("{:?}", info.environment).to_ascii_lowercase(),
                    ),
                    &info.connection_id,
                );
            }
            SessionEvent::Closed { info, duration_ms } => {
                if let Ok(mut sessions) = self.in_mysql.lock() {
                    sessions.remove(&info.id);
                }
                if let Ok(mut sessions) = self.superusers.lock() {
                    sessions.remove(&info.id);
                }
                let details = self.session_details(info);
                self.write(
                    Self::with_purpose(
                        ActivityEvent::connection_closed(&details, *duration_ms),
                        info,
                    ),
                    &info.connection_id,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privilege_verbs_follow_comments() {
        assert_eq!(
            first_word("-- why\n/* x */ grant select on t to u"),
            Some("grant")
        );
        assert_eq!(first_word("# note\nREVOKE ALL ON t FROM u"), Some("REVOKE"));
    }

    fn statuses(sql: &str, end: RunEnd<'_>) -> Vec<Durability> {
        completed_statements(&split_statements(sql, Dialect::Postgres), end)
    }

    fn success(before: Option<bool>, after: Option<bool>) -> RunEnd<'static> {
        RunEnd {
            succeeded: true,
            error: None,
            in_transaction_before: before,
            in_transaction_after: after,
            ran: None,
            server: None,
            in_mysql_database: false,
            notices: &[],
            creator_admin: Some(false),
        }
    }

    #[test]
    fn successful_postgres_scripts_follow_their_transaction_control() {
        use Durability::{Committed, InTransaction, RolledBack, Unknown};
        assert_eq!(
            statuses("GRANT SELECT ON t TO u", success(Some(false), Some(false))),
            [Committed]
        );
        // Rolled back by the script itself: never logged.
        assert_eq!(
            statuses(
                "BEGIN; GRANT SELECT ON t TO u; ROLLBACK",
                success(Some(false), Some(false))
            ),
            [RolledBack, RolledBack, Committed]
        );
        assert_eq!(
            statuses(
                "BEGIN; GRANT SELECT ON t TO u; SAVEPOINT a; REVOKE SELECT ON t FROM u; \
                 ROLLBACK TO a; COMMIT",
                success(Some(false), Some(false))
            ),
            [
                Committed, Committed, Committed, RolledBack, Committed, Committed
            ]
        );
        // Still open after the run.
        assert_eq!(
            statuses(
                "BEGIN; GRANT SELECT ON t TO u",
                success(Some(false), Some(true))
            ),
            [InTransaction, InTransaction]
        );
        // A transaction opened by an earlier run.
        assert_eq!(
            statuses(
                "GRANT SELECT ON t TO u; COMMIT",
                success(Some(true), Some(false))
            ),
            [Committed, Committed]
        );
        // Unknown status before, or a mismatch with the driver: nothing.
        assert!(statuses("GRANT SELECT ON t TO u", success(None, Some(false))).is_empty());
        assert!(statuses("GRANT SELECT ON t TO u", success(Some(false), Some(true))).is_empty());
        assert_eq!(
            statuses("PREPARE TRANSACTION 'x'", success(Some(true), Some(false))),
            [Unknown]
        );
    }

    fn ran(offset: usize, text: &str, kind: StatementKind, durability: Durability) -> RanStatement {
        RanStatement {
            offset,
            len: text.len(),
            kind,
            durability,
        }
    }

    #[test]
    fn mysql_grants_are_logged_only_from_the_exact_report() {
        // Audit probe P-M1: Varve's split sees a GRANT the server never ran
        // as a statement. Without the driver's exact report nothing is
        // logged, whatever the text says.
        let sql =
            "CREATE PROCEDURE pc_p() IF FALSE THEN DO 0; GRANT SELECT ON varve.* TO pc_u; END IF";
        for engine in [Engine::Mysql, Engine::Mariadb] {
            assert!(
                privilege_statements(engine, sql, success(Some(false), Some(false))).is_empty()
            );
        }
        // The report names the statements that ran.
        let sql = "START TRANSACTION; GRANT SELECT ON t TO u; REVOKE SELECT ON t FROM v; SELECT 1";
        let report = [
            ran(
                0,
                "START TRANSACTION",
                StatementKind::Transaction,
                Durability::Committed,
            ),
            ran(
                19,
                "GRANT SELECT ON t TO u",
                StatementKind::Grant,
                Durability::Committed,
            ),
            ran(
                43,
                "REVOKE SELECT ON t FROM v",
                StatementKind::Grant,
                Durability::Unknown,
            ),
        ];
        let end = RunEnd {
            ran: Some(&report),
            ..success(Some(false), Some(false))
        };
        assert_eq!(
            privilege_statements(Engine::Mysql, sql, end),
            [("privilege.granted", "GRANT SELECT ON t TO u".to_owned())]
        );
        // A report that does not fit the text logs nothing.
        let odd = [ran(
            500,
            "GRANT",
            StatementKind::Grant,
            Durability::Committed,
        )];
        let end = RunEnd {
            ran: Some(&odd),
            ..success(Some(false), Some(false))
        };
        assert!(privilege_statements(Engine::Mysql, sql, end).is_empty());
    }

    #[test]
    fn privilege_changes_that_cannot_be_recorded_are_flagged() {
        // Audit round 2, R2-6: never a false grant, but an honest doubt.
        let end = |ran: Option<&'static [RanStatement]>| RunEnd {
            ran,
            ..success(Some(false), Some(false))
        };
        let doubt = |engine: Engine, sql: &str, end: RunEnd<'_>| {
            let logged = privilege_statements(engine, sql, end);
            unverified_privileges(engine, sql, end, &logged).map(|(reason, _)| reason)
        };
        // A grant in an executable comment that ran.
        let sql = "/*!50000 GRANT SELECT ON d.* TO u */";
        let report: &'static [RanStatement] = Box::leak(Box::new([ran(
            0,
            sql,
            StatementKind::Grant,
            Durability::Committed,
        )]));
        assert!(privilege_statements(Engine::Mysql, sql, end(Some(report))).is_empty());
        assert_eq!(
            doubt(Engine::Mysql, sql, end(Some(report))),
            Some("executable_comment")
        );
        // A procedure call, dynamic SQL, a MariaDB compound statement.
        assert_eq!(
            doubt(Engine::Mysql, "CALL p()", end(None)),
            Some("procedure_call")
        );
        assert_eq!(
            doubt(
                Engine::Mysql,
                "PREPARE s FROM 'GRANT SELECT ON d.* TO u'; EXECUTE s",
                end(None)
            ),
            Some("dynamic_sql")
        );
        assert_eq!(
            doubt(
                Engine::Mariadb,
                "IF 1 THEN GRANT SELECT ON d.* TO u; END IF",
                end(None)
            ),
            Some("compound_statement")
        );
        // A GRANT without an exact report.
        assert_eq!(
            doubt(Engine::Mysql, "GRANT SELECT ON d.* TO u", end(None)),
            Some("not_confirmed")
        );
        // An exactly recorded grant, or nothing about privileges: no doubt.
        let sql = "GRANT SELECT ON d.* TO u";
        let report: &'static [RanStatement] = Box::leak(Box::new([ran(
            0,
            sql,
            StatementKind::Grant,
            Durability::Committed,
        )]));
        assert_eq!(doubt(Engine::Mysql, sql, end(Some(report))), None);
        assert_eq!(
            doubt(Engine::Mysql, "SELECT user FROM mysql.user", end(None)),
            None
        );
        // PostgreSQL: a rolled back GRANT is certain.
        let pg = success(Some(false), Some(false));
        assert_eq!(
            doubt(
                Engine::Postgres,
                "BEGIN; GRANT SELECT ON t TO u; ROLLBACK",
                pg
            ),
            None
        );
        // Every DO block and every CALL that ran is in doubt, whatever its
        // body (audit round 4: the code they run is not read).
        for sql in [
            "DO $$ BEGIN END $$",
            "DO $$ BEGIN GRANT SELECT ON t TO u; END $$",
            "DO $$ BEGIN PERFORM grant_it(); END $$",
            "/* c */ do language plpgsql $$ BEGIN CALL p(); END $$",
        ] {
            assert_eq!(doubt(Engine::Postgres, sql, pg), Some("do_block"), "{sql}");
        }
        for sql in ["CALL noop()", "call s.p(1)", "SELECT 1; CALL p()"] {
            assert_eq!(
                doubt(Engine::Postgres, sql, pg),
                Some("procedure_call"),
                "{sql}"
            );
            assert_eq!(
                doubt(Engine::Mysql, sql, end(None)),
                Some("procedure_call"),
                "{sql}"
            );
        }
        // A CALL in an executable comment that runs is a CALL (Low 2).
        assert_eq!(
            doubt(Engine::Mysql, "/*!50000 CALL grantp() */", end(None)),
            Some("procedure_call")
        );
        // A CALL rolled back by the script may still have committed.
        let sql = "CALL p()";
        let report: &'static [RanStatement] = Box::leak(Box::new([ran(
            0,
            sql,
            StatementKind::Call,
            Durability::RolledBack,
        )]));
        assert_eq!(
            doubt(Engine::Mysql, sql, end(Some(report))),
            Some("procedure_call")
        );
        // PostgreSQL ALTER GROUP: an exact event when committed, a doubt
        // when its transaction is still open.
        assert_eq!(
            privilege_statements(Engine::Postgres, "ALTER GROUP g ADD USER u, v", pg),
            [(
                "privilege.granted",
                "ALTER GROUP g ADD USER u, v".to_owned()
            )]
        );
        assert_eq!(
            privilege_statements(Engine::Postgres, "alter group \"G\" drop user u", pg),
            [(
                "privilege.revoked",
                "alter group \"G\" drop user u".to_owned()
            )]
        );
        assert_eq!(
            privilege_statements(Engine::Postgres, "ALTER GROUP g RENAME TO h", pg),
            [("user.altered", "ALTER GROUP g RENAME TO h".to_owned())]
        );
        let open = success(Some(false), Some(true));
        assert_eq!(
            doubt(Engine::Postgres, "BEGIN; ALTER GROUP g ADD USER u", open),
            Some("not_confirmed")
        );
        // Grant table writes and FLUSH PRIVILEGES.
        for sql in [
            "INSERT INTO mysql.user (Host, User) VALUES ('%', 'x')",
            "UPDATE `mysql`.`db` SET Select_priv = 'Y'",
            "DELETE FROM mysql . tables_priv WHERE User = 'x'",
            "REPLACE INTO mysql.columns_priv SELECT * FROM t",
            "TRUNCATE TABLE mysql.procs_priv",
            "update mysql.proxies_priv set Grantor = ''",
            "DELETE FROM mysql.global_priv",
            "INSERT INTO mysql.role_edges VALUES ('%', 'r', '%', 'u', 'N')",
            "INSERT INTO mysql.default_roles VALUES ('%', 'u', '%', 'r')",
            "WITH x AS (SELECT 1) UPDATE mysql.user SET Super_priv = 'Y'",
            "/*!50000 UPDATE mysql.user SET Super_priv = 'Y' */",
            "FLUSH PRIVILEGES",
            "flush no_write_to_binlog privileges",
        ] {
            assert_eq!(
                doubt(Engine::Mysql, sql, end(None)),
                Some("grant_tables"),
                "{sql}"
            );
        }
        // An unqualified grant table counts only when the default database
        // may be mysql.
        let sql = "UPDATE user SET Super_priv = 'Y'";
        assert_eq!(doubt(Engine::Mysql, sql, end(None)), None);
        let in_mysql = RunEnd {
            in_mysql_database: true,
            ..end(None)
        };
        assert_eq!(doubt(Engine::Mysql, sql, in_mysql), Some("grant_tables"));
        assert!(uses_mysql_database("USE mysql; UPDATE user SET x = 1"));
        assert!(uses_mysql_database("use `mysql`"));
        assert!(!uses_mysql_database("USE app; SELECT 'use mysql'"));
        // Reads and other tables are not writes.
        for sql in [
            "SELECT * FROM mysql.user",
            "UPDATE app.user SET name = 'x'",
            "INSERT INTO t VALUES ('mysql.user')",
            "WITH x AS (SELECT 1) SELECT * FROM mysql.user",
            "FLUSH TABLES",
        ] {
            assert_eq!(doubt(Engine::Mysql, sql, end(None)), None, "{sql}");
        }
        // Executable comments that do not run on the server (Low 1).
        let mysql80 = RunEnd {
            server: Some(MysqlServer::new(false, 8, 0, 46)),
            ..success(Some(false), Some(false))
        };
        for sql in [
            "SELECT 5 /*!99999 ; GRANT SELECT ON d.* TO u */",
            "SELECT 5 /*M! GRANT SELECT ON d.* TO u */",
        ] {
            assert_eq!(doubt(Engine::Mysql, sql, mysql80), None, "{sql}");
        }
        let mariadb = RunEnd {
            server: Some(MysqlServer::new(true, 10, 6, 28)),
            ..success(Some(false), Some(false))
        };
        assert_eq!(
            doubt(
                Engine::Mariadb,
                "SELECT 5 /*!57000 GRANT SELECT ON d.* TO u */",
                mariadb
            ),
            None
        );
        assert_eq!(
            doubt(
                Engine::Mysql,
                "/*!50000 GRANT SELECT ON d.* TO u */",
                mysql80
            ),
            Some("executable_comment")
        );
        // A script that failed at its first statement ran nothing after it.
        let failed = DriverError::new(varve_driver::ErrorKind::Query, "boom");
        let end = RunEnd {
            succeeded: false,
            error: Some(&failed),
            in_transaction_before: Some(false),
            in_transaction_after: Some(false),
            ran: None,
            server: None,
            in_mysql_database: false,
            notices: &[],
            creator_admin: Some(false),
        };
        assert_eq!(doubt(Engine::Mysql, "SELECT nothing; CALL p()", end), None);
    }

    #[test]
    fn failed_scripts_use_the_driver_report() {
        use varve_driver::{ErrorKind, PartialOutput};
        let report = |failed: Option<u32>, completed: Vec<Durability>| {
            DriverError::new(ErrorKind::Query, "boom").with_partial(PartialOutput {
                results: Vec::new(),
                notices: Vec::new(),
                elapsed_ms: 1,
                failed_statement: failed,
                completed,
            })
        };
        fn failed(error: &DriverError) -> RunEnd<'_> {
            RunEnd {
                succeeded: false,
                error: Some(error),
                in_transaction_before: Some(false),
                in_transaction_after: Some(false),
                ran: None,
                server: None,
                in_mysql_database: false,
                notices: &[],
                creator_admin: Some(false),
            }
        }
        let sql = "GRANT SELECT ON t TO u; SELECT 1/0";
        let error = report(Some(1), vec![Durability::Committed]);
        assert_eq!(statuses(sql, failed(&error)), [Durability::Committed]);
        assert_eq!(
            privilege_statements(Engine::Postgres, sql, failed(&error)),
            [("privilege.granted", "GRANT SELECT ON t TO u".to_owned())]
        );
        // MySQL logs from the exact report only, never from the partial
        // output, which matches results with Varve's split.
        assert!(privilege_statements(Engine::Mysql, sql, failed(&error)).is_empty());
        // No report, no attribution, or an inconsistent one: nothing.
        let none = DriverError::new(ErrorKind::Query, "boom");
        assert!(statuses(sql, failed(&none)).is_empty());
        let unknown = report(None, Vec::new());
        assert!(statuses(sql, failed(&unknown)).is_empty());
        let odd = report(Some(2), vec![Durability::Committed]);
        assert!(statuses(sql, failed(&odd)).is_empty());
    }

    fn failure(kind: ErrorKind, failed: u32) -> DriverError {
        DriverError::new(kind, "boom").with_partial(varve_driver::PartialOutput {
            results: Vec::new(),
            notices: Vec::new(),
            elapsed_ms: 1,
            failed_statement: Some(failed),
            completed: vec![Durability::Committed; failed as usize],
        })
    }

    fn ended<'a>(
        error: &'a DriverError,
        report: Option<&'a [RanStatement]>,
        server: Option<MysqlServer>,
    ) -> RunEnd<'a> {
        RunEnd {
            succeeded: false,
            error: Some(error),
            in_transaction_before: Some(false),
            in_transaction_after: Some(false),
            ran: report,
            server,
            in_mysql_database: false,
            notices: &[],
            creator_admin: Some(false),
        }
    }

    /// The report entry of `statement`, found in `sql`.
    fn entry(sql: &str, statement: &str) -> RanStatement {
        let offset = sql.find(statement).unwrap_or(usize::MAX);
        ran(
            offset,
            statement,
            StatementKind::Other,
            Durability::Committed,
        )
    }

    fn actions_of(events: &[(&'static str, String)]) -> Vec<&'static str> {
        events.iter().map(|(action, _)| *action).collect()
    }

    /// Audit round 5, High 1: the statement an exactly reported MySQL or
    /// MariaDB script failed at may have changed privileges before it
    /// failed.
    #[test]
    fn the_statement_an_exact_mysql_script_failed_at_is_examined() {
        let mysql = Some(MysqlServer::new(false, 8, 0, 46));
        let mariadb = Some(MysqlServer::new(true, 10, 6, 28));
        let both = [(Engine::Mysql, mysql), (Engine::Mariadb, mariadb)];
        // A GRANT, then a CALL whose procedure grants and then fails.
        let sql = "GRANT SELECT ON d.* TO u; CALL gf()";
        let report = [entry(sql, "GRANT SELECT ON d.* TO u")];
        let error = failure(ErrorKind::Query, 1);
        for (engine, server) in both {
            let (exact, unverified) =
                security_events(engine, sql, ended(&error, Some(&report), server));
            assert_eq!(
                exact,
                [("privilege.granted", "GRANT SELECT ON d.* TO u".to_owned())]
            );
            assert_eq!(unverified, Some(("procedure_call", "CALL gf()".to_owned())));
        }
        // The same through dynamic SQL.
        let sql = "GRANT SELECT ON d.* TO u; PREPARE g FROM 'CALL gf()'; EXECUTE g";
        let report = [
            entry(sql, "GRANT SELECT ON d.* TO u"),
            entry(sql, "PREPARE g FROM 'CALL gf()'"),
        ];
        let error = failure(ErrorKind::Query, 2);
        for (engine, server) in both {
            let (_, unverified) =
                security_events(engine, sql, ended(&error, Some(&report), server));
            assert_eq!(unverified, Some(("dynamic_sql", "EXECUTE g".to_owned())));
        }
        // A MariaDB GRANT or REVOKE naming a missing account applies to the
        // others; a MySQL one is atomic.
        let error = failure(ErrorKind::Query, 0);
        for sql in [
            "GRANT SELECT ON d.* TO a, missing",
            "REVOKE SELECT ON d.* FROM a, missing",
            "CREATE USER a, existing",
            "DROP USER a, missing",
        ] {
            let (_, unverified) =
                security_events(Engine::Mariadb, sql, ended(&error, Some(&[]), mariadb));
            assert_eq!(unverified, Some(("not_confirmed", sql.to_owned())), "{sql}");
            let (_, unverified) =
                security_events(Engine::Mysql, sql, ended(&error, Some(&[]), mysql));
            assert_eq!(unverified, None, "{sql}");
        }
        let sql = "GRANT INSERT ON d.* TO a; GRANT SELECT ON d.* TO a, missing";
        let report = [entry(sql, "GRANT INSERT ON d.* TO a")];
        let error = failure(ErrorKind::Query, 1);
        let (exact, unverified) =
            security_events(Engine::Mariadb, sql, ended(&error, Some(&report), mariadb));
        assert_eq!(actions_of(&exact), ["privilege.granted"]);
        assert_eq!(
            unverified,
            Some((
                "not_confirmed",
                "GRANT SELECT ON d.* TO a, missing".to_owned()
            ))
        );
        // A cancelled statement may have completed, on MySQL too.
        let cancelled = failure(ErrorKind::Cancelled, 0);
        let sql = "GRANT SELECT ON d.* TO a";
        let (_, unverified) =
            security_events(Engine::Mysql, sql, ended(&cancelled, Some(&[]), mysql));
        assert_eq!(unverified, Some(("not_confirmed", sql.to_owned())));
        // An ordinary statement that failed: nothing more.
        let sql = "GRANT SELECT ON d.* TO u; SELECT * FROM missing";
        let report = [entry(sql, "GRANT SELECT ON d.* TO u")];
        let error = failure(ErrorKind::Query, 1);
        for (engine, server) in both {
            let (exact, unverified) =
                security_events(engine, sql, ended(&error, Some(&report), server));
            assert_eq!(actions_of(&exact), ["privilege.granted"]);
            assert_eq!(unverified, None);
        }
    }

    /// Audit round 5, Medium 1: PostgreSQL role statements, default
    /// privileges, embedded grants, ownership changes and extensions.
    #[test]
    fn postgres_role_ownership_and_default_privilege_statements_are_logged() {
        let pg = success(Some(false), Some(false));
        let exact = |sql: &str| actions_of(&security_events(Engine::Postgres, sql, pg).0);
        let doubt = |sql: &str, end: RunEnd<'_>| {
            security_events(Engine::Postgres, sql, end)
                .1
                .map(|(reason, _)| reason)
        };
        let created_member = ["user.created", "privilege.granted"];
        for sql in [
            "CREATE ROLE n IN ROLE g",
            "CREATE ROLE n ROLE u",
            "CREATE ROLE n ADMIN u",
            "CREATE USER n IN GROUP g",
            "CREATE GROUP n USER u",
            "create role \"Admin\" with login admin u",
        ] {
            assert_eq!(exact(sql), created_member, "{sql}");
        }
        for (sql, expected) in [
            ("CREATE ROLE admin", vec!["user.created"]),
            ("CREATE ROLE n SUPERUSER", vec!["user.created"]),
            ("CREATE USER mapping", vec!["user.created"]),
            ("ALTER ROLE u SUPERUSER CREATEROLE", vec!["user.altered"]),
            ("ALTER USER u WITH BYPASSRLS", vec!["user.altered"]),
            ("ALTER ROLE u SET search_path = app", vec!["user.altered"]),
            ("ALTER GROUP g RENAME TO h", vec!["user.altered"]),
            ("DROP ROLE g", vec!["user.dropped"]),
            ("DROP USER IF EXISTS a, b", vec!["user.dropped"]),
            ("DROP GROUP g", vec!["user.dropped"]),
            (
                "ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT SELECT ON TABLES TO u",
                vec!["privilege.granted"],
            ),
            (
                "ALTER DEFAULT PRIVILEGES FOR ROLE varve REVOKE SELECT ON TABLES FROM u",
                vec!["privilege.revoked"],
            ),
            (
                "CREATE SCHEMA s2 CREATE TABLE x (a int) GRANT SELECT ON TABLE x TO u",
                vec!["privilege.granted"],
            ),
            ("DROP OWNED BY u", vec!["privilege.revoked"]),
            ("CREATE USER MAPPING FOR u SERVER s", vec![]),
            ("DROP USER MAPPING IF EXISTS FOR u SERVER s", vec![]),
            (
                "ALTER USER MAPPING FOR u SERVER s OPTIONS (SET password 'x')",
                vec![],
            ),
            ("CREATE SCHEMA s3", vec![]),
        ] {
            assert_eq!(exact(sql), expected, "{sql}");
        }
        for sql in [
            "CREATE USER MAPPING FOR u SERVER s",
            "CREATE SCHEMA s3",
            "ALTER TABLE t ADD COLUMN owner int",
        ] {
            assert_eq!(doubt(sql, pg), None, "{sql}");
        }
        for (sql, reason) in [
            ("ALTER TABLE app.t OWNER TO u", "ownership"),
            ("alter function f() owner to u", "ownership"),
            ("REASSIGN OWNED BY o TO u", "ownership"),
            ("CREATE DATABASE d OWNER u", "ownership"),
            ("CREATE SCHEMA s AUTHORIZATION u", "ownership"),
            ("CREATE EXTENSION pg_stat_statements", "extension"),
            ("ALTER EXTENSION e UPDATE", "extension"),
        ] {
            assert_eq!(doubt(sql, pg), Some(reason), "{sql}");
        }
        // Rolled back: nothing; still open: in doubt.
        for sql in [
            "BEGIN; ALTER TABLE t OWNER TO u; ROLLBACK",
            "BEGIN; CREATE ROLE n IN ROLE g; ROLLBACK",
            "BEGIN; DROP ROLE g; ROLLBACK",
        ] {
            assert_eq!(exact(sql), Vec::<&str>::new(), "{sql}");
            assert_eq!(doubt(sql, pg), None, "{sql}");
        }
        let open = success(Some(false), Some(true));
        assert_eq!(doubt("BEGIN; DROP ROLE g", open), Some("not_confirmed"));
        assert_eq!(
            doubt("BEGIN; ALTER TABLE t OWNER TO u", open),
            Some("ownership")
        );
        // Passwords are "password changed".
        assert!(sets_password(
            "ALTER ROLE u PASSWORD 'x'",
            Dialect::Postgres
        ));
        assert!(sets_password(
            "CREATE ROLE u LOGIN ENCRYPTED PASSWORD 'x'",
            Dialect::Postgres
        ));
        assert!(sets_password(
            "ALTER USER u PASSWORD NULL",
            Dialect::Postgres
        ));
        assert!(!sets_password(
            "ALTER ROLE u VALID UNTIL 'infinity'",
            Dialect::Postgres
        ));
    }

    /// Audit round 5, Medium 2: MySQL and MariaDB account and role
    /// statements, and role settings.
    #[test]
    fn mysql_account_statements_and_role_settings_are_logged() {
        let exact = |engine: Engine, sql: &str| {
            let report = [entry(sql, sql)];
            let end = RunEnd {
                ran: Some(&report),
                ..success(Some(false), Some(false))
            };
            actions_of(&security_events(engine, sql, end).0)
        };
        for (sql, mysql, mariadb) in [
            ("CREATE USER b", vec!["user.created"], vec!["user.created"]),
            (
                "CREATE USER IF NOT EXISTS b DEFAULT ROLE r",
                vec!["user.created", "privilege.granted"],
                vec!["user.created", "privilege.granted"],
            ),
            (
                "CREATE ROLE s",
                vec!["user.created"],
                vec!["user.created", "privilege.granted"],
            ),
            (
                "CREATE ROLE s WITH ADMIN a",
                vec!["user.created"],
                vec!["user.created", "privilege.granted"],
            ),
            (
                "CREATE OR REPLACE USER x",
                vec!["user.created"],
                vec!["user.created"],
            ),
            (
                "SET DEFAULT ROLE r TO b",
                vec!["user.altered"],
                vec!["user.altered"],
            ),
            (
                "SET DEFAULT ROLE r FOR a",
                vec!["user.altered"],
                vec!["user.altered"],
            ),
            (
                "ALTER USER b DEFAULT ROLE r",
                vec!["user.altered"],
                vec!["user.altered"],
            ),
            (
                "RENAME USER b TO c",
                vec!["user.altered"],
                vec!["user.altered"],
            ),
            (
                "SET PASSWORD FOR a = 'x'",
                vec!["user.altered"],
                vec!["user.altered"],
            ),
            ("DROP USER c", vec!["user.dropped"], vec!["user.dropped"]),
            (
                "DROP ROLE IF EXISTS r",
                vec!["user.dropped"],
                vec!["user.dropped"],
            ),
            ("SET ROLE r", vec![], vec![]),
            ("SET @r = 'CREATE USER x'", vec![], vec![]),
        ] {
            assert_eq!(exact(Engine::Mysql, sql), mysql, "{sql}");
            assert_eq!(exact(Engine::Mariadb, sql), mariadb, "{sql}");
        }
        assert!(sets_password("SET PASSWORD FOR a = 'x'", Dialect::Mysql));
        assert!(sets_password(
            "ALTER USER a IDENTIFIED BY 'x'",
            Dialect::Mysql
        ));
        assert!(!sets_password(
            "ALTER USER a PASSWORD EXPIRE",
            Dialect::Mysql
        ));
        // Without an exact report: in doubt.
        let none = success(Some(false), Some(false));
        let doubt = |sql: &str| {
            security_events(Engine::Mysql, sql, none)
                .1
                .map(|(reason, _)| reason)
        };
        assert_eq!(doubt("SET DEFAULT ROLE r TO b"), Some("not_confirmed"));
        assert_eq!(doubt("RENAME USER b TO c"), Some("not_confirmed"));
        // The roles every account gets or activates.
        for sql in [
            "SET GLOBAL mandatory_roles = 'r'",
            "SET PERSIST activate_all_roles_on_login = ON",
            "SET @@GLOBAL.mandatory_roles = 'r'",
            "set persist_only `mandatory_roles` = ''",
            "RESET PERSIST mandatory_roles",
        ] {
            assert_eq!(doubt(sql), Some("role_settings"), "{sql}");
        }
        for sql in ["SET @x = 'mandatory_roles'", "SET SESSION sql_mode = ''"] {
            assert_eq!(doubt(sql), None, "{sql}");
        }
    }

    /// Audit round 5, Medium 3 and Medium 4: grant table writes the
    /// classifier missed, and events that change privileges later.
    #[test]
    fn more_grant_table_writes_and_events_are_in_doubt() {
        let none = success(Some(false), Some(false));
        let doubt = |sql: &str| {
            security_events(Engine::Mysql, sql, none)
                .1
                .map(|(reason, _)| reason)
        };
        for sql in [
            "INSERT INTO mysql.global_grants VALUES ('a', '%', 'BACKUP_ADMIN', 'N')",
            "INSERT INTO mysql.roles_mapping VALUES ('%', 'a', 'r', 'N')",
            "RENAME TABLE mysql.db TO d.db_old, d.dbx TO mysql.db",
            "ALTER TABLE mysql.user ENGINE = InnoDB",
            "DROP TABLE mysql.tables_priv",
            "CREATE OR REPLACE TABLE mysql.db LIKE d.dbx",
            "ANALYZE UPDATE mysql.db SET Select_priv = 'Y'",
            "ANALYZE FORMAT=JSON DELETE FROM mysql.db WHERE User = 'a'",
        ] {
            assert_eq!(doubt(sql), Some("grant_tables"), "{sql}");
        }
        for sql in [
            "ANALYZE TABLE mysql.db",
            "ALTER TABLE app.user ADD COLUMN x int",
            "RENAME TABLE app.a TO app.b",
            "CREATE TABLE t (event int)",
        ] {
            assert_eq!(doubt(sql), None, "{sql}");
        }
        for sql in [
            "CREATE EVENT ev ON SCHEDULE AT CURRENT_TIMESTAMP + INTERVAL 2 SECOND \
             DO GRANT SELECT ON d.* TO a",
            "CREATE DEFINER = `root`@`%` EVENT ev ON SCHEDULE EVERY 1 DAY DO CALL grantp()",
            "CREATE EVENT IF NOT EXISTS ev ON SCHEDULE EVERY 1 DAY DO BEGIN \
             INSERT INTO mysql.db SELECT * FROM t; FLUSH PRIVILEGES; END",
            "CREATE EVENT mysql.ev ON SCHEDULE EVERY 1 DAY DO UPDATE db SET Select_priv = 'Y'",
            "ALTER EVENT ev ON SCHEDULE EVERY 1 HOUR DO DROP USER a",
            "ALTER EVENT ev ENABLE",
            // Round 6, Medium 4: the body is not in the statement.
            "ALTER EVENT ev ON SCHEDULE AT CURRENT_TIMESTAMP + INTERVAL 2 SECOND",
            "ALTER EVENT a6db.ev ON SCHEDULE EVERY 2 SECOND STARTS CURRENT_TIMESTAMP",
            "ALTER EVENT ev RENAME TO ev2",
            // Round 6, Low 1: pending events start to run.
            "SET GLOBAL event_scheduler = ON",
            "SET GLOBAL event_scheduler = 1",
            "SET @@global.event_scheduler := 'ON'",
        ] {
            assert_eq!(doubt(sql), Some("scheduled_event"), "{sql}");
        }
        for sql in [
            "CREATE EVENT ev ON SCHEDULE EVERY 1 DAY DO DELETE FROM app.log",
            "ALTER EVENT ev DISABLE",
            "ALTER EVENT ev ON SCHEDULE EVERY 1 HOUR DISABLE",
            "SET GLOBAL event_scheduler = OFF",
            "SET @event_scheduler = 1",
            // Round 6, Low 5: a column named like a grant table.
            "CREATE EVENT ev ON SCHEDULE EVERY 1 DAY DO \
             INSERT INTO a6db.log (user, db) VALUES (CURRENT_USER(), DATABASE())",
            "CREATE EVENT ev ON SCHEDULE EVERY 1 DAY DO DELETE FROM a6db.sessions WHERE user = 'x'",
            "CREATE EVENT ev ON SCHEDULE EVERY 1 DAY DO UPDATE db SET Select_priv = 'Y'",
        ] {
            assert_eq!(doubt(sql), None, "{sql}");
        }
        // With an exact report (the GRANT in the body makes the driver run
        // the script one statement per request).
        let sql = "CREATE EVENT ev ON SCHEDULE EVERY 1 DAY DO GRANT SELECT ON d.* TO a";
        let report = [entry(sql, sql)];
        let end = RunEnd {
            ran: Some(&report),
            ..none
        };
        assert_eq!(
            security_events(Engine::Mysql, sql, end),
            (Vec::new(), Some(("scheduled_event", sql.to_owned())))
        );
    }

    /// Audit round 5, Low 1: PostgreSQL warnings and membership notices
    /// say a GRANT or REVOKE may have had no effect.
    #[test]
    fn postgres_no_effect_warnings_turn_exact_events_into_doubts() {
        let notice = |severity: &str, message: &str| Notice {
            severity: severity.to_owned(),
            message: message.to_owned(),
        };
        for (sql, said) in [
            (
                "GRANT SELECT ON t TO u",
                notice("WARNING", "no privileges were granted for \"t\""),
            ),
            (
                "REVOKE SELECT ON t FROM u",
                notice("WARNING", "no privileges could be revoked for \"t\""),
            ),
            (
                "REVOKE g FROM u",
                notice(
                    "WARNING",
                    "role \"u\" has not been granted membership in role \"g\" by role \"varve\"",
                ),
            ),
            (
                "ALTER GROUP g DROP USER u",
                notice(
                    "WARNING",
                    "role \"u\" has not been granted membership in role \"g\" by role \"varve\"",
                ),
            ),
            (
                "GRANT g TO u",
                notice(
                    "NOTICE",
                    "role \"u\" has already been granted membership in role \"g\" by role \"varve\"",
                ),
            ),
            (
                "GRANT g TO u",
                notice("NOTICE", "role \"u\" is already a member of role \"g\""),
            ),
        ] {
            let notices = [said];
            let end = RunEnd {
                notices: &notices,
                ..success(Some(false), Some(false))
            };
            assert_eq!(
                security_events(Engine::Postgres, sql, end),
                (Vec::new(), Some(("no_effect", sql.to_owned()))),
                "{sql}"
            );
        }
        // Other notices change nothing; user events stay.
        let notices = [notice("NOTICE", "relation \"t\" already exists, skipping")];
        let end = RunEnd {
            notices: &notices,
            ..success(Some(false), Some(false))
        };
        let sql = "CREATE TABLE IF NOT EXISTS t (a int); GRANT SELECT ON t TO u";
        let (exact, unverified) = security_events(Engine::Postgres, sql, end);
        assert_eq!(actions_of(&exact), ["privilege.granted"]);
        assert_eq!(unverified, None);
        // Round 6, Low 5: a warning about something else, or about
        // another statement, leaves the exact event.
        for (sql, said) in [
            (
                "GRANT SELECT ON t TO u; COMMIT",
                notice("WARNING", "there is no transaction in progress"),
            ),
            (
                "CREATE ROLE n IN ROLE g",
                notice("WARNING", "no privileges were granted for \"t\""),
            ),
            (
                "GRANT SELECT ON t TO u; GRANT SELECT ON v TO u",
                notice("WARNING", "no privileges were granted for \"x\""),
            ),
        ] {
            let notices = [said];
            let end = RunEnd {
                notices: &notices,
                ..success(Some(false), Some(false))
            };
            let (exact, unverified) = security_events(Engine::Postgres, sql, end);
            assert!(
                exact
                    .iter()
                    .any(|(action, _)| *action == "privilege.granted"),
                "{sql}"
            );
            assert_eq!(unverified, None, "{sql}");
        }
    }

    /// Audit round 5, Low 2: a PostgreSQL statement the server rejected
    /// changed nothing; a cancelled one may have.
    #[test]
    fn failed_postgres_privilege_statements_are_not_in_doubt() {
        let rejected = failure(ErrorKind::Query, 0);
        let cancelled = failure(ErrorKind::Cancelled, 0);
        for sql in [
            "GRANT SELECT ON app.missing TO u",
            "ALTER GROUP g ADD USER missing",
            "CREATE ROLE n IN ROLE missing",
            "ALTER TABLE missing OWNER TO u",
        ] {
            assert_eq!(
                security_events(Engine::Postgres, sql, ended(&rejected, None, None)),
                (Vec::new(), None),
                "{sql}"
            );
            assert!(
                security_events(Engine::Postgres, sql, ended(&cancelled, None, None))
                    .1
                    .is_some(),
                "{sql}"
            );
        }
        // A rejected GRANT after a committed one.
        let sql = "GRANT SELECT ON t TO u; GRANT SELECT ON missing TO u";
        let error = failure(ErrorKind::Query, 1);
        let (exact, unverified) = security_events(Engine::Postgres, sql, ended(&error, None, None));
        assert_eq!(actions_of(&exact), ["privilege.granted"]);
        assert_eq!(unverified, None);
    }

    /// Audit round 5, Low 3: privilege words inside string literals are no
    /// code in any quoting mode; code hidden from Varve's default reading
    /// still counts.
    #[test]
    fn literals_without_execute_are_not_privilege_code() {
        let none = success(Some(false), Some(false));
        let doubt = |sql: &str| {
            security_events(Engine::Mysql, sql, none)
                .1
                .map(|(reason, _)| reason)
        };
        for sql in [
            "SELECT 1; SELECT 'GRANT ALL ON *.* TO x'",
            "SET @g = 'GRANT ALL ON *.* TO x'",
            "INSERT INTO notes VALUES ('create user later')",
        ] {
            assert_eq!(doubt(sql), None, "{sql}");
        }
        // Under NO_BACKSLASH_ESCAPES the GRANT is code.
        assert_eq!(
            doubt("SELECT 'a\\'; GRANT SELECT ON d.* TO u; SELECT '"),
            Some("not_attributed")
        );
        // Under ANSI_QUOTES the CALL is code.
        assert_eq!(
            doubt("SELECT \"x\\\"; CALL p(); SELECT \""),
            Some("not_attributed")
        );
    }

    fn reason_of(engine: Engine, sql: &str, end: RunEnd<'_>) -> Option<&'static str> {
        security_events(engine, sql, end)
            .1
            .map(|(reason, _)| reason)
    }

    /// Audit round 6, Medium 3: superuser writes to the PostgreSQL system
    /// catalogs that carry privileges.
    #[test]
    fn postgres_privilege_catalog_writes_are_in_doubt() {
        let pg = success(Some(false), Some(false));
        for sql in [
            "UPDATE pg_catalog.pg_authid SET rolcreatedb = true WHERE rolname = 'a6_u'",
            "UPDATE pg_class SET relacl = array_append(relacl, 'a6_u=r/varve'::aclitem) \
             WHERE oid = 'app.t'::regclass",
            "update pg_catalog.\"pg_auth_members\" set admin_option = true",
            "INSERT INTO pg_catalog.pg_default_acl SELECT * FROM x",
            "DELETE FROM pg_namespace WHERE false",
            "WITH x AS (SELECT 1) UPDATE pg_proc SET proacl = NULL WHERE false",
            "COPY pg_catalog.pg_database FROM STDIN",
        ] {
            assert_eq!(
                reason_of(Engine::Postgres, sql, pg),
                Some("catalog_write"),
                "{sql}"
            );
        }
        for sql in [
            "SELECT relacl FROM pg_class",
            "UPDATE app.t SET a = 1",
            "COPY (SELECT * FROM pg_authid) TO STDOUT",
            "BEGIN; UPDATE pg_authid SET rolcreatedb = true; ROLLBACK",
        ] {
            assert_eq!(reason_of(Engine::Postgres, sql, pg), None, "{sql}");
        }
        let rejected = failure(ErrorKind::Query, 0);
        assert_eq!(
            reason_of(
                Engine::Postgres,
                "UPDATE pg_authid SET rolsuper = true",
                ended(&rejected, None, None)
            ),
            None
        );
    }

    /// Audit round 6, Low 2: automatic routine privileges.
    #[test]
    fn mysql_routine_definitions_are_in_doubt() {
        let none = success(Some(false), Some(false));
        for sql in [
            "CREATE PROCEDURE a6c.p1() SELECT 1",
            "CREATE DEFINER = `root`@`%` FUNCTION f() RETURNS INT DETERMINISTIC RETURN 1",
            "CREATE OR REPLACE PROCEDURE p() BEGIN END",
            "DROP PROCEDURE IF EXISTS a6c.p1",
            "drop function f",
        ] {
            assert_eq!(
                reason_of(Engine::Mysql, sql, none),
                Some("routine_privileges"),
                "{sql}"
            );
        }
        for sql in [
            "CREATE FUNCTION f RETURNS STRING SONAME 'x.so'",
            "CREATE TABLE procedure_log (a int)",
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET NEW.a = 1",
        ] {
            assert_eq!(reason_of(Engine::Mysql, sql, none), None, "{sql}");
        }
    }

    /// Audit round 6, Low 3: on PostgreSQL 16 and later a non-superuser's
    /// CREATE ROLE grants the new role to its creator.
    #[test]
    fn postgres_creator_admin_membership_is_logged() {
        let pg = |creator_admin| RunEnd {
            creator_admin,
            ..success(Some(false), Some(false))
        };
        let sql = "CREATE ROLE a6_n1 LOGIN";
        let (exact, unverified) = security_events(Engine::Postgres, sql, pg(Some(true)));
        assert_eq!(actions_of(&exact), ["user.created", "privilege.granted"]);
        assert_eq!(unverified, None);
        let (exact, _) =
            security_events(Engine::Postgres, "CREATE ROLE n IN ROLE g", pg(Some(true)));
        assert_eq!(actions_of(&exact), ["user.created", "privilege.granted"]);
        let (exact, unverified) = security_events(Engine::Postgres, sql, pg(Some(false)));
        assert_eq!(actions_of(&exact), ["user.created"]);
        assert_eq!(unverified, None);
        let (exact, unverified) = security_events(Engine::Postgres, sql, pg(None));
        assert_eq!(actions_of(&exact), ["user.created"]);
        assert_eq!(unverified, Some(("not_confirmed", sql.to_owned())));
        assert_eq!(
            security_events(
                Engine::Postgres,
                "CREATE USER MAPPING FOR u SERVER s",
                pg(Some(true))
            ),
            (Vec::new(), None)
        );
    }

    /// Audit round 6, Low 4: MySQL and MariaDB warnings that an IF [NOT]
    /// EXISTS or IGNORE UNKNOWN USER statement skipped something.
    #[test]
    fn mysql_skip_warnings_turn_exact_privilege_events_into_doubts() {
        let warn = |code: u32| Notice {
            severity: "Note".to_owned(),
            message: format!("{code}: skipped"),
        };
        let run = |engine: Engine, sql: &str, notices: &[Notice]| {
            let report = [entry(sql, sql)];
            let end = RunEnd {
                ran: Some(&report),
                notices,
                ..success(Some(false), Some(false))
            };
            let (exact, unverified) = security_events(engine, sql, end);
            (actions_of(&exact), unverified.map(|(reason, _)| reason))
        };
        assert_eq!(
            run(
                Engine::Mysql,
                "CREATE USER IF NOT EXISTS a6_u DEFAULT ROLE a6_r",
                &[warn(3163)]
            ),
            (vec!["user.created"], Some("no_effect"))
        );
        assert_eq!(
            run(
                Engine::Mariadb,
                "CREATE ROLE IF NOT EXISTS a6_r",
                &[warn(1975)]
            ),
            (vec!["user.created"], Some("no_effect"))
        );
        for sql in [
            "REVOKE IF EXISTS SELECT ON a6db.* FROM a6_u",
            "REVOKE SELECT ON a6db.* FROM a6_missing IGNORE UNKNOWN USER",
        ] {
            for code in [1141, 3162] {
                assert_eq!(
                    run(Engine::Mysql, sql, &[warn(code)]),
                    (vec![], Some("no_effect")),
                    "{sql}"
                );
            }
        }
        // Without such a clause, or without the warning: exact.
        assert_eq!(
            run(
                Engine::Mysql,
                "REVOKE SELECT ON a6db.* FROM a6_u",
                &[warn(1141)]
            ),
            (vec!["privilege.revoked"], None)
        );
        assert_eq!(
            run(
                Engine::Mysql,
                "REVOKE IF EXISTS SELECT ON a6db.* FROM a6_u",
                &[warn(1287)]
            ),
            (vec!["privilege.revoked"], None)
        );
        // PostgreSQL: only the statement the warning names.
        let notices = [Notice {
            severity: "WARNING".to_owned(),
            message: "no privileges were granted for \"v\"".to_owned(),
        }];
        let end = RunEnd {
            notices: &notices,
            ..success(Some(false), Some(false))
        };
        let (exact, unverified) = security_events(
            Engine::Postgres,
            "GRANT SELECT ON t TO u; GRANT SELECT ON v TO u",
            end,
        );
        assert_eq!(
            exact,
            [("privilege.granted", "GRANT SELECT ON t TO u".to_owned())]
        );
        assert_eq!(
            unverified,
            Some(("no_effect", "GRANT SELECT ON v TO u".to_owned()))
        );
    }

    /// Audit round 6, Low 5 and Low 6: no false doubts for a renamed
    /// column, user variables, reading a setting, or a failing statement
    /// about one account; MariaDB CREATE OR REPLACE is exact.
    #[test]
    fn round_six_noise_is_gone() {
        let pg = success(Some(false), Some(false));
        for sql in [
            "ALTER TABLE app.z RENAME COLUMN owner TO o2",
            "ALTER TABLE app.z RENAME owner TO o2",
        ] {
            assert_eq!(reason_of(Engine::Postgres, sql, pg), None, "{sql}");
        }
        assert_eq!(
            reason_of(Engine::Postgres, "ALTER TABLE app.z OWNER TO o2", pg),
            Some("ownership")
        );
        let none = success(Some(false), Some(false));
        for sql in [
            "SET @mandatory_roles = 1",
            "SET @activate_all_roles_on_login = 1",
            "SELECT @@global.mandatory_roles",
            "SELECT 1; SELECT @@activate_all_roles_on_login",
            "REPLACE user VALUES (1)",
        ] {
            assert_eq!(reason_of(Engine::Mysql, sql, none), None, "{sql}");
        }
        assert_eq!(
            reason_of(Engine::Mysql, "SET @@mandatory_roles = 'r'", none),
            Some("role_settings")
        );
        // A MariaDB statement about one account that failed applied
        // nothing.
        let mariadb = Some(MysqlServer::new(true, 10, 6, 28));
        let error = failure(ErrorKind::Query, 0);
        assert_eq!(
            reason_of(
                Engine::Mariadb,
                "SET PASSWORD FOR a6_u = 'plain'",
                ended(&error, Some(&[]), mariadb)
            ),
            None
        );
        // MariaDB CREATE OR REPLACE with the exact report.
        for (sql, expected) in [
            ("CREATE OR REPLACE USER a6_u", vec!["user.created"]),
            (
                "CREATE OR REPLACE ROLE a6_r",
                vec!["user.created", "privilege.granted"],
            ),
        ] {
            let report = [entry(sql, sql)];
            let end = RunEnd {
                ran: Some(&report),
                server: mariadb,
                ..success(Some(false), Some(false))
            };
            let (exact, unverified) = security_events(Engine::Mariadb, sql, end);
            assert_eq!(actions_of(&exact), expected, "{sql}");
            assert_eq!(unverified, None, "{sql}");
        }
    }

    /// Audit round 7, Low 2: scripts that may switch the current role.
    #[test]
    fn role_switches_are_recognised() {
        for sql in [
            "SET ROLE a6_l",
            "set session role a6_l",
            "SET LOCAL ROLE a6_l",
            "RESET ROLE",
            "SET SESSION AUTHORIZATION a6_l",
            "SET LOCAL SESSION AUTHORIZATION a6_l",
            "RESET SESSION AUTHORIZATION",
            "SELECT 1; ALTER ROLE varve NOSUPERUSER",
            "DO $$ BEGIN END $$",
            "CALL p()",
        ] {
            assert!(may_switch_role(sql), "{sql}");
        }
        for sql in [
            "SET search_path = a",
            "SELECT 'SET ROLE x'",
            "ALTER ROLE u CREATEDB",
            "CREATE ROLE n",
        ] {
            assert!(!may_switch_role(sql), "{sql}");
        }
        assert_eq!(creator_admin_for(Engine::Postgres, 18, None), None);
        assert_eq!(
            creator_admin_for(Engine::Postgres, 18, Some(false)),
            Some(true)
        );
        assert_eq!(creator_admin_for(Engine::Postgres, 13, None), Some(false));
    }

    /// Audit round 7, Low 3: MySQL warnings belong to the statement that
    /// produced them.
    #[test]
    fn mysql_skip_warnings_are_tied_to_their_statement() {
        let sql = "CREATE USER IF NOT EXISTS a DEFAULT ROLE r; DROP USER IF EXISTS m";
        let report = [
            entry(sql, "CREATE USER IF NOT EXISTS a DEFAULT ROLE r"),
            entry(sql, "DROP USER IF EXISTS m"),
        ];
        let run = |message: &str| {
            let notices = [Notice {
                severity: "Note".to_owned(),
                message: message.to_owned(),
            }];
            let end = RunEnd {
                ran: Some(&report),
                notices: &notices,
                ..success(Some(false), Some(false))
            };
            let (exact, doubt) = security_events_all(Engine::Mysql, sql, end);
            (actions_of(&exact), doubt.map(|(reasons, _)| reasons))
        };
        // The DROP's warning: the CREATE stays exact.
        assert_eq!(
            run("3162: Authorization ID 'm'@'%' does not exist."),
            (
                vec!["user.created", "privilege.granted", "user.dropped"],
                None
            )
        );
        // A note that the first statement raised warnings: it may have
        // skipped.
        assert_eq!(
            run(
                "Statement 1 raised 1 warning(s). The server keeps details only for the last \
                 statement of a script."
            ),
            (
                vec!["user.created", "user.dropped"],
                Some(vec!["no_effect"])
            )
        );
    }

    /// Audit round 7, Low 4: every reason is kept, the most telling first.
    #[test]
    fn every_doubt_reason_is_kept_in_priority() {
        let mysql = Some(MysqlServer::new(false, 8, 0, 46));
        let sql = "GRANT SELECT ON d.* TO u; CREATE PROCEDURE d.tmpp() SELECT 1; CALL d.gf()";
        let report = [
            entry(sql, "GRANT SELECT ON d.* TO u"),
            entry(sql, "CREATE PROCEDURE d.tmpp() SELECT 1"),
        ];
        let error = failure(ErrorKind::Query, 2);
        let (_, doubt) =
            security_events_all(Engine::Mysql, sql, ended(&error, Some(&report), mysql));
        assert_eq!(
            doubt.map(|(reasons, _)| reasons),
            Some(vec!["procedure_call", "routine_privileges"])
        );
        assert_eq!(
            by_priority(["not_attributed", "no_effect", "dynamic_sql", "no_effect"].into_iter()),
            ["dynamic_sql", "no_effect", "not_attributed"]
        );
    }

    /// Audit round 8, Lows 1, 3 and 4: a login that failed never ran the
    /// init SQL; PostgreSQL password literal forms; a MariaDB account
    /// statement with a syntax error applied nothing.
    #[test]
    fn round_eight_lows() {
        use varve_driver::ErrorKind::{Auth, Connect, ConnectionLost, Query, Timeout, Tls};
        for kind in [Auth, Connect, Tls] {
            assert!(!init_sql_may_have_run(Some(&DriverError::new(
                kind,
                "password authentication failed"
            ))));
        }
        assert!(init_sql_may_have_run(Some(&DriverError::new(
            Query,
            "init SQL failed: relation does not exist"
        ))));
        for kind in [Timeout, ConnectionLost] {
            assert!(init_sql_may_have_run(Some(&DriverError::new(kind, "x"))));
        }
        for sql in [
            "ALTER ROLE u PASSWORD E'Secret\\'1'",
            "ALTER ROLE u PASSWORD U&'Secret'",
            "ALTER ROLE u PASSWORD $$Secret$$",
            "ALTER ROLE u PASSWORD $p$Secret$p$",
            "CREATE ROLE u LOGIN PASSWORD e'x'",
        ] {
            assert!(sets_password(sql, Dialect::Postgres), "{sql}");
        }
        assert!(!sets_password(
            "ALTER ROLE u VALID UNTIL 'x'",
            Dialect::Postgres
        ));
        let mariadb = Some(MysqlServer::new(true, 10, 6, 28));
        let sql = "GRANT SELECT ON d.* TO a, b WITH BOGUS";
        let syntax = failure(ErrorKind::Query, 0).with_code("1064 (42000)");
        assert_eq!(
            reason_of(Engine::Mariadb, sql, ended(&syntax, Some(&[]), mariadb)),
            None
        );
        let missing = failure(ErrorKind::Query, 0).with_code("1133 (28000)");
        assert_eq!(
            reason_of(Engine::Mariadb, sql, ended(&missing, Some(&[]), mariadb)),
            Some("not_confirmed")
        );
    }

    #[test]
    fn kind_labels() {
        assert_eq!(kind_label(&[]), "command");
        assert_eq!(kind_label(&[StatementKind::Select]), "select");
        assert_eq!(
            kind_label(&[
                StatementKind::Select,
                StatementKind::Select,
                StatementKind::Drop
            ]),
            "script:select,drop"
        );
    }
}
