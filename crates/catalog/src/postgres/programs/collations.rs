//! Collations (`pg_collation`).

use serde::{Deserialize, Serialize};
use varve_driver::{ServerInfo, Session};

use super::Scope;
use super::common::{
    PgMeta, bad_code, comment_on, extension_sql, fetch_one, flag, lit, meta, owner_to, qname,
    schema_of, script, summary, system_schema_sql, with_system,
};
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef, ObjectSummary};
use crate::rows::Row;

/// Collation provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PgCollationProvider {
    /// The database default (the built-in `default` collation).
    Default,
    /// The C library.
    Libc,
    /// ICU.
    Icu,
    /// The built-in provider (17+).
    Builtin,
}

impl PgCollationProvider {
    fn parse(code: &str) -> CatalogResult<Self> {
        match code {
            "d" => Ok(Self::Default),
            "c" => Ok(Self::Libc),
            "i" => Ok(Self::Icu),
            "b" => Ok(Self::Builtin),
            other => Err(bad_code("collprovider", other)),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Libc => "libc",
            Self::Icu => "icu",
            Self::Builtin => "builtin",
        }
    }
}

/// A collation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgCollation {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Provider.
    pub provider: PgCollationProvider,
    /// False for nondeterministic collations.
    pub deterministic: bool,
    /// `LC_COLLATE` of a libc collation.
    pub lc_collate: Option<String>,
    /// `LC_CTYPE` of a libc collation.
    pub lc_ctype: Option<String>,
    /// Locale of an ICU or built-in collation.
    pub locale: Option<String>,
    /// ICU tailoring rules (16+).
    pub icu_rules: Option<String>,
    /// Provider version recorded at creation.
    pub version: Option<String>,
    /// Encoding it applies to; `None` for every encoding.
    pub encoding: Option<String>,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

fn base(server: &ServerInfo, filter: &str, extra: &str) -> String {
    // ICU locales live in collcollate before 15, in colliculocale on 15 and
    // 16, and in colllocale (shared with the built-in provider) from 17.
    let (locale, lc_collate, lc_ctype) = if server.at_least(17, 0) {
        ("c.colllocale", "c.collcollate", "c.collctype")
    } else if server.at_least(15, 0) {
        ("c.colliculocale", "c.collcollate", "c.collctype")
    } else {
        (
            "CASE WHEN c.collprovider OPERATOR(pg_catalog.=) 'i' THEN c.collcollate END",
            "CASE WHEN c.collprovider OPERATOR(pg_catalog.=) 'i' THEN NULL ELSE c.collcollate END",
            "CASE WHEN c.collprovider OPERATOR(pg_catalog.=) 'i' THEN NULL ELSE c.collctype END",
        )
    };
    let rules = if server.at_least(16, 0) {
        "c.collicurules"
    } else {
        "NULL::pg_catalog.text"
    };
    format!(
        "SELECT c.oid, pg_catalog.current_database() AS db, n.nspname AS schema, c.collname AS name, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) c.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_collation'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {ext} AS extension, \
         {sys} AS system_schema, c.collprovider::pg_catalog.text AS provider, \
         c.collisdeterministic AS deterministic, (CASE WHEN {locale} OPERATOR(pg_catalog.<>) '' THEN {locale} END) AS locale, \
         (CASE WHEN {lc_collate} OPERATOR(pg_catalog.<>) '' THEN {lc_collate} END) AS lc_collate, (CASE WHEN {lc_ctype} OPERATOR(pg_catalog.<>) '' THEN {lc_ctype} END) AS lc_ctype, \
         {rules} AS icu_rules{extra} \
         FROM pg_catalog.pg_collation c JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.collnamespace \
         WHERE (c.collencoding OPERATOR(pg_catalog.=) -1 OR c.collencoding OPERATOR(pg_catalog.=) (SELECT d.encoding \
         FROM pg_catalog.pg_database d WHERE d.datname OPERATOR(pg_catalog.=) pg_catalog.current_database())){filter}",
        ext = extension_sql("pg_catalog.pg_collation", "c.oid"),
        sys = system_schema_sql("n.nspname"),
    )
}

/// Inner list query (see [`super::common::with_system`]).
pub(super) fn inner(server: &ServerInfo, scope: &Scope) -> String {
    base(server, &scope.schema_filter("n.nspname"), "")
}

/// Order of the list.
pub(super) const ORDER: &str = "q.name";

/// A tree entry from a row of [`inner`].
pub(super) fn summarize(row: &Row<'_>) -> CatalogResult<ObjectSummary> {
    let provider = PgCollationProvider::parse(&row.req_text("provider")?)?;
    let locale = row
        .text("locale")?
        .or(row.text("lc_collate")?)
        .unwrap_or_default();
    let detail = Some(
        format!("{} {locale}", provider.name())
            .trim_end()
            .to_owned(),
    );
    let mut flags = Vec::new();
    if !flag(row, "deterministic")? {
        flags.push("nondeterministic".to_owned());
    }
    summary(ObjectKind::Collation, row, true, None, detail, flags)
}

/// Details of a collation.
pub(super) async fn describe(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<PgCollation> {
    let filter = format!(
        " AND n.nspname OPERATOR(pg_catalog.=) {} AND c.collname OPERATOR(pg_catalog.=) {}",
        lit(schema_of(object)?)?,
        lit(&object.name)?
    );
    let main = base(
        server,
        &filter,
        ", pg_catalog.pg_get_userbyid(c.collowner) AS owner, c.collversion AS version, \
         CASE WHEN c.collencoding OPERATOR(pg_catalog.=) -1 THEN NULL ELSE pg_catalog.pg_encoding_to_char(c.collencoding) END \
         AS encoding",
    );
    let rows = fetch_one(session, &with_system(&main, true), "q.name").await?;
    if rows.is_empty() {
        return Err(CatalogError::NotFound {
            message: format!("collation {} was not found", object.name),
        });
    }
    let row = rows.single("the collation")?;
    Ok(PgCollation {
        schema: row.req_text("schema")?,
        name: row.req_text("name")?,
        provider: PgCollationProvider::parse(&row.req_text("provider")?)?,
        deterministic: flag(&row, "deterministic")?,
        lc_collate: row.text("lc_collate")?,
        lc_ctype: row.text("lc_ctype")?,
        locale: row.text("locale")?,
        icu_rules: row.text("icu_rules")?,
        version: row.text("version")?,
        encoding: row.text("encoding")?,
        meta: meta(&row)?,
    })
}

/// `CREATE COLLATION` with owner and comment.
pub(super) fn ddl(collation: &PgCollation) -> CatalogResult<String> {
    let name = qname(&collation.schema, &collation.name)?;
    let mut options = vec![format!("provider = {}", collation.provider.name())];
    match collation.provider {
        PgCollationProvider::Default => {
            return Err(CatalogError::Unsupported {
                message: "the default collation is built in and has no DDL".into(),
            });
        }
        PgCollationProvider::Libc => {
            let collate = collation.lc_collate.as_deref().unwrap_or_default();
            let ctype = collation.lc_ctype.as_deref().unwrap_or(collate);
            options.push(format!("lc_collate = {}", lit(collate)?));
            options.push(format!("lc_ctype = {}", lit(ctype)?));
        }
        PgCollationProvider::Icu | PgCollationProvider::Builtin => {
            options.push(format!(
                "locale = {}",
                lit(collation.locale.as_deref().unwrap_or_default())?
            ));
        }
    }
    if !collation.deterministic {
        options.push("deterministic = false".to_owned());
    }
    if let Some(rules) = &collation.icu_rules {
        options.push(format!("rules = {}", lit(rules)?));
    }
    let target = format!("COLLATION {name}");
    let mut statements = vec![format!("CREATE {target} ({})", options.join(", "))];
    statements.extend(owner_to(&target, collation.meta.owner.as_deref())?);
    statements.extend(comment_on(&target, collation.meta.comment.as_deref())?);
    Ok(script(statements))
}
