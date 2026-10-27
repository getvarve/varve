//! Logical replication: publications and subscriptions.

use serde::{Deserialize, Serialize};
use varve_driver::{ServerInfo, Session};

use super::common::{
    PgMeta, comment_on, fetch_one, fetch_pinned, flag, ident, is_permission_denied, lit, meta,
    owner_to, qname, script, summary, texts, with_system,
};
use super::conninfo::strip_passwords;
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef, ObjectSummary};
use crate::rows::Row;

/// A table of a publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgPublicationTable {
    /// Schema.
    pub schema: String,
    /// Table name.
    pub name: String,
    /// Published columns (15+); empty for every column.
    pub columns: Vec<String>,
    /// Row filter as SQL (15+).
    pub row_filter: Option<String>,
}

/// A publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgPublication {
    /// Name.
    pub name: String,
    /// `FOR ALL TABLES`.
    pub all_tables: bool,
    /// Publishes `INSERT`.
    pub insert: bool,
    /// Publishes `UPDATE`.
    pub update: bool,
    /// Publishes `DELETE`.
    pub delete: bool,
    /// Publishes `TRUNCATE`.
    pub truncate: bool,
    /// `publish_via_partition_root`.
    pub via_root: bool,
    /// `publish_generated_columns` (18+): `none` or `stored`.
    pub generated_columns: Option<String>,
    /// Tables listed explicitly.
    pub tables: Vec<PgPublicationTable>,
    /// Schemas of `TABLES IN SCHEMA` (15+).
    pub schemas: Vec<String>,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

/// A subscription. The connection string never contains a password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgSubscription {
    /// Name.
    pub name: String,
    /// True when it is enabled.
    pub enabled: bool,
    /// Connection string in `keyword=value` form with every secret
    /// (`password`, `sslpassword`, ...) removed, also from a `dbname` that is
    /// itself a connection string (see [`super::strip_passwords`]); `None`
    /// when `connection_visible` is false.
    pub connection: Option<String>,
    /// False when the connection string is unknown: the role may not read
    /// `pg_subscription.subconninfo` (superuser only; on PostgreSQL 13 the
    /// comment and `synchronous_commit` are then unknown too), or the text
    /// is not a valid libpq connection string and is therefore not shown at
    /// all. No DDL can be built then.
    pub connection_visible: bool,
    /// Replication slot on the publisher; `None` for `slot_name = NONE`.
    pub slot_name: Option<String>,
    /// `synchronous_commit` of the apply worker; `None` when the role may
    /// not read it (PostgreSQL 13 without superuser rights).
    pub synchronous_commit: Option<String>,
    /// Publications subscribed to.
    pub publications: Vec<String>,
    /// `binary` (14+).
    pub binary: Option<bool>,
    /// `streaming` (14+): `off`, `on` or `parallel` (16+).
    pub streaming: Option<String>,
    /// `two_phase` (15+).
    pub two_phase: Option<bool>,
    /// `disable_on_error` (15+).
    pub disable_on_error: Option<bool>,
    /// `password_required` (16+).
    pub password_required: Option<bool>,
    /// `run_as_owner` (16+).
    pub run_as_owner: Option<bool>,
    /// `origin` (16+): `any` or `none`.
    pub origin: Option<String>,
    /// `failover` (17+).
    pub failover: Option<bool>,
    /// False when the comment is unknown: PostgreSQL 13 hides the
    /// subscription's `oid` from roles without superuser rights, so its
    /// comment cannot be looked up and `meta.comment` is `None` without
    /// meaning "no comment". From 14 on every role can read it.
    pub comment_visible: bool,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

fn publication_base(server: &ServerInfo, filter: &str, extra_columns: &str) -> String {
    let generated = if server.at_least(18, 0) {
        "CASE WHEN p.pubgencols OPERATOR(pg_catalog.=) 's' THEN 'stored' ELSE 'none' END"
    } else {
        "NULL::pg_catalog.text"
    };
    let schemas = if server.at_least(15, 0) {
        "(SELECT pg_catalog.count(*) FROM pg_catalog.pg_publication_namespace pn WHERE pn.pnpubid OPERATOR(pg_catalog.=) p.oid)"
    } else {
        "0::bigint"
    };
    format!(
        "SELECT p.oid, pg_catalog.current_database() AS db, p.pubname AS name, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) p.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_publication'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, NULL::pg_catalog.name AS extension, \
         false AS system_schema, p.puballtables AS all_tables, p.pubinsert AS ins, \
         p.pubupdate AS upd, p.pubdelete AS del, p.pubtruncate AS trunc, \
         p.pubviaroot AS via_root, {generated} AS generated_columns, \
         (SELECT pg_catalog.count(*) FROM pg_catalog.pg_publication_rel pr WHERE pr.prpubid OPERATOR(pg_catalog.=) p.oid) AS table_count, \
         {schemas} AS schema_count{extra_columns} \
         FROM pg_catalog.pg_publication p WHERE true{filter}"
    )
}

/// The subscription query. `restricted` reads only the columns the role
/// may read: PostgreSQL 13 also hides `oid` and `subsynccommit` from roles
/// without superuser rights (so the comment cannot be looked up either and
/// `comment_visible` is false); from 14 only `subconninfo` is hidden and the
/// comment is read as for a superuser.
fn subscription_base(
    server: &ServerInfo,
    filter: &str,
    extra_columns: &str,
    restricted: bool,
) -> String {
    // A subscription is never built in: its OID is always a normal one, and
    // the restricted form on 13 uses the first normal OID in its place.
    let (oid, comment) = if restricted && !server.at_least(14, 0) {
        (
            "'16384'::pg_catalog.oid AS oid",
            "NULL::pg_catalog.text AS comment, false AS comment_visible",
        )
    } else {
        (
            "s.oid",
            // pg_subscription is a shared catalog, yet COMMENT ON SUBSCRIPTION
            // stores the comment in the database's pg_description.
            "(SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) s.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_subscription'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, true AS comment_visible",
        )
    };
    format!(
        "SELECT {oid}, pg_catalog.current_database() AS db, s.subname AS name, {comment}, \
         NULL::pg_catalog.name AS extension, false AS system_schema, s.subenabled AS enabled, \
         s.subpublications::pg_catalog.text[] AS publications{extra_columns} \
         FROM pg_catalog.pg_subscription s \
         WHERE s.subdbid OPERATOR(pg_catalog.=) (SELECT d.oid FROM pg_catalog.pg_database d WHERE d.datname OPERATOR(pg_catalog.=) pg_catalog.current_database())\
         {filter}"
    )
}

/// Inner list query (see [`super::common::with_system`]); `restricted`
/// as for subscriptions without superuser rights (see
/// [`describe_subscription`]).
pub(super) fn inner(kind: ObjectKind, server: &ServerInfo, restricted: bool) -> String {
    if kind == ObjectKind::Subscription {
        subscription_base(server, "", "", restricted)
    } else {
        publication_base(server, "", "")
    }
}

/// Order of the list.
pub(super) const ORDER: &str = "q.name";

/// A tree entry from a row of [`inner`].
pub(super) fn summarize(kind: ObjectKind, row: &Row<'_>) -> CatalogResult<ObjectSummary> {
    if kind == ObjectKind::Subscription {
        let flags = if flag(row, "enabled")? {
            Vec::new()
        } else {
            vec!["disabled".to_owned()]
        };
        let detail = Some(texts(row, "publications")?.join(", "));
        return summary(kind, row, false, None, detail, flags);
    }
    let mut operations = Vec::new();
    for (column, word) in [
        ("ins", "insert"),
        ("upd", "update"),
        ("del", "delete"),
        ("trunc", "truncate"),
    ] {
        if flag(row, column)? {
            operations.push(word);
        }
    }
    let detail = Some(if flag(row, "all_tables")? {
        "all tables".to_owned()
    } else {
        let tables = super::common::int(row, "table_count")?;
        let schemas = super::common::int(row, "schema_count")?;
        if schemas > 0 {
            format!("{tables} tables, {schemas} schemas")
        } else {
            format!("{tables} tables")
        }
    });
    let flags = vec![operations.join(", ")];
    summary(kind, row, false, None, detail, flags)
}

/// Details of a publication.
pub(super) async fn describe_publication(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<PgPublication> {
    let name = lit(&object.name)?;
    let main = publication_base(
        server,
        &format!(" AND p.pubname OPERATOR(pg_catalog.=) {name}"),
        ", pg_catalog.pg_get_userbyid(p.pubowner) AS owner",
    );
    let (columns, filter) = if server.at_least(15, 0) {
        (
            "CASE WHEN pr.prattrs IS NULL THEN ARRAY[]::pg_catalog.text[] ELSE ARRAY(\
               SELECT a.attname::pg_catalog.text FROM pg_catalog.unnest(pr.prattrs::pg_catalog.int2[]) WITH ORDINALITY k(attnum, ord) \
               JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) pr.prrelid AND a.attnum OPERATOR(pg_catalog.=) k.attnum \
               ORDER BY k.ord) END",
            "pg_catalog.pg_get_expr(pr.prqual, pr.prrelid)",
        )
    } else {
        ("ARRAY[]::pg_catalog.text[]", "NULL::pg_catalog.text")
    };
    let tables = format!(
        "SELECT n.nspname AS schema, c.relname AS name, {columns} AS columns, \
         {filter} AS row_filter \
         FROM pg_catalog.pg_publication p JOIN pg_catalog.pg_publication_rel pr ON pr.prpubid OPERATOR(pg_catalog.=) p.oid \
         JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) pr.prrelid JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
         WHERE p.pubname OPERATOR(pg_catalog.=) {name} ORDER BY n.nspname, c.relname"
    );
    let schemas = if server.at_least(15, 0) {
        format!(
            "SELECT n.nspname AS schema FROM pg_catalog.pg_publication p \
             JOIN pg_catalog.pg_publication_namespace pn ON pn.pnpubid OPERATOR(pg_catalog.=) p.oid \
             JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) pn.pnnspid WHERE p.pubname OPERATOR(pg_catalog.=) {name} \
             ORDER BY n.nspname"
        )
    } else {
        "SELECT NULL::pg_catalog.name AS schema WHERE false".to_owned()
    };
    let sets = fetch_pinned(
        session,
        &[
            (with_system(&main, true), "q.name".to_owned()),
            (tables, "q.schema, q.name".to_owned()),
            (schemas, "q.schema".to_owned()),
        ],
    )
    .await?;
    let [main_rows, table_rows, schema_rows] = sets.as_slice() else {
        return Err(CatalogError::Unexpected {
            message: "the publication query returned the wrong number of result sets".into(),
        });
    };
    if main_rows.is_empty() {
        return Err(CatalogError::NotFound {
            message: format!("publication {} was not found", object.name),
        });
    }
    let row = main_rows.single("the publication")?;
    let tables = table_rows
        .iter()
        .map(|t| {
            Ok(PgPublicationTable {
                schema: t.req_text("schema")?,
                name: t.req_text("name")?,
                columns: texts(&t, "columns")?,
                row_filter: t.text("row_filter")?,
            })
        })
        .collect::<CatalogResult<Vec<_>>>()?;
    let schemas = schema_rows
        .iter()
        .map(|s| s.req_text("schema"))
        .collect::<CatalogResult<Vec<_>>>()?;
    Ok(PgPublication {
        name: row.req_text("name")?,
        all_tables: flag(&row, "all_tables")?,
        insert: flag(&row, "ins")?,
        update: flag(&row, "upd")?,
        delete: flag(&row, "del")?,
        truncate: flag(&row, "trunc")?,
        via_root: flag(&row, "via_root")?,
        generated_columns: row.text("generated_columns")?,
        tables,
        schemas,
        meta: meta(&row)?,
    })
}

/// `CREATE PUBLICATION` for the server version: on 15+ every table is its
/// own `TABLE` item (column lists, row filters) followed by
/// `TABLES IN SCHEMA`; older servers take one `FOR TABLE` list.
pub(super) fn publication_ddl(
    server: &ServerInfo,
    publication: &PgPublication,
) -> CatalogResult<String> {
    let name = ident(&publication.name)?;
    let mut create = format!("CREATE PUBLICATION {name}");
    let modern = server.at_least(15, 0);
    if publication.all_tables {
        create.push_str(" FOR ALL TABLES");
    } else if !publication.tables.is_empty() || !publication.schemas.is_empty() {
        let mut items = Vec::new();
        for table in &publication.tables {
            let mut item = format!("ONLY {}", qname(&table.schema, &table.name)?);
            if !table.columns.is_empty() {
                let columns = table
                    .columns
                    .iter()
                    .map(|c| ident(c))
                    .collect::<CatalogResult<Vec<_>>>()?;
                item.push_str(&format!(" ({})", columns.join(", ")));
            }
            if let Some(filter) = &table.row_filter {
                item.push_str(&format!(" WHERE ({filter})"));
            }
            items.push(if modern {
                format!("TABLE {item}")
            } else {
                item
            });
        }
        if !modern && !publication.schemas.is_empty() {
            return Err(CatalogError::Unsupported {
                message: "TABLES IN SCHEMA needs PostgreSQL 15 or newer".into(),
            });
        }
        for schema in &publication.schemas {
            items.push(format!("TABLES IN SCHEMA {}", ident(schema)?));
        }
        if modern {
            create.push_str(&format!("\n    FOR {}", items.join(",\n        ")));
        } else {
            create.push_str(&format!("\n    FOR TABLE {}", items.join(", ")));
        }
    }
    let mut publish = Vec::new();
    for (enabled, word) in [
        (publication.insert, "insert"),
        (publication.update, "update"),
        (publication.delete, "delete"),
        (publication.truncate, "truncate"),
    ] {
        if enabled {
            publish.push(word);
        }
    }
    let mut options = vec![
        format!("publish = {}", lit(&publish.join(", "))?),
        format!("publish_via_partition_root = {}", publication.via_root),
    ];
    if let Some(generated) = &publication.generated_columns {
        options.push(format!("publish_generated_columns = {}", ident(generated)?));
    }
    create.push_str(&format!("\n    WITH ({})", options.join(", ")));
    let target = format!("PUBLICATION {name}");
    let mut statements = vec![create];
    statements.extend(owner_to(&target, publication.meta.owner.as_deref())?);
    statements.extend(comment_on(&target, publication.meta.comment.as_deref())?);
    Ok(script(statements))
}

fn subscription_columns(server: &ServerInfo, restricted: bool) -> String {
    let mut columns = vec![
        "pg_catalog.pg_get_userbyid(s.subowner) AS owner".to_owned(),
        "s.subslotname AS slot_name".to_owned(),
        if restricted {
            "NULL::pg_catalog.text AS conninfo".to_owned()
        } else {
            "s.subconninfo AS conninfo".to_owned()
        },
        if restricted && !server.at_least(14, 0) {
            "NULL::pg_catalog.text AS synchronous_commit".to_owned()
        } else {
            "s.subsynccommit AS synchronous_commit".to_owned()
        },
    ];
    let gated = |at: (u32, u32), expr: &str, column: &str, null: &str| {
        if server.at_least(at.0, at.1) {
            format!("{expr} AS {column}")
        } else {
            format!("NULL::pg_catalog.{null} AS {column}")
        }
    };
    columns.push(gated((14, 0), "s.subbinary", "sub_binary", "bool"));
    columns.push(if server.at_least(16, 0) {
        "CASE WHEN s.substream::pg_catalog.text OPERATOR(pg_catalog.=) 't' THEN 'on' WHEN s.substream::pg_catalog.text OPERATOR(pg_catalog.=) 'p' THEN 'parallel' ELSE 'off' END \
         AS streaming"
            .to_owned()
    } else if server.at_least(14, 0) {
        "CASE WHEN s.substream THEN 'on' ELSE 'off' END AS streaming".to_owned()
    } else {
        "NULL::pg_catalog.text AS streaming".to_owned()
    });
    columns.push(gated(
        (15, 0),
        "s.subtwophasestate::pg_catalog.text OPERATOR(pg_catalog.<>) 'd'",
        "two_phase",
        "bool",
    ));
    columns.push(gated(
        (15, 0),
        "s.subdisableonerr",
        "disable_on_error",
        "bool",
    ));
    columns.push(gated(
        (16, 0),
        "s.subpasswordrequired",
        "password_required",
        "bool",
    ));
    columns.push(gated((16, 0), "s.subrunasowner", "run_as_owner", "bool"));
    columns.push(gated((16, 0), "s.suborigin", "origin", "text"));
    columns.push(gated((17, 0), "s.subfailover", "failover", "bool"));
    format!(", {}", columns.join(", "))
}

/// Details of a subscription. Without the privilege to read the
/// connection string, the details come back with `connection_visible`
/// false instead of failing.
pub(super) async fn describe_subscription(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<PgSubscription> {
    let filter = format!(
        " AND s.subname OPERATOR(pg_catalog.=) {}",
        lit(&object.name)?
    );
    let query = |restricted: bool| {
        with_system(
            &subscription_base(
                server,
                &filter,
                &subscription_columns(server, restricted),
                restricted,
            ),
            true,
        )
    };
    let rows = match fetch_one(session, &query(false), "q.name").await {
        Ok(rows) => rows,
        Err(error) if is_permission_denied(&error) => {
            fetch_one(session, &query(true), "q.name").await?
        }
        Err(error) => return Err(error),
    };
    if rows.is_empty() {
        return Err(CatalogError::NotFound {
            message: format!("subscription {} was not found", object.name),
        });
    }
    let row = rows.single("the subscription")?;
    // A connection string that does not parse is not returned in any
    // form: it might hold a password where the parser did not expect one.
    let connection = row.text("conninfo")?.and_then(|c| strip_passwords(&c));
    let connection_visible = connection.is_some();
    Ok(PgSubscription {
        name: row.req_text("name")?,
        enabled: flag(&row, "enabled")?,
        connection,
        connection_visible,
        slot_name: row.text("slot_name")?,
        synchronous_commit: row.text("synchronous_commit")?,
        publications: texts(&row, "publications")?,
        binary: row.bool("sub_binary")?,
        streaming: row.text("streaming")?,
        two_phase: row.bool("two_phase")?,
        disable_on_error: row.bool("disable_on_error")?,
        password_required: row.bool("password_required")?,
        run_as_owner: row.bool("run_as_owner")?,
        origin: row.text("origin")?,
        failover: row.bool("failover")?,
        comment_visible: flag(&row, "comment_visible")?,
        meta: meta(&row)?,
    })
}

/// `CREATE SUBSCRIPTION` the way `pg_dump` writes it: `connect = false`,
/// so it is accepted without reaching the publisher (and without the
/// password, which is never read back). Enable it afterwards.
pub(super) fn subscription_ddl(subscription: &PgSubscription) -> CatalogResult<String> {
    let Some(connection) = &subscription.connection else {
        return Err(CatalogError::Unsupported {
            message: "the connection string of this subscription is not readable \
                      (it needs superuser rights) or not valid, so no DDL can be built"
                .into(),
        });
    };
    let name = ident(&subscription.name)?;
    let publications = subscription
        .publications
        .iter()
        .map(|p| ident(p))
        .collect::<CatalogResult<Vec<_>>>()?;
    let mut options = vec!["connect = false".to_owned()];
    // The default slot is named after the subscription and is left implicit:
    // stating it explicitly would validate it as a slot name, which a
    // subscription name with spaces or capitals does not pass.
    match &subscription.slot_name {
        Some(slot) if *slot == subscription.name => {}
        Some(slot) => options.push(format!("slot_name = {}", lit(slot)?)),
        None => options.push("slot_name = NONE".to_owned()),
    }
    if let Some(commit) = &subscription.synchronous_commit {
        options.push(format!("synchronous_commit = {}", lit(commit)?));
    }
    let booleans = [
        ("binary", subscription.binary),
        ("two_phase", subscription.two_phase),
        ("disable_on_error", subscription.disable_on_error),
        ("password_required", subscription.password_required),
        ("run_as_owner", subscription.run_as_owner),
        ("failover", subscription.failover),
    ];
    for (option, value) in booleans {
        if let Some(value) = value {
            options.push(format!("{option} = {value}"));
        }
    }
    if let Some(streaming) = &subscription.streaming {
        options.push(format!("streaming = {}", lit(streaming)?));
    }
    if let Some(origin) = &subscription.origin {
        options.push(format!("origin = {}", lit(origin)?));
    }
    let mut statements = vec![
        "-- Created without connecting to the publisher and without a password: \
         add the password to the connection, then ALTER SUBSCRIPTION ... ENABLE."
            .to_owned(),
        format!(
            "CREATE SUBSCRIPTION {name}\n    CONNECTION {}\n    PUBLICATION {}\n    WITH ({})",
            lit(connection)?,
            publications.join(", "),
            options.join(", ")
        ),
    ];
    let target = format!("SUBSCRIPTION {name}");
    statements.extend(owner_to(&target, subscription.meta.owner.as_deref())?);
    statements.extend(comment_on(&target, subscription.meta.comment.as_deref())?);
    Ok(script(statements))
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_core::Engine;

    fn server(major: u32) -> ServerInfo {
        ServerInfo {
            engine: Engine::Postgres,
            version: format!("{major}.0"),
            major,
            minor: 0,
            patch: None,
        }
    }

    fn publication() -> PgPublication {
        PgPublication {
            name: "p".into(),
            all_tables: false,
            insert: true,
            update: false,
            delete: true,
            truncate: false,
            via_root: false,
            generated_columns: None,
            tables: vec![
                PgPublicationTable {
                    schema: "s".into(),
                    name: "a".into(),
                    columns: Vec::new(),
                    row_filter: None,
                },
                PgPublicationTable {
                    schema: "s".into(),
                    name: "b".into(),
                    columns: Vec::new(),
                    row_filter: None,
                },
            ],
            schemas: Vec::new(),
            meta: PgMeta::default(),
        }
    }

    #[test]
    fn publications_use_the_syntax_of_the_version() -> CatalogResult<()> {
        assert_eq!(
            publication_ddl(&server(13), &publication())?,
            "CREATE PUBLICATION \"p\"\n    FOR TABLE ONLY \"s\".\"a\", ONLY \"s\".\"b\"\n    \
             WITH (publish = 'insert, delete', publish_via_partition_root = false);\n"
        );
        let mut modern = publication();
        modern.tables[0].columns = vec!["id".into()];
        modern.tables[0].row_filter = Some("(id > 1)".into());
        modern.schemas = vec!["t".into()];
        modern.generated_columns = Some("stored".into());
        let ddl = publication_ddl(&server(18), &modern)?;
        assert!(ddl.contains(
            "FOR TABLE ONLY \"s\".\"a\" (\"id\") WHERE ((id > 1)),\n        \
             TABLE ONLY \"s\".\"b\",\n        TABLES IN SCHEMA \"t\""
        ));
        assert!(ddl.contains("publish_generated_columns = \"stored\""));
        assert!(publication_ddl(&server(13), &modern).is_err());
        Ok(())
    }

    #[test]
    fn subscriptions_without_a_readable_connection_have_no_ddl() {
        let subscription = PgSubscription {
            name: "s".into(),
            enabled: false,
            connection: None,
            connection_visible: false,
            slot_name: None,
            synchronous_commit: Some("off".into()),
            publications: vec!["p".into()],
            binary: None,
            streaming: None,
            two_phase: None,
            disable_on_error: None,
            password_required: None,
            run_as_owner: None,
            origin: None,
            failover: None,
            comment_visible: true,
            meta: PgMeta::default(),
        };
        assert!(matches!(
            subscription_ddl(&subscription),
            Err(CatalogError::Unsupported { .. })
        ));
    }
}
