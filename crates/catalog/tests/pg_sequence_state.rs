//! The state of a sequence as `describe` reports it, against the shared M2
//! database matrix (PostgreSQL 13 and 18):
//!
//! ```sh
//! cargo test -p varve-catalog --test pg_sequence_state -- --include-ignored
//! ```
//!
//! `pg_sequences.last_value` is NULL until `is_called`, which is also the
//! state after `RESTART [WITH n]` and `setval(.., false)`: the sequence
//! editor must still know the value the next `nextval` returns. Objects are
//! named `m2_uiseq*` and dropped at the start and the end of a run.

use std::error::Error;

use varve_catalog::catalog_for;
use varve_catalog::model::{ObjectDetails, ObjectKind, ObjectRef};
use varve_catalog::postgres::PgObject;
use varve_catalog::postgres::relations::{PgRelationObject, PgSequence};
use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, ExecOptions, Session};
use varve_driver_postgres::PostgresDriver;
use varve_secrets::Secret;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const SCHEMA: &str = "m2_uiseq";

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

async fn connect(port: u16) -> TestResult<Box<dyn Session>> {
    let mut params = ConnectParams::new(
        Engine::Postgres,
        env_or("VARVE_M2_PG_HOST", "127.0.0.1"),
        port,
    );
    params.database = Some("varve".to_owned());
    params.username = Some(env_or("VARVE_M2_PG_USER", "varve"));
    params.password = Some(Secret::new(env_or("VARVE_M2_PG_PASSWORD", "varve_dev")));
    params.application_name = "varve-catalog pg_sequence_state tests".into();
    Ok(PostgresDriver::new().connect(&params).await?)
}

async fn exec(session: &dyn Session, sql: &str) -> TestResult {
    session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}\n=> {e}"))?;
    Ok(())
}

async fn describe(session: &dyn Session) -> TestResult<PgSequence> {
    let catalog = catalog_for(session.server_info())?;
    let object = ObjectRef {
        kind: ObjectKind::Sequence,
        database: Some("varve".into()),
        schema: Some(SCHEMA.into()),
        parent: None,
        name: "Seq \"One\"".into(),
        signature: None,
    };
    match catalog.describe(session, &object).await? {
        ObjectDetails::Postgres(object) => match *object {
            PgObject::Relation(PgRelationObject::Sequence(seq)) => Ok(seq),
            other => Err(format!("not a sequence: {other:?}").into()),
        },
        other => Err(format!("not PostgreSQL: {other:?}").into()),
    }
}

async fn run(session: &dyn Session) -> TestResult {
    let seq = format!("{SCHEMA}.\"Seq \"\"One\"\"\"");
    exec(
        session,
        &format!("CREATE SEQUENCE {seq} START 10 INCREMENT 5"),
    )
    .await?;
    let state = |s: &PgSequence| (s.last_value, s.is_called);

    // New: the next nextval returns the start.
    assert_eq!(state(&describe(session).await?), (Some(10), Some(false)));
    exec(session, &format!("SELECT nextval('{SCHEMA}.\"Seq \"\"One\"\"\"'), nextval('{SCHEMA}.\"Seq \"\"One\"\"\"')")).await?;
    assert_eq!(state(&describe(session).await?), (Some(15), Some(true)));
    // RESTART WITH: the next nextval returns 500.
    exec(session, &format!("ALTER SEQUENCE {seq} RESTART WITH 500")).await?;
    assert_eq!(state(&describe(session).await?), (Some(500), Some(false)));
    // setval(.., false): the next nextval returns 42; setval(.., true): 42 was handed out.
    exec(
        session,
        &format!("SELECT setval('{SCHEMA}.\"Seq \"\"One\"\"\"', 42, false)"),
    )
    .await?;
    assert_eq!(state(&describe(session).await?), (Some(42), Some(false)));
    exec(
        session,
        &format!("SELECT setval('{SCHEMA}.\"Seq \"\"One\"\"\"', 42, true)"),
    )
    .await?;
    assert_eq!(state(&describe(session).await?), (Some(42), Some(true)));
    Ok(())
}

async fn check(major: u32, port: u16) -> TestResult {
    let session = connect(port).await?;
    assert_eq!(session.server_info().major, major);
    exec(
        session.as_ref(),
        &format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA}"),
    )
    .await?;
    let result = run(session.as_ref()).await;
    let cleaned = exec(
        session.as_ref(),
        &format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"),
    )
    .await;
    result?;
    cleaned
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn sequence_state_after_restart_and_setval_on_postgres_13() -> TestResult {
    check(13, port("VARVE_M2_PG13_PORT", 55513)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn sequence_state_after_restart_and_setval_on_postgres_18() -> TestResult {
    check(18, port("VARVE_M2_PG18_PORT", 55518)).await
}
