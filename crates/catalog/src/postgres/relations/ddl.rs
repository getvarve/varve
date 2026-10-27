//! DDL generation. Definitions the server can print (`pg_get_viewdef`,
//! `pg_get_indexdef`, `pg_get_constraintdef`, `pg_get_triggerdef`,
//! `pg_get_ruledef`, `pg_get_partkeydef`, `format_type`, `pg_get_expr`) are
//! used as printed; every name and value Varve adds is quoted with
//! [`crate::sql`]. The output is a script of complete statements, each
//! ending in `;`, in an order the server accepts.

use std::collections::HashSet;

use varve_driver::ServerInfo;

use crate::model::CatalogResult;

use crate::postgres::programs::REDACTED;

use super::load::IndexChild;
use super::support::{ident, lit, option_list, qname, qualified, setting_value};
use super::types::{
    PgColumn, PgConstraint, PgDatabase, PgFdwOption, PgIdentity, PgIndex, PgMaterializedView,
    PgPolicy, PgRule, PgSchema, PgSequence, PgTable, PgTablespace, PgTrigger, PgView,
};

/// Statements of a script.
#[derive(Debug, Default)]
pub(super) struct Script {
    notes: Vec<String>,
    statements: Vec<String>,
}

/// The note a script starts with when it holds [`REDACTED`] values (the
/// wording of the programs catalog).
const REDACTED_NOTE: &str = "Option values that may hold secrets are written as '<redacted>': replace them before running this.";

impl Script {
    /// Adds a `--` comment line printed before the statements.
    pub(super) fn note(&mut self, note: &str) {
        if !self.notes.iter().any(|n| n == note) {
            self.notes.push(note.to_owned());
        }
    }

    /// Adds a statement (without a trailing semicolon).
    pub(super) fn push(&mut self, statement: impl Into<String>) {
        self.statements.push(statement.into());
    }

    /// Appends another script.
    pub(super) fn extend(&mut self, other: Script) {
        for note in &other.notes {
            self.note(note);
        }
        self.statements.extend(other.statements);
    }

    /// The script text: statements separated by blank lines, each ending
    /// in `;`.
    pub(super) fn finish(self) -> String {
        let mut out = String::new();
        for note in &self.notes {
            out.push_str("-- ");
            out.push_str(note);
            out.push('\n');
        }
        for statement in self.statements {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&statement);
            out.push(';');
        }
        out.push('\n');
        out
    }
}

/// How `ALTER` and `COMMENT` statements name a relation.
#[derive(Debug, Clone)]
pub(super) struct Target {
    /// Quoted qualified name.
    pub(super) name: String,
    /// `TABLE`, `FOREIGN TABLE`, `VIEW` or `MATERIALIZED VIEW`.
    pub(super) keyword: &'static str,
    /// Use `ONLY` in `ALTER` statements (plain and foreign tables; never
    /// partitioned tables, where `ONLY` is refused for most changes).
    pub(super) only: bool,
}

impl Target {
    /// The target for a table detail.
    pub(super) fn table(table: &PgTable) -> CatalogResult<Self> {
        Ok(Self {
            name: qname(&table.schema, &table.name)?,
            keyword: if table.foreign.is_some() {
                "FOREIGN TABLE"
            } else {
                "TABLE"
            },
            only: table.partitioning.is_none(),
        })
    }

    /// The target for a view.
    pub(super) fn view(schema: &str, name: &str) -> CatalogResult<Self> {
        Ok(Self {
            name: qname(schema, name)?,
            keyword: "VIEW",
            only: false,
        })
    }

    /// The target for a materialized view.
    pub(super) fn materialized_view(schema: &str, name: &str) -> CatalogResult<Self> {
        Ok(Self {
            name: qname(schema, name)?,
            keyword: "MATERIALIZED VIEW",
            only: false,
        })
    }

    fn alter(&self) -> String {
        format!(
            "ALTER {} {}{}",
            self.keyword,
            if self.only { "ONLY " } else { "" },
            self.name
        )
    }

    fn alter_plain(&self) -> String {
        format!("ALTER {} {}", self.keyword, self.name)
    }

    /// `ALTER TABLE` form for trigger and rule state changes, which only
    /// exist for tables.
    fn alter_table(&self) -> String {
        format!("ALTER TABLE {}", self.name)
    }
}

/// `name 'value', ...` for foreign data wrapper options; hidden values are
/// written as [`REDACTED`] and the script gets [`REDACTED_NOTE`].
fn fdw_option_list(options: &[PgFdwOption], script: &mut Script) -> CatalogResult<String> {
    let mut parts = Vec::with_capacity(options.len());
    for option in options {
        let value = match &option.value {
            Some(value) => value.as_str(),
            None => {
                script.note(REDACTED_NOTE);
                REDACTED
            }
        };
        parts.push(format!("{} {}", ident(&option.name)?, lit(value)?));
    }
    Ok(parts.join(", "))
}

fn comment(script: &mut Script, on: &str, text: Option<&str>) -> CatalogResult<()> {
    if let Some(text) = text {
        script.push(format!("COMMENT ON {on} IS {}", lit(text)?));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Databases, schemas, tablespaces

/// `CREATE DATABASE` and the database's comment and settings.
pub(super) fn database(server: &ServerInfo, db: &PgDatabase) -> CatalogResult<String> {
    let name = ident(&db.name)?;
    let mut options = vec![
        "TEMPLATE = template0".to_owned(),
        format!("OWNER = {}", ident(&db.owner)?),
        format!("ENCODING = {}", lit(&db.encoding)?),
    ];
    if server.at_least(15, 0) {
        options.push(format!("LOCALE_PROVIDER = {}", db.locale_provider));
    }
    options.push(format!("LC_COLLATE = {}", lit(&db.collation)?));
    options.push(format!("LC_CTYPE = {}", lit(&db.ctype)?));
    if let Some(locale) = &db.locale {
        match db.locale_provider.as_str() {
            "icu" => options.push(format!("ICU_LOCALE = {}", lit(locale)?)),
            "builtin" => options.push(format!("BUILTIN_LOCALE = {}", lit(locale)?)),
            _ => {}
        }
    }
    if let Some(rules) = &db.icu_rules {
        options.push(format!("ICU_RULES = {}", lit(rules)?));
    }
    options.push(format!("TABLESPACE = {}", ident(&db.tablespace)?));
    options.push(format!("ALLOW_CONNECTIONS = {}", db.allow_connections));
    options.push(format!("CONNECTION LIMIT = {}", db.connection_limit));
    options.push(format!("IS_TEMPLATE = {}", db.is_template));
    let mut script = Script::default();
    script.push(format!(
        "CREATE DATABASE {name} WITH\n    {}",
        options.join("\n    ")
    ));
    comment(
        &mut script,
        &format!("DATABASE {name}"),
        db.comment.as_deref(),
    )?;
    for setting in &db.settings {
        let value = setting_value(&setting.name, &setting.value)?;
        let parameter = ident(&setting.name.to_ascii_lowercase())?;
        match &setting.role {
            None => script.push(format!("ALTER DATABASE {name} SET {parameter} TO {value}")),
            Some(role) => script.push(format!(
                "ALTER ROLE {} IN DATABASE {name} SET {parameter} TO {value}",
                ident(role)?
            )),
        }
    }
    Ok(script.finish())
}

/// `CREATE SCHEMA` with owner and comment.
pub(super) fn schema(schema: &PgSchema) -> CatalogResult<String> {
    let name = ident(&schema.name)?;
    let mut script = Script::default();
    script.push(format!(
        "CREATE SCHEMA {name} AUTHORIZATION {}",
        ident(&schema.owner)?
    ));
    comment(
        &mut script,
        &format!("SCHEMA {name}"),
        schema.comment.as_deref(),
    )?;
    Ok(script.finish())
}

/// `CREATE TABLESPACE` with owner, options and comment. The built-in
/// tablespaces have no location and cannot be created.
pub(super) fn tablespace(ts: &PgTablespace) -> CatalogResult<String> {
    let Some(location) = &ts.location else {
        return Err(crate::model::CatalogError::Invalid {
            message: format!("{} is a built-in tablespace and cannot be created", ts.name),
        });
    };
    let name = ident(&ts.name)?;
    let mut statement = format!(
        "CREATE TABLESPACE {name} OWNER {} LOCATION {}",
        ident(&ts.owner)?,
        lit(location)?
    );
    if !ts.options.is_empty() {
        statement.push_str(&format!(" WITH ({})", option_list(&ts.options, " = ")?));
    }
    let mut script = Script::default();
    script.push(statement);
    comment(
        &mut script,
        &format!("TABLESPACE {name}"),
        ts.comment.as_deref(),
    )?;
    Ok(script.finish())
}

// ---------------------------------------------------------------------------
// Sequences

fn sequence_options(
    data_type: Option<&str>,
    start: Option<i64>,
    increment: Option<i64>,
    min: Option<i64>,
    max: Option<i64>,
    cache: Option<i64>,
    cycle: Option<bool>,
) -> String {
    let mut parts = Vec::new();
    if let Some(data_type) = data_type {
        parts.push(format!("AS {data_type}"));
    }
    if let Some(v) = start {
        parts.push(format!("START WITH {v}"));
    }
    if let Some(v) = increment {
        parts.push(format!("INCREMENT BY {v}"));
    }
    if let Some(v) = min {
        parts.push(format!("MINVALUE {v}"));
    }
    if let Some(v) = max {
        parts.push(format!("MAXVALUE {v}"));
    }
    if let Some(v) = cache {
        parts.push(format!("CACHE {v}"));
    }
    if let Some(cycle) = cycle {
        parts.push(if cycle { "CYCLE" } else { "NO CYCLE" }.to_owned());
    }
    parts.join(" ")
}

/// `CREATE SEQUENCE` with owner and comment, without `OWNED BY`.
fn sequence_create(seq: &PgSequence) -> CatalogResult<Script> {
    let name = qname(&seq.schema, &seq.name)?;
    let mut script = Script::default();
    script.push(format!(
        "CREATE {}SEQUENCE {name} {}",
        if seq.unlogged { "UNLOGGED " } else { "" },
        sequence_options(
            Some(&seq.data_type),
            Some(seq.start),
            Some(seq.increment),
            Some(seq.min),
            Some(seq.max),
            Some(seq.cache),
            Some(seq.cycle),
        )
    ));
    script.push(format!(
        "ALTER SEQUENCE {name} OWNER TO {}",
        ident(&seq.owner)?
    ));
    comment(
        &mut script,
        &format!("SEQUENCE {name}"),
        seq.comment.as_deref(),
    )?;
    Ok(script)
}

fn sequence_owned_by(seq: &PgSequence) -> CatalogResult<Option<String>> {
    let Some(owner) = &seq.owned_by else {
        return Ok(None);
    };
    Ok(Some(format!(
        "ALTER SEQUENCE {} OWNED BY {}.{}",
        qname(&seq.schema, &seq.name)?,
        qname(&owner.schema, &owner.table)?,
        ident(&owner.column)?
    )))
}

/// DDL of a sequence. The sequence of an identity column is created by
/// making the column an identity column again.
pub(super) fn sequence(seq: &PgSequence) -> CatalogResult<String> {
    if let (Some(generation), Some(owner)) = (&seq.identity, &seq.owned_by) {
        let mut script = Script::default();
        script.push(format!(
            "ALTER TABLE {} ALTER COLUMN {} ADD GENERATED {} AS IDENTITY (SEQUENCE NAME {} {})",
            qname(&owner.schema, &owner.table)?,
            ident(&owner.column)?,
            if generation == "always" {
                "ALWAYS"
            } else {
                "BY DEFAULT"
            },
            qname(&seq.schema, &seq.name)?,
            sequence_options(
                None,
                Some(seq.start),
                Some(seq.increment),
                Some(seq.min),
                Some(seq.max),
                Some(seq.cache),
                Some(seq.cycle),
            )
        ));
        comment(
            &mut script,
            &format!("SEQUENCE {}", qname(&seq.schema, &seq.name)?),
            seq.comment.as_deref(),
        )?;
        return Ok(script.finish());
    }
    let mut script = sequence_create(seq)?;
    if let Some(owned) = sequence_owned_by(seq)? {
        script.push(owned);
    }
    Ok(script.finish())
}

// ---------------------------------------------------------------------------
// Columns

fn identity_clause(identity: &PgIdentity) -> CatalogResult<String> {
    let mut options = Vec::new();
    if let Some(seq) = &identity.sequence {
        options.push(format!("SEQUENCE NAME {}", qualified(seq)?));
    }
    let rest = sequence_options(
        None,
        identity.start,
        identity.increment,
        identity.min,
        identity.max,
        identity.cache,
        identity.cycle,
    );
    if !rest.is_empty() {
        options.push(rest);
    }
    Ok(format!(
        "GENERATED {} AS IDENTITY{}",
        if identity.generation == "always" {
            "ALWAYS"
        } else {
            "BY DEFAULT"
        },
        if options.is_empty() {
            String::new()
        } else {
            format!(" ({})", options.join(" "))
        }
    ))
}

/// The inline `NOT NULL` clause of a column; `None` when the column is
/// nullable or its constraint must be added separately (`NOT VALID`).
fn not_null_clause(
    server: &ServerInfo,
    column: &PgColumn,
    constraints: &[PgConstraint],
) -> CatalogResult<Option<String>> {
    if !column.not_null {
        return Ok(None);
    }
    if !server.at_least(18, 0) {
        return Ok(Some("NOT NULL".to_owned()));
    }
    let Some(constraint) = not_null_constraint(column, constraints) else {
        return Ok(Some("NOT NULL".to_owned()));
    };
    if !constraint.validated {
        return Ok(None);
    }
    Ok(Some(format!(
        "CONSTRAINT {} NOT NULL{}",
        ident(&constraint.name)?,
        if constraint.no_inherit {
            " NO INHERIT"
        } else {
            ""
        }
    )))
}

fn not_null_constraint<'a>(
    column: &PgColumn,
    constraints: &'a [PgConstraint],
) -> Option<&'a PgConstraint> {
    constraints
        .iter()
        .find(|c| c.constraint_type == "notNull" && c.columns == [column.name.clone()])
}

/// Which parts of a column definition to print.
#[derive(Debug, Clone, Copy)]
struct ColumnStyle {
    /// `WITH OPTIONS` instead of the type (typed tables).
    typed: bool,
    /// Print the identity clause.
    identity: bool,
}

fn column_element(
    server: &ServerInfo,
    column: &PgColumn,
    constraints: &[PgConstraint],
    style: ColumnStyle,
    script: &mut Script,
) -> CatalogResult<String> {
    let mut parts = vec![ident(&column.name)?];
    if style.typed {
        parts.push("WITH OPTIONS".to_owned());
    } else {
        parts.push(column.data_type.clone());
        if !column.fdw_options.is_empty() {
            parts.push(format!(
                "OPTIONS ({})",
                fdw_option_list(&column.fdw_options, script)?
            ));
        }
        if let Some(collation) = &column.collation {
            parts.push(format!("COLLATE {}", qualified(collation)?));
        }
    }
    if let Some(generated) = &column.generated {
        parts.push(format!(
            "GENERATED ALWAYS AS ({}) {}",
            generated.expression,
            if generated.stored {
                "STORED"
            } else {
                "VIRTUAL"
            }
        ));
    }
    if let Some(default) = &column.default {
        parts.push(format!("DEFAULT {default}"));
    }
    if style.identity
        && let Some(identity) = &column.identity
    {
        parts.push(identity_clause(identity)?);
    }
    if let Some(clause) = not_null_clause(server, column, constraints)? {
        parts.push(clause);
    }
    Ok(parts.join(" "))
}

/// `ALTER ... ALTER COLUMN` statements for storage, compression, statistics
/// and attribute options that differ from the defaults. `alter` is the
/// `ALTER <kind> [ONLY] <name>` prefix.
fn column_settings(
    server: &ServerInfo,
    alter: &str,
    column: &PgColumn,
    script: &mut Script,
) -> CatalogResult<()> {
    let alter = format!("{alter} ALTER COLUMN {}", ident(&column.name)?);
    if column.storage != column.default_storage {
        script.push(format!(
            "{alter} SET STORAGE {}",
            column.storage.to_ascii_uppercase()
        ));
    }
    if server.at_least(14, 0)
        && let Some(compression) = &column.compression
    {
        script.push(format!("{alter} SET COMPRESSION {}", ident(compression)?));
    }
    if let Some(target) = column.statistics_target {
        script.push(format!("{alter} SET STATISTICS {target}"));
    }
    if !column.options.is_empty() {
        script.push(format!(
            "{alter} SET ({})",
            option_list(&column.options, " = ")?
        ));
    }
    Ok(())
}

fn column_comment(target: &Target, column: &PgColumn, script: &mut Script) -> CatalogResult<()> {
    comment(
        script,
        &format!("COLUMN {}.{}", target.name, ident(&column.name)?),
        column.comment.as_deref(),
    )
}

/// DDL of one column: `ADD COLUMN` for tables, the settable parts for
/// views and materialized views (whose columns come from the query).
///
/// The statements use `ALTER TABLE` without `ONLY`: on a table with
/// `INHERITS` children a new column (and its settings) must reach the
/// children as well, which `ONLY` refuses. `sequences` are the sequences
/// owned by the column (a `serial` column's default needs its sequence).
pub(super) fn column(
    server: &ServerInfo,
    target: &Target,
    column: &PgColumn,
    constraints: &[PgConstraint],
    sequences: &[PgSequence],
) -> CatalogResult<String> {
    let mut script = Script::default();
    let alter = target.alter_plain();
    match target.keyword {
        "VIEW" | "MATERIALIZED VIEW" => {
            let mut out = format!(
                "-- Column {} is defined by the query of {}.\n",
                ident(&column.name)?,
                target.name
            );
            if let Some(default) = &column.default {
                script.push(format!(
                    "{alter} ALTER COLUMN {} SET DEFAULT {default}",
                    ident(&column.name)?
                ));
            }
            if target.keyword == "MATERIALIZED VIEW" {
                column_settings(server, &alter, column, &mut script)?;
            }
            column_comment(target, column, &mut script)?;
            if !script.statements.is_empty() {
                out.push_str(&script.finish());
            }
            return Ok(out);
        }
        _ => {
            for seq in sequences {
                script.extend(sequence_create(seq)?);
            }
            let element = column_element(
                server,
                column,
                constraints,
                ColumnStyle {
                    typed: false,
                    identity: true,
                },
                &mut script,
            )?;
            script.push(format!("{alter} ADD COLUMN {element}"));
            if let Some(nn) = not_null_constraint(column, constraints)
                && server.at_least(18, 0)
                && !nn.validated
            {
                script.push(add_constraint(&alter, nn)?);
            }
            column_settings(server, &alter, column, &mut script)?;
            column_comment(target, column, &mut script)?;
            for seq in sequences {
                if let Some(owned) = sequence_owned_by(seq)? {
                    script.push(owned);
                }
            }
        }
    }
    Ok(script.finish())
}

// ---------------------------------------------------------------------------
// Constraints, indexes, triggers, rules, policies

/// `<alter> ADD CONSTRAINT name definition`; `alter` is the `ALTER <kind>
/// [ONLY] <name>` prefix.
fn add_constraint(alter: &str, constraint: &PgConstraint) -> CatalogResult<String> {
    Ok(format!(
        "{alter} ADD CONSTRAINT {} {}",
        ident(&constraint.name)?,
        constraint.definition
    ))
}

fn constraint_comment(
    target: &Target,
    constraint: &PgConstraint,
    script: &mut Script,
) -> CatalogResult<()> {
    comment(
        script,
        &format!("CONSTRAINT {} ON {}", ident(&constraint.name)?, target.name),
        constraint.comment.as_deref(),
    )
}

/// DDL of one constraint, with `ALTER TABLE` without `ONLY`: a check or
/// not-null constraint on a table with `INHERITS` children must reach the
/// children, which `ONLY` refuses; other constraint types are never
/// inherited, so leaving out `ONLY` changes nothing for them.
pub(super) fn constraint(target: &Target, constraint: &PgConstraint) -> CatalogResult<String> {
    let mut script = Script::default();
    script.push(add_constraint(&target.alter_plain(), constraint)?);
    constraint_comment(target, constraint, &mut script)?;
    Ok(script.finish())
}

/// Tablespace, clustering, replica identity and comment of an index.
fn index_extras(
    target: &Target,
    index: &PgIndex,
    with_tablespace: bool,
    script: &mut Script,
) -> CatalogResult<()> {
    let name = qname(&index.schema, &index.name)?;
    if with_tablespace && let Some(ts) = &index.tablespace {
        script.push(format!("ALTER INDEX {name} SET TABLESPACE {}", ident(ts)?));
    }
    if index.clustered {
        script.push(format!(
            "{} CLUSTER ON {}",
            target.alter(),
            ident(&index.name)?
        ));
    }
    if index.replica_identity {
        script.push(format!(
            "{} REPLICA IDENTITY USING INDEX {}",
            target.alter(),
            ident(&index.name)?
        ));
    }
    comment(script, &format!("INDEX {name}"), index.comment.as_deref())
}

/// DDL of one index. An index that backs a constraint is created by
/// adding the constraint (`backing`).
///
/// A partitioned table's index is printed by the server as `CREATE INDEX
/// ... ON ONLY`, which on its own leaves an invalid index while the table
/// has partitions; `children` are the indexes attached to it (parents
/// first), each created on its partition and attached, which makes the
/// partitioned index valid again under the same names as before.
pub(super) fn index(
    target: &Target,
    index: &PgIndex,
    backing: Option<&PgConstraint>,
    children: &[IndexChild],
) -> CatalogResult<String> {
    let mut script = Script::default();
    match backing {
        Some(constraint) => {
            script.push(add_constraint(&target.alter_plain(), constraint)?);
            index_extras(target, index, false, &mut script)?;
            constraint_comment(target, constraint, &mut script)?;
        }
        None => {
            script.push(index.definition.clone());
            for child in children {
                let name = qualified(&child.index)?;
                script.push(child.definition.clone());
                if let Some(ts) = &child.tablespace {
                    script.push(format!("ALTER INDEX {name} SET TABLESPACE {}", ident(ts)?));
                }
                script.push(format!(
                    "ALTER INDEX {} ATTACH PARTITION {name}",
                    qualified(&child.parent)?
                ));
                comment(
                    &mut script,
                    &format!("INDEX {name}"),
                    child.comment.as_deref(),
                )?;
            }
            index_extras(target, index, true, &mut script)?;
        }
    }
    Ok(script.finish())
}

fn trigger_state(target: &Target, trigger: &PgTrigger) -> CatalogResult<Option<String>> {
    let action = match trigger.enabled.as_str() {
        "disabled" => "DISABLE TRIGGER",
        "replica" => "ENABLE REPLICA TRIGGER",
        "always" => "ENABLE ALWAYS TRIGGER",
        _ => return Ok(None),
    };
    Ok(Some(format!(
        "{} {action} {}",
        target.alter_table(),
        ident(&trigger.name)?
    )))
}

fn trigger_comment(target: &Target, trigger: &PgTrigger, script: &mut Script) -> CatalogResult<()> {
    comment(
        script,
        &format!("TRIGGER {} ON {}", ident(&trigger.name)?, target.name),
        trigger.comment.as_deref(),
    )
}

/// DDL of one trigger.
pub(super) fn trigger(target: &Target, trigger: &PgTrigger) -> CatalogResult<String> {
    let mut script = Script::default();
    script.push(trigger.definition.clone());
    if let Some(state) = trigger_state(target, trigger)? {
        script.push(state);
    }
    trigger_comment(target, trigger, &mut script)?;
    Ok(script.finish())
}

fn rule_state(target: &Target, rule: &PgRule) -> CatalogResult<Option<String>> {
    let action = match rule.enabled.as_str() {
        "disabled" => "DISABLE RULE",
        "replica" => "ENABLE REPLICA RULE",
        "always" => "ENABLE ALWAYS RULE",
        _ => return Ok(None),
    };
    Ok(Some(format!(
        "{} {action} {}",
        target.alter_table(),
        ident(&rule.name)?
    )))
}

fn rule_comment(target: &Target, rule: &PgRule, script: &mut Script) -> CatalogResult<()> {
    comment(
        script,
        &format!("RULE {} ON {}", ident(&rule.name)?, target.name),
        rule.comment.as_deref(),
    )
}

/// DDL of one rule.
pub(super) fn rule(target: &Target, rule: &PgRule) -> CatalogResult<String> {
    let mut script = Script::default();
    script.push(rule.definition.clone());
    if let Some(state) = rule_state(target, rule)? {
        script.push(state);
    }
    rule_comment(target, rule, &mut script)?;
    Ok(script.finish())
}

fn policy_create(target: &Target, policy: &PgPolicy) -> CatalogResult<String> {
    let roles = policy
        .roles
        .iter()
        .map(|r| {
            if r == "public" {
                Ok("PUBLIC".to_owned())
            } else {
                ident(r)
            }
        })
        .collect::<CatalogResult<Vec<_>>>()?;
    let mut statement = format!(
        "CREATE POLICY {} ON {} AS {} FOR {} TO {}",
        ident(&policy.name)?,
        target.name,
        if policy.permissive {
            "PERMISSIVE"
        } else {
            "RESTRICTIVE"
        },
        policy.command.to_ascii_uppercase(),
        roles.join(", ")
    );
    if let Some(using) = &policy.using {
        statement.push_str(&format!(" USING ({using})"));
    }
    if let Some(check) = &policy.with_check {
        statement.push_str(&format!(" WITH CHECK ({check})"));
    }
    Ok(statement)
}

fn policy_comment(target: &Target, policy: &PgPolicy, script: &mut Script) -> CatalogResult<()> {
    comment(
        script,
        &format!("POLICY {} ON {}", ident(&policy.name)?, target.name),
        policy.comment.as_deref(),
    )
}

/// DDL of one policy.
pub(super) fn policy(target: &Target, policy: &PgPolicy) -> CatalogResult<String> {
    let mut script = Script::default();
    script.push(policy_create(target, policy)?);
    policy_comment(target, policy, &mut script)?;
    Ok(script.finish())
}

// ---------------------------------------------------------------------------
// Tables

/// The full DDL of a table, partitioned table, partition or foreign table,
/// in the shape `pg_dump` uses: sequences owned by its columns, `CREATE
/// TABLE` with columns and validated check constraints, owner, column
/// settings, the remaining constraints, indexes, foreign keys, the
/// partition attach, triggers, rules, row level security, policies,
/// replica identity, comments and sequence ownership.
///
/// An `INHERITS` child is created with its local columns and `INHERITS`,
/// as `pg_dump` does, which keeps every column's and constraint's
/// inheritance exactly as on the source. The one thing that cannot be
/// reproduced that way (by `pg_dump` either) is column order after a
/// parent gained a column once the child existed: such a column sits at
/// the end of the source child, but with the inherited columns in the copy.
///
/// A partition is created as a standalone table with every column,
/// constraint and index it has, then attached: the server then marks the
/// columns and constraints as inherited and attaches the indexes, exactly
/// as on the source.
pub(super) fn table(server: &ServerInfo, table: &PgTable) -> CatalogResult<String> {
    let target = Target::table(table)?;
    let is_partition = table.partition_of.is_some();
    let typed = table.of_type.is_some();
    let mut script = Script::default();

    for seq in &table.owned_sequences {
        script.extend(sequence_create(seq)?);
    }

    // Which columns and constraints this script creates itself (the rest
    // come from an INHERITS parent).
    let prints_column = |c: &PgColumn| is_partition || table.inherits.is_empty() || c.local;
    let prints_constraint = |c: &PgConstraint| is_partition || table.inherits.is_empty() || c.local;

    let mut elements = Vec::new();
    let mut inline_not_null: HashSet<String> = HashSet::new();
    for column in table.columns.iter().filter(|c| prints_column(c)) {
        if typed
            && column.default.is_none()
            && !column.not_null
            && column.generated.is_none()
            && column.identity.is_none()
        {
            continue;
        }
        let element = column_element(
            server,
            column,
            &table.constraints,
            ColumnStyle {
                typed,
                identity: !is_partition,
            },
            &mut script,
        )?;
        if let Some(nn) = not_null_constraint(column, &table.constraints)
            && nn.validated
        {
            inline_not_null.insert(nn.name.clone());
        }
        elements.push(element);
    }
    let mut inline_checks: HashSet<String> = HashSet::new();
    for constraint in table
        .constraints
        .iter()
        .filter(|c| c.constraint_type == "check" && c.validated && prints_constraint(c))
    {
        elements.push(format!(
            "CONSTRAINT {} {}",
            ident(&constraint.name)?,
            constraint.definition
        ));
        inline_checks.insert(constraint.name.clone());
    }

    let mut create = format!(
        "CREATE {}{} {}",
        if table.unlogged { "UNLOGGED " } else { "" },
        target.keyword,
        target.name
    );
    if let Some(of_type) = &table.of_type {
        create.push_str(&format!(" OF {of_type}"));
        if !elements.is_empty() {
            create.push_str(&format!(" (\n    {}\n)", elements.join(",\n    ")));
        }
    } else {
        create.push_str(&format!(" (\n    {}\n)", elements.join(",\n    ")));
    }
    if !is_partition && !table.inherits.is_empty() {
        let parents = table
            .inherits
            .iter()
            .map(qualified)
            .collect::<CatalogResult<Vec<_>>>()?;
        create.push_str(&format!("\nINHERITS ({})", parents.join(", ")));
    }
    if let Some(partitioning) = &table.partitioning {
        create.push_str(&format!("\nPARTITION BY {}", partitioning.key));
    }
    if let Some(am) = &table.access_method {
        create.push_str(&format!("\nUSING {}", ident(am)?));
    }
    if !table.storage_parameters.is_empty() {
        create.push_str(&format!(
            "\nWITH ({})",
            option_list(&table.storage_parameters, " = ")?
        ));
    }
    if let Some(ts) = &table.tablespace {
        create.push_str(&format!("\nTABLESPACE {}", ident(ts)?));
    }
    if let Some(foreign) = &table.foreign {
        create.push_str(&format!("\nSERVER {}", ident(&foreign.server)?));
        if !foreign.options.is_empty() {
            create.push_str(&format!(
                "\nOPTIONS ({})",
                fdw_option_list(&foreign.options, &mut script)?
            ));
        }
    }
    script.push(create);
    script.push(format!(
        "{} OWNER TO {}",
        target.alter_plain(),
        ident(&table.owner)?
    ));

    // Inherited columns of an INHERITS child keep their own defaults and
    // NOT NULL.
    for column in table.columns.iter().filter(|c| !prints_column(c)) {
        let alter = format!("{} ALTER COLUMN {}", target.alter(), ident(&column.name)?);
        if let Some(default) = &column.default {
            script.push(format!("{alter} SET DEFAULT {default}"));
        }
        if column.not_null {
            let local = if server.at_least(18, 0) {
                not_null_constraint(column, &table.constraints).is_some_and(|c| c.local)
            } else {
                true
            };
            if local {
                script.push(format!("{alter} SET NOT NULL"));
                if let Some(nn) = not_null_constraint(column, &table.constraints) {
                    inline_not_null.insert(nn.name.clone());
                }
            }
        }
    }
    for column in &table.columns {
        column_settings(server, &target.alter(), column, &mut script)?;
    }

    // Constraints that are not inline: NOT VALID not-null and check
    // constraints, then primary key, unique and exclusion constraints
    // (except those an attach clones again).
    for constraint in table.constraints.iter().filter(|c| prints_constraint(c)) {
        let separate = match constraint.constraint_type.as_str() {
            "notNull" => {
                !inline_not_null.contains(&constraint.name) && (constraint.local || is_partition)
            }
            "check" => !inline_checks.contains(&constraint.name),
            "primaryKey" | "unique" | "exclusion" => !attach_clones(is_partition, constraint),
            _ => false,
        };
        if separate {
            script.push(add_constraint(&target.alter(), constraint)?);
        }
    }
    for index in table.indexes.iter().filter(|i| i.constraint.is_none()) {
        script.push(index.definition.clone());
    }
    for index in &table.indexes {
        if let Some(ts) = &index.tablespace
            && index.constraint.is_none()
        {
            script.push(format!(
                "ALTER INDEX {} SET TABLESPACE {}",
                qname(&index.schema, &index.name)?,
                ident(ts)?
            ));
        }
    }
    for constraint in table.constraints.iter().filter(|c| {
        c.constraint_type == "foreignKey" && prints_constraint(c) && !attach_clones(is_partition, c)
    }) {
        script.push(add_constraint(&target.alter(), constraint)?);
    }

    if let Some(partition_of) = &table.partition_of {
        script.push(format!(
            "ALTER TABLE {} ATTACH PARTITION {} {}",
            qualified(&partition_of.parent)?,
            target.name,
            partition_of.bound
        ));
    }

    for index in table.indexes.iter().filter(|i| i.clustered) {
        script.push(format!(
            "{} CLUSTER ON {}",
            target.alter(),
            ident(&index.name)?
        ));
    }
    for trigger in &table.triggers {
        if !trigger.inherited {
            script.push(trigger.definition.clone());
        }
    }
    for trigger in &table.triggers {
        if let Some(state) = trigger_state(&target, trigger)? {
            script.push(state);
        }
    }
    for rule in &table.rules {
        script.push(rule.definition.clone());
        if let Some(state) = rule_state(&target, rule)? {
            script.push(state);
        }
    }
    if table.rls_enabled {
        script.push(format!("{} ENABLE ROW LEVEL SECURITY", target.alter()));
    }
    if table.rls_forced {
        script.push(format!("{} FORCE ROW LEVEL SECURITY", target.alter()));
    }
    for policy in &table.policies {
        script.push(policy_create(&target, policy)?);
    }
    // Foreign tables always report NOTHING and cannot change it.
    let replica_identity = if table.foreign.is_some() {
        "default"
    } else {
        table.replica_identity.as_str()
    };
    match replica_identity {
        "full" => script.push(format!("{} REPLICA IDENTITY FULL", target.alter())),
        "nothing" => script.push(format!("{} REPLICA IDENTITY NOTHING", target.alter())),
        "index" => {
            if let Some(index) = table.indexes.iter().find(|i| i.replica_identity) {
                script.push(format!(
                    "{} REPLICA IDENTITY USING INDEX {}",
                    target.alter(),
                    ident(&index.name)?
                ));
            }
        }
        _ => {}
    }

    comment(
        &mut script,
        &format!("{} {}", target.keyword, target.name),
        table.comment.as_deref(),
    )?;
    for column in &table.columns {
        column_comment(&target, column, &mut script)?;
    }
    for constraint in &table.constraints {
        constraint_comment(&target, constraint, &mut script)?;
    }
    for index in &table.indexes {
        comment(
            &mut script,
            &format!("INDEX {}", qname(&index.schema, &index.name)?),
            index.comment.as_deref(),
        )?;
    }
    for trigger in &table.triggers {
        trigger_comment(&target, trigger, &mut script)?;
    }
    for rule in &table.rules {
        rule_comment(&target, rule, &mut script)?;
    }
    for policy in &table.policies {
        policy_comment(&target, policy, &mut script)?;
    }
    for seq in &table.owned_sequences {
        if let Some(owned) = sequence_owned_by(seq)? {
            script.push(owned);
        }
    }
    Ok(script.finish())
}

/// True when `ATTACH PARTITION` recreates this constraint of a partition
/// by itself.
///
/// The server marks primary key, unique, exclusion and foreign key
/// constraints it cloned from the partitioned table with `connoinherit`
/// off; constraints that existed on the table before it was attached keep
/// it on. Adding a cloned one before the attach would turn it into an
/// adopted one, so those are left to the attach, which clones them again
/// under the same name. Adopted ones are added explicitly, as they were.
fn attach_clones(is_partition: bool, constraint: &PgConstraint) -> bool {
    is_partition && constraint.has_parent_constraint && !constraint.no_inherit
}

// ---------------------------------------------------------------------------
// Views

/// DDL of a view with owner, column defaults, triggers, rules and comments.
pub(super) fn view(view: &PgView) -> CatalogResult<String> {
    let target = Target::view(&view.schema, &view.name)?;
    let mut script = Script::default();
    let mut create = format!("CREATE VIEW {}", target.name);
    if !view.options.is_empty() {
        create.push_str(&format!(" WITH ({})", option_list(&view.options, " = ")?));
    }
    create.push_str(&format!(" AS\n{}", view.definition));
    script.push(create);
    script.push(format!(
        "ALTER VIEW {} OWNER TO {}",
        target.name,
        ident(&view.owner)?
    ));
    for column in &view.columns {
        if let Some(default) = &column.default {
            script.push(format!(
                "ALTER VIEW {} ALTER COLUMN {} SET DEFAULT {default}",
                target.name,
                ident(&column.name)?
            ));
        }
    }
    for trigger in &view.triggers {
        script.push(trigger.definition.clone());
    }
    for rule in &view.rules {
        script.push(rule.definition.clone());
    }
    comment(
        &mut script,
        &format!("VIEW {}", target.name),
        view.comment.as_deref(),
    )?;
    for column in &view.columns {
        column_comment(&target, column, &mut script)?;
    }
    for trigger in &view.triggers {
        trigger_comment(&target, trigger, &mut script)?;
    }
    for rule in &view.rules {
        rule_comment(&target, rule, &mut script)?;
    }
    Ok(script.finish())
}

/// DDL of a materialized view with owner, column settings, indexes and
/// comments.
pub(super) fn materialized_view(
    server: &ServerInfo,
    view: &PgMaterializedView,
) -> CatalogResult<String> {
    let target = Target::materialized_view(&view.schema, &view.name)?;
    let mut script = Script::default();
    let mut create = format!("CREATE MATERIALIZED VIEW {}", target.name);
    if let Some(am) = &view.access_method {
        create.push_str(&format!("\nUSING {}", ident(am)?));
    }
    if !view.storage_parameters.is_empty() {
        create.push_str(&format!(
            "\nWITH ({})",
            option_list(&view.storage_parameters, " = ")?
        ));
    }
    if let Some(ts) = &view.tablespace {
        create.push_str(&format!("\nTABLESPACE {}", ident(ts)?));
    }
    create.push_str(&format!(
        " AS\n{}\nWITH {}DATA",
        view.definition,
        if view.populated { "" } else { "NO " }
    ));
    script.push(create);
    script.push(format!(
        "ALTER MATERIALIZED VIEW {} OWNER TO {}",
        target.name,
        ident(&view.owner)?
    ));
    for column in &view.columns {
        column_settings(server, &target.alter(), column, &mut script)?;
    }
    for index in &view.indexes {
        script.push(index.definition.clone());
        index_extras(&target, index, true, &mut script)?;
    }
    comment(
        &mut script,
        &format!("MATERIALIZED VIEW {}", target.name),
        view.comment.as_deref(),
    )?;
    for column in &view.columns {
        column_comment(&target, column, &mut script)?;
    }
    Ok(script.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_end_every_statement() {
        let mut script = Script::default();
        script.push("SELECT 1");
        script.push("SELECT 2");
        assert_eq!(script.finish(), "SELECT 1;\n\nSELECT 2;\n");
    }
}
