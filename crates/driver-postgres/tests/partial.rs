//! Partial output of failed scripts (`DriverError::partial`) against the
//! PostgreSQL servers of `dev/docker-compose.yml`:
//!
//! ```sh
//! docker compose -p varve-m1-pg -f dev/docker-compose.yml up -d --wait postgres-13 postgres-18
//! cargo test -p varve-driver-postgres --test partial -- --include-ignored
//! ```
//!
//! The driver sends every statement of a script as a query of its own, so a
//! statement outside a transaction block commits before the next one runs:
//! a later failure never rolls it back (no implicit transaction around the
//! script).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestResult, connect, params, run, scalar, servers};
use varve_driver::{
    DriverError, Durability, ErrorKind, PartialOutput, Session, StatementResult, Value,
};

use Durability::{Committed, InTransaction, RolledBack};

/// The error of a script that must fail.
async fn fails(session: &dyn Session, sql: &str) -> Result<DriverError, String> {
    match run(session, sql).await {
        Ok(output) => Err(format!("`{sql}` succeeded: {output:?}")),
        Err(error) => Ok(error),
    }
}

fn partial(error: &DriverError) -> Result<&PartialOutput, String> {
    error
        .partial
        .as_deref()
        .ok_or_else(|| format!("no partial output: {error:?}"))
}

fn kinds(partial: &PartialOutput) -> Vec<&'static str> {
    partial
        .results
        .iter()
        .map(|r| match r {
            StatementResult::Rows { .. } => "rows",
            StatementResult::Command { .. } => "command",
        })
        .collect()
}

async fn count(session: &dyn Session, table: &str) -> Result<Value, Box<dyn std::error::Error>> {
    scalar(session, &format!("SELECT count(*) FROM {table}")).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_failing_first_statement_has_no_partial_output() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        let error = fails(session.as_ref(), "SELECT 1/0; SELECT 1").await?;
        assert_eq!(error.code.as_deref(), Some("22012"));
        assert!(error.partial.is_none(), "{error:?}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn earlier_results_and_row_counts_survive_a_failure() -> TestResult {
    for (major, port) in servers() {
        let session = connect(&params(port)).await?;
        let s = session.as_ref();
        run(
            s,
            "DROP TABLE IF EXISTS partial_mid; CREATE TABLE partial_mid (id int)",
        )
        .await?;
        // Comments and an empty statement are not statements.
        let error = fails(
            s,
            "-- first\nSELECT 1 AS a; /* nothing */ ;\nINSERT INTO partial_mid VALUES (1), (2);\n\
             SELECT 1/0; SELECT 2",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2), "PG {major}: {p:?}");
        assert_eq!(kinds(p), ["rows", "command"]);
        assert!(matches!(
            &p.results[1],
            StatementResult::Command { tag, rows_affected: Some(2) } if tag == "INSERT 0 2"
        ));
        assert_eq!(p.completed, [Committed, Committed]);
        // No implicit transaction around the script: the insert stays.
        assert_eq!(count(s, "partial_mid").await?, Value::Int(2));
        assert_eq!(session.in_transaction(), Some(false));

        // The last statement fails.
        let error = fails(s, "SELECT 1; SELECT 2; SELECT 1/0").await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2));
        assert_eq!(kinds(p), ["rows", "rows"]);
        run(s, "DROP TABLE partial_mid").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn statements_are_counted_like_the_splitter_counts_them() -> TestResult {
    for (major, port) in servers() {
        let session = connect(&params(port)).await?;
        let s = session.as_ref();
        let atomic = if major >= 14 {
            "CREATE OR REPLACE FUNCTION partial_atomic() RETURNS int LANGUAGE sql \
             BEGIN ATOMIC SELECT 1; SELECT 2; END;\n"
        } else {
            "CREATE OR REPLACE FUNCTION partial_atomic() RETURNS int LANGUAGE sql \
             AS 'SELECT 1; SELECT 2';\n"
        };
        let script = format!(
            "{atomic}CREATE OR REPLACE FUNCTION partial_dollar() RETURNS text LANGUAGE plpgsql \
             AS $body$ BEGIN RETURN 'a;b'; END $body$;\n\
             DO $$ BEGIN PERFORM 1; END $$;\n\
             SELECT 'x;y' AS \"semi;colon\", partial_dollar();\n\
             SELECT 1/0;\n\
             SELECT 3"
        );
        let error = fails(s, &script).await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(4), "PG {major}: {p:?}");
        assert_eq!(kinds(p), ["command", "command", "command", "rows"]);
        // A DO block outside a transaction block may commit inside itself.
        assert_eq!(
            p.completed,
            [Committed, Committed, Durability::Unknown, Committed]
        );
        run(
            s,
            "DROP FUNCTION partial_atomic(); DROP FUNCTION partial_dollar()",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn statements_in_an_open_transaction_are_reported_as_open() -> TestResult {
    for (major, port) in servers() {
        let session = connect(&params(port)).await?;
        let s = session.as_ref();
        run(
            s,
            "DROP TABLE IF EXISTS partial_tx; CREATE TABLE partial_tx (id int)",
        )
        .await?;

        // The failure aborts the block: nothing is committed.
        let error = fails(
            s,
            "BEGIN; INSERT INTO partial_tx VALUES (1); SELECT 1/0; INSERT INTO partial_tx VALUES (2)",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2), "PG {major}: {p:?}");
        assert_eq!(p.completed, [InTransaction, InTransaction]);
        assert_eq!(session.in_transaction(), Some(true));
        run(s, "ROLLBACK").await?;
        assert_eq!(count(s, "partial_tx").await?, Value::Int(0));

        // A COMMIT before the failure commits what came before it.
        let error = fails(
            s,
            "BEGIN; INSERT INTO partial_tx VALUES (1); COMMIT; \
             BEGIN; INSERT INTO partial_tx VALUES (2); SAVEPOINT s; \
             INSERT INTO partial_tx VALUES (3); ROLLBACK TO SAVEPOINT s; \
             INSERT INTO partial_tx VALUES (4); SELECT 1/0",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(9));
        assert_eq!(
            p.completed,
            [
                Committed,
                Committed,
                Committed,
                InTransaction,
                InTransaction,
                InTransaction,
                RolledBack,
                InTransaction,
                InTransaction
            ]
        );
        run(s, "ROLLBACK").await?;
        assert_eq!(count(s, "partial_tx").await?, Value::Int(1));

        // A COMMIT that fails (a deferred constraint) rolls back.
        run(
            s,
            "DROP TABLE IF EXISTS partial_child; DROP TABLE IF EXISTS partial_parent; \
             CREATE TABLE partial_parent (id int PRIMARY KEY); \
             CREATE TABLE partial_child (parent int REFERENCES partial_parent \
             DEFERRABLE INITIALLY DEFERRED)",
        )
        .await?;
        let error = fails(
            s,
            "BEGIN; INSERT INTO partial_child VALUES (42); COMMIT; SELECT 1",
        )
        .await?;
        assert_eq!(error.code.as_deref(), Some("23503"));
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2));
        assert_eq!(p.completed, [RolledBack, RolledBack]);
        assert_eq!(session.in_transaction(), Some(false));
        assert_eq!(count(s, "partial_child").await?, Value::Int(0));
        run(
            s,
            "DROP TABLE partial_child; DROP TABLE partial_parent; DROP TABLE partial_tx",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn server_notices_before_the_failure_are_kept() -> TestResult {
    for (_, port) in servers() {
        let session = connect(&params(port)).await?;
        let error = fails(
            session.as_ref(),
            "DROP TABLE IF EXISTS partial_never_there; SELECT 1/0",
        )
        .await?;
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(1));
        assert!(
            p.notices
                .iter()
                .any(|n| n.message.contains("partial_never_there")),
            "{p:?}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_cancel_mid_script_keeps_what_ran() -> TestResult {
    for (major, port) in servers() {
        let session: Arc<dyn Session> = Arc::from(connect(&params(port)).await?);
        run(
            session.as_ref(),
            "DROP TABLE IF EXISTS partial_cancel; CREATE TABLE partial_cancel (id int)",
        )
        .await?;
        let running = Arc::clone(&session);
        let task = tokio::spawn(async move {
            run(
                running.as_ref(),
                "SELECT 1; INSERT INTO partial_cancel VALUES (1); SELECT pg_sleep(30); SELECT 2",
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        session.cancel_handle().cancel().await?;
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await??
            .err()
            .ok_or("the script finished")?;
        assert_eq!(error.kind, ErrorKind::Cancelled, "PG {major}: {error:?}");
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(2));
        assert_eq!(kinds(p), ["rows", "command"]);
        assert_eq!(p.completed, [Committed, Committed]);
        assert_eq!(
            count(session.as_ref(), "partial_cancel").await?,
            Value::Int(1)
        );
        run(session.as_ref(), "DROP TABLE partial_cancel").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix"]
async fn a_statement_timeout_mid_script_keeps_what_ran() -> TestResult {
    for (major, port) in servers() {
        let mut params = params(port);
        params.statement_timeout = Some(Duration::from_millis(300));
        let session = connect(&params).await?;
        let error = fails(session.as_ref(), "SELECT 1; SELECT pg_sleep(5); SELECT 2").await?;
        assert_eq!(error.kind, ErrorKind::Timeout, "PG {major}: {error:?}");
        let p = partial(&error)?;
        assert_eq!(p.failed_statement, Some(1));
        assert_eq!(kinds(p), ["rows"]);
        assert_eq!(p.completed, [Committed]);
    }
    Ok(())
}
