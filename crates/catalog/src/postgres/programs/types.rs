//! Enum, composite and range types and domains (`pg_type`).

use serde::{Deserialize, Serialize};
use varve_driver::{ServerInfo, Session};

use super::Scope;
use super::common::{
    PgMeta, comment_on, extension_sql, fetch_pinned, flag, ident, int, lit, meta, owner_to, qname,
    schema_of, script, summary, system_schema_sql, texts, with_system,
};
use crate::model::{CatalogError, CatalogResult, ObjectKind, ObjectRef, ObjectSummary};
use crate::rows::{Row, Rows};

/// An enum type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgEnumType {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Labels in sort order.
    pub labels: Vec<String>,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

/// An attribute of a composite type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgAttribute {
    /// Name.
    pub name: String,
    /// Type as SQL.
    pub data_type: String,
    /// Collation as SQL, when it is not the type's default.
    pub collation: Option<String>,
    /// Comment.
    pub comment: Option<String>,
}

/// A composite type (`CREATE TYPE ... AS (...)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgCompositeType {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Attributes in order.
    pub attributes: Vec<PgAttribute>,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

/// A constraint of a domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgDomainConstraint {
    /// Name.
    pub name: String,
    /// Definition as SQL (`CHECK (...)`, `NOT NULL`), with `NOT VALID`
    /// when it is not validated.
    pub definition: String,
    /// True when existing data was validated.
    pub validated: bool,
    /// True for a `NOT NULL` constraint (stored as a constraint on 17+).
    pub not_null: bool,
    /// Comment.
    pub comment: Option<String>,
}

/// A domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgDomain {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Base type as SQL, with modifiers.
    pub base_type: String,
    /// Collation as SQL, when it is not the base type's default.
    pub collation: Option<String>,
    /// Default expression as SQL.
    pub default: Option<String>,
    /// `NOT NULL`.
    pub not_null: bool,
    /// Constraints by name.
    pub constraints: Vec<PgDomainConstraint>,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

/// A range type. Function, operator class and collation fields are SQL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PgRangeType {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Element type.
    pub subtype: String,
    /// B-tree operator class of the subtype.
    pub subtype_opclass: String,
    /// Collation of the subtype.
    pub collation: Option<String>,
    /// Canonical function.
    pub canonical: Option<String>,
    /// Subtype difference function.
    pub subtype_diff: Option<String>,
    /// The matching multirange type (14+).
    pub multirange: Option<String>,
    /// Owner, comment, system status.
    pub meta: PgMeta,
}

/// SQL for the qualified name of the collation `oid_expr`.
fn collation_sql(oid_expr: &str) -> String {
    format!(
        "(SELECT pg_catalog.quote_ident(cn.nspname) OPERATOR(pg_catalog.||) '.' OPERATOR(pg_catalog.||) pg_catalog.quote_ident(co.collname) FROM pg_catalog.pg_collation co \
         JOIN pg_catalog.pg_namespace cn ON cn.oid OPERATOR(pg_catalog.=) co.collnamespace WHERE co.oid OPERATOR(pg_catalog.=) {oid_expr})"
    )
}

fn typtype(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::EnumType => "e",
        ObjectKind::CompositeType => "c",
        ObjectKind::Domain => "d",
        _ => "r",
    }
}

fn base(kind: ObjectKind, filter: &str, extra_columns: &str) -> String {
    let composite_only = if kind == ObjectKind::CompositeType {
        " AND EXISTS (SELECT 1 FROM pg_catalog.pg_class c WHERE c.oid OPERATOR(pg_catalog.=) t.typrelid AND c.relkind OPERATOR(pg_catalog.=) 'c')"
    } else {
        ""
    };
    format!(
        "SELECT t.oid, pg_catalog.current_database() AS db, n.nspname AS schema, t.typname AS name, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) t.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_type'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment, {ext} AS extension, \
         {sys} AS system_schema{extra_columns} \
         FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.typnamespace \
         WHERE t.typtype OPERATOR(pg_catalog.=) '{tt}'{composite_only}{filter}",
        ext = extension_sql("pg_catalog.pg_type", "t.oid"),
        sys = system_schema_sql("n.nspname"),
        tt = typtype(kind),
    )
}

/// Inner list query (see [`super::common::with_system`]).
pub(super) fn inner(kind: ObjectKind, scope: &Scope) -> String {
    let extra = match kind {
        ObjectKind::EnumType => {
            ", (SELECT pg_catalog.count(*) FROM pg_catalog.pg_enum e WHERE e.enumtypid OPERATOR(pg_catalog.=) t.oid) AS detail_count"
        }
        ObjectKind::CompositeType => {
            ", (SELECT pg_catalog.count(*) FROM pg_catalog.pg_attribute a WHERE a.attrelid OPERATOR(pg_catalog.=) t.typrelid \
             AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped) AS detail_count"
        }
        ObjectKind::Domain => {
            ", pg_catalog.format_type(t.typbasetype, t.typtypmod) AS detail_text, t.typnotnull AS not_null"
        }
        _ => {
            ", (SELECT pg_catalog.format_type(r.rngsubtype, NULL) FROM pg_catalog.pg_range r \
             WHERE r.rngtypid OPERATOR(pg_catalog.=) t.oid) AS detail_text"
        }
    };
    base(kind, &scope.schema_filter("n.nspname"), extra)
}

/// Order of the list.
pub(super) const ORDER: &str = "q.name";

/// A tree entry from a row of [`inner`].
pub(super) fn summarize(kind: ObjectKind, row: &Row<'_>) -> CatalogResult<ObjectSummary> {
    let mut flags = Vec::new();
    let detail = match kind {
        ObjectKind::EnumType => Some(plural(int(row, "detail_count")?, "label")),
        ObjectKind::CompositeType => Some(plural(int(row, "detail_count")?, "attribute")),
        ObjectKind::Domain => {
            if flag(row, "not_null")? {
                flags.push("not null".to_owned());
            }
            row.text("detail_text")?
        }
        _ => row.text("detail_text")?,
    };
    summary(kind, row, true, None, detail, flags)
}

fn plural(count: i64, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else {
        format!("{count} {word}s")
    }
}

fn identity(object: &ObjectRef) -> CatalogResult<String> {
    Ok(format!(
        " AND n.nspname OPERATOR(pg_catalog.=) {} AND t.typname OPERATOR(pg_catalog.=) {}",
        lit(schema_of(object)?)?,
        lit(&object.name)?
    ))
}

fn single_main<'a>(rows: &'a Rows, object: &ObjectRef) -> CatalogResult<Row<'a>> {
    if rows.is_empty() {
        return Err(CatalogError::NotFound {
            message: format!("{:?} {} was not found", object.kind, object.name),
        });
    }
    rows.single("the type")
}

fn wrong_sets() -> CatalogError {
    CatalogError::Unexpected {
        message: "the type query returned the wrong number of result sets".into(),
    }
}

/// Details of an enum type.
pub(super) async fn describe_enum(
    session: &dyn Session,
    object: &ObjectRef,
) -> CatalogResult<PgEnumType> {
    let main = base(
        object.kind,
        &identity(object)?,
        ", pg_catalog.pg_get_userbyid(t.typowner) AS owner, \
         ARRAY(SELECT e.enumlabel::pg_catalog.text FROM pg_catalog.pg_enum e WHERE e.enumtypid OPERATOR(pg_catalog.=) t.oid \
         ORDER BY e.enumsortorder) AS labels",
    );
    let sets = fetch_pinned(session, &[(with_system(&main, true), "q.name".to_owned())]).await?;
    let rows = sets.first().ok_or_else(wrong_sets)?;
    let row = single_main(rows, object)?;
    Ok(PgEnumType {
        schema: row.req_text("schema")?,
        name: row.req_text("name")?,
        labels: texts(&row, "labels")?,
        meta: meta(&row)?,
    })
}

/// `CREATE TYPE ... AS ENUM`.
pub(super) fn enum_ddl(enum_type: &PgEnumType) -> CatalogResult<String> {
    let name = qname(&enum_type.schema, &enum_type.name)?;
    let labels = enum_type
        .labels
        .iter()
        .map(|label| lit(label))
        .collect::<CatalogResult<Vec<_>>>()?;
    let create = if labels.is_empty() {
        format!("CREATE TYPE {name} AS ENUM ()")
    } else {
        format!(
            "CREATE TYPE {name} AS ENUM (\n    {}\n)",
            labels.join(",\n    ")
        )
    };
    finish(create, &format!("TYPE {name}"), &enum_type.meta, Vec::new())
}

fn finish(create: String, target: &str, meta: &PgMeta, tail: Vec<String>) -> CatalogResult<String> {
    let mut statements = vec![create];
    statements.extend(owner_to(target, meta.owner.as_deref())?);
    statements.extend(comment_on(target, meta.comment.as_deref())?);
    statements.extend(tail);
    Ok(script(statements))
}

/// Details of a composite type.
pub(super) async fn describe_composite(
    session: &dyn Session,
    object: &ObjectRef,
) -> CatalogResult<PgCompositeType> {
    let filter = identity(object)?;
    let main = base(
        object.kind,
        &filter,
        ", pg_catalog.pg_get_userbyid(t.typowner) AS owner",
    );
    let attributes = format!(
        "SELECT a.attname AS name, pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type, \
         CASE WHEN a.attcollation OPERATOR(pg_catalog.<>) 0 AND a.attcollation OPERATOR(pg_catalog.<>) ty.typcollation \
         THEN {collation} END AS collation, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) a.attrelid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) a.attnum) AS comment, a.attnum \
         FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.typnamespace \
         JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) t.typrelid \
         JOIN pg_catalog.pg_type ty ON ty.oid OPERATOR(pg_catalog.=) a.atttypid \
         WHERE t.typtype OPERATOR(pg_catalog.=) 'c' AND a.attnum OPERATOR(pg_catalog.>) 0 AND NOT a.attisdropped{filter} \
         ORDER BY a.attnum",
        collation = collation_sql("a.attcollation"),
    );
    let sets = fetch_pinned(
        session,
        &[
            (with_system(&main, true), "q.name".to_owned()),
            (attributes, "q.attnum".to_owned()),
        ],
    )
    .await?;
    let [main_rows, attribute_rows] = sets.as_slice() else {
        return Err(wrong_sets());
    };
    let row = single_main(main_rows, object)?;
    let attributes = attribute_rows
        .iter()
        .map(|a| {
            Ok(PgAttribute {
                name: a.req_text("name")?,
                data_type: a.req_text("data_type")?,
                collation: a.text("collation")?,
                comment: a.text("comment")?,
            })
        })
        .collect::<CatalogResult<Vec<_>>>()?;
    Ok(PgCompositeType {
        schema: row.req_text("schema")?,
        name: row.req_text("name")?,
        attributes,
        meta: meta(&row)?,
    })
}

/// `CREATE TYPE ... AS (...)` with attribute comments.
pub(super) fn composite_ddl(composite: &PgCompositeType) -> CatalogResult<String> {
    let name = qname(&composite.schema, &composite.name)?;
    let mut columns = Vec::new();
    let mut comments = Vec::new();
    for attribute in &composite.attributes {
        let mut column = format!("{} {}", ident(&attribute.name)?, attribute.data_type);
        if let Some(collation) = &attribute.collation {
            column.push_str(&format!(" COLLATE {collation}"));
        }
        columns.push(column);
        comments.extend(comment_on(
            &format!("COLUMN {name}.{}", ident(&attribute.name)?),
            attribute.comment.as_deref(),
        )?);
    }
    let create = if columns.is_empty() {
        format!("CREATE TYPE {name} AS ()")
    } else {
        format!(
            "CREATE TYPE {name} AS (\n    {}\n)",
            columns.join(",\n    ")
        )
    };
    finish(create, &format!("TYPE {name}"), &composite.meta, comments)
}

/// Details of a domain.
pub(super) async fn describe_domain(
    session: &dyn Session,
    object: &ObjectRef,
) -> CatalogResult<PgDomain> {
    let filter = identity(object)?;
    let main = base(
        object.kind,
        &filter,
        &format!(
            ", pg_catalog.pg_get_userbyid(t.typowner) AS owner, \
             pg_catalog.format_type(t.typbasetype, t.typtypmod) AS base_type, \
             CASE WHEN t.typcollation OPERATOR(pg_catalog.<>) 0 AND t.typcollation OPERATOR(pg_catalog.<>) \
             (SELECT b.typcollation FROM pg_catalog.pg_type b WHERE b.oid OPERATOR(pg_catalog.=) t.typbasetype) \
             THEN {collation} END AS collation, \
             pg_catalog.pg_get_expr(t.typdefaultbin, 0) AS default_value, t.typnotnull AS not_null",
            collation = collation_sql("t.typcollation"),
        ),
    );
    let constraints = format!(
        "SELECT c.conname AS name, pg_catalog.pg_get_constraintdef(c.oid) AS definition, \
         c.convalidated AS validated, c.contype::pg_catalog.text OPERATOR(pg_catalog.=) 'n' AS not_null, \
         (SELECT cmt.description FROM pg_catalog.pg_description cmt WHERE cmt.objoid OPERATOR(pg_catalog.=) c.oid AND cmt.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_constraint'::pg_catalog.regclass AND cmt.objsubid OPERATOR(pg_catalog.=) 0) AS comment \
         FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.typnamespace \
         JOIN pg_catalog.pg_constraint c ON c.contypid OPERATOR(pg_catalog.=) t.oid \
         WHERE t.typtype OPERATOR(pg_catalog.=) 'd'{filter} ORDER BY c.conname"
    );
    let sets = fetch_pinned(
        session,
        &[
            (with_system(&main, true), "q.name".to_owned()),
            (constraints, "q.name".to_owned()),
        ],
    )
    .await?;
    let [main_rows, constraint_rows] = sets.as_slice() else {
        return Err(wrong_sets());
    };
    let row = single_main(main_rows, object)?;
    let constraints = constraint_rows
        .iter()
        .map(|c| {
            Ok(PgDomainConstraint {
                name: c.req_text("name")?,
                definition: c.req_text("definition")?,
                validated: flag(&c, "validated")?,
                not_null: flag(&c, "not_null")?,
                comment: c.text("comment")?,
            })
        })
        .collect::<CatalogResult<Vec<_>>>()?;
    Ok(PgDomain {
        schema: row.req_text("schema")?,
        name: row.req_text("name")?,
        base_type: row.req_text("base_type")?,
        collation: row.text("collation")?,
        default: row.text("default_value")?,
        not_null: flag(&row, "not_null")?,
        constraints,
        meta: meta(&row)?,
    })
}

/// `CREATE DOMAIN`: validated constraints inline, constraints that are not
/// validated added afterwards with `NOT VALID`.
pub(super) fn domain_ddl(domain: &PgDomain) -> CatalogResult<String> {
    let name = qname(&domain.schema, &domain.name)?;
    let mut create = format!("CREATE DOMAIN {name} AS {}", domain.base_type);
    if let Some(collation) = &domain.collation {
        create.push_str(&format!(" COLLATE {collation}"));
    }
    if let Some(default) = &domain.default {
        create.push_str(&format!(" DEFAULT {default}"));
    }
    let named_not_null = domain.constraints.iter().any(|c| c.not_null);
    if domain.not_null && !named_not_null {
        create.push_str(" NOT NULL");
    }
    let mut tail = Vec::new();
    for constraint in &domain.constraints {
        let clause = format!(
            "CONSTRAINT {} {}",
            ident(&constraint.name)?,
            constraint.definition
        );
        if constraint.validated {
            create.push_str(&format!("\n    {clause}"));
        } else {
            tail.push(format!("ALTER DOMAIN {name} ADD {clause}"));
        }
    }
    for constraint in &domain.constraints {
        tail.extend(comment_on(
            &format!("CONSTRAINT {} ON DOMAIN {name}", ident(&constraint.name)?),
            constraint.comment.as_deref(),
        )?);
    }
    finish(create, &format!("DOMAIN {name}"), &domain.meta, tail)
}

/// Details of a range type.
pub(super) async fn describe_range(
    session: &dyn Session,
    server: &ServerInfo,
    object: &ObjectRef,
) -> CatalogResult<PgRangeType> {
    let multirange = if server.at_least(14, 0) {
        "(SELECT pg_catalog.quote_ident(mn.nspname) OPERATOR(pg_catalog.||) '.' OPERATOR(pg_catalog.||) pg_catalog.quote_ident(m.typname) FROM pg_catalog.pg_type m \
         JOIN pg_catalog.pg_namespace mn ON mn.oid OPERATOR(pg_catalog.=) m.typnamespace WHERE m.oid OPERATOR(pg_catalog.=) r.rngmultitypid)"
    } else {
        "NULL::pg_catalog.text"
    };
    let main = base(
        object.kind,
        &identity(object)?,
        &format!(
            ", pg_catalog.pg_get_userbyid(t.typowner) AS owner, \
             (SELECT pg_catalog.format_type(r.rngsubtype, NULL) FROM pg_catalog.pg_range r WHERE r.rngtypid OPERATOR(pg_catalog.=) t.oid) \
             AS subtype, \
             (SELECT pg_catalog.quote_ident(ocn.nspname) OPERATOR(pg_catalog.||) '.' OPERATOR(pg_catalog.||) pg_catalog.quote_ident(oc.opcname) \
              FROM pg_catalog.pg_range r JOIN pg_catalog.pg_opclass oc ON oc.oid OPERATOR(pg_catalog.=) r.rngsubopc \
              JOIN pg_catalog.pg_namespace ocn ON ocn.oid OPERATOR(pg_catalog.=) oc.opcnamespace WHERE r.rngtypid OPERATOR(pg_catalog.=) t.oid) \
             AS subtype_opclass, \
             (SELECT {collation} FROM pg_catalog.pg_range r WHERE r.rngtypid OPERATOR(pg_catalog.=) t.oid \
              AND r.rngcollation OPERATOR(pg_catalog.<>) 0) AS collation, \
             (SELECT (CASE WHEN r.rngcanonical::pg_catalog.oid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN r.rngcanonical::pg_catalog.oid END)::pg_catalog.regproc::pg_catalog.text FROM pg_catalog.pg_range r \
              WHERE r.rngtypid OPERATOR(pg_catalog.=) t.oid) AS canonical, \
             (SELECT (CASE WHEN r.rngsubdiff::pg_catalog.oid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid THEN r.rngsubdiff::pg_catalog.oid END)::pg_catalog.regproc::pg_catalog.text FROM pg_catalog.pg_range r \
              WHERE r.rngtypid OPERATOR(pg_catalog.=) t.oid) AS subtype_diff, \
             (SELECT {multirange} FROM pg_catalog.pg_range r WHERE r.rngtypid OPERATOR(pg_catalog.=) t.oid) AS multirange",
            collation = collation_sql("r.rngcollation"),
        ),
    );
    let sets = fetch_pinned(session, &[(with_system(&main, true), "q.name".to_owned())]).await?;
    let rows = sets.first().ok_or_else(wrong_sets)?;
    let row = single_main(rows, object)?;
    Ok(PgRangeType {
        schema: row.req_text("schema")?,
        name: row.req_text("name")?,
        subtype: row.req_text("subtype")?,
        subtype_opclass: row.req_text("subtype_opclass")?,
        collation: row.text("collation")?,
        canonical: row.text("canonical")?,
        subtype_diff: row.text("subtype_diff")?,
        multirange: row.text("multirange")?,
        meta: meta(&row)?,
    })
}

/// `CREATE TYPE ... AS RANGE`.
pub(super) fn range_ddl(range: &PgRangeType) -> CatalogResult<String> {
    let name = qname(&range.schema, &range.name)?;
    let mut options = vec![
        format!("SUBTYPE = {}", range.subtype),
        format!("SUBTYPE_OPCLASS = {}", range.subtype_opclass),
    ];
    if let Some(collation) = &range.collation {
        options.push(format!("COLLATION = {collation}"));
    }
    if let Some(canonical) = &range.canonical {
        options.push(format!("CANONICAL = {canonical}"));
    }
    if let Some(diff) = &range.subtype_diff {
        options.push(format!("SUBTYPE_DIFF = {diff}"));
    }
    if let Some(multirange) = &range.multirange {
        options.push(format!("MULTIRANGE_TYPE_NAME = {multirange}"));
    }
    let create = format!(
        "CREATE TYPE {name} AS RANGE (\n    {}\n)",
        options.join(",\n    ")
    );
    finish(create, &format!("TYPE {name}"), &range.meta, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_defer_unvalidated_constraints() -> CatalogResult<()> {
        let domain = PgDomain {
            schema: "s".into(),
            name: "d".into(),
            base_type: "integer".into(),
            collation: None,
            default: Some("1".into()),
            not_null: true,
            constraints: vec![
                PgDomainConstraint {
                    name: "pos".into(),
                    definition: "CHECK ((VALUE > 0))".into(),
                    validated: true,
                    not_null: false,
                    comment: Some("positive".into()),
                },
                PgDomainConstraint {
                    name: "small".into(),
                    definition: "CHECK ((VALUE < 10)) NOT VALID".into(),
                    validated: false,
                    not_null: false,
                    comment: None,
                },
            ],
            meta: PgMeta::default(),
        };
        assert_eq!(
            domain_ddl(&domain)?,
            "CREATE DOMAIN \"s\".\"d\" AS integer DEFAULT 1 NOT NULL\n    \
             CONSTRAINT \"pos\" CHECK ((VALUE > 0));\n\n\
             ALTER DOMAIN \"s\".\"d\" ADD CONSTRAINT \"small\" CHECK ((VALUE < 10)) NOT VALID;\n\n\
             COMMENT ON CONSTRAINT \"pos\" ON DOMAIN \"s\".\"d\" IS 'positive';\n"
        );
        Ok(())
    }

    #[test]
    fn named_not_null_constraints_replace_the_flag() -> CatalogResult<()> {
        let domain = PgDomain {
            schema: "s".into(),
            name: "d".into(),
            base_type: "text".into(),
            collation: Some("pg_catalog.\"C\"".into()),
            default: None,
            not_null: true,
            constraints: vec![PgDomainConstraint {
                name: "d_not_null".into(),
                definition: "NOT NULL".into(),
                validated: true,
                not_null: true,
                comment: None,
            }],
            meta: PgMeta::default(),
        };
        assert_eq!(
            domain_ddl(&domain)?,
            "CREATE DOMAIN \"s\".\"d\" AS text COLLATE pg_catalog.\"C\"\n    \
             CONSTRAINT \"d_not_null\" NOT NULL;\n"
        );
        Ok(())
    }

    #[test]
    fn enums_quote_labels() -> CatalogResult<()> {
        let enum_type = PgEnumType {
            schema: "s".into(),
            name: "e".into(),
            labels: vec!["it's".into(), "b".into()],
            meta: PgMeta::default(),
        };
        assert_eq!(
            enum_ddl(&enum_type)?,
            "CREATE TYPE \"s\".\"e\" AS ENUM (\n    'it''s',\n    'b'\n);\n"
        );
        Ok(())
    }
}
