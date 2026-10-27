//! Views, stored procedures and functions, events, and the MariaDB-only
//! sequences and packages.

use varve_driver::{ServerInfo, Session};

use super::schema;
use super::text::{self, Lex};
use super::types::{
    MyEvent, MyEventScheduler, MyEventStatus, MyPackage, MyParameter, MyRoutine, MySchedule,
    MySequence, MyView,
};
use super::util::{
    at_least, is_mariadb, is_system_db, lit, object_ref, qname, query_all, query_one, req, take,
    text, text_nonempty, uint, uint32, yes,
};
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectSummary};
use crate::rows::Rows;

// -------------------------------------------------------------------- views

/// The statements that read views: `TABLES` (views and system views)
/// and `VIEWS`, each filtered by constants and joined in Rust (a join makes
/// MariaDB open every table of the server, see `schema::columns_statements`).
fn views_statements(
    server: &ServerInfo,
    database: &str,
    name: Option<&str>,
    with_definition: bool,
) -> CatalogResult<Vec<String>> {
    let algorithm = if is_mariadb(server) {
        "ALGORITHM"
    } else {
        "NULL"
    };
    let definition = if with_definition {
        "VIEW_DEFINITION"
    } else {
        "NULL"
    };
    let filter = match name {
        Some(name) => format!(" AND TABLE_NAME = {}", lit(name)?),
        None => String::new(),
    };
    let db = lit(database)?;
    Ok(vec![
        format!(
            "SELECT TABLE_NAME AS `name`, TABLE_TYPE AS `table_type` FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = {db} AND TABLE_TYPE IN ('VIEW', 'SYSTEM VIEW'){filter} \
             ORDER BY TABLE_NAME"
        ),
        format!(
            "SELECT TABLE_NAME AS `name`, SECURITY_TYPE AS `security`, CHECK_OPTION AS `check_option`, \
             IS_UPDATABLE AS `updatable`, DEFINER AS `definer`, {definition} AS `definition`, \
             CHARACTER_SET_CLIENT AS `charset_client`, COLLATION_CONNECTION AS `collation_connection`, \
             {algorithm} AS `algorithm` FROM information_schema.VIEWS \
             WHERE TABLE_SCHEMA = {db}{filter}"
        ),
    ])
}

/// Reads the results of [`views_statements`].
fn parse_views(sets: &[Rows], database: &str) -> CatalogResult<Vec<MyView>> {
    let missing = || CatalogError::Unexpected {
        message: "the view query returned nothing".into(),
    };
    let views = sets.get(1).ok_or_else(missing)?;
    let mut out = Vec::new();
    for row in sets.first().ok_or_else(missing)?.iter() {
        let name = req(&row, "name")?;
        let system =
            is_system_db(database) || req(&row, "table_type")?.eq_ignore_ascii_case("SYSTEM VIEW");
        let mut view = MyView {
            database: database.to_owned(),
            name: name.clone(),
            definition: None,
            algorithm: None,
            security: None,
            definer: None,
            check_option: None,
            updatable: None,
            charset_client: None,
            collation_connection: None,
            columns: Vec::new(),
            system,
        };
        if let Some(v) = views
            .iter()
            .find(|v| text(v, "name").ok().flatten().as_deref() == Some(name.as_str()))
        {
            view.definition = text(&v, "definition")?;
            view.algorithm = text_nonempty(&v, "algorithm")?;
            view.security = text_nonempty(&v, "security")?;
            view.definer = text_nonempty(&v, "definer")?;
            view.check_option = text_nonempty(&v, "check_option")?;
            view.updatable = text(&v, "updatable")?.map(|u| u.eq_ignore_ascii_case("YES"));
            view.charset_client = text_nonempty(&v, "charset_client")?;
            view.collation_connection = text_nonempty(&v, "collation_connection")?;
        }
        out.push(view);
    }
    Ok(out)
}

fn view_summary(view: &MyView) -> ObjectSummary {
    let mut flags = Vec::new();
    if view
        .check_option
        .as_deref()
        .is_some_and(|c| !c.eq_ignore_ascii_case("NONE"))
    {
        flags.push("check option".to_owned());
    }
    if view.updatable == Some(false) {
        flags.push("read only".to_owned());
    }
    ObjectSummary {
        object: object_ref(ObjectKind::View, &view.database, None, &view.name),
        comment: None,
        detail: view.security.as_ref().map(|s| format!("SQL SECURITY {s}")),
        flags,
        has_children: true,
        system: view.system,
        row_estimate: None,
        size_bytes: None,
    }
}

/// Views of a database (system views marked).
pub(crate) async fn list_views(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
) -> CatalogResult<Vec<ObjectSummary>> {
    let sql = views_statements(server, database, None, false)?.join(";\n");
    let sets = query_all(session, &sql, "the database").await?;
    Ok(parse_views(&sets, database)?
        .iter()
        .map(view_summary)
        .collect())
}

/// Details of one view with its columns.
pub(crate) async fn describe_view(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
    name: &str,
) -> CatalogResult<MyView> {
    let what = format!("view {database}.{name}");
    let sql = views_statements(server, database, Some(name), true)?.join(";\n");
    let sets = query_all(session, &sql, &what).await?;
    let mut view = parse_views(&sets, database)?
        .into_iter()
        .next()
        .ok_or_else(|| CatalogError::NotFound {
            message: format!("{what} was not found"),
        })?;
    view.columns = schema::load_columns(session, server, database, name, None).await?;
    if view.algorithm.is_none() && !view.system {
        let sql = format!(
            "SELECT @@SESSION.sql_mode AS `mode`;\nSHOW CREATE VIEW {}",
            qname(database, name)?
        );
        let mut sets = query_all(session, &sql, &what).await?;
        let mode = single_text(&take(&mut sets, 0)?, "mode")?.unwrap_or_default();
        let create = single_text(&take(&mut sets, 1)?, "Create View")?.unwrap_or_default();
        view.algorithm = text::view_algorithm(&create, Lex::for_sql_mode(&mode, false));
    }
    Ok(view)
}

/// The text of `column` in the first row of a result, if any.
pub(crate) fn single_text(rows: &Rows, column: &str) -> CatalogResult<Option<String>> {
    match rows.iter().next() {
        Some(row) => text(&row, column),
        None => Ok(None),
    }
}

// ----------------------------------------------------------------- routines

/// The `ROUTINE_TYPE` of routine-like kinds.
pub(crate) fn routine_type(kind: ObjectKind) -> Option<&'static str> {
    match kind {
        ObjectKind::Procedure => Some("PROCEDURE"),
        ObjectKind::Function => Some("FUNCTION"),
        ObjectKind::Package => Some("PACKAGE"),
        ObjectKind::PackageBody => Some("PACKAGE BODY"),
        _ => None,
    }
}

fn routines_sql(
    database: &str,
    routine_type: &str,
    name: Option<&str>,
    with_body: bool,
) -> CatalogResult<String> {
    let body = if with_body {
        "ROUTINE_DEFINITION"
    } else {
        "NULL"
    };
    let filter = match name {
        Some(name) => format!(" AND ROUTINE_NAME = {}", lit(name)?),
        None => String::new(),
    };
    Ok(format!(
        "SELECT ROUTINE_NAME AS `name`, DTD_IDENTIFIER AS `returns`, \
         IS_DETERMINISTIC AS `deterministic`, SQL_DATA_ACCESS AS `data_access`, \
         SECURITY_TYPE AS `security`, DEFINER AS `definer`, SQL_MODE AS `sql_mode`, \
         ROUTINE_COMMENT AS `comment`, {body} AS `body`, CREATED AS `created`, \
         LAST_ALTERED AS `altered`, CHARACTER_SET_CLIENT AS `charset_client`, \
         COLLATION_CONNECTION AS `collation_connection`, DATABASE_COLLATION AS `database_collation` \
         FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = {} AND ROUTINE_TYPE = {}{filter} \
         ORDER BY ROUTINE_NAME",
        lit(database)?,
        lit(routine_type)?
    ))
}

fn parse_routine(
    row: &crate::rows::Row<'_>,
    database: &str,
    function: bool,
) -> CatalogResult<MyRoutine> {
    Ok(MyRoutine {
        database: database.to_owned(),
        name: req(row, "name")?,
        parameters: Vec::new(),
        returns: if function {
            text_nonempty(row, "returns")?
        } else {
            None
        },
        deterministic: yes(row, "deterministic")?,
        data_access: text_nonempty(row, "data_access")?,
        security: text_nonempty(row, "security")?,
        definer: text_nonempty(row, "definer")?,
        sql_mode: text(row, "sql_mode")?,
        comment: text_nonempty(row, "comment")?,
        body: text(row, "body")?,
        created: text(row, "created")?,
        altered: text(row, "altered")?,
        charset_client: text_nonempty(row, "charset_client")?,
        collation_connection: text_nonempty(row, "collation_connection")?,
        database_collation: text_nonempty(row, "database_collation")?,
    })
}

/// Procedures or functions of a database.
pub(crate) async fn list_routines(
    session: &dyn Session,
    database: &str,
    kind: ObjectKind,
) -> CatalogResult<Vec<ObjectSummary>> {
    let routine_type = routine_type(kind).ok_or_else(|| CatalogError::Invalid {
        message: format!("{kind:?} is not a routine"),
    })?;
    let sql = routines_sql(database, routine_type, None, false)?;
    let rows = query_one(session, &sql, "the database").await?;
    let function = kind == ObjectKind::Function;
    rows.iter()
        .map(|row| {
            let r = parse_routine(&row, database, function)?;
            let mut flags = Vec::new();
            if r.deterministic {
                flags.push("deterministic".to_owned());
            }
            if r.security.as_deref() == Some("INVOKER") {
                flags.push("invoker".to_owned());
            }
            Ok(ObjectSummary {
                object: object_ref(kind, database, None, &r.name),
                comment: r.comment.clone(),
                detail: r.returns.as_ref().map(|t| format!("returns {t}")),
                flags,
                has_children: false,
                system: is_system_db(database),
                row_estimate: None,
                size_bytes: None,
            })
        })
        .collect()
}

/// Details of one procedure or function, with its parameters.
pub(crate) async fn describe_routine(
    session: &dyn Session,
    database: &str,
    kind: ObjectKind,
    name: &str,
) -> CatalogResult<MyRoutine> {
    let routine_type = routine_type(kind).ok_or_else(|| CatalogError::Invalid {
        message: format!("{kind:?} is not a routine"),
    })?;
    let sql = format!(
        "{};\nSELECT ORDINAL_POSITION AS `position`, PARAMETER_MODE AS `mode`, \
         PARAMETER_NAME AS `name`, DTD_IDENTIFIER AS `data_type` \
         FROM information_schema.PARAMETERS WHERE SPECIFIC_SCHEMA = {} AND SPECIFIC_NAME = {} \
         AND ROUTINE_TYPE = {} AND ORDINAL_POSITION > 0 ORDER BY ORDINAL_POSITION",
        routines_sql(database, routine_type, Some(name), true)?,
        lit(database)?,
        lit(name)?,
        lit(routine_type)?
    );
    let what = format!("{} {database}.{name}", routine_type.to_ascii_lowercase());
    let mut sets = query_all(session, &sql, &what).await?;
    let routines = take(&mut sets, 0)?;
    let mut routine = parse_routine(
        &routines.single(&what)?,
        database,
        kind == ObjectKind::Function,
    )?;
    routine.parameters = take(&mut sets, 1)?
        .iter()
        .map(|row| {
            Ok(MyParameter {
                position: uint32(&row, "position")?.unwrap_or(0),
                mode: text_nonempty(&row, "mode")?,
                name: text_nonempty(&row, "name")?,
                data_type: text(&row, "data_type")?.unwrap_or_default(),
            })
        })
        .collect::<CatalogResult<_>>()?;
    Ok(routine)
}

// ----------------------------------------------------------------- packages

fn parse_package(row: &crate::rows::Row<'_>, database: &str) -> CatalogResult<MyPackage> {
    Ok(MyPackage {
        database: database.to_owned(),
        name: req(row, "name")?,
        definer: text_nonempty(row, "definer")?,
        security: text_nonempty(row, "security")?,
        sql_mode: text(row, "sql_mode")?,
        comment: text_nonempty(row, "comment")?,
        body: text(row, "body")?,
        created: text(row, "created")?,
        altered: text(row, "altered")?,
    })
}

/// MariaDB packages or package bodies of a database.
pub(crate) async fn list_packages(
    session: &dyn Session,
    database: &str,
    kind: ObjectKind,
) -> CatalogResult<Vec<ObjectSummary>> {
    let routine_type = routine_type(kind).ok_or_else(|| CatalogError::Invalid {
        message: format!("{kind:?} is not a package"),
    })?;
    let sql = routines_sql(database, routine_type, None, false)?;
    let rows = query_one(session, &sql, "the database").await?;
    rows.iter()
        .map(|row| {
            let p = parse_package(&row, database)?;
            Ok(ObjectSummary {
                object: object_ref(kind, database, None, &p.name),
                comment: p.comment.clone(),
                detail: None,
                flags: Vec::new(),
                has_children: false,
                system: is_system_db(database),
                row_estimate: None,
                size_bytes: None,
            })
        })
        .collect()
}

/// Details of one MariaDB package or package body.
pub(crate) async fn describe_package(
    session: &dyn Session,
    database: &str,
    kind: ObjectKind,
    name: &str,
) -> CatalogResult<MyPackage> {
    let routine_type = routine_type(kind).ok_or_else(|| CatalogError::Invalid {
        message: format!("{kind:?} is not a package"),
    })?;
    let what = format!("{} {database}.{name}", routine_type.to_ascii_lowercase());
    let rows = query_one(
        session,
        &routines_sql(database, routine_type, Some(name), true)?,
        &what,
    )
    .await?;
    parse_package(&rows.single(&what)?, database)
}

// ------------------------------------------------------------------- events

fn events_sql(database: &str, name: Option<&str>) -> CatalogResult<String> {
    let filter = match name {
        Some(name) => format!(" AND EVENT_NAME = {}", lit(name)?),
        None => String::new(),
    };
    Ok(format!(
        "SELECT EVENT_NAME AS `name`, DEFINER AS `definer`, TIME_ZONE AS `time_zone`, \
         EVENT_TYPE AS `event_type`, EXECUTE_AT AS `execute_at`, \
         CAST(INTERVAL_VALUE AS CHAR) AS `interval_value`, INTERVAL_FIELD AS `interval_field`, \
         STARTS AS `starts`, ENDS AS `ends`, STATUS AS `status`, ON_COMPLETION AS `on_completion`, \
         SQL_MODE AS `sql_mode`, EVENT_COMMENT AS `comment`, EVENT_DEFINITION AS `body`, \
         CREATED AS `created`, LAST_ALTERED AS `altered`, LAST_EXECUTED AS `last_executed`, \
         ORIGINATOR AS `originator` \
         FROM information_schema.EVENTS WHERE EVENT_SCHEMA = {}{filter} ORDER BY EVENT_NAME",
        lit(database)?
    ))
}

/// The `@@event_scheduler` value read as a state.
pub(crate) fn parse_scheduler(value: Option<&str>) -> MyEventScheduler {
    match value.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("ON") || v == "1" => MyEventScheduler::On,
        Some(v) if v.eq_ignore_ascii_case("DISABLED") => MyEventScheduler::Disabled,
        _ => MyEventScheduler::Off,
    }
}

/// The query that reads the event scheduler state as `scheduler`.
pub(crate) const SCHEDULER_SQL: &str = "SELECT @@GLOBAL.event_scheduler AS `scheduler`";

fn parse_event(
    row: &crate::rows::Row<'_>,
    database: &str,
    scheduler: MyEventScheduler,
) -> CatalogResult<MyEvent> {
    let one_time = text(row, "event_type")?.is_some_and(|t| t.eq_ignore_ascii_case("ONE TIME"));
    let schedule = if one_time {
        MySchedule::At {
            at: text(row, "execute_at")?,
        }
    } else {
        MySchedule::Every {
            interval_value: text(row, "interval_value")?,
            interval_field: text(row, "interval_field")?,
            starts: text(row, "starts")?,
            ends: text(row, "ends")?,
        }
    };
    let status = match text(row, "status")?
        .unwrap_or_default()
        .to_ascii_uppercase()
        .as_str()
    {
        "ENABLED" => MyEventStatus::Enabled,
        "DISABLED" => MyEventStatus::Disabled,
        _ => MyEventStatus::SlavesideDisabled,
    };
    Ok(MyEvent {
        database: database.to_owned(),
        name: req(row, "name")?,
        definer: text_nonempty(row, "definer")?,
        time_zone: text_nonempty(row, "time_zone")?,
        schedule,
        status,
        preserve: text(row, "on_completion")?
            .is_some_and(|c| c.trim().eq_ignore_ascii_case("PRESERVE")),
        sql_mode: text(row, "sql_mode")?,
        comment: text_nonempty(row, "comment")?,
        body: text(row, "body")?,
        created: text(row, "created")?,
        altered: text(row, "altered")?,
        last_executed: text(row, "last_executed")?,
        originator: uint(row, "originator")?,
        scheduler,
    })
}

fn schedule_text(schedule: &MySchedule) -> String {
    match schedule {
        MySchedule::At { at } => format!("AT {}", at.as_deref().unwrap_or("?")),
        MySchedule::Every {
            interval_value,
            interval_field,
            ..
        } => format!(
            "EVERY {} {}",
            interval_value.as_deref().unwrap_or("?"),
            interval_field.as_deref().unwrap_or("?")
        ),
    }
}

/// Events of a database.
pub(crate) async fn list_events(
    session: &dyn Session,
    database: &str,
) -> CatalogResult<Vec<ObjectSummary>> {
    let rows = query_one(session, &events_sql(database, None)?, "the database").await?;
    rows.iter()
        .map(|row| {
            let e = parse_event(&row, database, MyEventScheduler::Off)?;
            let flags = match e.status {
                MyEventStatus::Enabled => Vec::new(),
                MyEventStatus::Disabled => vec!["disabled".to_owned()],
                MyEventStatus::SlavesideDisabled => vec!["disabled on replica".to_owned()],
            };
            Ok(ObjectSummary {
                object: object_ref(ObjectKind::Event, database, None, &e.name),
                comment: e.comment.clone(),
                detail: Some(schedule_text(&e.schedule)),
                flags,
                has_children: false,
                system: is_system_db(database),
                row_estimate: None,
                size_bytes: None,
            })
        })
        .collect()
}

/// Details of one event, with the server's event scheduler state.
pub(crate) async fn describe_event(
    session: &dyn Session,
    database: &str,
    name: &str,
) -> CatalogResult<MyEvent> {
    let sql = format!("{};\n{SCHEDULER_SQL}", events_sql(database, Some(name))?);
    let what = format!("event {database}.{name}");
    let mut sets = query_all(session, &sql, &what).await?;
    let events = take(&mut sets, 0)?;
    let scheduler = parse_scheduler(single_text(&take(&mut sets, 1)?, "scheduler")?.as_deref());
    parse_event(&events.single(&what)?, database, scheduler)
}

// ---------------------------------------------------------------- sequences

/// MariaDB sequences of a database.
pub(crate) async fn list_sequences(
    session: &dyn Session,
    database: &str,
) -> CatalogResult<Vec<ObjectSummary>> {
    let sql = format!(
        "SELECT TABLE_NAME AS `name`, TABLE_COMMENT AS `comment` FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = {} AND TABLE_TYPE = 'SEQUENCE' ORDER BY TABLE_NAME",
        lit(database)?
    );
    let rows = query_one(session, &sql, "the database").await?;
    rows.iter()
        .map(|row| {
            Ok(ObjectSummary {
                object: object_ref(ObjectKind::Sequence, database, None, &req(&row, "name")?),
                comment: text_nonempty(&row, "comment")?,
                detail: None,
                flags: Vec::new(),
                has_children: false,
                system: is_system_db(database),
                row_estimate: None,
                size_bytes: None,
            })
        })
        .collect()
}

/// Details of one MariaDB sequence: its options from the sequence table,
/// the next uncached value and this session's `LASTVAL`.
pub(crate) async fn describe_sequence(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
    name: &str,
) -> CatalogResult<MySequence> {
    let what = format!("sequence {database}.{name}");
    let table = qname(database, name)?;
    let mut statements = vec![
        format!(
            "SELECT TABLE_COMMENT AS `comment`, ENGINE AS `engine` FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} AND TABLE_TYPE = 'SEQUENCE'",
            lit(database)?,
            lit(name)?
        ),
        format!(
            "SELECT CAST(next_not_cached_value AS CHAR) AS `next_value`, \
             CAST(minimum_value AS CHAR) AS `minimum`, CAST(maximum_value AS CHAR) AS `maximum`, \
             CAST(start_value AS CHAR) AS `start`, CAST(increment AS CHAR) AS `increment`, \
             CAST(cache_size AS CHAR) AS `cache`, cycle_option AS `cycle`, \
             CAST(cycle_count AS CHAR) AS `cycle_count` FROM {table}"
        ),
        format!("SELECT CAST(LASTVAL({table}) AS CHAR) AS `last_value`"),
    ];
    let typed = at_least(server, 11, 5, 0);
    if typed {
        statements.push(format!(
            "SELECT DATA_TYPE AS `data_type` FROM information_schema.SEQUENCES \
             WHERE SEQUENCE_SCHEMA = {} AND SEQUENCE_NAME = {}",
            lit(database)?,
            lit(name)?
        ));
    }
    let mut sets = query_all(session, &statements.join(";\n"), &what).await?;
    let info = take(&mut sets, 0)?;
    let info = info.single(&what)?;
    let values = take(&mut sets, 1)?;
    let row = values.single(&what)?;
    let last_value = single_text(&take(&mut sets, 2)?, "last_value")?;
    let data_type = if typed {
        single_text(&take(&mut sets, 3)?, "data_type")?
    } else {
        Some("bigint".to_owned())
    };
    Ok(MySequence {
        database: database.to_owned(),
        name: name.to_owned(),
        data_type,
        start: req(&row, "start")?,
        increment: req(&row, "increment")?,
        minimum: req(&row, "minimum")?,
        maximum: req(&row, "maximum")?,
        cache: req(&row, "cache")?,
        cycle: yes(&row, "cycle")?,
        next_value: req(&row, "next_value")?,
        cycle_count: req(&row, "cycle_count")?,
        last_value,
        comment: text_nonempty(&info, "comment")?,
        engine: text_nonempty(&info, "engine")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_states() {
        assert_eq!(parse_scheduler(Some("ON")), MyEventScheduler::On);
        assert_eq!(parse_scheduler(Some("OFF")), MyEventScheduler::Off);
        assert_eq!(
            parse_scheduler(Some("DISABLED")),
            MyEventScheduler::Disabled
        );
        assert_eq!(routine_type(ObjectKind::PackageBody), Some("PACKAGE BODY"));
        assert_eq!(routine_type(ObjectKind::Table), None);
    }
}
