//! Catalog queries. Every loader reads all matching objects with one query,
//! so lists never issue a query per object; `describe` uses the same
//! loaders with a narrower filter.

use varve_driver::{ServerInfo, Session};

use crate::model::{CatalogResult, ObjectKind};
use crate::rows::Row;

use super::support::{
    Pin, fetch, flag, int, is_lock_timeout, json, lit, non_empty, opt_int, opt_text, options_json,
    pretty_bytes, strip_semicolon, system_relation, system_schema, text, text_list,
    trigger_condition,
};
use crate::postgres::programs::{fdw_extension_sql, redact_option};

use super::types::{
    PgColumn, PgConstraint, PgDatabase, PgFdwOption, PgForeignInfo, PgForeignKey, PgGenerated,
    PgIdentity, PgIndex, PgIndexKey, PgOption, PgOwnedBy, PgPartitionOf, PgPolicy, PgQualifiedName,
    PgRule, PgSchema, PgSequence, PgSetting, PgTablespace, PgTrigger,
};

/// Which relations (or children of which relation) a loader reads.
#[derive(Debug, Clone, Copy)]
pub(super) enum Filter<'a> {
    /// Everything in a schema.
    Schema(&'a str),
    /// One relation, or the children of one relation.
    Relation {
        /// Schema of the relation.
        schema: &'a str,
        /// The relation.
        name: &'a str,
    },
    /// One child of one relation.
    Child {
        /// Schema of the relation.
        schema: &'a str,
        /// The relation.
        table: &'a str,
        /// The child.
        name: &'a str,
    },
    /// The partitions of a partitioned table.
    PartitionsOf {
        /// Schema of the partitioned table.
        schema: &'a str,
        /// The partitioned table.
        name: &'a str,
    },
}

impl Filter<'_> {
    /// SQL condition on the relation `c` in namespace `n`, plus the child
    /// name column for [`Filter::Child`].
    fn sql(&self, c: &str, n: &str, child: Option<&str>) -> CatalogResult<String> {
        Ok(match self {
            Filter::Schema(schema) => {
                format!("{n}.nspname OPERATOR(pg_catalog.=) {}", lit(schema)?)
            }
            Filter::Relation { schema, name } => format!(
                "{n}.nspname OPERATOR(pg_catalog.=) {} AND {c}.relname OPERATOR(pg_catalog.=) {}",
                lit(schema)?,
                lit(name)?
            ),
            Filter::Child {
                schema,
                table,
                name,
            } => {
                let mut sql = format!(
                    "{n}.nspname OPERATOR(pg_catalog.=) {} AND {c}.relname OPERATOR(pg_catalog.=) {}",
                    lit(schema)?,
                    lit(table)?
                );
                if let Some(column) = child {
                    sql.push_str(&format!(
                        " AND {column} OPERATOR(pg_catalog.=) {}",
                        lit(name)?
                    ));
                }
                sql
            }
            Filter::PartitionsOf { schema, name } => format!(
                "{c}.relispartition AND EXISTS (SELECT 1 FROM pg_catalog.pg_inherits fh \
                 JOIN pg_catalog.pg_class fp ON fp.oid OPERATOR(pg_catalog.=) fh.inhparent \
                 JOIN pg_catalog.pg_namespace fn ON fn.oid OPERATOR(pg_catalog.=) fp.relnamespace \
                 WHERE fh.inhrelid OPERATOR(pg_catalog.=) {c}.oid AND fn.nspname OPERATOR(pg_catalog.=) {} AND fp.relname OPERATOR(pg_catalog.=) {})",
                lit(schema)?,
                lit(name)?
            ),
        })
    }
}

fn system_filter(include_system: bool, expr: &str) -> String {
    if include_system {
        String::new()
    } else {
        format!(" AND NOT {expr}")
    }
}

// ---------------------------------------------------------------------------
// Server-wide objects and schemas

/// Every database on the server.
pub(super) async fn databases(
    session: &dyn Session,
    pin: Pin,
    server: &ServerInfo,
    name: Option<&str>,
) -> CatalogResult<Vec<PgDatabase>> {
    let provider = if server.at_least(15, 0) {
        "d.datlocprovider::pg_catalog.text"
    } else {
        "'c'::pg_catalog.text"
    };
    let locale = if server.at_least(17, 0) {
        "d.datlocale"
    } else if server.at_least(15, 0) {
        "d.daticulocale"
    } else {
        "NULL::pg_catalog.text"
    };
    let rules = if server.at_least(16, 0) {
        "d.daticurules"
    } else {
        "NULL::pg_catalog.text"
    };
    let version = if server.at_least(15, 0) {
        "d.datcollversion"
    } else {
        "NULL::pg_catalog.text"
    };
    let filter = match name {
        Some(name) => format!("WHERE d.datname OPERATOR(pg_catalog.=) {}", lit(name)?),
        None => String::new(),
    };
    let sql = format!(
        "SELECT d.datname AS name, pg_catalog.pg_get_userbyid(d.datdba) AS owner, \
           pg_catalog.pg_encoding_to_char(d.encoding) AS encoding, \
           d.datcollate AS collation, d.datctype AS ctype, {provider} AS provider, \
           {locale} AS locale, {rules} AS icu_rules, {version} AS collation_version, \
           d.datistemplate AS is_template, d.datallowconn AS allow_connections, \
           d.datconnlimit::pg_catalog.int8 AS connection_limit, ts.spcname AS tablespace, \
           CASE WHEN pg_catalog.has_database_privilege(d.oid, 'CONNECT') \
             THEN pg_catalog.pg_database_size(d.oid) END AS size_bytes, \
           (SELECT pg_catalog.json_agg(pg_catalog.json_build_object( \
              'role', CASE WHEN s.setrole OPERATOR(pg_catalog.=) 0 THEN NULL ELSE pg_catalog.pg_get_userbyid(s.setrole) END, \
              'entry', cfg.entry) ORDER BY s.setrole, cfg.ord) \
            FROM pg_catalog.pg_db_role_setting s \
            CROSS JOIN LATERAL pg_catalog.unnest(s.setconfig) WITH ORDINALITY cfg(entry, ord) \
            WHERE s.setdatabase OPERATOR(pg_catalog.=) d.oid) AS settings, \
           (SELECT cmt.description FROM pg_catalog.pg_shdescription cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) d.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_database'::pg_catalog.regclass) AS comment, \
           d.datname OPERATOR(pg_catalog.=) ANY (ARRAY['template0', 'template1']::pg_catalog.name[]) AS system \
         FROM pg_catalog.pg_database d \
         JOIN pg_catalog.pg_tablespace ts ON ts.oid OPERATOR(pg_catalog.=) d.dattablespace \
         {filter} ORDER BY d.datname"
    );
    let rows = fetch(session, pin, &sql, "q.name").await?;
    rows.iter().map(|row| database_row(&row)).collect()
}

#[derive(serde::Deserialize, Default)]
struct RawSetting {
    role: Option<String>,
    entry: String,
}

fn database_row(row: &Row<'_>) -> CatalogResult<PgDatabase> {
    let raw: Vec<RawSetting> = json(row, "settings")?;
    let settings = raw
        .into_iter()
        .map(|s| {
            let (name, value) = s.entry.split_once('=').unwrap_or((s.entry.as_str(), ""));
            PgSetting {
                role: s.role,
                name: name.to_owned(),
                value: value.to_owned(),
            }
        })
        .collect();
    Ok(PgDatabase {
        name: text(row, "name")?,
        owner: text(row, "owner")?,
        encoding: text(row, "encoding")?,
        collation: text(row, "collation")?,
        ctype: text(row, "ctype")?,
        locale_provider: match text(row, "provider")?.as_str() {
            "i" => "icu",
            "b" => "builtin",
            _ => "libc",
        }
        .to_owned(),
        locale: non_empty(row, "locale")?,
        icu_rules: non_empty(row, "icu_rules")?,
        collation_version: non_empty(row, "collation_version")?,
        is_template: flag(row, "is_template")?,
        allow_connections: flag(row, "allow_connections")?,
        connection_limit: int(row, "connection_limit")?,
        tablespace: text(row, "tablespace")?,
        size_bytes: opt_int(row, "size_bytes")?,
        settings,
        comment: opt_text(row, "comment")?,
        system: flag(row, "system")?,
    })
}

/// Schemas of the session's database.
pub(super) async fn schemas(
    session: &dyn Session,
    pin: Pin,
    name: Option<&str>,
    include_system: bool,
) -> CatalogResult<Vec<PgSchema>> {
    let system = system_schema("n");
    let mut filter = match name {
        Some(name) => format!("WHERE n.nspname OPERATOR(pg_catalog.=) {}", lit(name)?),
        None => "WHERE true".to_owned(),
    };
    filter.push_str(&system_filter(include_system, &system));
    let sql = format!(
        "SELECT n.nspname AS name, pg_catalog.pg_get_userbyid(n.nspowner) AS owner, \
           (SELECT e.extname FROM pg_catalog.pg_depend d \
              JOIN pg_catalog.pg_extension e ON e.oid OPERATOR(pg_catalog.=) d.refobjid \
            WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_namespace'::pg_catalog.regclass \
              AND d.objid OPERATOR(pg_catalog.=) n.oid AND d.deptype OPERATOR(pg_catalog.=) 'e' LIMIT 1) AS extension, \
           (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) n.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_namespace'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {system} AS system \
         FROM pg_catalog.pg_namespace n {filter} ORDER BY n.nspname"
    );
    let rows = fetch(session, pin, &sql, "q.name").await?;
    rows.iter()
        .map(|row| {
            Ok(PgSchema {
                name: text(&row, "name")?,
                owner: text(&row, "owner")?,
                extension: opt_text(&row, "extension")?,
                comment: opt_text(&row, "comment")?,
                system: flag(&row, "system")?,
            })
        })
        .collect()
}

/// Tablespaces of the server.
pub(super) async fn tablespaces(
    session: &dyn Session,
    pin: Pin,
    name: Option<&str>,
    include_system: bool,
) -> CatalogResult<Vec<PgTablespace>> {
    let system = "t.spcname OPERATOR(pg_catalog.=) ANY (ARRAY['pg_default', 'pg_global']::pg_catalog.name[])";
    let mut filter = match name {
        Some(name) => format!("WHERE t.spcname OPERATOR(pg_catalog.=) {}", lit(name)?),
        None => "WHERE true".to_owned(),
    };
    filter.push_str(&system_filter(include_system, system));
    let options = options_json("t.spcoptions", "");
    let sql = format!(
        "SELECT t.spcname AS name, pg_catalog.pg_get_userbyid(t.spcowner) AS owner, \
           pg_catalog.pg_tablespace_location(t.oid) AS location, {options} AS options, \
           CASE WHEN pg_catalog.has_tablespace_privilege(t.oid, 'CREATE') \
                  OR pg_catalog.pg_has_role('pg_read_all_stats', 'MEMBER') \
                  OR t.oid OPERATOR(pg_catalog.=) (SELECT dattablespace FROM pg_catalog.pg_database \
                              WHERE datname OPERATOR(pg_catalog.=) pg_catalog.current_database()) \
             THEN pg_catalog.pg_tablespace_size(t.oid) END AS size_bytes, \
           (SELECT cmt.description FROM pg_catalog.pg_shdescription cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) t.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_tablespace'::pg_catalog.regclass) AS comment, {system} AS system \
         FROM pg_catalog.pg_tablespace t {filter} ORDER BY t.spcname"
    );
    let rows = fetch(session, pin, &sql, "q.name").await?;
    rows.iter()
        .map(|row| {
            Ok(PgTablespace {
                name: text(&row, "name")?,
                owner: text(&row, "owner")?,
                location: non_empty(&row, "location")?,
                options: json(&row, "options")?,
                size_bytes: opt_int(&row, "size_bytes")?,
                comment: opt_text(&row, "comment")?,
                system: flag(&row, "system")?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Relations

/// The relation-level facts of a table, view, materialized view or
/// foreign table, without its children.
#[derive(Debug, Clone)]
pub(super) struct RelRow {
    pub(super) schema: String,
    pub(super) name: String,
    /// `r`, `p`, `v`, `m` or `f`.
    pub(super) relkind: String,
    pub(super) is_partition: bool,
    pub(super) owner: String,
    pub(super) unlogged: bool,
    pub(super) of_type: Option<String>,
    pub(super) access_method: Option<String>,
    pub(super) tablespace: Option<String>,
    pub(super) options: Vec<PgOption>,
    pub(super) toast_options: Vec<PgOption>,
    pub(super) inherits: Vec<PgQualifiedName>,
    pub(super) part_strategy: Option<String>,
    pub(super) part_key: Option<String>,
    pub(super) default_partition: Option<PgQualifiedName>,
    pub(super) partition_of: Option<PgPartitionOf>,
    pub(super) foreign: Option<PgForeignInfo>,
    pub(super) row_estimate: Option<i64>,
    /// Estimated total size from `pg_class.relpages` (no lock taken).
    pub(super) total_bytes: Option<i64>,
    pub(super) rls_enabled: bool,
    pub(super) rls_forced: bool,
    pub(super) replica_identity: String,
    pub(super) populated: bool,
    pub(super) definition: Option<String>,
    pub(super) comment: Option<String>,
    pub(super) system: bool,
    pub(super) child_count: i64,
    pub(super) has_plain_unique_index: bool,
}

/// Which relations [`relations`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RelKinds {
    /// Tables, partitioned tables, foreign tables that are not partitions.
    TopLevelTables,
    /// Partitions (any relkind).
    Partitions,
    /// Views.
    Views,
    /// Materialized views.
    MaterializedViews,
    /// Every relation kind.
    Any,
}

/// The block size, for size estimates from `relpages`.
const BLOCK: &str = "pg_catalog.current_setting('block_size')::pg_catalog.int8";

/// Pages of the relation `c` and its TOAST table and TOAST index, from
/// `pg_class` (as of the last `VACUUM`, `ANALYZE` or `CREATE INDEX`). Size
/// functions such as `pg_table_size` take a lock and wait behind a session
/// holding the table in `ACCESS EXCLUSIVE` mode; `pg_class` never waits.
const TABLE_PAGES: &str = "(c.relpages::pg_catalog.int8 OPERATOR(pg_catalog.+) COALESCE((SELECT tc.relpages::pg_catalog.int8 OPERATOR(pg_catalog.+) COALESCE(( \
     SELECT pg_catalog.sum(ti.relpages)::pg_catalog.int8 FROM pg_catalog.pg_index tx \
     JOIN pg_catalog.pg_class ti ON ti.oid OPERATOR(pg_catalog.=) tx.indexrelid WHERE tx.indrelid OPERATOR(pg_catalog.=) tc.oid), 0) \
     FROM pg_catalog.pg_class tc WHERE tc.oid OPERATOR(pg_catalog.=) c.reltoastrelid), 0))";

/// Pages of the indexes of the relation `c`, from `pg_class`.
const INDEX_PAGES: &str = "COALESCE((SELECT pg_catalog.sum(xi.relpages)::pg_catalog.int8 FROM pg_catalog.pg_index xx \
     JOIN pg_catalog.pg_class xi ON xi.oid OPERATOR(pg_catalog.=) xx.indexrelid WHERE xx.indrelid OPERATOR(pg_catalog.=) c.oid), 0)";

/// Exact sizes of one relation, or why they are not available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sizes {
    /// Sizes in bytes: table with TOAST, indexes, total.
    Exact {
        /// `pg_table_size`.
        table: i64,
        /// `pg_indexes_size`.
        indexes: i64,
        /// `pg_total_relation_size`.
        total: i64,
    },
    /// Another session holds a lock that the size functions would wait
    /// for.
    Locked,
}

/// Exact sizes of a relation or index, waiting at most a moment for its
/// lock.
pub(super) async fn exact_sizes(
    session: &dyn Session,
    schema: &str,
    name: &str,
) -> CatalogResult<Sizes> {
    let sql = format!(
        "SELECT pg_catalog.pg_table_size(c.oid) AS table_bytes, \
           pg_catalog.pg_indexes_size(c.oid) AS index_bytes, \
           pg_catalog.pg_total_relation_size(c.oid) AS total_bytes \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
         WHERE n.nspname OPERATOR(pg_catalog.=) {} AND c.relname OPERATOR(pg_catalog.=) {}",
        lit(schema)?,
        lit(name)?
    );
    match fetch(session, Pin::Sizes, &sql, "1").await {
        Ok(rows) => {
            let row = rows.single("the relation")?;
            Ok(Sizes::Exact {
                table: opt_int(&row, "table_bytes")?.unwrap_or(0),
                indexes: opt_int(&row, "index_bytes")?.unwrap_or(0),
                total: opt_int(&row, "total_bytes")?.unwrap_or(0),
            })
        }
        Err(error) if is_lock_timeout(&error) => Ok(Sizes::Locked),
        Err(error) => Err(error),
    }
}

/// Tables, views, materialized views and foreign tables.
pub(super) async fn relations(
    session: &dyn Session,
    pin: Pin,
    server: &ServerInfo,
    filter: Filter<'_>,
    kinds: RelKinds,
    include_system: bool,
    details: bool,
) -> CatalogResult<Vec<RelRow>> {
    let system = system_relation("c", "n");
    let detach = if server.at_least(14, 0) {
        "COALESCE(pi.inhdetachpending, false)"
    } else {
        "false"
    };
    // `pg_get_viewdef` and `pg_get_partkeydef` lock the relation they
    // print and so wait behind a session holding it in ACCESS EXCLUSIVE
    // mode: they run only for the details of one relation, never in a list,
    // so a locked object only affects its own details. Partition bounds
    // hold constants only and are printed without naming a relation
    // (`pg_get_expr(..., 0)`), which takes no lock.
    let (definition, part_key) = if details {
        (
            "CASE WHEN c.relkind OPERATOR(pg_catalog.=) ANY (ARRAY['v', 'm']::pg_catalog.\"char\"[]) THEN pg_catalog.pg_get_viewdef(c.oid, true) END",
            "CASE WHEN pt.partrelid IS NOT NULL THEN pg_catalog.pg_get_partkeydef(c.oid) END",
        )
    } else {
        ("NULL::pg_catalog.text", "NULL::pg_catalog.text")
    };
    let kind_filter = match kinds {
        RelKinds::TopLevelTables => {
            "c.relkind OPERATOR(pg_catalog.=) ANY (ARRAY['r', 'p', 'f']::pg_catalog.\"char\"[]) AND NOT c.relispartition"
        }
        RelKinds::Partitions => {
            "c.relkind OPERATOR(pg_catalog.=) ANY (ARRAY['r', 'p', 'f']::pg_catalog.\"char\"[]) AND c.relispartition"
        }
        RelKinds::Views => "c.relkind OPERATOR(pg_catalog.=) 'v'",
        RelKinds::MaterializedViews => "c.relkind OPERATOR(pg_catalog.=) 'm'",
        RelKinds::Any => {
            "c.relkind OPERATOR(pg_catalog.=) ANY (ARRAY['r', 'p', 'v', 'm', 'f']::pg_catalog.\"char\"[])"
        }
    };
    let options = options_json("c.reloptions", "");
    let foreign_options = options_json("ft.ftoptions", "");
    let sql = format!(
        "SELECT n.nspname AS schema, c.relname AS name, c.relkind::pg_catalog.text AS relkind, \
           c.relispartition AS is_partition, pg_catalog.pg_get_userbyid(c.relowner) AS owner, \
           c.relpersistence OPERATOR(pg_catalog.=) 'u' AS unlogged, \
           CASE WHEN c.reloftype OPERATOR(pg_catalog.<>) 0 THEN pg_catalog.format_type(c.reloftype, NULL) END AS of_type, \
           am.amname AS access_method, ts.spcname AS tablespace, {options} AS options, \
           (SELECT pg_catalog.json_agg(pg_catalog.json_build_object('name', 'toast.' OPERATOR(pg_catalog.||) o.option_name, \
              'value', o.option_value) ORDER BY o.option_name) \
            FROM pg_catalog.pg_class tc \
            CROSS JOIN LATERAL pg_catalog.pg_options_to_table(tc.reloptions) o \
            WHERE tc.oid OPERATOR(pg_catalog.=) c.reltoastrelid) AS toast_options, \
           (SELECT pg_catalog.json_agg(pg_catalog.json_build_object('schema', ipn.nspname, \
              'name', ip.relname) ORDER BY ih.inhseqno) \
            FROM pg_catalog.pg_inherits ih \
            JOIN pg_catalog.pg_class ip ON ip.oid OPERATOR(pg_catalog.=) ih.inhparent \
            JOIN pg_catalog.pg_namespace ipn ON ipn.oid OPERATOR(pg_catalog.=) ip.relnamespace \
            WHERE ih.inhrelid OPERATOR(pg_catalog.=) c.oid AND NOT c.relispartition) AS inherits, \
           pt.partstrat::pg_catalog.text AS part_strategy, \
           {part_key} AS part_key, \
           dn.nspname AS default_schema, dc.relname AS default_name, \
           pn.nspname AS parent_schema, pc.relname AS parent_name, \
           CASE WHEN c.relispartition THEN pg_catalog.pg_get_expr(c.relpartbound, 0) END AS bound, \
           {detach} AS detach_pending, \
           fs.srvname AS foreign_server, {foreign_options} AS foreign_options, \
           {wrapper_extension} AS wrapper_extension, \
           CASE WHEN c.reltuples OPERATOR(pg_catalog.<) 0 THEN NULL ELSE c.reltuples::pg_catalog.int8 END AS row_estimate, \
           ({table_pages} OPERATOR(pg_catalog.+) {index_pages}) OPERATOR(pg_catalog.*) {BLOCK} AS total_bytes, \
           c.relrowsecurity AS rls_enabled, c.relforcerowsecurity AS rls_forced, \
           c.relreplident::pg_catalog.text AS replica_identity, c.relispopulated AS populated, \
           {definition} AS definition, \
           (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) c.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {system} AS system, \
           (SELECT pg_catalog.count(*) FROM pg_catalog.pg_inherits ch WHERE ch.inhparent OPERATOR(pg_catalog.=) c.oid)::pg_catalog.int8 AS child_count, \
           EXISTS (SELECT 1 FROM pg_catalog.pg_index ux WHERE ux.indrelid OPERATOR(pg_catalog.=) c.oid AND ux.indisunique \
                   AND ux.indisvalid AND ux.indpred IS NULL AND ux.indexprs IS NULL) AS has_plain_unique_index \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
         LEFT JOIN pg_catalog.pg_am am ON am.oid OPERATOR(pg_catalog.=) c.relam \
         LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid OPERATOR(pg_catalog.=) c.reltablespace \
         LEFT JOIN pg_catalog.pg_partitioned_table pt ON pt.partrelid OPERATOR(pg_catalog.=) c.oid \
         LEFT JOIN pg_catalog.pg_class dc ON dc.oid OPERATOR(pg_catalog.=) pt.partdefid \
         LEFT JOIN pg_catalog.pg_namespace dn ON dn.oid OPERATOR(pg_catalog.=) dc.relnamespace \
         LEFT JOIN pg_catalog.pg_inherits pi ON c.relispartition AND pi.inhrelid OPERATOR(pg_catalog.=) c.oid \
         LEFT JOIN pg_catalog.pg_class pc ON pc.oid OPERATOR(pg_catalog.=) pi.inhparent \
         LEFT JOIN pg_catalog.pg_namespace pn ON pn.oid OPERATOR(pg_catalog.=) pc.relnamespace \
         LEFT JOIN pg_catalog.pg_foreign_table ft ON ft.ftrelid OPERATOR(pg_catalog.=) c.oid \
         LEFT JOIN pg_catalog.pg_foreign_server fs ON fs.oid OPERATOR(pg_catalog.=) ft.ftserver \
         WHERE {kind_filter} AND {where_}{system_where} \
         ORDER BY n.nspname, c.relname",
        where_ = filter.sql("c", "n", None)?,
        system_where = system_filter(include_system, &system),
        table_pages = TABLE_PAGES,
        wrapper_extension = fdw_extension_sql("fs.srvfdw"),
        index_pages = INDEX_PAGES,
    );
    let rows = fetch(session, pin, &sql, "q.schema, q.name").await?;
    rows.iter().map(|row| rel_row(&row)).collect()
}

fn rel_row(row: &Row<'_>) -> CatalogResult<RelRow> {
    let default_partition = match (
        opt_text(row, "default_schema")?,
        opt_text(row, "default_name")?,
    ) {
        (Some(schema), Some(name)) => Some(PgQualifiedName { schema, name }),
        _ => None,
    };
    let partition_of = match (
        opt_text(row, "parent_schema")?,
        opt_text(row, "parent_name")?,
        opt_text(row, "bound")?,
    ) {
        (Some(schema), Some(name), Some(bound)) => Some(PgPartitionOf {
            parent: PgQualifiedName { schema, name },
            is_default: bound == "DEFAULT",
            bound,
            detach_pending: flag(row, "detach_pending")?,
        }),
        _ => None,
    };
    let foreign = match opt_text(row, "foreign_server")? {
        Some(server) => Some(PgForeignInfo {
            server,
            options: redacted(
                row.text("wrapper_extension")?.as_deref(),
                json(row, "foreign_options")?,
            ),
        }),
        None => None,
    };
    Ok(RelRow {
        schema: text(row, "schema")?,
        name: text(row, "name")?,
        relkind: text(row, "relkind")?,
        is_partition: flag(row, "is_partition")?,
        owner: text(row, "owner")?,
        unlogged: flag(row, "unlogged")?,
        of_type: opt_text(row, "of_type")?,
        access_method: opt_text(row, "access_method")?,
        tablespace: opt_text(row, "tablespace")?,
        options: json(row, "options")?,
        toast_options: json(row, "toast_options")?,
        inherits: json(row, "inherits")?,
        part_strategy: opt_text(row, "part_strategy")?.map(|s| {
            match s.as_str() {
                "r" => "range",
                "l" => "list",
                "h" => "hash",
                other => other,
            }
            .to_owned()
        }),
        part_key: opt_text(row, "part_key")?,
        default_partition,
        partition_of,
        foreign,
        row_estimate: opt_int(row, "row_estimate")?,
        total_bytes: opt_int(row, "total_bytes")?,
        rls_enabled: flag(row, "rls_enabled")?,
        rls_forced: flag(row, "rls_forced")?,
        replica_identity: match text(row, "replica_identity")?.as_str() {
            "n" => "nothing",
            "f" => "full",
            "i" => "index",
            _ => "default",
        }
        .to_owned(),
        populated: flag(row, "populated")?,
        definition: opt_text(row, "definition")?.map(|d| strip_semicolon(&d)),
        comment: opt_text(row, "comment")?,
        system: flag(row, "system")?,
        child_count: int(row, "child_count")?,
        has_plain_unique_index: flag(row, "has_plain_unique_index")?,
    })
}

/// Foreign data wrapper options with every value hidden that is not known
/// to be harmless for the wrapper (the extension implementing it, `None`
/// for an unknown wrapper, which hides every value) or that looks like it
/// carries credentials: the shared allowlist of the programs catalog.
fn redacted(wrapper_extension: Option<&str>, options: Vec<PgOption>) -> Vec<PgFdwOption> {
    options
        .into_iter()
        .map(|o| PgFdwOption {
            value: redact_option(wrapper_extension, &o.name, &o.value),
            name: o.name,
        })
        .collect()
}

/// SQL `FROM` part shared by the child loaders: the relation `t` in
/// namespace `n`.
const REL_JOIN: &str = "JOIN pg_catalog.pg_class t ON t.oid OPERATOR(pg_catalog.=) {rel} \
                        JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.relnamespace";

fn rel_join(rel: &str) -> String {
    REL_JOIN.replace("{rel}", rel)
}

// ---------------------------------------------------------------------------
// Columns

/// Columns of the relations matched by `filter`.
pub(super) async fn columns(
    session: &dyn Session,
    pin: Pin,
    server: &ServerInfo,
    filter: Filter<'_>,
) -> CatalogResult<Vec<PgColumn>> {
    let compression = if server.at_least(14, 0) {
        "(CASE WHEN a.attcompression::pg_catalog.text OPERATOR(pg_catalog.<>) '' THEN a.attcompression::pg_catalog.text END)"
    } else {
        "NULL::pg_catalog.text"
    };
    let not_null_name = if server.at_least(18, 0) {
        "(SELECT k.conname FROM pg_catalog.pg_constraint k WHERE k.conrelid OPERATOR(pg_catalog.=) a.attrelid \
          AND k.contype OPERATOR(pg_catalog.=) 'n' AND k.conkey[1] OPERATOR(pg_catalog.=) a.attnum LIMIT 1)"
    } else {
        "NULL::pg_catalog.name"
    };
    let options = options_json("a.attoptions", "");
    let fdw_options = options_json("a.attfdwoptions", "");
    let sql = format!(
        "SELECT n.nspname AS schema, t.relname AS table_name, a.attname AS name, \
           a.attnum::pg_catalog.int8 AS position, pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type, \
           a.attnotnull AS not_null, {not_null_name}::pg_catalog.text AS not_null_constraint, \
           pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) AS expr, a.attgenerated::pg_catalog.text AS generated, \
           a.attidentity::pg_catalog.text AS identity, sn.nspname AS seq_schema, s.relname AS seq_name, \
           sq.seqstart AS seq_start, sq.seqincrement AS seq_increment, sq.seqmin AS seq_min, \
           sq.seqmax AS seq_max, sq.seqcache AS seq_cache, sq.seqcycle AS seq_cycle, \
           CASE WHEN a.attcollation OPERATOR(pg_catalog.<>) ty.typcollation THEN con.nspname END AS collation_schema, \
           CASE WHEN a.attcollation OPERATOR(pg_catalog.<>) ty.typcollation THEN co.collname END AS collation_name, \
           a.attstorage::pg_catalog.text AS storage, ty.typstorage::pg_catalog.text AS type_storage, \
           {compression} AS compression, \
           CASE WHEN a.attstattarget OPERATOR(pg_catalog.>=) 0 THEN a.attstattarget::pg_catalog.int8 END AS statistics_target, \
           {options} AS options, {fdw_options} AS fdw_options, \
           a.attislocal AS local, a.attinhcount::pg_catalog.int8 AS inherited_count, \
           (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) t.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) a.attnum) AS comment, \
           {wrapper_extension} AS wrapper_extension \
         FROM pg_catalog.pg_attribute a {join} \
         JOIN pg_catalog.pg_type ty ON ty.oid OPERATOR(pg_catalog.=) a.atttypid \
         LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid OPERATOR(pg_catalog.=) a.attrelid AND ad.adnum OPERATOR(pg_catalog.=) a.attnum \
         LEFT JOIN pg_catalog.pg_foreign_table ft ON ft.ftrelid OPERATOR(pg_catalog.=) a.attrelid \
         LEFT JOIN pg_catalog.pg_foreign_server fs ON fs.oid OPERATOR(pg_catalog.=) ft.ftserver \
         LEFT JOIN pg_catalog.pg_collation co ON co.oid OPERATOR(pg_catalog.=) a.attcollation \
         LEFT JOIN pg_catalog.pg_namespace con ON con.oid OPERATOR(pg_catalog.=) co.collnamespace \
         LEFT JOIN pg_catalog.pg_depend dep ON a.attidentity OPERATOR(pg_catalog.<>) '' \
           AND dep.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass \
           AND dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass \
           AND dep.refobjid OPERATOR(pg_catalog.=) a.attrelid AND dep.refobjsubid OPERATOR(pg_catalog.=) a.attnum AND dep.deptype OPERATOR(pg_catalog.=) 'i' \
         LEFT JOIN pg_catalog.pg_class s ON s.oid OPERATOR(pg_catalog.=) dep.objid AND s.relkind OPERATOR(pg_catalog.=) 'S' \
         LEFT JOIN pg_catalog.pg_namespace sn ON sn.oid OPERATOR(pg_catalog.=) s.relnamespace \
         LEFT JOIN pg_catalog.pg_sequence sq ON sq.seqrelid OPERATOR(pg_catalog.=) s.oid \
         WHERE a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped AND t.relkind OPERATOR(pg_catalog.=) ANY (ARRAY['r', 'p', 'v', 'm', 'f']::pg_catalog.\"char\"[]) \
           AND {where_} \
         ORDER BY n.nspname, t.relname, a.attnum",
        join = rel_join("a.attrelid"),
        wrapper_extension = fdw_extension_sql("fs.srvfdw"),
        where_ = filter.sql("t", "n", Some("a.attname"))?,
    );
    let rows = fetch(session, pin, &sql, "q.schema, q.table_name, q.position").await?;
    rows.iter().map(|row| column_row(&row)).collect()
}

fn column_row(row: &Row<'_>) -> CatalogResult<PgColumn> {
    let expr = opt_text(row, "expr")?;
    let generated_kind = text(row, "generated")?;
    let (default, generated) = match generated_kind.as_str() {
        "" => (expr, None),
        kind => (
            None,
            expr.map(|expression| PgGenerated {
                expression,
                stored: kind == "s",
            }),
        ),
    };
    let identity = match text(row, "identity")?.as_str() {
        "" => None,
        kind => Some(PgIdentity {
            generation: if kind == "a" { "always" } else { "byDefault" }.to_owned(),
            sequence: match (opt_text(row, "seq_schema")?, opt_text(row, "seq_name")?) {
                (Some(schema), Some(name)) => Some(PgQualifiedName { schema, name }),
                _ => None,
            },
            start: opt_int(row, "seq_start")?,
            increment: opt_int(row, "seq_increment")?,
            min: opt_int(row, "seq_min")?,
            max: opt_int(row, "seq_max")?,
            cache: opt_int(row, "seq_cache")?,
            cycle: row.bool("seq_cycle")?,
        }),
    };
    let collation = match (
        opt_text(row, "collation_schema")?,
        opt_text(row, "collation_name")?,
    ) {
        (Some(schema), Some(name)) => Some(PgQualifiedName { schema, name }),
        _ => None,
    };
    Ok(PgColumn {
        schema: text(row, "schema")?,
        table: text(row, "table_name")?,
        name: text(row, "name")?,
        position: int(row, "position")?,
        data_type: text(row, "data_type")?,
        not_null: flag(row, "not_null")?,
        not_null_constraint: opt_text(row, "not_null_constraint")?,
        default,
        identity,
        generated,
        collation,
        storage: storage_name(&text(row, "storage")?),
        default_storage: storage_name(&text(row, "type_storage")?),
        compression: opt_text(row, "compression")?.map(|c| {
            match c.as_str() {
                "p" => "pglz",
                "l" => "lz4",
                other => other,
            }
            .to_owned()
        }),
        statistics_target: opt_int(row, "statistics_target")?,
        options: json(row, "options")?,
        fdw_options: redacted(
            row.text("wrapper_extension")?.as_deref(),
            json(row, "fdw_options")?,
        ),
        local: flag(row, "local")?,
        inherited_count: int(row, "inherited_count")?,
        comment: opt_text(row, "comment")?,
    })
}

fn storage_name(code: &str) -> String {
    match code {
        "p" => "plain",
        "e" => "external",
        "m" => "main",
        "x" => "extended",
        other => other,
    }
    .to_owned()
}

// ---------------------------------------------------------------------------
// Indexes

/// JSON array of the key elements of the index `i` (a `pg_index` row), as
/// [`PgIndexKey`]: the column name unquoted (or the expression), order,
/// and the collation, operator class and its parameters when they are not
/// what the key gets by default, so the UI can build the same key again.
///
/// Defaults follow `pg_get_indexdef`: the collation is the column's; for an
/// expression every collation is listed (its derived collation cannot be
/// read from SQL, and naming it builds the same index); the operator class is the
/// method's default for the key type, found by exact type, implicit binary
/// coercion or a polymorphic input type (`anyarray`, `anyenum`,
/// `anyrange`, `anymultirange`). Anything not proven default is listed:
/// naming a default class or collation again builds the same index. A
/// class with parameters is always listed, as the parameters need it.
fn index_keys_sql() -> String {
    let n = "k.n OPERATOR(pg_catalog.-) 1";
    let opclass_options = options_json("ia.attoptions", "");
    format!(
        "SELECT pg_catalog.json_agg(pg_catalog.json_build_object( \
           'definition', pg_catalog.pg_get_indexdef(i.indexrelid, k.n, true), \
           'column', ka.attname, \
           'expression', i.indkey[{n}] OPERATOR(pg_catalog.=) 0, \
           'descending', (i.indoption[{n}]::int OPERATOR(pg_catalog.&) 1) OPERATOR(pg_catalog.<>) 0, \
           'nullsFirst', (i.indoption[{n}]::int OPERATOR(pg_catalog.&) 2) OPERATOR(pg_catalog.<>) 0, \
           'collation', CASE WHEN i.indcollation[{n}] OPERATOR(pg_catalog.<>) 0 \
               AND (ka.attnum IS NULL OR i.indcollation[{n}] OPERATOR(pg_catalog.<>) ka.attcollation) THEN \
             (SELECT pg_catalog.json_build_object('schema', ccn.nspname, 'name', cco.collname) \
              FROM pg_catalog.pg_collation cco JOIN pg_catalog.pg_namespace ccn ON ccn.oid OPERATOR(pg_catalog.=) cco.collnamespace \
              WHERE cco.oid OPERATOR(pg_catalog.=) i.indcollation[{n}]) END, \
           'opclass', CASE WHEN ia.attoptions IS NOT NULL OR NOT (oc.opcdefault AND ( \
               oc.opcintype OPERATOR(pg_catalog.=) kb.oid \
               OR (NOT EXISTS (SELECT 1 FROM pg_catalog.pg_opclass dc WHERE dc.opcmethod OPERATOR(pg_catalog.=) oc.opcmethod \
                     AND dc.opcdefault AND dc.opcintype OPERATOR(pg_catalog.=) kb.oid) \
                 AND (EXISTS (SELECT 1 FROM pg_catalog.pg_cast bc WHERE bc.castsource OPERATOR(pg_catalog.=) kb.oid \
                        AND bc.casttarget OPERATOR(pg_catalog.=) oc.opcintype AND bc.castmethod OPERATOR(pg_catalog.=) 'b' \
                        AND bc.castcontext OPERATOR(pg_catalog.=) 'i') \
                   OR (oit.typname OPERATOR(pg_catalog.=) 'anyarray' AND kb.typcategory OPERATOR(pg_catalog.=) 'A') \
                   OR (oit.typname OPERATOR(pg_catalog.=) 'anyenum' AND kb.typtype OPERATOR(pg_catalog.=) 'e') \
                   OR (oit.typname OPERATOR(pg_catalog.=) 'anyrange' AND kb.typtype OPERATOR(pg_catalog.=) 'r') \
                   OR (oit.typname OPERATOR(pg_catalog.=) 'anymultirange' AND kb.typtype OPERATOR(pg_catalog.=) 'm'))))) \
             THEN pg_catalog.json_build_object('schema', ocn.nspname, 'name', oc.opcname) END, \
           'opclassOptions', COALESCE({opclass_options}, '[]'::pg_catalog.json), \
           'statisticsTarget', CASE WHEN ia.attstattarget::pg_catalog.int4 OPERATOR(pg_catalog.<>) -1 THEN ia.attstattarget::pg_catalog.int4 END) ORDER BY k.n) \
         FROM pg_catalog.generate_series(1, i.indnkeyatts::pg_catalog.int4) k(n) \
         JOIN pg_catalog.pg_attribute ia ON ia.attrelid OPERATOR(pg_catalog.=) i.indexrelid AND ia.attnum OPERATOR(pg_catalog.=) k.n \
         LEFT JOIN pg_catalog.pg_attribute ka ON i.indkey[{n}] OPERATOR(pg_catalog.<>) 0 \
          AND ka.attrelid OPERATOR(pg_catalog.=) i.indrelid AND ka.attnum OPERATOR(pg_catalog.=) i.indkey[{n}] \
         JOIN pg_catalog.pg_type kt ON kt.oid OPERATOR(pg_catalog.=) COALESCE(ka.atttypid, ia.atttypid) \
         JOIN pg_catalog.pg_type kb ON kb.oid OPERATOR(pg_catalog.=) \
           CASE WHEN kt.typtype OPERATOR(pg_catalog.=) 'd' THEN kt.typbasetype ELSE kt.oid END \
         JOIN pg_catalog.pg_opclass oc ON oc.oid OPERATOR(pg_catalog.=) i.indclass[{n}] \
         JOIN pg_catalog.pg_namespace ocn ON ocn.oid OPERATOR(pg_catalog.=) oc.opcnamespace \
         JOIN pg_catalog.pg_type oit ON oit.oid OPERATOR(pg_catalog.=) oc.opcintype"
    )
}

/// Indexes of the relations matched by `filter`.
pub(super) async fn indexes(
    session: &dyn Session,
    pin: Pin,
    server: &ServerInfo,
    filter: Filter<'_>,
) -> CatalogResult<Vec<PgIndex>> {
    let nulls = if server.at_least(15, 0) {
        "i.indnullsnotdistinct"
    } else {
        "NULL::pg_catalog.bool"
    };
    let system = system_relation("t", "n");
    let options = options_json("ic.reloptions", "");
    let sql = format!(
        "SELECT n.nspname AS schema, t.relname AS table_name, ic.relname AS name, am.amname AS method, \
           i.indisunique AS is_unique, i.indisprimary AS is_primary, i.indisexclusion AS is_exclusion, \
           i.indimmediate AS immediate, {nulls} AS nulls_not_distinct, i.indisclustered AS clustered, \
           i.indisvalid AS valid, i.indisready AS ready, i.indisreplident AS replica_identity, \
           ic.relkind OPERATOR(pg_catalog.=) 'I' AS partitioned, \
           (SELECT pg_catalog.json_build_object('schema', ppn.nspname, 'name', pp.relname) \
            FROM pg_catalog.pg_inherits ph JOIN pg_catalog.pg_class pp ON pp.oid OPERATOR(pg_catalog.=) ph.inhparent \
            JOIN pg_catalog.pg_namespace ppn ON ppn.oid OPERATOR(pg_catalog.=) pp.relnamespace \
            WHERE ph.inhrelid OPERATOR(pg_catalog.=) ic.oid LIMIT 1) AS parent_index, \
           ({keys}) AS keys, \
           ARRAY(SELECT ca.attname::pg_catalog.text \
                 FROM pg_catalog.generate_series(i.indnkeyatts OPERATOR(pg_catalog.+) 1, i.indnatts) k \
                 JOIN pg_catalog.pg_attribute ca ON ca.attrelid OPERATOR(pg_catalog.=) i.indrelid \
                  AND ca.attnum OPERATOR(pg_catalog.=) i.indkey[k OPERATOR(pg_catalog.-) 1] \
                 ORDER BY k) AS include_columns, \
           pg_catalog.pg_get_expr(i.indpred, i.indrelid, true) AS predicate, \
           {options} AS options, ts.spcname AS tablespace, \
           ic.relpages::pg_catalog.int8 OPERATOR(pg_catalog.*) {BLOCK} AS size_bytes, \
           (SELECT k.conname FROM pg_catalog.pg_constraint k WHERE k.conindid OPERATOR(pg_catalog.=) i.indexrelid \
              AND k.conrelid OPERATOR(pg_catalog.=) i.indrelid AND k.contype OPERATOR(pg_catalog.=) ANY (ARRAY['p', 'u', 'x']::pg_catalog.\"char\"[]) LIMIT 1) AS constraint_name, \
           pg_catalog.pg_get_indexdef(i.indexrelid, 0, true) AS definition, \
           ARRAY(SELECT fn.nspname::pg_catalog.text OPERATOR(pg_catalog.||) '.' OPERATOR(pg_catalog.||) ft.relname::pg_catalog.text \
                   OPERATOR(pg_catalog.||) '.' OPERATOR(pg_catalog.||) fk.conname::pg_catalog.text \
                 FROM pg_catalog.pg_constraint fk JOIN pg_catalog.pg_class ft ON ft.oid OPERATOR(pg_catalog.=) fk.conrelid \
                 JOIN pg_catalog.pg_namespace fn ON fn.oid OPERATOR(pg_catalog.=) ft.relnamespace \
                 WHERE fk.contype OPERATOR(pg_catalog.=) 'f' AND fk.conindid OPERATOR(pg_catalog.=) i.indexrelid ORDER BY 1) AS foreign_keys, \
           (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) ic.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {system} AS system \
         FROM pg_catalog.pg_index i \
         JOIN pg_catalog.pg_class ic ON ic.oid OPERATOR(pg_catalog.=) i.indexrelid {join} \
         JOIN pg_catalog.pg_am am ON am.oid OPERATOR(pg_catalog.=) ic.relam \
         LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid OPERATOR(pg_catalog.=) ic.reltablespace \
         WHERE {where_} ORDER BY n.nspname, t.relname, ic.relname",
        join = rel_join("i.indrelid"),
        where_ = filter.sql("t", "n", Some("ic.relname"))?,
        keys = index_keys_sql(),
    );
    let rows = fetch(session, pin, &sql, "q.schema, q.table_name, q.name").await?;
    rows.iter()
        .map(|row| {
            let keys: Vec<PgIndexKey> = json(&row, "keys")?;
            Ok(PgIndex {
                schema: text(&row, "schema")?,
                table: text(&row, "table_name")?,
                name: text(&row, "name")?,
                method: text(&row, "method")?,
                unique: flag(&row, "is_unique")?,
                primary: flag(&row, "is_primary")?,
                exclusion: flag(&row, "is_exclusion")?,
                immediate: flag(&row, "immediate")?,
                nulls_not_distinct: row.bool("nulls_not_distinct")?,
                clustered: flag(&row, "clustered")?,
                valid: flag(&row, "valid")?,
                ready: flag(&row, "ready")?,
                replica_identity: flag(&row, "replica_identity")?,
                partitioned: flag(&row, "partitioned")?,
                parent_index: json::<Option<PgQualifiedName>>(&row, "parent_index")?,
                keys,
                include_columns: text_list(&row, "include_columns")?,
                predicate: opt_text(&row, "predicate")?,
                storage_parameters: json(&row, "options")?,
                tablespace: opt_text(&row, "tablespace")?,
                size_bytes: opt_int(&row, "size_bytes")?,
                size_note: None,
                constraint: opt_text(&row, "constraint_name")?,
                definition: text(&row, "definition")?,
                foreign_keys: text_list(&row, "foreign_keys")?,
                comment: opt_text(&row, "comment")?,
                system: flag(&row, "system")?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Constraints

/// Constraints of the relations matched by `filter` (constraint triggers
/// are listed as triggers).
pub(super) async fn constraints(
    session: &dyn Session,
    pin: Pin,
    server: &ServerInfo,
    filter: Filter<'_>,
) -> CatalogResult<Vec<PgConstraint>> {
    let enforced = if server.at_least(18, 0) {
        "k.conenforced"
    } else {
        "true"
    };
    let set_columns = if server.at_least(15, 0) {
        "ARRAY(SELECT a.attname::pg_catalog.text FROM pg_catalog.unnest(k.confdelsetcols) WITH ORDINALITY x(num, ord) \
         JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) k.conrelid AND a.attnum OPERATOR(pg_catalog.=) x.num ORDER BY x.ord)"
    } else {
        "NULL::pg_catalog.text[]"
    };
    let sql = format!(
        "SELECT n.nspname AS schema, t.relname AS table_name, k.conname AS name, \
           k.contype::pg_catalog.text AS contype, \
           ARRAY(SELECT a.attname::pg_catalog.text FROM pg_catalog.unnest(k.conkey) WITH ORDINALITY x(num, ord) \
                 JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) k.conrelid AND a.attnum OPERATOR(pg_catalog.=) x.num \
                 ORDER BY x.ord) AS columns, \
           pg_catalog.pg_get_constraintdef(k.oid, true) AS definition, \
           k.condeferrable AS deferrable, k.condeferred AS deferred, k.convalidated AS validated, \
           {enforced} AS enforced, k.connoinherit AS no_inherit, k.conislocal AS local, \
           k.coninhcount::pg_catalog.int8 AS inherited_count, k.conparentid OPERATOR(pg_catalog.<>) 0 AS has_parent, \
           CASE WHEN k.contype OPERATOR(pg_catalog.=) ANY (ARRAY['p', 'u', 'x']::pg_catalog.\"char\"[]) THEN ic.relname END AS index_name, \
           rn.nspname AS ref_schema, rt.relname AS ref_table, \
           ARRAY(SELECT a.attname::pg_catalog.text FROM pg_catalog.unnest(k.confkey) WITH ORDINALITY x(num, ord) \
                 JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) k.confrelid AND a.attnum OPERATOR(pg_catalog.=) x.num \
                 ORDER BY x.ord) AS ref_columns, \
           k.confupdtype::pg_catalog.text AS on_update, k.confdeltype::pg_catalog.text AS on_delete, \
           k.confmatchtype::pg_catalog.text AS match_type, {set_columns} AS set_columns, \
           (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) k.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_constraint'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, \
           CASE WHEN k.contype OPERATOR(pg_catalog.=) 'p' THEN 0 WHEN k.contype OPERATOR(pg_catalog.=) 'u' THEN 1 \
             WHEN k.contype OPERATOR(pg_catalog.=) 'x' THEN 2 WHEN k.contype OPERATOR(pg_catalog.=) 'c' THEN 3 \
             WHEN k.contype OPERATOR(pg_catalog.=) 'n' THEN 4 ELSE 5 END AS type_order \
         FROM pg_catalog.pg_constraint k {join} \
         LEFT JOIN pg_catalog.pg_class ic ON ic.oid OPERATOR(pg_catalog.=) k.conindid \
         LEFT JOIN pg_catalog.pg_class rt ON k.contype OPERATOR(pg_catalog.=) 'f' AND rt.oid OPERATOR(pg_catalog.=) k.confrelid \
         LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid OPERATOR(pg_catalog.=) rt.relnamespace \
         WHERE k.contype OPERATOR(pg_catalog.<>) 't' AND {NOT_A_CLONE} AND {where_}",
        join = rel_join("k.conrelid"),
        where_ = filter.sql("t", "n", Some("k.conname"))?,
    );
    let rows = fetch(
        session,
        pin,
        &sql,
        "q.schema, q.table_name, q.type_order, q.name",
    )
    .await?;
    rows.iter().map(|row| constraint_row(&row)).collect()
}

/// Leaves out the rows PostgreSQL keeps for a foreign key that references a
/// partitioned table: one clone per referenced partition, on the same
/// referencing table, whose parent is that table's own foreign key. They
/// are not constraints of their own (the server creates and drops them
/// with the foreign key), so they are neither listed nor written as DDL.
/// A partition's foreign key inherited from its partitioned parent has a
/// parent on another table and stays. Refers to the constraint as `k`.
pub(super) const NOT_A_CLONE: &str = "NOT EXISTS (SELECT FROM pg_catalog.pg_constraint pk \
     WHERE pk.oid OPERATOR(pg_catalog.=) k.conparentid \
     AND pk.conrelid OPERATOR(pg_catalog.=) k.conrelid)";

/// The columns of a constraint `k` by name, in key order; an expression
/// element of an exclusion constraint (0 in `conkey`) shows as
/// `expression`.
const CONSTRAINT_COLUMNS: &str = "ARRAY(SELECT COALESCE(a.attname::pg_catalog.text, 'expression') \
     FROM pg_catalog.unnest(k.conkey) WITH ORDINALITY x(num, ord) \
     LEFT JOIN pg_catalog.pg_attribute a \
     ON a.attrelid OPERATOR(pg_catalog.=) k.conrelid \
     AND a.attnum OPERATOR(pg_catalog.=) x.num ORDER BY x.ord)";

fn fk_action(code: &str) -> String {
    match code {
        "r" => "restrict",
        "c" => "cascade",
        "n" => "setNull",
        "d" => "setDefault",
        _ => "noAction",
    }
    .to_owned()
}

fn constraint_row(row: &Row<'_>) -> CatalogResult<PgConstraint> {
    let contype = text(row, "contype")?;
    let foreign_key = match (opt_text(row, "ref_schema")?, opt_text(row, "ref_table")?) {
        (Some(schema), Some(name)) if contype == "f" => Some(PgForeignKey {
            referenced: PgQualifiedName { schema, name },
            referenced_columns: text_list(row, "ref_columns")?,
            on_update: fk_action(&text(row, "on_update")?),
            on_delete: fk_action(&text(row, "on_delete")?),
            match_type: match text(row, "match_type")?.as_str() {
                "f" => "full",
                "p" => "partial",
                _ => "simple",
            }
            .to_owned(),
            on_delete_set_columns: text_list(row, "set_columns")?,
        }),
        _ => None,
    };
    Ok(PgConstraint {
        schema: text(row, "schema")?,
        table: text(row, "table_name")?,
        name: text(row, "name")?,
        constraint_type: match contype.as_str() {
            "p" => "primaryKey",
            "u" => "unique",
            "c" => "check",
            "x" => "exclusion",
            "f" => "foreignKey",
            "n" => "notNull",
            other => other,
        }
        .to_owned(),
        columns: text_list(row, "columns")?,
        definition: text(row, "definition")?,
        deferrable: flag(row, "deferrable")?,
        deferred: flag(row, "deferred")?,
        validated: flag(row, "validated")?,
        enforced: flag(row, "enforced")?,
        no_inherit: flag(row, "no_inherit")?,
        local: flag(row, "local")?,
        inherited_count: int(row, "inherited_count")?,
        has_parent_constraint: flag(row, "has_parent")?,
        index: opt_text(row, "index_name")?,
        foreign_key,
        comment: opt_text(row, "comment")?,
    })
}

// ---------------------------------------------------------------------------
// Triggers, rules, policies

/// Triggers of the relations matched by `filter` (internal triggers
/// excluded).
pub(super) async fn triggers(
    session: &dyn Session,
    pin: Pin,
    filter: Filter<'_>,
) -> CatalogResult<Vec<PgTrigger>> {
    let sql = format!(
        "SELECT n.nspname AS schema, t.relname AS table_name, g.tgname AS name, \
           g.tgtype::pg_catalog.int8 AS tgtype, \
           ARRAY(SELECT a.attname::pg_catalog.text FROM pg_catalog.unnest(g.tgattr::pg_catalog.int2[]) WITH ORDINALITY x(num, ord) \
                 JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) g.tgrelid AND a.attnum OPERATOR(pg_catalog.=) x.num \
                 ORDER BY x.ord) AS update_columns, \
           g.tgqual IS NOT NULL AS has_condition, g.tgfoid::pg_catalog.regprocedure::pg_catalog.text AS function, \
           g.tgenabled::pg_catalog.text AS enabled, g.tgconstraint OPERATOR(pg_catalog.<>) 0 AS constraint_trigger, \
           g.tgdeferrable AS deferrable, g.tginitdeferred AS deferred, \
           g.tgoldtable::pg_catalog.text AS old_table, g.tgnewtable::pg_catalog.text AS new_table, \
           g.tgparentid OPERATOR(pg_catalog.<>) 0 AS inherited, \
           pg_catalog.pg_get_triggerdef(g.oid, true) AS definition, \
           (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) g.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_trigger'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment \
         FROM pg_catalog.pg_trigger g {join} \
         WHERE NOT g.tgisinternal AND {where_} ORDER BY n.nspname, t.relname, g.tgname",
        join = rel_join("g.tgrelid"),
        where_ = filter.sql("t", "n", Some("g.tgname"))?,
    );
    let rows = fetch(session, pin, &sql, "q.schema, q.table_name, q.name").await?;
    rows.iter()
        .map(|row| {
            let tgtype = int(&row, "tgtype")?;
            let mut events = Vec::new();
            for (bit, name) in [
                (4, "insert"),
                (16, "update"),
                (8, "delete"),
                (32, "truncate"),
            ] {
                if tgtype & bit != 0 {
                    events.push(name.to_owned());
                }
            }
            let definition = text(&row, "definition")?;
            let condition = if flag(&row, "has_condition")? {
                trigger_condition(&definition)
            } else {
                None
            };
            Ok(PgTrigger {
                schema: text(&row, "schema")?,
                table: text(&row, "table_name")?,
                name: text(&row, "name")?,
                timing: if tgtype & 64 != 0 {
                    "insteadOf"
                } else if tgtype & 2 != 0 {
                    "before"
                } else {
                    "after"
                }
                .to_owned(),
                events,
                update_columns: text_list(&row, "update_columns")?,
                level: if tgtype & 1 != 0 { "row" } else { "statement" }.to_owned(),
                condition,
                function: text(&row, "function")?,
                enabled: enabled_state(&text(&row, "enabled")?),
                constraint_trigger: flag(&row, "constraint_trigger")?,
                deferrable: flag(&row, "deferrable")?,
                deferred: flag(&row, "deferred")?,
                old_table: opt_text(&row, "old_table")?,
                new_table: opt_text(&row, "new_table")?,
                inherited: flag(&row, "inherited")?,
                definition,
                comment: opt_text(&row, "comment")?,
            })
        })
        .collect()
}

fn enabled_state(code: &str) -> String {
    match code {
        "D" => "disabled",
        "R" => "replica",
        "A" => "always",
        _ => "origin",
    }
    .to_owned()
}

/// Rewrite rules of the relations matched by `filter` (a view's own
/// `_RETURN` rule excluded).
pub(super) async fn rules(
    session: &dyn Session,
    pin: Pin,
    filter: Filter<'_>,
) -> CatalogResult<Vec<PgRule>> {
    let sql = format!(
        "SELECT n.nspname AS schema, t.relname AS table_name, r.rulename AS name, \
           r.ev_type::pg_catalog.text AS event, r.is_instead AS instead, r.ev_enabled::pg_catalog.text AS enabled, \
           pg_catalog.pg_get_ruledef(r.oid, true) AS definition, \
           (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) r.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_rewrite'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment \
         FROM pg_catalog.pg_rewrite r {join} \
         WHERE r.rulename OPERATOR(pg_catalog.<>) '_RETURN' AND {where_} ORDER BY n.nspname, t.relname, r.rulename",
        join = rel_join("r.ev_class"),
        where_ = filter.sql("t", "n", Some("r.rulename"))?,
    );
    let rows = fetch(session, pin, &sql, "q.schema, q.table_name, q.name").await?;
    rows.iter()
        .map(|row| {
            Ok(PgRule {
                schema: text(&row, "schema")?,
                table: text(&row, "table_name")?,
                name: text(&row, "name")?,
                event: match text(&row, "event")?.as_str() {
                    "1" => "select",
                    "2" => "update",
                    "3" => "insert",
                    _ => "delete",
                }
                .to_owned(),
                instead: flag(&row, "instead")?,
                enabled: enabled_state(&text(&row, "enabled")?),
                definition: strip_semicolon(&text(&row, "definition")?),
                comment: opt_text(&row, "comment")?,
            })
        })
        .collect()
}

/// Row level security policies of the relations matched by `filter`.
pub(super) async fn policies(
    session: &dyn Session,
    pin: Pin,
    filter: Filter<'_>,
) -> CatalogResult<Vec<PgPolicy>> {
    let sql = format!(
        "SELECT n.nspname AS schema, t.relname AS table_name, p.polname AS name, \
           p.polcmd::pg_catalog.text AS command, p.polpermissive AS permissive, \
           ARRAY(SELECT CASE WHEN x.r OPERATOR(pg_catalog.=) 0 THEN 'public' ELSE pg_catalog.pg_get_userbyid(x.r)::pg_catalog.text END \
                 FROM pg_catalog.unnest(p.polroles) WITH ORDINALITY x(r, ord) ORDER BY x.ord) AS roles, \
           pg_catalog.pg_get_expr(p.polqual, p.polrelid, true) AS using_expr, \
           pg_catalog.pg_get_expr(p.polwithcheck, p.polrelid, true) AS check_expr, \
           (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) p.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_policy'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment \
         FROM pg_catalog.pg_policy p {join} \
         WHERE {where_} ORDER BY n.nspname, t.relname, p.polname",
        join = rel_join("p.polrelid"),
        where_ = filter.sql("t", "n", Some("p.polname"))?,
    );
    let rows = fetch(session, pin, &sql, "q.schema, q.table_name, q.name").await?;
    rows.iter()
        .map(|row| {
            Ok(PgPolicy {
                schema: text(&row, "schema")?,
                table: text(&row, "table_name")?,
                name: text(&row, "name")?,
                command: policy_command(&text(&row, "command")?).to_owned(),
                permissive: flag(&row, "permissive")?,
                roles: text_list(&row, "roles")?,
                using: opt_text(&row, "using_expr")?,
                with_check: opt_text(&row, "check_expr")?,
                comment: opt_text(&row, "comment")?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Child lists

/// One entry of a relation's child list (columns, indexes, constraints,
/// triggers, rules, policies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChildSummary {
    /// Name of the child.
    pub name: String,
    /// Comment on the child.
    pub comment: Option<String>,
    /// Short description shown next to the name.
    pub detail: Option<String>,
    /// Flags such as `primary` or `disabled`.
    pub flags: Vec<String>,
    /// True for children that belong to the system or an extension.
    pub system: bool,
}

/// The names of the flags that are on.
fn on(pairs: &[(bool, &str)]) -> Vec<String> {
    pairs
        .iter()
        .filter(|(set, _)| *set)
        .map(|(_, name)| (*name).to_owned())
        .collect()
}

/// The children of `kind` of the relation(s) matched by `filter`, for the
/// tree.
///
/// Unlike [`columns`], [`indexes`], [`constraints`], [`triggers`],
/// [`rules`] and [`policies`] these queries read catalog columns only and
/// never deparse (`pg_get_expr`, `pg_get_indexdef`, `pg_get_constraintdef`,
/// `pg_get_triggerdef`, `pg_get_ruledef` open the relation and wait behind
/// a session that holds an `ACCESS EXCLUSIVE` lock on it), so the children
/// of a locked table are still listed; definitions are part of `describe`.
///
/// # Errors
///
/// Driver errors; [`crate::model::CatalogError::Invalid`] for a kind
/// without child list.
pub(super) async fn child_summaries(
    session: &dyn Session,
    server: &ServerInfo,
    filter: Filter<'_>,
    kind: ObjectKind,
) -> CatalogResult<Vec<ChildSummary>> {
    let comment = |oid: &str, catalog: &str| {
        format!(
            "(SELECT cmt.description FROM pg_catalog.pg_description cmt \
             WHERE cmt.objoid OPERATOR(pg_catalog.=) {oid} \
             AND cmt.classoid OPERATOR(pg_catalog.=) '{catalog}'::pg_catalog.regclass \
             AND cmt.objsubid OPERATOR(pg_catalog.=) 0)"
        )
    };
    match kind {
        ObjectKind::Column => {
            let sql = format!(
                "SELECT a.attname AS name, a.attnum::pg_catalog.int8 AS position, \
                   pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type, \
                   a.attnotnull AS not_null, a.attidentity OPERATOR(pg_catalog.<>) '' AS identity, \
                   a.attgenerated OPERATOR(pg_catalog.<>) '' AS generated, a.attislocal AS local, \
                   (SELECT cmt.description FROM pg_catalog.pg_description cmt \
                    WHERE cmt.objoid OPERATOR(pg_catalog.=) t.oid \
                    AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass \
                    AND cmt.objsubid OPERATOR(pg_catalog.=) a.attnum) AS comment \
                 FROM pg_catalog.pg_attribute a {join} \
                 WHERE a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped \
                   AND t.relkind OPERATOR(pg_catalog.=) ANY (ARRAY['r', 'p', 'v', 'm', 'f']::pg_catalog.\"char\"[]) \
                   AND {where_}",
                join = rel_join("a.attrelid"),
                where_ = filter.sql("t", "n", Some("a.attname"))?,
            );
            let rows = fetch(session, Pin::Catalog, &sql, "q.position").await?;
            rows.iter()
                .map(|row| {
                    Ok(ChildSummary {
                        name: text(&row, "name")?,
                        comment: opt_text(&row, "comment")?,
                        detail: Some(text(&row, "data_type")?),
                        flags: on(&[
                            (flag(&row, "not_null")?, "not null"),
                            (flag(&row, "identity")?, "identity"),
                            (flag(&row, "generated")?, "generated"),
                            (!flag(&row, "local")?, "inherited"),
                        ]),
                        system: false,
                    })
                })
                .collect()
        }
        ObjectKind::Index => {
            let sql = format!(
                "SELECT ic.relname AS name, am.amname AS method, i.indisunique AS is_unique, \
                   i.indisprimary AS is_primary, i.indisvalid AS valid, i.indisclustered AS clustered, \
                   ic.relkind OPERATOR(pg_catalog.=) 'I' AS partitioned, \
                   i.indpred IS NOT NULL AS partial, i.indexprs IS NOT NULL AS expression, \
                   ic.relpages::pg_catalog.int8 OPERATOR(pg_catalog.*) {BLOCK} AS size_bytes, \
                   {comment} AS comment, {system} AS system \
                 FROM pg_catalog.pg_index i \
                 JOIN pg_catalog.pg_class ic ON ic.oid OPERATOR(pg_catalog.=) i.indexrelid {join} \
                 JOIN pg_catalog.pg_am am ON am.oid OPERATOR(pg_catalog.=) ic.relam \
                 WHERE {where_}",
                comment = comment("ic.oid", "pg_catalog.pg_class"),
                system = system_relation("t", "n"),
                join = rel_join("i.indrelid"),
                where_ = filter.sql("t", "n", Some("ic.relname"))?,
            );
            let rows = fetch(session, Pin::Catalog, &sql, "q.name").await?;
            rows.iter()
                .map(|row| {
                    let method = text(&row, "method")?;
                    let partitioned = flag(&row, "partitioned")?;
                    let primary = flag(&row, "is_primary")?;
                    let detail = match opt_int(&row, "size_bytes")? {
                        Some(size) if !partitioned => format!("{method}, {}", pretty_bytes(size)),
                        _ => method,
                    };
                    Ok(ChildSummary {
                        name: text(&row, "name")?,
                        comment: opt_text(&row, "comment")?,
                        detail: Some(detail),
                        flags: on(&[
                            (primary, "primary"),
                            (flag(&row, "is_unique")? && !primary, "unique"),
                            (flag(&row, "partial")?, "partial"),
                            (flag(&row, "expression")?, "expression"),
                            (!flag(&row, "valid")?, "invalid"),
                            (flag(&row, "clustered")?, "clustered"),
                        ]),
                        system: flag(&row, "system")?,
                    })
                })
                .collect()
        }
        ObjectKind::Constraint => {
            let enforced = if server.at_least(18, 0) {
                "k.conenforced"
            } else {
                "true"
            };
            let names = |array: &str, rel: &str| {
                format!(
                    "ARRAY(SELECT a.attname::pg_catalog.text FROM pg_catalog.unnest({array}) \
                     WITH ORDINALITY x(num, ord) JOIN pg_catalog.pg_attribute a \
                     ON a.attrelid OPERATOR(pg_catalog.=) {rel} AND a.attnum OPERATOR(pg_catalog.=) x.num \
                     ORDER BY x.ord)"
                )
            };
            let sql = format!(
                "SELECT k.conname AS name, k.contype::pg_catalog.text AS contype, \
                   {columns} AS columns, rn.nspname AS ref_schema, rt.relname AS ref_table, \
                   {ref_columns} AS ref_columns, k.convalidated AS validated, {enforced} AS enforced, \
                   k.condeferrable AS deferrable, k.conislocal AS local, k.connoinherit AS no_inherit, \
                   {comment} AS comment, \
                   CASE WHEN k.contype OPERATOR(pg_catalog.=) 'p' THEN 0 \
                     WHEN k.contype OPERATOR(pg_catalog.=) 'u' THEN 1 \
                     WHEN k.contype OPERATOR(pg_catalog.=) 'x' THEN 2 \
                     WHEN k.contype OPERATOR(pg_catalog.=) 'c' THEN 3 \
                     WHEN k.contype OPERATOR(pg_catalog.=) 'n' THEN 4 ELSE 5 END AS type_order \
                 FROM pg_catalog.pg_constraint k {join} \
                 LEFT JOIN pg_catalog.pg_class rt ON k.contype OPERATOR(pg_catalog.=) 'f' \
                   AND rt.oid OPERATOR(pg_catalog.=) k.confrelid \
                 LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid OPERATOR(pg_catalog.=) rt.relnamespace \
                 WHERE k.contype OPERATOR(pg_catalog.<>) 't' AND {NOT_A_CLONE} AND {where_}",
                columns = CONSTRAINT_COLUMNS,
                ref_columns = names("k.confkey", "k.confrelid"),
                comment = comment("k.oid", "pg_catalog.pg_constraint"),
                join = rel_join("k.conrelid"),
                where_ = filter.sql("t", "n", Some("k.conname"))?,
            );
            let rows = fetch(session, Pin::Catalog, &sql, "q.type_order, q.name").await?;
            rows.iter()
                .map(|row| {
                    let columns = text_list(&row, "columns")?.join(", ");
                    let detail = match text(&row, "contype")?.as_str() {
                        "p" => format!("primary key ({columns})"),
                        "u" => format!("unique ({columns})"),
                        "x" => format!("exclusion ({columns})"),
                        "n" => format!("not null ({columns})"),
                        "c" => "check".to_owned(),
                        "f" => format!(
                            "foreign key ({columns}) references {}.{} ({})",
                            opt_text(&row, "ref_schema")?.unwrap_or_default(),
                            opt_text(&row, "ref_table")?.unwrap_or_default(),
                            text_list(&row, "ref_columns")?.join(", ")
                        ),
                        other => other.to_owned(),
                    };
                    Ok(ChildSummary {
                        name: text(&row, "name")?,
                        comment: opt_text(&row, "comment")?,
                        detail: Some(detail),
                        flags: on(&[
                            (!flag(&row, "validated")?, "not valid"),
                            (!flag(&row, "enforced")?, "not enforced"),
                            (flag(&row, "deferrable")?, "deferrable"),
                            (!flag(&row, "local")?, "inherited"),
                            // The server marks primary keys, unique,
                            // exclusion and foreign keys of plain tables no
                            // inherit itself; only CHECK and NOT NULL take
                            // the clause.
                            (
                                flag(&row, "no_inherit")?
                                    && matches!(text(&row, "contype")?.as_str(), "c" | "n"),
                                "no inherit",
                            ),
                        ]),
                        system: false,
                    })
                })
                .collect()
        }
        ObjectKind::Trigger => {
            let sql = format!(
                "SELECT g.tgname AS name, g.tgtype::pg_catalog.int8 AS tgtype, \
                   g.tgenabled::pg_catalog.text AS enabled, \
                   g.tgconstraint OPERATOR(pg_catalog.<>) 0 AS constraint_trigger, \
                   g.tgparentid OPERATOR(pg_catalog.<>) 0 AS inherited, {comment} AS comment \
                 FROM pg_catalog.pg_trigger g {join} \
                 WHERE NOT g.tgisinternal AND {where_}",
                comment = comment("g.oid", "pg_catalog.pg_trigger"),
                join = rel_join("g.tgrelid"),
                where_ = filter.sql("t", "n", Some("g.tgname"))?,
            );
            let rows = fetch(session, Pin::Catalog, &sql, "q.name").await?;
            rows.iter()
                .map(|row| {
                    let tgtype = int(&row, "tgtype")?;
                    let events: Vec<&str> = [
                        (4, "insert"),
                        (16, "update"),
                        (8, "delete"),
                        (32, "truncate"),
                    ]
                    .into_iter()
                    .filter(|(bit, _)| tgtype & bit != 0)
                    .map(|(_, name)| name)
                    .collect();
                    let timing = if tgtype & 64 != 0 {
                        "instead of"
                    } else if tgtype & 2 != 0 {
                        "before"
                    } else {
                        "after"
                    };
                    let level = if tgtype & 1 != 0 { "row" } else { "statement" };
                    let enabled = enabled_state(&text(&row, "enabled")?);
                    Ok(ChildSummary {
                        name: text(&row, "name")?,
                        comment: opt_text(&row, "comment")?,
                        detail: Some(format!(
                            "{timing} {}, for each {level}",
                            events.join(" or ")
                        )),
                        flags: on(&[
                            (enabled == "disabled", "disabled"),
                            (enabled == "replica", "replica"),
                            (enabled == "always", "always"),
                            (flag(&row, "constraint_trigger")?, "constraint"),
                            (flag(&row, "inherited")?, "inherited"),
                        ]),
                        system: false,
                    })
                })
                .collect()
        }
        ObjectKind::Rule => {
            let sql = format!(
                "SELECT r.rulename AS name, r.ev_type::pg_catalog.text AS event, \
                   r.is_instead AS instead, r.ev_enabled::pg_catalog.text AS enabled, \
                   {comment} AS comment \
                 FROM pg_catalog.pg_rewrite r {join} \
                 WHERE r.rulename OPERATOR(pg_catalog.<>) '_RETURN' AND {where_}",
                comment = comment("r.oid", "pg_catalog.pg_rewrite"),
                join = rel_join("r.ev_class"),
                where_ = filter.sql("t", "n", Some("r.rulename"))?,
            );
            let rows = fetch(session, Pin::Catalog, &sql, "q.name").await?;
            rows.iter()
                .map(|row| {
                    let event = match text(&row, "event")?.as_str() {
                        "1" => "select",
                        "2" => "update",
                        "3" => "insert",
                        _ => "delete",
                    };
                    Ok(ChildSummary {
                        name: text(&row, "name")?,
                        comment: opt_text(&row, "comment")?,
                        detail: Some(format!("on {event}")),
                        flags: on(&[
                            (flag(&row, "instead")?, "instead"),
                            (
                                enabled_state(&text(&row, "enabled")?) == "disabled",
                                "disabled",
                            ),
                        ]),
                        system: false,
                    })
                })
                .collect()
        }
        ObjectKind::Policy => {
            let sql = format!(
                "SELECT p.polname AS name, p.polcmd::pg_catalog.text AS command, \
                   p.polpermissive AS permissive, {comment} AS comment \
                 FROM pg_catalog.pg_policy p {join} WHERE {where_}",
                comment = comment("p.oid", "pg_catalog.pg_policy"),
                join = rel_join("p.polrelid"),
                where_ = filter.sql("t", "n", Some("p.polname"))?,
            );
            let rows = fetch(session, Pin::Catalog, &sql, "q.name").await?;
            rows.iter()
                .map(|row| {
                    Ok(ChildSummary {
                        name: text(&row, "name")?,
                        comment: opt_text(&row, "comment")?,
                        detail: Some(policy_command(&text(&row, "command")?).to_owned()),
                        flags: on(&[(!flag(&row, "permissive")?, "restrictive")]),
                        system: false,
                    })
                })
                .collect()
        }
        other => Err(crate::model::CatalogError::Invalid {
            message: format!("{other:?} is no child list of a relation"),
        }),
    }
}

/// The command a policy applies to, from `polcmd`.
fn policy_command(code: &str) -> &'static str {
    match code {
        "r" => "select",
        "a" => "insert",
        "w" => "update",
        "d" => "delete",
        _ => "all",
    }
}

// ---------------------------------------------------------------------------
// Sequences

/// Which sequences [`sequences`] returns.
#[derive(Debug, Clone, Copy)]
pub(super) enum SeqFilter<'a> {
    /// Sequences of a schema, identity sequences excluded.
    Schema(&'a str),
    /// One sequence (identity sequences included).
    One(&'a str, &'a str),
    /// Sequences owned (`OWNED BY`, not identity) by a column of a table.
    OwnedBy(&'a str, &'a str),
}

/// Sequences.
pub(super) async fn sequences(
    session: &dyn Session,
    pin: Pin,
    filter: SeqFilter<'_>,
    include_system: bool,
) -> CatalogResult<Vec<PgSequence>> {
    let system = system_relation("c", "n");
    let where_ = match filter {
        SeqFilter::Schema(schema) => format!(
            "n.nspname OPERATOR(pg_catalog.=) {} AND COALESCE(dep.deptype, 'a') OPERATOR(pg_catalog.<>) 'i'{}",
            lit(schema)?,
            system_filter(include_system, &system)
        ),
        SeqFilter::One(schema, name) => {
            format!(
                "n.nspname OPERATOR(pg_catalog.=) {} AND c.relname OPERATOR(pg_catalog.=) {}",
                lit(schema)?,
                lit(name)?
            )
        }
        SeqFilter::OwnedBy(schema, table) => format!(
            "dep.deptype OPERATOR(pg_catalog.=) 'a' AND otn.nspname OPERATOR(pg_catalog.=) {} AND ot.relname OPERATOR(pg_catalog.=) {}",
            lit(schema)?,
            lit(table)?
        ),
    };
    // The last value reads the sequence and waits behind a session holding
    // it in ACCESS EXCLUSIVE mode: only the details of sequences (one
    // sequence, or those owned by one table) read it, never the list of a
    // schema. It is computed for the matched sequences only, with the
    // guards of the `pg_sequences` view; joining that view would compute it
    // for every sequence of the database first.
    let readable = match filter {
        SeqFilter::Schema(_) => "false",
        SeqFilter::One(..) | SeqFilter::OwnedBy(..) => {
            "(NOT pg_catalog.pg_is_other_temp_schema(c.relnamespace) \
               AND pg_catalog.has_sequence_privilege(c.oid, 'SELECT'))"
        }
    };
    let sql = format!(
        "SELECT n.nspname AS schema, c.relname AS name, pg_catalog.pg_get_userbyid(c.relowner) AS owner, \
           pg_catalog.format_type(s.seqtypid, NULL) AS data_type, s.seqstart AS start, \
           s.seqincrement AS increment, s.seqmin AS min, s.seqmax AS max, s.seqcache AS cache, \
           s.seqcycle AS cycle, {readable} AS readable, c.relpersistence OPERATOR(pg_catalog.=) 'u' AS unlogged, \
           otn.nspname AS owner_schema, ot.relname AS owner_table, oa.attname AS owner_column, \
           CASE WHEN dep.deptype OPERATOR(pg_catalog.=) 'i' THEN oa.attidentity::pg_catalog.text END AS identity, \
           (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) c.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {system} AS system \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
         JOIN pg_catalog.pg_sequence s ON s.seqrelid OPERATOR(pg_catalog.=) c.oid \
         LEFT JOIN pg_catalog.pg_depend dep ON dep.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass \
           AND dep.objid OPERATOR(pg_catalog.=) c.oid AND dep.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass \
           AND dep.refobjsubid OPERATOR(pg_catalog.>) 0 AND dep.deptype OPERATOR(pg_catalog.=) ANY (ARRAY['a', 'i']::pg_catalog.\"char\"[]) \
         LEFT JOIN pg_catalog.pg_class ot ON ot.oid OPERATOR(pg_catalog.=) dep.refobjid \
         LEFT JOIN pg_catalog.pg_namespace otn ON otn.oid OPERATOR(pg_catalog.=) ot.relnamespace \
         LEFT JOIN pg_catalog.pg_attribute oa ON oa.attrelid OPERATOR(pg_catalog.=) dep.refobjid AND oa.attnum OPERATOR(pg_catalog.=) dep.refobjsubid \
         WHERE c.relkind OPERATOR(pg_catalog.=) 'S' AND {where_} ORDER BY n.nspname, c.relname"
    );
    let rows = fetch(session, pin, &sql, "q.schema, q.name").await?;
    let mut list = rows
        .iter()
        .map(|row| {
            let owned_by = match (
                opt_text(&row, "owner_schema")?,
                opt_text(&row, "owner_table")?,
                opt_text(&row, "owner_column")?,
            ) {
                (Some(schema), Some(table), Some(column)) => Some(PgOwnedBy {
                    schema,
                    table,
                    column,
                }),
                _ => None,
            };
            Ok(PgSequence {
                schema: text(&row, "schema")?,
                name: text(&row, "name")?,
                owner: text(&row, "owner")?,
                data_type: text(&row, "data_type")?,
                start: int(&row, "start")?,
                increment: int(&row, "increment")?,
                min: int(&row, "min")?,
                max: int(&row, "max")?,
                cache: int(&row, "cache")?,
                cycle: flag(&row, "cycle")?,
                // Read below from the sequence itself.
                last_value: None,
                is_called: None,
                unlogged: flag(&row, "unlogged")?,
                owned_by,
                identity: non_empty(&row, "identity")?
                    .map(|kind| if kind == "a" { "always" } else { "byDefault" }.to_owned()),
                comment: opt_text(&row, "comment")?,
                system: flag(&row, "system")?,
            })
        })
        .collect::<CatalogResult<Vec<_>>>()?;
    let readable: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, row)| match flag(&row, "readable") {
            Ok(true) => Some(Ok(i)),
            Ok(false) => None,
            Err(e) => Some(Err(e)),
        })
        .collect::<CatalogResult<_>>()?;
    if readable.is_empty() {
        return Ok(list);
    }
    // `last_value` and `is_called` come from the sequence relation: the
    // `pg_sequences` view (and `pg_sequence_last_value`) report NULL until
    // `is_called`, which is also the state after RESTART or setval(.., false),
    // where `last_value` is the value the next nextval returns.
    let mut parts = Vec::with_capacity(readable.len());
    for i in &readable {
        let Some(seq) = list.get(*i) else { continue };
        parts.push(format!(
            "SELECT {i}::pg_catalog.int4 AS i, last_value::pg_catalog.int8 AS last_value, is_called FROM {}",
            crate::sql::qualified(varve_driver::Dialect::Postgres, &[Some(seq.schema.as_str()), Some(seq.name.as_str())])?
        ));
    }
    let values = fetch(session, pin, &parts.join(" UNION ALL "), "q.i").await?;
    for row in values.iter() {
        let i = int(&row, "i")?;
        if let Some(seq) = usize::try_from(i).ok().and_then(|i| list.get_mut(i)) {
            seq.last_value = opt_int(&row, "last_value")?;
            seq.is_called = row.bool("is_called")?;
        }
    }
    Ok(list)
}

/// An index attached (directly or further down) to a partitioned index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IndexChild {
    /// The index.
    pub(super) index: PgQualifiedName,
    /// The partitioned index it is attached to.
    pub(super) parent: PgQualifiedName,
    /// `pg_get_indexdef` of the index (`ON ONLY` when it is partitioned
    /// itself).
    pub(super) definition: String,
    /// Tablespace; `None` for the database default.
    pub(super) tablespace: Option<String>,
    /// Comment.
    pub(super) comment: Option<String>,
}

/// Every index attached to the partitioned index `schema.name`, parents
/// before their children.
pub(super) async fn index_children(
    session: &dyn Session,
    pin: Pin,
    schema: &str,
    name: &str,
) -> CatalogResult<Vec<IndexChild>> {
    let sql = format!(
        "WITH RECURSIVE tree AS ( \
           SELECT ri.oid, NULL::pg_catalog.oid AS parent, 0 AS depth FROM pg_catalog.pg_class ri \
           JOIN pg_catalog.pg_namespace rn ON rn.oid OPERATOR(pg_catalog.=) ri.relnamespace \
           WHERE rn.nspname OPERATOR(pg_catalog.=) {} AND ri.relname OPERATOR(pg_catalog.=) {} AND ri.relkind OPERATOR(pg_catalog.=) 'I' \
           UNION ALL \
           SELECT h.inhrelid, h.inhparent, tree.depth OPERATOR(pg_catalog.+) 1 FROM pg_catalog.pg_inherits h \
           JOIN tree ON h.inhparent OPERATOR(pg_catalog.=) tree.oid) \
         SELECT n.nspname AS schema, c.relname AS name, pn.nspname AS parent_schema, \
           p.relname AS parent_name, tree.depth, \
           pg_catalog.pg_get_indexdef(c.oid, 0, true) AS definition, ts.spcname AS tablespace, \
           (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) c.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment \
         FROM tree JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) tree.oid \
         JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
         JOIN pg_catalog.pg_class p ON p.oid OPERATOR(pg_catalog.=) tree.parent \
         JOIN pg_catalog.pg_namespace pn ON pn.oid OPERATOR(pg_catalog.=) p.relnamespace \
         LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid OPERATOR(pg_catalog.=) c.reltablespace \
         WHERE tree.depth OPERATOR(pg_catalog.>) 0",
        lit(schema)?,
        lit(name)?
    );
    let rows = fetch(session, pin, &sql, "q.depth, q.schema, q.name").await?;
    rows.iter()
        .map(|row| {
            Ok(IndexChild {
                index: PgQualifiedName {
                    schema: text(&row, "schema")?,
                    name: text(&row, "name")?,
                },
                parent: PgQualifiedName {
                    schema: text(&row, "parent_schema")?,
                    name: text(&row, "parent_name")?,
                },
                definition: text(&row, "definition")?,
                tablespace: opt_text(&row, "tablespace")?,
                comment: opt_text(&row, "comment")?,
            })
        })
        .collect()
}
