//! Installed extensions (`pg_extension`) with update information from
//! `pg_available_extensions` and `pg_extension_update_paths`.

use serde::{Deserialize, Serialize};
use varve_driver::Session;

use super::common::{
    PgMeta, comment_on, fetch_one, flag, ident, lit, meta, script, summary, texts, with_system,
};
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef, ObjectSummary};
use crate::rows::Row;

/// An installed extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgExtension {
    /// Name.
    pub name: String,
    /// Schema holding its objects.
    pub schema: String,
    /// Installed version.
    pub version: String,
    /// True when it can be moved to another schema.
    pub relocatable: bool,
    /// Version a plain `CREATE EXTENSION` installs, from the control file;
    /// `None` when the extension's files are no longer on the server.
    pub default_version: Option<String>,
    /// Versions reachable from the installed one with
    /// `ALTER EXTENSION ... UPDATE TO`.
    pub available_updates: Vec<String>,
    /// True when the default version is newer and reachable.
    pub update_available: bool,
    /// Description from the control file.
    pub description: Option<String>,
    /// Extensions this one requires.
    pub requires: Vec<String>,
    /// Configuration tables whose data `pg_dump` includes.
    pub config_tables: Vec<String>,
    /// Objects owned by the extension (`pg_describe_object`). Lists of
    /// other kinds mark these objects as system objects.
    pub members: Vec<String>,
    /// Owner, comment, system status. The owner is the installing role.
    pub meta: PgMeta,
}

fn base(filter: &str, extra_columns: &str) -> String {
    format!(
        "SELECT e.oid, pg_catalog.current_database() AS db, e.extname AS name, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) e.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, NULL::pg_catalog.name AS extension, \
         false AS system_schema, n.nspname AS ext_schema, e.extversion AS version, \
         a.default_version, \
         CASE WHEN a.name IS NULL OR a.default_version OPERATOR(pg_catalog.=) e.extversion THEN false \
         ELSE EXISTS (SELECT 1 FROM pg_catalog.pg_extension_update_paths(e.extname) u \
         WHERE u.source OPERATOR(pg_catalog.=) e.extversion AND u.target OPERATOR(pg_catalog.=) a.default_version \
         AND u.path IS NOT NULL) END AS update_available{extra_columns} \
         FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) e.extnamespace \
         LEFT JOIN pg_catalog.pg_available_extensions a ON a.name OPERATOR(pg_catalog.=) e.extname WHERE true{filter}"
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
    let mut flags = Vec::new();
    if flag(row, "update_available")? {
        flags.push("update available".to_owned());
    }
    let detail = Some(format!(
        "{} in {}",
        row.req_text("version")?,
        row.req_text("ext_schema")?
    ));
    summary(ObjectKind::Extension, row, false, None, detail, flags)
}

/// Details of an installed extension.
pub(super) async fn describe(
    session: &dyn Session,
    object: &ObjectRef,
) -> CatalogResult<PgExtension> {
    let main = base(
        &format!(
            " AND e.extname OPERATOR(pg_catalog.=) {}",
            lit(&object.name)?
        ),
        ", pg_catalog.pg_get_userbyid(e.extowner) AS owner, e.extrelocatable AS relocatable, \
         a.comment AS description, \
         CASE WHEN a.name IS NULL THEN ARRAY[]::pg_catalog.text[] ELSE ARRAY(\
           SELECT u.target FROM pg_catalog.pg_extension_update_paths(e.extname) u \
           WHERE u.source OPERATOR(pg_catalog.=) e.extversion AND u.path IS NOT NULL ORDER BY u.target) END \
         AS available_updates, \
         ARRAY(SELECT r.extname::pg_catalog.text FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_extension r ON r.oid OPERATOR(pg_catalog.=) d.refobjid \
           WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass AND d.objid OPERATOR(pg_catalog.=) e.oid \
           AND d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass AND d.deptype OPERATOR(pg_catalog.=) 'n' ORDER BY 1) AS requires, \
         ARRAY(SELECT c::pg_catalog.regclass::pg_catalog.text FROM pg_catalog.unnest(e.extconfig) c) AS config_tables, \
         ARRAY(SELECT pg_catalog.pg_describe_object(d.classid, d.objid, 0) FROM pg_catalog.pg_depend d \
           WHERE d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass AND d.refobjid OPERATOR(pg_catalog.=) e.oid \
           AND d.deptype OPERATOR(pg_catalog.=) 'e' ORDER BY 1) AS members",
    );
    let rows = fetch_one(session, &with_system(&main, true), "q.name").await?;
    if rows.is_empty() {
        return Err(CatalogError::NotFound {
            message: format!("extension {} was not found", object.name),
        });
    }
    let row = rows.single("the extension")?;
    Ok(PgExtension {
        name: row.req_text("name")?,
        schema: row.req_text("ext_schema")?,
        version: row.req_text("version")?,
        relocatable: flag(&row, "relocatable")?,
        default_version: row.text("default_version")?,
        available_updates: texts(&row, "available_updates")?,
        update_available: flag(&row, "update_available")?,
        description: row.text("description")?,
        requires: texts(&row, "requires")?,
        config_tables: texts(&row, "config_tables")?,
        members: texts(&row, "members")?,
        meta: meta(&row)?,
    })
}

/// `CREATE EXTENSION` at the installed version and schema, with the comment.
pub(super) fn ddl(extension: &PgExtension) -> CatalogResult<String> {
    let name = ident(&extension.name)?;
    let mut statements = vec![format!(
        "CREATE EXTENSION {name} WITH SCHEMA {} VERSION {}",
        ident(&extension.schema)?,
        lit(&extension.version)?
    )];
    statements.extend(comment_on(
        &format!("EXTENSION {name}"),
        extension.meta.comment.as_deref(),
    )?);
    Ok(script(statements))
}

/// An extension the server can install (`pg_available_extensions`), with
/// the versions its control and script files offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgAvailableExtension {
    /// Name.
    pub name: String,
    /// Version a plain `CREATE EXTENSION` installs; `None` when the control
    /// file names none.
    pub default_version: Option<String>,
    /// Every version that can be installed, oldest first (numeric parts
    /// compared as numbers).
    pub versions: Vec<String>,
    /// Installed version in the current database; `None` when it is not
    /// installed there.
    pub installed_version: Option<String>,
    /// Schema of the installed extension; `None` when it is not installed.
    pub installed_schema: Option<String>,
    /// Description from the control file.
    pub comment: Option<String>,
    /// True when the default version can be moved to another schema after
    /// installation.
    pub relocatable: bool,
    /// The schema the default version must be installed in, when its control
    /// file fixes one (a schema choice is then refused by the server).
    pub fixed_schema: Option<String>,
    /// True when only a superuser can install the default version (unless it
    /// is `trusted`).
    pub superuser: bool,
    /// True when a non-superuser with `CREATE` on the database can install
    /// the default version.
    pub trusted: bool,
    /// Extensions the default version requires (`CASCADE` installs the
    /// missing ones).
    pub requires: Vec<String>,
    /// What each installable version's control file says, in the order of
    /// `versions`: a version other than the default may need other
    /// extensions, rights or a fixed schema.
    pub version_details: Vec<PgExtensionVersion>,
}

/// One installable version of an extension
/// (`pg_available_extension_versions`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgExtensionVersion {
    /// Version.
    pub version: String,
    /// True when only a superuser can install it (unless `trusted`).
    pub superuser: bool,
    /// True when a non-superuser with `CREATE` on the database can install it.
    pub trusted: bool,
    /// True when it can be moved to another schema after installation.
    pub relocatable: bool,
    /// The schema its control file fixes, if any.
    pub fixed_schema: Option<String>,
    /// Extensions it requires.
    pub requires: Vec<String>,
}

/// One element of the `version_details` JSON array the query builds.
#[derive(Deserialize)]
struct VersionRow {
    version: String,
    superuser: bool,
    trusted: bool,
    relocatable: bool,
    schema: Option<String>,
    requires: Option<Vec<String>>,
}

/// One statement: every available extension with its versions, joined to
/// the installed ones. `pg_available_extension_versions` reads every control
/// file on each call, so it is read once (materialized) and grouped.
fn available_sql() -> String {
    "WITH v AS MATERIALIZED (SELECT x.name, x.version, x.superuser, x.trusted, x.relocatable, \
       x.schema, x.requires FROM pg_catalog.pg_available_extension_versions x), \
     vs AS (SELECT v.name, pg_catalog.json_agg(pg_catalog.json_build_object('version', v.version, \
       'superuser', v.superuser, 'trusted', v.trusted, 'relocatable', v.relocatable, 'schema', v.schema, \
       'requires', v.requires))::pg_catalog.text AS version_details FROM v GROUP BY v.name) \
     SELECT a.name::pg_catalog.text AS name, a.default_version, a.comment, \
       e.extversion AS installed_version, n.nspname::pg_catalog.text AS installed_schema, \
       COALESCE(vs.version_details, '[]') AS version_details, \
       COALESCE(d.relocatable, false) AS relocatable, d.schema::pg_catalog.text AS fixed_schema, \
       COALESCE(d.superuser, true) AS superuser, COALESCE(d.trusted, false) AS trusted, \
       COALESCE(d.requires::pg_catalog.text[], ARRAY[]::pg_catalog.text[]) AS requires \
     FROM pg_catalog.pg_available_extensions a \
     LEFT JOIN vs ON vs.name OPERATOR(pg_catalog.=) a.name \
     LEFT JOIN v d ON d.name OPERATOR(pg_catalog.=) a.name AND d.version OPERATOR(pg_catalog.=) a.default_version \
     LEFT JOIN pg_catalog.pg_extension e ON e.extname OPERATOR(pg_catalog.=) a.name \
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) e.extnamespace"
        .to_owned()
}

/// Orders extension versions: dot or dash separated parts, numeric parts
/// compared as numbers and before text parts (`1.2` < `1.10` < `1.10beta`).
fn version_order(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let parts = |s: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut current = String::new();
        let mut digits = None;
        for c in s.chars() {
            if c == '.' || c == '-' || c == '_' {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                digits = None;
                continue;
            }
            let is_digit = c.is_ascii_digit();
            if digits.is_some_and(|d| d != is_digit) && !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            digits = Some(is_digit);
            current.push(c);
        }
        if !current.is_empty() {
            out.push(current);
        }
        out
    };
    let (pa, pb) = (parts(a), parts(b));
    for (x, y) in pa.iter().zip(pb.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(m), Ok(n)) => m.cmp(&n),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    pa.len().cmp(&pb.len()).then_with(|| a.cmp(b))
}

fn version_details(row: &Row<'_>) -> CatalogResult<Vec<PgExtensionVersion>> {
    parse_version_details(&row.req_text("version_details")?)
}

/// The `version_details` JSON array, sorted by version.
fn parse_version_details(text: &str) -> CatalogResult<Vec<PgExtensionVersion>> {
    let rows: Vec<VersionRow> =
        serde_json::from_str(text).map_err(|e| CatalogError::Unexpected {
            message: format!("the extension versions could not be read: {e}"),
        })?;
    let mut details: Vec<PgExtensionVersion> = rows
        .into_iter()
        .map(|r| PgExtensionVersion {
            version: r.version,
            superuser: r.superuser,
            trusted: r.trusted,
            relocatable: r.relocatable,
            fixed_schema: r.schema,
            requires: r.requires.unwrap_or_default(),
        })
        .collect();
    details.sort_by(|a, b| version_order(&a.version, &b.version));
    Ok(details)
}

fn available_row(row: &Row<'_>) -> CatalogResult<PgAvailableExtension> {
    let version_details = version_details(row)?;
    Ok(PgAvailableExtension {
        name: row.req_text("name")?,
        default_version: row.text("default_version")?,
        versions: version_details.iter().map(|v| v.version.clone()).collect(),
        installed_version: row.text("installed_version")?,
        installed_schema: row.text("installed_schema")?,
        comment: row.text("comment")?,
        relocatable: flag(row, "relocatable")?,
        fixed_schema: row.text("fixed_schema")?,
        superuser: flag(row, "superuser")?,
        trusted: flag(row, "trusted")?,
        requires: texts(row, "requires")?,
        version_details,
    })
}

/// Every extension the server can install, by name, with its versions and
/// whether (and where) it is installed in the session's database.
///
/// # Errors
///
/// [`CatalogError::Invalid`] inside a transaction block (as every catalog
/// call), driver errors.
pub(super) async fn available(session: &dyn Session) -> CatalogResult<Vec<PgAvailableExtension>> {
    let rows = fetch_one(session, &available_sql(), "q.name").await?;
    rows.iter().map(|row| available_row(&row)).collect()
}

#[cfg(test)]
mod available_tests {
    use super::{available_sql, parse_version_details, version_order};
    use crate::model::CatalogResult;
    use std::cmp::Ordering;

    #[test]
    fn each_version_keeps_its_own_control_file() -> CatalogResult<()> {
        let details = parse_version_details(
            r#"[{"version": "2.0", "superuser": false, "trusted": true, "relocatable": false, "schema": "ext", "requires": ["cube", "plpgsql"]},
                {"version": "1.10", "superuser": true, "trusted": false, "relocatable": true, "schema": null, "requires": null}]"#,
        )?;
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].version, "1.10");
        assert!(details[0].superuser && details[0].relocatable && details[0].requires.is_empty());
        assert_eq!(details[0].fixed_schema, None);
        assert_eq!(details[1].fixed_schema.as_deref(), Some("ext"));
        assert_eq!(
            details[1].requires,
            vec!["cube".to_owned(), "plpgsql".to_owned()]
        );
        assert!(parse_version_details("not json").is_err());
        Ok(())
    }

    #[test]
    fn versions_sort_numerically() {
        let mut v = vec!["1.10", "1.2", "1.10beta", "2.0", "1.2.1", "1.0"];
        v.sort_by(|a, b| version_order(a, b));
        assert_eq!(v, vec!["1.0", "1.2", "1.2.1", "1.10", "1.10beta", "2.0"]);
        assert_eq!(version_order("1.1", "1.1"), Ordering::Equal);
        assert_eq!(version_order("unpackaged", "1.0"), Ordering::Greater);
    }

    #[test]
    fn available_reads_the_control_files_once() {
        let sql = available_sql();
        assert_eq!(sql.matches("pg_available_extension_versions").count(), 1);
        assert!(sql.contains("MATERIALIZED"));
    }
}
