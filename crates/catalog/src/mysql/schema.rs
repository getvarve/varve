//! Databases, tables and what hangs off a table: columns, indexes,
//! constraints, partitions and triggers.
//!
//! Every list is one catalog query; a table's details are one round trip
//! with one statement per part.

use std::collections::HashMap;

use varve_driver::{ServerInfo, Session};

use super::text::{self, Lex, TableItem};
use super::types::{
    MyCheckLevel, MyColumn, MyConstraint, MyConstraintType, MyDatabase, MyDefault, MyGenerated,
    MyGeneratedKind, MyIndex, MyIndexKind, MyKeyPart, MyPartition, MyPartitioning, MyPeriod,
    MyReference, MySystemVersioning, MyTable, MyTrigger,
};
use super::util::{
    at_least, charset_of, human_size, id, is_mariadb, is_system_db, lit, object_ref, query_all,
    query_one, req, take, text, text_nonempty, uint, uint32, yes,
};
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectSummary};
use crate::rows::Rows;

// ---------------------------------------------------------------- databases

fn databases_sql(server: &ServerInfo, name: Option<&str>) -> CatalogResult<String> {
    let maria = is_mariadb(server);
    let encryption = if !maria && at_least(server, 8, 0, 16) {
        "s.DEFAULT_ENCRYPTION"
    } else {
        "NULL"
    };
    let (read_only, read_only_join) = if !maria && at_least(server, 8, 0, 22) {
        (
            "x.OPTIONS",
            " LEFT JOIN information_schema.SCHEMATA_EXTENSIONS x ON x.SCHEMA_NAME = s.SCHEMA_NAME",
        )
    } else {
        ("NULL", "")
    };
    let comment = if maria && at_least(server, 10, 5, 0) {
        "s.SCHEMA_COMMENT"
    } else {
        "NULL"
    };
    let filter = match name {
        Some(name) => format!(" WHERE s.SCHEMA_NAME = {}", lit(name)?),
        None => String::new(),
    };
    Ok(format!(
        "SELECT s.SCHEMA_NAME AS `name`, s.DEFAULT_CHARACTER_SET_NAME AS `charset`, \
         s.DEFAULT_COLLATION_NAME AS `collation`, {encryption} AS `encryption`, \
         {read_only} AS `options`, {comment} AS `comment` \
         FROM information_schema.SCHEMATA s{read_only_join}{filter} ORDER BY s.SCHEMA_NAME"
    ))
}

fn parse_database(row: &crate::rows::Row<'_>) -> CatalogResult<MyDatabase> {
    let name = req(row, "name")?;
    let encryption = text(row, "encryption")?.map(|e| e.eq_ignore_ascii_case("YES"));
    let read_only = text(row, "options")?.map(|o| {
        o.to_ascii_uppercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("READ ONLY=1")
    });
    Ok(MyDatabase {
        system: is_system_db(&name),
        charset: text_nonempty(row, "charset")?,
        collation: text_nonempty(row, "collation")?,
        encryption,
        read_only,
        comment: text_nonempty(row, "comment")?,
        size_bytes: None,
        table_count: None,
        name,
    })
}

/// The read-only state when the server has the option at all: a schema
/// without options is writable.
fn read_only_default(server: &ServerInfo, value: Option<bool>) -> Option<bool> {
    if !is_mariadb(server) && at_least(server, 8, 0, 22) {
        Some(value.unwrap_or(false))
    } else {
        None
    }
}

fn database_summary(db: &MyDatabase) -> ObjectSummary {
    let mut flags = Vec::new();
    if db.read_only == Some(true) {
        flags.push("read only".to_owned());
    }
    if db.encryption == Some(true) {
        flags.push("encrypted".to_owned());
    }
    ObjectSummary {
        object: crate::model::ObjectRef {
            kind: ObjectKind::Database,
            database: Some(db.name.clone()),
            schema: None,
            parent: None,
            name: db.name.clone(),
            signature: None,
        },
        comment: db.comment.clone(),
        detail: db.collation.clone(),
        flags,
        has_children: true,
        system: db.system,
        row_estimate: None,
        size_bytes: None,
    }
}

/// Databases on the server, system databases marked.
pub(crate) async fn databases(
    session: &dyn Session,
    server: &ServerInfo,
) -> CatalogResult<Vec<ObjectSummary>> {
    let rows = query_one(session, &databases_sql(server, None)?, "the database").await?;
    rows.iter()
        .map(|row| {
            let mut db = parse_database(&row)?;
            db.read_only = read_only_default(server, db.read_only);
            Ok(database_summary(&db))
        })
        .collect()
}

/// Details of one database, with the size and count of its tables.
pub(crate) async fn describe_database(
    session: &dyn Session,
    server: &ServerInfo,
    name: &str,
) -> CatalogResult<MyDatabase> {
    let sql = format!(
        "{};\nSELECT SUM(COALESCE(DATA_LENGTH, 0) + COALESCE(INDEX_LENGTH, 0)) AS `size`, \
         COUNT(*) AS `tables` FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} \
         AND TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED')",
        databases_sql(server, Some(name))?,
        lit(name)?
    );
    let mut sets = query_all(session, &sql, "the database").await?;
    let dbs = take(&mut sets, 0)?;
    let mut db = parse_database(&dbs.single(&format!("database {name}"))?)?;
    db.read_only = read_only_default(server, db.read_only);
    let sizes = take(&mut sets, 1)?;
    let row = sizes.single("database size")?;
    db.size_bytes = Some(uint(&row, "size")?.unwrap_or(0));
    db.table_count = Some(uint(&row, "tables")?.unwrap_or(0));
    Ok(db)
}

// ------------------------------------------------------------------- tables

fn tables_sql(database: &str, name: Option<&str>) -> CatalogResult<String> {
    let filter = match name {
        Some(name) => format!(" AND t.TABLE_NAME = {}", lit(name)?),
        None => String::new(),
    };
    Ok(format!(
        "SELECT t.TABLE_NAME AS `name`, t.TABLE_TYPE AS `table_type`, t.ENGINE AS `engine`, \
         t.ROW_FORMAT AS `row_format`, t.AUTO_INCREMENT AS `auto_increment`, \
         t.TABLE_COLLATION AS `collation`, t.TABLE_COMMENT AS `comment`, \
         t.CREATE_OPTIONS AS `create_options`, t.TABLE_ROWS AS `row_estimate`, \
         t.AVG_ROW_LENGTH AS `avg_row_length`, t.DATA_LENGTH AS `data_length`, \
         t.INDEX_LENGTH AS `index_length`, t.DATA_FREE AS `data_free`, \
         t.CREATE_TIME AS `created`, t.UPDATE_TIME AS `updated` \
         FROM information_schema.TABLES t WHERE t.TABLE_SCHEMA = {} \
         AND t.TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED'){filter} ORDER BY t.TABLE_NAME",
        lit(database)?
    ))
}

fn parse_table(row: &crate::rows::Row<'_>, database: &str) -> CatalogResult<MyTable> {
    let collation = text_nonempty(row, "collation")?;
    let table_type = req(row, "table_type")?;
    Ok(MyTable {
        database: database.to_owned(),
        name: req(row, "name")?,
        system_versioning: table_type
            .eq_ignore_ascii_case("SYSTEM VERSIONED")
            .then_some(MySystemVersioning {
                row_start: None,
                row_end: None,
            }),
        table_type,
        engine: text_nonempty(row, "engine")?,
        row_format: text_nonempty(row, "row_format")?,
        auto_increment: uint(row, "auto_increment")?,
        charset: collation.as_deref().map(charset_of),
        collation,
        comment: text_nonempty(row, "comment")?,
        create_options: text_nonempty(row, "create_options")?,
        row_estimate: uint(row, "row_estimate")?,
        avg_row_length: uint(row, "avg_row_length")?,
        data_length: uint(row, "data_length")?,
        index_length: uint(row, "index_length")?,
        data_free: uint(row, "data_free")?,
        created: text(row, "created")?,
        updated: text(row, "updated")?,
        periods: Vec::new(),
        columns: Vec::new(),
        indexes: Vec::new(),
        constraints: Vec::new(),
        partitioning: None,
        triggers: Vec::new(),
        system: is_system_db(database),
    })
}

fn table_summary(table: &MyTable) -> ObjectSummary {
    let mut flags = Vec::new();
    if table
        .create_options
        .as_deref()
        .is_some_and(|o| o.to_ascii_lowercase().contains("partitioned"))
    {
        flags.push("partitioned".to_owned());
    }
    if table.system_versioning.is_some() {
        flags.push("system versioned".to_owned());
    }
    let mut detail = table.engine.clone().unwrap_or_default();
    if let Some(rows) = table.row_estimate {
        if !detail.is_empty() {
            detail.push_str(", ");
        }
        detail.push_str(&format!("~{rows} rows"));
    }
    if let (Some(data), Some(index)) = (table.data_length, table.index_length) {
        if !detail.is_empty() {
            detail.push_str(", ");
        }
        detail.push_str(&human_size(data.saturating_add(index)));
    }
    ObjectSummary {
        object: object_ref(ObjectKind::Table, &table.database, None, &table.name),
        comment: table.comment.clone(),
        detail: (!detail.is_empty()).then_some(detail),
        flags,
        has_children: true,
        system: table.system,
        row_estimate: table.row_estimate,
        size_bytes: match (table.data_length, table.index_length) {
            (Some(data), Some(index)) => Some(data.saturating_add(index)),
            (data, index) => data.or(index),
        },
    }
}

/// Base tables of a database.
pub(crate) async fn list_tables(
    session: &dyn Session,
    database: &str,
) -> CatalogResult<Vec<ObjectSummary>> {
    let rows = query_one(session, &tables_sql(database, None)?, "the database").await?;
    rows.iter()
        .map(|row| Ok(table_summary(&parse_table(&row, database)?)))
        .collect()
}

/// Full details of one table: options, columns, indexes, constraints,
/// partitioning, triggers and (MariaDB) periods.
pub(crate) async fn describe_table(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
    name: &str,
) -> CatalogResult<MyTable> {
    let maria = is_mariadb(server);
    // One round trip; each part is a run of statements in the batch.
    let mut statements = vec![tables_sql(database, Some(name))?];
    let part = |list: Vec<String>, statements: &mut Vec<String>| {
        let range = statements.len()..statements.len() + list.len();
        statements.extend(list);
        range
    };
    let columns = part(
        columns_statements(server, database, name, None)?,
        &mut statements,
    );
    let indexes = part(
        vec![indexes_sql(server, database, name, None)?],
        &mut statements,
    );
    let constraints = part(
        constraints_statements(server, database, name, None)?,
        &mut statements,
    );
    let partitions = part(vec![partitions_sql(database, name)?], &mut statements);
    let triggers = part(
        vec![triggers_sql(server, database, Some(name))?],
        &mut statements,
    );
    // The DDL gives what `information_schema` lacks: MariaDB periods and
    // the full-text parser of an index (`WITH PARSER`).
    let create = part(
        vec![
            "SELECT @@SESSION.sql_mode AS `mode`".to_owned(),
            format!("SHOW CREATE TABLE {}", super::util::qname(database, name)?),
        ],
        &mut statements,
    );
    let what = format!("table {database}.{name}");
    let sets = query_all(session, &statements.join(";\n"), &what).await?;
    let range = |r: std::ops::Range<usize>| {
        sets.get(r).ok_or_else(|| CatalogError::Unexpected {
            message: format!("results of {what} are missing"),
        })
    };
    let tables = range(0..1)?;
    let mut table = parse_table(&tables[0].single(&what)?, database)?;
    let mut system_time = HashMap::new();
    let mut index_ddl;
    {
        let create = range(create)?;
        let mode = create[0]
            .iter()
            .next()
            .map(|r| text(&r, "mode"))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        let lex = Lex::for_sql_mode(&mode, maria);
        let ddl = create[1]
            .iter()
            .next()
            .map(|r| text(&r, "Create Table"))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        index_ddl = index_ddl_of(&ddl, lex, server);
        let periods = if maria {
            periods_of(&ddl, lex)
        } else {
            Vec::new()
        };
        for (period, start, end) in periods {
            if period.eq_ignore_ascii_case("SYSTEM_TIME") {
                system_time.insert(start.clone(), MyGeneratedKind::RowStart);
                system_time.insert(end.clone(), MyGeneratedKind::RowEnd);
                if let Some(versioning) = table.system_versioning.as_mut() {
                    versioning.row_start = Some(start);
                    versioning.row_end = Some(end);
                }
            } else {
                table.periods.push(MyPeriod {
                    name: period,
                    start_column: start,
                    end_column: end,
                });
            }
        }
    }
    table.columns = parse_columns(range(columns)?, server, &system_time)?;
    table.indexes = parse_indexes(&range(indexes)?[0])?;
    for index in &mut table.indexes {
        let ddl = index_ddl.remove(&index.name).unwrap_or_default();
        index.parser = ddl.parser;
        index.method_explicit = ddl.explicit_method;
    }
    table.constraints = parse_constraints(range(constraints)?, server, database)?;
    mark_period_checks(&mut table.constraints, &table.periods);
    table.partitioning = parse_partitioning(&range(partitions)?[0])?;
    table.triggers = parse_triggers(&range(triggers)?[0], database)?
        .into_iter()
        .map(|t| t.name)
        .collect();
    Ok(table)
}

/// What `SHOW CREATE TABLE` writes for an index beyond `information_schema`.
#[derive(Debug, Default, PartialEq, Eq)]
struct IndexDdl {
    /// `WITH PARSER plugin`.
    parser: Option<String>,
    /// `USING BTREE` / `USING HASH` is written (the method was given, not
    /// the engine's default).
    explicit_method: bool,
}

/// The parser and explicit method of each index of a `CREATE TABLE`
/// statement (`FULLTEXT KEY ft (f) /*!50100 WITH PARSER `ngram` */`,
/// `KEY kb (k) USING BTREE`), by index name.
fn index_ddl_of(ddl: &str, lex: Lex, server: &ServerInfo) -> HashMap<String, IndexDdl> {
    let version = server.major * 10_000 + server.minor * 100 + server.patch.unwrap_or(0);
    let ddl = text::unwrap_exec_comments(ddl, lex, version);
    let Some(parts) = text::table_parts(&ddl, lex) else {
        return HashMap::new();
    };
    parts
        .items
        .iter()
        .filter_map(|item| {
            let TableItem::Index(name) = text::table_item(item, lex) else {
                return None;
            };
            let toks: Vec<_> = text::tokens(item, lex)
                .into_iter()
                .filter(text::Tok::significant)
                .collect();
            let mut found = IndexDdl::default();
            for w in toks.windows(2) {
                if let [using, method] = w
                    && using.is_word(item, "USING")
                    && (method.is_word(item, "BTREE") || method.is_word(item, "HASH"))
                {
                    found.explicit_method = true;
                }
            }
            found.parser = toks.windows(3).find_map(|w| match w {
                [with, parser, plugin]
                    if with.is_word(item, "WITH") && parser.is_word(item, "PARSER") =>
                {
                    text::name_of(item, plugin)
                }
                _ => None,
            });
            Some((name, found))
        })
        .collect()
}

/// The periods of a `CREATE TABLE` statement: `(name, start, end)`.
fn periods_of(ddl: &str, lex: Lex) -> Vec<(String, String, String)> {
    text::table_parts(ddl, lex)
        .map(|parts| {
            parts
                .items
                .iter()
                .filter_map(|item| match text::table_item(item, lex) {
                    TableItem::Period { name, start, end } => Some((name, start, end)),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------ columns

/// The statements that read columns: `COLUMNS`, and on MariaDB also
/// `GEOMETRY_COLUMNS` (SRIDs) and `CHECK_CONSTRAINTS` (column-level checks).
///
/// Each statement reads one `information_schema` table with constant
/// database and table filters, and the results are joined in Rust: MariaDB
/// only skips other tables' files for constant filters on the table it
/// reads, and a join (or a derived table, which it merges) makes it open
/// every table of the server.
fn columns_statements(
    server: &ServerInfo,
    database: &str,
    table: &str,
    column: Option<&str>,
) -> CatalogResult<Vec<String>> {
    let db = lit(database)?;
    let tb = lit(table)?;
    let maria = is_mariadb(server);
    let srid = if !maria && at_least(server, 8, 0, 3) {
        "SRS_ID"
    } else {
        "NULL"
    };
    let filter = match column {
        Some(column) => format!(" AND COLUMN_NAME = {}", lit(column)?),
        None => String::new(),
    };
    let mut out = vec![format!(
        "SELECT TABLE_NAME AS `table_name`, COLUMN_NAME AS `name`, \
         ORDINAL_POSITION AS `position`, DATA_TYPE AS `data_type`, \
         COLUMN_TYPE AS `column_type`, IS_NULLABLE AS `nullable`, \
         COLUMN_DEFAULT AS `dflt`, EXTRA AS `extra`, \
         GENERATION_EXPRESSION AS `gen_expr`, CHARACTER_SET_NAME AS `charset`, \
         COLLATION_NAME AS `collation`, COLUMN_COMMENT AS `comment`, \
         COLUMN_KEY AS `col_key`, {srid} AS `srid` \
         FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {tb}{filter} \
         ORDER BY ORDINAL_POSITION"
    )];
    if maria {
        // GEOMETRY_COLUMNS is looked up by its F_ columns.
        out.push(format!(
            "SELECT G_GEOMETRY_COLUMN AS `name`, SRID AS `srid` \
             FROM information_schema.GEOMETRY_COLUMNS \
             WHERE F_TABLE_SCHEMA = {db} AND F_TABLE_NAME = {tb}"
        ));
        out.push(format!(
            "SELECT CONSTRAINT_NAME AS `name`, CHECK_CLAUSE AS `check_clause` \
             FROM information_schema.CHECK_CONSTRAINTS \
             WHERE CONSTRAINT_SCHEMA = {db} AND TABLE_NAME = {tb} AND LEVEL = 'Column'"
        ));
    }
    Ok(out)
}

/// True for numeric and bit literals as MariaDB writes defaults.
fn is_number_literal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("b'") || lower.starts_with("x'") || lower.starts_with("0x") {
        return true;
    }
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '-' | '+'))
        && body
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '.')
}

/// The `ON UPDATE` expression in an `EXTRA` value.
fn on_update_of(extra: &str) -> Option<String> {
    let lower = extra.to_ascii_lowercase();
    let at = lower.find("on update ")?;
    let rest = extra.get(at + "on update ".len()..)?;
    let value: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ',')
        .collect();
    (!value.is_empty()).then_some(value)
}

/// Reads the results of [`columns_statements`].
fn parse_columns(
    sets: &[Rows],
    server: &ServerInfo,
    system_time: &HashMap<String, MyGeneratedKind>,
) -> CatalogResult<Vec<MyColumn>> {
    let maria = is_mariadb(server);
    let mut srids: HashMap<String, u32> = HashMap::new();
    let mut checks: HashMap<String, String> = HashMap::new();
    if maria {
        for row in sets.get(1).into_iter().flat_map(Rows::iter) {
            if let (Some(name), Some(srid)) = (text(&row, "name")?, uint32(&row, "srid")?) {
                srids.insert(name, srid);
            }
        }
        for row in sets.get(2).into_iter().flat_map(Rows::iter) {
            if let (Some(name), Some(clause)) =
                (text(&row, "name")?, text_nonempty(&row, "check_clause")?)
            {
                checks.insert(name, clause);
            }
        }
    }
    let rows = sets.first().ok_or_else(|| CatalogError::Unexpected {
        message: "the column query returned nothing".into(),
    })?;
    rows.iter()
        .map(|row| {
            let name = req(&row, "name")?;
            let extra = text(&row, "extra")?.unwrap_or_default();
            let extra_lower = extra.to_ascii_lowercase();
            let nullable = yes(&row, "nullable")?;
            let data_type = req(&row, "data_type")?.to_ascii_lowercase();
            let expression = text_nonempty(&row, "gen_expr")?;
            let generated = if extra_lower.contains("virtual generated") {
                Some(MyGeneratedKind::Virtual)
            } else if extra_lower.contains("stored generated")
                || extra_lower.contains("persistent generated")
            {
                Some(MyGeneratedKind::Stored)
            } else {
                None
            }
            .map(|kind| {
                // MariaDB shows system-time columns as generated with the
                // expression `ROW START` / `ROW END` (or none on some
                // versions, then the table's PERIOD FOR SYSTEM_TIME tells).
                let row_time = expression.as_deref().and_then(|e| {
                    if e.eq_ignore_ascii_case("ROW START") {
                        Some(MyGeneratedKind::RowStart)
                    } else if e.eq_ignore_ascii_case("ROW END") {
                        Some(MyGeneratedKind::RowEnd)
                    } else {
                        None
                    }
                });
                if let Some(kind) = row_time.filter(|_| maria) {
                    return MyGenerated {
                        generated_kind: kind,
                        expression: None,
                    };
                }
                let kind = if maria && expression.is_none() {
                    system_time.get(&name).copied().unwrap_or(kind)
                } else {
                    kind
                };
                MyGenerated {
                    generated_kind: kind,
                    expression: expression.clone(),
                }
            });
            let auto_increment = extra_lower.contains("auto_increment");
            let raw_default = text(&row, "dflt")?;
            let null_default = nullable && generated.is_none() && !auto_increment;
            let default = if maria {
                match raw_default {
                    None => null_default.then_some(MyDefault::Null),
                    Some(d) if d.eq_ignore_ascii_case("NULL") => Some(MyDefault::Null),
                    Some(d) if d.len() >= 2 && d.starts_with('\'') && d.ends_with('\'') => {
                        let inner = d.get(1..d.len() - 1).unwrap_or("");
                        Some(MyDefault::Literal(text::unescape(inner, '\'', true)))
                    }
                    Some(d) if is_number_literal(&d) => Some(MyDefault::Literal(d)),
                    Some(d) => Some(MyDefault::Expression(d)),
                }
            } else {
                match raw_default {
                    None => null_default.then_some(MyDefault::Null),
                    Some(d) if extra_lower.contains("default_generated") => {
                        Some(MyDefault::Expression(d))
                    }
                    Some(d)
                        if matches!(data_type.as_str(), "timestamp" | "datetime")
                            && d.to_ascii_uppercase().starts_with("CURRENT_TIMESTAMP") =>
                    {
                        Some(MyDefault::Expression(d))
                    }
                    Some(d) => Some(MyDefault::Literal(d)),
                }
            };
            let mut check = checks.get(&name).cloned();
            let json_check = format!("json_valid({})", id(&name)?);
            let json = if maria {
                let alias =
                    data_type == "longtext" && check.as_deref() == Some(json_check.as_str());
                if alias {
                    check = None;
                }
                alias
            } else {
                data_type == "json"
            };
            Ok(MyColumn {
                table: req(&row, "table_name")?,
                position: uint32(&row, "position")?.unwrap_or(0),
                column_type: req(&row, "column_type")?,
                nullable,
                default,
                auto_increment,
                on_update: on_update_of(&extra),
                generated,
                invisible: extra_lower.contains("invisible"),
                charset: text_nonempty(&row, "charset")?,
                collation: text_nonempty(&row, "collation")?,
                comment: text_nonempty(&row, "comment")?,
                srid: if maria {
                    srids.get(&name).copied().filter(|s| *s != 0)
                } else {
                    uint32(&row, "srid")?
                },
                json,
                check,
                key: text_nonempty(&row, "col_key")?,
                data_type,
                name,
            })
        })
        .collect()
}

/// The periods of a table, read from its `CREATE TABLE`:
/// `(name, start, end)`, `SYSTEM_TIME` included.
async fn table_periods(
    session: &dyn Session,
    database: &str,
    table: &str,
) -> CatalogResult<Vec<(String, String, String)>> {
    let sql = format!(
        "SELECT @@SESSION.sql_mode AS `mode`;\nSHOW CREATE TABLE {}",
        super::util::qname(database, table)?
    );
    let mut sets = query_all(session, &sql, &format!("table {database}.{table}")).await?;
    let mode = take(&mut sets, 0)?
        .iter()
        .next()
        .map(|r| text(&r, "mode"))
        .transpose()?
        .flatten()
        .unwrap_or_default();
    let create = take(&mut sets, 1)?;
    let ddl = create
        .iter()
        .next()
        .map(|r| text(&r, "Create Table"))
        .transpose()?
        .flatten()
        .unwrap_or_default();
    Ok(periods_of(&ddl, Lex::for_sql_mode(&mode, true)))
}

/// The row start / row end columns of a MariaDB system-versioned table,
/// for columns the catalog shows as generated without an expression.
async fn system_time_columns(
    session: &dyn Session,
    database: &str,
    table: &str,
) -> CatalogResult<HashMap<String, MyGeneratedKind>> {
    let mut map = HashMap::new();
    for (period, start, end) in table_periods(session, database, table).await? {
        if period.eq_ignore_ascii_case("SYSTEM_TIME") {
            map.insert(start, MyGeneratedKind::RowStart);
            map.insert(end, MyGeneratedKind::RowEnd);
        }
    }
    Ok(map)
}

/// Columns of a table or view (all, or the one named `column`).
pub(crate) async fn load_columns(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
    table: &str,
    column: Option<&str>,
) -> CatalogResult<Vec<MyColumn>> {
    let sql = columns_statements(server, database, table, column)?.join(";\n");
    let rows = query_all(session, &sql, &format!("table {database}.{table}")).await?;
    let empty = HashMap::new();
    let mut columns = parse_columns(&rows, server, &empty)?;
    let needs_system_time = is_mariadb(server)
        && columns.iter().any(|c| {
            c.generated.as_ref().is_some_and(|g| {
                g.expression.is_none()
                    && matches!(
                        g.generated_kind,
                        MyGeneratedKind::Virtual | MyGeneratedKind::Stored
                    )
            })
        });
    if needs_system_time {
        let map = system_time_columns(session, database, table).await?;
        columns = parse_columns(&rows, server, &map)?;
    }
    Ok(columns)
}

/// Summary of a column.
pub(crate) fn column_summary(database: &str, column: &MyColumn) -> ObjectSummary {
    let mut flags = Vec::new();
    if column.key.as_deref() == Some("PRI") {
        flags.push("primary key".to_owned());
    }
    if column.auto_increment {
        flags.push("auto increment".to_owned());
    }
    if !column.nullable {
        flags.push("not null".to_owned());
    }
    if column.invisible {
        flags.push("invisible".to_owned());
    }
    if let Some(generated) = &column.generated {
        flags.push(
            match generated.generated_kind {
                MyGeneratedKind::Virtual => "virtual",
                MyGeneratedKind::Stored => "stored",
                MyGeneratedKind::RowStart => "row start",
                MyGeneratedKind::RowEnd => "row end",
            }
            .to_owned(),
        );
    }
    if column.json {
        flags.push("json".to_owned());
    }
    ObjectSummary {
        object: object_ref(
            ObjectKind::Column,
            database,
            Some(&column.table),
            &column.name,
        ),
        comment: column.comment.clone(),
        detail: Some(column.column_type.clone()),
        flags,
        has_children: false,
        system: is_system_db(database),
        row_estimate: None,
        size_bytes: None,
    }
}

// ------------------------------------------------------------------ indexes

fn indexes_sql(
    server: &ServerInfo,
    database: &str,
    table: &str,
    index: Option<&str>,
) -> CatalogResult<String> {
    let maria = is_mariadb(server);
    let visible = if !maria { "IS_VISIBLE" } else { "'YES'" };
    let expression = if !maria && at_least(server, 8, 0, 13) {
        "EXPRESSION"
    } else {
        "NULL"
    };
    let ignored = if maria && at_least(server, 10, 6, 0) {
        "IGNORED"
    } else {
        "'NO'"
    };
    let filter = match index {
        Some(index) => format!(" AND INDEX_NAME = {}", lit(index)?),
        None => String::new(),
    };
    Ok(format!(
        "SELECT TABLE_NAME AS `table_name`, INDEX_NAME AS `name`, NON_UNIQUE AS `non_unique`, \
         SEQ_IN_INDEX AS `seq`, COLUMN_NAME AS `column_name`, COLLATION AS `collation`, \
         SUB_PART AS `sub_part`, INDEX_TYPE AS `index_type`, INDEX_COMMENT AS `comment`, \
         {visible} AS `visible`, {expression} AS `expression`, {ignored} AS `ignored` \
         FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}{filter} \
         ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX",
        lit(database)?,
        lit(table)?
    ))
}

fn parse_indexes(rows: &Rows) -> CatalogResult<Vec<MyIndex>> {
    let mut out: Vec<MyIndex> = Vec::new();
    for row in rows.iter() {
        let name = req(&row, "name")?;
        let part = MyKeyPart {
            column: text_nonempty(&row, "column_name")?,
            // STATISTICS.EXPRESSION escapes quotes and backslashes as inside
            // a string literal (`_utf8mb4\'$.a\'`); the key part is the
            // expression itself.
            expression: text_nonempty(&row, "expression")?.map(|e| text::unescape(&e, '\'', true)),
            prefix_length: uint32(&row, "sub_part")?,
            descending: text(&row, "collation")?.as_deref() == Some("D"),
        };
        if let Some(index) = out.last_mut().filter(|i| i.name == name) {
            index.parts.push(part);
            continue;
        }
        let index_type = text(&row, "index_type")?
            .unwrap_or_default()
            .to_ascii_uppercase();
        let unique = uint(&row, "non_unique")? == Some(0);
        let index_kind = if name == "PRIMARY" {
            MyIndexKind::Primary
        } else if index_type == "FULLTEXT" {
            MyIndexKind::Fulltext
        } else if index_type == "SPATIAL" {
            MyIndexKind::Spatial
        } else if unique {
            MyIndexKind::Unique
        } else {
            MyIndexKind::Index
        };
        let method = matches!(
            index_kind,
            MyIndexKind::Primary | MyIndexKind::Unique | MyIndexKind::Index
        )
        .then_some(index_type)
        .filter(|t| !t.is_empty());
        out.push(MyIndex {
            table: req(&row, "table_name")?,
            name,
            index_kind,
            method,
            parts: vec![part],
            invisible: text(&row, "visible")?.is_some_and(|v| v.eq_ignore_ascii_case("NO")),
            ignored: yes(&row, "ignored")?,
            comment: text_nonempty(&row, "comment")?,
            parser: None,
            method_explicit: false,
        });
    }
    // SUB_PART of a spatial key part is the size of the stored geometry
    // (32), not a prefix the definition can repeat; full-text parts take
    // none either.
    for index in &mut out {
        if matches!(
            index.index_kind,
            MyIndexKind::Spatial | MyIndexKind::Fulltext
        ) {
            for part in &mut index.parts {
                part.prefix_length = None;
            }
        }
    }
    Ok(out)
}

/// Indexes of a table (all, or the one named `index`).
pub(crate) async fn load_indexes(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
    table: &str,
    index: Option<&str>,
) -> CatalogResult<Vec<MyIndex>> {
    let sql = indexes_sql(server, database, table, index)?;
    parse_indexes(&query_one(session, &sql, &format!("table {database}.{table}")).await?)
}

/// Summary of an index.
pub(crate) fn index_summary(database: &str, index: &MyIndex) -> ObjectSummary {
    let mut flags = vec![
        match index.index_kind {
            MyIndexKind::Primary => "primary",
            MyIndexKind::Unique => "unique",
            MyIndexKind::Index => "index",
            MyIndexKind::Fulltext => "fulltext",
            MyIndexKind::Spatial => "spatial",
        }
        .to_owned(),
    ];
    if index.invisible {
        flags.push("invisible".to_owned());
    }
    if index.ignored {
        flags.push("ignored".to_owned());
    }
    let parts: Vec<String> = index
        .parts
        .iter()
        .map(|p| {
            let mut s = p
                .column
                .clone()
                .or_else(|| p.expression.as_ref().map(|e| format!("({e})")))
                .unwrap_or_default();
            if let Some(len) = p.prefix_length {
                s.push_str(&format!("({len})"));
            }
            if p.descending {
                s.push_str(" DESC");
            }
            s
        })
        .collect();
    let detail = match &index.method {
        Some(method) => format!("{method} ({})", parts.join(", ")),
        None => format!("({})", parts.join(", ")),
    };
    ObjectSummary {
        object: object_ref(ObjectKind::Index, database, Some(&index.table), &index.name),
        comment: index.comment.clone(),
        detail: Some(detail),
        flags,
        has_children: false,
        system: is_system_db(database),
        row_estimate: None,
        size_bytes: None,
    }
}

// -------------------------------------------------------------- constraints

/// The statements that read constraints: `TABLE_CONSTRAINTS`,
/// `KEY_COLUMN_USAGE`, `REFERENTIAL_CONSTRAINTS` and (MySQL 8.0.16+,
/// MariaDB) `CHECK_CONSTRAINTS`, each filtered by constants and joined in
/// Rust (see [`columns_statements`]).
fn constraints_statements(
    server: &ServerInfo,
    database: &str,
    table: &str,
    name: Option<&str>,
) -> CatalogResult<Vec<String>> {
    let maria = is_mariadb(server);
    let db = lit(database)?;
    let tb = lit(table)?;
    let enforced = if !maria && at_least(server, 8, 0, 16) {
        "ENFORCED"
    } else {
        "NULL"
    };
    let filter = match name {
        Some(name) => format!(" AND CONSTRAINT_NAME = {}", lit(name)?),
        None => String::new(),
    };
    let mut out = vec![
        format!(
            "SELECT TABLE_NAME AS `table_name`, CONSTRAINT_NAME AS `name`, \
             CONSTRAINT_TYPE AS `constraint_type`, {enforced} AS `enforced` \
             FROM information_schema.TABLE_CONSTRAINTS \
             WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {tb}{filter} \
             ORDER BY CONSTRAINT_TYPE = 'PRIMARY KEY' DESC, CONSTRAINT_NAME"
        ),
        format!(
            "SELECT CONSTRAINT_NAME AS `name`, COLUMN_NAME AS `column_name`, \
             REFERENCED_TABLE_SCHEMA AS `ref_schema`, REFERENCED_TABLE_NAME AS `ref_table`, \
             REFERENCED_COLUMN_NAME AS `ref_column` FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {tb}{filter} \
             ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION"
        ),
        format!(
            "SELECT CONSTRAINT_NAME AS `name`, UPDATE_RULE AS `on_update`, \
             DELETE_RULE AS `on_delete`, MATCH_OPTION AS `match_option` \
             FROM information_schema.REFERENTIAL_CONSTRAINTS \
             WHERE CONSTRAINT_SCHEMA = {db} AND TABLE_NAME = {tb}{filter}"
        ),
    ];
    if maria {
        out.push(format!(
            "SELECT CONSTRAINT_NAME AS `name`, CHECK_CLAUSE AS `check_clause`, LEVEL AS `check_level` \
             FROM information_schema.CHECK_CONSTRAINTS \
             WHERE CONSTRAINT_SCHEMA = {db} AND TABLE_NAME = {tb}{filter}"
        ));
    } else if at_least(server, 8, 0, 16) {
        // MySQL's CHECK_CONSTRAINTS has no table column; check names are
        // unique per database, and TABLE_CONSTRAINTS says which are this
        // table's.
        out.push(format!(
            "SELECT CONSTRAINT_NAME AS `name`, CHECK_CLAUSE AS `check_clause`, NULL AS `check_level` \
             FROM information_schema.CHECK_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = {db}{filter}"
        ));
    }
    Ok(out)
}

/// A key column: column, referenced database, table and column.
type KeyColumn = (String, Option<String>, Option<String>, Option<String>);

/// A foreign key's `ON UPDATE`, `ON DELETE` and match option.
type ForeignKeyRules = (Option<String>, Option<String>, Option<String>);

/// Reads the results of [`constraints_statements`].
fn parse_constraints(
    sets: &[Rows],
    server: &ServerInfo,
    database: &str,
) -> CatalogResult<Vec<MyConstraint>> {
    let maria = is_mariadb(server);
    let missing = || CatalogError::Unexpected {
        message: "the constraint query returned nothing".into(),
    };
    let mut columns: HashMap<String, Vec<KeyColumn>> = HashMap::new();
    for row in sets.get(1).ok_or_else(missing)?.iter() {
        if let Some(column) = text_nonempty(&row, "column_name")? {
            columns.entry(req(&row, "name")?).or_default().push((
                column,
                text_nonempty(&row, "ref_schema")?,
                text_nonempty(&row, "ref_table")?,
                text_nonempty(&row, "ref_column")?,
            ));
        }
    }
    let mut rules: HashMap<String, ForeignKeyRules> = HashMap::new();
    for row in sets.get(2).ok_or_else(missing)?.iter() {
        rules.insert(
            req(&row, "name")?,
            (
                text_nonempty(&row, "on_update")?,
                text_nonempty(&row, "on_delete")?,
                text_nonempty(&row, "match_option")?,
            ),
        );
    }
    let mut checks: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    for row in sets.get(3).into_iter().flat_map(Rows::iter) {
        checks.insert(
            req(&row, "name")?,
            (
                text_nonempty(&row, "check_clause")?,
                text(&row, "check_level")?,
            ),
        );
    }
    let mut out: Vec<MyConstraint> = Vec::new();
    for row in sets.first().ok_or_else(missing)?.iter() {
        let name = req(&row, "name")?;
        let constraint_type = match req(&row, "constraint_type")?.to_ascii_uppercase().as_str() {
            "PRIMARY KEY" => MyConstraintType::PrimaryKey,
            "UNIQUE" => MyConstraintType::Unique,
            "FOREIGN KEY" => MyConstraintType::ForeignKey,
            "CHECK" => MyConstraintType::Check,
            other => {
                return Err(CatalogError::Unexpected {
                    message: format!("unknown constraint type {other}"),
                });
            }
        };
        let parts = if constraint_type == MyConstraintType::Check {
            Vec::new()
        } else {
            columns.get(&name).cloned().unwrap_or_default()
        };
        let references = if constraint_type == MyConstraintType::ForeignKey {
            let (on_update, on_delete, match_option) =
                rules.get(&name).cloned().unwrap_or_default();
            let first = parts.first();
            Some(MyReference {
                database: first
                    .and_then(|p| p.1.clone())
                    .unwrap_or_else(|| database.to_owned()),
                table: first.and_then(|p| p.2.clone()).unwrap_or_default(),
                columns: parts.iter().filter_map(|p| p.3.clone()).collect(),
                on_update,
                on_delete,
                match_option,
            })
        } else {
            None
        };
        let (check_clause, level) = if constraint_type == MyConstraintType::Check {
            let (clause, level) = checks.get(&name).cloned().unwrap_or_default();
            let level = level.map(|l| {
                if l.eq_ignore_ascii_case("Column") {
                    MyCheckLevel::Column
                } else {
                    MyCheckLevel::Table
                }
            });
            (clause, level)
        } else {
            (None, None)
        };
        let json = maria
            && level == Some(MyCheckLevel::Column)
            && check_clause.as_deref() == Some(format!("json_valid({})", id(&name)?).as_str());
        out.push(MyConstraint {
            table: req(&row, "table_name")?,
            columns: parts.into_iter().map(|p| p.0).collect(),
            constraint_type,
            references,
            check_clause,
            enforced: if maria {
                None
            } else {
                text(&row, "enforced")?.map(|e| e.eq_ignore_ascii_case("YES"))
            },
            level: if maria { level } else { None },
            json,
            period: false,
            name,
        });
    }
    Ok(out)
}

/// Constraints of a table (all, or the one named `name`).
pub(crate) async fn load_constraints(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
    table: &str,
    name: Option<&str>,
) -> CatalogResult<Vec<MyConstraint>> {
    let sql = constraints_statements(server, database, table, name)?.join(";\n");
    let rows = query_all(session, &sql, &format!("table {database}.{table}")).await?;
    let mut constraints = parse_constraints(&rows, server, database)?;
    let table_checks = constraints
        .iter()
        .any(|c| c.level == Some(MyCheckLevel::Table));
    if is_mariadb(server) && table_checks {
        let periods: Vec<MyPeriod> = table_periods(session, database, table)
            .await?
            .into_iter()
            .filter(|(name, _, _)| !name.eq_ignore_ascii_case("SYSTEM_TIME"))
            .map(|(name, start, end)| MyPeriod {
                name,
                start_column: start,
                end_column: end,
            })
            .collect();
        mark_period_checks(&mut constraints, &periods);
    }
    Ok(constraints)
}

/// Marks the MariaDB checks that application-time periods add (named after
/// the period).
fn mark_period_checks(constraints: &mut [MyConstraint], periods: &[MyPeriod]) {
    for c in constraints {
        if c.constraint_type == MyConstraintType::Check
            && c.level == Some(MyCheckLevel::Table)
            && periods.iter().any(|p| p.name == c.name)
        {
            c.period = true;
        }
    }
}

/// Summary of a constraint.
pub(crate) fn constraint_summary(database: &str, c: &MyConstraint) -> ObjectSummary {
    let mut flags = vec![
        match c.constraint_type {
            MyConstraintType::PrimaryKey => "primary key",
            MyConstraintType::Unique => "unique",
            MyConstraintType::ForeignKey => "foreign key",
            MyConstraintType::Check => "check",
        }
        .to_owned(),
    ];
    if c.enforced == Some(false) {
        flags.push("not enforced".to_owned());
    }
    if c.level == Some(MyCheckLevel::Column) {
        flags.push("column".to_owned());
    }
    if c.json {
        flags.push("json".to_owned());
    }
    if c.period {
        flags.push("period".to_owned());
    }
    let detail = match (&c.references, &c.check_clause) {
        (Some(r), _) => Some(format!(
            "({}) -> {}.{} ({})",
            c.columns.join(", "),
            r.database,
            r.table,
            r.columns.join(", ")
        )),
        (None, Some(check)) => Some(check.clone()),
        (None, None) => Some(format!("({})", c.columns.join(", "))),
    };
    ObjectSummary {
        object: object_ref(ObjectKind::Constraint, database, Some(&c.table), &c.name),
        comment: None,
        detail,
        flags,
        has_children: false,
        system: is_system_db(database),
        row_estimate: None,
        size_bytes: None,
    }
}

// --------------------------------------------------------------- partitions

fn partitions_sql(database: &str, table: &str) -> CatalogResult<String> {
    Ok(format!(
        "SELECT TABLE_NAME AS `table_name`, PARTITION_NAME AS `name`, SUBPARTITION_NAME AS `sub_name`, \
         PARTITION_ORDINAL_POSITION AS `pos`, SUBPARTITION_ORDINAL_POSITION AS `sub_pos`, \
         PARTITION_METHOD AS `method`, SUBPARTITION_METHOD AS `sub_method`, \
         PARTITION_EXPRESSION AS `expr`, SUBPARTITION_EXPRESSION AS `sub_expr`, \
         PARTITION_DESCRIPTION AS `description`, TABLE_ROWS AS `row_count`, \
         DATA_LENGTH AS `data_length`, INDEX_LENGTH AS `index_length`, \
         PARTITION_COMMENT AS `comment`, TABLESPACE_NAME AS `tablespace` \
         FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} \
         AND PARTITION_NAME IS NOT NULL ORDER BY PARTITION_ORDINAL_POSITION, SUBPARTITION_ORDINAL_POSITION",
        lit(database)?,
        lit(table)?
    ))
}

/// The values clause of a partition.
fn values_clause(method: Option<&str>, description: Option<&str>) -> Option<String> {
    let method = method?.to_ascii_uppercase();
    if method.starts_with("RANGE") {
        description.map(|d| format!("VALUES LESS THAN ({d})"))
    } else if method.starts_with("LIST") {
        description.map(|d| format!("VALUES IN ({d})"))
    } else if method == "SYSTEM_TIME" {
        Some(
            if description.is_some_and(|d| d.eq_ignore_ascii_case("CURRENT")) {
                "CURRENT".to_owned()
            } else {
                "HISTORY".to_owned()
            },
        )
    } else {
        None
    }
}

fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    }
}

fn parse_partitioning(rows: &Rows) -> CatalogResult<Option<MyPartitioning>> {
    let mut partitioning: Option<MyPartitioning> = None;
    for row in rows.iter() {
        let method = text_nonempty(&row, "method")?;
        let p = partitioning.get_or_insert_with(|| MyPartitioning {
            linear: method
                .as_deref()
                .is_some_and(|m| m.to_ascii_uppercase().starts_with("LINEAR")),
            method: method.clone().unwrap_or_default(),
            expression: None,
            subpartition_method: None,
            subpartition_expression: None,
            partitions: Vec::new(),
        });
        if p.expression.is_none() {
            p.expression = text_nonempty(&row, "expr")?;
        }
        if p.subpartition_method.is_none() {
            p.subpartition_method = text_nonempty(&row, "sub_method")?;
            p.subpartition_expression = text_nonempty(&row, "sub_expr")?;
        }
        let table = req(&row, "table_name")?;
        let name = req(&row, "name")?;
        let rows_n = uint(&row, "row_count")?;
        let data = uint(&row, "data_length")?;
        let index = uint(&row, "index_length")?;
        let description = text_nonempty(&row, "description")?;
        let sub_name = text_nonempty(&row, "sub_name")?;
        let is_new = p.partitions.last().is_none_or(|last| last.name != name);
        if is_new {
            p.partitions.push(MyPartition {
                table: table.clone(),
                name: name.clone(),
                partition: None,
                position: uint32(&row, "pos")?.unwrap_or(0),
                values: values_clause(method.as_deref(), description.as_deref()),
                method: method.clone(),
                expression: text_nonempty(&row, "expr")?,
                description,
                rows: None,
                data_length: None,
                index_length: None,
                comment: text_nonempty(&row, "comment")?,
                tablespace: text_nonempty(&row, "tablespace")?,
                subpartitions: Vec::new(),
            });
        }
        let Some(partition) = p.partitions.last_mut() else {
            continue;
        };
        partition.rows = add(partition.rows, rows_n);
        partition.data_length = add(partition.data_length, data);
        partition.index_length = add(partition.index_length, index);
        if let Some(sub_name) = sub_name {
            partition.subpartitions.push(MyPartition {
                table,
                name: sub_name,
                partition: Some(name),
                position: uint32(&row, "sub_pos")?.unwrap_or(0),
                method: text_nonempty(&row, "sub_method")?,
                expression: text_nonempty(&row, "sub_expr")?,
                description: None,
                values: None,
                rows: rows_n,
                data_length: data,
                index_length: index,
                comment: text_nonempty(&row, "comment")?,
                tablespace: text_nonempty(&row, "tablespace")?,
                subpartitions: Vec::new(),
            });
        }
    }
    Ok(partitioning)
}

/// Partitioning of a table, `None` when it is not partitioned.
pub(crate) async fn load_partitioning(
    session: &dyn Session,
    database: &str,
    table: &str,
) -> CatalogResult<Option<MyPartitioning>> {
    let sql = partitions_sql(database, table)?;
    parse_partitioning(&query_one(session, &sql, &format!("table {database}.{table}")).await?)
}

/// Finds a partition or subpartition by name.
pub(crate) fn find_partition(partitioning: &MyPartitioning, name: &str) -> Option<MyPartition> {
    partitioning.partitions.iter().find_map(|p| {
        if p.name == name {
            Some(p.clone())
        } else {
            p.subpartitions.iter().find(|s| s.name == name).cloned()
        }
    })
}

/// Summary of a partition or subpartition.
pub(crate) fn partition_summary(database: &str, p: &MyPartition) -> ObjectSummary {
    let mut detail = p.values.clone().unwrap_or_default();
    if let Some(rows) = p.rows {
        if !detail.is_empty() {
            detail.push_str(", ");
        }
        detail.push_str(&format!("~{rows} rows"));
    }
    ObjectSummary {
        object: object_ref(ObjectKind::Partition, database, Some(&p.table), &p.name),
        comment: p.comment.clone(),
        detail: (!detail.is_empty()).then_some(detail),
        flags: p
            .partition
            .as_ref()
            .map(|_| vec!["subpartition".to_owned()])
            .unwrap_or_default(),
        has_children: !p.subpartitions.is_empty(),
        system: is_system_db(database),
        row_estimate: p.rows,
        size_bytes: match (p.data_length, p.index_length) {
            (Some(data), Some(index)) => Some(data.saturating_add(index)),
            (data, index) => data.or(index),
        },
    }
}

// ----------------------------------------------------------------- triggers

/// Triggers query. MariaDB keeps one ordered list per table and timing,
/// which `information_schema` returns in order (a trigger for several
/// events, MariaDB 12, has one `ACTION_ORDER` for all of them), so MariaDB
/// rows keep the server's order; MySQL orders per event by `ACTION_ORDER`.
fn triggers_sql(server: &ServerInfo, database: &str, table: Option<&str>) -> CatalogResult<String> {
    let order = if is_mariadb(server) {
        ""
    } else {
        " ORDER BY EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION, ACTION_ORDER"
    };
    let filter = match table {
        Some(table) => format!(" AND EVENT_OBJECT_TABLE = {}", lit(table)?),
        None => String::new(),
    };
    Ok(format!(
        "SELECT TRIGGER_NAME AS `name`, EVENT_OBJECT_TABLE AS `table_name`, \
         ACTION_TIMING AS `timing`, EVENT_MANIPULATION AS `event`, ACTION_ORDER AS `action_order`, \
         DEFINER AS `definer`, SQL_MODE AS `sql_mode`, ACTION_STATEMENT AS `body`, CREATED AS `created`, \
         CHARACTER_SET_CLIENT AS `charset_client`, COLLATION_CONNECTION AS `collation_connection`, \
         DATABASE_COLLATION AS `database_collation` \
         FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = {}{filter}{order}",
        lit(database)?
    ))
}

fn parse_triggers(rows: &Rows, database: &str) -> CatalogResult<Vec<MyTrigger>> {
    let mut out: Vec<MyTrigger> = rows
        .iter()
        .map(|row| {
            Ok(MyTrigger {
                database: database.to_owned(),
                table: req(&row, "table_name")?,
                name: req(&row, "name")?,
                timing: req(&row, "timing")?,
                event: req(&row, "event")?,
                action_order: uint32(&row, "action_order")?.unwrap_or(1),
                follows: None,
                precedes: None,
                definer: text_nonempty(&row, "definer")?,
                sql_mode: text(&row, "sql_mode")?,
                body: text(&row, "body")?,
                created: text(&row, "created")?,
                charset_client: text_nonempty(&row, "charset_client")?,
                collation_connection: text_nonempty(&row, "collation_connection")?,
                database_collation: text_nonempty(&row, "database_collation")?,
            })
        })
        .collect::<CatalogResult<_>>()?;
    // Rows of one table and timing are kept together without changing
    // their order within the group.
    out.sort_by(|a, b| (&a.table, &a.timing).cmp(&(&b.table, &b.timing)));
    let events = |t: &MyTrigger| -> Vec<String> {
        t.event
            .split(',')
            .map(|e| e.trim().to_ascii_uppercase())
            .collect()
    };
    let overlap = |a: &MyTrigger, b: &MyTrigger| {
        let (ea, eb) = (events(a), events(b));
        a.table == b.table && a.timing == b.timing && ea.iter().any(|e| eb.contains(e))
    };
    let neighbours: Vec<(Option<String>, Option<String>)> = (0..out.len())
        .map(|i| {
            let current = &out[i];
            let prev = out[..i]
                .iter()
                .rev()
                .find(|p| overlap(p, current))
                .map(|p| p.name.clone());
            let next = out[i + 1..]
                .iter()
                .find(|n| overlap(n, current))
                .map(|n| n.name.clone());
            (prev, next)
        })
        .collect();
    for (trigger, (prev, next)) in out.iter_mut().zip(neighbours) {
        trigger.follows = prev;
        trigger.precedes = next;
    }
    Ok(out)
}

/// Triggers of a database, or of one table.
pub(crate) async fn load_triggers(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
    table: Option<&str>,
) -> CatalogResult<Vec<MyTrigger>> {
    let sql = triggers_sql(server, database, table)?;
    parse_triggers(&query_one(session, &sql, "the database").await?, database)
}

/// Summary of a trigger.
pub(crate) fn trigger_summary(t: &MyTrigger) -> ObjectSummary {
    ObjectSummary {
        object: object_ref(ObjectKind::Trigger, &t.database, Some(&t.table), &t.name),
        comment: None,
        detail: Some(format!("{} {} ON {}", t.timing, t.event, t.table)),
        flags: Vec::new(),
        has_children: false,
        system: is_system_db(&t.database),
        row_estimate: None,
        size_bytes: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_parser_and_explicit_method_from_the_ddl() {
        let server = ServerInfo {
            engine: varve_core::Engine::Mysql,
            version: "8.0.40".into(),
            major: 8,
            minor: 0,
            patch: Some(40),
        };
        let ddl = "CREATE TABLE `t` (\n  `k` int,\n  `f` text,\n  KEY `kb` (`k`) USING BTREE,\n  KEY `ki` (`k`),\n  \
                   FULLTEXT KEY `ft` (`f`) /*!50100 WITH PARSER `ngram` */ \n) ENGINE=MEMORY";
        let found = index_ddl_of(ddl, Lex::for_sql_mode("", false), &server);
        assert_eq!(
            found.get("kb"),
            Some(&IndexDdl {
                parser: None,
                explicit_method: true
            })
        );
        assert_eq!(found.get("ki"), Some(&IndexDdl::default()));
        assert_eq!(
            found.get("ft"),
            Some(&IndexDdl {
                parser: Some("ngram".into()),
                explicit_method: false
            })
        );
    }

    #[test]
    fn small_parsers() {
        assert_eq!(
            on_update_of("DEFAULT_GENERATED on update CURRENT_TIMESTAMP(3)").as_deref(),
            Some("CURRENT_TIMESTAMP(3)")
        );
        assert_eq!(on_update_of("auto_increment"), None);
        assert!(is_number_literal("1.50"));
        assert!(is_number_literal("-3"));
        assert!(is_number_literal("b'101'"));
        assert!(!is_number_literal("current_timestamp()"));
        assert_eq!(
            values_clause(Some("RANGE COLUMNS"), Some("10,'a'")).as_deref(),
            Some("VALUES LESS THAN (10,'a')")
        );
        assert_eq!(
            values_clause(Some("LIST"), Some("1,2")).as_deref(),
            Some("VALUES IN (1,2)")
        );
        assert_eq!(
            values_clause(Some("SYSTEM_TIME"), Some("CURRENT")).as_deref(),
            Some("CURRENT")
        );
        assert_eq!(values_clause(Some("HASH"), None), None);
    }
}
