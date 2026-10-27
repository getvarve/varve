//! DDL of MySQL and MariaDB objects.
//!
//! Where the server has `SHOW CREATE` (databases, tables, views,
//! procedures, functions, triggers, events, MariaDB sequences and
//! packages), its output is the source, kept faithful (the `DEFINER`
//! clause included) and made re-runnable and independent of the session's
//! default database:
//!
//! - the object name is qualified with its database (and a trigger's
//!   table too); a view's query is the server's fully qualified
//!   `VIEW_DEFINITION`, because `SHOW CREATE VIEW` leaves out the session's
//!   current database;
//! - executable comments the server wrote for itself (`/*!50100 ... */`)
//!   are unwrapped in table, sequence and database DDL;
//! - databases, tables and sequences get `IF NOT EXISTS`, so running the
//!   DDL again changes nothing;
//! - views get `CREATE OR REPLACE`; routines, triggers, events and packages
//!   get `CREATE OR REPLACE` on MariaDB and a `DROP ... IF EXISTS` before
//!   the `CREATE` on MySQL, which has no `OR REPLACE` for them;
//! - a trigger keeps its place among the table's triggers with the same
//!   timing (and, on MariaDB 12, an overlapping event) with
//!   `PRECEDES <next>`; the last one needs no clause, because a re-created
//!   trigger goes last. Re-running the DDL of any one trigger keeps the
//!   order. To rebuild all triggers of a table from scratch, run their DDL
//!   from the last to the first. MariaDB 12 triggers for several events
//!   may only reference triggers that have all their events; see
//!   `trigger_order_clause` for when that falls back to `FOLLOWS` or to no
//!   clause;
//! - the character set, collations, `sql_mode` and (events) time zone the
//!   object was created with, and the database collation routines keep,
//!   travel in a session header on the first line when they differ from
//!   the session's (see [`super::session`]). The `CREATE` is always the
//!   last statement, so MySQL stores single-statement bodies exactly as
//!   they were (it keeps a body's closing `;` when more statements follow).
//!
//! Columns, indexes, constraints and partitions have no `SHOW CREATE`;
//! their DDL is the `ALTER TABLE ... ADD` of their definition as it appears
//! in `SHOW CREATE TABLE`, with `IF NOT EXISTS` on MariaDB (except for a
//! primary key, which has no such option). MySQL has no
//! `IF NOT EXISTS` there, so on MySQL that DDL runs only while the object
//! is absent. A MariaDB column-level check is part of its column and comes
//! back as `ALTER TABLE ... MODIFY COLUMN`, and the check MariaDB adds for
//! an application-time period as `ALTER TABLE ... ADD PERIOD`. Column and
//! constraint changes of a MariaDB system-versioned table run with
//! `SET STATEMENT system_versioning_alter_history = KEEP FOR`. A range or
//! list partition that is not the last comes back as
//! `REORGANIZE PARTITION <next> INTO (<partition>, <next>)`, which puts it
//! back in its place (it runs while the partition is absent).
//!
//! Statements are separated by `;` and a newline; the last has no `;`.

use varve_driver::{ServerInfo, Session};

use super::programs::single_text;
use super::schema;
use super::session::{DdlSession, with_header};
use super::text::{self, CreateHead, Lex, TableItem};
use super::util::{
    at_least, database_of, id, is_mariadb, lit, parent_of, qname, query_all, take, text,
    version_number,
};
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef};

/// The session's settings that objects keep.
const SESSION_SQL: &str = "SELECT @@SESSION.sql_mode AS `mode`, @@SESSION.time_zone AS `tz`, \
     @@SESSION.character_set_client AS `csc`, @@SESSION.character_set_results AS `csr`, \
     @@SESSION.collation_connection AS `cc`";

/// DDL of one object.
pub(crate) async fn ddl(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<String> {
    match object.kind {
        ObjectKind::Database => database_ddl(session, server, &object.name).await,
        ObjectKind::Table => {
            let db = database_of(object)?;
            created_with_if_not_exists(session, server, db, &object.name, "TABLE").await
        }
        ObjectKind::Sequence => {
            let db = database_of(object)?;
            created_with_if_not_exists(session, server, db, &object.name, "SEQUENCE").await
        }
        ObjectKind::View => view_ddl(session, server, database_of(object)?, &object.name).await,
        ObjectKind::Procedure
        | ObjectKind::Function
        | ObjectKind::Trigger
        | ObjectKind::Event
        | ObjectKind::Package
        | ObjectKind::PackageBody => program_ddl(session, server, object).await,
        ObjectKind::Column | ObjectKind::Index | ObjectKind::Constraint => {
            table_item_ddl(session, server, object).await
        }
        ObjectKind::Partition => partition_ddl(session, server, object).await,
        other => Err(CatalogError::Unsupported {
            message: format!("{} has no {other:?} objects", engine_name(server)),
        }),
    }
}

fn engine_name(server: &ServerInfo) -> &'static str {
    if is_mariadb(server) {
        "MariaDB"
    } else {
        "MySQL"
    }
}

/// The session settings read with [`SESSION_SQL`].
struct SessionSettings {
    sql_mode: String,
    time_zone: String,
    character_set_client: String,
    character_set_results: String,
    collation_connection: String,
}

fn session_settings(rows: &crate::rows::Rows) -> CatalogResult<SessionSettings> {
    let row = rows.single("session settings")?;
    Ok(SessionSettings {
        sql_mode: text(&row, "mode")?.unwrap_or_default(),
        time_zone: text(&row, "tz")?.unwrap_or_default(),
        character_set_client: text(&row, "csc")?.unwrap_or_default(),
        character_set_results: text(&row, "csr")?.unwrap_or_default(),
        collation_connection: text(&row, "cc")?.unwrap_or_default(),
    })
}

/// What an object keeps of the session that created it.
#[derive(Default)]
struct Kept {
    character_set_client: Option<String>,
    collation_connection: Option<String>,
    sql_mode: Option<String>,
    time_zone: Option<String>,
    collation_database: Option<String>,
}

impl Kept {
    /// Reads the kept settings from a `SHOW CREATE` row.
    fn of(rows: &crate::rows::Rows) -> CatalogResult<Self> {
        let get = |column: &str| -> CatalogResult<Option<String>> {
            if rows.columns().iter().any(|c| c == column) {
                single_text(rows, column)
                    .map(|v| v.filter(|v| !v.is_empty() || column == "sql_mode"))
            } else {
                Ok(None)
            }
        };
        Ok(Self {
            character_set_client: get("character_set_client")?,
            collation_connection: get("collation_connection")?,
            sql_mode: get("sql_mode")?,
            time_zone: get("time_zone")?,
            collation_database: get("Database Collation")?,
        })
    }

    /// The header settings: the kept values that differ from the session
    /// (and, for the database collation, from the database's current
    /// default).
    fn differences(
        &self,
        session: &SessionSettings,
        database_collation: Option<&str>,
    ) -> DdlSession {
        let differs = |kept: &Option<String>, current: &str| {
            kept.as_ref().filter(|k| k.as_str() != current).cloned()
        };
        let client = differs(&self.character_set_client, &session.character_set_client);
        // mysqldump sets the results character set with the client one.
        let results = self
            .character_set_client
            .as_ref()
            .filter(|k| k.as_str() != session.character_set_results)
            .cloned();
        DdlSession {
            character_set_client: client,
            character_set_results: results,
            collation_connection: differs(
                &self.collation_connection,
                &session.collation_connection,
            ),
            sql_mode: differs(&self.sql_mode, &session.sql_mode),
            time_zone: differs(&self.time_zone, &session.time_zone),
            collation_database: database_collation
                .and_then(|current| differs(&self.collation_database, current)),
        }
    }
}

/// The first non-NULL text of `columns` in the single row of a
/// `SHOW CREATE` result.
fn show_create_text(
    rows: &crate::rows::Rows,
    columns: &[&str],
    what: &str,
) -> CatalogResult<String> {
    let row = rows.single(what)?;
    for column in columns {
        if rows.columns().iter().any(|c| c == column)
            && let Some(value) = text(&row, column)?
        {
            return Ok(value);
        }
    }
    Err(CatalogError::Unsupported {
        message: format!(
            "the server did not return the definition of {what}; reading it needs more privileges"
        ),
    })
}

fn no_structure(what: &str) -> CatalogError {
    CatalogError::Unexpected {
        message: format!("could not read the structure of {what}"),
    }
}

/// Joins statements into a script.
fn script(statements: &[String]) -> String {
    statements.join(";\n")
}

/// What to add after `CREATE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modifier {
    None,
    OrReplace,
    IfNotExists,
}

/// The edits that qualify the name and add the modifier.
fn head_edits(
    head: &CreateHead,
    database: &str,
    modifier: Modifier,
) -> CatalogResult<Vec<(usize, usize, String)>> {
    let mut edits = Vec::new();
    if modifier == Modifier::OrReplace && !head.or_replace {
        edits.push((head.create_end, head.create_end, " OR REPLACE".to_owned()));
    }
    if modifier == Modifier::IfNotExists && !head.if_not_exists {
        edits.push((head.kind_end, head.kind_end, " IF NOT EXISTS".to_owned()));
    }
    if !head.name.qualified {
        edits.push((
            head.name.start,
            head.name.end,
            qname(database, &head.name.name)?,
        ));
    }
    Ok(edits)
}

// ----------------------------------------------------------------- database

async fn database_ddl(
    session: &dyn Session,
    server: &ServerInfo,
    name: &str,
) -> CatalogResult<String> {
    let has_read_only = !is_mariadb(server) && at_least(server, 8, 0, 22);
    let mut statements = vec![
        SESSION_SQL.to_owned(),
        format!("SHOW CREATE DATABASE IF NOT EXISTS {}", id(name)?),
    ];
    if has_read_only {
        statements.push(format!(
            "SELECT OPTIONS AS `options` FROM information_schema.SCHEMATA_EXTENSIONS \
             WHERE SCHEMA_NAME = {}",
            lit(name)?
        ));
    }
    let what = format!("database {name}");
    let mut sets = query_all(session, &script(&statements), &what).await?;
    let settings = session_settings(&take(&mut sets, 0)?)?;
    let lex = Lex::for_sql_mode(&settings.sql_mode, is_mariadb(server));
    let create = show_create_text(&take(&mut sets, 1)?, &["Create Database"], &what)?;
    let mut out = vec![text::unwrap_exec_comments(
        &create,
        lex,
        version_number(server),
    )];
    if has_read_only {
        let read_only = single_text(&take(&mut sets, 2)?, "options")?
            .is_some_and(|o| o.to_ascii_uppercase().contains("READ ONLY=1"));
        if read_only {
            out.push(format!("ALTER DATABASE {} READ ONLY = 1", id(name)?));
        }
    }
    Ok(script(&out))
}

// --------------------------------------------------------- table, sequence

async fn show_create_table(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
    name: &str,
    kind: &str,
) -> CatalogResult<(String, Lex)> {
    // MariaDB keeps a geometry column's SRID but leaves it out of
    // SHOW CREATE TABLE; it is read from GEOMETRY_COLUMNS and added back.
    let srids = is_mariadb(server) && kind == "TABLE";
    let mut sql = format!(
        "{SESSION_SQL};\nSHOW CREATE {kind} {}",
        qname(database, name)?
    );
    if srids {
        sql.push_str(&format!(
            ";\nSELECT G_GEOMETRY_COLUMN AS `name`, SRID AS `srid` \
             FROM information_schema.GEOMETRY_COLUMNS WHERE F_TABLE_SCHEMA = {} \
             AND F_TABLE_NAME = {} AND SRID <> 0",
            lit(database)?,
            lit(name)?
        ));
    }
    let what = format!("{} {database}.{name}", kind.to_ascii_lowercase());
    let mut sets = query_all(session, &sql, &what).await?;
    let settings = session_settings(&take(&mut sets, 0)?)?;
    let lex = Lex::for_sql_mode(&settings.sql_mode, is_mariadb(server));
    let create = show_create_text(&take(&mut sets, 1)?, &["Create Table"], &what)?;
    let mut create = text::unwrap_exec_comments(&create, lex, version_number(server));
    if srids {
        let mut columns = Vec::new();
        for row in take(&mut sets, 2)?.iter() {
            if let (Some(name), Some(srid)) =
                (text(&row, "name")?, super::util::uint32(&row, "srid")?)
            {
                columns.push((name, srid));
            }
        }
        create = text::add_ref_system_ids(&create, lex, &columns);
    }
    Ok((create, lex))
}

async fn created_with_if_not_exists(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
    name: &str,
    kind: &str,
) -> CatalogResult<String> {
    let (create, lex) = show_create_table(session, server, database, name, kind).await?;
    let what = format!("{} {database}.{name}", kind.to_ascii_lowercase());
    let head = text::create_head(&create, lex, &[kind]).ok_or_else(|| no_structure(&what))?;
    Ok(text::apply_edits(
        &create,
        head_edits(&head, database, Modifier::IfNotExists)?,
    ))
}

// --------------------------------------------------------------------- view

async fn view_ddl(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
    name: &str,
) -> CatalogResult<String> {
    let sql = format!(
        "{SESSION_SQL};\nSHOW CREATE VIEW {};\nSELECT VIEW_DEFINITION AS `definition`, \
         CHECK_OPTION AS `check_option` FROM information_schema.VIEWS \
         WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
        qname(database, name)?,
        lit(database)?,
        lit(name)?
    );
    let what = format!("view {database}.{name}");
    let mut sets = query_all(session, &sql, &what).await?;
    let settings = session_settings(&take(&mut sets, 0)?)?;
    let lex = Lex::for_sql_mode(&settings.sql_mode, is_mariadb(server));
    let created = take(&mut sets, 1)?;
    let create = show_create_text(&created, &["Create View"], &what)?;
    let kept = Kept::of(&created)?.differences(&settings, None);
    let views = take(&mut sets, 2)?;
    let head = text::create_head(&create, lex, &["VIEW"]).ok_or_else(|| no_structure(&what))?;
    let as_end = text::view_as_end(&create, lex, &head).ok_or_else(|| no_structure(&what))?;
    let definition = single_text(&views, "definition")?.filter(|d| !d.trim().is_empty());
    let check_option = single_text(&views, "check_option")?;
    let (header, query) = create.split_at(as_end);
    let edits = head_edits(&head, database, Modifier::OrReplace)?;
    let header = text::apply_edits(header, edits);
    let ddl = match definition {
        Some(definition) => {
            let mut out = format!("{header} {definition}");
            if let Some(option) = check_option.filter(|o| !o.eq_ignore_ascii_case("NONE")) {
                out.push_str(&format!(
                    " WITH {} CHECK OPTION",
                    option.to_ascii_uppercase()
                ));
            }
            out
        }
        // Without the privilege to read VIEW_DEFINITION, SHOW CREATE VIEW's
        // own query is the best there is.
        None => format!("{header}{query}"),
    };
    with_header(&kept, ddl)
}

// ------------------------------------------- routines, triggers, events, packages

struct ProgramKind {
    show: &'static str,
    keywords: &'static [&'static str],
    columns: &'static [&'static str],
}

fn program_kind(kind: ObjectKind) -> Option<ProgramKind> {
    Some(match kind {
        ObjectKind::Procedure => ProgramKind {
            show: "PROCEDURE",
            keywords: &["PROCEDURE"],
            columns: &["Create Procedure"],
        },
        ObjectKind::Function => ProgramKind {
            show: "FUNCTION",
            keywords: &["FUNCTION"],
            columns: &["Create Function"],
        },
        ObjectKind::Trigger => ProgramKind {
            show: "TRIGGER",
            keywords: &["TRIGGER"],
            columns: &["SQL Original Statement"],
        },
        ObjectKind::Event => ProgramKind {
            show: "EVENT",
            keywords: &["EVENT"],
            columns: &["Create Event"],
        },
        ObjectKind::Package => ProgramKind {
            show: "PACKAGE",
            keywords: &["PACKAGE"],
            columns: &["Create Package"],
        },
        ObjectKind::PackageBody => ProgramKind {
            show: "PACKAGE BODY",
            keywords: &["PACKAGE", "BODY"],
            columns: &["Create Package Body"],
        },
        _ => return None,
    })
}

async fn program_ddl(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<String> {
    let maria = is_mariadb(server);
    let database = database_of(object)?;
    let name = &object.name;
    let kind = program_kind(object.kind).ok_or_else(|| CatalogError::Invalid {
        message: format!("{:?} has no SHOW CREATE", object.kind),
    })?;
    if !maria && matches!(object.kind, ObjectKind::Package | ObjectKind::PackageBody) {
        return Err(CatalogError::Unsupported {
            message: "MySQL has no packages".into(),
        });
    }
    let quoted = qname(database, name)?;
    // The comment as stored: SHOW CREATE double encodes the comment of an
    // object created with a client character set other than UTF-8 (see
    // repaired_comment).
    let comment_sql = match object.kind {
        ObjectKind::Event => format!(
            "SELECT EVENT_COMMENT AS `comment` FROM information_schema.EVENTS \
             WHERE EVENT_SCHEMA = {} AND EVENT_NAME = {}",
            lit(database)?,
            lit(name)?
        ),
        ObjectKind::Trigger => "SELECT NULL AS `comment`".to_owned(),
        _ => format!(
            "SELECT ROUTINE_COMMENT AS `comment` FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = {} AND ROUTINE_NAME = {} AND ROUTINE_TYPE = {}",
            lit(database)?,
            lit(name)?,
            lit(kind.show)?
        ),
    };
    let sql = format!(
        "{SESSION_SQL};\nSHOW CREATE {} {quoted};\nSELECT DEFAULT_COLLATION_NAME AS `dbc` \
         FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {};\n{comment_sql}",
        kind.show,
        lit(database)?
    );
    let what = format!("{} {database}.{name}", kind.show.to_ascii_lowercase());
    let mut sets = query_all(session, &sql, &what).await?;
    let settings = session_settings(&take(&mut sets, 0)?)?;
    let created = take(&mut sets, 1)?;
    let create = show_create_text(&created, kind.columns, &what)?;
    let database_collation = single_text(&take(&mut sets, 2)?, "dbc")?;
    let stored_comment = single_text(&take(&mut sets, 3)?, "comment")?;
    let object_mode = single_text(&created, "sql_mode")?.unwrap_or_default();
    let kept = Kept::of(&created)?.differences(&settings, database_collation.as_deref());
    let lex = Lex::for_sql_mode(&object_mode, maria);
    let client = single_text(&created, "character_set_client")?;
    let create = match (stored_comment, client) {
        (Some(comment), Some(client)) if !comment.is_ascii() => {
            repaired_comment(session, &create, lex, &comment, &client, &what).await?
        }
        _ => create,
    };
    let head = text::create_head(&create, lex, kind.keywords).ok_or_else(|| no_structure(&what))?;
    let modifier = if maria {
        Modifier::OrReplace
    } else {
        Modifier::None
    };
    let mut edits = head_edits(&head, database, modifier)?;
    if object.kind == ObjectKind::Trigger {
        let (table, row_end, ordered) =
            text::trigger_parts(&create, lex, &head).ok_or_else(|| no_structure(&what))?;
        if !table.qualified {
            edits.push((table.start, table.end, qname(database, &table.name)?));
        }
        if !ordered {
            let triggers =
                schema::load_triggers(session, server, database, object.parent.as_deref()).await?;
            if let Some(trigger) = triggers.iter().find(|t| &t.name == name)
                && let Some(order) = trigger_order_clause(trigger, &triggers)?
            {
                edits.push((row_end, row_end, order));
            }
        }
    }
    let create = text::apply_edits(&create, edits);
    let mut statements = Vec::new();
    if !maria {
        statements.push(format!("DROP {} IF EXISTS {quoted}", kind.show));
    }
    statements.push(create);
    with_header(&kept, script(&statements))
}

/// `create` with the `COMMENT` literal that `SHOW CREATE` double encoded
/// put right.
///
/// MySQL and MariaDB keep a routine's or event's comment in UTF-8, but
/// `SHOW CREATE` reads its UTF-8 bytes as the object's client character set
/// and converts that to utf8mb4, so a comment created with, say, a cp1251
/// client comes back as mojibake. The server computes that double encoded
/// form of the stored comment with its own conversion,
/// `CONVERT(CONVERT(CAST(CONVERT(comment USING utf8mb4) AS BINARY) USING
/// <client charset>) USING utf8mb4)`, and the literal is replaced only when
/// it is exactly that text. UTF-8 clients, ASCII comments and anything that
/// does not match are left as the server wrote them.
async fn repaired_comment(
    session: &dyn Session,
    create: &str,
    lex: Lex,
    comment: &str,
    client: &str,
    what: &str,
) -> CatalogResult<String> {
    let charset = client.to_ascii_lowercase();
    let utf8 = matches!(charset.as_str(), "utf8" | "utf8mb3" | "utf8mb4" | "binary");
    let valid = !charset.is_empty()
        && charset
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if utf8 || !valid {
        return Ok(create.to_owned());
    }
    let sql = format!(
        "SELECT CONVERT(CONVERT(CAST(CONVERT({} USING utf8mb4) AS BINARY) USING {charset}) \
         USING utf8mb4) AS `broken`",
        lit(comment)?
    );
    let rows = query_all(session, &sql, what).await?;
    let broken = match rows.first() {
        Some(rows) => single_text(rows, "broken")?,
        None => None,
    };
    Ok(match broken {
        Some(broken) if broken != comment => {
            text::repair_comment(create, lex, &broken, &lit(comment)?)
        }
        _ => create.to_owned(),
    })
}

/// The events of a trigger (`INSERT`, or `INSERT,UPDATE` on MariaDB 12).
fn trigger_events(trigger: &super::types::MyTrigger) -> Vec<String> {
    trigger
        .event
        .split(',')
        .map(|e| e.trim().to_ascii_uppercase())
        .collect()
}

/// The clause that puts a re-created trigger back in its place.
///
/// A trigger re-created without a clause goes last, which is right for the
/// last one. Otherwise it is `PRECEDES <next>`. MariaDB only accepts a
/// reference to a trigger that has all of this trigger's events, so a
/// MariaDB 12 trigger for several events whose next trigger has fewer
/// events uses `FOLLOWS <previous>` when the previous one has them all;
/// when neither neighbour does, no clause can express the place and the
/// trigger goes last.
fn trigger_order_clause(
    trigger: &super::types::MyTrigger,
    all: &[super::types::MyTrigger],
) -> CatalogResult<Option<String>> {
    let events = trigger_events(trigger);
    let covers = |name: &str| {
        all.iter().find(|t| t.name == name).is_some_and(|t| {
            let theirs = trigger_events(t);
            events.iter().all(|e| theirs.contains(e))
        })
    };
    let Some(next) = &trigger.precedes else {
        return Ok(None);
    };
    if covers(next) {
        return Ok(Some(format!(" PRECEDES {}", id(next)?)));
    }
    match &trigger.follows {
        Some(prev) if covers(prev) => Ok(Some(format!(" FOLLOWS {}", id(prev)?))),
        _ => Ok(None),
    }
}

// ------------------------------------------- columns, indexes, constraints

async fn table_item_ddl(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<String> {
    let maria = is_mariadb(server);
    let database = database_of(object)?;
    let table = parent_of(object)?;
    let (create, lex) = show_create_table(session, server, database, table, "TABLE").await?;
    let what = format!("table {database}.{table}");
    let parts = text::table_parts(&create, lex).ok_or_else(|| no_structure(&what))?;
    let items: Vec<(TableItem, &String)> = parts
        .items
        .iter()
        .map(|item| (text::table_item(item, lex), item))
        .collect();
    // Changing the columns or constraints of a MariaDB system-versioned
    // table needs the history kept explicitly.
    let versioned = maria && text::has_system_versioning(&parts.tail, lex);
    let keep = if versioned {
        "SET STATEMENT system_versioning_alter_history = KEEP FOR "
    } else {
        ""
    };
    let alter = format!("{keep}ALTER TABLE {}", qname(database, table)?);
    let name = object.name.as_str();
    let not_found = || CatalogError::NotFound {
        message: format!(
            "{:?} {name} of {database}.{table} was not found",
            object.kind
        ),
    };
    let find = |wanted: &TableItem| {
        items
            .iter()
            .find(|(kind, _)| kind == wanted)
            .map(|(_, i)| *i)
    };
    let column_add = |column: &str| -> CatalogResult<Option<String>> {
        let Some(at) = items
            .iter()
            .position(|(kind, _)| *kind == TableItem::Column(column.to_owned()))
        else {
            return Ok(None);
        };
        let previous = items[..at].iter().rev().find_map(|(kind, _)| match kind {
            TableItem::Column(prev) => Some(prev.clone()),
            _ => None,
        });
        let position = match previous {
            Some(prev) => format!(" AFTER {}", id(&prev)?),
            None => " FIRST".to_owned(),
        };
        let if_not_exists = if maria { "IF NOT EXISTS " } else { "" };
        Ok(Some(format!(
            "{alter} ADD COLUMN {if_not_exists}{}{position}",
            items[at].1
        )))
    };
    let index_add = |item: &str, primary: bool| -> String {
        match text::key_keyword_end(item, lex) {
            Some(end) if maria && !primary => format!(
                "{alter} ADD {}",
                text::apply_edits(item, vec![(end, end, " IF NOT EXISTS".to_owned())])
            ),
            _ => format!("{alter} ADD {item}"),
        }
    };
    match object.kind {
        ObjectKind::Column => column_add(name)?.ok_or_else(not_found),
        ObjectKind::Index => {
            if name == "PRIMARY" {
                find(&TableItem::PrimaryKey)
                    .map(|item| index_add(item, true))
                    .ok_or_else(not_found)
            } else {
                find(&TableItem::Index(name.to_owned()))
                    .map(|item| index_add(item, false))
                    .ok_or_else(not_found)
            }
        }
        ObjectKind::Constraint => {
            if name == "PRIMARY"
                && let Some(item) = find(&TableItem::PrimaryKey)
            {
                return Ok(index_add(item, true));
            }
            if let Some(item) = find(&TableItem::ForeignKey(name.to_owned())) {
                return Ok(index_add(item, false));
            }
            if let Some(item) = find(&TableItem::Check(name.to_owned())) {
                // CONSTRAINT [IF NOT EXISTS] `name` CHECK (...)
                let toks_end = item
                    .get(.."CONSTRAINT".len())
                    .filter(|w| w.eq_ignore_ascii_case("CONSTRAINT"))
                    .map(|_| "CONSTRAINT".len());
                return Ok(match toks_end {
                    Some(end) if maria => format!(
                        "{alter} ADD {}",
                        text::apply_edits(item, vec![(end, end, " IF NOT EXISTS".to_owned())])
                    ),
                    _ => format!("{alter} ADD {item}"),
                });
            }
            if let Some(item) = find(&TableItem::Index(name.to_owned())) {
                return Ok(index_add(item, false));
            }
            // MariaDB checks `start < end` of an application-time period
            // with a constraint named after the period.
            if maria
                && let Some((start, end)) = items.iter().find_map(|(kind, _)| match kind {
                    TableItem::Period {
                        name: period,
                        start,
                        end,
                    } if period == name => Some((start, end)),
                    _ => None,
                })
            {
                return Ok(format!(
                    "{alter} ADD PERIOD IF NOT EXISTS FOR {} ({}, {})",
                    id(name)?,
                    id(start)?,
                    id(end)?
                ));
            }
            // A MariaDB column-level check carries its column's name and
            // lives in the column definition.
            if maria
                && let Some((_, item)) = items
                    .iter()
                    .find(|(kind, _)| *kind == TableItem::Column(name.to_owned()))
            {
                return Ok(format!("{alter} MODIFY COLUMN {item}"));
            }
            Err(not_found())
        }
        other => Err(CatalogError::Invalid {
            message: format!("{other:?} is not part of a table definition"),
        }),
    }
}

// --------------------------------------------------------------- partitions

async fn partition_ddl(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<String> {
    let maria = is_mariadb(server);
    let database = database_of(object)?;
    let table = parent_of(object)?;
    let partitioning = schema::load_partitioning(session, database, table)
        .await?
        .ok_or_else(|| CatalogError::NotFound {
            message: format!("{database}.{table} is not partitioned"),
        })?;
    // A subpartition is created with its partition.
    let partition = partitioning
        .partitions
        .iter()
        .find(|p| p.name == object.name || p.subpartitions.iter().any(|s| s.name == object.name))
        .ok_or_else(|| CatalogError::NotFound {
            message: format!(
                "partition {} of {database}.{table} was not found",
                object.name
            ),
        })?;
    let (create, lex) = show_create_table(session, server, database, table, "TABLE").await?;
    let what = format!("table {database}.{table}");
    let parts = text::table_parts(&create, lex).ok_or_else(|| no_structure(&what))?;
    let listed = text::partition_items(&parts.tail, lex).unwrap_or_default();
    let definition_of = |p: &super::types::MyPartition| -> CatalogResult<String> {
        if let Some(item) = listed
            .iter()
            .find(|item| text::partition_item_name(item, lex).as_deref() == Some(p.name.as_str()))
        {
            return Ok(item.clone());
        }
        let mut def = format!("PARTITION {}", id(&p.name)?);
        if let Some(values) = &p.values {
            def.push(' ');
            def.push_str(values);
        }
        if let Some(comment) = &p.comment {
            def.push_str(&format!(" COMMENT = {}", lit(comment)?));
        }
        if !p.subpartitions.is_empty() {
            let subs: Vec<String> = p
                .subpartitions
                .iter()
                .map(|s| Ok(format!("SUBPARTITION {}", id(&s.name)?)))
                .collect::<CatalogResult<_>>()?;
            def.push_str(&format!(" ({})", subs.join(", ")));
        }
        Ok(def)
    };
    let definition = definition_of(partition)?;
    let quoted = qname(database, table)?;
    // Range and list partitions keep their order: one that is not last is
    // put back by splitting it off the next one.
    let ordered = partitioning
        .method
        .to_ascii_uppercase()
        .starts_with("RANGE")
        || partitioning.method.to_ascii_uppercase().starts_with("LIST");
    let next = partitioning
        .partitions
        .iter()
        .skip_while(|p| p.name != partition.name)
        .nth(1);
    if ordered && let Some(next) = next {
        return Ok(format!(
            "ALTER TABLE {quoted} REORGANIZE PARTITION {} INTO ({definition}, {})",
            id(&next.name)?,
            definition_of(next)?
        ));
    }
    let if_not_exists = if maria { " IF NOT EXISTS" } else { "" };
    Ok(format!(
        "ALTER TABLE {quoted} ADD PARTITION{if_not_exists} ({definition})"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads_are_rewritten() -> Result<(), String> {
        let lex = Lex::for_sql_mode("", false);
        let sql = "CREATE DEFINER=`r`@`%` PROCEDURE `p`(a int) begin end";
        let head = text::create_head(sql, lex, &["PROCEDURE"]).ok_or("no head")?;
        let edits = head_edits(&head, "d", Modifier::OrReplace).map_err(|e| e.to_string())?;
        assert_eq!(
            text::apply_edits(sql, edits),
            "CREATE OR REPLACE DEFINER=`r`@`%` PROCEDURE `d`.`p`(a int) begin end"
        );
        let sql = "CREATE TABLE `t` (\n  `a` int\n) ENGINE=InnoDB";
        let head = text::create_head(sql, lex, &["TABLE"]).ok_or("no head")?;
        let edits = head_edits(&head, "d", Modifier::IfNotExists).map_err(|e| e.to_string())?;
        assert_eq!(
            text::apply_edits(sql, edits),
            "CREATE TABLE IF NOT EXISTS `d`.`t` (\n  `a` int\n) ENGINE=InnoDB"
        );
        Ok(())
    }
}
