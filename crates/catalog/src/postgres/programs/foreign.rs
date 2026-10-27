//! Foreign data wrappers and foreign servers with their user mappings.

use serde::{Deserialize, Serialize};
use varve_driver::Session;

use super::common::{
    PgMeta, PgOption, REDACTED_NOTE, comment_on, extension_sql, fdw_extension_sql, fetch_one,
    fetch_pinned, ident, lit, meta, option_names, options, options_clause, owner_to, script,
    summary, with_system,
};
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef, ObjectSummary};
use crate::rows::Row;

/// A foreign data wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgForeignDataWrapper {
    /// Name.
    pub name: String,
    /// Handler function as SQL.
    pub handler: Option<String>,
    /// Validator function as SQL.
    pub validator: Option<String>,
    /// Options; values not known to be harmless are hidden (see
    /// [`super::redact_option`]).
    pub options: Vec<PgOption>,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

/// A user mapping of a foreign server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgUserMapping {
    /// The mapped role; `None` for `PUBLIC`.
    pub user: Option<String>,
    /// Option names visible to the current role; values are never
    /// returned, since any of them may be a credential.
    pub options: Vec<PgOption>,
}

/// A foreign server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgForeignServer {
    /// Name.
    pub name: String,
    /// The foreign data wrapper.
    pub wrapper: String,
    /// `TYPE`.
    pub server_type: Option<String>,
    /// `VERSION`.
    pub version: Option<String>,
    /// Options; values not known to be harmless are hidden (see
    /// [`super::redact_option`]).
    pub options: Vec<PgOption>,
    /// User mappings.
    pub user_mappings: Vec<PgUserMapping>,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

fn wrapper_base(filter: &str, extra: &str) -> String {
    format!(
        "SELECT w.oid, pg_catalog.current_database() AS db, w.fdwname AS name, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) w.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_foreign_data_wrapper'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {ext} AS extension, \
         false AS system_schema, (CASE WHEN w.fdwhandler OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN w.fdwhandler END)::pg_catalog.regproc::pg_catalog.text AS handler{extra} \
         FROM pg_catalog.pg_foreign_data_wrapper w WHERE true{filter}",
        ext = extension_sql("pg_catalog.pg_foreign_data_wrapper", "w.oid"),
    )
}

fn server_base(filter: &str, extra: &str) -> String {
    format!(
        "SELECT s.oid, pg_catalog.current_database() AS db, s.srvname AS name, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) s.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_foreign_server'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {ext} AS extension, \
         false AS system_schema, w.fdwname AS wrapper{extra} \
         FROM pg_catalog.pg_foreign_server s JOIN pg_catalog.pg_foreign_data_wrapper w ON w.oid OPERATOR(pg_catalog.=) s.srvfdw \
         WHERE true{filter}",
        ext = extension_sql("pg_catalog.pg_foreign_server", "s.oid"),
    )
}

/// Inner list query (see [`super::common::with_system`]).
pub(super) fn inner(kind: ObjectKind) -> String {
    if kind == ObjectKind::ForeignServer {
        server_base("", "")
    } else {
        wrapper_base("", "")
    }
}

/// Order of the list.
pub(super) const ORDER: &str = "q.name";

/// A tree entry from a row of [`inner`].
pub(super) fn summarize(kind: ObjectKind, row: &Row<'_>) -> CatalogResult<ObjectSummary> {
    let detail = if kind == ObjectKind::ForeignServer {
        Some(row.req_text("wrapper")?)
    } else {
        row.text("handler")?
    };
    summary(kind, row, false, None, detail, Vec::new())
}

fn not_found(object: &ObjectRef) -> CatalogError {
    CatalogError::NotFound {
        message: format!("{:?} {} was not found", object.kind, object.name),
    }
}

/// Details of a foreign data wrapper.
pub(super) async fn describe_wrapper(
    session: &dyn Session,
    object: &ObjectRef,
) -> CatalogResult<PgForeignDataWrapper> {
    let main = wrapper_base(
        &format!(
            " AND w.fdwname OPERATOR(pg_catalog.=) {}",
            lit(&object.name)?
        ),
        &format!(
            ", pg_catalog.pg_get_userbyid(w.fdwowner) AS owner, \
             (CASE WHEN w.fdwvalidator OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN w.fdwvalidator END)::pg_catalog.regproc::pg_catalog.text AS validator, \
             w.fdwoptions::pg_catalog.text[] AS opts, {} AS wrapper_extension",
            fdw_extension_sql("w.oid")
        ),
    );
    let rows = fetch_one(session, &with_system(&main, true), "q.name").await?;
    if rows.is_empty() {
        return Err(not_found(object));
    }
    let row = rows.single("the foreign data wrapper")?;
    Ok(PgForeignDataWrapper {
        name: row.req_text("name")?,
        handler: row.text("handler")?,
        validator: row.text("validator")?,
        options: options(&row, "opts", row.text("wrapper_extension")?.as_deref())?,
        meta: meta(&row)?,
    })
}

/// `CREATE FOREIGN DATA WRAPPER` with owner and comment.
pub(super) fn wrapper_ddl(wrapper: &PgForeignDataWrapper) -> CatalogResult<String> {
    let name = ident(&wrapper.name)?;
    let mut create = format!("CREATE FOREIGN DATA WRAPPER {name}");
    match &wrapper.handler {
        Some(handler) => create.push_str(&format!("\n    HANDLER {handler}")),
        None => create.push_str("\n    NO HANDLER"),
    }
    match &wrapper.validator {
        Some(validator) => create.push_str(&format!("\n    VALIDATOR {validator}")),
        None => create.push_str("\n    NO VALIDATOR"),
    }
    let (clause, hidden) = options_clause(&wrapper.options)?;
    create.push_str(&clause);
    let target = format!("FOREIGN DATA WRAPPER {name}");
    let mut statements = Vec::new();
    if hidden {
        statements.push(REDACTED_NOTE.to_owned());
    }
    statements.push(create);
    statements.extend(owner_to(&target, wrapper.meta.owner.as_deref())?);
    statements.extend(comment_on(&target, wrapper.meta.comment.as_deref())?);
    Ok(script(statements))
}

/// Details of a foreign server and its user mappings.
pub(super) async fn describe_server(
    session: &dyn Session,
    object: &ObjectRef,
) -> CatalogResult<PgForeignServer> {
    let name = lit(&object.name)?;
    let main = server_base(
        &format!(" AND s.srvname OPERATOR(pg_catalog.=) {name}"),
        &format!(
            ", pg_catalog.pg_get_userbyid(s.srvowner) AS owner, s.srvtype AS server_type, \
             s.srvversion AS version, s.srvoptions::pg_catalog.text[] AS opts, {} AS wrapper_extension",
            fdw_extension_sql("w.oid")
        ),
    );
    let mappings = format!(
        "SELECT CASE WHEN um.umuser OPERATOR(pg_catalog.=) 0 THEN NULL ELSE um.usename::pg_catalog.text END AS username, \
         um.umuser OPERATOR(pg_catalog.<>) 0 AS named, um.umoptions::pg_catalog.text[] AS opts FROM pg_catalog.pg_user_mappings um \
         WHERE um.srvname OPERATOR(pg_catalog.=) {name} ORDER BY um.umuser OPERATOR(pg_catalog.<>) 0, um.usename"
    );
    let sets = fetch_pinned(
        session,
        &[
            (with_system(&main, true), "q.name".to_owned()),
            (mappings, "q.named, q.username".to_owned()),
        ],
    )
    .await?;
    let [main_rows, mapping_rows] = sets.as_slice() else {
        return Err(CatalogError::Unexpected {
            message: "the foreign server query returned the wrong number of result sets".into(),
        });
    };
    if main_rows.is_empty() {
        return Err(not_found(object));
    }
    let row = main_rows.single("the foreign server")?;
    let user_mappings = mapping_rows
        .iter()
        .map(|m| {
            Ok(PgUserMapping {
                user: m.text("username")?,
                options: option_names(&m, "opts")?,
            })
        })
        .collect::<CatalogResult<Vec<_>>>()?;
    Ok(PgForeignServer {
        name: row.req_text("name")?,
        wrapper: row.req_text("wrapper")?,
        server_type: row.text("server_type")?,
        version: row.text("version")?,
        options: options(&row, "opts", row.text("wrapper_extension")?.as_deref())?,
        user_mappings,
        meta: meta(&row)?,
    })
}

/// `CREATE SERVER` with owner, comment and user mappings. Hidden option
/// values (every user mapping value) are written as
/// [`super::common::REDACTED`] under a leading note.
pub(super) fn server_ddl(server: &PgForeignServer) -> CatalogResult<String> {
    let name = ident(&server.name)?;
    let mut create = format!("CREATE SERVER {name}");
    if let Some(server_type) = &server.server_type {
        create.push_str(&format!(" TYPE {}", lit(server_type)?));
    }
    if let Some(version) = &server.version {
        create.push_str(&format!(" VERSION {}", lit(version)?));
    }
    create.push_str(&format!(
        "\n    FOREIGN DATA WRAPPER {}",
        ident(&server.wrapper)?
    ));
    let (clause, mut hidden) = options_clause(&server.options)?;
    create.push_str(&clause);
    let target = format!("SERVER {name}");
    let mut statements = vec![create];
    statements.extend(owner_to(&target, server.meta.owner.as_deref())?);
    statements.extend(comment_on(&target, server.meta.comment.as_deref())?);
    for mapping in &server.user_mappings {
        let user = match &mapping.user {
            Some(user) => ident(user)?,
            None => "PUBLIC".to_owned(),
        };
        let (clause, mapping_hidden) = options_clause(&mapping.options)?;
        hidden |= mapping_hidden;
        statements.push(format!(
            "CREATE USER MAPPING FOR {user} SERVER {name}{clause}"
        ));
    }
    if hidden {
        statements.insert(0, REDACTED_NOTE.to_owned());
    }
    Ok(script(statements))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_mappings_leave_passwords_out() -> CatalogResult<()> {
        let server = PgForeignServer {
            name: "srv".into(),
            wrapper: "fdw".into(),
            server_type: None,
            version: Some("1".into()),
            options: vec![PgOption {
                name: "host".into(),
                value: Some("h".into()),
            }],
            user_mappings: vec![
                PgUserMapping {
                    user: None,
                    options: Vec::new(),
                },
                PgUserMapping {
                    user: Some("app".into()),
                    options: vec![
                        PgOption {
                            name: "user".into(),
                            value: Some("remote".into()),
                        },
                        PgOption {
                            name: "password".into(),
                            value: None,
                        },
                    ],
                },
            ],
            meta: PgMeta::default(),
        };
        let ddl = server_ddl(&server)?;
        assert_eq!(
            ddl,
            "-- Option values that may hold secrets are written as '<redacted>': \
             replace them before running this.\n\n\
             CREATE SERVER \"srv\" VERSION '1'\n    FOREIGN DATA WRAPPER \"fdw\" OPTIONS (\"host\" 'h');\n\n\
             CREATE USER MAPPING FOR PUBLIC SERVER \"srv\";\n\n\
             CREATE USER MAPPING FOR \"app\" SERVER \"srv\" \
             OPTIONS (\"user\" 'remote', \"password\" '<redacted>');\n"
        );
        Ok(())
    }
}
