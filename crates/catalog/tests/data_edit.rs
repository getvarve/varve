//! Grid data editing against the M2 database matrix: PostgreSQL 13 and 18,
//! MySQL 8.0 and 9.7, MariaDB 10.6 and 12.3.
//!
//! Run with `cargo test -p varve-catalog --test data_edit -- --include-ignored`.
//! Endpoints come from environment variables (defaults in brackets):
//! `VARVE_M2_HOST` (127.0.0.1), `VARVE_M2_PG13_PORT` (55513),
//! `VARVE_M2_PG18_PORT` (55518), `VARVE_M2_MYSQL80_PORT` (35080),
//! `VARVE_M2_MYSQL97_PORT` (35097), `VARVE_M2_MARIADB106_PORT` (35106),
//! `VARVE_M2_MARIADB123_PORT` (35123). Each test works in its own
//! `m2_dataedit` schema (PostgreSQL) or database (MySQL, MariaDB), dropped
//! before and after the run.

use std::error::Error;

use varve_catalog::CatalogError;
use varve_catalog::data_edit::{
    ApplyOutcome, CellValue, ColumnInfo, ConflictReason, GeneratedKind, IdentityKind, KeyCollation,
    KeyInfo, KeyKind, PlanOptions, QualifiedOperator, RowChange, RowKey, TableTarget, apply,
    editable_key, plan_edits,
};
use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, ErrorKind, ExecOptions, ServerInfo, Session, StatementResult, Value,
};
use varve_driver_mysql::MysqlDriver;
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
type Row = Vec<(String, Value)>;

const SCHEMA: &str = "m2_dataedit";

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn params(engine: Engine, port: u16, read_only: bool) -> ConnectParams {
    let host = std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let mut params = ConnectParams::new(engine, host, port);
    match engine {
        Engine::Postgres => {
            params.username = Some("varve".into());
            params.database = Some("varve".into());
        }
        _ => params.username = Some("root".into()),
    }
    params.password = Some(Secret::new("varve_dev".into()));
    params.read_only = read_only;
    params
}

async fn connect(engine: Engine, port: u16, read_only: bool) -> TestResult<Box<dyn Session>> {
    let params = params(engine, port, read_only);
    Ok(match engine {
        Engine::Postgres => PostgresDriver::new().connect(&params).await?,
        _ => MysqlDriver::new().connect(&params).await?,
    })
}

async fn exec(session: &dyn Session, sql: &str) -> TestResult<Vec<StatementResult>> {
    Ok(session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}: {e}"))?
        .results)
}

/// Every row of the first result set, as `(column, value)` pairs.
async fn rows(session: &dyn Session, sql: &str) -> TestResult<Vec<Row>> {
    for result in exec(session, sql).await? {
        if let StatementResult::Rows { columns, rows, .. } = result {
            return Ok(rows
                .into_iter()
                .map(|values| columns.iter().map(|c| c.name.clone()).zip(values).collect())
                .collect());
        }
    }
    Err(format!("{sql} returned no rows").into())
}

fn get<'a>(row: &'a Row, column: &str) -> TestResult<&'a Value> {
    row.iter()
        .find(|(name, _)| name == column)
        .map(|(_, value)| value)
        .ok_or_else(|| format!("column {column} is missing from {row:?}").into())
}

struct Ctx {
    name: &'static str,
    engine: Engine,
    port: u16,
    server: ServerInfo,
    a: Box<dyn Session>,
    b: Box<dyn Session>,
}

impl Ctx {
    fn pg(&self) -> bool {
        self.engine == Engine::Postgres
    }

    fn target(&self, table: &str) -> TableTarget {
        if self.pg() {
            TableTarget {
                database: "varve".into(),
                schema: Some(SCHEMA.into()),
                table: table.into(),
            }
        } else {
            TableTarget {
                database: SCHEMA.into(),
                schema: None,
                table: table.into(),
            }
        }
    }

    fn table(&self, table: &str) -> String {
        if self.pg() {
            format!("{SCHEMA}.{table}")
        } else {
            format!("`{SCHEMA}`.`{table}`")
        }
    }

    async fn key(&self, table: &str) -> TestResult<KeyInfo> {
        editable_key(self.a.as_ref(), &self.server, &self.target(table))
            .await?
            .ok_or_else(|| format!("{}: {table} has no editable key", self.name).into())
    }

    async fn plan_apply(
        &self,
        table: &str,
        changes: &[RowChange],
        check_original: bool,
    ) -> TestResult<ApplyOutcome> {
        let options = PlanOptions {
            check_original,
            key: self.key(table).await?,
        };
        let plan = plan_edits(&self.server, &self.target(table), changes, &options)?;
        Ok(apply(self.a.as_ref(), &self.server, &plan).await?)
    }

    async fn count(&self, table: &str, filter: &str) -> TestResult<i64> {
        let found = rows(
            self.a.as_ref(),
            &format!(
                "SELECT COUNT(*) AS n FROM {} WHERE {filter}",
                self.table(table)
            ),
        )
        .await?;
        match found.first().map(|row| get(row, "n")).transpose()? {
            Some(Value::Int(n)) => Ok(*n),
            Some(Value::Decimal(n)) => Ok(n.parse()?),
            other => Err(format!("unexpected count {other:?}").into()),
        }
    }
}

fn applied_rows(outcome: ApplyOutcome) -> TestResult<Vec<Option<Row>>> {
    match outcome {
        ApplyOutcome::Applied { rows } => Ok(rows.into_iter().map(|r| r.row).collect()),
        other => Err(format!("expected the changes to apply, got {other:?}").into()),
    }
}

fn single_row(outcome: ApplyOutcome) -> TestResult<Row> {
    applied_rows(outcome)?
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| "the change returned no row".into())
}

fn cell(value: Value) -> CellValue {
    CellValue::Value(value)
}

fn pairs(values: &[(&str, Value)]) -> Vec<(String, Value)> {
    values
        .iter()
        .map(|(n, v)| ((*n).to_owned(), v.clone()))
        .collect()
}

fn cells(values: &[(&str, Value)]) -> Vec<(String, CellValue)> {
    values
        .iter()
        .map(|(n, v)| ((*n).to_owned(), cell(v.clone())))
        .collect()
}

fn key(values: &[(&str, Value)]) -> RowKey {
    RowKey {
        columns: pairs(values),
    }
}

async fn setup(ctx: &Ctx) -> TestResult {
    let a = ctx.a.as_ref();
    if ctx.pg() {
        exec(a, &format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE")).await?;
        let mut sql = format!(
            "CREATE SCHEMA {SCHEMA};
             CREATE TYPE {SCHEMA}.mood AS ENUM ('sad', 'ok', 'happy');
             CREATE TABLE {SCHEMA}.items (
               id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
               name text NOT NULL,
               payload bytea, doc jsonb, raw json, uid uuid, tags text[], nums int[],
               mood {SCHEMA}.mood, span interval, born date, at timestamptz,
               local_at timestamp, t timetz, weight real, price numeric(10,2), code char(4),
               created timestamptz NOT NULL DEFAULT now(),
               status text NOT NULL DEFAULT 'new',
               name_len int GENERATED ALWAYS AS (length(name)) STORED);
             CREATE TABLE {SCHEMA}.pairs (a int, b text, val text, PRIMARY KEY (a, b));
             CREATE TABLE {SCHEMA}.uniq (code text NOT NULL, maybe text UNIQUE, note text,
               CONSTRAINT uniq_code UNIQUE (code));
             CREATE TABLE {SCHEMA}.nokey (x int, y text UNIQUE);
             CREATE TABLE {SCHEMA}.deferred (x int NOT NULL,
               CONSTRAINT dx UNIQUE (x) DEFERRABLE);
             CREATE TABLE {SCHEMA}.partial (x int NOT NULL);
             CREATE UNIQUE INDEX partial_x ON {SCHEMA}.partial (x) WHERE x > 0;
             CREATE TABLE {SCHEMA}.ident (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               v text);
             CREATE TABLE {SCHEMA}.dup (k int NOT NULL, v text);
             CREATE VIEW {SCHEMA}.v AS SELECT * FROM {SCHEMA}.pairs;"
        );
        if ctx.server.at_least(18, 0) {
            sql.push_str(&format!(
                "ALTER TABLE {SCHEMA}.items ADD COLUMN name_upper text \
                 GENERATED ALWAYS AS (upper(name)) VIRTUAL;"
            ));
        }
        exec(a, &sql).await?;
    } else {
        exec(a, &format!("DROP DATABASE IF EXISTS {SCHEMA}")).await?;
        exec(
            a,
            &format!(
                "CREATE DATABASE {SCHEMA};
                 CREATE TABLE {SCHEMA}.items (
                   id bigint AUTO_INCREMENT PRIMARY KEY,
                   name varchar(100) NOT NULL,
                   payload varbinary(64), doc json, uid binary(16),
                   mood enum('sad', 'ok', 'happy'), born date,
                   at timestamp(6) NULL DEFAULT NULL, local_at datetime(6), t time(3),
                   weight float, price decimal(10,2), bits bit(5),
                   created datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
                   status varchar(10) NOT NULL DEFAULT 'new',
                   name_len int GENERATED ALWAYS AS (char_length(name)) STORED,
                   name_upper varchar(100) GENERATED ALWAYS AS (upper(name)) VIRTUAL);
                 CREATE TABLE {SCHEMA}.pairs (a int, b varchar(20), val varchar(20),
                   PRIMARY KEY (a, b));
                 CREATE TABLE {SCHEMA}.uniq (code varchar(20) NOT NULL, maybe varchar(20) NULL,
                   note varchar(20), UNIQUE KEY uniq_code (code), UNIQUE KEY uniq_maybe (maybe));
                 CREATE TABLE {SCHEMA}.nokey (x int, y varchar(10) UNIQUE);
                 CREATE TABLE {SCHEMA}.dup (k int NOT NULL, v varchar(10));
                 CREATE VIEW {SCHEMA}.v AS SELECT * FROM {SCHEMA}.pairs;"
            ),
        )
        .await?;
    }
    Ok(())
}

async fn teardown(ctx: &Ctx) -> TestResult {
    let sql = if ctx.pg() {
        format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE")
    } else {
        format!("DROP DATABASE IF EXISTS {SCHEMA}")
    };
    exec(ctx.a.as_ref(), &sql).await?;
    Ok(())
}

async fn keys_are_found(ctx: &Ctx) -> TestResult {
    let items = ctx.key("items").await?;
    assert_eq!(items.kind, KeyKind::Primary, "{}", ctx.name);
    assert_eq!(items.columns, vec!["id".to_owned()]);
    let column = |name: &str| -> TestResult<&ColumnInfo> {
        items
            .table_columns
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| format!("{name} missing").into())
    };
    let id = column("id")?;
    assert!(id.has_default && id.editable && !id.nullable);
    if ctx.pg() {
        assert_eq!(id.identity, Some(IdentityKind::ByDefault));
        assert_eq!(column("mood")?.type_schema.as_deref(), Some(SCHEMA));
        assert_eq!(column("tags")?.type_name, "_text");
        assert_eq!(column("tags")?.data_type, "text[]");
    } else {
        assert!(id.auto_increment);
        assert_eq!(column("weight")?.type_name, "float");
    }
    let len = column("name_len")?;
    assert_eq!(len.generated, Some(GeneratedKind::Stored));
    assert!(!len.editable);
    if !ctx.pg() || ctx.server.at_least(18, 0) {
        assert_eq!(
            column("name_upper")?.generated,
            Some(GeneratedKind::Virtual)
        );
    }
    assert!(column("created")?.has_default);
    assert!(!column("payload")?.has_default);

    let composite = ctx.key("pairs").await?;
    assert_eq!(composite.columns, vec!["a".to_owned(), "b".to_owned()]);
    let unique = ctx.key("uniq").await?;
    assert_eq!(unique.kind, KeyKind::Unique);
    assert_eq!(unique.name, "uniq_code");
    assert_eq!(unique.columns, vec!["code".to_owned()]);

    let mut read_only = vec!["nokey", "dup", "v"];
    if ctx.pg() {
        read_only.extend(["deferred", "partial"]);
        let ident = ctx.key("ident").await?;
        let id = ident.table_columns.first().ok_or("ident has no columns")?;
        assert_eq!(id.identity, Some(IdentityKind::Always));
        assert!(!id.editable);
    }
    for table in read_only {
        let found = editable_key(ctx.a.as_ref(), &ctx.server, &ctx.target(table)).await?;
        assert_eq!(found, None, "{}: {table}", ctx.name);
    }
    let missing = editable_key(ctx.a.as_ref(), &ctx.server, &ctx.target("missing")).await;
    assert!(matches!(missing, Err(CatalogError::NotFound { .. })));
    Ok(())
}

/// The typed values written to `items`, and how the server returns them.
fn typed_values(ctx: &Ctx) -> Vec<(&'static str, Value, Value)> {
    let same = |name: &'static str, v: Value| (name, v.clone(), v);
    let mut values = vec![
        same("name", Value::Text("Zoë 'quoted' \\ back".into())),
        same("payload", Value::Bytes(vec![0, 1, 0x5c, 0x27, 0xff])),
        same("mood", Value::Text("happy".into())),
        same("born", Value::Date("2026-10-19".into())),
        same("weight", Value::Float(1.1)),
        same("price", Value::Decimal("12.50".into())),
    ];
    if ctx.pg() {
        values.extend([
            same("doc", Value::json(r#"{"a": 1, "b": [1, 2.50]}"#)),
            same("raw", Value::json(r#"{"b":1,  "a":2}"#)),
            same(
                "uid",
                Value::Uuid("6f1c2b8e-1d2e-4f00-8a00-00000000beef".into()),
            ),
            same(
                "tags",
                Value::Array(vec![
                    Value::Text("a,b".into()),
                    Value::Text("q\"uote {x}".into()),
                    Value::Null,
                ]),
            ),
            same("nums", Value::Array(vec![Value::Int(1), Value::Int(-2)])),
            same("span", Value::Interval("P1Y2M3DT4H5M6.5S".into())),
            same(
                "at",
                Value::TimestampTz("2026-10-19T11:45:00.123456Z".into()),
            ),
            same("local_at", Value::Timestamp("2026-10-19T13:45:00.5".into())),
            same("t", Value::Time("13:45:00.25+02".into())),
            same("code", Value::Text("abcd".into())),
        ]);
    } else {
        let doc = r#"{"a": 1, "b": [1, 2.5]}"#;
        values.extend([
            if ctx.engine == Engine::Mysql {
                same("doc", Value::json(doc))
            } else {
                ("doc", Value::json(doc), Value::Text(doc.into()))
            },
            same("uid", Value::Bytes((0u8..16).collect())),
            same(
                "at",
                Value::TimestampTz("2026-10-19T11:45:00.123456Z".into()),
            ),
            same(
                "local_at",
                Value::Timestamp("2026-10-19T13:45:00.500000".into()),
            ),
            same("t", Value::Time("13:45:00.250".into())),
            same("bits", Value::Int(5)),
        ]);
    }
    values
}

/// Other values for every typed column, for the update.
fn changed_values(ctx: &Ctx) -> Vec<(&'static str, Value)> {
    let mut values = vec![
        ("name", Value::Text("ZOË".into())),
        ("payload", Value::Bytes(vec![9, 8, 7])),
        ("mood", Value::Text("sad".into())),
        ("born", Value::Date("1999-12-31".into())),
        ("weight", Value::Float(-0.25)),
        ("price", Value::Decimal("-0.01".into())),
    ];
    if ctx.pg() {
        values.extend([
            ("doc", Value::json("[]")),
            ("raw", Value::json(r#"{"z":0, "a":[]}"#)),
            (
                "uid",
                Value::Uuid("00000000-0000-4000-8000-000000000000".into()),
            ),
            ("tags", Value::Array(vec![])),
            ("nums", Value::Array(vec![Value::Null])),
            ("span", Value::Interval("PT-1S".into())),
            ("at", Value::TimestampTz("1970-01-01T00:00:00Z".into())),
            ("local_at", Value::Timestamp("2000-02-29T00:00:00".into())),
            ("t", Value::Time("00:00:00-05:30".into())),
            ("code", Value::Text("wxyz".into())),
        ]);
    }
    values
}

async fn select_item(ctx: &Ctx, id: &Value) -> TestResult<Row> {
    let id = match id {
        Value::Int(i) => *i,
        other => return Err(format!("unexpected id {other:?}").into()),
    };
    rows(
        ctx.b.as_ref(),
        &format!("SELECT * FROM {} WHERE id = {id}", ctx.table("items")),
    )
    .await?
    .into_iter()
    .next()
    .ok_or_else(|| format!("item {id} not found").into())
}

async fn typed_values_round_trip(ctx: &Ctx) -> TestResult {
    let values = typed_values(ctx);
    let inserted = single_row(
        ctx.plan_apply(
            "items",
            &[RowChange::Insert {
                values: values
                    .iter()
                    .map(|(n, v, _)| ((*n).to_owned(), cell(v.clone())))
                    .collect(),
            }],
            false,
        )
        .await?,
    )?;
    for (name, _, expected) in &values {
        assert_eq!(get(&inserted, name)?, expected, "{}: {name}", ctx.name);
    }
    // Defaults and generated values come back with the insert.
    assert!(matches!(get(&inserted, "id")?, Value::Int(_)));
    assert_eq!(get(&inserted, "status")?, &Value::Text("new".into()));
    assert!(!matches!(get(&inserted, "created")?, Value::Null));
    assert_eq!(get(&inserted, "name_len")?, &Value::Int(19), "{}", ctx.name);
    let id = get(&inserted, "id")?.clone();
    assert_eq!(select_item(ctx, &id).await?, inserted);

    // Update every typed column, checking every original value exactly.
    let originals: Vec<(String, Value)> = values
        .iter()
        .map(|(n, _, _)| Ok(((*n).to_owned(), get(&inserted, n)?.clone())))
        .collect::<TestResult<_>>()?;
    let changed = changed_values(ctx);
    let updated = single_row(
        ctx.plan_apply(
            "items",
            &[RowChange::Update {
                key: key(&[("id", id.clone())]),
                original: originals.clone(),
                set: cells(&changed),
            }],
            true,
        )
        .await?,
    )?;
    for (name, expected) in &changed {
        assert_eq!(get(&updated, name)?, expected, "{}: {name}", ctx.name);
    }
    assert_eq!(get(&updated, "name_len")?, &Value::Int(3));
    if ctx.engine != Engine::Postgres || ctx.server.at_least(18, 0) {
        assert_eq!(get(&updated, "name_upper")?, &Value::Text("ZOË".into()));
    }
    assert_eq!(select_item(ctx, &id).await?, updated);

    // Writing the original values back, checked against the updated row.
    let restored = single_row(
        ctx.plan_apply(
            "items",
            &[RowChange::Update {
                key: key(&[("id", id.clone())]),
                original: originals
                    .iter()
                    .map(|(n, _)| Ok((n.clone(), get(&updated, n)?.clone())))
                    .collect::<TestResult<_>>()?,
                set: originals
                    .iter()
                    .map(|(n, v)| (n.clone(), cell(v.clone())))
                    .collect(),
            }],
            true,
        )
        .await?,
    )?;
    for (name, value) in &originals {
        assert_eq!(get(&restored, name)?, value, "{}: {name}", ctx.name);
    }

    // NULL originals: a NULL cell is matched NULL-safely.
    let nulled = single_row(
        ctx.plan_apply(
            "items",
            &[RowChange::Update {
                key: key(&[("id", id.clone())]),
                original: pairs(&[("payload", get(&restored, "payload")?.clone())]),
                set: cells(&[("payload", Value::Null)]),
            }],
            true,
        )
        .await?,
    )?;
    assert_eq!(get(&nulled, "payload")?, &Value::Null);
    let filled = single_row(
        ctx.plan_apply(
            "items",
            &[RowChange::Update {
                key: key(&[("id", id.clone())]),
                original: pairs(&[("payload", Value::Null)]),
                set: cells(&[("payload", Value::Bytes(vec![1]))]),
            }],
            true,
        )
        .await?,
    )?;
    assert_eq!(get(&filled, "payload")?, &Value::Bytes(vec![1]));

    // Generated columns are refused unless set to DEFAULT.
    let options = PlanOptions {
        check_original: false,
        key: ctx.key("items").await?,
    };
    assert!(
        plan_edits(
            &ctx.server,
            &ctx.target("items"),
            &[RowChange::Update {
                key: key(&[("id", id.clone())]),
                original: vec![],
                set: cells(&[("name_len", Value::Int(1))]),
            }],
            &options,
        )
        .is_err()
    );

    // Delete with every original value checked.
    let outcome = ctx
        .plan_apply(
            "items",
            &[RowChange::Delete {
                key: key(&[("id", id.clone())]),
                original: filled.clone(),
            }],
            true,
        )
        .await?;
    assert_eq!(applied_rows(outcome)?, vec![None]);
    assert_eq!(ctx.count("items", "1 = 1").await?, 0);

    // An insert with nothing but defaults.
    let empty = single_row(
        ctx.plan_apply(
            "pairs",
            &[RowChange::Insert {
                values: cells(&[("a", Value::Int(100)), ("b", Value::Text("d".into()))]),
            }],
            false,
        )
        .await?,
    )?;
    assert_eq!(get(&empty, "val")?, &Value::Null);
    Ok(())
}

async fn composite_and_unique_keys(ctx: &Ctx) -> TestResult {
    let inserted = applied_rows(
        ctx.plan_apply(
            "pairs",
            &[
                RowChange::Insert {
                    values: cells(&[
                        ("a", Value::Int(1)),
                        ("b", Value::Text("x".into())),
                        ("val", Value::Text("one".into())),
                    ]),
                },
                RowChange::Insert {
                    values: vec![
                        ("a".into(), cell(Value::Int(1))),
                        ("b".into(), cell(Value::Text("y".into()))),
                        ("val".into(), CellValue::Expression("upper('two')".into())),
                    ],
                },
            ],
            false,
        )
        .await?,
    )?;
    let second = inserted.get(1).cloned().flatten().ok_or("no second row")?;
    assert_eq!(get(&second, "val")?, &Value::Text("TWO".into()));

    // Change one column of the composite key; the row is read back by the
    // new key.
    let moved = single_row(
        ctx.plan_apply(
            "pairs",
            &[RowChange::Update {
                key: key(&[("b", Value::Text("x".into())), ("a", Value::Int(1))]),
                original: pairs(&[("b", Value::Text("x".into()))]),
                set: cells(&[("b", Value::Text("z".into()))]),
            }],
            true,
        )
        .await?,
    )?;
    assert_eq!(get(&moved, "b")?, &Value::Text("z".into()));
    assert_eq!(get(&moved, "val")?, &Value::Text("one".into()));
    applied_rows(
        ctx.plan_apply(
            "pairs",
            &[RowChange::Delete {
                key: key(&[("a", Value::Int(1)), ("b", Value::Text("y".into()))]),
                original: vec![],
            }],
            false,
        )
        .await?,
    )?;
    assert_eq!(ctx.count("pairs", "a = 1").await?, 1);

    // Unique key fallback.
    let row = single_row(
        ctx.plan_apply(
            "uniq",
            &[RowChange::Insert {
                values: cells(&[("code", Value::Text("c1".into()))]),
            }],
            false,
        )
        .await?,
    )?;
    assert_eq!(get(&row, "note")?, &Value::Null);
    let row = single_row(
        ctx.plan_apply(
            "uniq",
            &[RowChange::Update {
                key: key(&[("code", Value::Text("c1".into()))]),
                original: pairs(&[("note", Value::Null)]),
                set: cells(&[("note", Value::Text("noted".into()))]),
            }],
            true,
        )
        .await?,
    )?;
    assert_eq!(get(&row, "note")?, &Value::Text("noted".into()));
    applied_rows(
        ctx.plan_apply(
            "uniq",
            &[RowChange::Delete {
                key: key(&[("code", Value::Text("c1".into()))]),
                original: pairs(&[("note", Value::Text("noted".into()))]),
            }],
            true,
        )
        .await?,
    )?;
    assert_eq!(ctx.count("uniq", "1 = 1").await?, 0);

    if ctx.pg() {
        let row = single_row(
            ctx.plan_apply(
                "ident",
                &[RowChange::Insert {
                    values: vec![
                        ("id".into(), CellValue::Default),
                        ("v".into(), cell(Value::Text("x".into()))),
                    ],
                }],
                false,
            )
            .await?,
        )?;
        assert!(matches!(get(&row, "id")?, Value::Int(_)));
    }
    Ok(())
}

async fn concurrent_changes_are_detected(ctx: &Ctx) -> TestResult {
    let pairs_table = ctx.table("pairs");
    exec(
        ctx.a.as_ref(),
        &format!("INSERT INTO {pairs_table} (a, b, val) VALUES (7, 'k', 'abc'), (8, 'k', 'keep')"),
    )
    .await?;
    let row7 = key(&[("a", Value::Int(7)), ("b", Value::Text("k".into()))]);
    let update7 = |original: &str| RowChange::Update {
        key: row7.clone(),
        original: pairs(&[("val", Value::Text(original.into()))]),
        set: cells(&[("val", Value::Text("mine".into()))]),
    };

    // Someone else changes only the case: still a change.
    exec(
        ctx.b.as_ref(),
        &format!("UPDATE {pairs_table} SET val = 'ABC' WHERE a = 7"),
    )
    .await?;
    let outcome = ctx.plan_apply("pairs", &[update7("abc")], true).await?;
    assert!(
        matches!(
            outcome,
            ApplyOutcome::Conflict {
                change: 0,
                reason: ConflictReason::RowChanged,
                rows_matched: 0,
                ..
            }
        ),
        "{}: {outcome:?}",
        ctx.name
    );
    assert_eq!(ctx.count("pairs", "a = 7 AND val = 'ABC'").await?, 1);

    // A conflict in the second change rolls back the first.
    let outcome = ctx
        .plan_apply(
            "pairs",
            &[
                RowChange::Insert {
                    values: cells(&[("a", Value::Int(9)), ("b", Value::Text("new".into()))]),
                },
                RowChange::Update {
                    key: key(&[("a", Value::Int(8)), ("b", Value::Text("k".into()))]),
                    original: pairs(&[("val", Value::Text("keep".into()))]),
                    set: cells(&[("val", Value::Text("changed".into()))]),
                },
                update7("abc"),
            ],
            true,
        )
        .await?;
    assert!(
        matches!(outcome, ApplyOutcome::Conflict { change: 2, .. }),
        "{}: {outcome:?}",
        ctx.name
    );
    assert_eq!(ctx.count("pairs", "a = 9").await?, 0);
    assert_eq!(ctx.count("pairs", "a = 8 AND val = 'keep'").await?, 1);

    // With the current value it applies.
    applied_rows(ctx.plan_apply("pairs", &[update7("ABC")], true).await?)?;

    // Without original checks a deleted row is still a conflict.
    exec(
        ctx.b.as_ref(),
        &format!("DELETE FROM {pairs_table} WHERE a = 7"),
    )
    .await?;
    let outcome = ctx.plan_apply("pairs", &[update7("mine")], false).await?;
    assert!(
        matches!(
            outcome,
            ApplyOutcome::Conflict {
                reason: ConflictReason::RowChanged,
                ..
            }
        ),
        "{}: {outcome:?}",
        ctx.name
    );
    let outcome = ctx
        .plan_apply(
            "pairs",
            &[RowChange::Delete {
                key: row7.clone(),
                original: vec![],
            }],
            false,
        )
        .await?;
    assert!(matches!(outcome, ApplyOutcome::Conflict { .. }));

    // A server error names the change and leaves nothing applied.
    let outcome = ctx
        .plan_apply(
            "pairs",
            &[
                RowChange::Insert {
                    values: cells(&[("a", Value::Int(10)), ("b", Value::Text("n".into()))]),
                },
                RowChange::Insert {
                    values: cells(&[("a", Value::Int(8)), ("b", Value::Text("k".into()))]),
                },
            ],
            false,
        )
        .await?;
    assert!(
        matches!(outcome, ApplyOutcome::Failed { change: 1, .. }),
        "{}: {outcome:?}",
        ctx.name
    );
    assert_eq!(ctx.count("pairs", "a = 10").await?, 0);
    Ok(())
}

async fn non_unique_keys_roll_back(ctx: &Ctx) -> TestResult {
    let dup = ctx.table("dup");
    exec(
        ctx.a.as_ref(),
        &format!("INSERT INTO {dup} (k, v) VALUES (1, 'a'), (1, 'b')"),
    )
    .await?;
    let column = |name: &str, pg_type: &str, my_type: &str, nullable: bool| ColumnInfo {
        name: name.into(),
        data_type: my_type.into(),
        type_schema: ctx.pg().then(|| "pg_catalog".to_owned()),
        type_name: if ctx.pg() { pg_type } else { my_type }.into(),
        base_type: None,
        array: false,
        array_of_arrays: false,
        nullable,
        has_default: false,
        generated: None,
        identity: None,
        auto_increment: false,
        editable: true,
    };
    // A key the table does not really have.
    let options = PlanOptions {
        check_original: false,
        key: KeyInfo {
            kind: KeyKind::Unique,
            name: "not_really_unique".into(),
            columns: vec!["k".into()],
            table_columns: vec![
                column("k", "int4", "int", false),
                column("v", "text", "varchar", true),
            ],
            equality_operators: if ctx.pg() {
                vec![QualifiedOperator {
                    schema: "pg_catalog".into(),
                    name: "=".into(),
                }]
            } else {
                Vec::new()
            },
            key_collations: Vec::new(),
        },
    };
    for change in [
        RowChange::Update {
            key: key(&[("k", Value::Int(1))]),
            original: vec![],
            set: cells(&[("v", Value::Text("c".into()))]),
        },
        RowChange::Delete {
            key: key(&[("k", Value::Int(1))]),
            original: vec![],
        },
    ] {
        let plan = plan_edits(&ctx.server, &ctx.target("dup"), &[change], &options)?;
        let outcome = apply(ctx.a.as_ref(), &ctx.server, &plan).await?;
        assert!(
            matches!(
                outcome,
                ApplyOutcome::Conflict {
                    reason: ConflictReason::KeyNotUnique,
                    ..
                }
            ),
            "{}: {outcome:?}",
            ctx.name
        );
        assert_eq!(ctx.count("dup", "v IN ('a', 'b')").await?, 2);
    }
    Ok(())
}

async fn read_only_and_open_transactions(ctx: &Ctx) -> TestResult {
    let read_only = connect(ctx.engine, ctx.port, true).await?;
    let options = PlanOptions {
        check_original: false,
        key: ctx.key("pairs").await?,
    };
    let plan = plan_edits(
        &ctx.server,
        &ctx.target("pairs"),
        &[RowChange::Insert {
            values: cells(&[("a", Value::Int(50)), ("b", Value::Text("ro".into()))]),
        }],
        &options,
    )?;
    match apply(read_only.as_ref(), &ctx.server, &plan).await {
        Err(CatalogError::Driver { error }) => {
            assert_eq!(error.kind, ErrorKind::ReadOnly, "{}: {error:?}", ctx.name);
        }
        other => {
            return Err(format!("{}: expected a read-only error, got {other:?}", ctx.name).into());
        }
    }
    read_only.close().await?;
    assert_eq!(ctx.count("pairs", "a = 50").await?, 0);

    if ctx.engine != Engine::Mysql {
        let begin = if ctx.pg() {
            "BEGIN"
        } else {
            "START TRANSACTION"
        };
        exec(ctx.a.as_ref(), begin).await?;
        exec(
            ctx.a.as_ref(),
            &format!("SELECT COUNT(*) FROM {}", ctx.table("pairs")),
        )
        .await?;
        let outcome = apply(ctx.a.as_ref(), &ctx.server, &plan).await;
        exec(ctx.a.as_ref(), "ROLLBACK").await?;
        assert!(
            matches!(outcome, Err(CatalogError::Invalid { .. })),
            "{}: {outcome:?}",
            ctx.name
        );
        assert_eq!(ctx.count("pairs", "a = 50").await?, 0);
    }
    Ok(())
}

/// PostgreSQL values the client does not see as stored: arrays with lower
/// bounds other than 1 (the driver drops the bounds), also behind a domain,
/// `xml` with an XML declaration (not sent to the client, while what
/// follows it, a CRLF or an indented line, is), also behind a domain, and a
/// key whose unique index has
/// another collation than its column (a case-insensitive column with a
/// unique index in `"C"`), and `int2vector` / `oidvector` columns (no
/// arrays, although they have an element type). Each row can be updated
/// with every original value checked, and deleted. An array of a domain
/// over an array is left out of the check (and writing it is refused with
/// a clear message).
async fn postgres_special_values(ctx: &Ctx) -> TestResult {
    let a = ctx.a.as_ref();
    exec(
        a,
        &format!(
            "CREATE DOMAIN {SCHEMA}.intarr AS int[]; \
             CREATE DOMAIN {SCHEMA}.xdom AS xml; \
             CREATE TABLE {SCHEMA}.arr (id int[] PRIMARY KEY, v int[], d {SCHEMA}.intarr, n int); \
             INSERT INTO {SCHEMA}.arr VALUES ('[0:1]={{5,6}}', '[2:3]={{7,8}}', '[0:1]={{5,6}}', 1); \
             CREATE TABLE {SCHEMA}.xmlt (id int PRIMARY KEY, x xml, xd {SCHEMA}.xdom, n int); \
             INSERT INTO {SCHEMA}.xmlt VALUES \
               (1, '<?xml version=\"1.0\"?><r/>', '<?xml version=\"1.0\"?><r/>', 1), \
               (2, E'<?xml version=\"1.0\"?>\\r\\n<r/>', E'<?xml version=\"1.0\"?>\\r\\n<r/>', 1), \
               (3, E'<?xml version=\"1.0\"?>\\n  <r/>', E'<?xml version=\"1.0\"?>\\n  <r/>', 1), \
               (4, '<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><r/>', NULL, 1); \
             CREATE COLLATION {SCHEMA}.ci (provider = icu, locale = 'und-u-ks-level2', \
               deterministic = false); \
             CREATE TABLE {SCHEMA}.cik (k text COLLATE {SCHEMA}.ci NOT NULL, v int); \
             CREATE UNIQUE INDEX cik_k ON {SCHEMA}.cik (k COLLATE \"C\"); \
             INSERT INTO {SCHEMA}.cik VALUES ('abc', 1), ('ABC', 2); \
             CREATE TABLE {SCHEMA}.vec (id int PRIMARY KEY, iv int2vector, ov oidvector, n int); \
             INSERT INTO {SCHEMA}.vec VALUES (1, '1 2', '3 4', 1); \
             CREATE TABLE {SCHEMA}.nested (id int PRIMARY KEY, v {SCHEMA}.intarr[], n int); \
             INSERT INTO {SCHEMA}.nested VALUES (1, ARRAY['{{5,6}}'::{SCHEMA}.intarr], 1)"
        ),
    )
    .await?;
    for table in ["arr", "xmlt", "vec"] {
        let stored = rows(a, &format!("SELECT * FROM {SCHEMA}.{table}")).await?;
        for row in stored {
            let id = get(&row, "id")?.clone();
            let updated = single_row(
                ctx.plan_apply(
                    table,
                    &[RowChange::Update {
                        key: key(&[("id", id.clone())]),
                        original: row.clone(),
                        set: cells(&[("n", Value::Int(2))]),
                    }],
                    true,
                )
                .await?,
            )
            .map_err(|e| format!("{}: {table}: {e}", ctx.name))?;
            let outcome = ctx
                .plan_apply(
                    table,
                    &[RowChange::Delete {
                        key: key(&[("id", id)]),
                        original: updated,
                    }],
                    true,
                )
                .await?;
            assert!(
                matches!(outcome, ApplyOutcome::Applied { .. }),
                "{}: {table}: {outcome:?}",
                ctx.name
            );
        }
        assert_eq!(ctx.count(table, "true").await?, 0, "{}: {table}", ctx.name);
    }
    // An array of a domain over an array cannot be written back as a
    // literal: the column is not editable and is left out of the
    // original-value check (with a warning); the rest of the row can be
    // updated and the row deleted.
    let nested = ctx.key("nested").await?;
    let v = nested
        .table_columns
        .iter()
        .find(|c| c.name == "v")
        .ok_or("nested.v is missing")?;
    assert!(
        v.array_of_arrays && v.array && !v.editable,
        "{}: {v:?}",
        ctx.name
    );
    let stored = rows(a, &format!("SELECT * FROM {SCHEMA}.nested")).await?;
    let original = stored.first().ok_or("nested has no row")?.clone();
    let options = PlanOptions {
        check_original: true,
        key: nested.clone(),
    };
    let update = [RowChange::Update {
        key: key(&[("id", Value::Int(1))]),
        original: original.clone(),
        set: cells(&[("n", Value::Int(2))]),
    }];
    let plan = plan_edits(&ctx.server, &ctx.target("nested"), &update, &options)?;
    let outcome = apply(a, &ctx.server, &plan).await?;
    assert!(
        matches!(outcome, ApplyOutcome::Applied { .. }),
        "{}: nested update: {outcome:?}",
        ctx.name
    );
    assert_eq!(ctx.count("nested", "n = 2").await?, 1, "{}", ctx.name);
    // Writing the column itself is refused with a clear message.
    let refused = ctx
        .plan_apply(
            "nested",
            &[RowChange::Update {
                key: key(&[("id", Value::Int(1))]),
                original: Vec::new(),
                set: cells(&[("v", Value::Array(vec![Value::Array(vec![Value::Int(1)])]))]),
            }],
            false,
        )
        .await;
    assert!(
        matches!(&refused, Err(e) if e.to_string().contains("domain over an array")),
        "{}: nested write: {refused:?}",
        ctx.name
    );
    let stored = rows(a, &format!("SELECT * FROM {SCHEMA}.nested")).await?;
    let original = stored.first().ok_or("nested has no row")?.clone();
    let delete = [RowChange::Delete {
        key: key(&[("id", Value::Int(1))]),
        original,
    }];
    let plan = plan_edits(&ctx.server, &ctx.target("nested"), &delete, &options)?;
    assert!(
        plan.warnings.iter().any(|w| w.contains("not compared")),
        "{}: {:?}",
        ctx.name,
        plan.warnings
    );
    let outcome = apply(a, &ctx.server, &plan).await?;
    assert!(
        matches!(outcome, ApplyOutcome::Applied { .. }),
        "{}: nested delete: {outcome:?}",
        ctx.name
    );
    assert_eq!(ctx.count("nested", "true").await?, 0, "{}", ctx.name);
    let key_info = ctx.key("cik").await?;
    assert_eq!(
        key_info.key_collations,
        vec![Some(KeyCollation {
            schema: "pg_catalog".into(),
            name: "C".into()
        })],
        "{}",
        ctx.name
    );
    let outcome = ctx
        .plan_apply(
            "cik",
            &[RowChange::Update {
                key: key(&[("k", Value::Text("abc".into()))]),
                original: pairs(&[("v", Value::Int(1))]),
                set: cells(&[("v", Value::Int(10))]),
            }],
            true,
        )
        .await?;
    assert!(
        matches!(outcome, ApplyOutcome::Applied { .. }),
        "{}: cik: {outcome:?}",
        ctx.name
    );
    assert_eq!(
        ctx.count("cik", "k = 'ABC' AND v = 2").await?,
        1,
        "{}",
        ctx.name
    );
    assert_eq!(ctx.count("cik", "v = 10").await?, 1, "{}", ctx.name);
    exec(
        a,
        &format!(
            "DROP TABLE {SCHEMA}.arr, {SCHEMA}.xmlt, {SCHEMA}.cik, {SCHEMA}.vec, {SCHEMA}.nested; \
             DROP COLLATION {SCHEMA}.ci; \
             DROP DOMAIN {SCHEMA}.intarr, {SCHEMA}.xdom"
        ),
    )
    .await?;
    Ok(())
}

async fn scenario(name: &'static str, engine: Engine, port: u16) -> TestResult {
    let a = connect(engine, port, false).await?;
    let b = connect(engine, port, false).await?;
    let ctx = Ctx {
        name,
        engine,
        port,
        server: a.server_info().clone(),
        a,
        b,
    };
    setup(&ctx).await?;
    let result = async {
        keys_are_found(&ctx).await?;
        typed_values_round_trip(&ctx).await?;
        composite_and_unique_keys(&ctx).await?;
        concurrent_changes_are_detected(&ctx).await?;
        non_unique_keys_roll_back(&ctx).await?;
        if ctx.pg() {
            postgres_special_values(&ctx).await?;
        }
        read_only_and_open_transactions(&ctx).await
    }
    .await;
    let dropped = teardown(&ctx).await;
    result.and(dropped)
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn postgres_13() -> TestResult {
    scenario(
        "PostgreSQL 13",
        Engine::Postgres,
        port("VARVE_M2_PG13_PORT", 55513),
    )
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn postgres_18() -> TestResult {
    scenario(
        "PostgreSQL 18",
        Engine::Postgres,
        port("VARVE_M2_PG18_PORT", 55518),
    )
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn mysql_8_0() -> TestResult {
    scenario(
        "MySQL 8.0",
        Engine::Mysql,
        port("VARVE_M2_MYSQL80_PORT", 35080),
    )
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn mysql_9_7() -> TestResult {
    scenario(
        "MySQL 9.7",
        Engine::Mysql,
        port("VARVE_M2_MYSQL97_PORT", 35097),
    )
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn mariadb_10_6() -> TestResult {
    scenario(
        "MariaDB 10.6",
        Engine::Mariadb,
        port("VARVE_M2_MARIADB106_PORT", 35106),
    )
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn mariadb_12_3() -> TestResult {
    scenario(
        "MariaDB 12.3",
        Engine::Mariadb,
        port("VARVE_M2_MARIADB123_PORT", 35123),
    )
    .await
}
