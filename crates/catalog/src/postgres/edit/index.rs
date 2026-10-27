//! Indexes, and the index elements shared with constraints and partition
//! keys.

use serde::{Deserialize, Serialize};
use varve_driver::ServerInfo;

use super::common::{
    DropBehavior, PlanBuilder, QualifiedName, StorageParam, comment_on, expr, ident, ident_list,
    invalid, opt_paren_expr, require, storage_param_names, storage_params,
};
use crate::model::{CatalogResult, EditPlan};

/// Index access method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum IndexMethod {
    /// B-tree (the default).
    Btree,
    /// Hash.
    Hash,
    /// GiST.
    Gist,
    /// SP-GiST.
    Spgist,
    /// GIN.
    Gin,
    /// BRIN.
    Brin,
    /// An access method from an extension (for example `bloom`).
    Custom {
        /// The access method's name.
        name: String,
    },
}

impl IndexMethod {
    pub(crate) fn sql(&self) -> CatalogResult<String> {
        Ok(match self {
            Self::Btree => "btree".into(),
            Self::Hash => "hash".into(),
            Self::Gist => "gist".into(),
            Self::Spgist => "spgist".into(),
            Self::Gin => "gin".into(),
            Self::Brin => "brin".into(),
            Self::Custom { name } => ident(name)?,
        })
    }

    /// Whether the method orders entries (ASC / DESC / NULLS are allowed).
    fn orders(&self) -> bool {
        matches!(self, Self::Btree | Self::Custom { .. })
    }
}

/// What an index element or partition key is built on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum IndexKey {
    /// A column.
    Column(String),
    /// An expression, written without the surrounding parentheses.
    Expression(String),
}

impl IndexKey {
    pub(crate) fn sql(&self) -> CatalogResult<String> {
        match self {
            Self::Column(c) => ident(c),
            Self::Expression(e) => Ok(format!("({})", expr(e, "an index expression")?)),
        }
    }
}

/// Sort direction of an index element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum SortOrder {
    /// `ASC`.
    Asc,
    /// `DESC`.
    Desc,
}

/// Where nulls sort in an index element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum NullsOrder {
    /// `NULLS FIRST`.
    First,
    /// `NULLS LAST`.
    Last,
}

/// One element of an index, exclusion constraint or partition key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct IndexElement {
    /// Column or expression.
    pub key: IndexKey,
    /// `COLLATE`; without a schema the collation resolves through the
    /// `search_path`.
    #[serde(default)]
    pub collation: Option<QualifiedName>,
    /// Operator class, for example `text_pattern_ops` or `gin_trgm_ops`.
    #[serde(default)]
    pub opclass: Option<QualifiedName>,
    /// Operator class parameters (13+), for example `siglen = 32`; needs
    /// `opclass`.
    #[serde(default)]
    pub opclass_options: Vec<StorageParam>,
    /// `ASC` / `DESC` (ordered methods only).
    #[serde(default)]
    pub order: Option<SortOrder>,
    /// `NULLS FIRST` / `NULLS LAST` (ordered methods only).
    #[serde(default)]
    pub nulls: Option<NullsOrder>,
}

impl IndexElement {
    /// An element on a plain column.
    #[must_use]
    pub fn column(name: impl Into<String>) -> Self {
        Self {
            key: IndexKey::Column(name.into()),
            collation: None,
            opclass: None,
            opclass_options: Vec::new(),
            order: None,
            nulls: None,
        }
    }

    /// An element on an expression.
    #[must_use]
    pub fn expression(sql: impl Into<String>) -> Self {
        Self {
            key: IndexKey::Expression(sql.into()),
            ..Self::column(String::new())
        }
    }

    pub(crate) fn sql(&self, server: &ServerInfo, method: &IndexMethod) -> CatalogResult<String> {
        let mut s = self.key.sql()?;
        if let Some(c) = &self.collation {
            s.push_str(" COLLATE ");
            s.push_str(&c.sql()?);
        }
        if let Some(op) = &self.opclass {
            s.push(' ');
            s.push_str(&op.sql()?);
        }
        if !self.opclass_options.is_empty() {
            require(server, 13, "operator class parameters")?;
            if self.opclass.is_none() {
                return Err(invalid(
                    "operator class parameters need the operator class they belong to",
                ));
            }
            s.push_str(&format!(" ({})", storage_params(&self.opclass_options)?));
        }
        if (self.order.is_some() || self.nulls.is_some()) && !method.orders() {
            return Err(invalid(format!(
                "{} indexes do not sort, so ASC, DESC and NULLS FIRST/LAST do not apply",
                method.sql()?
            )));
        }
        match self.order {
            Some(SortOrder::Asc) => s.push_str(" ASC"),
            Some(SortOrder::Desc) => s.push_str(" DESC"),
            None => {}
        }
        match self.nulls {
            Some(NullsOrder::First) => s.push_str(" NULLS FIRST"),
            Some(NullsOrder::Last) => s.push_str(" NULLS LAST"),
            None => {}
        }
        Ok(s)
    }
}

/// Checks an `INCLUDE` list against the method.
pub(crate) fn include_sql(
    server: &ServerInfo,
    method: &IndexMethod,
    include: &[String],
) -> CatalogResult<String> {
    if include.is_empty() {
        return Ok(String::new());
    }
    match method {
        IndexMethod::Btree | IndexMethod::Gist | IndexMethod::Custom { .. } => {}
        IndexMethod::Spgist => require(server, 14, "INCLUDE on SP-GiST indexes")?,
        _ => {
            return Err(invalid(format!(
                "{} indexes do not support INCLUDE",
                method.sql()?
            )));
        }
    }
    Ok(format!(" INCLUDE ({})", ident_list(include)?))
}

/// `CREATE INDEX`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateIndex {
    /// Index name (created in the table's schema); `None` lets PostgreSQL
    /// choose one.
    #[serde(default)]
    pub name: Option<String>,
    /// The table or materialized view.
    pub table: QualifiedName,
    /// `UNIQUE` (B-tree only).
    #[serde(default)]
    pub unique: bool,
    /// `NULLS NOT DISTINCT` for unique indexes (15+).
    #[serde(default)]
    pub nulls_not_distinct: bool,
    /// `CONCURRENTLY`: does not block writes, cannot run in a transaction.
    #[serde(default)]
    pub concurrently: bool,
    /// `IF NOT EXISTS` (needs a name).
    #[serde(default)]
    pub if_not_exists: bool,
    /// `ON ONLY`: for a partitioned table, do not create the index on the
    /// partitions (the index stays invalid until each is attached).
    #[serde(default)]
    pub only: bool,
    /// Access method (B-tree when absent).
    #[serde(default)]
    pub method: Option<IndexMethod>,
    /// Indexed columns and expressions.
    pub elements: Vec<IndexElement>,
    /// `INCLUDE` columns (covering index).
    #[serde(default)]
    pub include: Vec<String>,
    /// Storage parameters.
    #[serde(default)]
    pub storage_params: Vec<StorageParam>,
    /// Tablespace.
    #[serde(default)]
    pub tablespace: Option<String>,
    /// `WHERE` predicate of a partial index.
    #[serde(default)]
    pub predicate: Option<String>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// One change of `ALTER INDEX`, applied in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum IndexOperation {
    /// Rename.
    Rename {
        /// New name.
        new_name: String,
    },
    /// Move to another tablespace (rewrites the index).
    SetTablespace {
        /// The tablespace.
        tablespace: String,
    },
    /// `SET (...)`.
    SetStorageParams {
        /// Parameters.
        params: Vec<StorageParam>,
    },
    /// `RESET (...)`.
    ResetStorageParams {
        /// Parameter names.
        names: Vec<String>,
    },
    /// Attach an index of a partition to this partitioned index.
    AttachPartition {
        /// The partition's index.
        index: QualifiedName,
    },
    /// `ALTER COLUMN n SET STATISTICS target` of an expression column
    /// (1-based position in the index; -1 restores the default).
    SetStatistics {
        /// Column position, from 1.
        column: u16,
        /// Statistics target.
        target: i32,
    },
}

/// What `REINDEX` rebuilds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ReindexTarget {
    /// One index.
    Index {
        /// The index.
        name: QualifiedName,
    },
    /// Every index of a table.
    Table {
        /// The table.
        name: QualifiedName,
    },
    /// Every index in a schema.
    Schema {
        /// The schema.
        name: String,
    },
    /// Every index in the current database (the name must be the current
    /// database).
    Database {
        /// The database.
        name: String,
    },
}

/// Changes to indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum IndexChange {
    /// Create an index.
    Create(CreateIndex),
    /// Alter an index.
    Alter {
        /// The index.
        name: QualifiedName,
        /// Changes in order.
        operations: Vec<IndexOperation>,
    },
    /// Drop an index.
    Drop {
        /// The index.
        name: QualifiedName,
        /// `CONCURRENTLY`: does not block the table, cannot run in a
        /// transaction and cannot cascade.
        #[serde(default)]
        concurrently: bool,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent objects.
        #[serde(default)]
        behavior: DropBehavior,
    },
    /// Rebuild indexes.
    Reindex {
        /// What to rebuild.
        target: ReindexTarget,
        /// `CONCURRENTLY`: does not block writes, cannot run in a transaction.
        #[serde(default)]
        concurrently: bool,
        /// Move the rebuilt indexes to this tablespace (14+).
        #[serde(default)]
        tablespace: Option<String>,
        /// `VERBOSE`.
        #[serde(default)]
        verbose: bool,
    },
}

pub(crate) fn plan_index(server: &ServerInfo, change: &IndexChange) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        IndexChange::Create(create) => create_index(server, create, &mut plan)?,
        IndexChange::Alter { name, operations } => {
            if operations.is_empty() {
                return Err(invalid("no index changes given"));
            }
            let mut current = name.clone();
            for op in operations {
                let index = current.sql()?;
                let statement = match op {
                    IndexOperation::Rename { new_name } => {
                        let s = format!("ALTER INDEX {index} RENAME TO {}", ident(new_name)?);
                        current = current.renamed(new_name);
                        s
                    }
                    IndexOperation::SetTablespace { tablespace } => {
                        plan.warn("moving an index to another tablespace copies it while holding an ACCESS EXCLUSIVE lock on the index");
                        format!("ALTER INDEX {index} SET TABLESPACE {}", ident(tablespace)?)
                    }
                    IndexOperation::SetStorageParams { params } => {
                        plan.warn("most index storage parameters only take effect after REINDEX");
                        format!("ALTER INDEX {index} SET ({})", storage_params(params)?)
                    }
                    IndexOperation::ResetStorageParams { names } => {
                        format!(
                            "ALTER INDEX {index} RESET ({})",
                            storage_param_names(names)?
                        )
                    }
                    IndexOperation::AttachPartition { index: child } => {
                        format!("ALTER INDEX {index} ATTACH PARTITION {}", child.sql()?)
                    }
                    IndexOperation::SetStatistics { column, target } => {
                        if *column == 0 {
                            return Err(invalid("index columns are numbered from 1"));
                        }
                        format!("ALTER INDEX {index} ALTER COLUMN {column} SET STATISTICS {target}")
                    }
                };
                plan.push(statement);
            }
        }
        IndexChange::Drop {
            name,
            concurrently,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            let mut s = String::from("DROP INDEX ");
            if *concurrently {
                if *behavior == DropBehavior::Cascade {
                    return Err(invalid("DROP INDEX CONCURRENTLY cannot cascade"));
                }
                plan.non_transactional();
                s.push_str("CONCURRENTLY ");
            } else {
                plan.warn("DROP INDEX takes an ACCESS EXCLUSIVE lock on the table; DROP INDEX CONCURRENTLY does not");
            }
            if *if_exists {
                s.push_str("IF EXISTS ");
            }
            s.push_str(&name.sql()?);
            s.push(' ');
            s.push_str(behavior.sql());
            plan.push(s);
        }
        IndexChange::Reindex {
            target,
            concurrently,
            tablespace,
            verbose,
        } => {
            let mut options = Vec::new();
            if *verbose {
                options.push("VERBOSE".to_owned());
            }
            if let Some(ts) = tablespace {
                require(server, 14, "REINDEX ... TABLESPACE")?;
                options.push(format!("TABLESPACE {}", ident(ts)?));
            }
            let mut s = String::from("REINDEX ");
            if !options.is_empty() {
                s.push_str(&format!("({}) ", options.join(", ")));
            }
            let (what, name, whole) = match target {
                ReindexTarget::Index { name } => ("INDEX", name.sql()?, false),
                ReindexTarget::Table { name } => ("TABLE", name.sql()?, false),
                ReindexTarget::Schema { name } => ("SCHEMA", ident(name)?, true),
                ReindexTarget::Database { name } => ("DATABASE", ident(name)?, true),
            };
            // REINDEX of a partitioned table or index (14+) cannot run in a
            // transaction block, and the plan cannot tell whether the target
            // is partitioned: on 14 and later a single REINDEX always runs
            // on its own.
            if whole || *concurrently || server.at_least(14, 0) {
                plan.non_transactional();
            }
            s.push_str(what);
            s.push(' ');
            if *concurrently {
                s.push_str("CONCURRENTLY ");
                plan.warn("REINDEX CONCURRENTLY leaves an invalid index behind when it fails; drop it and try again");
            } else {
                plan.warn(
                    "REINDEX blocks writes to the table and reads that use the index while it runs",
                );
            }
            s.push_str(&name);
            plan.push(s);
        }
    }
    plan.finish()
}

fn create_index(
    server: &ServerInfo,
    create: &CreateIndex,
    plan: &mut PlanBuilder,
) -> CatalogResult<()> {
    if create.elements.is_empty() {
        return Err(invalid("an index needs at least one column or expression"));
    }
    let method = create.method.clone().unwrap_or(IndexMethod::Btree);
    if create.unique && !matches!(method, IndexMethod::Btree | IndexMethod::Custom { .. }) {
        return Err(invalid(format!(
            "{} indexes cannot be unique",
            method.sql()?
        )));
    }
    let mut s = String::from("CREATE ");
    if create.unique {
        s.push_str("UNIQUE ");
    }
    s.push_str("INDEX ");
    if create.concurrently {
        if create.only {
            return Err(invalid("CREATE INDEX CONCURRENTLY cannot use ON ONLY"));
        }
        plan.non_transactional();
        plan.warn("CREATE INDEX CONCURRENTLY leaves an invalid index behind when it fails; drop it and try again");
        s.push_str("CONCURRENTLY ");
    } else {
        plan.warn("CREATE INDEX blocks writes to the table while it builds; CONCURRENTLY does not");
    }
    if create.if_not_exists {
        if create.name.is_none() {
            return Err(invalid("IF NOT EXISTS needs an index name"));
        }
        s.push_str("IF NOT EXISTS ");
    }
    if let Some(name) = &create.name {
        s.push_str(&ident(name)?);
        s.push(' ');
    }
    s.push_str("ON ");
    if create.only {
        s.push_str("ONLY ");
    }
    s.push_str(&create.table.sql()?);
    if create.method.is_some() {
        s.push_str(" USING ");
        s.push_str(&method.sql()?);
    }
    let elements = create
        .elements
        .iter()
        .map(|e| e.sql(server, &method))
        .collect::<CatalogResult<Vec<_>>>()?;
    s.push_str(&format!(" ({})", elements.join(", ")));
    s.push_str(&include_sql(server, &method, &create.include)?);
    if create.nulls_not_distinct {
        require(server, 15, "NULLS NOT DISTINCT")?;
        if !create.unique {
            return Err(invalid("NULLS NOT DISTINCT only applies to unique indexes"));
        }
        s.push_str(" NULLS NOT DISTINCT");
    }
    if !create.storage_params.is_empty() {
        s.push_str(&format!(
            " WITH ({})",
            storage_params(&create.storage_params)?
        ));
    }
    if let Some(ts) = &create.tablespace {
        s.push_str(" TABLESPACE ");
        s.push_str(&ident(ts)?);
    }
    if let Some(p) = opt_paren_expr(create.predicate.as_deref(), "an index predicate")? {
        s.push_str(" WHERE ");
        s.push_str(&p);
    }
    plan.push(s);
    if let Some(comment) = &create.comment {
        let Some(name) = &create.name else {
            return Err(invalid("a comment needs an index name"));
        };
        let index = QualifiedName {
            schema: create.table.schema.clone(),
            name: name.clone(),
        };
        if create.concurrently {
            return Err(invalid(
                "CREATE INDEX CONCURRENTLY runs on its own; set the comment in a separate change",
            ));
        }
        plan.push(comment_on(
            &format!("INDEX {}", index.sql()?),
            Some(comment),
        )?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::edit::common::tests::pg;

    fn base() -> CreateIndex {
        CreateIndex {
            name: Some("i".into()),
            table: QualifiedName::new("s", "t"),
            unique: false,
            nulls_not_distinct: false,
            concurrently: false,
            if_not_exists: false,
            only: false,
            method: None,
            elements: vec![IndexElement::column("a")],
            include: vec![],
            storage_params: vec![],
            tablespace: None,
            predicate: None,
            comment: None,
        }
    }

    #[test]
    fn create_index_variants() -> CatalogResult<()> {
        let full = CreateIndex {
            unique: true,
            nulls_not_distinct: true,
            if_not_exists: true,
            method: Some(IndexMethod::Btree),
            elements: vec![
                IndexElement {
                    collation: Some(QualifiedName::bare("C")),
                    order: Some(SortOrder::Desc),
                    nulls: Some(NullsOrder::Last),
                    ..IndexElement::column("a")
                },
                IndexElement {
                    opclass: Some(QualifiedName::bare("text_pattern_ops")),
                    ..IndexElement::expression("lower(b)")
                },
            ],
            include: vec!["c".into()],
            storage_params: vec![StorageParam::new("fillfactor", "80")],
            tablespace: Some("pg_default".into()),
            predicate: Some("c > 0".into()),
            comment: Some("x".into()),
            ..base()
        };
        let plan = plan_index(&pg(15), &IndexChange::Create(full.clone()))?;
        assert_eq!(
            plan.statements,
            vec![
                r#"CREATE UNIQUE INDEX IF NOT EXISTS "i" ON "s"."t" USING btree ("a" COLLATE "C" DESC NULLS LAST, (lower(b)) "text_pattern_ops") INCLUDE ("c") NULLS NOT DISTINCT WITH ("fillfactor" = 80) TABLESPACE "pg_default" WHERE (c > 0)"#,
                r#"COMMENT ON INDEX "s"."i" IS 'x'"#,
            ]
        );
        assert!(plan.transactional && !plan.destructive);
        assert!(plan_index(&pg(14), &IndexChange::Create(full)).is_err());

        let conc = plan_index(
            &pg(13),
            &IndexChange::Create(CreateIndex {
                name: None,
                concurrently: true,
                method: Some(IndexMethod::Gin),
                elements: vec![IndexElement {
                    opclass: Some(QualifiedName::new("public", "gin_trgm_ops")),
                    ..IndexElement::column("a")
                }],
                ..base()
            }),
        )?;
        assert_eq!(
            conc.statements,
            vec![r#"CREATE INDEX CONCURRENTLY ON "s"."t" USING gin ("a" "public"."gin_trgm_ops")"#]
        );
        assert!(!conc.transactional);

        let only = plan_index(
            &pg(13),
            &IndexChange::Create(CreateIndex {
                only: true,
                ..base()
            }),
        )?;
        assert_eq!(
            only.statements,
            vec![r#"CREATE INDEX "i" ON ONLY "s"."t" ("a")"#]
        );

        for method in [
            IndexMethod::Hash,
            IndexMethod::Gist,
            IndexMethod::Spgist,
            IndexMethod::Brin,
        ] {
            let p = plan_index(
                &pg(13),
                &IndexChange::Create(CreateIndex {
                    method: Some(method.clone()),
                    ..base()
                }),
            )?;
            assert_eq!(
                p.statements,
                vec![format!(
                    r#"CREATE INDEX "i" ON "s"."t" USING {} ("a")"#,
                    method.sql()?
                )]
            );
        }
        let bloom = plan_index(
            &pg(13),
            &IndexChange::Create(CreateIndex {
                method: Some(IndexMethod::Custom {
                    name: "bloom".into(),
                }),
                ..base()
            }),
        )?;
        assert_eq!(
            bloom.statements,
            vec![r#"CREATE INDEX "i" ON "s"."t" USING "bloom" ("a")"#]
        );

        // Rules per method.
        let bad = |c: CreateIndex| plan_index(&pg(18), &IndexChange::Create(c)).is_err();
        assert!(bad(CreateIndex {
            unique: true,
            method: Some(IndexMethod::Hash),
            ..base()
        }));
        assert!(bad(CreateIndex {
            include: vec!["b".into()],
            method: Some(IndexMethod::Gin),
            ..base()
        }));
        assert!(bad(CreateIndex {
            method: Some(IndexMethod::Gist),
            elements: vec![IndexElement {
                order: Some(SortOrder::Asc),
                ..IndexElement::column("a")
            }],
            ..base()
        }));
        assert!(bad(CreateIndex {
            nulls_not_distinct: true,
            ..base()
        }));
        assert!(bad(CreateIndex {
            elements: vec![],
            ..base()
        }));
        assert!(bad(CreateIndex {
            predicate: Some("a; drop table x".into()),
            ..base()
        }));
        assert!(
            plan_index(
                &pg(13),
                &IndexChange::Create(CreateIndex {
                    include: vec!["b".into()],
                    method: Some(IndexMethod::Spgist),
                    ..base()
                })
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn element_options_and_names_needing_quotes() -> CatalogResult<()> {
        let create = CreateIndex {
            method: Some(IndexMethod::Gist),
            elements: vec![IndexElement {
                collation: Some(QualifiedName::new("my schema", "Fr\"ench")),
                opclass: Some(QualifiedName::new("pg_catalog", "tsvector_ops")),
                opclass_options: vec![StorageParam::new("siglen", "100")],
                ..IndexElement::column("Body \"Text\"")
            }],
            include: vec!["OrderId".into()],
            ..base()
        };
        let plan = plan_index(&pg(13), &IndexChange::Create(create.clone()))?;
        assert_eq!(
            plan.statements,
            vec![
                r#"CREATE INDEX "i" ON "s"."t" USING gist ("Body ""Text""" COLLATE "my schema"."Fr""ench" "pg_catalog"."tsvector_ops" ("siglen" = 100)) INCLUDE ("OrderId")"#
            ]
        );
        // Parameters without the class they belong to.
        let mut no_class = create;
        if let Some(e) = no_class.elements.first_mut() {
            e.opclass = None;
        }
        assert!(plan_index(&pg(18), &IndexChange::Create(no_class)).is_err());
        Ok(())
    }

    #[test]
    fn methods_from_extensions_round_trip_as_custom() -> Result<(), serde_json::Error> {
        let hnsw: IndexMethod =
            serde_json::from_value(serde_json::json!({"kind": "custom", "name": "hnsw"}))?;
        assert_eq!(
            hnsw,
            IndexMethod::Custom {
                name: "hnsw".into()
            }
        );
        assert!(
            serde_json::from_value::<IndexMethod>(serde_json::json!({"kind": "hnsw"})).is_err()
        );
        let element: IndexElement = serde_json::from_value(serde_json::json!({
            "key": {"column": "a"},
            "collation": {"schema": "pg_catalog", "name": "C"},
            "opclass": {"schema": null, "name": "vector_l2_ops"},
            "opclassOptions": [{"name": "siglen", "value": "8"}]
        }))?;
        assert_eq!(
            element.collation,
            Some(QualifiedName::new("pg_catalog", "C"))
        );
        assert_eq!(
            element.opclass_options,
            vec![StorageParam::new("siglen", "8")]
        );
        Ok(())
    }

    #[test]
    fn alter_drop_reindex() -> CatalogResult<()> {
        let alter = plan_index(
            &pg(13),
            &IndexChange::Alter {
                name: QualifiedName::new("s", "i"),
                operations: vec![
                    IndexOperation::Rename {
                        new_name: "j".into(),
                    },
                    IndexOperation::SetTablespace {
                        tablespace: "pg_default".into(),
                    },
                    IndexOperation::SetStorageParams {
                        params: vec![StorageParam::new("fillfactor", "70")],
                    },
                    IndexOperation::ResetStorageParams {
                        names: vec!["fillfactor".into()],
                    },
                    IndexOperation::AttachPartition {
                        index: QualifiedName::new("s", "p_i"),
                    },
                    IndexOperation::SetStatistics {
                        column: 2,
                        target: 500,
                    },
                ],
            },
        )?;
        assert_eq!(
            alter.statements,
            vec![
                r#"ALTER INDEX "s"."i" RENAME TO "j""#,
                r#"ALTER INDEX "s"."j" SET TABLESPACE "pg_default""#,
                r#"ALTER INDEX "s"."j" SET ("fillfactor" = 70)"#,
                r#"ALTER INDEX "s"."j" RESET ("fillfactor")"#,
                r#"ALTER INDEX "s"."j" ATTACH PARTITION "s"."p_i""#,
                r#"ALTER INDEX "s"."j" ALTER COLUMN 2 SET STATISTICS 500"#,
            ]
        );
        let drop = plan_index(
            &pg(13),
            &IndexChange::Drop {
                name: QualifiedName::new("s", "i"),
                concurrently: true,
                if_exists: true,
                behavior: DropBehavior::Restrict,
            },
        )?;
        assert_eq!(
            drop.statements,
            vec![r#"DROP INDEX CONCURRENTLY IF EXISTS "s"."i" RESTRICT"#]
        );
        assert!(drop.destructive && !drop.transactional);
        assert!(
            plan_index(
                &pg(13),
                &IndexChange::Drop {
                    name: QualifiedName::new("s", "i"),
                    concurrently: true,
                    if_exists: false,
                    behavior: DropBehavior::Cascade,
                },
            )
            .is_err()
        );

        let reindex = plan_index(
            &pg(14),
            &IndexChange::Reindex {
                target: ReindexTarget::Table {
                    name: QualifiedName::new("s", "t"),
                },
                concurrently: true,
                tablespace: Some("pg_default".into()),
                verbose: true,
            },
        )?;
        assert_eq!(
            reindex.statements,
            vec![r#"REINDEX (VERBOSE, TABLESPACE "pg_default") TABLE CONCURRENTLY "s"."t""#]
        );
        assert!(!reindex.transactional);
        let simple = plan_index(
            &pg(13),
            &IndexChange::Reindex {
                target: ReindexTarget::Index {
                    name: QualifiedName::new("s", "i"),
                },
                concurrently: false,
                tablespace: None,
                verbose: false,
            },
        )?;
        assert_eq!(simple.statements, vec![r#"REINDEX INDEX "s"."i""#]);
        assert!(simple.transactional);
        // A partitioned target cannot be reindexed in a transaction on 14+.
        for major in [14, 18] {
            let p = plan_index(
                &pg(major),
                &IndexChange::Reindex {
                    target: ReindexTarget::Table {
                        name: QualifiedName::new("s", "t"),
                    },
                    concurrently: false,
                    tablespace: None,
                    verbose: false,
                },
            )?;
            assert!(!p.transactional);
        }
        let schema = plan_index(
            &pg(13),
            &IndexChange::Reindex {
                target: ReindexTarget::Schema { name: "s".into() },
                concurrently: false,
                tablespace: None,
                verbose: false,
            },
        )?;
        assert_eq!(schema.statements, vec![r#"REINDEX SCHEMA "s""#]);
        assert!(!schema.transactional);
        assert!(
            plan_index(
                &pg(13),
                &IndexChange::Reindex {
                    target: ReindexTarget::Database { name: "d".into() },
                    concurrently: false,
                    tablespace: Some("x".into()),
                    verbose: false,
                },
            )
            .is_err()
        );
        Ok(())
    }
}
