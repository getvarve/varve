//! Structured details of security events (design doc 9.2): the accounts or
//! roles a statement is about, the privileges, object and grantees of a
//! GRANT or REVOKE, and role attributes, read from the statement's code.
//! They are names and allowlisted keywords only, never literals other than
//! the quoted parts of a MySQL account name, so they hold no password; the
//! recorder keeps them at every text level (and still scrubs credentials).
//!
//! A MySQL statement is read in every quoting mode the session may be in
//! (`NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`); when the readings differ, or a
//! clause is not recognised, or a value is not in the statement as the
//! secret redactor leaves it, no details are stored (fail closed).

use std::cell::Cell;

use serde_json::{Map, Value};
use varve_activity::{SqlDialect, redact_secrets};
use varve_core::Engine;
use varve_driver::{Dialect, LexKind, MysqlMode, MysqlServer, lex_mysql};

use super::{quoting_modes, unquoted};

/// One code token of a statement.
#[derive(Debug, Clone)]
struct Tok {
    kind: LexKind,
    raw: String,
}

impl Tok {
    fn upper(&self) -> String {
        self.raw.to_ascii_uppercase()
    }

    fn is(&self, word: &str) -> bool {
        self.kind == LexKind::Word && self.raw.eq_ignore_ascii_case(word)
    }

    fn is_punct(&self, c: char) -> bool {
        self.kind == LexKind::Punct(c)
    }
}

/// Reads the code tokens of one statement.
struct Reader {
    toks: Vec<Tok>,
    mysql: bool,
    /// Something was not recognised: the details are not stored.
    uncertain: Cell<bool>,
}

impl Reader {
    /// Reads `statement`: PostgreSQL text, or MySQL text in `mode`.
    fn new(statement: &str, mode: Option<&MysqlMode>) -> Self {
        let lexemes = match mode {
            Some(mode) => lex_mysql(statement, mode),
            None => varve_driver::lex(statement, Dialect::Postgres, true),
        };
        let toks = lexemes
            .into_iter()
            .filter(|l| {
                !matches!(
                    l.kind,
                    LexKind::Comment
                        | LexKind::ExecutableCommentStart
                        | LexKind::ExecutableCommentEnd
                )
            })
            .map(|l| Tok {
                kind: l.kind,
                raw: statement[l.start..l.end].to_string(),
            })
            .collect();
        Self {
            toks,
            mysql: mode.is_some(),
            uncertain: Cell::new(false),
        }
    }

    /// Marks the reading as uncertain.
    fn doubt(&self) {
        self.uncertain.set(true);
    }

    fn get(&self, at: usize) -> Option<&Tok> {
        self.toks.get(at)
    }

    fn is(&self, at: usize, word: &str) -> bool {
        self.get(at).is_some_and(|t| t.is(word))
    }

    fn is_punct(&self, at: usize, c: char) -> bool {
        self.get(at).is_some_and(|t| t.is_punct(c))
    }

    /// The first word in `words` at parenthesis depth 0 from `from` on.
    fn find(&self, from: usize, words: &[&str]) -> Option<usize> {
        let mut depth = 0i32;
        for (at, tok) in self.toks.iter().enumerate().skip(from) {
            match tok.kind {
                LexKind::Punct('(') => depth += 1,
                LexKind::Punct(')') => depth -= 1,
                LexKind::Word if depth == 0 && words.iter().any(|w| tok.is(w)) => {
                    return Some(at);
                }
                _ => {}
            }
        }
        None
    }

    /// A name: an unquoted word (folded to lower case by PostgreSQL), a
    /// quoted identifier, or (MySQL account parts only) a string.
    fn name(&self, at: usize, strings: bool) -> Option<String> {
        let tok = self.get(at)?;
        match tok.kind {
            LexKind::Word if self.mysql => Some(tok.raw.clone()),
            LexKind::Word => Some(tok.raw.to_lowercase()),
            LexKind::QuotedIdent | LexKind::DoubleQuoted => Some(unquoted(&tok.raw)),
            LexKind::String if strings => Some(string_value(&tok.raw)),
            _ => None,
        }
    }

    /// One account or role at `at`: MySQL `user[@host]` (host `%` when not
    /// written, quoted parts unquoted), PostgreSQL a role name, or a
    /// keyword such as `CURRENT_USER` or `PUBLIC`. Returns it and the index
    /// after it.
    fn account(&self, at: usize) -> Option<(String, usize)> {
        let tok = self.get(at)?;
        // Keywords: PostgreSQL role specifications; in MySQL only
        // CURRENT_USER (an account named `public` is a name there).
        if tok.kind == LexKind::Word
            && if self.mysql {
                tok.is("CURRENT_USER")
            } else {
                matches!(
                    tok.upper().as_str(),
                    "CURRENT_USER" | "CURRENT_ROLE" | "SESSION_USER" | "PUBLIC"
                )
            }
        {
            let mut next = at + 1;
            if self.is_punct(next, '(') && self.is_punct(next + 1, ')') {
                next += 2;
            }
            return Some((tok.upper(), next));
        }
        // MySQL USER() as an account.
        if self.mysql && tok.is("USER") && self.is_punct(at + 1, '(') {
            let next = if self.is_punct(at + 2, ')') {
                at + 3
            } else {
                at + 2
            };
            return Some(("CURRENT_USER".into(), next));
        }
        // PostgreSQL `TO GROUP g` / `FROM GROUP g`: GROUP is a noise word.
        if !self.mysql
            && tok.is("GROUP")
            && self
                .get(at + 1)
                .is_some_and(|t| matches!(t.kind, LexKind::Word | LexKind::QuotedIdent))
        {
            return self.account(at + 1);
        }
        let user = self.name(at, self.mysql)?;
        if !self.mysql {
            return Some((user, at + 1));
        }
        if !self.is_punct(at + 1, '@') {
            return Some((account_name(&user, "%"), at + 1));
        }
        // The host: one name, or numbers and dots (127.0.0.1).
        let mut next = at + 2;
        let host = match self.name(next, true) {
            Some(host) => {
                next += 1;
                host
            }
            None => {
                let mut host = String::new();
                while let Some(t) = self.get(next) {
                    if t.kind == LexKind::Number || t.is_punct('.') {
                        host.push_str(&t.raw);
                        next += 1;
                    } else {
                        break;
                    }
                }
                host
            }
        };
        Some((account_name(&user, &host), next))
    }

    /// A comma separated list of accounts or roles from `at`.
    fn accounts(&self, at: usize) -> (Vec<String>, usize) {
        let mut names = Vec::new();
        let mut at = at;
        while let Some((name, next)) = self.account(at) {
            names.push(name);
            at = next;
            // MySQL auth options of one account in a list (`u1 IDENTIFIED
            // BY '...', u2`): skipped, never read.
            if self.mysql && self.is(at, "IDENTIFIED") {
                at = self.skip_auth(at);
            }
            if self.is_punct(at, ',') {
                at += 1;
            } else {
                break;
            }
        }
        (names, at)
    }

    /// The index after a MySQL `IDENTIFIED ...` clause: at the next comma
    /// or account clause at depth 0.
    fn skip_auth(&self, from: usize) -> usize {
        let mut at = from;
        let mut depth = 0i32;
        while let Some(tok) = self.get(at) {
            match tok.kind {
                LexKind::Punct('(') => depth += 1,
                LexKind::Punct(')') => depth -= 1,
                LexKind::Punct(',') if depth == 0 => return at,
                // WITH right after IDENTIFIED names the plugin.
                LexKind::Word
                    if depth == 0
                        && at > from
                        && !(tok.is("WITH") && self.is(at - 1, "IDENTIFIED"))
                        && (matches!(
                            tok.upper().as_str(),
                            "DEFAULT"
                                | "REQUIRE"
                                | "WITH"
                                | "ACCOUNT"
                                | "COMMENT"
                                | "ATTRIBUTE"
                                | "FAILED_LOGIN_ATTEMPTS"
                                | "PASSWORD_LOCK_TIME"
                        ) || (tok.is("PASSWORD")
                            && self.get(at + 1).is_some_and(|n| {
                                ["EXPIRE", "HISTORY", "REUSE", "REQUIRE"]
                                    .iter()
                                    .any(|w| n.is(w))
                            }))) =>
                {
                    return at;
                }
                _ => {}
            }
            at += 1;
        }
        at
    }

    /// Code from `from` to `to` as written, names unquoted; literals other
    /// than MySQL account parts (next to `@`) are left out.
    fn render(&self, from: usize, to: usize, upper: bool) -> String {
        let mut out = String::new();
        let mut depth = 0i32;
        let mut glue = false;
        for at in from..to.min(self.toks.len()) {
            let tok = &self.toks[at];
            let piece = match tok.kind {
                LexKind::Word if upper && depth == 0 => tok.upper(),
                LexKind::Word if !self.mysql && depth > 0 => tok.raw.clone(),
                LexKind::Word => tok.raw.clone(),
                LexKind::QuotedIdent | LexKind::DoubleQuoted => unquoted(&tok.raw),
                LexKind::String
                    if self.is_punct(at + 1, '@') || (at > 0 && self.is_punct(at - 1, '@')) =>
                {
                    string_value(&tok.raw)
                }
                LexKind::String | LexKind::DollarString => continue,
                LexKind::Number => tok.raw.clone(),
                LexKind::Punct(c) => c.to_string(),
                _ => continue,
            };
            match tok.kind {
                LexKind::Punct('(') => depth += 1,
                LexKind::Punct(')') => depth -= 1,
                _ => {}
            }
            let tight = matches!(tok.kind, LexKind::Punct('.' | '@' | ')' | ','));
            if !out.is_empty() && !glue && !tight {
                out.push(' ');
            }
            out.push_str(&piece);
            glue = matches!(tok.kind, LexKind::Punct('.' | '@' | '('));
        }
        out
    }

    /// Splits `from..to` at commas of depth 0.
    fn segments(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let mut parts = Vec::new();
        let mut depth = 0i32;
        let mut start = from;
        for at in from..to.min(self.toks.len()) {
            match self.toks[at].kind {
                LexKind::Punct('(') => depth += 1,
                LexKind::Punct(')') => depth -= 1,
                LexKind::Punct(',') if depth == 0 => {
                    parts.push((start, at));
                    start = at + 1;
                }
                _ => {}
            }
        }
        if start < to.min(self.toks.len()) {
            parts.push((start, to.min(self.toks.len())));
        }
        parts
    }
}

/// A MySQL account as `user@host`, or `'user'@'host'` (quotes doubled)
/// when either part has a character that would make that ambiguous.
fn account_name(user: &str, host: &str) -> String {
    let plain = |part: &str| {
        !part.is_empty()
            && !part
                .chars()
                .any(|c| matches!(c, '@' | '\'' | '"' | '`' | '\\') || c.is_whitespace())
    };
    if plain(user) && (plain(host) || host == "%") {
        format!("{user}@{host}")
    } else {
        let quote = |part: &str| format!("'{}'", part.replace('\'', "''"));
        format!("{}@{}", quote(user), quote(host))
    }
}

/// The value of a quoted string token (doubled quotes undone).
fn string_value(raw: &str) -> String {
    let inner = raw
        .strip_prefix('\'')
        .map(|r| r.strip_suffix('\'').unwrap_or(r))
        .unwrap_or(raw);
    inner.replace("''", "'").replace("\\'", "'")
}

fn list(names: Vec<String>) -> Value {
    Value::Array(names.into_iter().map(Value::String).collect())
}

/// Adds `names` under `key` when there are any.
fn put_list(map: &mut Map<String, Value>, key: &str, names: Vec<String>) {
    if !names.is_empty() {
        map.insert(key.into(), list(names));
    }
}

/// The details of a GRANT or REVOKE starting at token `start`:
/// `privileges` (or granted roles), `object`, `grantees`, and the options.
fn grant_details(r: &Reader, start: usize, map: &mut Map<String, Value>) {
    let revoke = r.is(start, "REVOKE");
    let mut at = start + 1;
    if r.mysql && r.is(at, "IF") && r.is(at + 1, "EXISTS") {
        map.insert("if_exists".into(), Value::Bool(true));
        at += 2;
    }
    // PostgreSQL REVOKE GRANT OPTION FOR / ADMIN OPTION FOR ...
    if !r.mysql && revoke && r.is(at + 1, "OPTION") && r.is(at + 2, "FOR") && r.get(at).is_some() {
        let option = r.get(at).map(Tok::upper).unwrap_or_default();
        map.insert(
            "option_only".into(),
            Value::String(format!("{option} OPTION")),
        );
        at += 3;
    }
    let keyword = if revoke { "FROM" } else { "TO" };
    let Some(to) = r.find(at, &[keyword]) else {
        return;
    };
    let on = r.find(at, &["ON"]).filter(|on| *on < to);
    let privileges: Vec<String> = match on {
        Some(on) => r
            .segments(at, on)
            .into_iter()
            .map(|(a, b)| r.render(a, b, true))
            .collect(),
        // Role grants: the roles.
        None => r
            .segments(at, to)
            .into_iter()
            .map(|(a, b)| match r.account(a) {
                Some((name, next)) if next == b && !r.mysql => name,
                _ => r.render(a, b, false),
            })
            .collect(),
    };
    put_list(map, "privileges", privileges);
    if let Some(on) = on {
        map.insert("object".into(), Value::String(r.render(on + 1, to, false)));
    }
    let (grantees, after) = r.accounts(to + 1);
    put_list(map, "grantees", grantees);
    let rest = r.render(after, r.toks.len(), true);
    for option in ["GRANT OPTION", "ADMIN OPTION"] {
        if rest.contains(&format!("WITH {option}")) {
            map.insert(
                format!("with_{}", option.to_ascii_lowercase().replace(' ', "_")),
                Value::Bool(true),
            );
        }
    }
    if let Some(by) = r.find(after, &["GRANTED"]).filter(|g| r.is(g + 1, "BY"))
        && let Some((name, _)) = r.account(by + 2)
    {
        map.insert("granted_by".into(), Value::String(name));
    }
    if rest.contains("IGNORE UNKNOWN USER") {
        map.insert("ignore_unknown_user".into(), Value::Bool(true));
    }
}

/// PostgreSQL role attributes stored as written (options with a value are
/// handled on their own).
const POSTGRES_ROLE_ATTRIBUTES: [&str; 14] = [
    "SUPERUSER",
    "NOSUPERUSER",
    "CREATEDB",
    "NOCREATEDB",
    "CREATEROLE",
    "NOCREATEROLE",
    "INHERIT",
    "NOINHERIT",
    "LOGIN",
    "NOLOGIN",
    "REPLICATION",
    "NOREPLICATION",
    "BYPASSRLS",
    "NOBYPASSRLS",
];

/// MySQL and MariaDB account clauses that start an attribute phrase.
const MYSQL_CLAUSE_STARTS: [&str; 8] = [
    "REQUIRE",
    "WITH",
    "PASSWORD",
    "ACCOUNT",
    "FAILED_LOGIN_ATTEMPTS",
    "PASSWORD_LOCK_TIME",
    "COMMENT",
    "ATTRIBUTE",
];

/// Words that may continue a MySQL or MariaDB account clause phrase.
const MYSQL_CLAUSE_WORDS: [&str; 25] = [
    "SSL",
    "X509",
    "NONE",
    "CIPHER",
    "ISSUER",
    "SUBJECT",
    "AND",
    "MAX_QUERIES_PER_HOUR",
    "MAX_UPDATES_PER_HOUR",
    "MAX_CONNECTIONS_PER_HOUR",
    "MAX_USER_CONNECTIONS",
    "MAX_STATEMENT_TIME",
    "EXPIRE",
    "DEFAULT",
    "NEVER",
    "INTERVAL",
    "DAY",
    "HISTORY",
    "REUSE",
    "CURRENT",
    "OPTIONAL",
    "LOCK",
    "UNLOCK",
    "UNBOUNDED",
    "REQUIRE",
];

/// PostgreSQL role options from token `from`: attributes (`SUPERUSER`,
/// `CONNECTION LIMIT`, `VALID UNTIL`, `PASSWORD`, ...) and memberships
/// (`member_of`, `members`, `admins`). Values are left out.
fn postgres_role_options(r: &Reader, from: usize, map: &mut Map<String, Value>) {
    let mut attributes = Vec::new();
    let (mut member_of, mut members, mut admins) = (Vec::new(), Vec::new(), Vec::new());
    let mut at = from;
    while let Some(tok) = r.get(at) {
        let word = tok.upper();
        at += 1;
        if tok.kind != LexKind::Word || word == "WITH" {
            continue;
        }
        match word.as_str() {
            "IN" if r.is(at, "ROLE") || r.is(at, "GROUP") => {
                let (names, next) = r.accounts(at + 1);
                member_of.extend(names);
                at = next;
            }
            "ROLE" | "USER" => {
                let (names, next) = r.accounts(at);
                members.extend(names);
                at = next;
            }
            "ADMIN" => {
                let (names, next) = r.accounts(at);
                admins.extend(names);
                at = next;
            }
            "PASSWORD" => {
                attributes.push("PASSWORD".to_string());
                at += 1;
            }
            "ENCRYPTED" | "UNENCRYPTED" => {}
            "VALID" if r.is(at, "UNTIL") => {
                attributes.push("VALID UNTIL".into());
                at += 2;
            }
            "CONNECTION" if r.is(at, "LIMIT") => {
                attributes.push("CONNECTION LIMIT".into());
                at += 2;
            }
            "SYSID" => at += 1,
            _ if POSTGRES_ROLE_ATTRIBUTES.contains(&word.as_str()) => attributes.push(word),
            // Not a role option: never stored as a free word.
            _ => r.doubt(),
        }
    }
    put_list(map, "attributes", attributes);
    put_list(map, "member_of", member_of);
    put_list(map, "members", members);
    put_list(map, "admins", admins);
}

/// MySQL account clauses from token `from` (after the account list):
/// `default_roles`, `admins` (MariaDB `WITH ADMIN`) and attribute phrases
/// (`ACCOUNT LOCK`, `REQUIRE SSL`, `PASSWORD EXPIRE INTERVAL DAY`, ...).
/// Values are left out.
fn mysql_account_options(r: &Reader, from: usize, map: &mut Map<String, Value>) {
    // Authentication (a password, a plugin) is named, never its value.
    let mut attributes: Vec<String> = r
        .find(0, &["IDENTIFIED"])
        .map(|_| "IDENTIFIED".to_string())
        .into_iter()
        .collect();
    let mut at = from;
    let mut phrase = false;
    while let Some(tok) = r.get(at) {
        if tok.is("DEFAULT") && r.is(at + 1, "ROLE") {
            let (names, next) = r.accounts(at + 2);
            put_list(map, "default_roles", names);
            at = next;
            continue;
        }
        if tok.is("WITH") && r.is(at + 1, "ADMIN") {
            let (names, next) = r.accounts(at + 2);
            put_list(map, "admins", names);
            at = next;
            continue;
        }
        if tok.is("IDENTIFIED") {
            at = r.skip_auth(at);
            continue;
        }
        at += 1;
        if tok.kind != LexKind::Word {
            continue;
        }
        let word = tok.upper();
        if MYSQL_CLAUSE_STARTS.contains(&word.as_str()) {
            attributes.push(word);
            phrase = true;
        } else if phrase && MYSQL_CLAUSE_WORDS.contains(&word.as_str()) {
            if let Some(last) = attributes.last_mut() {
                last.push(' ');
                last.push_str(&word);
            }
        } else {
            // Not a known account clause: never stored as a free word.
            r.doubt();
            phrase = false;
        }
    }
    put_list(map, "attributes", attributes);
}

/// The details of a statement read in one way (PostgreSQL, or MySQL in
/// `mode`), and whether something was not recognised.
fn parse_details(statement: &str, mode: Option<&MysqlMode>) -> (Map<String, Value>, bool) {
    let mut map = Map::new();
    let r = Reader::new(statement, mode);
    let word = |at: usize| r.get(at).map(Tok::upper);
    let first = word(0);
    match first.as_deref() {
        Some("GRANT" | "REVOKE") => grant_details(&r, 0, &mut map),
        Some("ALTER") if word(1).as_deref() == Some("DEFAULT") => {
            // ALTER DEFAULT PRIVILEGES [FOR ROLE ...] [IN SCHEMA ...] GRANT
            let mut at = 3;
            while let Some(w) = word(at) {
                match w.as_str() {
                    "FOR" => {
                        let (names, next) = r.accounts(at + 2);
                        put_list(&mut map, "for_roles", names);
                        at = next;
                    }
                    "IN" => {
                        let (names, next) = r.accounts(at + 2);
                        put_list(&mut map, "schemas", names);
                        at = next;
                    }
                    "GRANT" | "REVOKE" => {
                        grant_details(&r, at, &mut map);
                        break;
                    }
                    _ => at += 1,
                }
            }
            map.insert("default_privileges".into(), Value::Bool(true));
        }
        Some("CREATE") if word(1).as_deref() == Some("SCHEMA") => {
            let mut at = 2;
            if r.is(at, "IF") {
                at += 3;
            }
            if let Some(name) = r.name(at, false).filter(|_| !r.is(at, "AUTHORIZATION")) {
                put_list(&mut map, "schemas", vec![name]);
            }
            if let Some(grant) = r.find(at, &["GRANT"]) {
                grant_details(&r, grant, &mut map);
            }
        }
        Some("DROP") if word(1).as_deref() == Some("OWNED") => {
            let (names, _) = r.accounts(3);
            put_list(&mut map, "accounts", names);
        }
        Some("ALTER") if !r.mysql && word(1).as_deref() == Some("GROUP") => {
            let (names, next) = r.accounts(2);
            match word(next).as_deref() {
                Some("ADD" | "DROP") => {
                    put_list(&mut map, "privileges", names.clone());
                    let (members, _) = r.accounts(next + 2);
                    put_list(&mut map, "grantees", members);
                }
                Some("RENAME") => {
                    if let Some(new) = r.name(next + 2, false) {
                        put_list(&mut map, "renamed_to", vec![new]);
                    }
                }
                _ => {}
            }
            put_list(&mut map, "accounts", names);
        }
        Some("CREATE" | "ALTER" | "DROP") if !r.mysql => {
            let mut at = 2;
            if r.is(at, "IF") {
                map.insert("if_exists".into(), Value::Bool(true));
                at += if r.is(at + 1, "NOT") { 3 } else { 2 };
            }
            let (names, next) = r.accounts(at);
            put_list(&mut map, "accounts", names);
            match first.as_deref() {
                Some("CREATE") => postgres_role_options(&r, next, &mut map),
                Some("ALTER") => {
                    let mut at = next;
                    if r.is(at, "IN") && r.is(at + 1, "DATABASE") {
                        if let Some(db) = r.name(at + 2, false) {
                            map.insert("database".into(), Value::String(db));
                        }
                        at += 3;
                    }
                    if r.is(at, "RENAME") {
                        if let Some(new) = r.name(at + 2, false) {
                            put_list(&mut map, "renamed_to", vec![new]);
                        }
                    } else if r.is(at, "SET") || r.is(at, "RESET") {
                        // The setting's name, never its value.
                        let setting = r
                            .get(at + 1)
                            .map(|t| t.raw.to_lowercase())
                            .unwrap_or_default();
                        let verb = word(at).unwrap_or_default();
                        put_list(&mut map, "attributes", vec![format!("{verb} {setting}")]);
                    } else {
                        postgres_role_options(&r, at, &mut map);
                    }
                }
                _ => {}
            }
        }
        // MySQL and MariaDB account statements.
        Some("CREATE" | "ALTER" | "DROP") => {
            let mut at = 1;
            if r.is(at, "OR") && r.is(at + 1, "REPLACE") {
                map.insert("or_replace".into(), Value::Bool(true));
                at += 2;
            }
            at += 1;
            if r.is(at, "IF") {
                map.insert("if_exists".into(), Value::Bool(true));
                at += if r.is(at + 1, "NOT") { 3 } else { 2 };
            }
            let (names, next) = r.accounts(at);
            put_list(&mut map, "accounts", names);
            if first.as_deref() != Some("DROP") {
                mysql_account_options(&r, next, &mut map);
            }
        }
        Some("RENAME") => {
            let (mut from, mut to) = (Vec::new(), Vec::new());
            let mut at = 2;
            while let Some((old, next)) = r.account(at) {
                if !r.is(next, "TO") {
                    break;
                }
                let Some((new, after)) = r.account(next + 1) else {
                    break;
                };
                from.push(old);
                to.push(new);
                at = after;
                if r.is_punct(at, ',') {
                    at += 1;
                } else {
                    break;
                }
            }
            put_list(&mut map, "accounts", from);
            put_list(&mut map, "renamed_to", to);
        }
        Some("SET") if word(1).as_deref() == Some("PASSWORD") => {
            let names = if r.is(2, "FOR") {
                r.accounts(3).0
            } else {
                vec!["CURRENT_USER".into()]
            };
            put_list(&mut map, "accounts", names);
        }
        Some("SET") if word(1).as_deref() == Some("DEFAULT") => {
            // MySQL SET DEFAULT ROLE {NONE | ALL | roles} TO accounts;
            // MariaDB SET DEFAULT ROLE {NONE | role} [FOR account].
            let split = r.find(3, &["TO", "FOR"]);
            let end = split.unwrap_or(r.toks.len());
            let roles: Vec<String> = r
                .segments(3, end)
                .into_iter()
                .map(|(a, b)| match r.account(a) {
                    Some((name, next)) if next == b && !r.is(a, "NONE") && !r.is(a, "ALL") => name,
                    _ => r.render(a, b, true),
                })
                .collect();
            put_list(&mut map, "default_roles", roles);
            let accounts = match split {
                Some(at) => r.accounts(at + 1).0,
                None => vec!["CURRENT_USER".into()],
            };
            put_list(&mut map, "accounts", accounts);
        }
        _ => {}
    }
    let uncertain = r.uncertain.get();
    (map, uncertain)
}

/// Synthetic values the reader writes that are not in the statement.
const SYNTHETIC: [&str; 1] = ["current_user"];

/// The words and numbers of a detail value (runs of letters, digits, `_`
/// and `$`), lower-cased.
fn value_words(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.extend(
            text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .filter(|w| !w.is_empty())
                .map(str::to_lowercase),
        ),
        Value::Array(items) => items.iter().for_each(|v| value_words(v, out)),
        Value::Object(map) => map.values().for_each(|v| value_words(v, out)),
        _ => {}
    }
}

/// The structured details of a statement with exact security events (see
/// `security_actions`), as a JSON object (empty when there is nothing to
/// add), or why none are stored:
/// - `ambiguous_quoting`: a MySQL statement read differently in the
///   quoting modes the session may be in;
/// - `unrecognised_clause`: a clause the reader does not know (it never
///   stores free words);
/// - `secret_check`: a value that is not in the statement as the secret
///   redactor leaves it (a password read as a name, for example).
pub(super) fn security_details(
    statement: &str,
    engine: Engine,
    server: Option<MysqlServer>,
) -> Result<Map<String, Value>, &'static str> {
    let (map, dialect) = match engine {
        Engine::Postgres => {
            let (map, uncertain) = parse_details(statement, None);
            if uncertain {
                return Err("unrecognised_clause");
            }
            (map, SqlDialect::Postgres)
        }
        Engine::Mysql | Engine::Mariadb => {
            let mut readings = quoting_modes(server)
                .iter()
                .map(|mode| parse_details(statement, Some(mode)))
                .collect::<Vec<_>>()
                .into_iter();
            let Some((first, uncertain)) = readings.next() else {
                return Ok(Map::new());
            };
            let mut readings = readings.collect::<Vec<_>>();
            if readings.iter_mut().any(|(map, _)| *map != first) {
                return Err("ambiguous_quoting");
            }
            if uncertain || readings.iter().any(|(_, uncertain)| *uncertain) {
                return Err("unrecognised_clause");
            }
            (first, SqlDialect::Mysql)
        }
        Engine::Redis => return Ok(Map::new()),
    };
    if !secrets_kept_out(&map, statement, dialect) {
        return Err("secret_check");
    }
    Ok(map)
}

/// True when every word of every value of `map` is in the statement as
/// the secret redactor leaves it: a removed secret never reaches the
/// details.
fn secrets_kept_out(map: &Map<String, Value>, statement: &str, dialect: SqlDialect) -> bool {
    // Whole words: a removed password must not pass because it is part of
    // a longer kept word.
    let mut kept = Vec::new();
    value_words(
        &Value::String(redact_secrets(statement, dialect)),
        &mut kept,
    );
    let mut words = Vec::new();
    map.values().for_each(|v| value_words(v, &mut words));
    words
        .iter()
        .all(|w| kept.contains(w) || SYNTHETIC.contains(&w.as_str()))
}

/// The event `target`: the accounts a statement is about, else its
/// grantees.
pub(super) fn target_of(details: &Map<String, Value>) -> Option<String> {
    let names = details
        .get("accounts")
        .or_else(|| details.get("grantees"))?
        .as_array()?;
    let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// The number of accounts a statement names (accounts and grantees), for
/// telling whether a failing statement can have applied in part.
pub(super) fn named_accounts(
    statement: &str,
    engine: Engine,
    server: Option<MysqlServer>,
) -> usize {
    // Unknown accounts: assume several (a partial apply stays in doubt).
    let Ok(details) = security_details(statement, engine, server) else {
        return usize::MAX;
    };
    ["accounts", "grantees", "renamed_to"]
        .iter()
        .filter_map(|key| details.get(*key).and_then(Value::as_array))
        .map(Vec::len)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn details(statement: &str, engine: Engine) -> Value {
        match security_details(statement, engine, None) {
            Ok(map) => Value::Object(map),
            Err(reason) => json!({ "omitted": reason }),
        }
    }

    /// Audit round 6, Medium 1: the account, role, attributes and
    /// privileges of a statement, whatever the text level keeps.
    #[test]
    fn postgres_statements_name_their_roles_attributes_and_privileges() {
        let pg = Engine::Postgres;
        assert_eq!(
            details("ALTER ROLE a6_u SUPERUSER CREATEROLE", pg),
            json!({"accounts": ["a6_u"], "attributes": ["SUPERUSER", "CREATEROLE"]})
        );
        assert_eq!(
            details("CREATE ROLE \"a6_N1\" WITH NOLOGIN IN ROLE \"a6_g\"", pg),
            json!({"accounts": ["a6_N1"], "attributes": ["NOLOGIN"], "member_of": ["a6_g"]})
        );
        assert_eq!(
            details(
                "CREATE ROLE n LOGIN PASSWORD 'Secret-1' VALID UNTIL '2030-01-01' \
                 CONNECTION LIMIT 5 ROLE a, b ADMIN c",
                pg
            ),
            json!({
                "accounts": ["n"],
                "attributes": ["LOGIN", "PASSWORD", "VALID UNTIL", "CONNECTION LIMIT"],
                "members": ["a", "b"],
                "admins": ["c"]
            })
        );
        assert_eq!(
            details("ALTER USER u PASSWORD 'Secret-1'", pg),
            json!({"accounts": ["u"], "attributes": ["PASSWORD"]})
        );
        assert_eq!(
            details("ALTER ROLE u IN DATABASE d SET search_path = 'x'", pg),
            json!({"accounts": ["u"], "database": "d", "attributes": ["SET search_path"]})
        );
        assert_eq!(
            details("ALTER ROLE u RENAME TO v", pg),
            json!({"accounts": ["u"], "renamed_to": ["v"]})
        );
        assert_eq!(
            details("DROP ROLE IF EXISTS a, \"B\"", pg),
            json!({"if_exists": true, "accounts": ["a", "B"]})
        );
        assert_eq!(
            details(
                "GRANT SELECT, UPDATE (a, b) ON TABLE app.t TO u, GROUP g WITH GRANT OPTION",
                pg
            )["privileges"],
            json!(["SELECT", "UPDATE (a, b)"])
        );
        assert_eq!(
            details("GRANT SELECT ON TABLE app.t TO u WITH GRANT OPTION", pg),
            json!({
                "privileges": ["SELECT"],
                "object": "TABLE app.t",
                "grantees": ["u"],
                "with_grant_option": true
            })
        );
        assert_eq!(
            details("GRANT g TO u WITH ADMIN OPTION GRANTED BY o", pg),
            json!({
                "privileges": ["g"],
                "grantees": ["u"],
                "with_admin_option": true,
                "granted_by": "o"
            })
        );
        assert_eq!(
            details("REVOKE GRANT OPTION FOR ALL ON SCHEMA s FROM PUBLIC", pg),
            json!({
                "option_only": "GRANT OPTION",
                "privileges": ["ALL"],
                "object": "SCHEMA s",
                "grantees": ["PUBLIC"]
            })
        );
        assert_eq!(
            details(
                "ALTER DEFAULT PRIVILEGES FOR ROLE o IN SCHEMA app GRANT SELECT ON TABLES TO a6_u",
                pg
            ),
            json!({
                "for_roles": ["o"],
                "schemas": ["app"],
                "privileges": ["SELECT"],
                "object": "TABLES",
                "grantees": ["a6_u"],
                "default_privileges": true
            })
        );
        assert_eq!(
            details("ALTER GROUP g ADD USER u, v", pg),
            json!({"privileges": ["g"], "grantees": ["u", "v"], "accounts": ["g"]})
        );
        assert_eq!(
            details(
                "CREATE SCHEMA s2 CREATE TABLE x (a int) GRANT SELECT ON TABLE x TO u",
                pg
            ),
            json!({
                "schemas": ["s2"],
                "privileges": ["SELECT"],
                "object": "TABLE x",
                "grantees": ["u"]
            })
        );
        assert_eq!(
            details("DROP OWNED BY a, b", pg),
            json!({"accounts": ["a", "b"]})
        );
    }

    /// Audit round 7, High 1: a statement the quoting modes read
    /// differently (NO_BACKSLASH_ESCAPES), an unknown clause, or a value the
    /// secret redactor removes: no details, never a guess.
    #[test]
    fn uncertain_mysql_statements_store_no_details() {
        for engine in [Engine::Mysql, Engine::Mariadb] {
            for sql in [
                "CREATE USER 'a6_n1\\' IDENTIFIED BY 'hunterpw', 'a6_n2'@'%' IDENTIFIED BY 'x'",
                "CREATE USER 'a6_n1\\' IDENTIFIED BY 'Hunter2pw!', 'a6_n2'@'%'",
                "GRANT SELECT ON a6db.* TO 'a6_n1\\', a6_u",
            ] {
                assert_eq!(
                    security_details(sql, engine, None),
                    Err("ambiguous_quoting"),
                    "{sql}"
                );
                assert_eq!(named_accounts(sql, engine, None), usize::MAX, "{sql}");
            }
            // ANSI_QUOTES: a double-quoted name reads the same either way.
            assert_eq!(
                details("CREATE USER \"a6_n1\"@'%'", engine),
                json!({"accounts": ["a6_n1@%"]})
            );
            assert_eq!(
                security_details("ALTER USER u HUNTERPW", engine, None),
                Err("unrecognised_clause")
            );
        }
        assert_eq!(
            security_details("ALTER ROLE u HUNTERPW", Engine::Postgres, None),
            Err("unrecognised_clause")
        );
        // A value the redactor removes is never stored.
        let mut map = Map::new();
        map.insert("accounts".into(), json!(["HUNTERPW"]));
        assert!(!secrets_kept_out(
            &map,
            "CREATE USER a IDENTIFIED BY 'hunterpw'",
            SqlDialect::Mysql
        ));
        map.insert("accounts".into(), json!(["a@%"]));
        assert!(secrets_kept_out(
            &map,
            "CREATE USER a IDENTIFIED BY 'hunterpw'",
            SqlDialect::Mysql
        ));
    }

    /// Audit round 7, Medium 1 and Low 5: PostgreSQL GROUP and role
    /// keywords in grantee lists; ambiguous MySQL account names quoted.
    #[test]
    fn grantee_keywords_and_ambiguous_accounts() {
        let pg = Engine::Postgres;
        assert_eq!(
            details("GRANT SELECT ON app.t TO GROUP a6_g", pg)["grantees"],
            json!(["a6_g"])
        );
        assert_eq!(
            details(
                "REVOKE SELECT ON app.t FROM GROUP a6_g, PUBLIC, CURRENT_USER, SESSION_USER, \
                 CURRENT_ROLE, group \"G2\"",
                pg
            )["grantees"],
            json!([
                "a6_g",
                "PUBLIC",
                "CURRENT_USER",
                "SESSION_USER",
                "CURRENT_ROLE",
                "G2"
            ])
        );
        assert_eq!(
            details(
                "CREATE USER 'a6@x'@'%', 'b''q'@'h', c@'1.2.3.4'",
                Engine::Mysql
            )["accounts"],
            json!(["'a6@x'@'%'", "'b''q'@'h'", "c@1.2.3.4"])
        );
    }

    /// Audit round 8, Lows 2 and 5: `public` is a MySQL account name;
    /// the secret check compares whole words.
    #[test]
    fn mysql_public_and_whole_word_secret_check() {
        assert_eq!(
            details("CREATE USER public", Engine::Mysql)["accounts"],
            json!(["public@%"])
        );
        assert_eq!(
            details("GRANT SELECT ON t TO PUBLIC", Engine::Postgres)["grantees"],
            json!(["PUBLIC"])
        );
        let mut map = Map::new();
        map.insert("accounts".into(), json!(["pw"]));
        // `pw` is only part of the kept word `pwd_user`: not kept.
        assert!(!secrets_kept_out(
            &map,
            "CREATE USER pwd_user IDENTIFIED BY 'pw'",
            SqlDialect::Mysql
        ));
        map.insert("accounts".into(), json!(["pwd_user@%"]));
        assert!(secrets_kept_out(
            &map,
            "CREATE USER pwd_user IDENTIFIED BY 'pw'",
            SqlDialect::Mysql
        ));
    }

    #[test]
    fn mysql_statements_name_their_accounts_with_hosts() {
        let my = Engine::Mysql;
        assert_eq!(
            details("GRANT SELECT ON a6db.* TO 'a6_n1'@'%', b@localhost", my),
            json!({
                "privileges": ["SELECT"],
                "object": "a6db.*",
                "grantees": ["a6_n1@%", "b@localhost"]
            })
        );
        assert_eq!(
            details("RENAME USER a6_u TO a6_ren, 'x'@'10.0.0.1' TO y", my),
            json!({"accounts": ["a6_u@%", "x@10.0.0.1"], "renamed_to": ["a6_ren@%", "y@%"]})
        );
        let created = details(
            "CREATE USER IF NOT EXISTS a6_n1 IDENTIFIED BY 'Secret-1', b IDENTIFIED WITH \
             caching_sha2_password BY 'Other-2' DEFAULT ROLE a6_r, 'r2'@'%' \
             REQUIRE SSL PASSWORD EXPIRE INTERVAL 90 DAY ACCOUNT LOCK COMMENT 'hi'",
            my,
        );
        assert_eq!(
            created,
            json!({
                "if_exists": true,
                "accounts": ["a6_n1@%", "b@%"],
                "default_roles": ["a6_r@%", "r2@%"],
                "attributes": [
                    "IDENTIFIED",
                    "REQUIRE SSL",
                    "PASSWORD EXPIRE INTERVAL DAY",
                    "ACCOUNT LOCK",
                    "COMMENT"
                ]
            })
        );
        let text = created.to_string();
        assert!(!text.contains("Secret") && !text.contains("Other") && !text.contains("hi"));
        assert_eq!(
            details("ALTER USER u IDENTIFIED BY 'Secret-1' ACCOUNT UNLOCK", my),
            json!({"accounts": ["u@%"], "attributes": ["IDENTIFIED", "ACCOUNT UNLOCK"]})
        );
        assert_eq!(
            details("CREATE OR REPLACE ROLE r WITH ADMIN a6_u", Engine::Mariadb),
            json!({"or_replace": true, "accounts": ["r@%"], "admins": ["a6_u@%"]})
        );
        assert_eq!(
            details("SET DEFAULT ROLE a6_r, b TO a6_u, c", my),
            json!({"default_roles": ["a6_r@%", "b@%"], "accounts": ["a6_u@%", "c@%"]})
        );
        assert_eq!(
            details("SET DEFAULT ROLE NONE FOR a6_u", Engine::Mariadb),
            json!({"default_roles": ["NONE"], "accounts": ["a6_u@%"]})
        );
        assert_eq!(
            details("SET PASSWORD FOR 'a6_u'@'%' = 'Secret-1'", my),
            json!({"accounts": ["a6_u@%"]})
        );
        assert_eq!(
            details(
                "REVOKE IF EXISTS SELECT ON a6db.* FROM a6_m IGNORE UNKNOWN USER",
                my
            ),
            json!({
                "if_exists": true,
                "privileges": ["SELECT"],
                "object": "a6db.*",
                "grantees": ["a6_m@%"],
                "ignore_unknown_user": true
            })
        );
        assert_eq!(
            details("GRANT a6_r, 'r2'@'h' TO a6_u", my),
            json!({"privileges": ["a6_r", "r2@h"], "grantees": ["a6_u@%"]})
        );
        assert_eq!(
            details("GRANT PROXY ON 'a'@'%' TO b", my)["object"],
            json!("a@%")
        );
        assert_eq!(named_accounts("GRANT SELECT ON d.* TO a, b", my, None), 2);
        assert_eq!(named_accounts("SET PASSWORD FOR a = 'x'", my, None), 1);
        assert_eq!(
            security_details("DROP USER a, b", my, None)
                .ok()
                .and_then(|d| target_of(&d))
                .as_deref(),
            Some("a@%, b@%")
        );
    }
}
