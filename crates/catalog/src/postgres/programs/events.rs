//! Event triggers (`pg_event_trigger`).

use serde::{Deserialize, Serialize};
use varve_driver::Session;

use super::common::{
    PgMeta, bad_code, comment_on, extension_sql, fetch_one, ident, lit, meta, owner_to, script,
    summary, texts, with_system,
};
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef, ObjectSummary};
use crate::rows::Row;

/// When an event trigger fires (`session_replication_role`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgTriggerState {
    /// Fires in `origin` and `local` mode (`ENABLE`).
    Enabled,
    /// Never fires (`DISABLE`).
    Disabled,
    /// Fires only in `replica` mode (`ENABLE REPLICA`).
    Replica,
    /// Always fires (`ENABLE ALWAYS`).
    Always,
}

impl PgTriggerState {
    /// Parses the `evtenabled` / `tgenabled` code.
    pub(super) fn parse(code: &str) -> CatalogResult<Self> {
        match code {
            "O" => Ok(Self::Enabled),
            "D" => Ok(Self::Disabled),
            "R" => Ok(Self::Replica),
            "A" => Ok(Self::Always),
            other => Err(bad_code("evtenabled", other)),
        }
    }
}

/// An event trigger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgEventTrigger {
    /// Name.
    pub name: String,
    /// Event (`ddl_command_start`, `ddl_command_end`, `sql_drop`,
    /// `table_rewrite`, `login`).
    pub event: String,
    /// Command tags of the `WHEN TAG IN` filter; empty for every command.
    pub tags: Vec<String>,
    /// Function as SQL (schema-qualified outside `pg_catalog`).
    pub function: String,
    /// Firing state.
    pub state: PgTriggerState,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

fn base(filter: &str, extra_columns: &str) -> String {
    format!(
        "SELECT e.oid, pg_catalog.current_database() AS db, e.evtname AS name, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) e.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_event_trigger'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {ext} AS extension, \
         false AS system_schema, e.evtevent AS event, e.evtenabled::pg_catalog.text AS state{extra_columns} \
         FROM pg_catalog.pg_event_trigger e WHERE true{filter}",
        ext = extension_sql("pg_catalog.pg_event_trigger", "e.oid"),
    )
}

/// Inner list query (see [`super::common::with_system`]).
pub(super) fn inner() -> String {
    base("", "")
}

/// Order of the list.
pub(super) const ORDER: &str = "q.name";

/// A tree entry from a row of [`inner`].
pub(super) fn summarize(row: &Row<'_>) -> CatalogResult<ObjectSummary> {
    let flags = match PgTriggerState::parse(&row.req_text("state")?)? {
        PgTriggerState::Enabled => Vec::new(),
        PgTriggerState::Disabled => vec!["disabled".to_owned()],
        PgTriggerState::Replica => vec!["replica".to_owned()],
        PgTriggerState::Always => vec!["always".to_owned()],
    };
    summary(
        ObjectKind::EventTrigger,
        row,
        false,
        None,
        Some(row.req_text("event")?),
        flags,
    )
}

/// Details of an event trigger.
pub(super) async fn describe(
    session: &dyn Session,
    object: &ObjectRef,
) -> CatalogResult<PgEventTrigger> {
    let main = base(
        &format!(
            " AND e.evtname OPERATOR(pg_catalog.=) {}",
            lit(&object.name)?
        ),
        ", pg_catalog.pg_get_userbyid(e.evtowner) AS owner, e.evttags::pg_catalog.text[] AS tags, \
         e.evtfoid::pg_catalog.regproc::pg_catalog.text AS function",
    );
    let rows = fetch_one(session, &with_system(&main, true), "q.name").await?;
    if rows.is_empty() {
        return Err(CatalogError::NotFound {
            message: format!("event trigger {} was not found", object.name),
        });
    }
    let row = rows.single("the event trigger")?;
    Ok(PgEventTrigger {
        name: row.req_text("name")?,
        event: row.req_text("event")?,
        tags: texts(&row, "tags")?,
        function: row.req_text("function")?,
        state: PgTriggerState::parse(&row.req_text("state")?)?,
        meta: meta(&row)?,
    })
}

/// `CREATE EVENT TRIGGER` with its state, owner and comment.
pub(super) fn ddl(trigger: &PgEventTrigger) -> CatalogResult<String> {
    let name = ident(&trigger.name)?;
    let mut create = format!("CREATE EVENT TRIGGER {name} ON {}", ident(&trigger.event)?);
    if !trigger.tags.is_empty() {
        let tags = trigger
            .tags
            .iter()
            .map(|tag| lit(tag))
            .collect::<CatalogResult<Vec<_>>>()?;
        create.push_str(&format!("\n    WHEN TAG IN ({})", tags.join(", ")));
    }
    create.push_str(&format!("\n    EXECUTE FUNCTION {}()", trigger.function));
    let target = format!("EVENT TRIGGER {name}");
    let mut statements = vec![create];
    match trigger.state {
        PgTriggerState::Enabled => {}
        PgTriggerState::Disabled => statements.push(format!("ALTER {target} DISABLE")),
        PgTriggerState::Replica => statements.push(format!("ALTER {target} ENABLE REPLICA")),
        PgTriggerState::Always => statements.push(format!("ALTER {target} ENABLE ALWAYS")),
    }
    statements.extend(owner_to(&target, trigger.meta.owner.as_deref())?);
    statements.extend(comment_on(&target, trigger.meta.comment.as_deref())?);
    Ok(script(statements))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_triggers_keep_tags_and_state() -> CatalogResult<()> {
        let trigger = PgEventTrigger {
            name: "t".into(),
            event: "ddl_command_start".into(),
            tags: vec!["CREATE TABLE".into()],
            function: "s.f".into(),
            state: PgTriggerState::Disabled,
            meta: PgMeta::default(),
        };
        assert_eq!(
            ddl(&trigger)?,
            "CREATE EVENT TRIGGER \"t\" ON \"ddl_command_start\"\n    \
             WHEN TAG IN ('CREATE TABLE')\n    EXECUTE FUNCTION s.f();\n\n\
             ALTER EVENT TRIGGER \"t\" DISABLE;\n"
        );
        Ok(())
    }
}
