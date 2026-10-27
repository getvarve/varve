//! What dropping an object affects, shown before the user confirms a drop
//! in an object editor (design doc 4.2, "E" rows): the objects that depend
//! on it, what it holds, and for a database the sessions connected to it
//! and what makes the server refuse the drop whatever those sessions do.
//!
//! Every list is one catalog query on the metadata session (no query per
//! object). Names and values are quoted through [`varve_catalog::sql`].

use serde::{Deserialize, Serialize};
use varve_catalog::rows::{self, Rows};
use varve_catalog::sql;
use varve_catalog::{CatalogError, CatalogResult, ObjectKind, ObjectRef};
use varve_core::Engine;
use varve_driver::{Dialect, ServerInfo, Session};
use varve_live_stats::sql::{LiveError, process_list};

/// The note on a MySQL or MariaDB database drop while binary logging is
/// on (left out when the dropping session has `sql_log_bin` off).
pub const BINLOG_NOTE: &str = "Binary logging is on: replicas of this server drop the database \
     too when the statement replicates, and their sessions on it lose it.";

/// At most this many dependents are listed; [`DropImpact::dependents_truncated`]
/// says when there are more.
pub const MAX_DEPENDENTS: usize = 200;

/// How dropping an object affects one of its dependents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DependentEffect {
    /// The server refuses a plain drop while it exists; PostgreSQL's
    /// `CASCADE` drops it too (MySQL and MariaDB have no `CASCADE`).
    Blocks,
    /// It stays but stops working (a MySQL or MariaDB view over a dropped
    /// table).
    Breaks,
    /// Its body names the dropped object, so it may stop working (MySQL and
    /// MariaDB routines, triggers and events: the server does not track
    /// what they use, so this is found by name and can be a false match).
    MayBreak,
}

/// A session of this app (a tab) on the same connection that uses the
/// database being dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSession {
    /// The session id (the UI finds its tab by it).
    pub session_id: String,
    /// A statement is running on it now.
    pub busy: bool,
    /// It has an open transaction (lost when the session is terminated or
    /// the database disappears); `None` when unknown (busy, or the driver
    /// cannot tell).
    pub open_transaction: Option<bool>,
    /// True when it is known to be on the same server; false when it only
    /// may be (its server could not be identified, the endpoint matches,
    /// or only a weak MariaDB identity matches).
    pub certain: bool,
    /// It is on a PostgreSQL streaming standby of the server: the drop
    /// replays there and ends it, with or without FORCE; it never makes the
    /// primary refuse the drop.
    #[serde(default)]
    pub standby: bool,
}

/// An object that depends on the one being dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dependent {
    /// The object with its qualified name, for example
    /// `view public.order_totals` or (PostgreSQL)
    /// `table constraint orders_customer_fk on public.orders`.
    pub description: String,
    /// What the drop does to it.
    pub effect: DependentEffect,
}

/// How many objects of one kind the dropped object holds (they go with it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentCount {
    /// Plural label, for example `tables` or `triggers`.
    pub label: String,
    /// The count as decimal text (the UI's contract for integers that are
    /// not bounded by design).
    pub count: String,
}

/// Another session connected to a database that is about to be dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveSession {
    /// Backend process id (PostgreSQL) or connection id (MySQL, MariaDB),
    /// as decimal text.
    pub id: String,
    /// User name.
    pub user: Option<String>,
    /// Client application (PostgreSQL `application_name`).
    pub application: Option<String>,
    /// Client address as the server reports it.
    pub client: Option<String>,
    /// What the session is doing (`active`, `idle in transaction`, MySQL
    /// command and state).
    pub state: Option<String>,
    /// True when the application name says it is a Varve session (this app
    /// or another Varve instance).
    pub varve: bool,
}

/// What dropping an object affects.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropImpact {
    /// Objects outside the dropped one that depend on it (PostgreSQL:
    /// transitively, so everything `CASCADE` would also drop).
    pub dependents: Vec<Dependent>,
    /// True when there were more than [`MAX_DEPENDENTS`] dependents.
    pub dependents_truncated: bool,
    /// What the object holds and loses with it, by kind.
    pub contents: Vec<ContentCount>,
    /// Other sessions connected to the database (database drops only; this
    /// lookup's own connection is left out).
    pub sessions: Vec<ActiveSession>,
    /// False when the account may not see every session (MySQL and
    /// MariaDB without the `PROCESS` privilege), so the list may be short.
    pub sessions_complete: bool,
    /// Reasons the server refuses the drop no matter which sessions are
    /// connected (PostgreSQL prepared transactions, active logical
    /// replication slots, subscriptions; a MySQL read-only database).
    pub blockers: Vec<String>,
    /// This app's other sessions (tabs) of the same connection on the
    /// database being dropped.
    #[serde(default)]
    pub app_sessions: Vec<AppSession>,
    /// What the lookup could not see (MySQL and MariaDB routine bodies the
    /// account may not read).
    #[serde(default)]
    pub notes: Vec<String>,
}

fn pg_string(text: &str) -> CatalogResult<String> {
    sql::string(Dialect::Postgres, text)
}

fn my_string(text: &str) -> CatalogResult<String> {
    sql::string(Dialect::Mysql, text)
}

fn required<'a>(value: Option<&'a str>, what: &str) -> CatalogResult<&'a str> {
    value.ok_or_else(|| CatalogError::Invalid {
        message: format!("the object needs its {what}"),
    })
}

/// What dropping `object` affects on this server.
///
/// # Errors
/// [`CatalogError::Invalid`] when the reference lacks its schema or
/// database, driver errors of the lookups.
pub async fn drop_impact(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<DropImpact> {
    match server.engine {
        Engine::Postgres => pg_impact(session, server, object).await,
        Engine::Mysql | Engine::Mariadb => my_impact(session, server, object).await,
        Engine::Redis => Err(CatalogError::Unsupported {
            message: "Redis has no catalog objects to drop".into(),
        }),
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL
// ---------------------------------------------------------------------------

/// The `(classid, objid)` pairs of the objects being dropped, as SQL that
/// returns them; `None` for kinds nothing can depend on.
fn pg_seed(object: &ObjectRef) -> CatalogResult<Option<String>> {
    use ObjectKind as K;
    let name = object.name.as_str();
    let seed = match object.kind {
        K::Table
        | K::PartitionedTable
        | K::Partition
        | K::View
        | K::MaterializedView
        | K::Sequence
        | K::ForeignTable => {
            let schema = required(object.schema.as_deref(), "schema")?;
            format!(
                "SELECT 'pg_catalog.pg_class'::pg_catalog.regclass::oid AS classid, c.oid AS objid, 0 AS objsubid \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
                 WHERE n.nspname OPERATOR(pg_catalog.=) {} AND c.relname OPERATOR(pg_catalog.=) {}",
                pg_string(schema)?,
                pg_string(name)?
            )
        }
        K::Function | K::Procedure | K::Aggregate => {
            let schema = required(object.schema.as_deref(), "schema")?;
            let qualified = sql::qualified(Dialect::Postgres, &[Some(schema), Some(name)])?;
            let signature = object.signature.as_deref().unwrap_or("");
            format!(
                "SELECT 'pg_catalog.pg_proc'::pg_catalog.regclass::oid AS classid, p.oid AS objid, 0 AS objsubid \
                 FROM pg_catalog.pg_proc p \
                 WHERE p.oid OPERATOR(pg_catalog.=) pg_catalog.to_regprocedure({})",
                pg_string(&format!("{qualified}({signature})"))?
            )
        }
        K::EnumType | K::CompositeType | K::RangeType | K::Domain => {
            let schema = required(object.schema.as_deref(), "schema")?;
            format!(
                "SELECT 'pg_catalog.pg_type'::pg_catalog.regclass::oid AS classid, t.oid AS objid, 0 AS objsubid \
                 FROM pg_catalog.pg_type t \
                 JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.typnamespace \
                 WHERE n.nspname OPERATOR(pg_catalog.=) {} AND t.typname OPERATOR(pg_catalog.=) {}",
                pg_string(schema)?,
                pg_string(name)?
            )
        }
        K::Extension => format!(
            "SELECT 'pg_catalog.pg_extension'::pg_catalog.regclass::oid AS classid, e.oid AS objid, 0 AS objsubid \
             FROM pg_catalog.pg_extension e WHERE e.extname OPERATOR(pg_catalog.=) {}",
            pg_string(name)?
        ),
        // The schema itself: what it holds is reached through its
        // dependencies and left out of the list (it is inside the schema).
        K::Schema => format!(
            "SELECT 'pg_catalog.pg_namespace'::pg_catalog.regclass::oid AS classid, n.oid AS objid, \
             0 AS objsubid FROM pg_catalog.pg_namespace n WHERE n.nspname OPERATOR(pg_catalog.=) {}",
            pg_string(name)?
        ),
        _ => return Ok(None),
    };
    Ok(Some(seed))
}

/// What `DROP ... CASCADE` drops besides the object itself, found the way
/// PostgreSQL finds it (`findDependentObjects`): a walk over `pg_depend`
/// from the object, where
/// - auto (`a`) and internal (`i`) dependents of the object are parts of
///   it (its row type and array type, indexes, constraints, triggers,
///   rules, policies, owned sequences, column defaults) and go with it
///   without being listed, and the walk goes on from them;
/// - normal (`n`) dependents of the object or of its parts are what
///   `RESTRICT` refuses for and `CASCADE` drops: they are listed and the
///   walk goes on from them and their parts;
/// - an object reached that is an internal part of another object (a
///   view's `_RETURN` rule) stands for that object;
/// - column-level edges keep their column (`objsubid`), so a column of
///   another table whose type is dropped is listed as that column, not
///   as the table.
///
/// Descriptions come from `pg_describe_object`, with `search_path` set to
/// `pg_catalog` for this statement only so every name is qualified (the
/// format of PostgreSQL's own "depends on" messages). For a schema,
/// objects inside it are left out (they are its contents).
fn pg_dependents_sql(seed: &str, schema: Option<&str>) -> CatalogResult<String> {
    // A schema's own objects are its contents, not dependents. Compared by
    // namespace oid (a quoted name never matches its raw form), and parts
    // without a schema of their own (column defaults, triggers, rules,
    // policies, constraints) by the namespace of the object they belong to.
    let outside = match schema {
        Some(schema) => format!(
            "AND ({}) IS DISTINCT FROM (SELECT n.oid FROM pg_catalog.pg_namespace n \
             WHERE n.nspname OPERATOR(pg_catalog.=) {})",
            owner_namespace_sql("f"),
            pg_string(schema)?
        ),
        None => String::new(),
    };
    Ok(format!(
        "WITH RECURSIVE target(classid, objid, objsubid) AS ({seed}), \
         walk(classid, objid, objsubid, part, listed) AS ( \
           SELECT classid, objid, objsubid, true, false FROM target \
           UNION \
           SELECT CASE WHEN o.refobjid IS NULL THEN d.classid ELSE o.refclassid END, \
                  CASE WHEN o.refobjid IS NULL THEN d.objid ELSE o.refobjid END, \
                  CASE WHEN o.refobjid IS NULL THEN d.objsubid ELSE 0 END, \
                  w.part AND d.deptype OPERATOR(pg_catalog.<>) 'n' AND o.refobjid IS NULL, \
                  d.deptype OPERATOR(pg_catalog.=) 'n' \
           FROM walk w \
           JOIN pg_catalog.pg_depend d ON d.refclassid OPERATOR(pg_catalog.=) w.classid \
             AND d.refobjid OPERATOR(pg_catalog.=) w.objid \
             AND (w.objsubid OPERATOR(pg_catalog.=) 0 OR d.refobjsubid OPERATOR(pg_catalog.=) w.objsubid) \
             AND d.deptype OPERATOR(pg_catalog.=) ANY (ARRAY['n', 'a', 'i']::\"char\"[]) \
           LEFT JOIN LATERAL ( \
             SELECT i.refclassid, i.refobjid FROM pg_catalog.pg_depend i \
             WHERE i.classid OPERATOR(pg_catalog.=) d.classid AND i.objid OPERATOR(pg_catalog.=) d.objid \
               AND i.deptype OPERATOR(pg_catalog.=) 'i' AND d.deptype OPERATOR(pg_catalog.<>) 'i' \
               AND d.objsubid OPERATOR(pg_catalog.=) 0 \
             LIMIT 1 \
           ) o ON true \
         ), \
         found AS (SELECT DISTINCT classid, objid, objsubid, part, listed FROM walk) \
         SELECT DISTINCT \
           (f.objsubid OPERATOR(pg_catalog.<>) 0 \
             OR f.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_attrdef'::pg_catalog.regclass::pg_catalog.oid) \
             AS sub_object, \
           pg_catalog.pg_describe_object(f.classid, f.objid, f.objsubid) AS description \
         FROM found f \
         WHERE f.listed \
           AND (SELECT pg_catalog.set_config('search_path', 'pg_catalog', true)) IS NOT NULL \
           AND NOT EXISTS (SELECT 1 FROM found p WHERE p.part \
             AND p.classid OPERATOR(pg_catalog.=) f.classid AND p.objid OPERATOR(pg_catalog.=) f.objid \
             AND (p.objsubid OPERATOR(pg_catalog.=) 0 OR p.objsubid OPERATOR(pg_catalog.=) f.objsubid)) \
           AND NOT (f.objsubid OPERATOR(pg_catalog.<>) 0 AND EXISTS (SELECT 1 FROM found g \
             WHERE g.listed AND g.objsubid OPERATOR(pg_catalog.=) 0 \
             AND g.classid OPERATOR(pg_catalog.=) f.classid AND g.objid OPERATOR(pg_catalog.=) f.objid)) \
           {outside} \
         ORDER BY sub_object, description LIMIT {}",
        MAX_DEPENDENTS + 1
    ))
}

/// The namespace oid of an object of the walk (alias `f`): its own, or for
/// parts without one the namespace of the relation they belong to.
fn owner_namespace_sql(f: &str) -> String {
    let class = |name: &str| format!("'pg_catalog.{name}'::pg_catalog.regclass::pg_catalog.oid");
    let rel = |table: &str, key: &str| {
        format!(
            "(SELECT c.relnamespace FROM pg_catalog.{table} x \
             JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) x.{key} \
             WHERE x.oid OPERATOR(pg_catalog.=) {f}.objid)"
        )
    };
    format!(
        "CASE \
         WHEN {f}.classid OPERATOR(pg_catalog.=) {pg_class} THEN (SELECT c.relnamespace \
           FROM pg_catalog.pg_class c WHERE c.oid OPERATOR(pg_catalog.=) {f}.objid) \
         WHEN {f}.classid OPERATOR(pg_catalog.=) {pg_attrdef} THEN {attrdef} \
         WHEN {f}.classid OPERATOR(pg_catalog.=) {pg_trigger} THEN {trigger} \
         WHEN {f}.classid OPERATOR(pg_catalog.=) {pg_rewrite} THEN {rewrite} \
         WHEN {f}.classid OPERATOR(pg_catalog.=) {pg_policy} THEN {policy} \
         WHEN {f}.classid OPERATOR(pg_catalog.=) {pg_constraint} THEN (SELECT x.connamespace \
           FROM pg_catalog.pg_constraint x WHERE x.oid OPERATOR(pg_catalog.=) {f}.objid) \
         WHEN {f}.classid OPERATOR(pg_catalog.=) {pg_proc} THEN (SELECT x.pronamespace \
           FROM pg_catalog.pg_proc x WHERE x.oid OPERATOR(pg_catalog.=) {f}.objid) \
         WHEN {f}.classid OPERATOR(pg_catalog.=) {pg_type} THEN (SELECT x.typnamespace \
           FROM pg_catalog.pg_type x WHERE x.oid OPERATOR(pg_catalog.=) {f}.objid) \
         ELSE (SELECT n.oid FROM pg_catalog.pg_namespace n \
           WHERE pg_catalog.quote_ident(n.nspname) OPERATOR(pg_catalog.=) \
           (pg_catalog.pg_identify_object({f}.classid, {f}.objid, 0)).schema) \
         END",
        pg_class = class("pg_class"),
        pg_attrdef = class("pg_attrdef"),
        pg_trigger = class("pg_trigger"),
        pg_rewrite = class("pg_rewrite"),
        pg_policy = class("pg_policy"),
        pg_constraint = class("pg_constraint"),
        pg_proc = class("pg_proc"),
        pg_type = class("pg_type"),
        attrdef = rel("pg_attrdef", "adrelid"),
        trigger = rel("pg_trigger", "tgrelid"),
        rewrite = rel("pg_rewrite", "ev_class"),
        policy = rel("pg_policy", "polrelid"),
    )
}

fn dependents_from(rows: &Rows, effect: DependentEffect) -> CatalogResult<(Vec<Dependent>, bool)> {
    let mut out = Vec::new();
    for row in rows.iter() {
        if let Some(description) = row.text("description")? {
            out.push(Dependent {
                description,
                effect,
            });
        }
    }
    let truncated = out.len() > MAX_DEPENDENTS;
    out.truncate(MAX_DEPENDENTS);
    Ok((out, truncated))
}

fn contents_from(rows: &Rows) -> CatalogResult<Vec<ContentCount>> {
    let mut out = Vec::new();
    for row in rows.iter() {
        let label = row.req_text("label")?;
        let count = row.req_text("count")?;
        if count != "0" {
            out.push(ContentCount { label, count });
        }
    }
    Ok(out)
}

/// The kinds of objects in a schema, counted in one query.
fn pg_schema_contents_sql(schema: &str) -> CatalogResult<String> {
    let nsp = format!(
        "(SELECT oid FROM pg_catalog.pg_namespace WHERE nspname OPERATOR(pg_catalog.=) {})",
        pg_string(schema)?
    );
    Ok(format!(
        "SELECT label, pg_catalog.count(*)::pg_catalog.text AS count FROM ( \
           SELECT CASE c.relkind WHEN 'r' THEN 'tables' WHEN 'p' THEN 'tables' \
             WHEN 'v' THEN 'views' WHEN 'm' THEN 'materialized views' \
             WHEN 'S' THEN 'sequences' ELSE 'foreign tables' END AS label \
           FROM pg_catalog.pg_class c WHERE c.relnamespace OPERATOR(pg_catalog.=) {nsp} \
             AND c.relkind OPERATOR(pg_catalog.=) ANY (ARRAY['r', 'p', 'v', 'm', 'S', 'f']::\"char\"[]) \
           UNION ALL \
           SELECT CASE p.prokind WHEN 'p' THEN 'procedures' WHEN 'a' THEN 'aggregates' \
             ELSE 'functions' END \
           FROM pg_catalog.pg_proc p WHERE p.pronamespace OPERATOR(pg_catalog.=) {nsp} \
           UNION ALL \
           SELECT CASE t.typtype WHEN 'd' THEN 'domains' ELSE 'types' END \
           FROM pg_catalog.pg_type t \
           LEFT JOIN pg_catalog.pg_class tc ON tc.oid OPERATOR(pg_catalog.=) t.typrelid \
           WHERE t.typnamespace OPERATOR(pg_catalog.=) {nsp} \
             AND t.typtype OPERATOR(pg_catalog.=) ANY (ARRAY['e', 'r', 'd', 'c']::\"char\"[]) \
             AND (t.typtype OPERATOR(pg_catalog.<>) 'c' OR tc.relkind OPERATOR(pg_catalog.=) 'c') \
         ) x GROUP BY label ORDER BY label"
    ))
}

/// Indexes, triggers, rules, policies and partitions (every level, in any
/// schema) of a relation: they go with it.
fn pg_relation_contents_sql(schema: &str, name: &str) -> CatalogResult<String> {
    let rel = format!(
        "(SELECT c.oid FROM pg_catalog.pg_class c \
          JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
          WHERE n.nspname OPERATOR(pg_catalog.=) {} AND c.relname OPERATOR(pg_catalog.=) {})",
        pg_string(schema)?,
        pg_string(name)?
    );
    Ok(format!(
        "SELECT 'indexes' AS label, (SELECT pg_catalog.count(*) FROM pg_catalog.pg_index i \
           WHERE i.indrelid OPERATOR(pg_catalog.=) {rel})::pg_catalog.text AS count \
         UNION ALL SELECT 'triggers', (SELECT pg_catalog.count(*) FROM pg_catalog.pg_trigger g \
           WHERE g.tgrelid OPERATOR(pg_catalog.=) {rel} AND NOT g.tgisinternal)::pg_catalog.text \
         UNION ALL SELECT 'rules', (SELECT pg_catalog.count(*) FROM pg_catalog.pg_rewrite w \
           WHERE w.ev_class OPERATOR(pg_catalog.=) {rel} \
           AND w.rulename OPERATOR(pg_catalog.<>) '_RETURN')::pg_catalog.text \
         UNION ALL SELECT 'policies', (SELECT pg_catalog.count(*) FROM pg_catalog.pg_policy y \
           WHERE y.polrelid OPERATOR(pg_catalog.=) {rel})::pg_catalog.text \
         UNION ALL SELECT 'partitions', (SELECT pg_catalog.count(*) FROM pg_catalog.pg_partition_tree({rel}) p \
           WHERE p.relid OPERATOR(pg_catalog.<>) {rel})::pg_catalog.text"
    ))
}

/// What blocks `DROP DATABASE` even with `FORCE`: prepared transactions,
/// active logical replication slots and subscriptions in the database.
fn pg_blockers_sql(database: &str) -> CatalogResult<String> {
    let db = pg_string(database)?;
    Ok(format!(
        "SELECT (SELECT pg_catalog.count(*) FROM pg_catalog.pg_prepared_xacts \
           WHERE database OPERATOR(pg_catalog.=) {db})::pg_catalog.text AS prepared, \
         (SELECT pg_catalog.count(*) FROM pg_catalog.pg_replication_slots \
           WHERE database OPERATOR(pg_catalog.=) {db} AND active)::pg_catalog.text AS slots, \
         (SELECT pg_catalog.count(s.subdbid) FROM pg_catalog.pg_subscription s \
           JOIN pg_catalog.pg_database d ON d.oid OPERATOR(pg_catalog.=) s.subdbid \
           WHERE d.datname OPERATOR(pg_catalog.=) {db})::pg_catalog.text AS subscriptions"
    ))
}

fn plural(count: &str, one: &str, many: &str) -> String {
    if count == "1" {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

async fn pg_impact(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<DropImpact> {
    let mut impact = DropImpact {
        sessions_complete: true,
        ..DropImpact::default()
    };
    if object.kind == ObjectKind::Database {
        let name = object.name.as_str();
        let (sessions, complete) = sessions_on(session, server, name).await?;
        impact.sessions = sessions;
        impact.sessions_complete = complete;
        let rows = rows::query(session, &pg_blockers_sql(name)?).await?;
        let row = rows.single("blockers")?;
        let prepared = row.req_text("prepared")?;
        let slots = row.req_text("slots")?;
        let subscriptions = row.req_text("subscriptions")?;
        if prepared != "0" {
            impact.blockers.push(format!(
                "{} in this database: commit or roll them back (COMMIT PREPARED / ROLLBACK PREPARED) first",
                plural(&prepared, "prepared transaction", "prepared transactions")
            ));
        }
        if slots != "0" {
            impact.blockers.push(format!(
                "{} of this database in use: stop the client that streams from it first",
                plural(
                    &slots,
                    "logical replication slot",
                    "logical replication slots"
                )
            ));
        }
        if subscriptions != "0" {
            impact.blockers.push(format!(
                "{} in this database: drop them (DROP SUBSCRIPTION) first",
                plural(&subscriptions, "subscription", "subscriptions")
            ));
        }
        return Ok(impact);
    }
    let Some(seed) = pg_seed(object)? else {
        return Ok(impact);
    };
    let schema = (object.kind == ObjectKind::Schema).then_some(object.name.as_str());
    let rows = rows::query(session, &pg_dependents_sql(&seed, schema)?).await?;
    let (dependents, truncated) = dependents_from(&rows, DependentEffect::Blocks)?;
    impact.dependents = dependents;
    impact.dependents_truncated = truncated;
    match object.kind {
        ObjectKind::Schema => {
            let rows = rows::query(session, &pg_schema_contents_sql(&object.name)?).await?;
            impact.contents = contents_from(&rows)?;
        }
        ObjectKind::Table
        | ObjectKind::PartitionedTable
        | ObjectKind::Partition
        | ObjectKind::View
        | ObjectKind::MaterializedView => {
            let schema = required(object.schema.as_deref(), "schema")?;
            let rows =
                rows::query(session, &pg_relation_contents_sql(schema, &object.name)?).await?;
            impact.contents = contents_from(&rows)?;
        }
        _ => {}
    }
    Ok(impact)
}

/// Other client sessions connected to `database` (MySQL and MariaDB: with
/// it as their default database), from the process list.
async fn sessions_on(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
) -> CatalogResult<(Vec<ActiveSession>, bool)> {
    let list = match process_list(session, server).await {
        Ok(list) => list,
        // Without the privilege to read the list nothing can be shown; the
        // UI says the list may be incomplete.
        Err(LiveError::PermissionDenied { .. }) => return Ok((Vec::new(), false)),
        Err(LiveError::Driver { error }) => return Err(CatalogError::Driver { error }),
        Err(other) => {
            return Err(CatalogError::Unexpected {
                message: other.to_string(),
            });
        }
    };
    let sessions = list
        .processes
        .into_iter()
        .filter(|p| !p.own && p.database.as_deref() == Some(database))
        .filter(|p| {
            p.backend_type
                .as_deref()
                .is_none_or(|t| t == "client backend")
        })
        .map(|p| {
            let state = match (p.command, p.state) {
                (Some(command), Some(state)) if !state.is_empty() => {
                    Some(format!("{command}: {state}"))
                }
                (Some(command), _) => Some(command),
                (None, state) => state,
            };
            ActiveSession {
                id: p.id.to_string(),
                varve: p
                    .application
                    .as_deref()
                    .is_some_and(|a| a.starts_with("Varve")),
                user: p.user,
                application: p.application,
                client: p.client_address,
                state,
            }
        })
        .collect();
    let complete = match server.engine {
        Engine::Postgres => true,
        _ => mysql_sees_all_sessions(session, server).await?,
    };
    Ok((sessions, complete))
}

/// Whether the account has `PROCESS` (MySQL and MariaDB list only the
/// account's own connections without it), directly or through its active
/// roles.
async fn mysql_sees_all_sessions(
    session: &dyn Session,
    server: &ServerInfo,
) -> CatalogResult<bool> {
    let privileges = mysql_global_privileges(session, server).await?;
    Ok(privileges.contains("PROCESS") || privileges.contains("ALL"))
}

/// The account's global privileges (`ON *.*`, upper case, `ALL` for
/// `ALL PRIVILEGES`), directly or through its active roles: `SHOW GRANTS`
/// with the active roles expanded (MySQL `USING` the roles
/// `CURRENT_ROLE()` names; MariaDB also the grants of `CURRENT_ROLE`).
async fn mysql_global_privileges(
    session: &dyn Session,
    server: &ServerInfo,
) -> CatalogResult<std::collections::BTreeSet<String>> {
    let role = rows::query(session, "SELECT CURRENT_ROLE() AS r").await?;
    let role = role.single("the current role")?.text("r")?;
    let mut statements = Vec::new();
    match (server.engine, role.as_deref()) {
        // `CURRENT_ROLE()` prints the active roles already quoted
        // (`` `r1`@`%`,`r2`@`%` ``), the form `USING` takes.
        (Engine::Mysql, Some(roles)) if roles != "NONE" && !roles.is_empty() => {
            if !roles.starts_with('`') {
                return Err(CatalogError::Unexpected {
                    message: format!("CURRENT_ROLE() returned {roles}"),
                });
            }
            statements.push(format!("SHOW GRANTS FOR CURRENT_USER() USING {roles}"));
        }
        (Engine::Mariadb, Some(_)) => {
            statements.push("SHOW GRANTS".to_owned());
            statements.push("SHOW GRANTS FOR CURRENT_ROLE".to_owned());
        }
        _ => statements.push("SHOW GRANTS".to_owned()),
    }
    let mut privileges = std::collections::BTreeSet::new();
    for statement in statements {
        let rows = rows::query(session, &statement).await?;
        let Some(column) = rows.columns().first().cloned() else {
            continue;
        };
        for row in rows.iter() {
            if let Some(grant) = row.text(&column)? {
                privileges.extend(global_privileges(&grant));
            }
        }
    }
    Ok(privileges)
}

/// The privileges of a `GRANT ... ON *.* ...` line (`ALL` for
/// `ALL PRIVILEGES`); none for grants on databases, tables or roles.
fn global_privileges(grant: &str) -> Vec<String> {
    let upper = grant.to_ascii_uppercase();
    let Some(rest) = upper.strip_prefix("GRANT ") else {
        return Vec::new();
    };
    let Some(on) = rest.find(" ON *.* ") else {
        return Vec::new();
    };
    rest[..on]
        .split(',')
        .map(|p| match p.trim() {
            "ALL PRIVILEGES" => "ALL".to_owned(),
            other => other.to_owned(),
        })
        .collect()
}

/// True for a `GRANT ... ON *.* ...` line that includes `PROCESS` (or
/// `ALL PRIVILEGES`).
#[cfg(test)]
fn grants_process(grant: &str) -> bool {
    global_privileges(grant)
        .iter()
        .any(|p| p == "PROCESS" || p == "ALL")
}

// ---------------------------------------------------------------------------
// MySQL and MariaDB
// ---------------------------------------------------------------------------

/// MySQL 8.0.13 and newer list view dependencies in
/// `information_schema.VIEW_TABLE_USAGE`; MariaDB does not.
fn has_view_table_usage(server: &ServerInfo) -> bool {
    server.engine == Engine::Mysql
        && (server.major, server.minor, server.patch.unwrap_or(0)) >= (8, 0, 13)
}

/// A `LIKE` pattern (escape `!`) that finds `` `database`.`table` `` in a
/// view definition, as MariaDB prints them.
fn mariadb_view_pattern(database: &str, table: Option<&str>) -> CatalogResult<String> {
    let quote = |s: &str| format!("`{}`", s.replace('`', "``"));
    let text = match table {
        Some(table) => format!("{}.{}", quote(database), quote(table)),
        None => format!("{}.", quote(database)),
    };
    let escaped = text
        .replace('!', "!!")
        .replace('%', "!%")
        .replace('_', "!_");
    my_string(&format!("%{escaped}%"))
}

/// A `REGEXP` (MySQL ICU and MariaDB PCRE alike) that finds `name` as a
/// whole word, quoted or not, case-insensitive through the
/// `information_schema` collations.
fn word_regexp(name: &str) -> CatalogResult<String> {
    let mut escaped = String::new();
    for c in name.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    my_string(&format!("(^|[^A-Za-z0-9_$]){escaped}($|[^A-Za-z0-9_$])"))
}

/// Routines, triggers and events whose body names the dropped object:
/// `table` in `database` (bodies in that database, or naming the database
/// too), or for a whole database the bodies in other databases that name
/// it. Triggers on the dropped table itself go with it and are left out.
fn my_body_dependents_sql(database: &str, table: Option<&str>) -> CatalogResult<String> {
    let db = my_string(database)?;
    let db_word = word_regexp(database)?;
    let filter = |body: &str, schema: &str| -> CatalogResult<String> {
        Ok(match table {
            Some(table) => format!(
                "{body} REGEXP {} AND ({schema} = {db} OR {body} REGEXP {db_word})",
                word_regexp(table)?
            ),
            None => format!("{schema} <> {db} AND {body} REGEXP {db_word}"),
        })
    };
    let own_triggers = match table {
        Some(table) => format!(
            " AND NOT (EVENT_OBJECT_SCHEMA = {db} AND EVENT_OBJECT_TABLE = {})",
            my_string(table)?
        ),
        None => String::new(),
    };
    Ok(format!(
        "SELECT CONCAT(LOWER(ROUTINE_TYPE), ' ', ROUTINE_SCHEMA, '.', ROUTINE_NAME) AS description, \
           'mayBreak' AS effect FROM information_schema.ROUTINES WHERE {} \
         UNION ALL SELECT CONCAT('trigger ', TRIGGER_SCHEMA, '.', TRIGGER_NAME, ' on table ', \
           EVENT_OBJECT_SCHEMA, '.', EVENT_OBJECT_TABLE), 'mayBreak' \
           FROM information_schema.TRIGGERS WHERE {}{own_triggers} \
         UNION ALL SELECT CONCAT('event ', EVENT_SCHEMA, '.', EVENT_NAME), 'mayBreak' \
           FROM information_schema.EVENTS WHERE {}",
        filter("ROUTINE_DEFINITION", "ROUTINE_SCHEMA")?,
        filter("ACTION_STATEMENT", "TRIGGER_SCHEMA")?,
        filter("EVENT_DEFINITION", "EVENT_SCHEMA")?,
    ))
}

/// Dependents of a table or view in another table or view: foreign keys of
/// other tables (they block the drop), views over it (they break), and
/// routines, triggers and events that name it (they may break).
fn my_dependents_sql(server: &ServerInfo, database: &str, table: &str) -> CatalogResult<String> {
    let db = my_string(database)?;
    let name = my_string(table)?;
    let views = if has_view_table_usage(server) {
        format!(
            "SELECT CONCAT('view ', VIEW_SCHEMA, '.', VIEW_NAME) AS description, 'breaks' AS effect \
             FROM information_schema.VIEW_TABLE_USAGE \
             WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {name} \
             AND NOT (VIEW_SCHEMA = {db} AND VIEW_NAME = {name})"
        )
    } else {
        format!(
            "SELECT CONCAT('view ', TABLE_SCHEMA, '.', TABLE_NAME) AS description, 'breaks' AS effect \
             FROM information_schema.VIEWS \
             WHERE VIEW_DEFINITION LIKE {} ESCAPE '!' \
             AND NOT (TABLE_SCHEMA = {db} AND TABLE_NAME = {name})",
            mariadb_view_pattern(database, Some(table))?
        )
    };
    Ok(format!(
        "SELECT description, effect FROM ( \
           SELECT CONCAT('foreign key ', CONSTRAINT_NAME, ' on table ', CONSTRAINT_SCHEMA, '.', TABLE_NAME) \
             AS description, 'blocks' AS effect \
           FROM information_schema.REFERENTIAL_CONSTRAINTS \
           WHERE UNIQUE_CONSTRAINT_SCHEMA = {db} AND REFERENCED_TABLE_NAME = {name} \
             AND NOT (CONSTRAINT_SCHEMA = {db} AND TABLE_NAME = {name}) \
           UNION ALL {views} \
           UNION ALL {} \
         ) x ORDER BY description LIMIT {}",
        my_body_dependents_sql(database, Some(table))?,
        MAX_DEPENDENTS + 1
    ))
}

/// Dependents of a database outside it: foreign keys of tables in other
/// databases referencing its tables, views in other databases over them.
fn my_database_dependents_sql(server: &ServerInfo, database: &str) -> CatalogResult<String> {
    let db = my_string(database)?;
    let views = if has_view_table_usage(server) {
        format!(
            "SELECT DISTINCT CONCAT('view ', VIEW_SCHEMA, '.', VIEW_NAME) AS description, 'breaks' AS effect \
             FROM information_schema.VIEW_TABLE_USAGE \
             WHERE TABLE_SCHEMA = {db} AND VIEW_SCHEMA <> {db}"
        )
    } else {
        format!(
            "SELECT CONCAT('view ', TABLE_SCHEMA, '.', TABLE_NAME) AS description, 'breaks' AS effect \
             FROM information_schema.VIEWS \
             WHERE VIEW_DEFINITION LIKE {} ESCAPE '!' AND TABLE_SCHEMA <> {db}",
            mariadb_view_pattern(database, None)?
        )
    };
    Ok(format!(
        "SELECT description, effect FROM ( \
           SELECT CONCAT('foreign key ', CONSTRAINT_NAME, ' on table ', CONSTRAINT_SCHEMA, '.', TABLE_NAME) \
             AS description, 'blocks' AS effect \
           FROM information_schema.REFERENTIAL_CONSTRAINTS \
           WHERE UNIQUE_CONSTRAINT_SCHEMA = {db} AND CONSTRAINT_SCHEMA <> {db} \
           UNION ALL {views} \
           UNION ALL {} \
         ) x ORDER BY description LIMIT {}",
        my_body_dependents_sql(database, None)?,
        MAX_DEPENDENTS + 1
    ))
}

/// What a database holds, by kind, in one query.
fn my_database_contents_sql(database: &str) -> CatalogResult<String> {
    let db = my_string(database)?;
    Ok(format!(
        "SELECT 'tables' AS label, CAST(COUNT(*) AS CHAR) AS count FROM information_schema.TABLES \
           WHERE TABLE_SCHEMA = {db} AND TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED') \
         UNION ALL SELECT 'views', CAST(COUNT(*) AS CHAR) FROM information_schema.TABLES \
           WHERE TABLE_SCHEMA = {db} AND TABLE_TYPE = 'VIEW' \
         UNION ALL SELECT 'sequences', CAST(COUNT(*) AS CHAR) FROM information_schema.TABLES \
           WHERE TABLE_SCHEMA = {db} AND TABLE_TYPE = 'SEQUENCE' \
         UNION ALL SELECT 'routines', CAST(COUNT(*) AS CHAR) FROM information_schema.ROUTINES \
           WHERE ROUTINE_SCHEMA = {db} \
         UNION ALL SELECT 'triggers', CAST(COUNT(*) AS CHAR) FROM information_schema.TRIGGERS \
           WHERE TRIGGER_SCHEMA = {db} \
         UNION ALL SELECT 'events', CAST(COUNT(*) AS CHAR) FROM information_schema.EVENTS \
           WHERE EVENT_SCHEMA = {db}"
    ))
}

/// Triggers of a table: they go with it.
fn my_table_contents_sql(database: &str, table: &str) -> CatalogResult<String> {
    Ok(format!(
        "SELECT 'triggers' AS label, CAST(COUNT(*) AS CHAR) AS count \
         FROM information_schema.TRIGGERS \
         WHERE EVENT_OBJECT_SCHEMA = {} AND EVENT_OBJECT_TABLE = {}",
        my_string(database)?,
        my_string(table)?
    ))
}

fn my_dependents_from(rows: &Rows) -> CatalogResult<(Vec<Dependent>, bool)> {
    let mut out = Vec::new();
    for row in rows.iter() {
        let Some(description) = row.text("description")? else {
            continue;
        };
        let effect = match row.text("effect")?.as_deref() {
            Some("blocks") => DependentEffect::Blocks,
            Some("mayBreak") => DependentEffect::MayBreak,
            _ => DependentEffect::Breaks,
        };
        out.push(Dependent {
            description,
            effect,
        });
    }
    let truncated = out.len() > MAX_DEPENDENTS;
    out.truncate(MAX_DEPENDENTS);
    Ok((out, truncated))
}

/// MySQL 8.0.22+ `READ ONLY` databases refuse every drop in them, and
/// their own drop (error 3989) until `READ ONLY = 0`.
async fn my_read_only(
    session: &dyn Session,
    server: &ServerInfo,
    database: &str,
) -> CatalogResult<bool> {
    if server.engine != Engine::Mysql
        || (server.major, server.minor, server.patch.unwrap_or(0)) < (8, 0, 22)
    {
        return Ok(false);
    }
    let rows = rows::query(
        session,
        &format!(
            "SELECT OPTIONS AS options FROM information_schema.SCHEMATA_EXTENSIONS \
             WHERE SCHEMA_NAME = {}",
            my_string(database)?
        ),
    )
    .await?;
    for row in rows.iter() {
        if row
            .text("options")?
            .is_some_and(|o| o.to_ascii_uppercase().contains("READ ONLY=1"))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Routine bodies the search cannot read: without `SHOW_ROUTINE` (MySQL
/// 8.0.20+) or a global `SELECT`, `information_schema.ROUTINES` shows the
/// body only of routines the account defined, and lists no routine it has
/// no privilege on. `None` when the account reads them all.
async fn my_hidden_routines_note(
    session: &dyn Session,
    server: &ServerInfo,
) -> CatalogResult<Option<String>> {
    let privileges = mysql_global_privileges(session, server).await?;
    if ["SHOW_ROUTINE", "SELECT", "ALL"]
        .iter()
        .any(|p| privileges.contains(*p))
    {
        return Ok(None);
    }
    let rows = rows::query(
        session,
        "SELECT CAST(COUNT(*) AS CHAR) AS n FROM information_schema.ROUTINES \
         WHERE ROUTINE_DEFINITION IS NULL",
    )
    .await?;
    let hidden = rows.single("hidden routines")?.req_text("n")?;
    let bodies = if hidden == "0" {
        String::new()
    } else {
        format!(
            " {} visible to it {} no readable body.",
            plural(&hidden, "routine", "routines"),
            if hidden == "1" { "has" } else { "have" }
        )
    };
    Ok(Some(format!(
        "Routines were searched only as far as this account can read them: routines it has \
         no privilege on are not listed by the server.{bodies} SHOW_ROUTINE (MySQL) or a \
         global SELECT lets the search read every routine."
    )))
}

async fn my_impact(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<DropImpact> {
    let mut impact = DropImpact {
        sessions_complete: true,
        ..DropImpact::default()
    };
    let database = match object.kind {
        ObjectKind::Database => Some(object.name.as_str()),
        _ => object.database.as_deref(),
    };
    if let Some(database) = database
        && my_read_only(session, server, database).await?
    {
        impact.blockers.push(format!(
            "database {database} is read only (READ ONLY = 1): turn that off first \
             (ALTER DATABASE ... READ ONLY = 0)"
        ));
    }
    if matches!(
        object.kind,
        ObjectKind::Database | ObjectKind::Table | ObjectKind::View
    ) && let Some(note) = my_hidden_routines_note(session, server).await?
    {
        impact.notes.push(note);
    }
    match object.kind {
        ObjectKind::Database => {
            let name = object.name.as_str();
            let rows = rows::query(session, &my_database_dependents_sql(server, name)?).await?;
            (impact.dependents, impact.dependents_truncated) = my_dependents_from(&rows)?;
            let rows = rows::query(session, &my_database_contents_sql(name)?).await?;
            impact.contents = contents_from(&rows)?;
            let (sessions, complete) = sessions_on(session, server, name).await?;
            impact.sessions = sessions;
            impact.sessions_complete = complete;
            // The statement goes to the binary log, so replicas run it too.
            let log = rows::query(session, "SELECT @@log_bin AS b").await?;
            if log.single("log_bin")?.int("b")? == Some(1) {
                impact.notes.push(BINLOG_NOTE.into());
            }
        }
        ObjectKind::Table | ObjectKind::View => {
            let database = required(object.database.as_deref(), "database")?;
            let rows =
                rows::query(session, &my_dependents_sql(server, database, &object.name)?).await?;
            (impact.dependents, impact.dependents_truncated) = my_dependents_from(&rows)?;
            if object.kind == ObjectKind::Table {
                let rows =
                    rows::query(session, &my_table_contents_sql(database, &object.name)?).await?;
                impact.contents = contents_from(&rows)?;
            }
        }
        _ => {}
    }
    Ok(impact)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(kind: ObjectKind, schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef {
            kind,
            database: Some("d".into()),
            schema: schema.map(str::to_owned),
            parent: None,
            name: name.into(),
            signature: None,
        }
    }

    #[test]
    fn seeds_quote_names_and_skip_kinds_without_dependents() -> CatalogResult<()> {
        let seed = pg_seed(&object(ObjectKind::Table, Some("s'x"), "t"))?;
        assert!(seed.is_some_and(|s| s.contains("'s''x'")));
        let routine = ObjectRef {
            signature: Some("integer, text".into()),
            ..object(ObjectKind::Function, Some("s"), "f")
        };
        assert!(
            pg_seed(&routine)?
                .is_some_and(|s| s.contains(r#"to_regprocedure('"s"."f"(integer, text)')"#))
        );
        assert!(pg_seed(&object(ObjectKind::Policy, Some("s"), "p"))?.is_none());
        assert!(pg_seed(&object(ObjectKind::Table, None, "t")).is_err());
        Ok(())
    }

    #[test]
    fn mariadb_pattern_escapes_like_characters() -> CatalogResult<()> {
        assert_eq!(
            mariadb_view_pattern("a_b", Some("t%`x"))?,
            "'%`a!_b`.`t!%``x`%'"
        );
        assert_eq!(mariadb_view_pattern("d", None)?, "'%`d`.%'");
        Ok(())
    }

    #[test]
    fn view_usage_needs_mysql_8_0_13() {
        let server = |engine, major, minor, patch| ServerInfo {
            engine,
            version: String::new(),
            major,
            minor,
            patch,
        };
        assert!(has_view_table_usage(&server(Engine::Mysql, 8, 0, Some(13))));
        assert!(!has_view_table_usage(&server(
            Engine::Mysql,
            8,
            0,
            Some(12)
        )));
        assert!(has_view_table_usage(&server(Engine::Mysql, 9, 7, None)));
        assert!(!has_view_table_usage(&server(
            Engine::Mariadb,
            12,
            3,
            Some(0)
        )));
    }

    #[test]
    fn process_grants_are_read_from_show_grants() {
        assert!(grants_process("GRANT PROCESS ON *.* TO `u`@`%`"));
        assert!(grants_process(
            "GRANT SELECT, PROCESS, RELOAD ON *.* TO 'r'@'%'"
        ));
        assert!(grants_process(
            "GRANT ALL PRIVILEGES ON *.* TO `root`@`%` WITH GRANT OPTION"
        ));
        assert!(!grants_process("GRANT PROCESS_X ON *.* TO `u`@`%`"));
        assert!(!grants_process(
            "GRANT ALL PRIVILEGES ON `shop`.* TO `u`@`%`"
        ));
        assert!(!grants_process("GRANT `r1`@`%` TO `u`@`%`"));
        assert_eq!(
            global_privileges("GRANT SELECT, SHOW_ROUTINE ON *.* TO `u`@`%`"),
            vec!["SELECT".to_owned(), "SHOW_ROUTINE".to_owned()]
        );
        assert!(global_privileges("GRANT SELECT ON `shop`.* TO `u`@`%`").is_empty());
    }

    #[test]
    fn body_search_quotes_names_and_regexp_characters() -> CatalogResult<()> {
        assert_eq!(
            word_regexp("a.b")?,
            "_utf8mb4 X'285e7c5b5e412d5a612d7a302d395f245d29615c2e6228247c5b5e412d5a612d7a302d395f245d29'"
        );
        assert_eq!(word_regexp("t")?, "'(^|[^A-Za-z0-9_$])t($|[^A-Za-z0-9_$])'");
        let sql = my_body_dependents_sql("d'b", Some("t"))?;
        assert!(sql.contains("ROUTINE_SCHEMA = 'd''b'"), "{sql}");
        assert!(
            sql.contains("NOT (EVENT_OBJECT_SCHEMA = 'd''b' AND EVENT_OBJECT_TABLE = 't')"),
            "{sql}"
        );
        assert!(my_body_dependents_sql("d", None)?.contains("ROUTINE_SCHEMA <> 'd'"));
        Ok(())
    }

    #[test]
    fn plural_counts() {
        assert_eq!(plural("1", "slot", "slots"), "1 slot");
        assert_eq!(plural("3", "slot", "slots"), "3 slots");
    }
}
