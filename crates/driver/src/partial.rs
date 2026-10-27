//! What a failed script produced before it failed, and what became of the
//! statements that completed (design doc 4.1).
//!
//! A script that fails at statement N still ran statements 0 to N-1: their
//! results, notices and changes are real. [`Session::execute`] keeps the
//! error path an `Err`, so no caller can take a failed script for a
//! successful one, and the error carries this [`PartialOutput`] in
//! [`DriverError::partial`].
//!
//! Drivers learn per statement whether a transaction was open after it (the
//! PostgreSQL `ReadyForQuery` status, the MySQL `SERVER_STATUS_IN_TRANS`
//! flag, the Redis `MULTI` state) and feed that, with what the statement
//! does to the transaction ([`transaction_effect`]), to a
//! [`TransactionTracker`]. The tracker decides for every completed statement
//! whether its changes were committed, rolled back, are still in the open
//! transaction, or cannot be known. It only says committed when that is
//! certain.
//!
//! [`Session::execute`]: crate::Session::execute
//! [`DriverError::partial`]: crate::DriverError::partial

use serde::{Deserialize, Serialize};

use crate::classify::{Dialect, LexKind, Lexeme, MysqlMode, lex, lex_mysql};
use crate::{Notice, StatementResult};

/// What became of the changes of a statement that completed before its
/// script failed, as far as the driver can tell when the call returns.
///
/// Transaction control statements count as [`Durability::Committed`] once
/// they took effect (a `ROLLBACK` that ran is committed; the statements it
/// rolled back are [`Durability::RolledBack`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Its changes are committed: it ran in autocommit mode, or the
    /// transaction it ran in was committed (explicitly or, in MySQL and
    /// MariaDB, implicitly by a later statement).
    Committed,
    /// Its changes were undone: by a `ROLLBACK` (or `ROLLBACK TO SAVEPOINT`)
    /// later in the script, by the failure itself (a failed PostgreSQL
    /// `COMMIT`), by a Redis `DISCARD` or a `MULTI` block that was dropped.
    RolledBack,
    /// It ran in the transaction that is still open after the call: its
    /// changes are not committed yet. In PostgreSQL the failure may have
    /// aborted that transaction, which can then only be rolled back.
    InTransaction,
    /// It ran but failed, so it changed nothing: a Redis command inside an
    /// `EXEC` whose reply element is an error.
    Failed,
    /// The driver cannot tell: the transaction status was not reported, or
    /// a statement whose transaction handling is not visible in its text (a
    /// procedure call, prepared or dynamic SQL, `XA`, `PREPARE TRANSACTION`)
    /// may have committed or rolled back.
    Unknown,
}

/// Everything a failed script produced before it failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialOutput {
    /// Results in the order the server sent them: those of the completed
    /// statements, then any the failing statement sent before it failed (a
    /// MySQL procedure's result sets).
    pub results: Vec<StatementResult>,
    /// Notices and warnings the server sent before the failure. The
    /// driver's own warnings stay in [`crate::DriverError::detail`].
    pub notices: Vec<Notice>,
    /// Wall time on the client until the failure, in milliseconds.
    pub elapsed_ms: u64,
    /// 0-based index of the first statement that did not complete,
    /// counted the way [`crate::split_statements`] splits the script
    /// (Redis: the way the console splits a script into commands, one per
    /// line). Normally the statement that failed: every statement before it
    /// completed, none after it ran. When the failure came after a
    /// statement completed (its reply could not be shown, or a check the
    /// driver runs between statements or after the script failed), it is
    /// the index after that statement, and equals the number of statements
    /// when all of them completed. `None` when
    /// the driver cannot attribute the server's results to statements (a
    /// MySQL script whose statement boundaries depend on the session's
    /// `sql_mode`, or whose `EXECUTE` returned rows that may come from a
    /// procedure); [`PartialOutput::completed`] is then empty.
    pub failed_statement: Option<u32>,
    /// What became of each completed statement, one entry per statement
    /// before [`PartialOutput::failed_statement`], in order.
    pub completed: Vec<Durability>,
}

impl PartialOutput {
    /// True when there is something to report: a completed statement, a
    /// result or a notice.
    #[must_use]
    pub fn has_content(&self) -> bool {
        !self.completed.is_empty() || !self.results.is_empty() || !self.notices.is_empty()
    }
}

/// A statement the server ran in a call, known exactly: the driver sent it
/// as a request of its own, so the server ran exactly this text (see
/// [`crate::Session::last_statements`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RanStatement {
    /// Byte offset of the statement in the script.
    pub offset: usize,
    /// Byte length of the statement text (trimmed, without its `;`).
    pub len: usize,
    /// What the statement is.
    pub kind: crate::StatementKind,
    /// What became of its changes when the call returned.
    pub durability: Durability,
}

/// What a completed statement did to the transaction, from its text or (in
/// Redis) its command and reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionEffect {
    /// Any statement that does not control the transaction itself. Whether
    /// it committed follows from the transaction status after it: none
    /// open means autocommit, or (MySQL, MariaDB) an implicit commit of
    /// everything before it.
    Statement,
    /// `BEGIN`, `START TRANSACTION`, Redis `MULTI`.
    Begin {
        /// Starting a transaction commits the open one first (MySQL,
        /// MariaDB). PostgreSQL only warns and keeps it.
        commits_open: bool,
    },
    /// `COMMIT`, `END`, Redis `EXEC` that ran the queued commands.
    Commit {
        /// `AND CHAIN`: a new transaction starts at once.
        chain: bool,
    },
    /// `ROLLBACK`, `ABORT`, a PostgreSQL `COMMIT` of an aborted
    /// transaction (its tag is `ROLLBACK`), Redis `DISCARD` or an `EXEC`
    /// aborted by `WATCH`.
    Rollback {
        /// `AND CHAIN`: a new transaction starts at once.
        chain: bool,
    },
    /// `SAVEPOINT name`.
    Savepoint {
        /// Normalized name (see [`transaction_effect`]).
        name: String,
        /// A savepoint of the same name is replaced (MySQL, MariaDB); in
        /// PostgreSQL the new one only hides the old one.
        replaces: bool,
    },
    /// `ROLLBACK TO [SAVEPOINT] name`.
    RollbackTo {
        /// Normalized name.
        name: String,
    },
    /// `RELEASE [SAVEPOINT] name`.
    Release {
        /// Normalized name.
        name: String,
    },
    /// A statement that may commit or roll back in ways its text does not
    /// show: `CALL`, `DO` (PostgreSQL, outside a transaction block),
    /// `EXECUTE` (MySQL, MariaDB), a MariaDB anonymous block, `XA`,
    /// `PREPARE TRANSACTION`.
    Opaque,
}

/// A word or a quoted identifier of a statement, comments and literals
/// left out.
enum Token {
    Word(String),
    Quoted(String),
    Colon,
    Other,
}

/// The tokens of `lexemes`; the markers of executable comments that run
/// are left out, so their contents read as code.
fn tokens(statement: &str, lexemes: Vec<Lexeme>) -> Vec<Token> {
    lexemes
        .into_iter()
        .filter(|lexeme| {
            !matches!(
                lexeme.kind,
                LexKind::Comment | LexKind::ExecutableCommentStart | LexKind::ExecutableCommentEnd
            )
        })
        .map(|lexeme| {
            let text = statement.get(lexeme.start..lexeme.end).unwrap_or_default();
            match lexeme.kind {
                LexKind::Word => Token::Word(text.to_ascii_uppercase()),
                LexKind::QuotedIdent | LexKind::DoubleQuoted => Token::Quoted(unquote(text)),
                LexKind::Punct(':') => Token::Colon,
                _ => Token::Other,
            }
        })
        .collect()
}

/// The name inside a quoted identifier: the outer quote characters removed
/// and doubled ones collapsed.
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

/// What `statement` (one statement of a script, as split by
/// [`crate::split_statements`]) does to the transaction.
///
/// Savepoint names are normalized the way the engine compares them:
/// PostgreSQL folds unquoted names to lower case and keeps quoted ones,
/// MySQL and MariaDB compare them case-insensitively.
///
/// A MySQL statement with an executable comment (`/*! */`, `/*M! */`) is
/// [`TransactionEffect::Opaque`] here, because whether its contents run
/// depends on the server: use [`transaction_effect_mysql`] with the server.
#[must_use]
pub fn transaction_effect(statement: &str, dialect: Dialect) -> TransactionEffect {
    let lexemes = lex(statement, dialect, true);
    if dialect == Dialect::Mysql
        && lexemes
            .iter()
            .any(|l| l.kind == LexKind::ExecutableCommentStart)
    {
        return TransactionEffect::Opaque;
    }
    effect_of(dialect, &tokens(statement, lexemes))
}

/// Like [`transaction_effect`] for a MySQL or MariaDB statement read as a
/// session in `mode` does: an executable comment that runs on the server is
/// code (`/*!ROLLBACK*/` rolls back), one that does not is a comment. Without
/// a server in `mode` a statement with an executable comment is
/// [`TransactionEffect::Opaque`].
#[must_use]
pub fn transaction_effect_mysql(statement: &str, mode: &MysqlMode) -> TransactionEffect {
    let lexemes = lex_mysql(statement, mode);
    if mode.server.is_none()
        && lexemes
            .iter()
            .any(|l| l.kind == LexKind::ExecutableCommentStart)
    {
        return TransactionEffect::Opaque;
    }
    effect_of(Dialect::Mysql, &tokens(statement, lexemes))
}

fn effect_of(dialect: Dialect, tokens: &[Token]) -> TransactionEffect {
    let word = |i: usize| match tokens.get(i) {
        Some(Token::Word(w)) => Some(w.as_str()),
        _ => None,
    };
    let name_at = |i: usize| -> Option<String> {
        match tokens.get(i)? {
            Token::Word(w) => Some(w.to_ascii_lowercase()),
            Token::Quoted(q) if dialect == Dialect::Postgres => Some(q.clone()),
            Token::Quoted(q) => Some(q.to_lowercase()),
            Token::Colon | Token::Other => None,
        }
    };
    let mysql = dialect == Dialect::Mysql;
    // A MySQL or MariaDB statement label (`name: BEGIN NOT ATOMIC ...`) or
    // a compound statement outside a routine.
    if mysql
        && (matches!(
            (tokens.first(), tokens.get(1)),
            (Some(Token::Word(_) | Token::Quoted(_)), Some(Token::Colon))
        ) || matches!(
            tokens.first(),
            Some(Token::Word(w)) if matches!(w.as_str(), "IF" | "CASE" | "LOOP" | "WHILE" | "REPEAT" | "FOR")
        ))
    {
        return TransactionEffect::Opaque;
    }
    // Skips the optional WORK / TRANSACTION noise word after a verb.
    let after_noise = |i: usize| match word(i) {
        Some("WORK") => i + 1,
        Some("TRANSACTION") if !mysql => i + 1,
        _ => i,
    };
    let chain = |from: usize| {
        (from..tokens.len()).any(|i| word(i) == Some("AND") && word(i + 1) == Some("CHAIN"))
    };
    match word(0) {
        Some("BEGIN") => {
            if mysql && word(1) == Some("NOT") && word(2) == Some("ATOMIC") {
                TransactionEffect::Opaque
            } else {
                TransactionEffect::Begin {
                    commits_open: mysql,
                }
            }
        }
        Some("START") if word(1) == Some("TRANSACTION") => TransactionEffect::Begin {
            commits_open: mysql,
        },
        Some("COMMIT") if word(1) == Some("PREPARED") => TransactionEffect::Statement,
        Some("ROLLBACK") if word(1) == Some("PREPARED") => TransactionEffect::Statement,
        Some("COMMIT") => TransactionEffect::Commit { chain: chain(1) },
        Some("END") if !mysql => TransactionEffect::Commit { chain: chain(1) },
        Some(verb @ ("ROLLBACK" | "ABORT")) => {
            let at = after_noise(1);
            if verb == "ROLLBACK" && word(at) == Some("TO") {
                let at = if word(at + 1) == Some("SAVEPOINT") {
                    at + 2
                } else {
                    at + 1
                };
                match name_at(at) {
                    Some(name) => TransactionEffect::RollbackTo { name },
                    None => TransactionEffect::Opaque,
                }
            } else {
                TransactionEffect::Rollback { chain: chain(1) }
            }
        }
        Some("SAVEPOINT") => match name_at(1) {
            Some(name) => TransactionEffect::Savepoint {
                name,
                replaces: mysql,
            },
            None => TransactionEffect::Opaque,
        },
        Some("RELEASE") => {
            let at = if word(1) == Some("SAVEPOINT") { 2 } else { 1 };
            match name_at(at) {
                Some(name) => TransactionEffect::Release { name },
                None => TransactionEffect::Opaque,
            }
        }
        Some("PREPARE") if !mysql && word(1) == Some("TRANSACTION") => TransactionEffect::Opaque,
        Some("CALL") => TransactionEffect::Opaque,
        Some("DO") if !mysql => TransactionEffect::Opaque,
        Some("EXECUTE" | "XA") if mysql => TransactionEffect::Opaque,
        _ => TransactionEffect::Statement,
    }
}

/// Decides, statement by statement, what became of the changes of the
/// statements of one call. See the module documentation.
#[derive(Debug, Default, Clone)]
pub struct TransactionTracker {
    /// One entry per completed statement; entries in `pending` are not
    /// decided yet.
    states: Vec<Durability>,
    /// Completed statements in the open transaction, in order.
    pending: Vec<usize>,
    /// Savepoints set in this call: name, and how many entries `pending`
    /// had right after the `SAVEPOINT` statement (statements after it are
    /// `pending[mark..]`).
    savepoints: Vec<(String, usize)>,
}

impl TransactionTracker {
    /// A tracker for one call.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many statements completed so far.
    #[must_use]
    pub fn completed_count(&self) -> usize {
        self.states.len()
    }

    /// Records the next statement as completed: what it did to the
    /// transaction, and whether a transaction was open right after it
    /// (`None` when the driver does not know).
    pub fn completed(&mut self, effect: TransactionEffect, in_transaction_after: Option<bool>) {
        let index = self.states.len();
        self.states.push(Durability::Unknown);
        match effect {
            TransactionEffect::Statement => self.place(index, in_transaction_after),
            TransactionEffect::Begin { commits_open } => {
                if commits_open {
                    self.end_transaction(Durability::Committed);
                }
                self.place(index, in_transaction_after);
            }
            TransactionEffect::Commit { .. } => {
                self.end_transaction(Durability::Committed);
                self.set(index, Durability::Committed);
            }
            TransactionEffect::Rollback { .. } => {
                self.end_transaction(Durability::RolledBack);
                self.set(index, Durability::Committed);
            }
            TransactionEffect::Opaque => {
                self.end_transaction(Durability::Unknown);
                self.set(index, Durability::Unknown);
            }
            TransactionEffect::Savepoint { name, replaces } => {
                self.place(index, in_transaction_after);
                if in_transaction_after == Some(true) {
                    if replaces {
                        self.savepoints.retain(|(existing, _)| *existing != name);
                    }
                    self.savepoints.push((name, self.pending.len()));
                }
            }
            TransactionEffect::RollbackTo { name } => {
                match self.savepoints.iter().rposition(|(n, _)| *n == name) {
                    Some(at) => {
                        let mark = self.savepoints.get(at).map_or(0, |(_, mark)| *mark);
                        self.savepoints.truncate(at + 1);
                        self.resolve_from(mark, Durability::RolledBack);
                    }
                    // Set before this call: everything of this call came
                    // after it.
                    None => {
                        self.savepoints.clear();
                        self.resolve_from(0, Durability::RolledBack);
                    }
                }
                self.place(index, in_transaction_after);
            }
            TransactionEffect::Release { name } => {
                match self.savepoints.iter().rposition(|(n, _)| *n == name) {
                    Some(at) => self.savepoints.truncate(at),
                    None => self.savepoints.clear(),
                }
                self.place(index, in_transaction_after);
            }
        }
    }

    /// The decisions for every completed statement, after the call failed.
    /// `in_transaction_after` is the transaction status after the failure;
    /// `ended` says what an end of the transaction during the failed
    /// statement means: [`Durability::RolledBack`] in PostgreSQL (a failed
    /// `COMMIT` rolls back) and Redis (a dropped or aborted `MULTI`),
    /// [`Durability::Unknown`] in MySQL and MariaDB (a deadlock rolls back,
    /// a failing DDL statement commits first).
    #[must_use]
    pub fn finish(
        mut self,
        in_transaction_after: Option<bool>,
        ended: Durability,
    ) -> Vec<Durability> {
        let open = match in_transaction_after {
            Some(true) => Durability::InTransaction,
            Some(false) => ended,
            None => Durability::Unknown,
        };
        self.resolve_from(0, open);
        self.states
    }

    fn set(&mut self, index: usize, durability: Durability) {
        if let Some(state) = self.states.get_mut(index) {
            *state = durability;
        }
    }

    /// Puts statement `index` in the open transaction, or decides it (and
    /// everything pending) when none is open after it.
    fn place(&mut self, index: usize, in_transaction_after: Option<bool>) {
        match in_transaction_after {
            Some(true) => self.pending.push(index),
            Some(false) => {
                self.end_transaction(Durability::Committed);
                self.set(index, Durability::Committed);
            }
            None => {
                self.end_transaction(Durability::Unknown);
                self.set(index, Durability::Unknown);
            }
        }
    }

    fn end_transaction(&mut self, durability: Durability) {
        self.savepoints.clear();
        self.resolve_from(0, durability);
    }

    /// Decides `pending[from..]`.
    fn resolve_from(&mut self, from: usize, durability: Durability) {
        let from = from.min(self.pending.len());
        for index in self.pending.split_off(from) {
            self.set(index, durability);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Durability::{Committed, InTransaction, RolledBack, Unknown};
    use super::*;

    fn effect_pg(sql: &str) -> TransactionEffect {
        transaction_effect(sql, Dialect::Postgres)
    }

    fn effect_my(sql: &str) -> TransactionEffect {
        transaction_effect(sql, Dialect::Mysql)
    }

    #[test]
    fn reads_transaction_control_from_postgres_text() {
        assert_eq!(
            effect_pg("begin"),
            TransactionEffect::Begin {
                commits_open: false
            }
        );
        assert_eq!(
            effect_pg("START TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
            TransactionEffect::Begin {
                commits_open: false
            }
        );
        assert_eq!(
            effect_pg("-- done\nCOMMIT"),
            TransactionEffect::Commit { chain: false }
        );
        assert_eq!(
            effect_pg("end work"),
            TransactionEffect::Commit { chain: false }
        );
        assert_eq!(
            effect_pg("COMMIT AND CHAIN"),
            TransactionEffect::Commit { chain: true }
        );
        assert_eq!(
            effect_pg("commit and no chain"),
            TransactionEffect::Commit { chain: false }
        );
        assert_eq!(
            effect_pg("ABORT"),
            TransactionEffect::Rollback { chain: false }
        );
        assert_eq!(
            effect_pg("ROLLBACK TRANSACTION TO SAVEPOINT \"Sp\""),
            TransactionEffect::RollbackTo { name: "Sp".into() }
        );
        assert_eq!(
            effect_pg("rollback to Sp"),
            TransactionEffect::RollbackTo { name: "sp".into() }
        );
        assert_eq!(
            effect_pg("SAVEPOINT a"),
            TransactionEffect::Savepoint {
                name: "a".into(),
                replaces: false
            }
        );
        assert_eq!(
            effect_pg("RELEASE a"),
            TransactionEffect::Release { name: "a".into() }
        );
        assert_eq!(
            effect_pg("COMMIT PREPARED 'x'"),
            TransactionEffect::Statement
        );
        assert_eq!(
            effect_pg("ROLLBACK PREPARED 'x'"),
            TransactionEffect::Statement
        );
        assert_eq!(
            effect_pg("PREPARE TRANSACTION 'x'"),
            TransactionEffect::Opaque
        );
        assert_eq!(effect_pg("CALL p()"), TransactionEffect::Opaque);
        assert_eq!(
            effect_pg("DO $$ BEGIN COMMIT; END $$"),
            TransactionEffect::Opaque
        );
        assert_eq!(effect_pg("EXECUTE s"), TransactionEffect::Statement);
        assert_eq!(
            effect_pg("GRANT SELECT ON t TO u"),
            TransactionEffect::Statement
        );
    }

    #[test]
    fn reads_transaction_control_from_mysql_text() {
        assert_eq!(
            effect_my("BEGIN WORK"),
            TransactionEffect::Begin { commits_open: true }
        );
        assert_eq!(
            effect_my("BEGIN NOT ATOMIC SELECT 1; END"),
            TransactionEffect::Opaque
        );
        assert_eq!(
            effect_my("blk: BEGIN NOT ATOMIC SELECT 1; END"),
            TransactionEffect::Opaque
        );
        assert_eq!(
            effect_my("start transaction read write"),
            TransactionEffect::Begin { commits_open: true }
        );
        assert_eq!(
            effect_my("COMMIT WORK AND CHAIN NO RELEASE"),
            TransactionEffect::Commit { chain: true }
        );
        assert_eq!(
            effect_my("ROLLBACK WORK TO SAVEPOINT `Sp`"),
            TransactionEffect::RollbackTo { name: "sp".into() }
        );
        assert_eq!(
            effect_my("SAVEPOINT Sp"),
            TransactionEffect::Savepoint {
                name: "sp".into(),
                replaces: true
            }
        );
        assert_eq!(
            effect_my("RELEASE SAVEPOINT sp"),
            TransactionEffect::Release { name: "sp".into() }
        );
        assert_eq!(effect_my("XA START 'x'"), TransactionEffect::Opaque);
        assert_eq!(effect_my("EXECUTE s USING @a"), TransactionEffect::Opaque);
        assert_eq!(effect_my("CALL p()"), TransactionEffect::Opaque);
        assert_eq!(effect_my("DO SLEEP(1)"), TransactionEffect::Statement);
        assert_eq!(effect_my("END"), TransactionEffect::Statement);
    }

    #[test]
    fn executable_comments_are_read_as_the_server_reads_them() {
        use crate::classify::MysqlServer;
        // Audit probes P-M3, M3b, M4b: without the server, never a guess.
        for sql in ["/*!ROLLBACK*/", "/*!50000 ROLLBACK */", "/*M!ROLLBACK*/"] {
            assert_eq!(effect_my(sql), TransactionEffect::Opaque, "{sql}");
        }
        let mysql = MysqlMode {
            server: Some(MysqlServer::new(false, 8, 0, 36)),
            ..MysqlMode::default()
        };
        let mariadb = MysqlMode {
            server: Some(MysqlServer::new(true, 10, 6, 11)),
            ..MysqlMode::default()
        };
        let rollback = TransactionEffect::Rollback { chain: false };
        assert_eq!(transaction_effect_mysql("/*!ROLLBACK*/", &mysql), rollback);
        assert_eq!(
            transaction_effect_mysql("/*!50000 ROLLBACK */", &mysql),
            rollback
        );
        assert_eq!(
            transaction_effect_mysql("/*!99999 ROLLBACK */", &mysql),
            TransactionEffect::Statement
        );
        assert_eq!(
            transaction_effect_mysql("/*M!ROLLBACK*/", &mysql),
            TransactionEffect::Statement
        );
        assert_eq!(
            transaction_effect_mysql("/*M!ROLLBACK*/", &mariadb),
            rollback
        );
        assert_eq!(
            transaction_effect_mysql("/*!COMMIT*/", &mariadb),
            TransactionEffect::Commit { chain: false }
        );
        assert_eq!(
            transaction_effect_mysql("IF 1 THEN COMMIT; END IF", &mariadb),
            TransactionEffect::Opaque
        );
        assert_eq!(
            transaction_effect_mysql("ROLLBACK", &MysqlMode::default()),
            rollback
        );
    }

    #[test]
    fn autocommitted_statements_are_committed() {
        let mut t = TransactionTracker::new();
        t.completed(TransactionEffect::Statement, Some(false));
        t.completed(TransactionEffect::Statement, Some(false));
        assert_eq!(t.completed_count(), 2);
        assert_eq!(
            t.finish(Some(false), RolledBack),
            vec![Committed, Committed]
        );
    }

    #[test]
    fn statements_in_an_open_transaction_stay_open_or_end_with_it() {
        let mut t = TransactionTracker::new();
        t.completed(
            TransactionEffect::Begin {
                commits_open: false,
            },
            Some(true),
        );
        t.completed(TransactionEffect::Statement, Some(true));
        assert_eq!(
            t.clone().finish(Some(true), RolledBack),
            vec![InTransaction; 2]
        );
        assert_eq!(
            t.clone().finish(Some(false), RolledBack),
            vec![RolledBack; 2]
        );
        assert_eq!(t.finish(None, RolledBack), vec![Unknown; 2]);
    }

    #[test]
    fn commit_and_rollback_decide_the_open_transaction() {
        let mut t = TransactionTracker::new();
        t.completed(
            TransactionEffect::Begin {
                commits_open: false,
            },
            Some(true),
        );
        t.completed(TransactionEffect::Statement, Some(true));
        t.completed(TransactionEffect::Rollback { chain: false }, Some(false));
        t.completed(
            TransactionEffect::Begin {
                commits_open: false,
            },
            Some(true),
        );
        t.completed(TransactionEffect::Statement, Some(true));
        t.completed(TransactionEffect::Commit { chain: true }, Some(true));
        t.completed(TransactionEffect::Statement, Some(true));
        assert_eq!(
            t.finish(Some(true), RolledBack),
            vec![
                RolledBack,
                RolledBack,
                Committed,
                Committed,
                Committed,
                Committed,
                InTransaction
            ]
        );
    }

    #[test]
    fn mysql_begin_and_implicit_commits_commit_what_was_open() {
        let mut t = TransactionTracker::new();
        t.completed(TransactionEffect::Begin { commits_open: true }, Some(true));
        t.completed(TransactionEffect::Statement, Some(true));
        t.completed(TransactionEffect::Begin { commits_open: true }, Some(true));
        t.completed(TransactionEffect::Statement, Some(true));
        // DDL: an implicit commit, no transaction open after it.
        t.completed(TransactionEffect::Statement, Some(false));
        assert_eq!(t.finish(Some(false), Unknown), vec![Committed; 5]);
    }

    #[test]
    fn savepoints_roll_back_only_what_came_after_them() {
        let mut t = TransactionTracker::new();
        t.completed(
            TransactionEffect::Begin {
                commits_open: false,
            },
            Some(true),
        );
        t.completed(TransactionEffect::Statement, Some(true));
        t.completed(
            TransactionEffect::Savepoint {
                name: "a".into(),
                replaces: false,
            },
            Some(true),
        );
        t.completed(TransactionEffect::Statement, Some(true));
        t.completed(
            TransactionEffect::Savepoint {
                name: "b".into(),
                replaces: false,
            },
            Some(true),
        );
        t.completed(TransactionEffect::Statement, Some(true));
        t.completed(
            TransactionEffect::RollbackTo { name: "a".into() },
            Some(true),
        );
        t.completed(TransactionEffect::Statement, Some(true));
        // a is still there: this undoes the first rollback and statement 7.
        t.completed(
            TransactionEffect::RollbackTo { name: "a".into() },
            Some(true),
        );
        t.completed(TransactionEffect::Commit { chain: false }, Some(false));
        assert_eq!(
            t.finish(Some(false), RolledBack),
            vec![
                Committed, Committed, Committed, RolledBack, RolledBack, RolledBack, RolledBack,
                RolledBack, Committed, Committed
            ]
        );
    }

    #[test]
    fn a_savepoint_from_an_earlier_call_rolls_back_the_whole_call() {
        let mut t = TransactionTracker::new();
        t.completed(TransactionEffect::Statement, Some(true));
        t.completed(
            TransactionEffect::Savepoint {
                name: "b".into(),
                replaces: false,
            },
            Some(true),
        );
        t.completed(
            TransactionEffect::RollbackTo { name: "a".into() },
            Some(true),
        );
        assert_eq!(
            t.finish(Some(true), RolledBack),
            vec![RolledBack, RolledBack, InTransaction]
        );
    }

    #[test]
    fn a_released_savepoint_keeps_its_statements() {
        let mut t = TransactionTracker::new();
        t.completed(
            TransactionEffect::Begin {
                commits_open: false,
            },
            Some(true),
        );
        t.completed(
            TransactionEffect::Savepoint {
                name: "a".into(),
                replaces: false,
            },
            Some(true),
        );
        t.completed(TransactionEffect::Statement, Some(true));
        t.completed(TransactionEffect::Release { name: "a".into() }, Some(true));
        t.completed(TransactionEffect::Commit { chain: false }, Some(false));
        assert_eq!(t.finish(Some(false), RolledBack), vec![Committed; 5]);
    }

    #[test]
    fn opaque_statements_and_unknown_status_are_never_committed() {
        let mut t = TransactionTracker::new();
        t.completed(TransactionEffect::Begin { commits_open: true }, Some(true));
        t.completed(TransactionEffect::Statement, Some(true));
        t.completed(TransactionEffect::Opaque, Some(false));
        t.completed(TransactionEffect::Statement, None);
        t.completed(TransactionEffect::Statement, Some(false));
        assert_eq!(
            t.finish(Some(false), Unknown),
            vec![Unknown, Unknown, Unknown, Unknown, Committed]
        );
    }

    #[test]
    fn partial_output_serializes_for_the_ui() -> Result<(), serde_json::Error> {
        let partial = PartialOutput {
            results: vec![StatementResult::Command {
                tag: "GRANT".into(),
                rows_affected: None,
            }],
            notices: Vec::new(),
            elapsed_ms: 3,
            failed_statement: Some(1),
            completed: vec![Committed],
        };
        assert!(partial.has_content());
        assert_eq!(
            serde_json::to_value(&partial)?,
            serde_json::json!({
                "results": [{ "kind": "command", "tag": "GRANT", "rowsAffected": null }],
                "notices": [],
                "elapsedMs": 3,
                "failedStatement": 1,
                "completed": ["committed"]
            })
        );
        assert_eq!(
            serde_json::to_value([RolledBack, InTransaction, Durability::Failed, Unknown])?,
            serde_json::json!(["rolled_back", "in_transaction", "failed", "unknown"])
        );
        Ok(())
    }
}
