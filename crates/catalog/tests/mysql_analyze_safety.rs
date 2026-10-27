//! The server-side part of the `EXPLAIN ANALYZE` safety check on MySQL
//! 8.0 and 9.7 and MariaDB 10.6 and 12.3: an unqualified call of a name the
//! server has no built-in for reaches a stored function of the default
//! database, so a call the text check takes for a side-effect-free
//! built-in (`random()`, a built-in on neither engine) must be confirmed
//! when such a stored function exists, also for names the text check takes
//! for syntax (`operator(`, `first(`), for stored functions behind views,
//! for views the server does not expand, for tables of an engine that is
//! not known to be local (named in every way the server accepts, temporary
//! ones included) and for locking reads; ordinary reads, tables with
//! triggers, `dual` and temporary tables of local engines run. Inside the
//! user's transaction EXPLAIN ANALYZE still works and leaves that
//! transaction intact.
//!
//! ```sh
//! cargo test -p varve-catalog --test mysql_analyze_safety -- --include-ignored
//! ```
//!
//! Works in the database `m2_myhijack` and the user `m2_myhijack_user`,
//! dropped before and after.

use std::error::Error;
use std::time::Duration;

use varve_catalog::CatalogError;
use varve_catalog::explain::{
    AnalyzeSafety, ExplainCancel, ExplainOptions, analyze_safety, analyze_safety_checked, explain,
    explain_with_cancel,
};
use varve_core::Engine;
use varve_driver::{
    ConnectParams, Driver, ExecOptions, ExecOutput, Session, StatementResult, TlsMode, Value,
};
use varve_driver_mysql::MysqlDriver;
use varve_secrets::Secret;

type R<T = ()> = Result<T, Box<dyn Error>>;

const DATABASE: &str = "m2_myhijack";
const USER: &str = "m2_myhijack_user";
const USER_PASSWORD: &str = "m2 hijack pw";

#[derive(Debug, Clone, Copy)]
struct Target {
    name: &'static str,
    engine: Engine,
    port: u16,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
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

/// The servers to test (see [`selected`]), never none.
fn targets() -> Vec<Target> {
    let targets: Vec<Target> = every_target()
        .into_iter()
        .filter(|target| selected(&target.name.to_lowercase().replace(' ', "-")))
        .collect();
    assert!(
        !targets.is_empty(),
        "VARVE_ONLY_SERVERS selects no MySQL or MariaDB server"
    );
    targets
}

fn every_target() -> Vec<Target> {
    let port = |name: &str, default: u16| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    vec![
        Target {
            name: "MySQL 8.0",
            engine: Engine::Mysql,
            port: port("VARVE_M2_MYSQL_8_0_PORT", 35080),
        },
        Target {
            name: "MySQL 9.7",
            engine: Engine::Mysql,
            port: port("VARVE_M2_MYSQL_9_7_PORT", 35097),
        },
        Target {
            name: "MariaDB 10.6",
            engine: Engine::Mariadb,
            port: port("VARVE_M2_MARIADB_10_6_PORT", 35106),
        },
        Target {
            name: "MariaDB 12.3",
            engine: Engine::Mariadb,
            port: port("VARVE_M2_MARIADB_12_3_PORT", 35123),
        },
    ]
}

async fn connect(
    target: &Target,
    user: &str,
    password: &str,
    database: Option<&str>,
) -> R<Box<dyn Session>> {
    let mut params = ConnectParams::new(
        target.engine,
        env_or("VARVE_M2_MYSQL_HOST", "127.0.0.1"),
        target.port,
    );
    params.username = Some(user.into());
    params.password = Some(Secret::new(password.into()));
    params.database = database.map(str::to_owned);
    params.tls.mode = TlsMode::Disable;
    params.connect_timeout = Duration::from_secs(10);
    params.application_name = "varve-catalog mysql_analyze_safety tests".into();
    Ok(MysqlDriver::new()
        .connect(&params)
        .await
        .map_err(|e| format!("{}: connect as {user}: {e}", target.name))?)
}

async fn root(target: &Target, database: Option<&str>) -> R<Box<dyn Session>> {
    connect(
        target,
        &env_or("VARVE_M2_MYSQL_USER", "root"),
        &env_or("VARVE_M2_MYSQL_PASSWORD", "varve_dev"),
        database,
    )
    .await
}

async fn exec(session: &dyn Session, sql: &str) -> R<ExecOutput> {
    Ok(session
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| format!("{sql}\n=> {e}"))?)
}

async fn count(session: &dyn Session, table: &str) -> R<i64> {
    let output = exec(session, &format!("SELECT COUNT(*) FROM {DATABASE}.{table}")).await?;
    for result in output.results {
        if let StatementResult::Rows { rows, .. } = result {
            return match rows.first().and_then(|row| row.first()) {
                Some(Value::Int(n)) => Ok(*n),
                Some(Value::UInt(n)) => Ok(i64::try_from(*n)?),
                other => Err(format!("unexpected count {other:?}").into()),
            };
        }
    }
    Err("no count".into())
}

async fn cleanup(admin: &dyn Session) -> R {
    exec(admin, &format!("DROP DATABASE IF EXISTS {DATABASE}")).await?;
    exec(admin, &format!("DROP USER IF EXISTS '{USER}'@'%'")).await?;
    Ok(())
}

async fn setup(admin: &dyn Session) -> R {
    cleanup(admin).await?;
    exec(admin, &format!("CREATE DATABASE {DATABASE}")).await?;
    exec(admin, &format!("CREATE TABLE {DATABASE}.hits (n INT)")).await?;
    exec(
        admin,
        &format!("CREATE TABLE {DATABASE}.marks (n INT) ENGINE = InnoDB"),
    )
    .await?;
    // `random` is a built-in on neither engine: an unqualified call
    // reaches this function.
    exec(
        admin,
        &format!(
            "CREATE FUNCTION {DATABASE}.random() RETURNS DOUBLE DETERMINISTIC MODIFIES SQL DATA \
             BEGIN INSERT INTO {DATABASE}.hits VALUES (1); RETURN 0.5; END"
        ),
    )
    .await?;
    // Functions named like keywords the text check takes for syntax, and a
    // view over a stored function; they write a MyISAM table, which no
    // rollback undoes.
    exec(
        admin,
        &format!("CREATE TABLE {DATABASE}.myhits (n INT) ENGINE = MyISAM"),
    )
    .await?;
    exec(
        admin,
        &format!(
            "CREATE TABLE {DATABASE}.twice (n INT); INSERT INTO {DATABASE}.twice VALUES (1), (2)"
        ),
    )
    .await?;
    for name in ["operator", "first"] {
        exec(
            admin,
            &format!(
                "CREATE FUNCTION {DATABASE}.`{name}`(x INT) RETURNS INT DETERMINISTIC \
                 MODIFIES SQL DATA BEGIN INSERT INTO {DATABASE}.myhits VALUES (x); RETURN x; END"
            ),
        )
        .await?;
    }
    exec(
        admin,
        &format!("CREATE VIEW {DATABASE}.vrand AS SELECT {DATABASE}.random() AS r"),
    )
    .await?;
    // A view the server cannot merge (its body stays hidden), a plain
    // view, and a table whose trigger calls a stored function.
    exec(
        admin,
        &format!(
            "CREATE ALGORITHM=TEMPTABLE VIEW {DATABASE}.vtemp AS \
             SELECT n FROM {DATABASE}.twice GROUP BY n; \
             CREATE VIEW {DATABASE}.vplain AS SELECT n FROM {DATABASE}.twice; \
             CREATE TABLE {DATABASE}.trg (n INT); \
             CREATE TRIGGER {DATABASE}.trg_ins BEFORE INSERT ON {DATABASE}.trg \
               FOR EACH ROW SET NEW.n = {DATABASE}.`first`(NEW.n)"
        ),
    )
    .await?;
    // 1000 InnoDB rows: a three-way join over them runs for minutes and
    // stops at once when killed.
    exec(
        admin,
        &format!(
            "CREATE TABLE {DATABASE}.big (n INT) ENGINE = InnoDB; \
             INSERT INTO {DATABASE}.big WITH RECURSIVE r (n) AS \
               (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 1000) SELECT n FROM r"
        ),
    )
    .await?;
    exec(
        admin,
        &format!(
            "CREATE TABLE {DATABASE}.csvt (n INT NOT NULL) ENGINE = CSV; \
             CREATE TABLE {DATABASE}.mem (n INT) ENGINE = MEMORY"
        ),
    )
    .await?;
    exec(
        admin,
        &format!("CREATE USER '{USER}'@'%' IDENTIFIED BY '{USER_PASSWORD}'"),
    )
    .await?;
    exec(
        admin,
        &format!(
            "GRANT SELECT, INSERT, DELETE, EXECUTE, SHOW VIEW, CREATE TEMPORARY TABLES \
             ON {DATABASE}.* TO '{USER}'@'%'"
        ),
    )
    .await?;
    Ok(())
}

fn confirmation(safety: AnalyzeSafety) -> R<Vec<String>> {
    match safety {
        AnalyzeSafety::NeedsConfirmation { warnings } => Ok(warnings),
        other => Err(format!("expected a confirmation, got {other:?}").into()),
    }
}

/// The checks on one session whose default database is [`DATABASE`].
async fn check(target: &Target, session: &dyn Session, who: &str) -> R {
    let server = session.server_info().clone();
    let context = format!("{} as {who}", target.name);
    let statement = "SELECT random() AS r";
    // The text alone takes random() for a built-in.
    assert_eq!(
        analyze_safety(&server, statement),
        AnalyzeSafety::ReadOnly,
        "{context}"
    );
    let warnings = confirmation(analyze_safety_checked(session, &server, statement).await)?;
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("random() is a stored function")),
        "{context}: {warnings:?}"
    );
    let analyze = ExplainOptions {
        analyze: true,
        ..ExplainOptions::default()
    };
    let refused = explain(session, &server, statement, &analyze).await;
    assert!(
        matches!(refused, Err(CatalogError::NeedsConfirmation { .. })),
        "{context}: {refused:?}"
    );
    // Upper case names the same function.
    let warnings =
        confirmation(analyze_safety_checked(session, &server, "SELECT RANDOM() AS r").await)?;
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("random() is a stored function")),
        "{context}: {warnings:?}"
    );
    // Keywords that are stored functions, a view over a stored function, a
    // schema-qualified call and a change of data need confirmation.
    for (statement, word) in [
        ("SELECT operator(1) AS o", "operator()"),
        ("SELECT first(1) AS f", "first()"),
        // Views ask whatever they hold (MariaDB's EXPLAIN would evaluate a
        // deterministic stored function in one while planning).
        ("SELECT * FROM vrand", "vrand is a view"),
        ("SELECT * FROM vtemp", "vtemp is a view"),
        ("SELECT * FROM vplain", "vplain is a view"),
        ("SELECT * FROM m2_myhijack.vplain", "vplain is a view"),
        // A stored function inside a derived table and a subquery.
        ("SELECT * FROM (SELECT random() AS r) d", "random()"),
        (
            "SELECT n FROM twice WHERE n > (SELECT random())",
            "random()",
        ),
        // Changing a table with a trigger asks like any change.
        ("UPDATE trg SET n = 1", "changes data"),
        // The text check already asks for it (and then nothing is looked up).
        ("SELECT m2_myhijack.random() AS r", "m2_myhijack.random()"),
        ("INSERT INTO marks VALUES (2)", "changes data"),
        // User variables are session state no rollback restores.
        ("SELECT @x := 5 AS v", "user variable"),
        ("SELECT n INTO @y FROM twice WHERE n = 1", "user variable"),
    ] {
        let warnings = confirmation(analyze_safety_checked(session, &server, statement).await)?;
        assert!(
            warnings.iter().any(|w| w.contains(word)),
            "{context}: {statement}: {warnings:?}"
        );
        let refused = explain(session, &server, statement, &analyze).await;
        assert!(
            matches!(refused, Err(CatalogError::NeedsConfirmation { .. })),
            "{context}: {statement}: {refused:?}"
        );
    }
    // A table read stays read only and runs.
    let read = "SELECT n FROM hits WHERE n > 0";
    assert_eq!(
        analyze_safety_checked(session, &server, read).await,
        AnalyzeSafety::ReadOnly,
        "{context}"
    );
    explain(session, &server, read, &analyze)
        .await
        .map_err(|e| format!("{context}: {read}: {e}"))?;
    // A proven read that fails at run time returns its own error.
    let failing = explain(
        session,
        &server,
        "SELECT n FROM twice WHERE n = (SELECT n FROM twice)",
        &analyze,
    )
    .await;
    assert!(
        matches!(&failing, Err(CatalogError::Driver { .. })),
        "{context}: {failing:?}"
    );
    // With autocommit off the failed statement opened a transaction; the
    // run rolls it back.
    exec(session, "SET autocommit = 0").await?;
    let failing = explain(
        session,
        &server,
        "SELECT n FROM twice WHERE n = (SELECT n FROM twice)",
        &analyze,
    )
    .await;
    let after = session.in_transaction();
    exec(session, "SET autocommit = 1").await?;
    assert!(
        matches!(&failing, Err(CatalogError::Driver { .. })),
        "{context}: autocommit off: {failing:?}"
    );
    assert_eq!(
        after,
        Some(false),
        "{context}: autocommit off: the failed run left a transaction open"
    );
    // Locking reads confirm, as on PostgreSQL.
    for statement in [
        "SELECT n FROM twice FOR UPDATE",
        "SELECT n FROM twice FOR SHARE",
        "SELECT n FROM twice LOCK IN SHARE MODE",
        "SELECT n FROM twice FOR UPDATE SKIP LOCKED",
    ] {
        let warnings = confirmation(analyze_safety_checked(session, &server, statement).await)?;
        assert!(
            warnings.iter().any(|w| w.contains("locks rows")),
            "{context}: {statement}: {warnings:?}"
        );
    }
    // Tables of local engines read without confirmation, also a table with
    // a trigger, `dual`, joins, derived tables, CTEs, JSON_TABLE and a
    // temporary table of a local engine.
    exec(session, "CREATE TEMPORARY TABLE tmp_local (n INT)").await?;
    for statement in [
        "SELECT * FROM myhits",
        "SELECT * FROM csvt",
        "SELECT * FROM mem",
        "SELECT * FROM trg",
        "SELECT 1 FROM dual",
        "SELECT 1",
        "SELECT * FROM (twice)",
        "SELECT a.n FROM twice a JOIN twice b USING (n) WHERE a.n > 1 ORDER BY 1",
        "SELECT * FROM (SELECT n FROM twice) d",
        "WITH c (x) AS (SELECT n FROM twice) SELECT x, COUNT(*) OVER () FROM c",
        "SELECT jt.v FROM twice, JSON_TABLE('[1]', '$[*]' COLUMNS (v INT PATH '$')) jt",
        "SELECT n FROM twice WHERE n IN (SELECT n FROM hits)",
        "SELECT * FROM tmp_local",
        "SELECT COUNT(*), MAX(n) FROM twice GROUP BY n HAVING COUNT(*) > 0",
        // A trailing line comment, and a CTE with a column list (printed
        // like a call).
        "SELECT n FROM twice -- f(1)",
        "SELECT 1 # f(1)",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 3) \
         SELECT * FROM c",
    ] {
        assert_eq!(
            analyze_safety_checked(session, &server, statement).await,
            AnalyzeSafety::ReadOnly,
            "{context}: {statement}"
        );
    }
    if target.engine == Engine::Mariadb {
        assert_eq!(
            analyze_safety_checked(session, &server, "SELECT * FROM .twice").await,
            AnalyzeSafety::ReadOnly,
            "{context}: leading dot"
        );
        // A blocking replication wait is a sleep.
        let warnings = confirmation(
            analyze_safety_checked(
                session,
                &server,
                "SELECT MASTER_GTID_WAIT('0-1-999999', 0.2)",
            )
            .await,
        )?;
        assert!(
            warnings.iter().any(|w| w.contains("master_gtid_wait")),
            "{context}: {warnings:?}"
        );
    }
    exec(session, "DROP TEMPORARY TABLE tmp_local").await?;
    // Dropping the call at any point leaves no transaction open: the next
    // statement writes and commits.
    for step in 0..30u64 {
        let _ = tokio::time::timeout(
            Duration::from_micros(200 * step),
            explain(session, &server, read, &analyze),
        )
        .await;
        exec(session, "INSERT INTO marks VALUES (99)")
            .await
            .map_err(|e| format!("{context}: after a drop at step {step}: {e}"))?;
        assert_ne!(
            session.in_transaction(),
            Some(true),
            "{context}: step {step}"
        );
    }
    exec(session, "DELETE FROM marks WHERE n = 99").await?;
    // One cancel stops the whole explain wherever it lands among its
    // calls (the lookup, the run; for a confirmed run the transaction,
    // the run and the rollback), and nothing stays open.
    let slow = "SELECT COUNT(*) FROM big a, big b, big c WHERE a.n + b.n > c.n";
    let confirmed = ExplainOptions {
        confirmed: true,
        ..analyze
    };
    for options in [&analyze, &confirmed] {
        for delay in [0u64, 1, 2, 4, 8, 15, 30, 60] {
            let how = format!(
                "{context}: confirmed {}, cancel at {delay} ms",
                options.confirmed
            );
            let cancel = ExplainCancel::new();
            let trigger = cancel.clone();
            let canceller = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                trigger.cancel();
            });
            let outcome = tokio::time::timeout(
                Duration::from_secs(10),
                explain_with_cancel(session, &server, slow, options, &cancel),
            )
            .await;
            let _ = canceller.await;
            let Ok(outcome) = outcome else {
                session.cancel_handle().cancel().await?;
                return Err(format!("{how}: the explain ran on").into());
            };
            // A cancel during the lookup is a cancel, not a reason to confirm.
            assert!(
                !matches!(outcome, Err(CatalogError::NeedsConfirmation { .. })),
                "{how}: {outcome:?}"
            );
            // MySQL returns the plan of an interrupted EXPLAIN ANALYZE; a
            // cancel either way.
            exec(session, "INSERT INTO marks VALUES (98)")
                .await
                .map_err(|e| format!("{how}: {e}"))?;
            assert_ne!(session.in_transaction(), Some(true), "{how}");
        }
    }
    exec(session, "DELETE FROM marks WHERE n = 98").await?;
    // A built-in nothing shadows stays read only and runs.
    let safe = "SELECT upper('x') AS u";
    assert_eq!(
        analyze_safety_checked(session, &server, safe).await,
        AnalyzeSafety::ReadOnly,
        "{context}"
    );
    explain(session, &server, safe, &analyze)
        .await
        .map_err(|e| format!("{context}: {safe}: {e}"))?;

    // Inside the user's transaction EXPLAIN ANALYZE works and leaves the
    // transaction as it was. MariaDB does not look anything up there (its
    // routine tables are Aria tables, which would break the savepoint), so
    // the call needs confirmation.
    exec(session, "START TRANSACTION").await?;
    exec(session, &format!("INSERT INTO {DATABASE}.marks VALUES (1)")).await?;
    let inside = analyze_safety_checked(session, &server, safe).await;
    let confirmed = ExplainOptions {
        confirmed: true,
        ..analyze
    };
    let ran = explain(session, &server, safe, &confirmed).await;
    let marks = count(session, "marks").await;
    exec(session, "ROLLBACK").await?;
    if target.engine == Engine::Mariadb {
        let warnings = confirmation(inside)?;
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("inside a MariaDB transaction")),
            "{context}: {warnings:?}"
        );
    } else {
        assert_eq!(inside, AnalyzeSafety::ReadOnly, "{context}");
    }
    ran.map_err(|e| format!("{context}: EXPLAIN ANALYZE in a transaction: {e}"))?;
    assert_eq!(marks?, 1, "{context}: the transaction lost its row");
    assert_eq!(
        count(session, "marks").await?,
        0,
        "{context}: the rollback did not undo the transaction"
    );
    Ok(())
}

/// A table of an engine that is not known to be local needs confirmation:
/// reading it can reach another server. MariaDB ships FederatedX as a
/// plugin (installed for the test when it is not, and uninstalled again);
/// MySQL's `FEDERATED` is disabled in its default configuration, so there
/// only the local engines are tested (in [`check`]).
async fn remote_tables(target: &Target, admin: &dyn Session) -> R {
    if target.engine != Engine::Mariadb {
        return Ok(());
    }
    let output = exec(
        admin,
        "SELECT SUPPORT FROM information_schema.ENGINES WHERE ENGINE = 'FEDERATED'",
    )
    .await?;
    let installed = output.results.iter().any(|result| {
        matches!(result, StatementResult::Rows { rows, .. }
            if rows.iter().any(|row| matches!(row.first(), Some(Value::Text(t)) if t == "YES")))
    });
    if !installed {
        exec(admin, "INSTALL SONAME 'ha_federatedx'").await?;
    }
    let created = exec(
        admin,
        &format!(
            "CREATE TABLE {DATABASE}.fed (n INT) ENGINE = FEDERATED \
             CONNECTION = 'mysql://{}:{}@127.0.0.1:3306/{DATABASE}/twice'",
            env_or("VARVE_M2_MYSQL_USER", "root"),
            env_or("VARVE_M2_MYSQL_PASSWORD", "varve_dev"),
        ),
    )
    .await;
    // A task of its own, so a failed assertion still reaches the cleanup
    // below (the plugin must not stay installed).
    let target = *target;
    let checks =
        tokio::spawn(async move { remote_checks(&target).await.map_err(|e| e.to_string()) }).await;
    exec(admin, &format!("DROP TABLE IF EXISTS {DATABASE}.fed")).await?;
    if !installed {
        exec(admin, "UNINSTALL SONAME 'ha_federatedx'").await?;
    }
    created?;
    match checks {
        Ok(result) => Ok(result?),
        Err(panic) => Err(format!("{}: {panic}", target.name).into()),
    }
}

/// The checks of [`remote_tables`] on a session of their own.
async fn remote_checks(target: &Target) -> R {
    let session = root(target, Some(DATABASE)).await?;
    let server = session.server_info().clone();
    // A temporary table of the remote engine, also one that shadows a
    // local table: information_schema.TABLES lists temporary tables
    // only from MariaDB 11.2.
    let connection = format!(
        "CONNECTION = 'mysql://{}:{}@127.0.0.1:3306/{DATABASE}/twice'",
        env_or("VARVE_M2_MYSQL_USER", "root"),
        env_or("VARVE_M2_MYSQL_PASSWORD", "varve_dev"),
    );
    exec(
        session.as_ref(),
        &format!(
            "CREATE TEMPORARY TABLE tfed (n INT) ENGINE = FEDERATED {connection}; \
             CREATE TEMPORARY TABLE marks (n INT) ENGINE = FEDERATED {connection}"
        ),
    )
    .await?;
    // Temporary tables named like reserved words, which only MariaDB's
    // `.name` syntax can read (the server prints them, so they are found
    // whatever the statement's words; `SELECT 1` does not read `select`:
    // with lower_case_table_names = 0 the keyword's case differs anyway).
    for name in ["`select`", "`now`"] {
        exec(
            session.as_ref(),
            &format!("CREATE TEMPORARY TABLE {name} (n INT) ENGINE = FEDERATED {connection}"),
        )
        .await?;
    }
    for statement in [
        "SELECT * FROM .select",
        "SELECT now() IS NOT NULL FROM (.now)",
    ] {
        let warnings =
            confirmation(analyze_safety_checked(session.as_ref(), &server, statement).await)
                .map_err(|e| format!("{}: {statement}: {e}", target.name))?;
        assert!(
            warnings.iter().any(|w| w.contains("FEDERATED")),
            "{}: {statement}: {warnings:?}",
            target.name
        );
    }
    exec(
        session.as_ref(),
        "DROP TEMPORARY TABLE `select`; DROP TEMPORARY TABLE `now`",
    )
    .await?;
    // Case (lower_case_table_names = 0 on the matrix: `TFU` is not `tfu`),
    // and `ENGINE=` inside the column list (a comment, a default, a name)
    // where the table options say FEDERATED.
    for definition in [
        "CREATE TEMPORARY TABLE TFU (n INT)",
        "CREATE TEMPORARY TABLE tfc (n INT COMMENT 'ENGINE=InnoDB', v VARCHAR(5))",
        "CREATE TEMPORARY TABLE tfd (n INT, v VARCHAR(20) DEFAULT 'ENGINE=MyISAM')",
        "CREATE TEMPORARY TABLE `ENGINE=InnoDB` (n INT)",
    ] {
        exec(
            session.as_ref(),
            &format!("{definition} ENGINE = FEDERATED {connection}"),
        )
        .await?;
    }
    for statement in [
        "SELECT * FROM TFU",
        "SELECT twice.n FROM twice WHERE EXISTS (SELECT * FROM TFU WHERE TFU.n = twice.n)",
        "SELECT * FROM tfc",
        "SELECT * FROM tfd",
        "SELECT * FROM `ENGINE=InnoDB`",
    ] {
        let warnings =
            confirmation(analyze_safety_checked(session.as_ref(), &server, statement).await)
                .map_err(|e| format!("{}: {statement}: {e}", target.name))?;
        assert!(
            warnings.iter().any(|w| w.contains("FEDERATED")),
            "{}: {statement}: {warnings:?}",
            target.name
        );
    }
    exec(
        session.as_ref(),
        "DROP TEMPORARY TABLE TFU; DROP TEMPORARY TABLE tfc; DROP TEMPORARY TABLE tfd; \
         DROP TEMPORARY TABLE `ENGINE=InnoDB`",
    )
    .await?;
    // Temporary tables named like non-reserved keywords.
    let keyword_names = [
        "status",
        "data",
        "events",
        "user",
        "`name`",
        "`value`",
        "comment",
        "text",
        "option",
        "schema",
        "`database`",
    ];
    for name in keyword_names {
        exec(
            session.as_ref(),
            &format!("CREATE TEMPORARY TABLE {name} (n INT) ENGINE = FEDERATED {connection}"),
        )
        .await?;
    }
    for statement in keyword_names.iter().flat_map(|name| {
        [
            format!("SELECT * FROM {name}"),
            format!("SELECT twice.n FROM twice, {name}"),
        ]
    }) {
        let warnings =
            confirmation(analyze_safety_checked(session.as_ref(), &server, &statement).await)
                .map_err(|e| format!("{}: {statement}: {e}", target.name))?;
        assert!(
            warnings.iter().any(|w| w.contains("FEDERATED")),
            "{}: {statement}: {warnings:?}",
            target.name
        );
    }
    // Names inside a parenthesized FROM item.
    for statement in [
        "SELECT * FROM (option)",
        "SELECT * FROM ((option))",
        "SELECT * FROM (schema)",
        "SELECT * FROM (`database`)",
    ] {
        let warnings =
            confirmation(analyze_safety_checked(session.as_ref(), &server, statement).await)
                .map_err(|e| format!("{}: {statement}: {e}", target.name))?;
        assert!(
            warnings.iter().any(|w| w.contains("FEDERATED")),
            "{}: {statement}: {warnings:?}",
            target.name
        );
    }
    // Double quotes: a string in the default SQL mode (names nothing), a
    // name under ANSI_QUOTES.
    assert_eq!(
        analyze_safety_checked(session.as_ref(), &server, "SELECT \"status\"").await,
        AnalyzeSafety::ReadOnly,
        "{}",
        target.name
    );
    let mode = exec(session.as_ref(), "SELECT @@SESSION.sql_mode AS m").await?;
    let mode = mode
        .results
        .iter()
        .find_map(|r| match r {
            StatementResult::Rows { rows, .. } => rows.first().and_then(|row| row.first()),
            _ => None,
        })
        .and_then(Value::to_plain_text)
        .unwrap_or_default();
    exec(
        session.as_ref(),
        "SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, ',ANSI_QUOTES')",
    )
    .await?;
    let ansi = analyze_safety_checked(session.as_ref(), &server, "SELECT * FROM \"status\"").await;
    exec(
        session.as_ref(),
        &format!(
            "SET SESSION sql_mode = {}",
            varve_catalog::sql::string(varve_driver::Dialect::Mysql, &mode)?
        ),
    )
    .await?;
    let warnings = confirmation(ansi)?;
    assert!(
        warnings.iter().any(|w| w.contains("FEDERATED")),
        "{}: ANSI_QUOTES: {warnings:?}",
        target.name
    );
    for name in keyword_names {
        exec(session.as_ref(), &format!("DROP TEMPORARY TABLE {name}")).await?;
    }
    // With the remote engine enabled, local reads still run: `SHOW CREATE
    // TABLE` tells every named table's engine, temporary or not.
    exec(
        session.as_ref(),
        "CREATE TEMPORARY TABLE tmp_inno (n INT) ENGINE = InnoDB",
    )
    .await?;
    for statement in [
        "SELECT 1",
        "SELECT UPPER('x')",
        "SELECT 1 FROM dual",
        "SELECT n FROM hits",
        "SELECT twice.n FROM twice, hits",
        "SELECT * FROM tmp_inno",
        "SELECT * FROM (SELECT n FROM twice) d",
        // Aliased joins and qualified columns: `db`.`alias`.`col` in the
        // rewritten statement names no table.
        "SELECT a.n, b.n FROM twice a JOIN twice b ON a.n = b.n",
        "SELECT x.n FROM twice x WHERE x.n > 1",
        "SELECT d.n FROM (SELECT n FROM twice) d JOIN hits h ON h.n = d.n",
    ] {
        assert_eq!(
            analyze_safety_checked(session.as_ref(), &server, statement).await,
            AnalyzeSafety::ReadOnly,
            "{}: {statement}",
            target.name
        );
    }
    // The remote table in every FROM syntax the server accepts.
    for statement in [
        "SELECT * FROM fed".to_owned(),
        format!("SELECT * FROM {DATABASE}.fed"),
        "SELECT * FROM tfed".to_owned(),
        "SELECT * FROM marks".to_owned(),
        "SELECT * FROM .tfed".to_owned(),
        "SELECT * FROM (.tfed)".to_owned(),
        "SELECT * FROM (SELECT n FROM tfed) d".to_owned(),
        "SELECT * FROM twice JOIN tfed USING (n)".to_owned(),
        "SELECT * FROM twice LEFT JOIN (tfed) ON twice.n = tfed.n".to_owned(),
        "SELECT n FROM twice WHERE n IN (SELECT n FROM tfed)".to_owned(),
        "SELECT n FROM twice WHERE EXISTS (SELECT 1 FROM tfed)".to_owned(),
        "WITH c AS (SELECT n FROM tfed) SELECT * FROM c".to_owned(),
        "SELECT jt.v FROM tfed, JSON_TABLE('[1]', '$[*]' COLUMNS (v INT PATH '$')) jt".to_owned(),
        "SELECT (SELECT MAX(n) FROM tfed) AS m".to_owned(),
        "SELECT * FROM twice UNION SELECT * FROM tfed".to_owned(),
    ] {
        let warnings =
            confirmation(analyze_safety_checked(session.as_ref(), &server, &statement).await)
                .map_err(|e| format!("{}: {statement}: {e}", target.name))?;
        assert!(
            warnings.iter().any(|w| w.contains("FEDERATED")),
            "{}: {statement}: {warnings:?}",
            target.name
        );
        let refused = explain(
            session.as_ref(),
            &server,
            &statement,
            &ExplainOptions {
                analyze: true,
                ..ExplainOptions::default()
            },
        )
        .await;
        assert!(
            matches!(refused, Err(CatalogError::NeedsConfirmation { .. })),
            "{}: {statement}: {refused:?}",
            target.name
        );
    }
    session.close().await?;
    Ok(())
}

/// An unqualified call answered by a stored function needs confirmation;
/// as a user who cannot read `mysql.func` too; and without a default
/// database (no stored function can be reached) nothing changes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn stored_functions_named_like_builtins_need_confirmation() -> R {
    for target in targets() {
        let admin = root(&target, None).await?;
        setup(admin.as_ref()).await?;
        let checked = async {
            let session = root(&target, Some(DATABASE)).await?;
            check(&target, session.as_ref(), "root").await?;
            session.close().await?;

            let user = connect(&target, USER, USER_PASSWORD, Some(DATABASE)).await?;
            let readable = user
                .execute("SELECT name FROM mysql.func", &ExecOptions::default())
                .await
                .is_ok();
            assert!(!readable, "{}: the user can read mysql.func", target.name);
            check(&target, user.as_ref(), USER).await?;
            user.close().await?;

            // Without a default database no stored function can be reached:
            // the statement runs (or, where `random` is a keyword, as on
            // MySQL 8 and later, is reported with its own syntax error, as
            // the run would give).
            let nowhere = root(&target, None).await?;
            let server = nowhere.server_info().clone();
            let verdict =
                analyze_safety_checked(nowhere.as_ref(), &server, "SELECT random() AS r").await;
            assert!(
                verdict == AnalyzeSafety::ReadOnly
                    || matches!(&verdict, AnalyzeSafety::NeedsConfirmation { warnings }
                        if warnings.iter().any(|w| w.contains("could not be planned"))),
                "{}: without a default database: {verdict:?}",
                target.name
            );
            assert_eq!(
                analyze_safety_checked(nowhere.as_ref(), &server, "SELECT UPPER('x') AS u").await,
                AnalyzeSafety::ReadOnly,
                "{}: without a default database",
                target.name
            );
            nowhere.close().await?;

            remote_tables(&target, admin.as_ref()).await?;

            for table in ["hits", "myhits"] {
                assert_eq!(
                    count(admin.as_ref(), table).await?,
                    0,
                    "{}: a stored function ran ({table})",
                    target.name
                );
            }
            Ok::<(), Box<dyn Error>>(())
        }
        .await;
        cleanup(admin.as_ref()).await?;
        admin.close().await?;
        checked?;
    }
    Ok(())
}
