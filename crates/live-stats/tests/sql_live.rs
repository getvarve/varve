//! Integration tests for the SQL live server panels against the M2 database
//! matrix (PostgreSQL 13 and 18, MySQL 8.0 and 9.7, MariaDB 10.6 and 12.3).
//!
//! Run with `cargo test -p varve-live-stats --test sql_live -- --include-ignored --test-threads=1`.
//!
//! Endpoints come from environment variables: `VARVE_M2_HOST` (default
//! `127.0.0.1`), `VARVE_M2_PG13_PORT` (55513), `VARVE_M2_PG18_PORT` (55518),
//! `VARVE_M2_MY80_PORT` (35080), `VARVE_M2_MY84_PORT` (35084, scope table only),
//! `VARVE_M2_MY97_PORT` (35097),
//! `VARVE_M2_MA106_PORT` (35106), `VARVE_M2_MA123_PORT` (35123),
//! `VARVE_M2_PG_PRIMARY_PORT` (55611) and `VARVE_M2_PG_REPLICA_PORT` (55612)
//! for the PostgreSQL 18 streaming pair, `VARVE_M2_PASSWORD` (`varve_dev`).
//! `VARVE_ONLY_SERVERS` (compose service names such as `postgres-18,mysql-9.7`)
//! limits the process and variable tests to those servers.
//!
//! Every global change is undone at the end of a test and also reset at its
//! start, in case an earlier run failed half way.

use std::error::Error;
use std::time::{Duration, Instant};

use varve_core::Engine;
use varve_driver::{ConnectParams, Driver, ExecOptions, ServerInfo, Session, TlsMode};
use varve_driver_mysql::MysqlDriver;
use varve_driver_postgres::PostgresDriver;
use varve_live_stats::sql::{
    EditScope, KillTarget, LiveError, ReplicationRole, VariableChange, VariableType, kill,
    plan_variable_change, process_list, run_edit_plan, server_status, server_variable,
    server_variables, variable_edit_plan,
};
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn Error>>;

#[derive(Debug, Clone, Copy)]
struct Target {
    name: &'static str,
    engine: Engine,
    port: u16,
}

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Whether the server with `dev/docker-compose.yml` service name `service`
/// (for example `mysql-9.7`) is tested. `VARVE_ONLY_SERVERS`, a comma
/// separated list of service names, limits a run to those servers (CI tests
/// only the newest version of each engine); without it every server is tested.
fn selected(service: &str) -> bool {
    std::env::var("VARVE_ONLY_SERVERS")
        .ok()
        .is_none_or(|list| list.split(',').any(|name| name.trim() == service))
}

/// `targets` without the servers [`selected`] leaves out, never none.
fn only_selected(targets: Vec<Target>) -> Vec<Target> {
    let targets: Vec<Target> = targets
        .into_iter()
        .filter(|target| {
            let service = target
                .name
                .to_lowercase()
                .replace("postgresql", "postgres")
                .replace(' ', "-");
            selected(&service)
        })
        .collect();
    assert!(
        !targets.is_empty(),
        "VARVE_ONLY_SERVERS selects none of these servers"
    );
    targets
}

/// PostgreSQL 13 and 18 (see [`selected`]).
fn pg_targets() -> Vec<Target> {
    only_selected(vec![
        Target {
            name: "PostgreSQL 13",
            engine: Engine::Postgres,
            port: port("VARVE_M2_PG13_PORT", 55513),
        },
        Target {
            name: "PostgreSQL 18",
            engine: Engine::Postgres,
            port: port("VARVE_M2_PG18_PORT", 55518),
        },
    ])
}

/// MySQL 8.0 and 9.7, MariaDB 10.6 and 12.3 (see [`selected`]).
fn my_targets() -> Vec<Target> {
    only_selected(vec![
        Target {
            name: "MySQL 8.0",
            engine: Engine::Mysql,
            port: port("VARVE_M2_MY80_PORT", 35080),
        },
        Target {
            name: "MySQL 9.7",
            engine: Engine::Mysql,
            port: port("VARVE_M2_MY97_PORT", 35097),
        },
        Target {
            name: "MariaDB 10.6",
            engine: Engine::Mariadb,
            port: port("VARVE_M2_MA106_PORT", 35106),
        },
        Target {
            name: "MariaDB 12.3",
            engine: Engine::Mariadb,
            port: port("VARVE_M2_MA123_PORT", 35123),
        },
    ])
}

fn password() -> String {
    std::env::var("VARVE_M2_PASSWORD").unwrap_or_else(|_| "varve_dev".into())
}

async fn connect_as(
    target: Target,
    user: &str,
    pass: &str,
) -> Result<Box<dyn Session>, Box<dyn Error>> {
    let host = std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let mut params = ConnectParams::new(target.engine, host, target.port);
    params.username = Some(user.into());
    params.password = Some(Secret::new(pass.into()));
    params.database = Some("varve".into());
    params.tls.mode = TlsMode::Disable;
    params.application_name = "m2_live tests".into();
    let session = match target.engine {
        Engine::Postgres => PostgresDriver::new().connect(&params).await,
        _ => MysqlDriver::new().connect(&params).await,
    };
    session.map_err(|e| format!("{}: connect as {user}: {e}", target.name).into())
}

/// Superuser (PostgreSQL `varve`) or `root` (MySQL, MariaDB).
async fn connect(target: Target) -> Result<Box<dyn Session>, Box<dyn Error>> {
    let user = if target.engine == Engine::Postgres {
        "varve"
    } else {
        "root"
    };
    connect_as(target, user, &password()).await
}

async fn exec(session: &dyn Session, sql: &str) -> TestResult {
    session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    Ok(())
}

fn on(target: Target, error: impl std::fmt::Display) -> Box<dyn Error> {
    format!("{}: {error}", target.name).into()
}

/// The sleep statement and the text to find it by in the list.
fn sleep_sql(engine: Engine) -> &'static str {
    if engine == Engine::Postgres {
        "SELECT pg_sleep(30) /* m2_live_sleep */, 'host=h password=hunter2' AS dsn"
    } else {
        "SELECT SLEEP(30) /* m2_live_sleep */, 'host=h password=hunter2' AS dsn"
    }
}

/// Starts a 30 second sleep on `session` and waits until the list shows it.
async fn start_sleep(
    target: Target,
    main: &dyn Session,
    info: &ServerInfo,
    session: Box<dyn Session>,
    id: i64,
) -> Result<tokio::task::JoinHandle<(Result<(), String>, Box<dyn Session>, Duration)>, Box<dyn Error>>
{
    let sql = sleep_sql(target.engine);
    let handle = tokio::spawn(async move {
        let started = Instant::now();
        let result = session
            .execute(sql, &ExecOptions::default())
            .await
            .map(|_| ())
            .map_err(|e| format!("{:?}: {e}", e.kind));
        (result, session, started.elapsed())
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = process_list(main, info).await.map_err(|e| on(target, e))?;
        // After a cancel PostgreSQL keeps the last statement text on the
        // idle backend, so only an active backend is the new sleep.
        if let Some(p) = list.processes.iter().find(|p| p.id == id)
            && p.query
                .as_deref()
                .is_some_and(|q| q.contains("m2_live_sleep"))
            && (target.engine != Engine::Postgres || p.state.as_deref() == Some("active"))
        {
            assert!(
                !p.own,
                "{}: the sleeper is not the own session",
                target.name
            );
            let text = p.query.as_deref().unwrap_or_default();
            assert!(
                !text.contains("hunter2"),
                "{}: statement text is redacted: {text}",
                target.name
            );
            if target.engine == Engine::Postgres {
                assert_eq!(p.state.as_deref(), Some("active"), "{}", target.name);
                assert_eq!(p.backend_type.as_deref(), Some("client backend"));
            } else {
                assert_eq!(p.command.as_deref(), Some("Query"), "{}", target.name);
            }
            assert!(p.duration_ms.is_some(), "{}: duration", target.name);
            return Ok(handle);
        }
        if handle.is_finished() {
            let (result, _, elapsed) = handle.await?;
            return Err(on(
                target,
                format!("the sleep ended before it showed in the list ({elapsed:?}): {result:?}"),
            ));
        }
        if Instant::now() > deadline {
            return Err(on(target, "the sleeping session never showed in the list"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn process_round_trip(target: Target) -> TestResult {
    let main = connect(target).await?;
    let info = main.server_info().clone();
    let list = process_list(main.as_ref(), &info)
        .await
        .map_err(|e| on(target, e))?;
    let own = list
        .own_id
        .ok_or_else(|| on(target, "own connection not marked"))?;
    assert_eq!(list.processes.iter().filter(|p| p.own).count(), 1);

    // The own connection is refused.
    let refused = kill(main.as_ref(), &info, own, KillTarget::Connection).await;
    assert!(
        matches!(refused, Err(LiveError::OwnConnection { id }) if id == own),
        "{}: {refused:?}",
        target.name
    );

    // Cancel: the statement stops, the connection stays.
    let second = connect(target).await?;
    let second_id = varve_live_stats::sql::own_connection_id(second.as_ref(), &info)
        .await
        .map_err(|e| on(target, e))?;
    let handle = start_sleep(target, main.as_ref(), &info, second, second_id).await?;
    let outcome = kill(main.as_ref(), &info, second_id, KillTarget::Query)
        .await
        .map_err(|e| on(target, e))?;
    assert!(outcome.signalled, "{}: {outcome:?}", target.name);
    let (result, second, elapsed) = handle.await?;
    assert!(
        elapsed < Duration::from_secs(20),
        "{}: cancel did not stop the sleep ({elapsed:?}, {result:?})",
        target.name
    );
    if target.engine == Engine::Postgres {
        assert!(
            result.is_err(),
            "{}: a cancelled pg_sleep fails",
            target.name
        );
    }
    second
        .ping()
        .await
        .map_err(|e| on(target, format!("still connected: {e}")))?;

    // Terminate: the connection goes away.
    let handle = start_sleep(target, main.as_ref(), &info, second, second_id).await?;
    let outcome = kill(main.as_ref(), &info, second_id, KillTarget::Connection)
        .await
        .map_err(|e| on(target, e))?;
    assert!(outcome.signalled, "{}: {outcome:?}", target.name);
    let (result, second, elapsed) = handle.await?;
    assert!(elapsed < Duration::from_secs(20), "{}", target.name);
    assert!(
        result.is_err(),
        "{}: a terminated session fails",
        target.name
    );
    assert!(
        second.ping().await.is_err(),
        "{}: disconnected",
        target.name
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = process_list(main.as_ref(), &info)
            .await
            .map_err(|e| on(target, e))?;
        if !list.processes.iter().any(|p| p.id == second_id) {
            break;
        }
        if Instant::now() > deadline {
            return Err(on(target, "the terminated connection stayed in the list"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // A connection that does not exist is reported as not signalled.
    let missing = kill(
        main.as_ref(),
        &info,
        i64::from(i32::MAX) - 7,
        KillTarget::Query,
    )
    .await
    .map_err(|e| on(target, e))?;
    assert!(!missing.signalled, "{}: {missing:?}", target.name);
    assert!(missing.message.is_some());
    main.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn postgres_process_list_cancel_and_terminate() -> TestResult {
    for target in pg_targets() {
        process_round_trip(target).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn mysql_process_list_cancel_and_terminate() -> TestResult {
    for target in my_targets() {
        process_round_trip(target).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn mysql_unprivileged_users_see_themselves_and_cannot_kill_others() -> TestResult {
    for target in my_targets() {
        let root = connect(target).await?;
        let info = root.server_info().clone();
        let root_id = varve_live_stats::sql::own_connection_id(root.as_ref(), &info)
            .await
            .map_err(|e| on(target, e))?;
        let low = connect_as(target, "varve", &password()).await?;
        let list = process_list(low.as_ref(), &info)
            .await
            .map_err(|e| on(target, e))?;
        assert!(list.own_id.is_some(), "{}: {}", target.name, list.source);
        let refused = kill(low.as_ref(), &info, root_id, KillTarget::Query).await;
        assert!(
            matches!(refused, Err(LiveError::PermissionDenied { .. })),
            "{}: {refused:?}",
            target.name
        );
        let plan = plan_variable_change(
            low.as_ref(),
            &info,
            "max_connect_errors",
            &VariableChange::Set {
                scope: EditScope::Global,
                value: "100".into(),
            },
        )
        .await
        .map_err(|e| on(target, e))?;
        let denied = run_edit_plan(low.as_ref(), &plan).await;
        assert!(
            matches!(denied, Err(LiveError::PermissionDenied { .. })),
            "{}: {denied:?}",
            target.name
        );
        low.close().await?;
        root.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn postgres_permission_errors_are_typed() -> TestResult {
    for target in pg_targets() {
        let main = connect(target).await?;
        let info = main.server_info().clone();
        exec(main.as_ref(), "DROP ROLE IF EXISTS m2_live_low").await?;
        exec(
            main.as_ref(),
            "CREATE ROLE m2_live_low LOGIN PASSWORD 'm2_live_pw'",
        )
        .await?;
        let result = async {
            let main_id = varve_live_stats::sql::own_connection_id(main.as_ref(), &info)
                .await
                .map_err(|e| on(target, e))?;
            let low = connect_as(target, "m2_live_low", "m2_live_pw").await?;
            let refused = kill(low.as_ref(), &info, main_id, KillTarget::Connection).await;
            assert!(
                matches!(refused, Err(LiveError::PermissionDenied { ref code, .. }) if code.as_deref() == Some("42501")),
                "{}: {refused:?}",
                target.name
            );
            let plan = plan_variable_change(
                low.as_ref(),
                &info,
                "log_min_duration_statement",
                &VariableChange::Set {
                    scope: EditScope::Global,
                    value: "1s".into(),
                },
            )
            .await
            .map_err(|e| on(target, e))?;
            let denied = run_edit_plan(low.as_ref(), &plan).await;
            assert!(
                matches!(denied, Err(LiveError::PermissionDenied { .. })),
                "{}: {denied:?}",
                target.name
            );
            low.close().await?;
            Ok::<(), Box<dyn Error>>(())
        }
        .await;
        exec(main.as_ref(), "DROP ROLE IF EXISTS m2_live_low").await?;
        result?;
        main.close().await?;
    }
    Ok(())
}

async fn value_of(
    session: &dyn Session,
    info: &ServerInfo,
    name: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    Ok(server_variable(session, info, name).await?.value)
}

/// Polls until the variable has `expected` (a configuration reload is
/// asynchronous).
async fn wait_for(
    session: &dyn Session,
    info: &ServerInfo,
    name: &str,
    expected: &str,
) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let value = value_of(session, info, name).await?;
        if value.as_deref() == Some(expected) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("{name} is {value:?}, expected {expected}").into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn apply(
    session: &dyn Session,
    info: &ServerInfo,
    name: &str,
    change: VariableChange,
) -> TestResult {
    let plan = plan_variable_change(session, info, name, &change).await?;
    run_edit_plan(session, &plan).await?;
    Ok(())
}

fn set(scope: EditScope, value: &str) -> VariableChange {
    VariableChange::Set {
        scope,
        value: value.into(),
    }
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn postgres_variables_round_trip() -> TestResult {
    for target in pg_targets() {
        let s = connect(target).await?;
        let info = s.server_info().clone();
        let session = s.as_ref();
        // Clean up after an earlier failed run.
        exec(session, "ALTER SYSTEM RESET log_min_duration_statement").await?;
        exec(session, "SELECT pg_reload_conf()").await?;

        let all = server_variables(session, &info)
            .await
            .map_err(|e| on(target, e))?;
        let work_mem = all
            .iter()
            .find(|v| v.name == "work_mem")
            .ok_or_else(|| on(target, "work_mem missing"))?;
        assert_eq!(work_mem.unit.as_deref(), Some("kB"));
        assert_eq!(work_mem.var_type, VariableType::Integer);
        assert_eq!(
            work_mem.settable_scopes,
            vec![EditScope::Session, EditScope::Global]
        );
        let shared = all
            .iter()
            .find(|v| v.name == "shared_buffers")
            .ok_or_else(|| on(target, "shared_buffers missing"))?;
        assert!(shared.restart_required);
        let restart_plan = variable_edit_plan(&info, shared, &set(EditScope::Global, "256MB"))?;
        assert!(restart_plan.warnings.iter().any(|w| w.contains("restart")));
        assert!(
            all.iter()
                .any(|v| v.var_type == VariableType::Enum && !v.enum_values.is_empty())
        );
        assert!(
            all.iter()
                .any(|v| v.read_only && v.settable_scopes.is_empty())
        );

        // Session scope.
        let original = value_of(session, &info, "work_mem").await?;
        apply(session, &info, "work_mem", set(EditScope::Session, "8MB")).await?;
        assert_eq!(
            value_of(session, &info, "WORK_MEM").await?.as_deref(),
            Some("8192")
        );
        apply(
            session,
            &info,
            "work_mem",
            VariableChange::Reset {
                scope: EditScope::Session,
            },
        )
        .await?;
        assert_eq!(value_of(session, &info, "work_mem").await?, original);

        // A list setting with a quoted element.
        apply(
            session,
            &info,
            "search_path",
            set(EditScope::Session, r#""M2 Live", public"#),
        )
        .await?;
        assert_eq!(
            value_of(session, &info, "search_path").await?.as_deref(),
            Some(r#""M2 Live", public"#)
        );
        apply(
            session,
            &info,
            "search_path",
            VariableChange::Reset {
                scope: EditScope::Session,
            },
        )
        .await?;

        // Server scope through ALTER SYSTEM and a reload.
        let original = value_of(session, &info, "log_min_duration_statement")
            .await?
            .ok_or_else(|| on(target, "no value"))?;
        let result = async {
            apply(
                session,
                &info,
                "log_min_duration_statement",
                set(EditScope::Global, "250ms"),
            )
            .await?;
            wait_for(session, &info, "log_min_duration_statement", "250").await?;
            let other = connect(target).await?;
            assert_eq!(
                value_of(other.as_ref(), &info, "log_min_duration_statement")
                    .await?
                    .as_deref(),
                Some("250")
            );
            other.close().await?;
            Ok::<(), Box<dyn Error>>(())
        }
        .await;
        apply(
            session,
            &info,
            "log_min_duration_statement",
            VariableChange::Reset {
                scope: EditScope::Global,
            },
        )
        .await?;
        wait_for(session, &info, "log_min_duration_statement", &original).await?;
        result.map_err(|e| on(target, e))?;

        // A setting holding a connection string never shows its password.
        exec(
            session,
            "SET application_name = 'host=primary user=rep password=hunter2'",
        )
        .await?;
        let dsn = server_variable(session, &info, "application_name").await?;
        assert!(dsn.masked, "{}: {dsn:?}", target.name);
        for value in [&dsn.value, &dsn.session_value] {
            let value = value.as_deref().unwrap_or_default();
            assert!(!value.contains("hunter2"), "{}: {value}", target.name);
            assert!(value.contains("host=primary"), "{}: {value}", target.name);
        }
        exec(session, "RESET application_name").await?;

        // Invalid values never reach the server.
        let refused =
            plan_variable_change(session, &info, "work_mem", &set(EditScope::Session, "8 mb"))
                .await;
        assert!(
            matches!(refused, Err(LiveError::Invalid { .. })),
            "{refused:?}"
        );
        let missing = server_variable(session, &info, "m2_live_no_such_setting").await;
        assert!(matches!(missing, Err(LiveError::NotFound { .. })));
        s.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn mysql_variables_round_trip() -> TestResult {
    for target in my_targets() {
        let s = connect(target).await?;
        let info = s.server_info().clone();
        let session = s.as_ref();
        let mysql = target.engine == Engine::Mysql;
        // Clean up after an earlier failed run.
        if mysql {
            exec(session, "RESET PERSIST IF EXISTS max_connect_errors").await?;
        }
        exec(session, "SET GLOBAL max_connect_errors = DEFAULT").await?;

        let all = server_variables(session, &info)
            .await
            .map_err(|e| on(target, e))?;
        assert!(all.len() > 300, "{}: {} variables", target.name, all.len());
        let max_conn = all
            .iter()
            .find(|v| v.name == "max_connections")
            .ok_or_else(|| on(target, "max_connections missing"))?;
        assert_eq!(max_conn.var_type, VariableType::Integer, "{}", target.name);
        assert!(
            !max_conn.settable_scopes.contains(&EditScope::Session),
            "{}: {:?}",
            target.name,
            max_conn.settable_scopes
        );
        let sql_mode = all
            .iter()
            .find(|v| v.name == "sql_mode")
            .ok_or_else(|| on(target, "sql_mode missing"))?;
        assert!(sql_mode.settable_scopes.contains(&EditScope::Session));
        let version = all
            .iter()
            .find(|v| v.name == "version")
            .ok_or_else(|| on(target, "version missing"))?;
        assert!(version.read_only, "{}: {version:?}", target.name);
        assert!(version.settable_scopes.is_empty(), "{}", target.name);
        let refused = variable_edit_plan(&info, version, &set(EditScope::Global, "1"));
        assert!(
            matches!(refused, Err(LiveError::Invalid { .. })),
            "{refused:?}"
        );
        if mysql {
            let instances = all
                .iter()
                .find(|v| v.name == "innodb_buffer_pool_instances")
                .ok_or_else(|| on(target, "innodb_buffer_pool_instances missing"))?;
            assert_eq!(instances.settable_scopes, vec![EditScope::PersistOnly]);
            assert!(instances.restart_required);
        }
        if target.engine == Engine::Mariadb {
            assert_eq!(sql_mode.var_type, VariableType::Set);
            assert!(!sql_mode.enum_values.is_empty());
            let datadir = all
                .iter()
                .find(|v| v.name == "datadir")
                .ok_or_else(|| on(target, "datadir missing"))?;
            assert!(datadir.read_only);
        }

        // Session scope: integer, real and set variables.
        apply(
            session,
            &info,
            "sort_buffer_size",
            set(EditScope::Session, "300000"),
        )
        .await?;
        assert_eq!(
            server_variable(session, &info, "sort_buffer_size")
                .await?
                .session_value
                .as_deref(),
            Some("300000"),
            "{}",
            target.name
        );
        apply(
            session,
            &info,
            "sort_buffer_size",
            VariableChange::Reset {
                scope: EditScope::Session,
            },
        )
        .await?;
        let sort = server_variable(session, &info, "sort_buffer_size").await?;
        assert_eq!(sort.session_value, sort.global_value, "{}", target.name);
        apply(
            session,
            &info,
            "long_query_time",
            set(EditScope::Session, "2.5"),
        )
        .await?;
        assert_eq!(
            server_variable(session, &info, "long_query_time")
                .await?
                .session_value
                .as_deref(),
            Some("2.500000")
        );
        apply(
            session,
            &info,
            "long_query_time",
            VariableChange::Reset {
                scope: EditScope::Session,
            },
        )
        .await?;
        // MySQL 8.0 has no type metadata: sql_mode is inferred as a string and
        // passed verbatim; elsewhere it is a set whose elements are validated
        // and normalized.
        let mode = if sql_mode.type_inferred {
            assert_eq!(sql_mode.var_type, VariableType::String, "{}", target.name);
            "ansi_quotes,strict_all_tables"
        } else {
            assert_eq!(sql_mode.var_type, VariableType::Set, "{}", target.name);
            let bogus = plan_variable_change(
                session,
                &info,
                "sql_mode",
                &set(EditScope::Session, "ansi_quotes, no_such_mode"),
            )
            .await;
            if target.engine == Engine::Mariadb {
                assert!(matches!(bogus, Err(LiveError::Invalid { .. })), "{bogus:?}");
            }
            "ansi_quotes, strict_all_tables"
        };
        apply(session, &info, "sql_mode", set(EditScope::Session, mode)).await?;
        assert_eq!(
            server_variable(session, &info, "sql_mode")
                .await?
                .session_value
                .as_deref(),
            Some("ANSI_QUOTES,STRICT_ALL_TABLES"),
            "{}",
            target.name
        );
        apply(
            session,
            &info,
            "sql_mode",
            VariableChange::Reset {
                scope: EditScope::Session,
            },
        )
        .await?;

        // SQL held in a variable is shown redacted. init_connect runs for new
        // non-admin sessions; the user variable it sets is harmless and the
        // original value is restored right away.
        let original_init = text_rows(session, "SELECT @@GLOBAL.init_connect")
            .await?
            .first()
            .and_then(|row| row.first().cloned().flatten())
            .unwrap_or_default();
        exec(
            session,
            "SET GLOBAL init_connect = 'SET @m2_live_dsn = ''mysql://app:m2_SECRET@db/x'''",
        )
        .await?;
        let init = server_variable(session, &info, "init_connect").await;
        let restore = varve_catalog::sql::string(varve_driver::Dialect::Mysql, &original_init)?;
        exec(session, &format!("SET GLOBAL init_connect = {restore}")).await?;
        let init = init?;
        assert!(init.masked, "{}: {init:?}", target.name);
        let shown = init.global_value.clone().unwrap_or_default();
        assert!(
            shown.contains("mysql://app:") && !shown.contains("m2_SECRET"),
            "{}: {shown}",
            target.name
        );
        let refused = variable_edit_plan(&info, &init, &set(EditScope::Global, &shown));
        assert!(
            matches!(refused, Err(LiveError::Invalid { .. })),
            "{refused:?}"
        );

        // Global scope, always reset afterwards.
        let result = async {
            apply(
                session,
                &info,
                "max_connect_errors",
                set(EditScope::Global, "4242"),
            )
            .await?;
            assert_eq!(
                server_variable(session, &info, "max_connect_errors")
                    .await?
                    .global_value
                    .as_deref(),
                Some("4242")
            );
            if mysql {
                apply(
                    session,
                    &info,
                    "max_connect_errors",
                    set(EditScope::Persist, "4343"),
                )
                .await?;
                let v = server_variable(session, &info, "max_connect_errors").await?;
                assert_eq!(v.global_value.as_deref(), Some("4343"));
                assert_eq!(v.persisted_value.as_deref(), Some("4343"));
                apply(
                    session,
                    &info,
                    "max_connect_errors",
                    VariableChange::Reset {
                        scope: EditScope::Persist,
                    },
                )
                .await?;
                assert_eq!(
                    server_variable(session, &info, "max_connect_errors")
                        .await?
                        .persisted_value,
                    None
                );
                apply(
                    session,
                    &info,
                    "max_connect_errors",
                    set(EditScope::PersistOnly, "4444"),
                )
                .await?;
                let v = server_variable(session, &info, "max_connect_errors").await?;
                assert_eq!(
                    v.global_value.as_deref(),
                    Some("4343"),
                    "persist only leaves the running value"
                );
                assert_eq!(v.persisted_value.as_deref(), Some("4444"));
                apply(
                    session,
                    &info,
                    "max_connect_errors",
                    VariableChange::Reset {
                        scope: EditScope::PersistOnly,
                    },
                )
                .await?;
                assert_eq!(
                    server_variable(session, &info, "max_connect_errors")
                        .await?
                        .persisted_value,
                    None
                );
            } else {
                let refused = plan_variable_change(
                    session,
                    &info,
                    "max_connect_errors",
                    &set(EditScope::Persist, "1"),
                )
                .await;
                assert!(
                    matches!(refused, Err(LiveError::Unsupported { .. })),
                    "{refused:?}"
                );
            }
            Ok::<(), Box<dyn Error>>(())
        }
        .await;
        if mysql {
            exec(session, "RESET PERSIST IF EXISTS max_connect_errors").await?;
        }
        apply(
            session,
            &info,
            "max_connect_errors",
            VariableChange::Reset {
                scope: EditScope::Global,
            },
        )
        .await?;
        assert_eq!(
            server_variable(session, &info, "max_connect_errors")
                .await?
                .global_value
                .as_deref(),
            Some("100"),
            "{}",
            target.name
        );
        result.map_err(|e| on(target, e))?;

        let refused = plan_variable_change(
            session,
            &info,
            "max_connect_errors",
            &set(EditScope::Global, "lots"),
        )
        .await;
        assert!(
            matches!(refused, Err(LiveError::Invalid { .. })),
            "{refused:?}"
        );
        s.close().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn server_status_summaries() -> TestResult {
    for target in pg_targets().into_iter().chain(my_targets()) {
        let s = connect(target).await?;
        let info = s.server_info().clone();
        let status = server_status(s.as_ref(), &info)
            .await
            .map_err(|e| on(target, e))?;
        assert!(
            status.uptime_seconds.is_some_and(|u| u >= 0),
            "{}: {status:?}",
            target.name
        );
        assert!(
            status.connections.is_some_and(|c| c >= 1),
            "{}",
            target.name
        );
        assert!(
            status.max_connections.is_some_and(|c| c > 0),
            "{}",
            target.name
        );
        assert!(
            status.active_queries.is_some_and(|a| a >= 0),
            "{}",
            target.name
        );
        assert!(status.version_detail.is_some(), "{}", target.name);
        if let Some(ratio) = status.cache_hit_ratio {
            assert!((0.0..=1.0).contains(&ratio), "{}", target.name);
        }
        assert!(
            status.replication.visible,
            "{}: {:?}",
            target.name, status.replication
        );
        assert_eq!(
            status.replication.role,
            ReplicationRole::Standalone,
            "{}",
            target.name
        );
        match target.engine {
            Engine::Postgres => {
                assert!(status.cache_hit_ratio.is_some());
                let db = status
                    .databases
                    .iter()
                    .find(|d| d.name == "varve")
                    .ok_or_else(|| on(target, "database varve missing"))?;
                assert!(db.size_bytes.is_some_and(|b| b > 0));
                assert!(status.galera.is_none());
            }
            Engine::Mariadb => {
                let galera = status
                    .galera
                    .as_ref()
                    .ok_or_else(|| on(target, "no galera"))?;
                assert!(!galera.enabled);
                assert!(!galera.values.is_empty());
                let direct = varve_live_stats::sql::galera_status(s.as_ref(), &info).await?;
                assert!(!direct.enabled);
            }
            _ => {
                assert!(status.galera.is_none());
                assert!(
                    varve_live_stats::sql::galera_status(s.as_ref(), &info)
                        .await
                        .is_err()
                );
            }
        }
        s.close().await?;
    }
    Ok(())
}

// Generated MySQL scope table ---------------------------------------------

/// The MySQL servers the scope table is generated from, with the table key
/// (`major.minor`, 9.x shared) and the const name.
fn readonly_sources() -> Vec<(Target, (u32, u32), &'static str)> {
    vec![
        (
            Target {
                name: "MySQL 8.0",
                engine: Engine::Mysql,
                port: port("VARVE_M2_MY80_PORT", 35080),
            },
            (8, 0),
            "MYSQL_8_0",
        ),
        (
            Target {
                name: "MySQL 8.4",
                engine: Engine::Mysql,
                port: port("VARVE_M2_MY84_PORT", 35084),
            },
            (8, 4),
            "MYSQL_8_4",
        ),
        (
            Target {
                name: "MySQL 9.7",
                engine: Engine::Mysql,
                port: port("VARVE_M2_MY97_PORT", 35097),
            },
            (9, 0),
            "MYSQL_9_X",
        ),
    ]
}

const READONLY_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/sql/mysql_readonly.rs");

/// Every row of a query as text, `NULL` as `None`.
async fn text_rows(
    session: &dyn Session,
    sql: &str,
) -> Result<Vec<Vec<Option<String>>>, Box<dyn Error>> {
    let output = session
        .execute(sql, &ExecOptions { max_rows: 100_000 })
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    let mut out = Vec::new();
    for result in output.results {
        if let varve_driver::StatementResult::Rows { rows, .. } = result {
            for row in rows {
                out.push(row.iter().map(|v| v.to_plain_text()).collect());
            }
        }
    }
    Ok(out)
}

/// Global values and persisted values, to prove the probe changed nothing.
async fn global_snapshot(
    session: &dyn Session,
) -> Result<Vec<Vec<Option<String>>>, Box<dyn Error>> {
    let mut rows = text_rows(session, "SHOW GLOBAL VARIABLES").await?;
    rows.extend(
        text_rows(
            session,
            "SELECT VARIABLE_NAME, VARIABLE_VALUE FROM performance_schema.persisted_variables \
             ORDER BY VARIABLE_NAME",
        )
        .await?,
    );
    Ok(rows)
}

fn quote_var(name: &str) -> String {
    name.split('.')
        .map(|part| format!("`{}`", part.replace('`', "``")))
        .collect::<Vec<_>>()
        .join(".")
}

/// Probes which scopes MySQL accepts for every variable, without changing
/// anything: each `SET` also assigns a read-only sentinel variable, and
/// MySQL checks every assignment of a `SET` before applying any, so the
/// statement always fails. The error names the variable it stopped at:
/// the probed one (read-only 1238, wrong scope 1228 or 1229) or, when the
/// probed one passed, the sentinel.
async fn probe_scopes(target: Target) -> Result<(String, Vec<(String, u8)>), Box<dyn Error>> {
    let session = connect(target).await?;
    let version = session.server_info().version.clone();
    let before = global_snapshot(session.as_ref()).await?;
    let names = text_rows(
        session.as_ref(),
        "SELECT VARIABLE_NAME FROM performance_schema.variables_info ORDER BY VARIABLE_NAME",
    )
    .await?;
    let mut table = Vec::new();
    for name in names
        .into_iter()
        .filter_map(|row| row.into_iter().next().flatten())
    {
        let name = name.to_ascii_lowercase();
        let sentinel = if name == "version" {
            "hostname"
        } else {
            "version"
        };
        let mut mask = 0u8;
        for (bit, scope) in [
            (1u8, "SESSION"),
            (2, "GLOBAL"),
            (4, "PERSIST"),
            (8, "PERSIST_ONLY"),
        ] {
            let sql = format!(
                "SET {scope} {var} = @@{var}, SESSION {sentinel} = 'x'",
                var = quote_var(&name)
            );
            match session.execute(&sql, &ExecOptions::default()).await {
                Ok(_) => {
                    return Err(on(
                        target,
                        format!("{sql} succeeded; the probe must always fail"),
                    ));
                }
                Err(error) => {
                    let code = error
                        .code
                        .as_deref()
                        .and_then(|c| c.split_whitespace().next())
                        .unwrap_or_default()
                        .to_string();
                    let names_probed = error.message.contains(&format!("'{name}'"));
                    let refused = names_probed && matches!(code.as_str(), "1238" | "1228" | "1229");
                    if !refused {
                        mask |= bit;
                    }
                }
            }
        }
        table.push((name, mask));
    }
    let after = global_snapshot(session.as_ref()).await?;
    if before != after {
        let changed: Vec<_> = before
            .iter()
            .zip(after.iter())
            .filter(|(a, b)| a != b)
            .take(5)
            .collect();
        return Err(on(
            target,
            format!(
                "the probe changed global state: {changed:?} (row counts {} and {})",
                before.len(),
                after.len()
            ),
        ));
    }
    session.close().await?;
    table.sort();
    Ok((version, table))
}

fn mask_text(mask: u8) -> String {
    let parts: Vec<&str> = [
        (1, "SESSION"),
        (2, "GLOBAL"),
        (4, "PERSIST"),
        (8, "PERSIST_ONLY"),
    ]
    .into_iter()
    .filter(|(bit, _)| mask & bit != 0)
    .map(|(_, name)| name)
    .collect();
    if parts.is_empty() {
        "0".into()
    } else {
        parts.join(" | ")
    }
}

/// The generated Rust for one server's table.
fn table_section(const_name: &str, version: &str, table: &[(String, u8)]) -> String {
    let mut out = format!(
        "/// Probed on MySQL {version}.\n#[rustfmt::skip]\nconst {const_name}: &[(&str, u8)] = &[\n"
    );
    for (name, mask) in table {
        out.push_str(&format!("    ({name:?}, {}),\n", mask_text(*mask)));
    }
    out.push_str("];\n");
    out
}

fn readonly_file(sections: &[(String, (u32, u32), &str, String)]) -> String {
    let versions: Vec<&str> = sections.iter().map(|s| s.0.as_str()).collect();
    let mut out = format!(
        "//! Which scopes MySQL accepts per system variable, probed on MySQL {}.\n\
         //!\n\
         //! Generated by the ignored `generate_mysql_readonly_table` test in\n\
         //! `tests/sql_live.rs` (run it with `VARVE_WRITE_MYSQL_READONLY=1`). Do not\n\
         //! edit by hand. Variables missing from a table are unknown and keep every\n\
         //! scope, relying on the server's error.\n\n\
         /// `SET SESSION` is accepted.\npub(super) const SESSION: u8 = 1;\n\
         /// `SET GLOBAL` is accepted.\npub(super) const GLOBAL: u8 = 2;\n\
         /// `SET PERSIST` is accepted.\npub(super) const PERSIST: u8 = 4;\n\
         /// `SET PERSIST_ONLY` is accepted.\npub(super) const PERSIST_ONLY: u8 = 8;\n\n\
         /// Per probed version: (major, minor) key (9.x shares one), the server\n\
         /// version, and the variables sorted by name with the accepted scopes.\n\
         pub(super) type Table = ((u32, u32), &'static str, &'static [(&'static str, u8)]);\n\n\
         /// Tables ordered by version.\npub(super) const TABLES: &[Table] = &[\n",
        versions.join(", ")
    );
    for (version, key, const_name, _) in sections {
        out.push_str(&format!(
            "    (({}, {}), {version:?}, {const_name}),\n",
            key.0, key.1
        ));
    }
    out.push_str("];\n");
    for (_, _, _, section) in sections {
        out.push('\n');
        out.push_str(section);
    }
    out
}

#[tokio::test]
#[ignore = "needs the M2 database matrix; writes src/sql/mysql_readonly.rs with VARVE_WRITE_MYSQL_READONLY=1"]
async fn generate_mysql_readonly_table() -> TestResult {
    let mut sections = Vec::new();
    for (target, key, const_name) in readonly_sources() {
        let (version, table) = probe_scopes(target).await?;
        assert!(
            table.len() > 400,
            "{}: {} variables",
            target.name,
            table.len()
        );
        let section = table_section(const_name, &version, &table);
        sections.push((version, key, const_name, section));
    }
    let text = readonly_file(&sections);
    if std::env::var("VARVE_WRITE_MYSQL_READONLY").is_ok_and(|v| v == "1") {
        std::fs::write(READONLY_FILE, text)?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn mysql_readonly_table_matches_the_servers() -> TestResult {
    let committed = std::fs::read_to_string(READONLY_FILE)?;
    for (target, _, const_name) in readonly_sources() {
        let (version, table) = probe_scopes(target).await?;
        let section = table_section(const_name, &version, &table);
        assert!(
            committed.contains(&section),
            "{}: the committed table differs from what MySQL {version} reports; regenerate it",
            target.name
        );
    }
    Ok(())
}

// Streaming replica -------------------------------------------------------

/// The PostgreSQL 18 streaming pair of the M2 matrix: primary on
/// `VARVE_M2_PG_PRIMARY_PORT` (55611), standby on `VARVE_M2_PG_REPLICA_PORT`
/// (55612), standby slot `replica1`. The disconnect step restores
/// `primary_conninfo` and waits until the standby streams again, also when
/// the checks in between fail, so the shared pair is left healthy.
#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn postgres_standby_lag_and_disconnect() -> TestResult {
    let replica_target = Target {
        name: "PostgreSQL 18 standby",
        engine: Engine::Postgres,
        port: port("VARVE_M2_PG_REPLICA_PORT", 55612),
    };
    let primary_target = Target {
        name: "PostgreSQL 18 primary",
        engine: Engine::Postgres,
        port: port("VARVE_M2_PG_PRIMARY_PORT", 55611),
    };
    let primary = connect(primary_target).await?;
    let replica = connect(replica_target).await?;
    let info = replica.server_info().clone();

    // The primary sees the standby, with a redacted application name.
    let status = server_status(primary.as_ref(), primary.server_info()).await?;
    assert_eq!(status.replication.role, ReplicationRole::Primary);
    assert!(
        status
            .replication
            .replicas
            .iter()
            .any(|r| r.state.as_deref() == Some("streaming")),
        "{:?}",
        status.replication
    );

    // Streaming and caught up: lag 0, with the primary's position.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = server_status(replica.as_ref(), &info).await?;
        assert_eq!(status.replication.role, ReplicationRole::Replica);
        let up = status
            .replication
            .upstream
            .first()
            .ok_or("no upstream on the standby")?;
        if up.state.as_deref() == Some("streaming") && up.lag_seconds == Some(0.0) {
            assert!(up.primary_position.is_some(), "{up:?}");
            assert!(
                up.lag_note
                    .as_deref()
                    .is_some_and(|n| n.contains("caught up"))
            );
            break;
        }
        if Instant::now() > deadline {
            return Err(format!("the standby never caught up: {up:?}").into());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // primary_conninfo is shown without its password.
    let conninfo = server_variable(replica.as_ref(), &info, "primary_conninfo").await?;
    assert!(conninfo.masked, "{conninfo:?}");
    let shown = conninfo.value.clone().unwrap_or_default();
    assert!(
        shown.contains("host=") && !shown.contains("password"),
        "{shown}"
    );
    let refused = variable_edit_plan(&info, &conninfo, &set(EditScope::Global, &shown));
    assert!(
        matches!(refused, Err(LiveError::Invalid { .. })),
        "{refused:?}"
    );

    // Stop the WAL receiver by clearing primary_conninfo, keeping the raw
    // value (in memory only) to restore it.
    let raw = text_rows(replica.as_ref(), "SHOW primary_conninfo").await?;
    let raw = raw
        .first()
        .and_then(|row| row.first().cloned().flatten())
        .ok_or("no primary_conninfo")?;
    if raw.is_empty() {
        return Err("the standby has no primary_conninfo: the shared pair is not streaming".into());
    }
    exec(replica.as_ref(), "ALTER SYSTEM SET primary_conninfo = ''").await?;
    exec(replica.as_ref(), "SELECT pg_reload_conf()").await?;
    let result = async {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = server_status(replica.as_ref(), &info).await?;
            let up = status
                .replication
                .upstream
                .first()
                .ok_or("no upstream on the standby")?
                .clone();
            // Errors, not asserts: the restore below must always run.
            if up.state.as_deref() != Some("streaming") {
                let note_ok = up.lag_note.as_deref().is_some_and(|n| {
                    n.starts_with("WAL receiver not streaming") && n.ends_with("lag unknown")
                });
                if status.replication.role != ReplicationRole::Replica
                    || up.lag_seconds.is_some()
                    || !note_ok
                {
                    return Err(format!(
                        "a disconnected standby must report unknown lag: {:?}",
                        status.replication
                    )
                    .into());
                }
                return Ok::<(), Box<dyn Error>>(());
            }
            if Instant::now() > deadline {
                return Err(format!("the WAL receiver kept streaming: {up:?}").into());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    .await;
    // Restore the original connection string and wait for streaming, also
    // when the checks above failed.
    let literal = format!("'{}'", raw.replace('\'', "''"));
    exec(
        replica.as_ref(),
        &format!("ALTER SYSTEM SET primary_conninfo = {literal}"),
    )
    .await?;
    exec(replica.as_ref(), "SELECT pg_reload_conf()").await?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let streaming = text_rows(replica.as_ref(), "SELECT status FROM pg_stat_wal_receiver")
            .await?
            .first()
            .and_then(|row| row.first().cloned().flatten());
        if streaming.as_deref() == Some("streaming") {
            break;
        }
        if Instant::now() > deadline {
            return Err("the standby did not resume streaming after the restore".into());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    result?;
    replica.close().await?;
    primary.close().await?;
    Ok(())
}
