//! Integration tests for the PostgreSQL editors: every plan runs against
//! the oldest and newest supported servers of the M2 matrix, and the result
//! is read back from `pg_catalog` / `information_schema` with queries
//! written here.
//!
//! ```sh
//! cargo test -p varve-catalog --test pg_edit -- --include-ignored
//! ```
//!
//! Every test works in its own schema or database named `m2_pgedit_*`,
//! drops it first (leftovers of a failed run) and again at the end.

use std::time::Duration;

use varve_catalog::model::{CatalogError, EditPlan, ObjectKind, ObjectRef};
use varve_catalog::postgres::edit::*;
use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, ExecOptions, ExecOutput, Session, StatementResult};
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type R<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

/// Whether the server with `dev/docker-compose.yml` service name `service`
/// (for example `postgres-18`) is tested. `VARVE_ONLY_SERVERS`, a comma
/// separated list of service names, limits a run to those servers (CI tests
/// only the newest version of each engine); without it every server is tested.
fn selected(service: &str) -> bool {
    std::env::var("VARVE_ONLY_SERVERS")
        .ok()
        .is_none_or(|list| list.split(',').any(|name| name.trim() == service))
}

/// `(major, port)` of the oldest and newest servers (see [`selected`]),
/// never none.
fn servers() -> Vec<(u32, u16)> {
    let servers: Vec<(u32, u16)> = [
        (13, port("VARVE_PG13_PORT", 55513)),
        (18, port("VARVE_PG18_PORT", 55518)),
    ]
    .into_iter()
    .filter(|(major, _)| selected(&format!("postgres-{major}")))
    .collect();
    assert!(
        !servers.is_empty(),
        "VARVE_ONLY_SERVERS selects no PostgreSQL server"
    );
    servers
}

async fn connect(port: u16, database: &str) -> R<Box<dyn Session>> {
    let mut params =
        ConnectParams::new(Engine::Postgres, env_or("VARVE_PG_HOST", "127.0.0.1"), port);
    params.database = Some(database.to_owned());
    params.username = Some(env_or("VARVE_PG_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_PG_PASSWORD", "varve_dev")));
    params.connect_timeout = Duration::from_secs(10);
    params.application_name = "varve-catalog pg_edit tests".into();
    Ok(PostgresDriver::new().connect(&params).await?)
}

async fn exec(s: &dyn Session, sql: &str) -> R<ExecOutput> {
    Ok(s.execute(sql, &ExecOptions::default()).await?)
}

/// Rows of the first result set, every value as text.
async fn rows(s: &dyn Session, sql: &str) -> R<Vec<Vec<Option<String>>>> {
    let out = exec(s, sql).await?;
    for result in out.results {
        if let StatementResult::Rows { rows, .. } = result {
            return Ok(rows
                .into_iter()
                .map(|r| r.iter().map(|v| v.to_plain_text()).collect())
                .collect());
        }
    }
    Err(format!("no rows from {sql}").into())
}

/// The first value of the first row.
async fn one(s: &dyn Session, sql: &str) -> R<Option<String>> {
    Ok(rows(s, sql)
        .await?
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .flatten())
}

async fn text(s: &dyn Session, sql: &str) -> R<String> {
    one(s, sql)
        .await?
        .ok_or_else(|| format!("NULL from {sql}").into())
}

/// Runs a plan the way the app does: in one transaction when it says so.
async fn apply(s: &dyn Session, plan: &EditPlan) -> R {
    if plan.transactional {
        exec(s, "BEGIN").await?;
        for statement in &plan.statements {
            if let Err(e) = exec(s, statement).await {
                exec(s, "ROLLBACK").await?;
                return Err(format!("{statement}\n=> {e}").into());
            }
        }
        exec(s, "COMMIT").await?;
    } else {
        for statement in &plan.statements {
            exec(s, statement)
                .await
                .map_err(|e| format!("{statement}\n=> {e}"))?;
        }
    }
    Ok(())
}

async fn run(s: &dyn Session, change: PgChange) -> R<EditPlan> {
    let plan = varve_catalog::postgres::edit::plan(s.server_info(), &change)?;
    apply(s, &plan).await?;
    Ok(plan)
}

fn q(schema: &str, name: &str) -> QualifiedName {
    QualifiedName::new(schema, name)
}

async fn reset_schema(s: &dyn Session, schemas: &[&str]) -> R {
    for schema in schemas {
        exec(s, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).await?;
    }
    Ok(())
}

async fn create_schema(s: &dyn Session, schema: &str) -> R {
    run(
        s,
        PgChange::Schema(SchemaChange::Create {
            name: schema.into(),
            owner: None,
            if_not_exists: false,
            comment: None,
        }),
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn databases_and_schemas() -> R {
    for (major, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        for db in ["m2_pgedit_db", "m2_pgedit_db2", "m2_pgedit_icu"] {
            exec(s, &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await?;
        }
        let create = CreateDatabase {
            name: "m2_pgedit_db".into(),
            owner: Some(RoleSpec::name(env_or("VARVE_PG_USER", "varve"))),
            template: Some("template0".into()),
            encoding: Some("UTF8".into()),
            strategy: (major >= 15).then_some(CreateStrategy::FileCopy),
            lc_collate: Some("C".into()),
            lc_ctype: Some("C".into()),
            connection_limit: Some(5),
            allow_connections: Some(true),
            is_template: Some(false),
            comment: Some("made by m2_pgedit".into()),
            ..CreateDatabase::default()
        };
        let plan = run(
            s,
            PgChange::Database(DatabaseChange::Create(Box::new(create))),
        )
        .await?;
        assert!(!plan.transactional);
        assert_eq!(
            rows(s, "SELECT datconnlimit::text, pg_encoding_to_char(encoding), datcollate::text, shobj_description(oid, 'pg_database') FROM pg_database WHERE datname = 'm2_pgedit_db'").await?,
            vec![vec![Some("5".into()), Some("UTF8".into()), Some("C".into()), Some("made by m2_pgedit".into())]]
        );
        run(
            s,
            PgChange::Database(DatabaseChange::Alter {
                name: "m2_pgedit_db".into(),
                operations: vec![
                    DatabaseOperation::Set {
                        setting: ConfigSetting {
                            name: "work_mem".into(),
                            values: vec!["8MB".into()],
                        },
                    },
                    DatabaseOperation::ConnectionLimit { limit: 7 },
                    DatabaseOperation::AllowConnections { allow: true },
                    DatabaseOperation::Rename {
                        new_name: "m2_pgedit_db2".into(),
                    },
                    DatabaseOperation::Owner {
                        owner: RoleSpec::CurrentUser,
                    },
                ],
            }),
        )
        .await?;
        assert_eq!(
            text(
                s,
                "SELECT datconnlimit::text FROM pg_database WHERE datname = 'm2_pgedit_db2'"
            )
            .await?,
            "7"
        );
        let settings = "SELECT array_to_string(setconfig, ',') FROM pg_db_role_setting r JOIN pg_database d ON d.oid = r.setdatabase WHERE d.datname = 'm2_pgedit_db2' AND r.setrole = 0";
        assert_eq!(one(s, settings).await?, Some("work_mem=8MB".into()));
        run(
            s,
            PgChange::Database(DatabaseChange::Alter {
                name: "m2_pgedit_db2".into(),
                operations: vec![DatabaseOperation::Reset {
                    name: "work_mem".into(),
                }],
            }),
        )
        .await?;
        assert!(rows(s, settings).await?.is_empty());
        if major >= 15 {
            run(
                s,
                PgChange::Database(DatabaseChange::Create(Box::new(CreateDatabase {
                    name: "m2_pgedit_icu".into(),
                    template: Some("template0".into()),
                    locale_provider: Some(LocaleProvider::Icu),
                    icu_locale: Some("en-US".into()),
                    ..CreateDatabase::default()
                }))),
            )
            .await?;
            assert_eq!(
                text(
                    s,
                    "SELECT datlocprovider::text FROM pg_database WHERE datname = 'm2_pgedit_icu'"
                )
                .await?,
                "i"
            );
        }
        for db in ["m2_pgedit_db2", "m2_pgedit_icu"] {
            let plan = run(
                s,
                PgChange::Database(DatabaseChange::Drop {
                    name: db.into(),
                    if_exists: true,
                    force: true,
                }),
            )
            .await?;
            assert!(plan.destructive);
        }
        assert_eq!(
            text(
                s,
                "SELECT count(*)::text FROM pg_database WHERE datname IN ('m2_pgedit_db', 'm2_pgedit_db2', 'm2_pgedit_icu')"
            )
            .await?,
            "0"
        );

        // Schemas.
        reset_schema(s, &["m2_pgedit_sch", "m2_pgedit_sch2"]).await?;
        run(
            s,
            PgChange::Schema(SchemaChange::Create {
                name: "m2_pgedit_sch".into(),
                owner: Some(RoleSpec::CurrentUser),
                if_not_exists: true,
                comment: Some("schema comment".into()),
            }),
        )
        .await?;
        assert_eq!(
            one(s, "SELECT obj_description(oid, 'pg_namespace') FROM pg_namespace WHERE nspname = 'm2_pgedit_sch'").await?,
            Some("schema comment".into())
        );
        run(
            s,
            PgChange::Schema(SchemaChange::Alter {
                name: "m2_pgedit_sch".into(),
                operations: vec![
                    SchemaOperation::Rename {
                        new_name: "m2_pgedit_sch2".into(),
                    },
                    SchemaOperation::Owner {
                        owner: RoleSpec::SessionUser,
                    },
                ],
            }),
        )
        .await?;
        exec(s, "CREATE TABLE m2_pgedit_sch2.t (a int)").await?;
        assert!(
            varve_catalog::postgres::edit::plan(
                s.server_info(),
                &PgChange::Schema(SchemaChange::Drop {
                    name: "m2_pgedit_sch2".into(),
                    if_exists: false,
                    behavior: DropBehavior::Restrict,
                })
            )
            .map(|p| p.destructive)?
        );
        assert!(
            run(
                s,
                PgChange::Schema(SchemaChange::Drop {
                    name: "m2_pgedit_sch2".into(),
                    if_exists: false,
                    behavior: DropBehavior::Restrict,
                }),
            )
            .await
            .is_err(),
            "RESTRICT must refuse a schema with objects"
        );
        run(
            s,
            PgChange::Schema(SchemaChange::Drop {
                name: "m2_pgedit_sch2".into(),
                if_exists: false,
                behavior: DropBehavior::Cascade,
            }),
        )
        .await?;
        assert_eq!(
            text(
                s,
                "SELECT count(*)::text FROM pg_namespace WHERE nspname LIKE 'm2_pgedit_sch%'"
            )
            .await?,
            "0"
        );
    }
    Ok(())
}

fn items_table(major: u32) -> CreateTable {
    const S: &str = "m2_pgedit_tbl";
    let mut constraints = vec![
        TableConstraint {
            name: Some("items_pk".into()),
            ..TableConstraint::new(ConstraintKind::PrimaryKey {
                columns: vec!["id".into()],
                include: vec!["name".into()],
                storage_params: vec![StorageParam::new("fillfactor", "90")],
                index_tablespace: Some("pg_default".into()),
            })
        },
        TableConstraint {
            name: Some("items_name_uq".into()),
            deferrable: true,
            initially_deferred: true,
            ..TableConstraint::new(ConstraintKind::Unique {
                columns: vec!["name".into()],
                include: vec![],
                nulls_not_distinct: major >= 15,
                storage_params: vec![],
                index_tablespace: None,
            })
        },
        TableConstraint {
            name: Some("qty_pos".into()),
            ..TableConstraint::new(ConstraintKind::Check {
                expression: "qty > 0".into(),
                no_inherit: false,
            })
        },
        TableConstraint {
            name: Some("no_overlap".into()),
            ..TableConstraint::new(ConstraintKind::Exclusion {
                method: None,
                elements: vec![ExclusionElement {
                    element: IndexElement::column("during"),
                    operator: "&&".into(),
                    operator_schema: None,
                }],
                include: vec![],
                storage_params: vec![],
                index_tablespace: None,
                predicate: Some("ref_id IS NOT NULL".into()),
            })
        },
        TableConstraint {
            name: Some("items_ref".into()),
            ..TableConstraint::new(ConstraintKind::ForeignKey {
                columns: vec!["ref_id".into()],
                references: q(S, "ref"),
                referenced_columns: vec!["id".into()],
                match_type: Some(MatchType::Full),
                on_delete: Some(ReferentialAction::SetNull {
                    columns: if major >= 15 {
                        vec!["ref_id".into()]
                    } else {
                        vec![]
                    },
                }),
                on_update: Some(ReferentialAction::Cascade),
            })
        },
    ];
    constraints.shrink_to_fit();
    CreateTable {
        name: q(S, "items"),
        if_not_exists: false,
        unlogged: false,
        columns: vec![
            ColumnDef {
                not_null: true,
                identity: Some(Identity {
                    generation: IdentityGeneration::Always,
                    options: SequenceOptions {
                        start: Some(100),
                        increment: Some(10),
                        ..SequenceOptions::default()
                    },
                }),
                comment: Some("key".into()),
                ..ColumnDef::new("id", "bigint")
            },
            ColumnDef {
                collation: Some("C".into()),
                default: Some("'x'".into()),
                storage: Some(ColumnStorage::External),
                compression: (major >= 14).then_some(Compression::Lz4),
                ..ColumnDef::new("name", "text")
            },
            ColumnDef {
                generated: Some(Generated {
                    expression: "upper(name)".into(),
                    storage: GeneratedStorage::Stored,
                }),
                ..ColumnDef::new("upper_name", "text")
            },
            ColumnDef::new("ref_id", "integer"),
            ColumnDef::new("during", "tstzrange"),
            ColumnDef {
                not_null: true,
                default: Some("1".into()),
                ..ColumnDef::new("qty", "integer")
            },
        ],
        constraints,
        inherits: vec![],
        partition_of: None,
        partition_by: None,
        storage_params: vec![StorageParam::new("fillfactor", "80")],
        tablespace: Some("pg_default".into()),
        comment: Some("items table".into()),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn table_designer() -> R {
    const S: &str = "m2_pgedit_tbl";
    const S2: &str = "m2_pgedit_tbl2";
    for (major, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S, S2]).await?;
        create_schema(s, S).await?;
        create_schema(s, S2).await?;
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(CreateTable {
                name: q(S, "ref"),
                if_not_exists: true,
                unlogged: false,
                columns: vec![ColumnDef {
                    not_null: true,
                    ..ColumnDef::new("id", "integer")
                }],
                constraints: vec![TableConstraint::new(ConstraintKind::PrimaryKey {
                    columns: vec!["id".into()],
                    include: vec![],
                    storage_params: vec![],
                    index_tablespace: None,
                })],
                inherits: vec![],
                partition_of: None,
                partition_by: None,
                storage_params: vec![],
                tablespace: None,
                comment: None,
            }))),
        )
        .await?;
        let plan = run(
            s,
            PgChange::Table(TableChange::Create(Box::new(items_table(major)))),
        )
        .await?;
        assert!(plan.transactional && !plan.destructive);

        let column = |c: &str| {
            format!(
                "SELECT is_nullable, column_default, is_identity, identity_generation, identity_start, identity_increment, is_generated, generation_expression, collation_name FROM information_schema.columns WHERE table_schema = '{S}' AND table_name = 'items' AND column_name = '{c}'"
            )
        };
        let id = rows(s, &column("id")).await?;
        assert_eq!(
            id[0][..6],
            [
                Some("NO".into()),
                None,
                Some("YES".into()),
                Some("ALWAYS".into()),
                Some("100".into()),
                Some("10".into())
            ]
        );
        let name = rows(s, &column("name")).await?;
        assert_eq!(name[0][1], Some("'x'::text".into()));
        assert_eq!(name[0][8], Some("C".into()));
        let upper = rows(s, &column("upper_name")).await?;
        assert_eq!(upper[0][6], Some("ALWAYS".into()));
        assert_eq!(upper[0][7], Some("upper(name)".into()));
        assert_eq!(
            text(s, &format!("SELECT attstorage::text FROM pg_attribute WHERE attrelid = '{S}.items'::regclass AND attname = 'name'")).await?,
            "e"
        );
        if major >= 14 {
            assert_eq!(
                text(s, &format!("SELECT attcompression::text FROM pg_attribute WHERE attrelid = '{S}.items'::regclass AND attname = 'name'")).await?,
                "l"
            );
        }
        assert_eq!(
            text(s, &format!("SELECT string_agg(conname || ':' || contype::text, ',' ORDER BY conname) FROM pg_constraint WHERE conrelid = '{S}.items'::regclass AND contype <> 'n'")).await?,
            "items_name_uq:u,items_pk:p,items_ref:f,no_overlap:x,qty_pos:c"
        );
        assert_eq!(
            rows(s, &format!("SELECT condeferrable::text, condeferred::text FROM pg_constraint WHERE conname = 'items_name_uq' AND conrelid = '{S}.items'::regclass")).await?,
            vec![vec![Some("true".into()), Some("true".into())]]
        );
        assert_eq!(
            rows(s, &format!("SELECT confdeltype::text, confupdtype::text, confmatchtype::text FROM pg_constraint WHERE conname = 'items_ref' AND conrelid = '{S}.items'::regclass")).await?,
            vec![vec![Some("n".into()), Some("c".into()), Some("f".into())]]
        );
        if major >= 15 {
            assert!(text(s, &format!("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'items_name_uq' AND conrelid = '{S}.items'::regclass")).await?.contains("NULLS NOT DISTINCT"));
            assert!(text(s, &format!("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'items_ref' AND conrelid = '{S}.items'::regclass")).await?.contains("SET NULL (ref_id)"));
        }
        assert!(text(s, &format!("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'items_pk' AND conrelid = '{S}.items'::regclass")).await?.contains("INCLUDE (name)"));
        assert_eq!(
            rows(s, &format!("SELECT array_to_string(reloptions, ','), obj_description('{S}.items'::regclass, 'pg_class'), col_description('{S}.items'::regclass, 1) FROM pg_class WHERE oid = '{S}.items'::regclass")).await?,
            vec![vec![Some("fillfactor=80".into()), Some("items table".into()), Some("key".into())]]
        );
        assert_eq!(
            text(
                s,
                &format!("INSERT INTO {S}.items (name, qty) VALUES ('a', 2) RETURNING id::text")
            )
            .await?,
            "100"
        );

        // Alter as an ordered list of operations.
        let mut operations = vec![
            TableOperation::AddColumn {
                column: ColumnDef {
                    default: Some("'n'".into()),
                    comment: Some("a note".into()),
                    ..ColumnDef::new("note", "text")
                },
                if_not_exists: true,
            },
            TableOperation::AddColumn {
                column: ColumnDef::new("tmp", "integer"),
                if_not_exists: false,
            },
            TableOperation::DropColumn {
                name: "tmp".into(),
                if_exists: true,
                behavior: DropBehavior::Restrict,
            },
            TableOperation::RenameColumn {
                name: "note".into(),
                new_name: "memo".into(),
            },
            TableOperation::DropConstraint {
                name: "qty_pos".into(),
                if_exists: false,
                behavior: DropBehavior::Restrict,
            },
            TableOperation::AlterColumnType {
                name: "qty".into(),
                data_type: "bigint".into(),
                collation: None,
                using: Some("qty::bigint".into()),
            },
            TableOperation::SetDefault {
                column: "qty".into(),
                expression: "5".into(),
            },
            TableOperation::DropNotNull {
                column: "qty".into(),
            },
            TableOperation::SetNotNull {
                column: "qty".into(),
            },
            TableOperation::AddConstraint {
                constraint: TableConstraint {
                    name: Some("memo_len".into()),
                    ..TableConstraint::new(ConstraintKind::Check {
                        expression: "length(memo) < 100".into(),
                        no_inherit: false,
                    })
                },
                not_valid: true,
            },
            TableOperation::ValidateConstraint {
                name: "memo_len".into(),
            },
            TableOperation::RenameConstraint {
                name: "memo_len".into(),
                new_name: "memo_short".into(),
            },
            TableOperation::AlterIdentity {
                column: "id".into(),
                generation: Some(IdentityGeneration::ByDefault),
                options: SequenceOptions {
                    increment: Some(5),
                    ..SequenceOptions::default()
                },
                restart: Some(Restart { with: Some(1000) }),
            },
            TableOperation::AddColumn {
                column: ColumnDef {
                    not_null: true,
                    default: Some("0".into()),
                    ..ColumnDef::new("seq_no", "integer")
                },
                if_not_exists: false,
            },
            TableOperation::DropDefault {
                column: "seq_no".into(),
            },
            TableOperation::AddIdentity {
                column: "seq_no".into(),
                identity: Identity {
                    generation: IdentityGeneration::ByDefault,
                    options: SequenceOptions::default(),
                },
            },
            TableOperation::DropIdentity {
                column: "seq_no".into(),
                if_exists: true,
            },
            TableOperation::DropNotNull {
                column: "seq_no".into(),
            },
            TableOperation::DropExpression {
                column: "upper_name".into(),
                if_exists: false,
            },
            TableOperation::SetStorage {
                column: "memo".into(),
                storage: ColumnStorage::Main,
            },
            TableOperation::SetStorageParams {
                params: vec![StorageParam::new("autovacuum_enabled", "false")],
            },
            TableOperation::ResetStorageParams {
                names: vec!["fillfactor".into()],
            },
            TableOperation::SetLogged { logged: false },
            TableOperation::SetLogged { logged: true },
            TableOperation::SetTablespace {
                tablespace: "pg_default".into(),
            },
            TableOperation::RowLevelSecurity { enabled: true },
            TableOperation::ForceRowLevelSecurity { forced: true },
            TableOperation::Owner {
                owner: RoleSpec::CurrentUser,
            },
        ];
        if major >= 17 {
            operations.push(TableOperation::AddColumn {
                column: ColumnDef {
                    generated: Some(Generated {
                        expression: "qty * 2".into(),
                        storage: GeneratedStorage::Stored,
                    }),
                    ..ColumnDef::new("g", "bigint")
                },
                if_not_exists: false,
            });
            operations.push(TableOperation::SetExpression {
                column: "g".into(),
                expression: "qty * 3".into(),
            });
        }
        if major >= 18 {
            operations.push(TableOperation::AddColumn {
                column: ColumnDef {
                    generated: Some(Generated {
                        expression: "qty + 1".into(),
                        storage: GeneratedStorage::Virtual,
                    }),
                    ..ColumnDef::new("v", "bigint")
                },
                if_not_exists: false,
            });
            operations.push(TableOperation::SetCompression {
                column: "memo".into(),
                compression: Compression::Pglz,
            });
        }
        operations.push(TableOperation::Rename {
            new_name: "things".into(),
        });
        operations.push(TableOperation::SetSchema { schema: S2.into() });
        let plan = run(
            s,
            PgChange::Table(TableChange::Alter {
                name: q(S, "items"),
                operations,
            }),
        )
        .await?;
        assert!(plan.transactional && plan.destructive);
        assert!(plan.warnings.iter().any(|w| w.contains("ACCESS EXCLUSIVE")));
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("rewrites the whole table"))
        );

        let column2 = |c: &str| {
            format!(
                "SELECT data_type, is_nullable, column_default, is_identity, identity_generation, identity_increment, is_generated, generation_expression FROM information_schema.columns WHERE table_schema = '{S2}' AND table_name = 'things' AND column_name = '{c}'"
            )
        };
        let qty = rows(s, &column2("qty")).await?;
        assert_eq!(
            qty[0][..3],
            [Some("bigint".into()), Some("NO".into()), Some("5".into())]
        );
        let id = rows(s, &column2("id")).await?;
        assert_eq!(id[0][4..6], [Some("BY DEFAULT".into()), Some("5".into())]);
        assert_eq!(
            rows(s, &column2("memo")).await?[0][2],
            Some("'n'::text".into())
        );
        assert_eq!(
            rows(s, &column2("upper_name")).await?[0][6],
            Some("NEVER".into())
        );
        assert_eq!(rows(s, &column2("seq_no")).await?[0][3], Some("NO".into()));
        assert!(rows(s, &column2("tmp")).await?.is_empty());
        assert_eq!(
            rows(s, &format!("SELECT relrowsecurity::text, relforcerowsecurity::text, relpersistence::text, array_to_string(reloptions, ',') FROM pg_class WHERE oid = '{S2}.things'::regclass")).await?,
            vec![vec![Some("true".into()), Some("true".into()), Some("p".into()), Some("autovacuum_enabled=false".into())]]
        );
        assert_eq!(
            text(s, &format!("SELECT string_agg(conname, ',' ORDER BY conname) FROM pg_constraint WHERE conrelid = '{S2}.things'::regclass AND contype <> 'n'")).await?,
            "items_name_uq,items_pk,items_ref,memo_short,no_overlap"
        );
        assert_eq!(
            text(s, &format!("SELECT attstorage::text FROM pg_attribute WHERE attrelid = '{S2}.things'::regclass AND attname = 'memo'")).await?,
            "m"
        );
        assert_eq!(
            one(s, &format!("SELECT col_description('{S2}.things'::regclass, attnum) FROM pg_attribute WHERE attrelid = '{S2}.things'::regclass AND attname = 'memo'")).await?,
            Some("a note".into())
        );
        if major >= 17 {
            assert_eq!(
                rows(s, &column2("g")).await?[0][7],
                Some("(qty * 3)".into())
            );
        }
        if major >= 18 {
            assert_eq!(
                text(s, &format!("SELECT attgenerated::text FROM pg_attribute WHERE attrelid = '{S2}.things'::regclass AND attname = 'v'")).await?,
                "v"
            );
        }
        // RLS is forced for the owner: disable it before writing.
        run(
            s,
            PgChange::Table(TableChange::Alter {
                name: q(S2, "things"),
                operations: vec![
                    TableOperation::ForceRowLevelSecurity { forced: false },
                    TableOperation::RowLevelSecurity { enabled: false },
                ],
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("INSERT INTO {S2}.things (name) VALUES ('b') RETURNING id::text || '/' || qty::text")).await?,
            "1000/5"
        );

        let drop = run(
            s,
            PgChange::Table(TableChange::Drop {
                name: q(S2, "things"),
                if_exists: false,
                behavior: DropBehavior::Restrict,
            }),
        )
        .await?;
        assert!(drop.destructive);
        assert_eq!(
            text(
                s,
                &format!("SELECT count(*)::text FROM pg_tables WHERE schemaname = '{S2}'")
            )
            .await?,
            "0"
        );
        reset_schema(s, &[S, S2]).await?;
    }
    Ok(())
}

fn plain_table(name: QualifiedName, columns: Vec<ColumnDef>) -> CreateTable {
    CreateTable {
        name,
        if_not_exists: false,
        unlogged: false,
        columns,
        constraints: vec![],
        inherits: vec![],
        partition_of: None,
        partition_by: None,
        storage_params: vec![],
        tablespace: None,
        comment: None,
    }
}

fn partition(name: QualifiedName, parent: QualifiedName, bound: PartitionBound) -> CreateTable {
    CreateTable {
        partition_of: Some(PartitionOf { parent, bound }),
        ..plain_table(name, vec![])
    }
}

fn lit(v: &str) -> BoundValue {
    BoundValue::Literal { value: v.into() }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn partitions_and_inheritance() -> R {
    const S: &str = "m2_pgedit_part";
    for (major, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        let columns = || {
            vec![
                ColumnDef::new("at", "date"),
                ColumnDef::new("kind", "text"),
                ColumnDef::new("v", "integer"),
            ]
        };
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(CreateTable {
                partition_by: Some(PartitionBy {
                    strategy: PartitionStrategy::Range,
                    keys: vec![PartitionKey {
                        key: IndexKey::Column("at".into()),
                        collation: None,
                        opclass: None,
                    }],
                }),
                ..plain_table(q(S, "events"), columns())
            }))),
        )
        .await?;
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(CreateTable {
                constraints: vec![TableConstraint::new(ConstraintKind::Check {
                    expression: "v >= 0".into(),
                    no_inherit: false,
                })],
                ..partition(
                    q(S, "events_2024"),
                    q(S, "events"),
                    PartitionBound::Range {
                        from: vec![lit("2024-01-01")],
                        to: vec![lit("2025-01-01")],
                    },
                )
            }))),
        )
        .await?;
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(partition(
                q(S, "events_default"),
                q(S, "events"),
                PartitionBound::Default,
            )))),
        )
        .await?;
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(plain_table(
                q(S, "events_2025"),
                columns(),
            )))),
        )
        .await?;
        let attach = run(
            s,
            PgChange::Table(TableChange::Alter {
                name: q(S, "events"),
                operations: vec![TableOperation::AttachPartition {
                    partition: q(S, "events_2025"),
                    bound: PartitionBound::Range {
                        from: vec![BoundValue::Expression {
                            sql: "'2025-01-01'::date".into(),
                        }],
                        to: vec![BoundValue::MaxValue],
                    },
                }],
            }),
        )
        .await?;
        assert!(
            attach
                .warnings
                .iter()
                .any(|w| w.contains("ATTACH PARTITION"))
        );
        let bounds = format!(
            "SELECT string_agg(c.relname || '=' || pg_get_expr(c.relpartbound, c.oid), '; ' ORDER BY c.relname) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid WHERE i.inhparent = '{S}.events'::regclass"
        );
        assert_eq!(
            text(s, &bounds).await?,
            "events_2024=FOR VALUES FROM ('2024-01-01') TO ('2025-01-01'); events_2025=FOR VALUES FROM ('2025-01-01') TO (MAXVALUE); events_default=DEFAULT"
        );
        run(
            s,
            PgChange::Table(TableChange::Alter {
                name: q(S, "events"),
                operations: vec![
                    TableOperation::DetachPartition {
                        partition: q(S, "events_2025"),
                        mode: DetachMode::Normal,
                    },
                    TableOperation::DetachPartition {
                        partition: q(S, "events_default"),
                        mode: DetachMode::Normal,
                    },
                    TableOperation::AttachPartition {
                        partition: q(S, "events_default"),
                        bound: PartitionBound::Default,
                    },
                ],
            }),
        )
        .await?;
        assert_eq!(
            text(s, &bounds).await?,
            "events_2024=FOR VALUES FROM ('2024-01-01') TO ('2025-01-01'); events_default=DEFAULT"
        );

        // List and hash partitioning.
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(CreateTable {
                partition_by: Some(PartitionBy {
                    strategy: PartitionStrategy::List,
                    keys: vec![PartitionKey {
                        key: IndexKey::Column("kind".into()),
                        collation: None,
                        opclass: None,
                    }],
                }),
                ..plain_table(q(S, "by_kind"), columns())
            }))),
        )
        .await?;
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(partition(
                q(S, "by_kind_a"),
                q(S, "by_kind"),
                PartitionBound::List {
                    values: vec![lit("a"), BoundValue::Null],
                },
            )))),
        )
        .await?;
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(CreateTable {
                partition_by: Some(PartitionBy {
                    strategy: PartitionStrategy::Hash,
                    keys: vec![PartitionKey {
                        key: IndexKey::Expression("v + 1".into()),
                        collation: None,
                        opclass: None,
                    }],
                }),
                ..plain_table(q(S, "by_hash"), columns())
            }))),
        )
        .await?;
        for r in 0..2 {
            run(
                s,
                PgChange::Table(TableChange::Create(Box::new(partition(
                    q(S, &format!("by_hash_{r}")),
                    q(S, "by_hash"),
                    PartitionBound::Hash {
                        modulus: 2,
                        remainder: r,
                    },
                )))),
            )
            .await?;
        }
        assert_eq!(
            text(s, &format!("SELECT pg_get_partkeydef('{S}.by_hash'::regclass) || ' / ' || pg_get_expr(relpartbound, oid) FROM pg_class WHERE oid = '{S}.by_kind_a'::regclass")).await?,
            "HASH (((v + 1))) / FOR VALUES IN ('a', NULL)"
        );
        if major >= 14 {
            // No default partition on by_kind, so CONCURRENTLY is allowed.
            let plan = run(
                s,
                PgChange::Table(TableChange::Alter {
                    name: q(S, "by_kind"),
                    operations: vec![TableOperation::DetachPartition {
                        partition: q(S, "by_kind_a"),
                        mode: DetachMode::Concurrently,
                    }],
                }),
            )
            .await?;
            assert!(!plan.transactional);
            assert_eq!(
                text(s, &format!("SELECT relispartition::text FROM pg_class WHERE oid = '{S}.by_kind_a'::regclass")).await?,
                "false"
            );
        }

        // Inheritance and unlogged tables.
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(plain_table(
                q(S, "base"),
                vec![ColumnDef::new("a", "integer")],
            )))),
        )
        .await?;
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(CreateTable {
                unlogged: true,
                inherits: vec![q(S, "base")],
                ..plain_table(q(S, "child"), vec![])
            }))),
        )
        .await?;
        let inherits = format!(
            "SELECT count(*)::text FROM pg_inherits WHERE inhrelid = '{S}.child'::regclass AND inhparent = '{S}.base'::regclass"
        );
        assert_eq!(text(s, &inherits).await?, "1");
        assert_eq!(
            text(
                s,
                &format!(
                    "SELECT relpersistence::text FROM pg_class WHERE oid = '{S}.child'::regclass"
                )
            )
            .await?,
            "u"
        );
        run(
            s,
            PgChange::Table(TableChange::Alter {
                name: q(S, "child"),
                operations: vec![TableOperation::NoInherit {
                    parent: q(S, "base"),
                }],
            }),
        )
        .await?;
        assert_eq!(text(s, &inherits).await?, "0");
        run(
            s,
            PgChange::Table(TableChange::Alter {
                name: q(S, "child"),
                operations: vec![TableOperation::Inherit {
                    parent: q(S, "base"),
                }],
            }),
        )
        .await?;
        assert_eq!(text(s, &inherits).await?, "1");
        run(
            s,
            PgChange::Table(TableChange::Drop {
                name: q(S, "base"),
                if_exists: true,
                behavior: DropBehavior::Cascade,
            }),
        )
        .await?;
        assert_eq!(text(s, &format!("SELECT count(*)::text FROM pg_class WHERE relname IN ('base', 'child') AND relnamespace = '{S}'::regnamespace")).await?, "0");
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}

fn index(table: QualifiedName, name: &str, elements: Vec<IndexElement>) -> CreateIndex {
    CreateIndex {
        name: Some(name.into()),
        table,
        unique: false,
        nulls_not_distinct: false,
        concurrently: false,
        if_not_exists: false,
        only: false,
        method: None,
        elements,
        include: vec![],
        storage_params: vec![],
        tablespace: None,
        predicate: None,
        comment: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn indexes() -> R {
    const S: &str = "m2_pgedit_idx";
    for (major, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        exec(s, &format!("CREATE TABLE {S}.docs (id int, title text, body text, tags text[], loc point, during tsrange, n int)")).await?;
        let docs = q(S, "docs");
        let uq = CreateIndex {
            unique: true,
            if_not_exists: true,
            method: Some(IndexMethod::Btree),
            elements: vec![IndexElement {
                collation: Some(QualifiedName::bare("C")),
                order: Some(SortOrder::Desc),
                nulls: Some(NullsOrder::Last),
                ..IndexElement::column("title")
            }],
            include: vec!["n".into()],
            storage_params: vec![StorageParam::new("fillfactor", "80")],
            tablespace: Some("pg_default".into()),
            predicate: Some("n > 0".into()),
            comment: Some("unique titles".into()),
            ..index(docs.clone(), "docs_title_uq", vec![])
        };
        run(s, PgChange::Index(IndexChange::Create(uq))).await?;
        let methods = [
            (
                "docs_hash",
                IndexMethod::Hash,
                IndexElement::column("title"),
            ),
            ("docs_gin", IndexMethod::Gin, IndexElement::column("tags")),
            (
                "docs_gist",
                IndexMethod::Gist,
                IndexElement::column("during"),
            ),
            (
                "docs_spgist",
                IndexMethod::Spgist,
                IndexElement::column("loc"),
            ),
            ("docs_brin", IndexMethod::Brin, IndexElement::column("n")),
            (
                "docs_lower",
                IndexMethod::Btree,
                IndexElement {
                    opclass: Some(QualifiedName::bare("text_pattern_ops")),
                    ..IndexElement::expression("lower(title)")
                },
            ),
        ];
        for (name, method, element) in methods.clone() {
            run(
                s,
                PgChange::Index(IndexChange::Create(CreateIndex {
                    method: Some(method),
                    ..index(docs.clone(), name, vec![element])
                })),
            )
            .await?;
        }
        let conc = run(
            s,
            PgChange::Index(IndexChange::Create(CreateIndex {
                concurrently: true,
                ..index(docs.clone(), "docs_n", vec![IndexElement::column("n")])
            })),
        )
        .await?;
        assert!(!conc.transactional);
        if major >= 15 {
            run(
                s,
                PgChange::Index(IndexChange::Create(CreateIndex {
                    unique: true,
                    nulls_not_distinct: true,
                    ..index(
                        docs.clone(),
                        "docs_body_nnd",
                        vec![IndexElement::column("body")],
                    )
                })),
            )
            .await?;
            assert_eq!(
                text(s, &format!("SELECT indnullsnotdistinct::text FROM pg_index WHERE indexrelid = '{S}.docs_body_nnd'::regclass")).await?,
                "true"
            );
        }
        let am = |name: &str| {
            format!(
                "SELECT a.amname::text FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = '{S}.{name}'::regclass"
            )
        };
        for (name, method, _) in methods {
            let expected = match method {
                IndexMethod::Hash => "hash",
                IndexMethod::Gin => "gin",
                IndexMethod::Gist => "gist",
                IndexMethod::Spgist => "spgist",
                IndexMethod::Brin => "brin",
                _ => "btree",
            };
            assert_eq!(text(s, &am(name)).await?, expected, "{name}");
        }
        assert_eq!(
            text(
                s,
                &format!("SELECT pg_get_indexdef('{S}.docs_lower'::regclass)")
            )
            .await?,
            format!(
                "CREATE INDEX docs_lower ON {S}.docs USING btree (lower(title) text_pattern_ops)"
            )
        );
        assert_eq!(
            text(
                s,
                &format!("SELECT pg_get_indexdef('{S}.docs_title_uq'::regclass)")
            )
            .await?,
            format!(
                "CREATE UNIQUE INDEX docs_title_uq ON {S}.docs USING btree (title COLLATE \"C\" DESC NULLS LAST) INCLUDE (n) WITH (fillfactor='80') WHERE (n > 0)"
            )
        );
        assert_eq!(
            one(
                s,
                &format!("SELECT obj_description('{S}.docs_title_uq'::regclass, 'pg_class')")
            )
            .await?,
            Some("unique titles".into())
        );

        run(
            s,
            PgChange::Index(IndexChange::Alter {
                name: q(S, "docs_title_uq"),
                operations: vec![
                    IndexOperation::Rename {
                        new_name: "docs_title_unique".into(),
                    },
                    IndexOperation::SetStorageParams {
                        params: vec![StorageParam::new("fillfactor", "70")],
                    },
                    IndexOperation::SetTablespace {
                        tablespace: "pg_default".into(),
                    },
                ],
            }),
        )
        .await?;
        assert_eq!(
            one(s, &format!("SELECT array_to_string(reloptions, ',') FROM pg_class WHERE oid = '{S}.docs_title_unique'::regclass")).await?,
            Some("fillfactor=70".into())
        );
        run(
            s,
            PgChange::Index(IndexChange::Alter {
                name: q(S, "docs_title_unique"),
                operations: vec![IndexOperation::ResetStorageParams {
                    names: vec!["fillfactor".into()],
                }],
            }),
        )
        .await?;
        assert_eq!(
            one(s, &format!("SELECT array_to_string(reloptions, ',') FROM pg_class WHERE oid = '{S}.docs_title_unique'::regclass")).await?,
            None
        );

        // REINDEX in every form.
        let reindex = |target, concurrently, tablespace: Option<&str>| {
            PgChange::Index(IndexChange::Reindex {
                target,
                concurrently,
                tablespace: tablespace.map(str::to_owned),
                verbose: false,
            })
        };
        // On 14+ a REINDEX may target a partitioned index, which cannot run
        // in a transaction, so the plan never asks for one there.
        assert_eq!(
            run(
                s,
                reindex(
                    ReindexTarget::Index {
                        name: q(S, "docs_gin")
                    },
                    false,
                    None
                )
            )
            .await?
            .transactional,
            major < 14
        );
        assert!(
            !run(
                s,
                reindex(ReindexTarget::Table { name: docs.clone() }, true, None)
            )
            .await?
            .transactional
        );
        assert!(
            !run(
                s,
                reindex(ReindexTarget::Schema { name: S.into() }, false, None)
            )
            .await?
            .transactional
        );
        if major >= 14 {
            run(
                s,
                reindex(
                    ReindexTarget::Index {
                        name: q(S, "docs_n"),
                    },
                    false,
                    Some("pg_default"),
                ),
            )
            .await?;
        }

        // Partitioned index: ON ONLY, then attach the partition's index.
        exec(s, &format!("CREATE TABLE {S}.p (a int) PARTITION BY LIST (a); CREATE TABLE {S}.p1 PARTITION OF {S}.p FOR VALUES IN (1)")).await?;
        run(
            s,
            PgChange::Index(IndexChange::Create(CreateIndex {
                only: true,
                ..index(q(S, "p"), "p_a", vec![IndexElement::column("a")])
            })),
        )
        .await?;
        let valid =
            format!("SELECT indisvalid::text FROM pg_index WHERE indexrelid = '{S}.p_a'::regclass");
        assert_eq!(text(s, &valid).await?, "false");
        run(
            s,
            PgChange::Index(IndexChange::Create(index(
                q(S, "p1"),
                "p1_a",
                vec![IndexElement::column("a")],
            ))),
        )
        .await?;
        run(
            s,
            PgChange::Index(IndexChange::Alter {
                name: q(S, "p_a"),
                operations: vec![IndexOperation::AttachPartition {
                    index: q(S, "p1_a"),
                }],
            }),
        )
        .await?;
        assert_eq!(text(s, &valid).await?, "true");

        let drop = run(
            s,
            PgChange::Index(IndexChange::Drop {
                name: q(S, "docs_n"),
                concurrently: true,
                if_exists: true,
                behavior: DropBehavior::Restrict,
            }),
        )
        .await?;
        assert!(drop.destructive && !drop.transactional);
        run(
            s,
            PgChange::Index(IndexChange::Drop {
                name: q(S, "docs_hash"),
                concurrently: false,
                if_exists: false,
                behavior: DropBehavior::Restrict,
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT count(*)::text FROM pg_class WHERE relname IN ('docs_n', 'docs_hash') AND relnamespace = '{S}'::regnamespace")).await?,
            "0"
        );
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn views_and_materialized_views() -> R {
    const S: &str = "m2_pgedit_view";
    for (major, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        exec(
            s,
            &format!(
                "CREATE TABLE {S}.t (a int, b text); INSERT INTO {S}.t VALUES (1, 'x'), (2, 'y')"
            ),
        )
        .await?;
        let view = CreateView {
            name: q(S, "v"),
            or_replace: false,
            columns: vec!["a".into(), "b".into()],
            query: format!("SELECT a, b FROM {S}.t WHERE a > 0"),
            check_option: Some(CheckOption::Cascaded),
            security_barrier: Some(true),
            security_invoker: (major >= 15).then_some(true),
            comment: Some("positive rows".into()),
        };
        run(s, PgChange::View(ViewChange::Create(view.clone()))).await?;
        let info = format!(
            "SELECT check_option FROM information_schema.views WHERE table_schema = '{S}' AND table_name = 'v'"
        );
        assert_eq!(text(s, &info).await?, "CASCADED");
        let options = format!(
            "SELECT array_to_string(reloptions, ',') FROM pg_class WHERE oid = '{S}.v'::regclass"
        );
        let expected = if major >= 15 {
            "security_barrier=true,security_invoker=true,check_option=cascaded"
        } else {
            "security_barrier=true,check_option=cascaded"
        };
        assert_eq!(text(s, &options).await?, expected);
        assert_eq!(
            one(
                s,
                &format!("SELECT obj_description('{S}.v'::regclass, 'pg_class')")
            )
            .await?,
            Some("positive rows".into())
        );
        run(
            s,
            PgChange::View(ViewChange::Create(CreateView {
                or_replace: true,
                columns: vec!["a".into(), "b".into(), "c".into()],
                query: format!("SELECT a, b, a * 2 FROM {S}.t WHERE a > 0"),
                comment: None,
                ..view
            })),
        )
        .await?;
        assert!(
            exec(s, &format!("INSERT INTO {S}.v (a, b) VALUES (-1, 'no')"))
                .await
                .is_err()
        );
        run(
            s,
            PgChange::View(ViewChange::Alter {
                name: q(S, "v"),
                operations: vec![
                    ViewOperation::SetOptions {
                        check_option: Some(CheckOption::Local),
                        security_barrier: None,
                        security_invoker: None,
                    },
                    ViewOperation::ResetOptions {
                        options: vec![ViewOption::SecurityBarrier],
                    },
                    ViewOperation::RenameColumn {
                        name: "c".into(),
                        new_name: "d".into(),
                    },
                    ViewOperation::SetColumnDefault {
                        column: "b".into(),
                        expression: "'dflt'".into(),
                    },
                    ViewOperation::Owner {
                        owner: RoleSpec::CurrentUser,
                    },
                    ViewOperation::Rename {
                        new_name: "v2".into(),
                    },
                ],
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT check_option || '/' || string_agg(c.column_name, ',' ORDER BY c.ordinal_position) FROM information_schema.views v JOIN information_schema.columns c ON c.table_schema = v.table_schema AND c.table_name = v.table_name WHERE v.table_schema = '{S}' AND v.table_name = 'v2' GROUP BY check_option")).await?,
            "LOCAL/a,b,d"
        );
        exec(s, &format!("INSERT INTO {S}.v2 (a) VALUES (3)")).await?;
        assert_eq!(
            text(s, &format!("SELECT b FROM {S}.t WHERE a = 3")).await?,
            "dflt"
        );
        run(
            s,
            PgChange::View(ViewChange::Drop {
                name: q(S, "v2"),
                if_exists: false,
                behavior: DropBehavior::Restrict,
            }),
        )
        .await?;
        assert_eq!(
            text(
                s,
                &format!("SELECT count(*)::text FROM pg_views WHERE schemaname = '{S}'")
            )
            .await?,
            "0"
        );

        // Materialized views.
        run(
            s,
            PgChange::MaterializedView(MaterializedViewChange::Create(CreateMaterializedView {
                name: q(S, "mv"),
                if_not_exists: false,
                columns: vec!["a".into()],
                query: format!("SELECT a FROM {S}.t"),
                with_data: false,
                storage_params: vec![StorageParam::new("fillfactor", "50")],
                tablespace: Some("pg_default".into()),
                comment: Some("mat".into()),
            })),
        )
        .await?;
        let populated =
            format!("SELECT relispopulated::text FROM pg_class WHERE oid = '{S}.mv'::regclass");
        assert_eq!(text(s, &populated).await?, "false");
        let refresh = |concurrently, has_unique_index, with_data| {
            PgChange::MaterializedView(MaterializedViewChange::Refresh {
                name: q(S, "mv"),
                concurrently,
                has_unique_index,
                with_data,
            })
        };
        assert!(matches!(
            varve_catalog::postgres::edit::plan(s.server_info(), &refresh(true, false, true)),
            Err(CatalogError::Invalid { .. })
        ));
        run(s, refresh(false, false, true)).await?;
        assert_eq!(text(s, &populated).await?, "true");
        run(
            s,
            PgChange::Index(IndexChange::Create(CreateIndex {
                unique: true,
                ..index(q(S, "mv"), "mv_a", vec![IndexElement::column("a")])
            })),
        )
        .await?;
        exec(s, &format!("INSERT INTO {S}.t VALUES (10, 'z')")).await?;
        let conc = run(s, refresh(true, true, true)).await?;
        assert!(conc.transactional);
        assert_eq!(
            text(s, &format!("SELECT count(*)::text FROM {S}.mv")).await?,
            "4"
        );
        let empty = run(s, refresh(false, true, false)).await?;
        assert!(empty.destructive);
        assert_eq!(text(s, &populated).await?, "false");
        run(
            s,
            PgChange::MaterializedView(MaterializedViewChange::Alter {
                name: q(S, "mv"),
                operations: vec![
                    MaterializedViewOperation::RenameColumn {
                        name: "a".into(),
                        new_name: "x".into(),
                    },
                    MaterializedViewOperation::SetStorageParams {
                        params: vec![StorageParam::new("fillfactor", "60")],
                    },
                    MaterializedViewOperation::SetTablespace {
                        tablespace: "pg_default".into(),
                    },
                    MaterializedViewOperation::Owner {
                        owner: RoleSpec::CurrentUser,
                    },
                    MaterializedViewOperation::Rename {
                        new_name: "mv2".into(),
                    },
                ],
            }),
        )
        .await?;
        assert_eq!(
            rows(s, &format!("SELECT array_to_string(c.reloptions, ','), a.attname::text, obj_description(c.oid, 'pg_class') FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = 1 WHERE c.oid = '{S}.mv2'::regclass")).await?,
            vec![vec![Some("fillfactor=60".into()), Some("x".into()), Some("mat".into())]]
        );
        run(
            s,
            PgChange::MaterializedView(MaterializedViewChange::Drop {
                name: q(S, "mv2"),
                if_exists: true,
                behavior: DropBehavior::Cascade,
            }),
        )
        .await?;
        assert_eq!(
            text(
                s,
                &format!("SELECT count(*)::text FROM pg_matviews WHERE schemaname = '{S}'")
            )
            .await?,
            "0"
        );
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn sequences() -> R {
    const S: &str = "m2_pgedit_seq";
    for (major, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        exec(s, &format!("CREATE TABLE {S}.t (id int)")).await?;
        run(
            s,
            PgChange::Sequence(SequenceChange::Create(CreateSequence {
                name: q(S, "ids"),
                if_not_exists: true,
                unlogged: major >= 15,
                options: SequenceOptions {
                    data_type: Some(SequenceType::Integer),
                    increment: Some(2),
                    min_value: Some(SequenceLimit::Value { value: 1 }),
                    max_value: Some(SequenceLimit::Value { value: 1000 }),
                    start: Some(10),
                    cache: Some(3),
                    cycle: Some(true),
                },
                owned_by: Some(ColumnRef {
                    table: q(S, "t"),
                    column: "id".into(),
                }),
                comment: Some("ids".into()),
            })),
        )
        .await?;
        let info = format!(
            "SELECT data_type::text, start_value::text, min_value::text, max_value::text, increment_by::text, cycle::text, cache_size::text FROM pg_sequences WHERE schemaname = '{S}' AND sequencename = 'ids'"
        );
        assert_eq!(
            rows(s, &info).await?,
            vec![
                ["integer", "10", "1", "1000", "2", "true", "3"]
                    .map(|v| Some(v.to_owned()))
                    .to_vec()
            ]
        );
        let owner = format!("SELECT pg_get_serial_sequence('{S}.t', 'id')");
        assert_eq!(one(s, &owner).await?, Some(format!("{S}.ids")));
        let persistence =
            format!("SELECT relpersistence::text FROM pg_class WHERE oid = '{S}.ids'::regclass");
        if major >= 15 {
            assert_eq!(text(s, &persistence).await?, "u");
        }
        assert_eq!(
            text(
                s,
                &format!("SELECT obj_description('{S}.ids'::regclass, 'pg_class')")
            )
            .await?,
            "ids"
        );
        let mut operations = vec![
            SequenceOperation::Options {
                options: SequenceOptions {
                    increment: Some(1),
                    max_value: Some(SequenceLimit::Default),
                    cycle: Some(false),
                    ..SequenceOptions::default()
                },
            },
            SequenceOperation::Restart { with: Some(50) },
        ];
        if major >= 15 {
            operations.push(SequenceOperation::Logged { logged: true });
        }
        run(
            s,
            PgChange::Sequence(SequenceChange::Alter {
                name: q(S, "ids"),
                operations,
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT nextval('{S}.ids')::text")).await?,
            "50"
        );
        assert_eq!(
            text(s, &format!("SELECT max_value::text || '/' || cycle::text FROM pg_sequences WHERE schemaname = '{S}' AND sequencename = 'ids'")).await?,
            "2147483647/false"
        );
        if major >= 15 {
            assert_eq!(text(s, &persistence).await?, "p");
        }
        run(
            s,
            PgChange::Sequence(SequenceChange::Alter {
                name: q(S, "ids"),
                operations: vec![
                    SequenceOperation::SetValue {
                        value: 42,
                        is_called: false,
                    },
                    SequenceOperation::OwnedBy { column: None },
                    SequenceOperation::Owner {
                        owner: RoleSpec::CurrentUser,
                    },
                    SequenceOperation::Rename {
                        new_name: "ids2".into(),
                    },
                ],
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT nextval('{S}.ids2')::text")).await?,
            "42"
        );
        assert_eq!(one(s, &owner).await?, None);
        run(
            s,
            PgChange::Sequence(SequenceChange::Drop {
                name: q(S, "ids2"),
                if_exists: false,
                behavior: DropBehavior::Restrict,
            }),
        )
        .await?;
        assert_eq!(
            text(
                s,
                &format!("SELECT count(*)::text FROM pg_sequences WHERE schemaname = '{S}'")
            )
            .await?,
            "0"
        );
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}

fn routine(
    kind: RoutineKind,
    name: QualifiedName,
    language: &str,
    body: RoutineBody,
) -> CreateRoutine {
    CreateRoutine {
        routine: kind,
        name,
        or_replace: false,
        arguments: vec![],
        returns: None,
        language: language.into(),
        body,
        volatility: None,
        strict: None,
        security_definer: None,
        leakproof: None,
        parallel: None,
        cost: None,
        rows: None,
        window: false,
        settings: vec![],
        comment: None,
    }
}

fn source(text: &str) -> RoutineBody {
    RoutineBody::Definition {
        source: text.into(),
    }
}

fn returns(t: &str) -> Option<Returns> {
    Some(Returns::Type {
        data_type: t.into(),
        setof: false,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn functions_and_procedures() -> R {
    const S: &str = "m2_pgedit_fn";
    for (major, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        run(
            s,
            PgChange::Routine(RoutineChange::Create(CreateRoutine {
                arguments: vec![
                    RoutineArg::new("a", "integer"),
                    RoutineArg {
                        default: Some("1".into()),
                        ..RoutineArg::new("b", "integer")
                    },
                ],
                returns: returns("integer"),
                volatility: Some(Volatility::Immutable),
                strict: Some(true),
                parallel: Some(Parallel::Safe),
                cost: Some(5.0),
                comment: Some("adds".into()),
                ..routine(
                    RoutineKind::Function,
                    q(S, "add_one"),
                    "sql",
                    source("SELECT a + b"),
                )
            })),
        )
        .await?;
        assert_eq!(text(s, &format!("SELECT {S}.add_one(1)::text")).await?, "2");
        let proc_row = |name: &str| {
            format!(
                "SELECT provolatile::text, proisstrict::text, proparallel::text, procost::text, prosecdef::text, proleakproof::text, array_to_string(proconfig, ','), obj_description(oid, 'pg_proc') FROM pg_proc WHERE proname = '{name}' AND pronamespace = '{S}'::regnamespace"
            )
        };
        assert_eq!(
            rows(s, &proc_row("add_one")).await?[0],
            vec![
                Some("i".into()),
                Some("true".into()),
                Some("s".into()),
                Some("5".into()),
                Some("false".into()),
                Some("false".into()),
                None,
                Some("adds".into())
            ]
        );
        run(
            s,
            PgChange::Routine(RoutineChange::Create(CreateRoutine {
                arguments: vec![
                    RoutineArg::new("n", "integer"),
                    RoutineArg {
                        mode: ArgMode::Out,
                        ..RoutineArg::new("x", "integer")
                    },
                ],
                returns: Some(Returns::Type {
                    data_type: "integer".into(),
                    setof: true,
                }),
                rows: Some(10.0),
                security_definer: Some(true),
                settings: vec![ConfigSetting {
                    name: "search_path".into(),
                    values: vec!["pg_catalog".into(), S.into()],
                }],
                ..routine(
                    RoutineKind::Function,
                    q(S, "series"),
                    "plpgsql",
                    source("BEGIN FOR i IN 1..n LOOP x := i; RETURN NEXT; END LOOP; END"),
                )
            })),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT count(*)::text FROM {S}.series(3)")).await?,
            "3"
        );
        let series = rows(s, &proc_row("series")).await?;
        assert_eq!(series[0][4], Some("true".into()));
        assert_eq!(series[0][6], Some(format!("search_path=pg_catalog, {S}")));
        run(
            s,
            PgChange::Routine(RoutineChange::Create(CreateRoutine {
                returns: Some(Returns::Table {
                    columns: vec![
                        ReturnColumn {
                            name: "k".into(),
                            data_type: "integer".into(),
                        },
                        ReturnColumn {
                            name: "v".into(),
                            data_type: "text".into(),
                        },
                    ],
                }),
                ..routine(
                    RoutineKind::Function,
                    q(S, "pairs"),
                    "sql",
                    source("SELECT 1, 'it''s'"),
                )
            })),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT v FROM {S}.pairs()")).await?,
            "it's"
        );
        run(
            s,
            PgChange::Routine(RoutineChange::Create(CreateRoutine {
                arguments: vec![RoutineArg {
                    mode: ArgMode::InOut,
                    ..RoutineArg::new("n", "integer")
                }],
                comment: Some("bumps".into()),
                ..routine(
                    RoutineKind::Procedure,
                    q(S, "bump"),
                    "plpgsql",
                    source("BEGIN n := n + 1; END"),
                )
            })),
        )
        .await?;
        assert_eq!(text(s, &format!("CALL {S}.bump(1)")).await?, "2");
        assert_eq!(
            rows(s, &proc_row("bump")).await?[0][7],
            Some("bumps".into())
        );
        if major >= 14 {
            exec(s, &format!("CREATE TABLE {S}.log (v int)")).await?;
            run(
                s,
                PgChange::Routine(RoutineChange::Create(CreateRoutine {
                    arguments: vec![RoutineArg::new("a", "integer")],
                    returns: returns("integer"),
                    ..routine(
                        RoutineKind::Function,
                        q(S, "twice"),
                        "sql",
                        RoutineBody::Return {
                            expression: "a * 2".into(),
                        },
                    )
                })),
            )
            .await?;
            assert_eq!(text(s, &format!("SELECT {S}.twice(4)::text")).await?, "8");
        }
        run(
            s,
            PgChange::Routine(RoutineChange::Alter {
                routine: RoutineKind::Function,
                name: q(S, "add_one"),
                signature: "integer, integer".into(),
                operations: vec![
                    RoutineOperation::Volatility {
                        volatility: Volatility::Stable,
                    },
                    RoutineOperation::Strict { strict: false },
                    RoutineOperation::Leakproof { leakproof: true },
                    RoutineOperation::Parallel {
                        parallel: Parallel::Restricted,
                    },
                    RoutineOperation::Cost { cost: 7.0 },
                    RoutineOperation::Set {
                        setting: ConfigSetting {
                            name: "work_mem".into(),
                            values: vec!["1MB".into()],
                        },
                    },
                    RoutineOperation::Security { definer: true },
                    RoutineOperation::Owner {
                        owner: RoleSpec::CurrentUser,
                    },
                    RoutineOperation::Rename {
                        new_name: "add_two".into(),
                    },
                ],
            }),
        )
        .await?;
        assert_eq!(
            rows(s, &proc_row("add_two")).await?[0][..7],
            [
                Some("s".into()),
                Some("false".into()),
                Some("r".into()),
                Some("7".into()),
                Some("true".into()),
                Some("true".into()),
                Some("work_mem=1MB".into())
            ]
        );
        run(
            s,
            PgChange::Routine(RoutineChange::Alter {
                routine: RoutineKind::Procedure,
                name: q(S, "bump"),
                signature: "integer".into(),
                operations: vec![
                    RoutineOperation::Security { definer: true },
                    RoutineOperation::Reset {
                        name: "work_mem".into(),
                    },
                ],
            }),
        )
        .await?;
        assert_eq!(rows(s, &proc_row("bump")).await?[0][4], Some("true".into()));
        for (kind, name, signature) in [
            (RoutineKind::Function, "add_two", "integer, integer"),
            (RoutineKind::Function, "series", "integer"),
            (RoutineKind::Function, "pairs", ""),
            (RoutineKind::Procedure, "bump", "integer"),
        ] {
            let p = run(
                s,
                PgChange::Routine(RoutineChange::Drop {
                    routine: kind,
                    name: q(S, name),
                    signature: signature.into(),
                    if_exists: false,
                    behavior: DropBehavior::Restrict,
                }),
            )
            .await?;
            assert!(p.destructive);
        }
        let left = if major >= 14 { "1" } else { "0" };
        assert_eq!(
            text(
                s,
                &format!(
                    "SELECT count(*)::text FROM pg_proc WHERE pronamespace = '{S}'::regnamespace"
                )
            )
            .await?,
            left
        );
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn triggers() -> R {
    const S: &str = "m2_pgedit_trg";
    for (major, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        exec(
            s,
            &format!("CREATE TABLE {S}.t (a int, b text); CREATE TABLE {S}.log (n bigint)"),
        )
        .await?;
        let trigger_fn = |name: &str, body: &str| {
            PgChange::Routine(RoutineChange::Create(CreateRoutine {
                returns: returns("trigger"),
                ..routine(RoutineKind::Function, q(S, name), "plpgsql", source(body))
            }))
        };
        run(
            s,
            trigger_fn(
                "fill_b",
                "BEGIN NEW.b := coalesce(NEW.b, TG_ARGV[0]); RETURN NEW; END",
            ),
        )
        .await?;
        run(
            s,
            trigger_fn(
                "count_added",
                &format!("BEGIN INSERT INTO {S}.log SELECT count(*) FROM added; RETURN NULL; END"),
            ),
        )
        .await?;
        run(s, trigger_fn("noop", "BEGIN RETURN NULL; END")).await?;
        let base = CreateTrigger {
            name: "fill".into(),
            table: q(S, "t"),
            or_replace: false,
            constraint: false,
            timing: TriggerTiming::Before,
            events: vec![
                TriggerEvent::Insert,
                TriggerEvent::Update {
                    columns: vec!["a".into(), "b".into()],
                },
            ],
            level: TriggerLevel::Row,
            old_table: None,
            new_table: None,
            condition: Some("NEW.a IS NOT NULL".into()),
            function: q(S, "fill_b"),
            arguments: vec!["it's default".into()],
            deferrable: false,
            initially_deferred: false,
            comment: Some("fills b".into()),
        };
        run(s, PgChange::Trigger(TriggerChange::Create(base.clone()))).await?;
        run(
            s,
            PgChange::Trigger(TriggerChange::Create(CreateTrigger {
                name: "count".into(),
                timing: TriggerTiming::After,
                events: vec![TriggerEvent::Insert],
                level: TriggerLevel::Statement,
                new_table: Some("added".into()),
                condition: None,
                function: q(S, "count_added"),
                arguments: vec![],
                comment: None,
                ..base.clone()
            })),
        )
        .await?;
        run(
            s,
            PgChange::Trigger(TriggerChange::Create(CreateTrigger {
                name: "deferred".into(),
                constraint: true,
                timing: TriggerTiming::After,
                events: vec![TriggerEvent::Delete],
                deferrable: true,
                initially_deferred: true,
                condition: None,
                function: q(S, "noop"),
                arguments: vec![],
                comment: None,
                ..base.clone()
            })),
        )
        .await?;
        exec(s, &format!("INSERT INTO {S}.t (a) VALUES (1), (2)")).await?;
        assert_eq!(
            text(s, &format!("SELECT b FROM {S}.t WHERE a = 1")).await?,
            "it's default"
        );
        assert_eq!(text(s, &format!("SELECT n::text FROM {S}.log")).await?, "2");
        let triggers = format!(
            "SELECT string_agg(tgname || ':' || tgenabled::text || ':' || tgdeferrable::text, ',' ORDER BY tgname) FROM pg_trigger WHERE tgrelid = '{S}.t'::regclass"
        );
        assert_eq!(
            text(s, &triggers).await?,
            "count:O:false,deferred:O:true,fill:O:false"
        );
        assert_eq!(
            one(s, &format!("SELECT obj_description(oid, 'pg_trigger') FROM pg_trigger WHERE tgname = 'fill' AND tgrelid = '{S}.t'::regclass")).await?,
            Some("fills b".into())
        );
        if major >= 14 {
            run(
                s,
                PgChange::Trigger(TriggerChange::Create(CreateTrigger {
                    or_replace: true,
                    events: vec![TriggerEvent::Insert],
                    comment: None,
                    ..base
                })),
            )
            .await?;
            assert!(text(s, &format!("SELECT pg_get_triggerdef(oid) FROM pg_trigger WHERE tgname = 'fill' AND tgrelid = '{S}.t'::regclass")).await?.contains("BEFORE INSERT ON"));
        }
        for (state, letter) in [
            (FiringState::Disabled, "D"),
            (FiringState::Replica, "R"),
            (FiringState::Always, "A"),
            (FiringState::Enabled, "O"),
        ] {
            run(
                s,
                PgChange::Trigger(TriggerChange::SetState {
                    name: "count".into(),
                    table: q(S, "t"),
                    state,
                }),
            )
            .await?;
            assert_eq!(
                text(s, &format!("SELECT tgenabled::text FROM pg_trigger WHERE tgname = 'count' AND tgrelid = '{S}.t'::regclass")).await?,
                letter
            );
        }
        run(
            s,
            PgChange::Trigger(TriggerChange::Rename {
                name: "count".into(),
                table: q(S, "t"),
                new_name: "counter".into(),
            }),
        )
        .await?;
        for name in ["counter", "deferred", "fill"] {
            run(
                s,
                PgChange::Trigger(TriggerChange::Drop {
                    name: name.into(),
                    table: q(S, "t"),
                    if_exists: false,
                    behavior: DropBehavior::Restrict,
                }),
            )
            .await?;
        }
        assert_eq!(
            text(
                s,
                &format!("SELECT count(*)::text FROM pg_trigger WHERE tgrelid = '{S}.t'::regclass")
            )
            .await?,
            "0"
        );
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}

/// Event triggers are database-wide, so they live in a database of their own.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn event_triggers() -> R {
    const DB: &str = "m2_pgedit_evt";
    for (major, port) in servers() {
        let admin = connect(port, "varve").await?;
        let admin = admin.as_ref();
        exec(admin, &format!("DROP DATABASE IF EXISTS {DB} WITH (FORCE)")).await?;
        run(
            admin,
            PgChange::Database(DatabaseChange::Create(Box::new(CreateDatabase {
                name: DB.into(),
                template: Some("template0".into()),
                ..CreateDatabase::default()
            }))),
        )
        .await?;
        {
            let s = connect(port, DB).await?;
            let s = s.as_ref();
            exec(s, "CREATE SCHEMA ev; CREATE TABLE ev.log (tag text)").await?;
            run(
                s,
                PgChange::Routine(RoutineChange::Create(CreateRoutine {
                    returns: returns("event_trigger"),
                    ..routine(
                        RoutineKind::Function,
                        q("ev", "on_ddl"),
                        "plpgsql",
                        source("BEGIN INSERT INTO ev.log VALUES (TG_TAG); END"),
                    )
                })),
            )
            .await?;
            run(
                s,
                PgChange::EventTrigger(EventTriggerChange::Create(CreateEventTrigger {
                    name: "track".into(),
                    event: EventTriggerEvent::DdlCommandEnd,
                    tags: vec!["CREATE TABLE".into()],
                    function: q("ev", "on_ddl"),
                    comment: Some("tracks".into()),
                })),
            )
            .await?;
            exec(
                s,
                "CREATE TABLE ev.x (a int); CREATE VIEW ev.y AS SELECT 1 AS a",
            )
            .await?;
            assert_eq!(
                text(s, "SELECT string_agg(tag, ',') FROM ev.log").await?,
                "CREATE TABLE"
            );
            assert_eq!(
                one(s, "SELECT obj_description(oid, 'pg_event_trigger') FROM pg_event_trigger WHERE evtname = 'track'").await?,
                Some("tracks".into())
            );
            for change in [
                EventTriggerChange::SetState {
                    name: "track".into(),
                    state: FiringState::Disabled,
                },
                EventTriggerChange::Rename {
                    name: "track".into(),
                    new_name: "tracker".into(),
                },
                EventTriggerChange::Owner {
                    name: "tracker".into(),
                    owner: RoleSpec::CurrentUser,
                },
            ] {
                run(s, PgChange::EventTrigger(change)).await?;
            }
            assert_eq!(
                text(
                    s,
                    "SELECT evtname::text || ':' || evtenabled::text FROM pg_event_trigger"
                )
                .await?,
                "tracker:D"
            );
            if major >= 17 {
                run(
                    s,
                    PgChange::Routine(RoutineChange::Create(CreateRoutine {
                        returns: returns("event_trigger"),
                        ..routine(
                            RoutineKind::Function,
                            q("ev", "on_login"),
                            "plpgsql",
                            source("BEGIN NULL; END"),
                        )
                    })),
                )
                .await?;
                run(
                    s,
                    PgChange::EventTrigger(EventTriggerChange::Create(CreateEventTrigger {
                        name: "login".into(),
                        event: EventTriggerEvent::Login,
                        tags: vec![],
                        function: q("ev", "on_login"),
                        comment: None,
                    })),
                )
                .await?;
                // New sessions still get in.
                let other = connect(port, DB).await?;
                assert_eq!(text(other.as_ref(), "SELECT 1::text").await?, "1");
                other.close().await?;
                run(
                    s,
                    PgChange::EventTrigger(EventTriggerChange::Drop {
                        name: "login".into(),
                        if_exists: false,
                        behavior: DropBehavior::Restrict,
                    }),
                )
                .await?;
            }
            run(
                s,
                PgChange::EventTrigger(EventTriggerChange::Drop {
                    name: "tracker".into(),
                    if_exists: true,
                    behavior: DropBehavior::Restrict,
                }),
            )
            .await?;
            assert_eq!(
                text(s, "SELECT count(*)::text FROM pg_event_trigger").await?,
                "0"
            );
        }
        exec(admin, &format!("DROP DATABASE IF EXISTS {DB} WITH (FORCE)")).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn types_and_domains() -> R {
    const S: &str = "m2_pgedit_typ";
    const S2: &str = "m2_pgedit_typ2";
    for (major, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S, S2]).await?;
        create_schema(s, S).await?;
        create_schema(s, S2).await?;
        let ty = |change| PgChange::Type(change);
        run(
            s,
            ty(TypeChange::CreateEnum {
                name: q(S, "mood"),
                labels: vec!["sad".into(), "ok".into()],
                comment: Some("moods".into()),
            }),
        )
        .await?;
        let add = run(
            s,
            ty(TypeChange::AddEnumValue {
                name: q(S, "mood"),
                value: "happy".into(),
                if_not_exists: false,
                position: Some(EnumPosition::After { label: "ok".into() }),
            }),
        )
        .await?;
        assert!(add.transactional);
        run(
            s,
            ty(TypeChange::AddEnumValue {
                name: q(S, "mood"),
                value: "meh".into(),
                if_not_exists: true,
                position: Some(EnumPosition::Before { label: "ok".into() }),
            }),
        )
        .await?;
        run(
            s,
            ty(TypeChange::RenameEnumValue {
                name: q(S, "mood"),
                value: "sad".into(),
                new_value: "blue".into(),
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT string_agg(enumlabel, ',' ORDER BY enumsortorder) FROM pg_enum WHERE enumtypid = '{S}.mood'::regtype")).await?,
            "blue,meh,ok,happy"
        );
        assert_eq!(
            text(
                s,
                &format!("SELECT obj_description('{S}.mood'::regtype, 'pg_type')")
            )
            .await?,
            "moods"
        );

        run(
            s,
            ty(TypeChange::CreateComposite {
                name: q(S, "pair"),
                attributes: vec![
                    CompositeAttribute {
                        name: "x".into(),
                        data_type: "integer".into(),
                        collation: None,
                    },
                    CompositeAttribute {
                        name: "y".into(),
                        data_type: "text".into(),
                        collation: Some("C".into()),
                    },
                ],
                comment: None,
            }),
        )
        .await?;
        run(
            s,
            ty(TypeChange::AlterComposite {
                name: q(S, "pair"),
                operations: vec![
                    CompositeOperation::AddAttribute {
                        attribute: CompositeAttribute {
                            name: "z".into(),
                            data_type: "date".into(),
                            collation: None,
                        },
                        cascade: false,
                    },
                    CompositeOperation::RenameAttribute {
                        name: "z".into(),
                        new_name: "w".into(),
                        cascade: false,
                    },
                    CompositeOperation::AlterAttributeType {
                        name: "w".into(),
                        data_type: "timestamptz".into(),
                        collation: None,
                        cascade: false,
                    },
                    CompositeOperation::DropAttribute {
                        name: "x".into(),
                        if_exists: true,
                        cascade: false,
                    },
                ],
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT string_agg(a.attname || ' ' || format_type(a.atttypid, a.atttypmod), ', ' ORDER BY a.attnum) FROM pg_type t JOIN pg_attribute a ON a.attrelid = t.typrelid WHERE t.oid = '{S}.pair'::regtype AND a.attnum > 0 AND NOT a.attisdropped")).await?,
            "y text, w timestamp with time zone"
        );

        run(
            s,
            ty(TypeChange::CreateRange(Box::new(CreateRangeType {
                name: q(S, "floatrange"),
                subtype: "float8".into(),
                subtype_opclass: None,
                collation: None,
                canonical: None,
                subtype_diff: Some(q("pg_catalog", "float8mi")),
                multirange_type_name: (major >= 14).then(|| q(S, "floatmulti")),
                comment: None,
            }))),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT rngsubtype::regtype::text || '/' || rngsubdiff::regproc::text FROM pg_range WHERE rngtypid = '{S}.floatrange'::regtype")).await?,
            "double precision/float8mi"
        );
        if major >= 14 {
            assert_eq!(
                text(s, &format!("SELECT rngmultitypid::regtype::text FROM pg_range WHERE rngtypid = '{S}.floatrange'::regtype")).await?,
                format!("{S}.floatmulti")
            );
        }
        assert_eq!(
            text(
                s,
                &format!("SELECT '[1.5,2.5)'::{S}.floatrange @> 2.0::float8")
            )
            .await?,
            "true"
        );

        // Domains.
        run(
            s,
            PgChange::Domain(DomainChange::Create(CreateDomain {
                name: q(S, "posint"),
                data_type: "integer".into(),
                collation: None,
                default: Some("1".into()),
                not_null: true,
                checks: vec![DomainCheck {
                    name: Some("positive".into()),
                    expression: "VALUE > 0".into(),
                }],
                comment: Some("positive ints".into()),
            })),
        )
        .await?;
        assert!(exec(s, &format!("SELECT (-1)::{S}.posint")).await.is_err());
        run(
            s,
            PgChange::Domain(DomainChange::Alter {
                name: q(S, "posint"),
                operations: vec![
                    DomainOperation::SetDefault {
                        expression: "2".into(),
                    },
                    DomainOperation::DropNotNull,
                    DomainOperation::AddCheck {
                        check: DomainCheck {
                            name: Some("small".into()),
                            expression: "VALUE < 1000".into(),
                        },
                        not_valid: true,
                    },
                    DomainOperation::ValidateConstraint {
                        name: "small".into(),
                    },
                    DomainOperation::RenameConstraint {
                        name: "small".into(),
                        new_name: "tiny".into(),
                    },
                    DomainOperation::CommentConstraint {
                        name: "tiny".into(),
                        comment: Some("under a thousand, it's small".into()),
                    },
                    DomainOperation::DropConstraint {
                        name: "positive".into(),
                        if_exists: false,
                        behavior: DropBehavior::Restrict,
                    },
                    DomainOperation::Owner {
                        owner: RoleSpec::CurrentUser,
                    },
                    DomainOperation::Rename {
                        new_name: "smallint2".into(),
                    },
                ],
            }),
        )
        .await?;
        assert_eq!(
            rows(s, &format!("SELECT t.typnotnull::text, t.typdefault, (SELECT string_agg(conname, ',') FROM pg_constraint WHERE contypid = t.oid), obj_description(t.oid, 'pg_type') FROM pg_type t WHERE t.oid = '{S}.smallint2'::regtype")).await?,
            vec![vec![Some("false".into()), Some("2".into()), Some("tiny".into()), Some("positive ints".into())]]
        );
        let constraint_comment = format!(
            "SELECT obj_description(c.oid, 'pg_constraint') FROM pg_constraint c WHERE c.contypid = '{S}.smallint2'::regtype AND c.conname = 'tiny'"
        );
        assert_eq!(
            rows(s, &constraint_comment).await?,
            vec![vec![Some("under a thousand, it's small".into())]],
            "PostgreSQL {major}"
        );
        // The comment is removed again (the domain is renamed: ON DOMAIN names it as it is now).
        run(
            s,
            PgChange::Domain(DomainChange::Alter {
                name: q(S, "smallint2"),
                operations: vec![DomainOperation::CommentConstraint {
                    name: "tiny".into(),
                    comment: None,
                }],
            }),
        )
        .await?;
        assert_eq!(
            rows(s, &constraint_comment).await?,
            vec![vec![None]],
            "PostgreSQL {major}"
        );

        // Common type changes.
        run(
            s,
            ty(TypeChange::Rename {
                name: q(S, "mood"),
                new_name: "feeling".into(),
            }),
        )
        .await?;
        run(
            s,
            ty(TypeChange::SetSchema {
                name: q(S, "feeling"),
                schema: S2.into(),
            }),
        )
        .await?;
        run(
            s,
            ty(TypeChange::Owner {
                name: q(S2, "feeling"),
                owner: RoleSpec::CurrentUser,
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT '{S2}.feeling'::regtype::text")).await?,
            format!("{S2}.feeling")
        );
        exec(s, &format!("CREATE TABLE {S2}.uses (f {S2}.feeling)")).await?;
        assert!(
            run(
                s,
                ty(TypeChange::Drop {
                    name: q(S2, "feeling"),
                    if_exists: false,
                    behavior: DropBehavior::Restrict
                })
            )
            .await
            .is_err()
        );
        run(
            s,
            ty(TypeChange::Drop {
                name: q(S2, "feeling"),
                if_exists: false,
                behavior: DropBehavior::Cascade,
            }),
        )
        .await?;
        for name in ["pair", "floatrange"] {
            run(
                s,
                ty(TypeChange::Drop {
                    name: q(S, name),
                    if_exists: true,
                    behavior: DropBehavior::Restrict,
                }),
            )
            .await?;
        }
        run(
            s,
            PgChange::Domain(DomainChange::Drop {
                name: q(S, "smallint2"),
                if_exists: false,
                behavior: DropBehavior::Restrict,
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT count(*)::text FROM pg_type WHERE typnamespace IN ('{S}'::regnamespace, '{S2}'::regnamespace) AND typtype IN ('e', 'c', 'r', 'm', 'd')")).await?,
            "1",
            "only the row type of the table uses remains"
        );
        reset_schema(s, &[S, S2]).await?;
    }
    Ok(())
}

/// Extensions are per database; a database of their own keeps them away
/// from other workers that install the same ones.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn extensions() -> R {
    const DB: &str = "m2_pgedit_ext";
    for (_, port) in servers() {
        let admin = connect(port, "varve").await?;
        let admin = admin.as_ref();
        exec(admin, &format!("DROP DATABASE IF EXISTS {DB} WITH (FORCE)")).await?;
        exec(admin, &format!("CREATE DATABASE {DB} TEMPLATE template0")).await?;
        {
            let s = connect(port, DB).await?;
            let s = s.as_ref();
            create_schema(s, "e1").await?;
            create_schema(s, "e2").await?;
            run(
                s,
                PgChange::Extension(ExtensionChange::Create(CreateExtension {
                    name: "hstore".into(),
                    if_not_exists: true,
                    schema: Some("e1".into()),
                    version: Some("1.4".into()),
                    cascade: false,
                    comment: Some("key value".into()),
                })),
            )
            .await?;
            let ext = "SELECT extversion || '@' || extnamespace::regnamespace::text || '/' || coalesce(obj_description(oid, 'pg_extension'), '') FROM pg_extension WHERE extname = 'hstore'";
            assert_eq!(text(s, ext).await?, "1.4@e1/key value");
            run(
                s,
                PgChange::Extension(ExtensionChange::Update {
                    name: "hstore".into(),
                    version: Some("1.5".into()),
                }),
            )
            .await?;
            assert!(text(s, ext).await?.starts_with("1.5@"));
            run(
                s,
                PgChange::Extension(ExtensionChange::Update {
                    name: "hstore".into(),
                    version: None,
                }),
            )
            .await?;
            let default = text(
                s,
                "SELECT default_version FROM pg_available_extensions WHERE name = 'hstore'",
            )
            .await?;
            assert!(text(s, ext).await?.starts_with(&format!("{default}@")));
            run(
                s,
                PgChange::Extension(ExtensionChange::SetSchema {
                    name: "hstore".into(),
                    schema: "e2".into(),
                }),
            )
            .await?;
            assert!(text(s, ext).await?.contains("@e2/"));
            run(
                s,
                PgChange::Extension(ExtensionChange::Create(CreateExtension {
                    name: "earthdistance".into(),
                    if_not_exists: false,
                    schema: None,
                    version: None,
                    cascade: true,
                    comment: None,
                })),
            )
            .await?;
            assert_eq!(
                text(s, "SELECT string_agg(extname::text, ',' ORDER BY extname) FROM pg_extension WHERE extname <> 'plpgsql'").await?,
                "cube,earthdistance,hstore"
            );
            for (name, behavior) in [
                ("hstore", DropBehavior::Restrict),
                ("cube", DropBehavior::Cascade),
            ] {
                let p = run(
                    s,
                    PgChange::Extension(ExtensionChange::Drop {
                        name: name.into(),
                        if_exists: false,
                        behavior,
                    }),
                )
                .await?;
                assert!(p.destructive);
            }
            assert_eq!(
                text(
                    s,
                    "SELECT count(*)::text FROM pg_extension WHERE extname <> 'plpgsql'"
                )
                .await?,
                "0"
            );
        }
        exec(admin, &format!("DROP DATABASE IF EXISTS {DB} WITH (FORCE)")).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn rules_and_policies() -> R {
    const S: &str = "m2_pgedit_rls";
    for (_, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        exec(s, &format!("CREATE TABLE {S}.t (a int, owner text DEFAULT current_user); CREATE TABLE {S}.log (a int)")).await?;
        run(
            s,
            PgChange::Rule(RuleChange::Create(CreateRule {
                name: "skip_negative".into(),
                table: q(S, "t"),
                or_replace: false,
                event: RuleEvent::Insert,
                condition: Some("NEW.a < 0".into()),
                instead: true,
                actions: vec![],
                comment: Some("drops negatives".into()),
            })),
        )
        .await?;
        run(
            s,
            PgChange::Rule(RuleChange::Create(CreateRule {
                name: "log_delete".into(),
                table: q(S, "t"),
                or_replace: true,
                event: RuleEvent::Delete,
                condition: None,
                instead: false,
                actions: vec![
                    format!("INSERT INTO {S}.log VALUES (OLD.a)"),
                    format!("INSERT INTO {S}.log VALUES (OLD.a * 10)"),
                ],
                comment: None,
            })),
        )
        .await?;
        exec(s, &format!("INSERT INTO {S}.t (a) VALUES (-1), (2)")).await?;
        exec(s, &format!("DELETE FROM {S}.t WHERE a = 2")).await?;
        assert_eq!(
            text(s, &format!("SELECT count(*)::text FROM {S}.t")).await?,
            "0"
        );
        assert_eq!(
            text(
                s,
                &format!("SELECT string_agg(a::text, ',' ORDER BY a) FROM {S}.log")
            )
            .await?,
            "2,20"
        );
        assert_eq!(
            one(s, &format!("SELECT obj_description(oid, 'pg_rewrite') FROM pg_rewrite WHERE rulename = 'skip_negative' AND ev_class = '{S}.t'::regclass")).await?,
            Some("drops negatives".into())
        );
        run(
            s,
            PgChange::Rule(RuleChange::Rename {
                name: "skip_negative".into(),
                table: q(S, "t"),
                new_name: "no_negative".into(),
            }),
        )
        .await?;
        run(
            s,
            PgChange::Rule(RuleChange::SetState {
                name: "no_negative".into(),
                table: q(S, "t"),
                state: FiringState::Disabled,
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT ev_enabled::text FROM pg_rewrite WHERE rulename = 'no_negative' AND ev_class = '{S}.t'::regclass")).await?,
            "D"
        );
        for name in ["no_negative", "log_delete"] {
            run(
                s,
                PgChange::Rule(RuleChange::Drop {
                    name: name.into(),
                    table: q(S, "t"),
                    if_exists: false,
                    behavior: DropBehavior::Restrict,
                }),
            )
            .await?;
        }
        assert_eq!(
            text(
                s,
                &format!("SELECT count(*)::text FROM pg_rules WHERE schemaname = '{S}'")
            )
            .await?,
            "0"
        );

        // Policies.
        run(
            s,
            PgChange::Table(TableChange::Alter {
                name: q(S, "t"),
                operations: vec![TableOperation::RowLevelSecurity { enabled: true }],
            }),
        )
        .await?;
        run(
            s,
            PgChange::Policy(PolicyChange::Create(CreatePolicy {
                name: "own_rows".into(),
                table: q(S, "t"),
                restrictive: false,
                command: PolicyCommand::All,
                roles: vec![RoleSpec::Public],
                using: Some("owner = current_user".into()),
                with_check: Some("owner = current_user".into()),
                comment: Some("only mine".into()),
            })),
        )
        .await?;
        run(
            s,
            PgChange::Policy(PolicyChange::Create(CreatePolicy {
                name: "non_negative".into(),
                table: q(S, "t"),
                restrictive: true,
                command: PolicyCommand::Select,
                roles: vec![RoleSpec::CurrentUser],
                using: Some("a >= 0".into()),
                with_check: None,
                comment: None,
            })),
        )
        .await?;
        let policies = format!(
            "SELECT string_agg(policyname || ':' || permissive || ':' || cmd || ':' || array_to_string(roles, '+') || ':' || coalesce(qual, '-') || ':' || coalesce(with_check, '-'), ' | ' ORDER BY policyname) FROM pg_policies WHERE schemaname = '{S}'"
        );
        assert_eq!(
            text(s, &policies).await?,
            "non_negative:RESTRICTIVE:SELECT:varve:(a >= 0):- | own_rows:PERMISSIVE:ALL:public:(owner = CURRENT_USER):(owner = CURRENT_USER)"
        );
        run(
            s,
            PgChange::Policy(PolicyChange::Alter {
                name: "own_rows".into(),
                table: q(S, "t"),
                roles: Some(vec![RoleSpec::name("varve")]),
                using: Some("true".into()),
                with_check: None,
                new_name: Some("all_rows".into()),
            }),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT policyname || ':' || array_to_string(roles, '+') || ':' || qual FROM pg_policies WHERE schemaname = '{S}' AND cmd = 'ALL'")).await?,
            "all_rows:varve:true"
        );
        assert_eq!(
            one(s, &format!("SELECT obj_description(oid, 'pg_policy') FROM pg_policy WHERE polname = 'all_rows'  AND polrelid = '{S}.t'::regclass")).await?,
            Some("only mine".into())
        );
        for name in ["all_rows", "non_negative"] {
            run(
                s,
                PgChange::Policy(PolicyChange::Drop {
                    name: name.into(),
                    table: q(S, "t"),
                    if_exists: true,
                }),
            )
            .await?;
        }
        assert_eq!(
            text(
                s,
                &format!("SELECT count(*)::text FROM pg_policies WHERE schemaname = '{S}'")
            )
            .await?,
            "0"
        );
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}

fn object(
    kind: ObjectKind,
    schema: &str,
    parent: Option<&str>,
    name: &str,
    signature: Option<&str>,
) -> ObjectRef {
    ObjectRef {
        kind,
        database: Some("varve".into()),
        schema: Some(schema.into()),
        parent: parent.map(str::to_owned),
        name: name.into(),
        signature: signature.map(str::to_owned),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn comments_and_owners() -> R {
    const S: &str = "m2_pgedit_obj";
    const ROLE: &str = "m2_pgedit_role";
    for (_, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        exec(s, &format!("DROP ROLE IF EXISTS {ROLE}")).await?;
        exec(s, &format!("CREATE ROLE {ROLE} NOLOGIN")).await?;
        create_schema(s, S).await?;
        exec(
            s,
            &format!(
                "CREATE TABLE {S}.t (a int CONSTRAINT a_pos CHECK (a > 0));
                 CREATE INDEX t_a ON {S}.t (a);
                 CREATE VIEW {S}.v AS SELECT a FROM {S}.t;
                 CREATE MATERIALIZED VIEW {S}.mv AS SELECT a FROM {S}.t;
                 CREATE SEQUENCE {S}.seq;
                 CREATE FUNCTION {S}.f(int) RETURNS int LANGUAGE sql AS 'SELECT $1';
                 CREATE PROCEDURE {S}.p() LANGUAGE sql AS 'SELECT 1';
                 CREATE AGGREGATE {S}.total(int) (SFUNC = int4pl, STYPE = int);
                 CREATE TYPE {S}.e AS ENUM ('x');
                 CREATE DOMAIN {S}.d AS int;
                 CREATE FUNCTION {S}.tf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
                 CREATE TRIGGER trg BEFORE INSERT ON {S}.t FOR EACH ROW EXECUTE FUNCTION {S}.tf();
                 CREATE COLLATION {S}.coll (provider = icu, locale = 'en-US')"
            ),
        )
        .await?;
        use ObjectKind as K;
        let cases = [
            (
                object(K::Schema, S, None, S, None),
                format!("SELECT obj_description('{S}'::regnamespace, 'pg_namespace')"),
            ),
            (
                object(K::Table, S, None, "t", None),
                format!("SELECT obj_description('{S}.t'::regclass, 'pg_class')"),
            ),
            (
                object(K::Column, S, Some("t"), "a", None),
                format!("SELECT col_description('{S}.t'::regclass, 1)"),
            ),
            (
                object(K::Constraint, S, Some("t"), "a_pos", None),
                format!(
                    "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint WHERE conname = 'a_pos'  AND conrelid = '{S}.t'::regclass"
                ),
            ),
            (
                object(K::Index, S, Some("t"), "t_a", None),
                format!("SELECT obj_description('{S}.t_a'::regclass, 'pg_class')"),
            ),
            (
                object(K::View, S, None, "v", None),
                format!("SELECT obj_description('{S}.v'::regclass, 'pg_class')"),
            ),
            (
                object(K::MaterializedView, S, None, "mv", None),
                format!("SELECT obj_description('{S}.mv'::regclass, 'pg_class')"),
            ),
            (
                object(K::Sequence, S, None, "seq", None),
                format!("SELECT obj_description('{S}.seq'::regclass, 'pg_class')"),
            ),
            (
                object(K::Function, S, None, "f", Some("integer")),
                format!("SELECT obj_description('{S}.f(int)'::regprocedure, 'pg_proc')"),
            ),
            (
                object(K::Procedure, S, None, "p", None),
                format!("SELECT obj_description('{S}.p()'::regprocedure, 'pg_proc')"),
            ),
            (
                object(K::Aggregate, S, None, "total", Some("integer")),
                format!("SELECT obj_description('{S}.total(int)'::regprocedure, 'pg_proc')"),
            ),
            (
                object(K::EnumType, S, None, "e", None),
                format!("SELECT obj_description('{S}.e'::regtype, 'pg_type')"),
            ),
            (
                object(K::Domain, S, None, "d", None),
                format!("SELECT obj_description('{S}.d'::regtype, 'pg_type')"),
            ),
            (
                object(K::Trigger, S, Some("t"), "trg", None),
                format!(
                    "SELECT obj_description(oid, 'pg_trigger') FROM pg_trigger WHERE tgname = 'trg' AND tgrelid = '{S}.t'::regclass"
                ),
            ),
            (
                object(K::Collation, S, None, "coll", None),
                format!(
                    "SELECT obj_description(oid, 'pg_collation') FROM pg_collation WHERE collname = 'coll' AND collnamespace = '{S}'::regnamespace"
                ),
            ),
        ];
        for (target, check) in &cases {
            run(
                s,
                PgChange::Comment(CommentChange {
                    target: target.clone(),
                    comment: Some(format!("about {}", target.name)),
                }),
            )
            .await?;
            assert_eq!(
                one(s, check).await?,
                Some(format!("about {}", target.name)),
                "{:?}",
                target.kind
            );
            run(
                s,
                PgChange::Comment(CommentChange {
                    target: target.clone(),
                    comment: None,
                }),
            )
            .await?;
            assert_eq!(one(s, check).await?, None, "{:?}", target.kind);
        }

        let owned = [
            (
                object(K::Schema, S, None, S, None),
                format!("SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname = '{S}'"),
            ),
            (
                object(K::Table, S, None, "t", None),
                format!(
                    "SELECT relowner::regrole::text FROM pg_class WHERE oid = '{S}.t'::regclass"
                ),
            ),
            (
                object(K::View, S, None, "v", None),
                format!(
                    "SELECT relowner::regrole::text FROM pg_class WHERE oid = '{S}.v'::regclass"
                ),
            ),
            (
                object(K::MaterializedView, S, None, "mv", None),
                format!(
                    "SELECT relowner::regrole::text FROM pg_class WHERE oid = '{S}.mv'::regclass"
                ),
            ),
            (
                object(K::Sequence, S, None, "seq", None),
                format!(
                    "SELECT relowner::regrole::text FROM pg_class WHERE oid = '{S}.seq'::regclass"
                ),
            ),
            (
                object(K::Function, S, None, "f", Some("integer")),
                format!(
                    "SELECT proowner::regrole::text FROM pg_proc WHERE oid = '{S}.f(int)'::regprocedure"
                ),
            ),
            (
                object(K::Procedure, S, None, "p", None),
                format!(
                    "SELECT proowner::regrole::text FROM pg_proc WHERE oid = '{S}.p()'::regprocedure"
                ),
            ),
            (
                object(K::Aggregate, S, None, "total", Some("integer")),
                format!(
                    "SELECT proowner::regrole::text FROM pg_proc WHERE oid = '{S}.total(int)'::regprocedure"
                ),
            ),
            (
                object(K::EnumType, S, None, "e", None),
                format!("SELECT typowner::regrole::text FROM pg_type WHERE oid = '{S}.e'::regtype"),
            ),
            (
                object(K::Domain, S, None, "d", None),
                format!("SELECT typowner::regrole::text FROM pg_type WHERE oid = '{S}.d'::regtype"),
            ),
            (
                object(K::Collation, S, None, "coll", None),
                format!(
                    "SELECT collowner::regrole::text FROM pg_collation WHERE collname = 'coll' AND collnamespace = '{S}'::regnamespace"
                ),
            ),
        ];
        for (target, check) in &owned {
            run(
                s,
                PgChange::Owner(OwnerChange {
                    target: target.clone(),
                    owner: RoleSpec::name(ROLE),
                }),
            )
            .await?;
            assert_eq!(text(s, check).await?, ROLE, "{:?}", target.kind);
        }
        assert!(
            varve_catalog::postgres::edit::plan(
                s.server_info(),
                &PgChange::Owner(OwnerChange {
                    target: object(K::Index, S, Some("t"), "t_a", None),
                    owner: RoleSpec::name(ROLE)
                })
            )
            .is_err()
        );
        reset_schema(s, &[S]).await?;
        exec(s, &format!("DROP ROLE IF EXISTS {ROLE}")).await?;
    }
    Ok(())
}

/// SQL-standard `BEGIN ATOMIC` bodies (14+), in a test of their own because
/// they need the driver to send the statement as one piece.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn sql_standard_atomic_body() -> R {
    const S: &str = "m2_pgedit_atomic";
    for (major, port) in servers() {
        if major < 14 {
            continue;
        }
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        exec(s, &format!("CREATE TABLE {S}.log (v int)")).await?;
        run(
            s,
            PgChange::Routine(RoutineChange::Create(CreateRoutine {
                arguments: vec![RoutineArg::new("a", "integer")],
                ..routine(
                    RoutineKind::Procedure,
                    q(S, "log_two"),
                    "sql",
                    RoutineBody::Atomic {
                        statements: vec![
                            format!("INSERT INTO {S}.log VALUES (a)"),
                            format!("INSERT INTO {S}.log VALUES (a + 1)"),
                        ],
                    },
                )
            })),
        )
        .await?;
        exec(s, &format!("CALL {S}.log_two(7)")).await?;
        // Words after `.` and `AS` are names, even `end` and `case`.
        exec(
            s,
            &format!("CREATE TABLE {S}.ends (\"end\" int); INSERT INTO {S}.ends VALUES (100)"),
        )
        .await?;
        run(
            s,
            PgChange::Routine(RoutineChange::Create(CreateRoutine {
                ..routine(
                    RoutineKind::Procedure,
                    q(S, "keywords"),
                    "sql",
                    RoutineBody::Atomic {
                        statements: vec![
                            format!("INSERT INTO {S}.log SELECT e.end FROM {S}.ends e"),
                            format!("INSERT INTO {S}.log SELECT 1 AS case"),
                        ],
                    },
                )
            })),
        )
        .await?;
        exec(s, &format!("CALL {S}.keywords()")).await?;
        assert_eq!(
            text(
                s,
                &format!("SELECT string_agg(v::text, ',' ORDER BY v) FROM {S}.log")
            )
            .await?,
            "1,7,8,100"
        );
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}

/// The injection payloads of the security audit, sent as the UI would
/// (JSON). Each must be refused, or run with no effect beyond its own
/// statement: the victim table and every column survive.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn injection_payloads_have_no_extra_effect() -> R {
    const S: &str = "m2_pgedit_inj";
    let drop_victim = format!("DROP TABLE {S}.victim");
    let alter = |op: String| {
        format!(
            r#"{{"object":"table","action":"alter","name":{{"schema":"{S}","name":"t"}},"operations":[{op}]}}"#
        )
    };
    let json_text = |s: &str| serde_json::Value::String(s.to_owned()).to_string();
    let set_default = |e: &str| {
        alter(format!(
            r#"{{"op":"setDefault","column":"a","expression":{}}}"#,
            json_text(e)
        ))
    };
    let refused: Vec<String> = vec![
        // `\r` ends a line comment.
        set_default(&format!("'x' --\r; {drop_victim}")),
        alter(format!(
            r#"{{"op":"addConstraint","constraint":{{"kind":{{"type":"check","expression":{}}}}}}}"#,
            json_text(&format!("b > 0) --\r; {drop_victim}; SELECT (1"))
        )),
        // Non-ASCII dollar tags and identifier characters.
        format!(
            r#"{{"object":"view","action":"create","name":{{"schema":"{S}","name":"v1"}},"query":{}}}"#,
            json_text(&format!(
                "SELECT $\u{20ac}$'$\u{20ac}$ AS a; {drop_victim}; --'"
            ))
        ),
        format!(
            r#"{{"object":"view","action":"create","name":{{"schema":"{S}","name":"v2"}},"query":{}}}"#,
            json_text(&format!("SELECT 1 AS \u{20ac}$x$; {drop_victim}; --$x$"))
        ),
        set_default(&format!("{S}.\u{20ac}e'\\'; {drop_victim}; --'")),
        format!(
            r#"{{"object":"index","action":"create","name":"inj_idx","table":{{"schema":"{S}","name":"t"}},"concurrently":true,"elements":[{{"key":{{"column":"b"}}}}],"predicate":{}}}"#,
            json_text(&format!("a <> $\u{20ac}$'$\u{20ac}$; {drop_victim}; --'"))
        ),
        // Unbalanced nested comment, `;` in a type.
        set_default("'x' /* /* */ ; drop table x"),
        alter(format!(
            r#"{{"op":"addColumn","column":{{"name":"z","dataType":{}}}}}"#,
            json_text(&format!("int; {drop_victim}"))
        )),
        // Destructive bypass through a top-level comma or closing parenthesis.
        set_default("0, DROP COLUMN b"),
        alter(format!(
            r#"{{"op":"addConstraint","constraint":{{"kind":{{"type":"check","expression":{}}}}}}}"#,
            json_text("b > 0), DROP COLUMN b, ADD CHECK (true")
        )),
        alter(format!(
            r#"{{"op":"alterColumnType","name":"b","dataType":"bigint","using":{}}}"#,
            json_text("b::bigint, DROP COLUMN a")
        )),
        // A second statement in a rule action, a signature and a policy.
        format!(
            r#"{{"object":"rule","action":"create","name":"r","table":{{"schema":"{S}","name":"t"}},"event":"insert","actions":[{}]}}"#,
            json_text(&format!("INSERT INTO {S}.t VALUES (NEW.a); {drop_victim}"))
        ),
        format!(
            r#"{{"object":"routine","action":"drop","routine":"function","name":{{"schema":"{S}","name":"f"}},"signature":{}}}"#,
            json_text(&format!("int); {drop_victim}; --"))
        ),
        format!(
            r#"{{"object":"policy","action":"create","name":"p","table":{{"schema":"{S}","name":"t"}},"using":{}}}"#,
            json_text(&format!("true)); {drop_victim}; --"))
        ),
    ];
    // Harmless lookalikes that must run as exactly one statement.
    let accepted: Vec<(String, &str)> = vec![
        (set_default("'x' /* a /* ; */ ; */"), "'x'::text"),
        (
            set_default(&format!("E'\\'; {drop_victim}; --'")),
            "'''; DROP TABLE m2_pgedit_inj.victim; --'::text",
        ),
        (
            set_default("'y' --\u{2028}; DROP TABLE m2_pgedit_inj.victim"),
            "'y'::text",
        ),
    ];
    for (_, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        exec(
            s,
            &format!("CREATE TABLE {S}.victim (x int); CREATE TABLE {S}.t (a text, b int)"),
        )
        .await?;
        let survived = format!(
            "SELECT (SELECT count(*) FROM pg_class WHERE oid = to_regclass('{S}.victim'))::text || '/' || (SELECT string_agg(attname, ',' ORDER BY attnum) FROM pg_attribute WHERE attrelid = '{S}.t'::regclass AND attnum > 0 AND NOT attisdropped)"
        );
        for json in &refused {
            let change: PgChange = serde_json::from_str(json)?;
            match varve_catalog::postgres::edit::plan(s.server_info(), &change) {
                Err(CatalogError::Invalid { .. }) => {}
                other => return Err(format!("not refused: {json}\n=> {other:?}").into()),
            }
        }
        for (json, default) in &accepted {
            let change: PgChange = serde_json::from_str(json)?;
            let p = varve_catalog::postgres::edit::plan(s.server_info(), &change)?;
            assert_eq!(p.statements.len(), 1, "{json}");
            // Worst case: run outside a transaction, as non-transactional
            // plans do, so a smuggled statement would not be rolled back.
            for statement in &p.statements {
                exec(s, statement).await?;
            }
            assert_eq!(
                text(s, &format!("SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = '{S}.t'::regclass")).await?,
                *default
            );
        }
        assert_eq!(text(s, &survived).await?, "1/a,b");
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}

/// Every type-name form the editors accept is a real type on 13 and 18:
/// columns of a table, a routine's argument and return types, a domain's
/// base type and a signature list.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn accepted_type_names_work() -> R {
    const S: &str = "m2_pgedit_tn";
    let forms: &[(&str, &str)] = &[
        ("integer", "integer"),
        ("pg_catalog.int8", "bigint"),
        ("\"char\"", "\"char\""),
        ("double precision", "double precision"),
        ("character varying(20)", "character varying(20)"),
        ("char varying(5)", "character varying(5)"),
        ("character(3)", "character(3)"),
        ("national character(3)", "character(3)"),
        ("national char varying(4)", "character varying(4)"),
        ("national character varying", "character varying"),
        ("nchar varying(6)", "character varying(6)"),
        ("nchar(2)", "character(2)"),
        ("bit varying(8)", "bit varying(8)"),
        ("bit(3)", "bit(3)"),
        ("float(10)", "real"),
        ("numeric(10, 2)", "numeric(10,2)"),
        ("time", "time without time zone"),
        ("time(3) with time zone", "time(3) with time zone"),
        ("time without time zone", "time without time zone"),
        (
            "timestamp(2) without time zone",
            "timestamp(2) without time zone",
        ),
        ("timestamp with time zone", "timestamp with time zone"),
        ("interval", "interval"),
        ("interval(4)", "interval(4)"),
        ("interval year", "interval year"),
        ("interval month", "interval month"),
        ("interval day", "interval day"),
        ("interval hour", "interval hour"),
        ("interval minute", "interval minute"),
        ("interval second(2)", "interval second(2)"),
        ("interval year to month", "interval year to month"),
        ("interval day to hour", "interval day to hour"),
        ("interval day to minute", "interval day to minute"),
        ("interval day to second(3)", "interval day to second(3)"),
        ("interval hour to minute", "interval hour to minute"),
        ("interval hour to second", "interval hour to second"),
        ("interval minute to second", "interval minute to second"),
        ("numeric(10,2)[]", "numeric(10,2)[]"),
        ("int[3][]", "integer[]"),
        ("integer ARRAY", "integer[]"),
        ("integer ARRAY[3]", "integer[]"),
        ("text[]", "text[]"),
        ("\"m2_pgedit_tn\".\"my type\"", "m2_pgedit_tn.\"my type\""),
    ];
    for (major, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        run(
            s,
            PgChange::Domain(DomainChange::Create(CreateDomain {
                name: q(S, "my type"),
                data_type: "double precision".into(),
                collation: None,
                default: None,
                not_null: false,
                checks: vec![],
                comment: None,
            })),
        )
        .await?;
        let mut forms: Vec<(&str, &str)> = forms.to_vec();
        if major >= 15 {
            // Negative scale is new in 15.
            forms.push(("numeric(10, -2)", "numeric(10,-2)"));
        }
        let columns = forms
            .iter()
            .enumerate()
            .map(|(i, (t, _))| ColumnDef::new(format!("c{i:02}"), *t))
            .collect();
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(plain_table(
                q(S, "types"),
                columns,
            )))),
        )
        .await?;
        let got = rows(
            s,
            &format!("SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = '{S}.types'::regclass AND attnum > 0 ORDER BY attnum"),
        )
        .await?;
        let expected: Vec<Vec<Option<String>>> = forms
            .iter()
            .map(|(_, f)| vec![Some((*f).to_owned())])
            .collect();
        assert_eq!(got, expected);

        // Argument, return and domain base types, and a signature list
        // (PostgreSQL drops modifiers of argument and return types).
        run(
            s,
            PgChange::Routine(RoutineChange::Create(CreateRoutine {
                arguments: vec![
                    RoutineArg::new("a", "interval day to second(3)"),
                    RoutineArg::new("b", "national character varying(4)"),
                ],
                returns: returns("timestamp(2) with time zone"),
                ..routine(
                    RoutineKind::Function,
                    q(S, "f"),
                    "sql",
                    source("SELECT now()"),
                )
            })),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT pg_get_function_identity_arguments('{S}.f'::regproc) || ' -> ' || format_type(prorettype, NULL) FROM pg_proc WHERE oid = '{S}.f'::regproc")).await?,
            "a interval, b character varying -> timestamp with time zone"
        );
        run(
            s,
            PgChange::Domain(DomainChange::Create(CreateDomain {
                name: q(S, "codes"),
                data_type: "character varying(10)[]".into(),
                collation: None,
                default: None,
                not_null: false,
                checks: vec![],
                comment: None,
            })),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT format_type(typbasetype, typtypmod) FROM pg_type WHERE oid = '{S}.codes'::regtype")).await?,
            "character varying(10)[]"
        );
        run(
            s,
            PgChange::Routine(RoutineChange::Drop {
                routine: RoutineKind::Function,
                name: q(S, "f"),
                signature: "a interval day to second(3), b character varying".into(),
                if_exists: false,
                behavior: DropBehavior::Restrict,
            }),
        )
        .await?;
        // Trailing clauses in a type field are refused before any SQL runs.
        for bad in [
            "int PRIMARY KEY",
            "int REFERENCES m2_pgedit_tn.types ON DELETE CASCADE",
            "int GENERATED ALWAYS AS IDENTITY",
        ] {
            let change = PgChange::Table(TableChange::Alter {
                name: q(S, "types"),
                operations: vec![TableOperation::AddColumn {
                    column: ColumnDef::new("z", bad),
                    if_not_exists: false,
                }],
            });
            assert!(matches!(
                varve_catalog::postgres::edit::plan(s.server_info(), &change),
                Err(CatalogError::Invalid { .. })
            ));
        }
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}

/// Type modifiers that are constants or names, `%TYPE` argument and return
/// types, and ordered-set aggregate signatures, on 13 and 18.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn modifiers_percent_type_and_aggregate_signatures() -> R {
    const S: &str = "m2_pgedit_tm";
    for (_, port) in servers() {
        let s = connect(port, "varve").await?;
        let s = s.as_ref();
        reset_schema(s, &[S]).await?;
        create_schema(s, S).await?;
        // String constants as modifiers.
        run(
            s,
            PgChange::Table(TableChange::Create(Box::new(plain_table(
                q(S, "t"),
                vec![
                    ColumnDef::new("a", "numeric('10', '2')"),
                    ColumnDef::new("b", "integer"),
                ],
            )))),
        )
        .await?;
        assert_eq!(
            text(s, &format!("SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = '{S}.t'::regclass AND attnum = 1")).await?,
            "numeric(10,2)"
        );
        // A name as a modifier reaches the type's modifier function (as for
        // PostGIS `geometry(Point, 4326)`); numeric rejects it with a data
        // error, not a syntax error.
        let p = varve_catalog::postgres::edit::plan(
            s.server_info(),
            &PgChange::Table(TableChange::Alter {
                name: q(S, "t"),
                operations: vec![TableOperation::AddColumn {
                    column: ColumnDef::new("c", "numeric(prec)"),
                    if_not_exists: false,
                }],
            }),
        )?;
        match s.execute(&p.statements[0], &ExecOptions::default()).await {
            Err(e) => assert_eq!(e.code.as_deref(), Some("22P02"), "{e}"),
            Ok(_) => return Err("numeric(prec) was accepted".into()),
        }

        // `%TYPE` in argument and return types and in a signature.
        run(
            s,
            PgChange::Routine(RoutineChange::Create(CreateRoutine {
                arguments: vec![RoutineArg::new("x", format!("{S}.t.b%TYPE"))],
                returns: returns(&format!("{S}.t.b%TYPE")),
                ..routine(
                    RoutineKind::Function,
                    q(S, "same"),
                    "sql",
                    source("SELECT x"),
                )
            })),
        )
        .await?;
        assert_eq!(text(s, &format!("SELECT {S}.same(5)::text")).await?, "5");
        run(
            s,
            PgChange::Comment(CommentChange {
                target: object(
                    ObjectKind::Function,
                    S,
                    None,
                    "same",
                    Some(&format!("{S}.t.b%TYPE")),
                ),
                comment: Some("by column type".into()),
            }),
        )
        .await?;
        assert_eq!(
            one(
                s,
                &format!("SELECT obj_description('{S}.same(integer)'::regprocedure, 'pg_proc')")
            )
            .await?,
            Some("by column type".into())
        );
        run(
            s,
            PgChange::Routine(RoutineChange::Drop {
                routine: RoutineKind::Function,
                name: q(S, "same"),
                signature: format!("x {S}.t.b%TYPE"),
                if_exists: false,
                behavior: DropBehavior::Restrict,
            }),
        )
        .await?;

        // An ordered-set aggregate addressed by its identity signature.
        exec(
            s,
            &format!(
                "CREATE FUNCTION {S}.fin(double precision[], double precision) RETURNS double precision LANGUAGE sql AS 'SELECT $2';
                 CREATE AGGREGATE {S}.pick(double precision ORDER BY double precision) (SFUNC = array_append, STYPE = double precision[], FINALFUNC = {S}.fin)"
            ),
        )
        .await?;
        let signature = text(
            s,
            &format!("SELECT pg_get_function_identity_arguments('{S}.pick'::regproc)"),
        )
        .await?;
        assert_eq!(signature, "double precision ORDER BY double precision");
        let aggregate = object(ObjectKind::Aggregate, S, None, "pick", Some(&signature));
        run(
            s,
            PgChange::Comment(CommentChange {
                target: aggregate.clone(),
                comment: Some("ordered".into()),
            }),
        )
        .await?;
        run(
            s,
            PgChange::Owner(OwnerChange {
                target: aggregate,
                owner: RoleSpec::CurrentUser,
            }),
        )
        .await?;
        assert_eq!(
            one(
                s,
                &format!("SELECT obj_description('{S}.pick'::regproc, 'pg_proc')")
            )
            .await?,
            Some("ordered".into())
        );
        reset_schema(s, &[S]).await?;
    }
    Ok(())
}
