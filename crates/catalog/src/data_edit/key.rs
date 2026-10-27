//! Finds the key that makes a table editable ([`editable_key`]).

use varve_core::Engine;
use varve_driver::{Dialect, ServerInfo, Session, Value};

use super::model::{
    ColumnInfo, GeneratedKind, IdentityKind, KeyCollation, KeyInfo, KeyKind, QualifiedOperator,
    TableTarget,
};
use super::plan::dialect_of;
use crate::model::{CatalogError, CatalogResult};
use crate::rows::{Row, query};
use crate::sql;

/// The key the grid edits a table by, and every column of the table with
/// what literals and the UI need (types, defaults, generated and identity
/// columns).
///
/// The key is the primary key, else the unique key with the fewest columns
/// (then by name) whose columns are all `NOT NULL`. PostgreSQL unique
/// indexes that are deferrable, partial, on expressions or not valid are
/// skipped, and so are MySQL / MariaDB unique indexes with functional key
/// parts. `INCLUDE` columns are not part of a key.
///
/// `Ok(None)` means the grid stays read only: the table has no such key,
/// or it is not a table (a view, a materialized view, a sequence, a
/// foreign table).
///
/// # Errors
///
/// [`CatalogError::NotFound`] when the table does not exist,
/// [`CatalogError::Invalid`] for a PostgreSQL target without a schema or in
/// another database than the session's, [`CatalogError::Unsupported`] for
/// Redis, and driver errors.
pub async fn editable_key(
    session: &dyn Session,
    server: &ServerInfo,
    target: &TableTarget,
) -> CatalogResult<Option<KeyInfo>> {
    match dialect_of(server.engine)? {
        Dialect::Postgres => postgres(session, target).await,
        Dialect::Mysql => mysql(session, server.engine, target).await,
    }
}

fn not_found(target: &TableTarget) -> CatalogError {
    let name = match &target.schema {
        Some(schema) => format!("{}.{schema}.{}", target.database, target.table),
        None => format!("{}.{}", target.database, target.table),
    };
    CatalogError::NotFound {
        message: format!("table {name} does not exist"),
    }
}

/// A key candidate: name, whether it is the primary key, its columns and
/// (PostgreSQL) their equality operators and index collations.
struct Candidate {
    name: String,
    primary: bool,
    columns: Vec<String>,
    operators: Vec<QualifiedOperator>,
    collations: Vec<Option<KeyCollation>>,
}

/// The primary key, else the first unique key (candidates come sorted)
/// whose columns all exist and are `NOT NULL`.
fn choose(candidates: Vec<Candidate>, columns: Vec<ColumnInfo>) -> Option<KeyInfo> {
    let usable = |candidate: &Candidate| {
        !candidate.columns.is_empty()
            && candidate.columns.iter().all(|name| {
                columns
                    .iter()
                    .any(|column| column.name == *name && !column.nullable)
            })
    };
    let chosen = candidates
        .iter()
        .find(|c| c.primary && usable(c))
        .or_else(|| candidates.iter().find(|c| !c.primary && usable(c)))?;
    Some(KeyInfo {
        kind: if chosen.primary {
            KeyKind::Primary
        } else {
            KeyKind::Unique
        },
        name: chosen.name.clone(),
        columns: chosen.columns.clone(),
        table_columns: columns,
        equality_operators: chosen.operators.clone(),
        key_collations: chosen.collations.clone(),
    })
}

/// One key column of a PostgreSQL unique index, as the key query lists it.
#[derive(serde::Deserialize)]
struct PgKeyColumn {
    name: String,
    schema: Option<String>,
    operator: Option<String>,
    collation_schema: Option<String>,
    collation: Option<String>,
}

fn unexpected(message: impl Into<String>) -> CatalogError {
    CatalogError::Unexpected {
        message: message.into(),
    }
}

/// Whether the `pg_type` row `alias` is a true array type: one whose
/// element type names it as its array type. `int2vector` and `oidvector`
/// also have an element type and category `A`, but are no arrays (their
/// text form is `1 2`, and a slice of them is an `int2[]` / `oid[]`).
fn real_array(alias: &str) -> String {
    format!(
        "({alias}.typelem OPERATOR(pg_catalog.<>) 0::pg_catalog.oid \
         AND EXISTS (SELECT FROM pg_catalog.pg_type el \
                     WHERE el.oid OPERATOR(pg_catalog.=) {alias}.typelem \
                       AND el.typarray OPERATOR(pg_catalog.=) {alias}.oid))"
    )
}

fn flag(row: &Row<'_>, column: &str) -> CatalogResult<bool> {
    Ok(row.bool(column)?.unwrap_or(false))
}

async fn postgres(session: &dyn Session, target: &TableTarget) -> CatalogResult<Option<KeyInfo>> {
    let dialect = Dialect::Postgres;
    let schema = target
        .schema
        .as_deref()
        .ok_or_else(|| CatalogError::Invalid {
            message: "a PostgreSQL table needs its schema".into(),
        })?;
    let schema_lit = sql::string(dialect, schema)?;
    let table_lit = sql::string(dialect, &target.table)?;
    let relation = query(
        session,
        &format!(
            "SELECT pg_catalog.current_database() AS database, \
                    c.relkind::pg_catalog.text AS relkind \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
             WHERE n.nspname OPERATOR(pg_catalog.=) {schema_lit} \
               AND c.relname OPERATOR(pg_catalog.=) {table_lit}"
        ),
    )
    .await?;
    let Some(row) = relation.iter().next() else {
        return Err(not_found(target));
    };
    let database = row.req_text("database")?;
    if database != target.database {
        return Err(CatalogError::Invalid {
            message: format!(
                "the session is connected to database {database}, not {}",
                target.database
            ),
        });
    }
    // Ordinary and partitioned tables; views, materialized views, foreign
    // tables and sequences are not edited in the grid.
    if !matches!(row.req_text("relkind")?.as_str(), "r" | "p") {
        return Ok(None);
    }

    let column_rows = query(
        session,
        &format!(
            "SELECT a.attname AS name, \
                    pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type, \
                    tn.nspname AS type_schema, t.typname AS type_name, \
                    NOT a.attnotnull AS nullable, a.atthasdef AS has_default, \
                    a.attidentity::pg_catalog.text AS identity, \
                    a.attgenerated::pg_catalog.text AS generated, \
                    CASE WHEN t.typtype OPERATOR(pg_catalog.=) 'd' THEN btn.nspname END AS base_schema, \
                    CASE WHEN t.typtype OPERATOR(pg_catalog.=) 'd' THEN btt.typname END AS base_name, \
                    {real_array_btt} AS is_array, \
                    {real_array_btt} AND {real_array_ett} AS is_array_of_arrays \
             FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) a.attrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
             JOIN pg_catalog.pg_type t ON t.oid OPERATOR(pg_catalog.=) a.atttypid \
             JOIN pg_catalog.pg_namespace tn ON tn.oid OPERATOR(pg_catalog.=) t.typnamespace \
             LEFT JOIN LATERAL (WITH RECURSIVE b AS ( \
                 SELECT t.oid, t.typtype, t.typbasetype AS base \
                 UNION ALL SELECT x.oid, x.typtype, x.typbasetype FROM pg_catalog.pg_type x \
                 JOIN b ON x.oid OPERATOR(pg_catalog.=) b.base \
                 WHERE b.typtype OPERATOR(pg_catalog.=) 'd') \
               SELECT b.oid FROM b WHERE b.typtype OPERATOR(pg_catalog.<>) 'd') bt ON true \
             LEFT JOIN pg_catalog.pg_type btt ON btt.oid OPERATOR(pg_catalog.=) bt.oid \
             LEFT JOIN pg_catalog.pg_namespace btn ON btn.oid OPERATOR(pg_catalog.=) btt.typnamespace \
             LEFT JOIN LATERAL (WITH RECURSIVE e AS ( \
                 SELECT x.oid, x.typtype, x.typbasetype AS base FROM pg_catalog.pg_type x \
                 WHERE x.oid OPERATOR(pg_catalog.=) btt.typelem \
                 UNION ALL SELECT x.oid, x.typtype, x.typbasetype FROM pg_catalog.pg_type x \
                 JOIN e ON x.oid OPERATOR(pg_catalog.=) e.base \
                 WHERE e.typtype OPERATOR(pg_catalog.=) 'd') \
               SELECT e.oid FROM e WHERE e.typtype OPERATOR(pg_catalog.<>) 'd') et ON true \
             LEFT JOIN pg_catalog.pg_type ett ON ett.oid OPERATOR(pg_catalog.=) et.oid \
             WHERE n.nspname OPERATOR(pg_catalog.=) {schema_lit} \
               AND c.relname OPERATOR(pg_catalog.=) {table_lit} \
               AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            real_array_btt = real_array("btt"),
            real_array_ett = real_array("ett"),
        ),
    )
    .await?;
    let mut columns = Vec::with_capacity(column_rows.len());
    for row in column_rows.iter() {
        let identity = match row.text("identity")?.as_deref() {
            Some("a") => Some(IdentityKind::Always),
            Some("d") => Some(IdentityKind::ByDefault),
            _ => None,
        };
        let generated = match row.text("generated")?.as_deref() {
            Some("s") => Some(GeneratedKind::Stored),
            Some("v") => Some(GeneratedKind::Virtual),
            _ => None,
        };
        let array_of_arrays = flag(&row, "is_array_of_arrays")?;
        columns.push(ColumnInfo {
            name: row.req_text("name")?,
            data_type: row.req_text("data_type")?,
            type_schema: Some(row.req_text("type_schema")?),
            type_name: row.req_text("type_name")?,
            base_type: match (row.text("base_schema")?, row.text("base_name")?) {
                (Some(schema), Some(name)) => Some((schema, name)),
                _ => None,
            },
            array: flag(&row, "is_array")?,
            array_of_arrays,
            nullable: flag(&row, "nullable")?,
            has_default: flag(&row, "has_default")? || identity.is_some() || generated.is_some(),
            editable: generated.is_none()
                && identity != Some(IdentityKind::Always)
                && !array_of_arrays,
            generated,
            identity,
            auto_increment: false,
        });
    }

    // Unique indexes usable as a row key, primary first, then by number of
    // key columns and name. The key columns come as a JSON array so names
    // with any character survive, each with the equality operator of the
    // index's operator class for it (btree strategy 3): the operator that
    // defines the key's uniqueness, named with its own schema.
    let key_rows = query(
        session,
        &format!(
            "SELECT COALESCE(con.conname, ic.relname) AS name, i.indisprimary AS is_primary, \
                    (SELECT pg_catalog.json_agg(pg_catalog.json_build_object( \
                              'name', a.attname, 'schema', opn.nspname, \
                              'operator', op.oprname, 'collation_schema', colln.nspname, \
                              'collation', coll.collname) ORDER BY k.ord) \
                     FROM ROWS FROM (pg_catalog.unnest(i.indkey::pg_catalog.int2[]), \
                                     pg_catalog.unnest(i.indclass::pg_catalog.oid[]), \
                                     pg_catalog.unnest(i.indcollation::pg_catalog.oid[])) \
                          WITH ORDINALITY AS k(attnum, opclass, collation_oid, ord) \
                     JOIN pg_catalog.pg_attribute a \
                       ON a.attrelid OPERATOR(pg_catalog.=) i.indrelid \
                      AND a.attnum OPERATOR(pg_catalog.=) k.attnum \
                     JOIN pg_catalog.pg_opclass oc ON oc.oid OPERATOR(pg_catalog.=) k.opclass \
                     LEFT JOIN pg_catalog.pg_amop ao \
                       ON ao.amopfamily OPERATOR(pg_catalog.=) oc.opcfamily \
                      AND ao.amopmethod OPERATOR(pg_catalog.=) oc.opcmethod \
                      AND ao.amoplefttype OPERATOR(pg_catalog.=) oc.opcintype \
                      AND ao.amoprighttype OPERATOR(pg_catalog.=) oc.opcintype \
                      AND ao.amopstrategy OPERATOR(pg_catalog.=) 3 \
                     LEFT JOIN pg_catalog.pg_operator op \
                       ON op.oid OPERATOR(pg_catalog.=) ao.amopopr \
                     LEFT JOIN pg_catalog.pg_namespace opn \
                       ON opn.oid OPERATOR(pg_catalog.=) op.oprnamespace \
                     LEFT JOIN pg_catalog.pg_collation coll \
                       ON coll.oid OPERATOR(pg_catalog.=) k.collation_oid \
                     LEFT JOIN pg_catalog.pg_namespace colln \
                       ON colln.oid OPERATOR(pg_catalog.=) coll.collnamespace \
                     WHERE k.ord OPERATOR(pg_catalog.<=) i.indnkeyatts)::pg_catalog.text \
                    AS columns \
             FROM pg_catalog.pg_index i \
             JOIN pg_catalog.pg_class ic ON ic.oid OPERATOR(pg_catalog.=) i.indexrelid \
             JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) i.indrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
             JOIN pg_catalog.pg_am am ON am.oid OPERATOR(pg_catalog.=) ic.relam \
             LEFT JOIN pg_catalog.pg_constraint con \
               ON con.conindid OPERATOR(pg_catalog.=) i.indexrelid \
              AND con.conrelid OPERATOR(pg_catalog.=) i.indrelid \
              AND con.contype OPERATOR(pg_catalog.=) ANY (ARRAY['p', 'u']::pg_catalog.\"char\"[]) \
             WHERE n.nspname OPERATOR(pg_catalog.=) {schema_lit} \
               AND c.relname OPERATOR(pg_catalog.=) {table_lit} \
               AND am.amname OPERATOR(pg_catalog.=) 'btree' \
               AND i.indisunique AND i.indimmediate AND i.indisvalid \
               AND i.indpred IS NULL AND i.indexprs IS NULL \
               AND NOT (0 OPERATOR(pg_catalog.=) ANY (i.indkey::pg_catalog.int2[])) \
             ORDER BY i.indisprimary DESC, i.indnkeyatts, 1"
        ),
    )
    .await?;
    let mut candidates = Vec::with_capacity(key_rows.len());
    for row in key_rows.iter() {
        let text = row.req_text("columns")?;
        let parts: Vec<PgKeyColumn> = serde_json::from_str(&text)
            .map_err(|e| unexpected(format!("index columns are not a JSON list: {e}")))?;
        // A column without a btree equality operator cannot be compared
        // safely; such an index is not used as the row key.
        let count = parts.len();
        let mut columns = Vec::with_capacity(count);
        let mut operators = Vec::with_capacity(count);
        let mut collations = Vec::with_capacity(count);
        for part in parts {
            if let (Some(schema), Some(name)) = (part.schema, part.operator) {
                columns.push(part.name);
                operators.push(QualifiedOperator { schema, name });
                collations.push(match (part.collation_schema, part.collation) {
                    (Some(schema), Some(name)) => Some(KeyCollation { schema, name }),
                    _ => None,
                });
            }
        }
        if count == 0 || columns.len() != count {
            continue;
        }
        candidates.push(Candidate {
            name: row.req_text("name")?,
            primary: flag(&row, "is_primary")?,
            columns,
            operators,
            collations,
        });
    }
    Ok(choose(candidates, columns))
}

async fn mysql(
    session: &dyn Session,
    engine: Engine,
    target: &TableTarget,
) -> CatalogResult<Option<KeyInfo>> {
    let dialect = Dialect::Mysql;
    if target.schema.is_some() {
        return Err(CatalogError::Invalid {
            message: "MySQL and MariaDB tables are named by database and table; leave the schema \
                      empty"
                .into(),
        });
    }
    let database_lit = sql::string(dialect, &target.database)?;
    let table_lit = sql::string(dialect, &target.table)?;
    let tables = query(
        session,
        &format!(
            "SELECT TABLE_TYPE AS table_type FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = {database_lit} AND TABLE_NAME = {table_lit}"
        ),
    )
    .await?;
    let Some(row) = tables.iter().next() else {
        return Err(not_found(target));
    };
    // MariaDB reports system-versioned tables separately; views, system
    // views and sequences are not edited in the grid.
    if !matches!(
        row.req_text("table_type")?.as_str(),
        "BASE TABLE" | "SYSTEM VERSIONED"
    ) {
        return Ok(None);
    }

    let column_rows = query(
        session,
        &format!(
            "SELECT COLUMN_NAME AS name, DATA_TYPE AS data_type, COLUMN_TYPE AS column_type, \
                    IS_NULLABLE AS nullable, COLUMN_DEFAULT AS column_default, EXTRA AS extra \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = {database_lit} AND TABLE_NAME = {table_lit} \
             ORDER BY ORDINAL_POSITION"
        ),
    )
    .await?;
    let mut columns = Vec::with_capacity(column_rows.len());
    for row in column_rows.iter() {
        let extra = row.text("extra")?.unwrap_or_default().to_ascii_lowercase();
        let generated = if extra.contains("stored generated") || extra.contains("persistent") {
            Some(GeneratedKind::Stored)
        } else if extra.contains("virtual generated") || extra.contains("virtual") {
            Some(GeneratedKind::Virtual)
        } else {
            None
        };
        let auto_increment = extra.contains("auto_increment");
        // MariaDB reports "no default" of a nullable column as the text
        // NULL; MySQL as SQL NULL, with expression defaults flagged in
        // EXTRA.
        let default = row.text("column_default")?;
        let explicit_default = match engine {
            Engine::Mariadb => default.is_some_and(|d| d != "NULL"),
            _ => default.is_some() || extra.contains("default_generated"),
        };
        columns.push(ColumnInfo {
            name: row.req_text("name")?,
            data_type: row.req_text("column_type")?,
            type_schema: None,
            type_name: row.req_text("data_type")?.to_ascii_lowercase(),
            base_type: None,
            array: false,
            array_of_arrays: false,
            nullable: row.req_text("nullable")? == "YES",
            has_default: explicit_default || auto_increment || generated.is_some(),
            editable: generated.is_none(),
            generated,
            identity: None,
            auto_increment,
        });
    }

    let key_rows = query(
        session,
        &format!(
            "SELECT INDEX_NAME AS index_name, COLUMN_NAME AS column_name \
             FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = {database_lit} AND TABLE_NAME = {table_lit} \
               AND NON_UNIQUE = 0 \
             ORDER BY INDEX_NAME, SEQ_IN_INDEX"
        ),
    )
    .await?;
    // Group the rows per index; an index with a functional key part
    // (no column name) cannot be used.
    let mut grouped: Vec<(String, Option<Vec<String>>)> = Vec::new();
    for row in key_rows.iter() {
        let index = row.req_text("index_name")?;
        let column = match row.value("column_name")? {
            Value::Null => None,
            _ => Some(row.req_text("column_name")?),
        };
        match grouped.last_mut() {
            Some((name, columns)) if *name == index => match (columns.as_mut(), column) {
                (Some(list), Some(column)) => list.push(column),
                _ => *columns = None,
            },
            _ => grouped.push((index, column.map(|c| vec![c]))),
        }
    }
    let mut candidates: Vec<Candidate> = grouped
        .into_iter()
        .filter_map(|(name, columns)| {
            columns.map(|columns| Candidate {
                primary: name == "PRIMARY",
                name,
                columns,
                operators: Vec::new(),
                collations: Vec::new(),
            })
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.primary
            .cmp(&a.primary)
            .then(a.columns.len().cmp(&b.columns.len()))
            .then(a.name.cmp(&b.name))
    });
    Ok(choose(candidates, columns))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: "int".into(),
            type_schema: None,
            type_name: "int".into(),
            base_type: None,
            array: false,
            array_of_arrays: false,
            nullable,
            has_default: false,
            generated: None,
            identity: None,
            auto_increment: false,
            editable: true,
        }
    }

    fn candidate(name: &str, primary: bool, columns: &[&str]) -> Candidate {
        Candidate {
            name: name.into(),
            primary,
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            operators: Vec::new(),
            collations: Vec::new(),
        }
    }

    #[test]
    fn primary_key_wins() {
        let key = choose(
            vec![
                candidate("u_b", false, &["b"]),
                candidate("pk", true, &["a", "b"]),
            ],
            vec![column("a", false), column("b", false)],
        );
        assert_eq!(key.as_ref().map(|k| k.kind), Some(KeyKind::Primary));
        assert_eq!(key.map(|k| k.columns), Some(vec!["a".into(), "b".into()]));
    }

    #[test]
    fn unique_keys_need_not_null_columns() {
        let columns = vec![column("a", true), column("b", false)];
        let key = choose(
            vec![
                candidate("u_a", false, &["a"]),
                candidate("u_b", false, &["b"]),
            ],
            columns.clone(),
        );
        assert_eq!(key.as_ref().map(|k| k.name.as_str()), Some("u_b"));
        assert_eq!(key.map(|k| k.kind), Some(KeyKind::Unique));
        assert_eq!(
            choose(vec![candidate("u_a", false, &["a"])], columns.clone()),
            None
        );
        assert_eq!(choose(vec![], columns), None);
    }
}
