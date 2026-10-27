//! The SQL normalizer of text level `normalized`, safe by construction.
//!
//! Each statement is parsed with `sqlparser` (PostgreSQL or MySQL
//! dialect) and rebuilt from its syntax tree:
//!
//! - every literal becomes `?`;
//! - an identifier is kept only where the tree says it is a name: table
//!   and other object names, aliases, `INSERT` column lists, assignment
//!   targets, function names, types, index columns, plain column
//!   references in a select list (with `FROM`), `GROUP BY` and `ORDER BY`,
//!   and the column side of a top-level condition whose other side is a
//!   literal (`WHERE a = ?`, `a IN (?, ?)`, `a LIKE ?`, `a IS NULL`,
//!   `t.a = u.b` with both sides qualified);
//! - every identifier in any other expression position becomes `?`.
//!
//! Only statements whose syntax tree is understood are rebuilt: queries,
//! DML, common DDL, `SET`, transactions, grants, `CALL`, `PREPARE`,
//! `EXECUTE`. Any other statement (account statements included), and any
//! statement the parser rejects (typos, unsupported syntax, a MySQL
//! `$word`, garbage), is normalized keywords-only: words from a fixed
//! keyword list and punctuation stay, every other word, number and literal
//! becomes `?`. A typo can therefore never keep a secret.

use std::ops::ControlFlow;

use std::collections::HashSet;

use sqlparser::ast::{
    AlterColumnOperation, AlterTableOperation, Assignment, AssignmentTarget, BinaryOperator,
    ColumnDef, ColumnOption, CommentDef, ConflictTarget, CreateTableOptions, DataType,
    DateTimeField, Expr, ForeignKeyConstraint, GrantObjects, Grantee, GranteeName, GranteesType,
    GroupByExpr, Ident, IndexColumn, IndexOption, JoinConstraint, JoinOperator, ObjectName,
    ObjectNamePart, OnConflictAction, OnInsert, OrderByKind, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, Set, SetExpr, SqlOption, Statement, TableAlias,
    TableConstraint, TableFactor, TableSampleKind, UnaryOperator, Use, Value, ValueWithSpan,
    VisitMut, VisitorMut, visit_expressions,
};
use sqlparser::dialect::{Dialect, MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Location;
use varve_driver::{Dialect as DriverDialect, LexKind, lex as driver_lex};

use super::REDACTED;
use super::keywords::{MYSQL_KEYWORDS, POSTGRES_KEYWORDS};
use super::sql::{Flavor, Kind, Tok, lex};

/// The parser gives every grantee after `TO GROUP a, b` (or `ROLE`,
/// `USER`) the type of the one before, and prints it again for each
/// (`GROUP a, GROUP b`): a type equal to the one before is not printed
/// again, so the text keeps the words it was written with.
fn collapse_grantee_types(grantees: &mut [Grantee]) {
    let mut previous = GranteesType::None;
    for grantee in grantees {
        let current = grantee.grantee_type.clone();
        if current == previous && current != GranteesType::Public && grantee.name.is_some() {
            grantee.grantee_type = GranteesType::None;
        }
        previous = current;
    }
}

/// Statements with more significant tokens than this are normalized
/// keywords-only: deep syntax trees cost stack and time to walk.
const MAX_PARSED_TOKENS: usize = 2_000;

/// How deep the parser may nest before it gives up (and the statement is
/// normalized keywords-only).
const RECURSION_LIMIT: usize = 64;

/// MySQL character sets, the only words kept as a charset introducer.
const CHARSETS: &[&str] = &[
    "armscii8", "ascii", "big5", "binary", "cp1250", "cp1251", "cp1256", "cp1257", "cp850",
    "cp852", "cp866", "cp932", "dec8", "eucjpms", "euckr", "gb18030", "gb2312", "gbk", "geostd8",
    "greek", "hebrew", "hp8", "keybcs2", "koi8r", "koi8u", "latin1", "latin2", "latin5", "latin7",
    "macce", "macroman", "sjis", "swe7", "tis620", "ucs2", "ujis", "utf16", "utf16le", "utf32",
    "utf8", "utf8mb3", "utf8mb4",
];

/// Fields of `EXTRACT(field FROM ...)`; any other field becomes `?`.
const EXTRACT_FIELDS: &[&str] = &[
    "century",
    "day",
    "decade",
    "dow",
    "doy",
    "epoch",
    "hour",
    "isodow",
    "isoyear",
    "julian",
    "microseconds",
    "millennium",
    "milliseconds",
    "minute",
    "month",
    "quarter",
    "second",
    "timezone",
    "timezone_hour",
    "timezone_minute",
    "week",
    "year",
];

/// Stands for a masked `COMMENT '...'` value; rendered as `?`.
const COMMENT_MARK: &str = "\u{1}C\u{1}";

/// Masks the modifiers of a custom type (`foo('secret')`): they are kept
/// by the parser as raw text.
fn mask_type_modifiers(data_type: &mut DataType) {
    if let DataType::Custom(_, modifiers) = data_type {
        for modifier in modifiers {
            *modifier = "?".into();
        }
    }
}

/// MySQL and MariaDB storage engines, the only `ENGINE = name` kept.
const ENGINES: &[&str] = &[
    "archive",
    "aria",
    "blackhole",
    "columnstore",
    "connect",
    "csv",
    "example",
    "federated",
    "innodb",
    "memory",
    "merge",
    "mrg_myisam",
    "myisam",
    "ndb",
    "ndbcluster",
    "rocksdb",
    "s3",
    "sequence",
    "spider",
];

/// Masks the `COMMENT` of index options.
fn mask_index_options(options: &mut [IndexOption]) {
    for option in options {
        if let IndexOption::Comment(text) = option {
            *text = COMMENT_MARK.into();
        }
    }
}

fn mask_comment(comment: &mut CommentDef) {
    match comment {
        CommentDef::WithEq(text) | CommentDef::WithoutEq(text) => *text = COMMENT_MARK.into(),
    }
}

/// Marks a kept name while the rest of the tree is masked.
const MARK: char = '\u{1}';

/// True when an introducer (`_utf8mb4`) names a MySQL character set.
fn charset(prefix: &str) -> bool {
    CHARSETS.contains(&prefix.trim_start_matches('_').to_ascii_lowercase().as_str())
}

/// True for a keyword of the engine, from the servers' own keyword lists.
pub(crate) fn is_keyword(word: &str, flavor: Flavor) -> bool {
    let list = match flavor {
        Flavor::Postgres => POSTGRES_KEYWORDS,
        Flavor::Mysql => MYSQL_KEYWORDS,
    };
    list.binary_search(&word.to_ascii_uppercase().as_str())
        .is_ok()
}

/// Normalizes `src` for text level `normalized`, statement by statement.
pub(crate) fn normalize(src: &str, flavor: Flavor) -> String {
    let toks = lex(src, flavor);
    let mut statements = Vec::new();
    let mut start = 0;
    for (i, tok) in toks.iter().enumerate() {
        if tok.kind == Kind::Punct && &src[tok.start..tok.end] == ";" {
            statements.push(&toks[start..i]);
            start = i + 1;
        }
    }
    statements.push(&toks[start..]);
    statements
        .into_iter()
        .filter(|toks| toks.iter().any(significant))
        .map(|toks| normalize_statement(src, toks, flavor))
        .collect::<Vec<_>>()
        .join("; ")
}

fn significant(tok: &Tok) -> bool {
    !matches!(tok.kind, Kind::Space | Kind::Comment)
}

fn normalize_statement(src: &str, toks: &[Tok], flavor: Flavor) -> String {
    let sig = toks.iter().filter(|t| significant(t)).count();
    let exec_comment = toks.iter().any(|t| t.kind == Kind::ExecMark);
    let redacted = toks
        .iter()
        .any(|t| src[t.start..t.end].contains(REDACTED) || src[t.start..].starts_with(REDACTED));
    if sig <= MAX_PARSED_TOKENS && !exec_comment && !redacted {
        let (Some(first), Some(last)) = (toks.first(), toks.last()) else {
            return String::new();
        };
        if let Some(text) = parsed(&src[first.start..last.end], flavor) {
            return text;
        }
    }
    keywords_only(src, toks, flavor)
}

/// Keywords-only normalization of a whole text.
pub(crate) fn keywords_only_text(src: &str, flavor: Flavor) -> String {
    keywords_only(src, &lex(src, flavor), flavor)
}

/// Keywords-only normalization: keywords and punctuation stay, everything
/// else becomes `?`.
fn keywords_only(src: &str, toks: &[Tok], flavor: Flavor) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    let mut skip_until = 0;
    for tok in toks {
        if tok.start < skip_until {
            continue;
        }
        if !significant(tok) {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        let text = &src[tok.start..tok.end];
        if src[tok.start..].starts_with(REDACTED) {
            out.push('?');
            skip_until = tok.start + REDACTED.len();
            continue;
        }
        match tok.kind {
            Kind::Punct | Kind::ExecMark => out.push_str(text),
            Kind::Word if is_keyword(text, flavor) => out.push_str(text),
            _ => out.push('?'),
        }
    }
    out
}

/// The statement rebuilt from its syntax tree, or `None` when it does not
/// parse as exactly one statement of a known kind.
fn parsed(text: &str, flavor: Flavor) -> Option<String> {
    let dialect: Box<dyn Dialect> = match flavor {
        Flavor::Postgres => Box::new(PostgreSqlDialect {}),
        Flavor::Mysql => Box::new(MySqlDialect {}),
    };
    let mut statements = Parser::new(dialect.as_ref())
        .with_recursion_limit(RECURSION_LIMIT)
        .try_with_sql(text)
        .and_then(|mut parser| parser.parse_statements())
        .ok()?;
    if statements.len() != 1 {
        return None;
    }
    let mut statement = statements.pop()?;
    if !trusted(&statement) {
        return None;
    }
    let mut masker = Masker {
        flavor,
        source: Source::new(text, flavor),
        kept: Vec::new(),
        names: HashSet::new(),
    };
    let _ = VisitMut::visit(&mut statement, &mut masker);
    let mut rendered = statement.to_string();
    for (i, name) in masker.kept.iter().enumerate().rev() {
        rendered = rendered.replace(&marker(i), name);
    }
    rendered = rendered.replace(&format!("'{COMMENT_MARK}'"), "?");
    if rendered.contains(MARK) {
        return None;
    }
    let rendered = mask_numbers(&rendered, flavor);
    verify(&rendered, flavor, &masker.names).then_some(rendered)
}

fn driver_dialect(flavor: Flavor) -> DriverDialect {
    match flavor {
        Flavor::Postgres => DriverDialect::Postgres,
        Flavor::Mysql => DriverDialect::Mysql,
    }
}

/// Replaces the numbers of a rendered statement (`VARCHAR(255)`) with `?`.
fn mask_numbers(rendered: &str, flavor: Flavor) -> String {
    let mut out = String::with_capacity(rendered.len());
    let mut copied = 0;
    for lexeme in driver_lex(rendered, driver_dialect(flavor), true) {
        if lexeme.kind == LexKind::Number {
            out.push_str(&rendered[copied..lexeme.start]);
            out.push('?');
            copied = lexeme.end;
        }
    }
    out.push_str(&rendered[copied..]);
    out
}

/// The safety net: accepts a rendered statement only when every token is a
/// keyword from the fixed list, punctuation or an operator, `?`, a
/// PostgreSQL `$n`, or a name the syntax tree walk recorded (compared by
/// exact text, quotes included). Strings, numbers, comments of any kind
/// (optimizer hints included) and unrecorded words are rejected, under
/// both MySQL backslash modes. Whatever `sqlparser` prints from text the
/// walk never saw is therefore caught here.
pub(crate) fn verify(rendered: &str, flavor: Flavor, recorded: &HashSet<String>) -> bool {
    let modes: &[bool] = match flavor {
        Flavor::Postgres => &[false],
        Flavor::Mysql => &[true, false],
    };
    modes.iter().all(|&backslash| {
        driver_lex(rendered, driver_dialect(flavor), backslash)
            .iter()
            .all(|lexeme| {
                let text = &rendered[lexeme.start..lexeme.end];
                match lexeme.kind {
                    // MySQL takes `$` in a word and a leading digit, but a
                    // name like that is more likely a typed secret.
                    LexKind::Word
                        if flavor == Flavor::Mysql
                            && (text.contains('$')
                                || text.starts_with(|c: char| c.is_ascii_digit())) =>
                    {
                        false
                    }
                    LexKind::Word => is_keyword(text, flavor) || recorded.contains(text),
                    LexKind::QuotedIdent => recorded.contains(text),
                    LexKind::Parameter => flavor == Flavor::Postgres,
                    LexKind::Punct(_) => true,
                    _ => false,
                }
            })
    })
}

/// Line starts of the parsed text, to turn token positions into offsets.
struct Source<'a> {
    text: &'a str,
    lines: Vec<usize>,
    /// The significant tokens of the text (no space or comments).
    toks: Vec<Tok>,
}

impl<'a> Source<'a> {
    fn new(text: &'a str, flavor: Flavor) -> Self {
        let mut lines = vec![0];
        lines.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        let toks = lex(text, flavor).into_iter().filter(significant).collect();
        Source { text, lines, toks }
    }

    /// Byte offset of a 1-based line and character column.
    fn offset(&self, location: Location) -> Option<usize> {
        let line = usize::try_from(location.line).ok()?.checked_sub(1)?;
        let column = usize::try_from(location.column).ok()?.checked_sub(1)?;
        let start = *self.lines.get(line)?;
        let rest = self.text.get(start..)?;
        Some(
            start
                + rest
                    .char_indices()
                    .nth(column)
                    .map_or(rest.len(), |(i, _)| i),
        )
    }

    /// True when the identifier is written right after the keyword `AS`:
    /// the significant token before it (comments skipped) is the word
    /// `AS`, and that word is not part of a qualified name (`x.as`).
    fn after_as(&self, ident: &Ident) -> bool {
        let Some(offset) = self.offset(ident.span.start) else {
            return false;
        };
        let before = self.toks.partition_point(|t| t.end <= offset);
        let Some(index) = before.checked_sub(1) else {
            return false;
        };
        let tok = self.toks[index];
        let is_as =
            tok.kind == Kind::Word && self.text[tok.start..tok.end].eq_ignore_ascii_case("AS");
        let qualified = index
            .checked_sub(1)
            .and_then(|i| self.toks.get(i))
            .is_some_and(|t| t.kind == Kind::Punct && &self.text[t.start..t.end] == ".");
        is_as && !qualified
    }
}

/// Statements whose syntax tree the masker understands.
fn trusted(statement: &Statement) -> bool {
    match statement {
        Statement::Query(_)
        | Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_)
        | Statement::CreateTable(_)
        | Statement::AlterTable(_)
        | Statement::CreateIndex(_)
        | Statement::CreateView(_)
        | Statement::Drop { .. }
        | Statement::Truncate(_)
        | Statement::ShowTables { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowSchemas { .. }
        | Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. }
        | Statement::Grant(_)
        | Statement::Revoke(_)
        | Statement::Call(_)
        | Statement::Execute { .. }
        | Statement::Deallocate { .. }
        | Statement::Use(_) => true,
        Statement::Set(set) => matches!(
            set,
            Set::SingleAssignment { .. }
                | Set::MultipleAssignments { .. }
                | Set::ParenthesizedAssignments { .. }
                | Set::SetTimeZone { .. }
                | Set::SetNamesDefault {}
                | Set::SetTransaction { .. }
        ),
        Statement::Explain { statement, .. } | Statement::Prepare { statement, .. } => {
            trusted(statement)
        }
        _ => false,
    }
}

fn marker(i: usize) -> String {
    format!("{MARK}{i}{MARK}")
}

fn is_name(expr: &Expr) -> bool {
    matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
}

/// A literal, possibly signed, cast or parenthesized.
fn literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(_) | Expr::TypedString { .. } => true,
        Expr::UnaryOp { op, expr } => {
            matches!(op, UnaryOperator::Minus | UnaryOperator::Plus) && literal(expr)
        }
        Expr::Nested(inner) | Expr::Cast { expr: inner, .. } => literal(inner),
        Expr::Interval(interval) => literal(&interval.value),
        Expr::Prefixed { prefix, value } => charset(&prefix.value) && literal(value),
        _ => false,
    }
}

/// True when `expr` holds no identifier at all, only values, operators
/// and function calls (`now() - interval '1 day'`). Its values are masked,
/// so the name compared with it can stay.
fn nameless(expr: &Expr) -> bool {
    literal(expr)
        || visit_expressions(expr, |e| {
            if is_name(e) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .is_continue()
}

fn comparison(op: &BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    )
}

struct Masker<'a> {
    flavor: Flavor,
    source: Source<'a>,
    /// Names kept, in marker order.
    kept: Vec<String>,
    /// Every name the walk chose to keep, as rendered (quotes included).
    /// Only these, keywords and punctuation pass [`verify`].
    names: HashSet<String>,
}

impl Masker<'_> {
    /// The one place a name is licensed: the walk calls this for an
    /// identifier only where the syntax tree says it names something.
    fn name(&mut self, ident: &Ident) {
        self.license(ident.to_string());
    }

    /// Adds rendered name text to what [`verify`] accepts. Called only by
    /// [`Self::name`] and for the words of a type, a known `EXTRACT` field
    /// and a known storage engine.
    fn license(&mut self, text: String) {
        self.names.insert(text);
    }

    fn name_object(&mut self, object: &ObjectName) {
        for part in &object.0 {
            if let ObjectNamePart::Identifier(ident) = part {
                self.name(ident);
            }
        }
    }

    /// A type in a type position: its modifiers are masked and the words
    /// of what remains (`numeric`, `TIMESTAMP WITH TIME ZONE`, `foo`) are
    /// names.
    fn name_type(&mut self, data_type: &mut DataType) {
        mask_type_modifiers(data_type);
        let text = data_type.to_string();
        for lexeme in driver_lex(&text, driver_dialect(self.flavor), true) {
            if matches!(lexeme.kind, LexKind::Word | LexKind::QuotedIdent) {
                self.license(text[lexeme.start..lexeme.end].to_string());
            }
        }
    }

    fn name_column(&mut self, column: &mut ColumnDef) {
        self.name(&column.name);
        self.name_type(&mut column.data_type);
        for option in &mut column.options {
            if let Some(name) = &option.name {
                self.name(name);
            }
            self.name_column_option(&mut option.option);
        }
    }

    fn name_column_option(&mut self, option: &mut ColumnOption) {
        match option {
            ColumnOption::Comment(text) => *text = COMMENT_MARK.into(),
            ColumnOption::CharacterSet(name) | ColumnOption::Collation(name) => {
                self.name_object(name);
            }
            ColumnOption::ForeignKey(key) => self.name_foreign_key(key),
            _ => {}
        }
    }

    fn name_foreign_key(&mut self, key: &ForeignKeyConstraint) {
        for ident in key.name.iter().chain(&key.index_name).chain(&key.columns) {
            self.name(ident);
        }
        self.name_object(&key.foreign_table);
        for ident in &key.referred_columns {
            self.name(ident);
        }
    }

    fn name_index_columns(&mut self, columns: &mut [IndexColumn]) {
        for column in columns {
            self.keep(&mut column.column.expr);
        }
    }

    fn name_constraint(&mut self, constraint: &mut TableConstraint) {
        match constraint {
            TableConstraint::Unique(c) => {
                for ident in c.name.iter().chain(&c.index_name).chain(&c.include) {
                    self.name(ident);
                }
                self.name_index_columns(&mut c.columns);
                mask_index_options(&mut c.index_options);
            }
            TableConstraint::PrimaryKey(c) => {
                for ident in c.name.iter().chain(&c.index_name).chain(&c.include) {
                    self.name(ident);
                }
                self.name_index_columns(&mut c.columns);
                mask_index_options(&mut c.index_options);
            }
            TableConstraint::ForeignKey(c) => self.name_foreign_key(c),
            TableConstraint::Check(c) => {
                if let Some(name) = &c.name {
                    self.name(name);
                }
            }
            TableConstraint::Index(c) => {
                if let Some(name) = &c.name {
                    self.name(name);
                }
                self.name_index_columns(&mut c.columns);
                mask_index_options(&mut c.index_options);
            }
            _ => {}
        }
    }

    /// A table option: a known `ENGINE` is a name; comments, unknown
    /// engines, parenthesized name lists and bare words are values.
    fn table_option(&mut self, option: &mut SqlOption) {
        match option {
            SqlOption::Comment(comment) => mask_comment(comment),
            SqlOption::NamedParenthesizedList(list) => {
                if let Some(name) = &mut list.name {
                    if ENGINES.contains(&name.value.to_ascii_lowercase().as_str()) {
                        self.license(name.to_string());
                    } else {
                        *name = Ident::new("?");
                    }
                }
                for value in &mut list.values {
                    *value = Ident::new("?");
                }
            }
            SqlOption::Ident(ident) => *ident = Ident::new("?"),
            _ => {}
        }
    }

    fn alter_operation(&mut self, operation: &mut AlterTableOperation) {
        match operation {
            AlterTableOperation::AddColumn { column_def, .. } => self.name_column(column_def),
            AlterTableOperation::AlterColumn { column_name, op } => {
                self.name(column_name);
                if let AlterColumnOperation::SetDataType { data_type, .. } = op {
                    self.name_type(data_type);
                }
            }
            AlterTableOperation::ChangeColumn {
                old_name,
                new_name,
                data_type,
                options,
                ..
            } => {
                self.name(old_name);
                self.name(new_name);
                self.name_type(data_type);
                options.iter_mut().for_each(|o| self.name_column_option(o));
            }
            AlterTableOperation::ModifyColumn {
                col_name,
                data_type,
                options,
                ..
            } => {
                self.name(col_name);
                self.name_type(data_type);
                options.iter_mut().for_each(|o| self.name_column_option(o));
            }
            AlterTableOperation::RenameColumn {
                old_column_name,
                new_column_name,
            } => {
                self.name(old_column_name);
                self.name(new_column_name);
            }
            AlterTableOperation::DropColumn { column_names, .. } => {
                column_names.iter().for_each(|c| self.name(c));
            }
            AlterTableOperation::AddConstraint { constraint, .. } => {
                self.name_constraint(constraint);
            }
            _ => {}
        }
    }

    fn name_grant(&mut self, objects: Option<&GrantObjects>, grantees: &[Grantee]) {
        if let Some(GrantObjects::Tables(tables)) = objects {
            tables.iter().for_each(|t| self.name_object(t));
        }
        for grantee in grantees {
            match &grantee.name {
                Some(GranteeName::ObjectName(name)) => self.name_object(name),
                Some(GranteeName::UserHost { user, host }) => {
                    self.name(user);
                    self.name(host);
                }
                None => {}
            }
        }
    }

    fn name_assignments(&mut self, assignments: &[Assignment]) {
        for assignment in assignments {
            match &assignment.target {
                AssignmentTarget::ColumnName(name) => self.name_object(name),
                AssignmentTarget::Tuple(names) => names.iter().for_each(|n| self.name_object(n)),
            }
        }
    }

    fn name_alias(&mut self, alias: &mut TableAlias) {
        self.name(&alias.name);
        for column in &mut alias.columns {
            self.name(&column.name);
            if let Some(data_type) = &mut column.data_type {
                self.name_type(data_type);
            }
        }
    }

    /// Swaps a name for a marker that masking leaves alone.
    fn keep(&mut self, expr: &mut Expr) {
        if is_name(expr) {
            match &*expr {
                Expr::Identifier(ident) => self.name(ident),
                Expr::CompoundIdentifier(parts) => {
                    for part in parts {
                        self.name(part);
                    }
                }
                _ => {}
            }
            let marker = marker(self.kept.len());
            self.kept.push(expr.to_string());
            *expr = Expr::Value(Value::Placeholder(marker).with_empty_span());
        }
    }

    /// Keeps the column side of the conditions joined by AND / OR / NOT
    /// at the top of a `WHERE`, `HAVING` or `ON` clause.
    fn keep_condition(&mut self, expr: &mut Expr) {
        match expr {
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And | BinaryOperator::Or,
                right,
            } => {
                self.keep_condition(left);
                self.keep_condition(right);
            }
            Expr::UnaryOp {
                op: UnaryOperator::Not,
                expr: inner,
            }
            | Expr::Nested(inner) => self.keep_condition(inner),
            Expr::BinaryOp { left, op, right } if comparison(op) => {
                let qualified = |e: &Expr| matches!(e, Expr::CompoundIdentifier(_));
                if qualified(left) && qualified(right) {
                    self.keep(left);
                    self.keep(right);
                } else if is_name(left) && nameless(right) {
                    self.keep(left);
                } else if nameless(left) && is_name(right) {
                    self.keep(right);
                }
            }
            Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. }
                if nameless(pattern) =>
            {
                self.keep(expr);
            }
            Expr::InList { expr, list, .. } if list.iter().all(nameless) => self.keep(expr),
            Expr::Between {
                expr, low, high, ..
            } if nameless(low) && nameless(high) => self.keep(expr),
            Expr::InSubquery { expr, .. }
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr)
            | Expr::IsTrue(expr)
            | Expr::IsFalse(expr) => self.keep(expr),
            _ => {}
        }
    }

    fn keep_projection(&mut self, items: &mut [SelectItem]) {
        for item in items {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    // A plain column reference, also cast (`id::text`).
                    match expr {
                        Expr::Cast { expr: inner, .. } => self.keep(inner),
                        _ => self.keep(expr),
                    }
                }
                _ => {}
            }
        }
    }
}

impl VisitorMut for Masker<'_> {
    type Break = ();

    fn pre_visit_relation(&mut self, relation: &mut ObjectName) -> ControlFlow<()> {
        self.name_object(relation);
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<()> {
        // `TABLESAMPLE word (10)` parses as a call of `word`: the sample
        // quantity is a value, not a function.
        if let TableFactor::Table {
            sample:
                Some(
                    TableSampleKind::BeforeTableAlias(sample)
                    | TableSampleKind::AfterTableAlias(sample),
                ),
            ..
        } = factor
            && let Some(quantity) = &mut sample.quantity
            && !literal(&quantity.value)
        {
            quantity.value = Expr::Value(Value::Placeholder("?".into()).with_empty_span());
        }
        match factor {
            TableFactor::Table { alias, .. }
            | TableFactor::Derived { alias, .. }
            | TableFactor::Function { alias, .. }
            | TableFactor::UNNEST { alias, .. }
            | TableFactor::NestedJoin { alias, .. } => {
                if let Some(alias) = alias {
                    self.name_alias(alias);
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_select(&mut self, select: &mut Select) -> ControlFlow<()> {
        // Optimizer hints are raw text.
        select.optimizer_hints.clear();
        // An alias written without AS may be the tail of a value
        // (`SELECT 9secret`, `INTERVAL '1' secret`): it is dropped.
        for item in &mut select.projection {
            match item {
                SelectItem::ExprWithAlias { expr, alias } if !self.source.after_as(alias) => {
                    let expr = std::mem::replace(expr, Expr::Value(Value::Null.with_empty_span()));
                    *item = SelectItem::UnnamedExpr(expr);
                }
                SelectItem::ExprWithAlias { alias, .. } => self.name(alias),
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(o),
                    _,
                ) => {
                    self.name_object(o);
                }
                _ => {}
            }
        }
        // Without FROM, a bare word in the select list is no column.
        if !select.from.is_empty() {
            self.keep_projection(&mut select.projection);
            if let GroupByExpr::Expressions(exprs, _) = &mut select.group_by {
                exprs.iter_mut().for_each(|e| self.keep(e));
            }
        }
        if let Some(selection) = &mut select.selection {
            self.keep_condition(selection);
        }
        if let Some(having) = &mut select.having {
            self.keep_condition(having);
        }
        for table in &mut select.from {
            for join in &mut table.joins {
                match join_constraint(&mut join.join_operator) {
                    Some(JoinConstraint::On(on)) => self.keep_condition(on),
                    Some(JoinConstraint::Using(columns)) => {
                        columns.iter().for_each(|c| self.name_object(c));
                    }
                    _ => {}
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<()> {
        if let Some(with) = &mut query.with {
            for cte in &mut with.cte_tables {
                self.name_alias(&mut cte.alias);
            }
        }
        let from = matches!(&*query.body, SetExpr::Select(select) if !select.from.is_empty());
        if from
            && let Some(order_by) = &mut query.order_by
            && let OrderByKind::Expressions(exprs) = &mut order_by.kind
        {
            exprs.iter_mut().for_each(|e| self.keep(&mut e.expr));
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<()> {
        match statement {
            Statement::CreateTable(table) => {
                for column in &mut table.columns {
                    self.name_column(column);
                }
                for constraint in &mut table.constraints {
                    self.name_constraint(constraint);
                }
                if let Some(comment) = &mut table.comment {
                    mask_comment(comment);
                }
                match &mut table.table_options {
                    CreateTableOptions::With(options)
                    | CreateTableOptions::Options(options)
                    | CreateTableOptions::Plain(options)
                    | CreateTableOptions::TableProperties(options) => {
                        for option in options {
                            self.table_option(option);
                        }
                    }
                    CreateTableOptions::None => {}
                }
            }
            Statement::AlterTable(alter) => {
                for operation in &mut alter.operations {
                    self.alter_operation(operation);
                }
            }
            Statement::CreateView(view) => {
                for column in &mut view.columns {
                    self.name(&column.name);
                    if let Some(data_type) = &mut column.data_type {
                        self.name_type(data_type);
                    }
                }
            }
            Statement::Drop { names, .. } => names.iter().for_each(|n| self.name_object(n)),
            Statement::Grant(grant) => {
                collapse_grantee_types(&mut grant.grantees);
                self.name_grant(grant.objects.as_ref(), &grant.grantees);
            }
            Statement::Revoke(revoke) => {
                collapse_grantee_types(&mut revoke.grantees);
                self.name_grant(revoke.objects.as_ref(), &revoke.grantees);
            }
            Statement::Set(set) => match set {
                Set::SingleAssignment { variable, .. } => self.name_object(variable),
                Set::ParenthesizedAssignments { variables, .. } => {
                    variables.iter().for_each(|v| self.name_object(v));
                }
                Set::MultipleAssignments { assignments } => {
                    for assignment in assignments {
                        self.name_object(&assignment.name);
                    }
                }
                _ => {}
            },
            Statement::Execute {
                name: Some(name), ..
            } => self.name_object(name),
            Statement::Prepare {
                name, data_types, ..
            } => {
                self.name(name);
                for data_type in data_types {
                    self.name_type(data_type);
                }
            }
            Statement::Deallocate { name, .. }
            | Statement::Savepoint { name }
            | Statement::ReleaseSavepoint { name } => self.name(name),
            Statement::Use(
                Use::Catalog(name) | Use::Schema(name) | Use::Database(name) | Use::Object(name),
            ) => self.name_object(name),
            Statement::Call(function) => self.name_object(&function.name),
            Statement::Update(update) => {
                update.optimizer_hints.clear();
                self.name_assignments(&update.assignments);
                if let Some(selection) = &mut update.selection {
                    self.keep_condition(selection);
                }
                if let Some(returning) = &mut update.returning {
                    self.keep_projection(returning);
                }
            }
            Statement::Delete(delete) => {
                delete.optimizer_hints.clear();
                if let Some(selection) = &mut delete.selection {
                    self.keep_condition(selection);
                }
                if let Some(returning) = &mut delete.returning {
                    self.keep_projection(returning);
                }
            }
            Statement::Insert(insert) => {
                insert.optimizer_hints.clear();
                insert.columns.iter().for_each(|c| self.name_object(c));
                self.name_assignments(&insert.assignments);
                if let Some(alias) = &insert.table_alias {
                    self.name(&alias.alias);
                }
                if let Some(aliases) = &insert.insert_alias {
                    self.name_object(&aliases.row_alias);
                    for column in aliases.col_aliases.iter().flatten() {
                        self.name(column);
                    }
                }
                match &insert.on {
                    Some(OnInsert::DuplicateKeyUpdate(assignments)) => {
                        self.name_assignments(assignments);
                    }
                    Some(OnInsert::OnConflict(conflict)) => {
                        match &conflict.conflict_target {
                            Some(ConflictTarget::Columns(columns)) => {
                                columns.iter().for_each(|c| self.name(c));
                            }
                            Some(ConflictTarget::OnConstraint(name)) => self.name_object(name),
                            None => {}
                        }
                        if let OnConflictAction::DoUpdate(update) = &conflict.action {
                            self.name_assignments(&update.assignments);
                        }
                    }
                    _ => {}
                }
                if let Some(returning) = &mut insert.returning {
                    self.keep_projection(returning);
                }
            }
            Statement::CreateIndex(index) => {
                if let Some(name) = &index.name {
                    self.name_object(name);
                }
                index.include.iter().for_each(|i| self.name(i));
                mask_index_options(&mut index.index_options);
                self.name_index_columns(&mut index.columns);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        match expr {
            Expr::Cast { data_type, .. } => self.name_type(data_type),
            Expr::TypedString(typed) => self.name_type(&mut typed.data_type),
            Expr::Function(function) => self.name_object(&function.name),
            Expr::Collate { collation, .. } => self.name_object(collation),
            // A charset introducer is kept when it names a real charset.
            Expr::Prefixed { prefix, .. } if charset(&prefix.value) => self.name(prefix),
            Expr::Extract { field, .. } => {
                // A known field is a name; any other word may be a value.
                let text = field.to_string();
                if EXTRACT_FIELDS.contains(&text.to_ascii_lowercase().as_str()) {
                    self.license(text);
                } else {
                    *field = DateTimeField::Custom(Ident::new("?"));
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        // A MySQL charset introducer (`_utf8mb4'x'`) stays only when it
        // names a real character set: `_secret'x'` parses the same way.
        let unknown_prefix =
            matches!(expr, Expr::Prefixed { prefix, .. } if !charset(&prefix.value));
        if is_name(expr) || unknown_prefix {
            *expr = Expr::Value(Value::Placeholder("?".into()).with_empty_span());
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_value(&mut self, value: &mut ValueWithSpan) -> ControlFlow<()> {
        let keep = match &value.value {
            Value::Placeholder(text) => {
                text.starts_with(MARK)
                    || text == "?"
                    || (self.flavor == Flavor::Postgres
                        && text.len() > 1
                        && text.starts_with('$')
                        && text[1..].bytes().all(|b| b.is_ascii_digit()))
            }
            _ => false,
        };
        if !keep {
            value.value = Value::Placeholder("?".into());
        }
        ControlFlow::Continue(())
    }
}

fn join_constraint(operator: &mut JoinOperator) -> Option<&mut JoinConstraint> {
    match operator {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c)
        | JoinOperator::Semi(c)
        | JoinOperator::LeftSemi(c)
        | JoinOperator::RightSemi(c)
        | JoinOperator::Anti(c)
        | JoinOperator::LeftAnti(c)
        | JoinOperator::RightAnti(c)
        | JoinOperator::StraightJoin(c) => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(text: &str) -> String {
        normalize(text, Flavor::Postgres)
    }

    fn my(text: &str) -> String {
        normalize(text, Flavor::Mysql)
    }

    #[test]
    fn values_and_value_identifiers_become_placeholders() {
        for (input, expected) in [
            (
                "INSERT INTO users (email, password) VALUES ('a@x', Zq9wSecret)",
                "INSERT INTO users (email, password) VALUES (?, ?)",
            ),
            (
                "UPDATE users SET password = Zq9wSecret WHERE id = 1",
                "UPDATE users SET password = ? WHERE id = ?",
            ),
            ("SELECT f(Zq9wSecret)", "SELECT f(?)"),
            (
                "SELECT * FROM t WHERE a IN (x, y)",
                "SELECT * FROM t WHERE ? IN (?, ?)",
            ),
            (
                "SELECT * FROM t WHERE note = hunter2xyz.part",
                "SELECT * FROM t WHERE ? = ?",
            ),
            (
                "SELECT * FROM t WHERE pw = hunter2xyz LIKE 'a'",
                "SELECT * FROM t WHERE ? = ? LIKE ?",
            ),
            ("SELECT 'x' AS a, hunter2xyz AS b", "SELECT ? AS a, ? AS b"),
        ] {
            assert_eq!(pg(input), expected, "{input}");
        }
    }

    #[test]
    fn statements_that_do_not_parse_keep_keywords_only() {
        for (input, expected) in [
            ("FOO Zq9wSecret BAR", "? ? ?"),
            ("ALTER ROLE alice PASSWROD Zq9wSecret", "ALTER ROLE ? ? ?"),
            ("XYZZY hunter2xyz.more", "? ?.?"),
        ] {
            assert_eq!(pg(input), expected, "{input}");
        }
        assert_eq!(
            my("UPDATE t SET note = $ecretXyz9 WHERE id = 1"),
            "UPDATE t SET note = ? WHERE id = ?"
        );
        assert_eq!(
            my("ALTER USER u IDENTIFIED BY [redacted]"),
            "ALTER USER ? IDENTIFIED BY ?"
        );
        // An unknown charset introducer is no introducer.
        assert_eq!(
            my("SELECT * FROM t WHERE pw = _hunter2xyz'x'"),
            "SELECT * FROM t WHERE pw = ?"
        );
        assert_eq!(
            my("SELECT * FROM t WHERE pw = _utf8mb4'x'"),
            "SELECT * FROM t WHERE pw = _utf8mb4 ?"
        );
    }

    #[test]
    fn names_stay_readable() {
        for (input, expected) in [
            (
                "SELECT u.id, o.total FROM users u LEFT JOIN orders o ON o.user_id = u.id WHERE o.total > 100 ORDER BY o.total DESC LIMIT 20",
                "SELECT u.id, o.total FROM users u LEFT JOIN orders o ON o.user_id = u.id WHERE o.total > ? ORDER BY o.total DESC LIMIT ?",
            ),
            (
                "SELECT id, email FROM users WHERE id = 42",
                "SELECT id, email FROM users WHERE id = ?",
            ),
            (
                "CREATE INDEX ON users (login)",
                "CREATE INDEX ON users(login)",
            ),
        ] {
            assert_eq!(pg(input), expected, "{input}");
        }
    }

    fn names(list: &[&str]) -> HashSet<String> {
        list.iter().map(|n| (*n).to_string()).collect()
    }

    /// The renders `sqlparser` produced before the explicit fixes, for the
    /// four classes of the fourth audit: the verifier alone rejects each.
    #[test]
    fn the_verifier_alone_rejects_re_emitted_text() {
        let pg = Flavor::Postgres;
        let my = Flavor::Mysql;
        let cases: &[(Flavor, &str, &[&str])] = &[
            // 1. Optimizer hints, kept as raw text.
            (
                pg,
                "SELECT /*+ SeqScan(t) Zq9wSecret */ * FROM t",
                &["t", "SeqScan"],
            ),
            (
                my,
                "UPDATE /*+ NO_INDEX(t) 'Zq9wSecret' */ t SET a = ?",
                &["t", "a"],
            ),
            (pg, "SELECT --+ Zq9wSecret\n * FROM t", &["t"]),
            // 2. Type modifier strings printed without quotes.
            (pg, "CREATE TABLE t (c foo(Zq9wSecret))", &["t", "c", "foo"]),
            (pg, "SELECT ?::foo(my Zq9wSecret pw)", &["foo"]),
            // 3. MySQL comments printed without escaping.
            (
                my,
                "CREATE TABLE t (id INT) COMMENT 'it's P@ssw0rdXy'",
                &["t", "id"],
            ),
            (
                my,
                "CREATE TABLE t (a INT COMMENT 'C:\\', b INT COMMENT 'pw P@ssw0rdXy x')",
                &["t", "a", "b"],
            ),
            // 4. MySQL names with `@` or `$`, or a leading digit.
            (
                my,
                "SELECT id, P@ssw0rdXy FROM users",
                &["id", "P@ssw0rdXy", "users"],
            ),
            (my, "SELECT $ecretXyz9 FROM t", &["$ecretXyz9", "t"]),
            (my, "SELECT 9hunter2xyz FROM t", &["9hunter2xyz", "t"]),
        ];
        for (flavor, rendered, recorded) in cases {
            assert!(!verify(rendered, *flavor, &names(recorded)), "{rendered}");
        }
        // What the walk produces now passes.
        assert!(verify(
            "SELECT u.id FROM users u WHERE u.id = ? AND name LIKE ?",
            pg,
            &names(&["u", "id", "users", "name"])
        ));
        assert!(verify(
            "CREATE TABLE t (id INT) COMMENT ? ENGINE = InnoDB",
            my,
            &names(&["t", "id", "InnoDB", "ENGINE"])
        ));
    }

    #[test]
    fn re_emitted_text_is_masked_or_falls_back() {
        for (input, leaked) in [
            ("SELECT /*+ SeqScan(t) Zq9wSecret */ * FROM t", "Zq9wSecret"),
            ("CREATE TABLE t (c foo('Zq9wSecret'))", "Zq9wSecret"),
            ("SELECT 'x'::foo('Zq9wSecret')", "Zq9wSecret"),
            ("SELECT extract(hunter2xyz FROM now())", "hunter2xyz"),
            ("SELECT INTERVAL '1' hunter2xyz", "hunter2xyz"),
            ("SELECT 9hunter2xyz FROM t", "hunter2xyz"),
        ] {
            assert!(!pg(input).contains(leaked), "{input} => {}", pg(input));
        }
        for (input, leaked) in [
            (
                "CREATE TABLE t (id INT) COMMENT 'it''s P@ssw0rdXy'",
                "ssw0rdXy",
            ),
            (
                "CREATE TABLE t (a INT COMMENT 'C:\\\\', b INT COMMENT 'pw P@ssw0rdXy x')",
                "ssw0rdXy",
            ),
            (
                "CREATE INDEX i ON t (a) COMMENT 'it''s P@ssw0rdXy'",
                "ssw0rdXy",
            ),
            ("SELECT id, P@ssw0rdXy FROM users", "ssw0rdXy"),
            ("SELECT 9hunter2xyz FROM t", "hunter2xyz"),
            ("CREATE TABLE t (a INT) ENGINE=hunter2xyz", "hunter2xyz"),
            (
                "UPDATE /*+ NO_INDEX(t) */ t SET a = 'Zq9wSecret'",
                "Zq9wSecret",
            ),
        ] {
            assert!(!my(input).contains(leaked), "{input} => {}", my(input));
        }
        assert_eq!(
            my("CREATE TABLE t (id INT) COMMENT 'it''s P@ssw0rdXy' ENGINE=InnoDB"),
            "CREATE TABLE t (id INT) COMMENT ? ENGINE = InnoDB"
        );
    }

    /// The fifth audit's leaking cases, through the real walker: an
    /// identifier the walk did not choose to keep never reaches the output.
    #[test]
    fn only_names_the_walk_keeps_pass() {
        let pg_cases: &[(&str, &str)] = &[
            ("SELECT CEIL(ts TO Login) FROM t", "Login"),
            ("EXECUTE s USING 1 AS hunter2xyz", "hunter2xyz"),
            ("SELECT CEIL(ts TO hunter2xyz) FROM CEIL", "hunter2xyz"),
            (
                "SELECT CEIL(price TO hunter2xyz) AS CEIL FROM t",
                "hunter2xyz",
            ),
            (
                "SELECT FLOOR FROM t WHERE ts = FLOOR(ts TO hunter2xyz)",
                "hunter2xyz",
            ),
            (
                "SELECT * FROM TABLESAMPLE TABLESAMPLE hunter2xyz (10)",
                "hunter2xyz",
            ),
            ("SELECT * FROM t TABLESAMPLE hunter2xyz (10)", "hunter2xyz"),
            ("ALTER TABLE t ADD COLUMN c foo(Login)", "Login"),
            ("ALTER TABLE t ADD COLUMN c foo(Superuser)", "Superuser"),
            ("ALTER TABLE t ADD COLUMN c foo(Password)", "Password"),
            ("EXECUTE s(1) INTO hunter2xyz", "hunter2xyz"),
        ];
        for (input, secret) in pg_cases {
            let out = pg(input);
            assert!(!out.contains(secret), "{input} => {out}");
        }
        let my_cases: &[(&str, &str)] = &[
            ("CREATE TABLE t (a INT) TABLESPACE Createdb", "Createdb"),
            ("EXECUTE s USING @a AS hunter2xyz", "hunter2xyz"),
        ];
        for (input, secret) in my_cases {
            let out = my(input);
            assert!(!out.contains(secret), "{input} => {out}");
        }
        // A pure operator run is punctuation; the operands are masked.
        assert_eq!(
            pg("SELECT * FROM t WHERE pw = 'x' AND a <!@#%^&> b"),
            "SELECT * FROM t WHERE pw = ? AND ? <!@#%^&> ?"
        );
        // A quoted column name is a name; the bare modifier is not.
        assert_eq!(
            pg("ALTER TABLE t ADD COLUMN \"Zq9wSecret\" foo(Zq9wSecret)"),
            "ALTER TABLE t ADD COLUMN \"Zq9wSecret\" foo(?)"
        );
    }

    #[test]
    fn type_modifiers_are_masked_wherever_types_appear() {
        for input in [
            "ALTER TABLE t ADD COLUMN c foo('Zq9wSecret')",
            "ALTER TABLE t ALTER COLUMN c TYPE foo('Zq9wSecret')",
            "CREATE TABLE t (c foo('Zq9wSecret'))",
            "SELECT 'x'::foo('Zq9wSecret')",
            "SELECT CAST('x' AS foo('Zq9wSecret'))",
            "PREPARE q(foo('Zq9wSecret')) AS SELECT 1",
            "WITH c(x) AS (SELECT 1) SELECT * FROM c",
        ] {
            let out = pg(input);
            assert!(!out.contains("Zq9wSecret"), "{input} => {out}");
        }
        for input in [
            "ALTER TABLE t MODIFY c foo('Zq9wSecret')",
            "ALTER TABLE t CHANGE c d foo('Zq9wSecret')",
            "ALTER TABLE t MODIFY COLUMN c INT COMMENT 'Zq9wSecret'",
        ] {
            let out = my(input);
            assert!(!out.contains("Zq9wSecret"), "{input} => {out}");
        }
        assert_eq!(
            pg("ALTER TABLE t ADD COLUMN c foo('Zq9wSecret')"),
            "ALTER TABLE t ADD COLUMN c foo(?)"
        );
    }

    #[test]
    fn deep_input_falls_back_quickly() {
        let long = format!(
            "SELECT * FROM t WHERE {}pw = P@ss",
            "a = 1 AND ".repeat(5000)
        );
        assert!(!pg(&long).contains("P@ss"));
        let parens = format!("SELECT * FROM t WHERE x IN {}hunter2xyz", "(".repeat(3000));
        assert!(!pg(&parens).contains("hunter2xyz"));
    }

    /// A grantee list keeps the words it was written with: the parser's
    /// carried-over `GROUP` is not printed again for each name.
    #[test]
    fn grantee_lists_do_not_repeat_their_type() {
        for (input, expected) in [
            (
                "REVOKE SELECT ON t FROM GROUP a6_g, CURRENT_USER",
                "REVOKE SELECT ON t FROM GROUP a6_g, CURRENT_USER",
            ),
            (
                "GRANT SELECT ON t TO GROUP a, b, c",
                "GRANT SELECT ON t TO GROUP a, b, c",
            ),
            (
                "GRANT SELECT ON t TO ROLE a, b",
                "GRANT SELECT ON t TO ROLE a, b",
            ),
        ] {
            assert_eq!(pg(input), expected, "{input}");
        }
        // Normalizing again changes nothing.
        let once = pg("REVOKE SELECT ON t FROM GROUP a6_g, CURRENT_USER");
        assert_eq!(pg(&once), once);
    }
}
