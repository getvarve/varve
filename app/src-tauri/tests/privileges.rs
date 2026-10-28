//! Privilege events of MySQL and MariaDB scripts (audit finding PC-1): a
//! `privilege.granted` event is never logged for a grant the server did
//! not make, whatever the script looks like.
//!
//! Needs MySQL 8.0, 8.4 and 9.7 and MariaDB 10.6 and 12.3 from
//! `dev/docker-compose.yml`, reached as `root`. Ports come from
//! `VARVE_PRIV_MYSQL80_PORT` (33080), `VARVE_PRIV_MYSQL84_PORT` (33084),
//! `VARVE_PRIV_MYSQL97_PORT` (33097), `VARVE_PRIV_MARIADB106_PORT` (33106)
//! and `VARVE_PRIV_MARIADB123_PORT` (33123), the host from
//! `VARVE_PRIV_HOST`, the root password from `VARVE_MYSQL_ROOT_PASSWORD`.
//! `VARVE_ONLY_SERVERS` (compose service names such as `mysql-9.7,mariadb-12.3`)
//! limits the run to those servers; CI uses it to test only the newest versions.
//! Run with `cargo test -p varve-app --test privileges -- --include-ignored`.

use std::sync::Arc;

use varve_activity::ActivityFilter;
use varve_app::commands::{SecretInput, Services};
use varve_app::sessions::SessionInfo;
use varve_core::Engine;
use varve_records::{ConnectionInput, Environment};
use varve_secrets::MemoryStore;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    services: Arc<Services>,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(name: &str) -> Res<Fixture> {
    let dir = std::env::temp_dir().join(format!("varve-priv-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let services = varve_app::build_services(store, &dir, &host, Arc::new(MemoryStore::default()))?;
    Ok(Fixture { services, dir })
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
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

/// The servers to test (see [`selected`]), never none.
fn targets() -> Vec<(&'static str, Engine, u16)> {
    let targets: Vec<(&'static str, Engine, u16)> = every_target()
        .into_iter()
        .filter(|(name, _, _)| selected(&name.to_lowercase().replace(' ', "-")))
        .collect();
    assert!(
        !targets.is_empty(),
        "VARVE_ONLY_SERVERS selects no MySQL or MariaDB server"
    );
    targets
}

fn every_target() -> [(&'static str, Engine, u16); 5] {
    [
        (
            "MySQL 8.0",
            Engine::Mysql,
            port("VARVE_PRIV_MYSQL80_PORT", 33080),
        ),
        (
            "MySQL 8.4",
            Engine::Mysql,
            port("VARVE_PRIV_MYSQL84_PORT", 33084),
        ),
        (
            "MySQL 9.7",
            Engine::Mysql,
            port("VARVE_PRIV_MYSQL97_PORT", 33097),
        ),
        (
            "MariaDB 10.6",
            Engine::Mariadb,
            port("VARVE_PRIV_MARIADB106_PORT", 33106),
        ),
        (
            "MariaDB 12.3",
            Engine::Mariadb,
            port("VARVE_PRIV_MARIADB123_PORT", 33123),
        ),
    ]
}

async fn open(s: &Services, name: &str, engine: Engine, port: u16) -> Res<SessionInfo> {
    let profile = s.create_connection(&ConnectionInput {
        folder_id: None,
        name: format!("{name} privileges"),
        color: None,
        engine,
        host: env_or("VARVE_PRIV_HOST", "127.0.0.1"),
        port: Some(port),
        database: None,
        username: Some("root".into()),
        environment: Environment::Development,
        tls: Default::default(),
        ssh: Default::default(),
        options: Default::default(),
    })?;
    let secret: SecretInput = serde_json::from_value(serde_json::Value::String(env_or(
        "VARVE_MYSQL_ROOT_PASSWORD",
        "varve_dev",
    )))?;
    Ok(s.open_session(&profile.id, Some(secret), None, None)
        .await?)
}

/// The database names (`pc_prop_<n>`) in every `privilege.granted` event of
/// the connection.
fn logged_grants(s: &Services, connection_id: &str) -> Res<Vec<String>> {
    let page = s.activity(&ActivityFilter {
        connection_id: Some(connection_id.to_owned()),
        limit: 5000,
        ..Default::default()
    })?;
    let mut names = Vec::new();
    for event in page
        .events
        .iter()
        .filter(|e| e.action == "privilege.granted")
    {
        let text = event.details.to_string();
        let found: Vec<String> = text
            .match_indices("pc_prop_")
            .filter_map(|(at, _)| {
                let digits: String = text[at + 8..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                // `pc_prop_v` (the user) and `pc_prop_db..` are no grant
                // targets.
                (!digits.is_empty()).then(|| format!("pc_prop_{digits}"))
            })
            .collect();
        assert!(!found.is_empty(), "a grant event names no database: {text}");
        names.extend(found);
    }
    Ok(names)
}

/// A small deterministic generator (a linear congruential one), so a
/// failure can be replayed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from((self.0 >> 33) % n as u64).unwrap_or(0)
    }
}

/// One script fragment granting on database `pc_prop_<n>` to `{user}` in
/// some way, or not granting at all.
fn fragment(kind: usize, n: usize, mariadb: bool) -> String {
    let grant = format!("GRANT SELECT ON pc_prop_{n}.* TO {{user}}");
    match kind {
        0 => grant,
        1 => format!("CREATE PROCEDURE pc_prop_p{n}() IF FALSE THEN DO 0; {grant}; END IF"),
        2 => format!(
            "CREATE PROCEDURE pc_prop_p{n}() BEGIN DECLARE CONTINUE HANDLER FOR SQLEXCEPTION \
             BEGIN END; IF 0 THEN {grant}; END IF; END"
        ),
        3 => format!(
            "CREATE TRIGGER pc_prop_tr{n} BEFORE INSERT ON pc_prop_t FOR EACH ROW \
             CASE WHEN NEW.x > 1 THEN SET NEW.x = 1; ELSE SET NEW.x = 0; END CASE; {grant}"
        ),
        4 => format!("SELECT 'a\\'' AS c; {grant}; SELECT '"),
        5 => format!("/*!99999 {grant} */"),
        6 => format!("/*M! {grant} */"),
        7 if mariadb => format!("IF 0 THEN DO 0; {grant}; END IF"),
        7 => format!("SELECT \"a\\\"; {grant}; SELECT \""),
        8 => "SELECT * FROM pc_prop_missing.nothing".into(),
        9 => "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES')".into(),
        10 => "SET SESSION sql_mode = CONCAT(@@sql_mode, ',ANSI_QUOTES')".into(),
        11 => "START TRANSACTION".into(),
        12 => "/*!ROLLBACK*/".into(),
        13 => ";".into(),
        14 => "SELECT CASE WHEN 1 THEN REPEAT('x;', 2) END AS `begin`".into(),
        15 => format!(
            "CREATE PROCEDURE pc_prop_e{n}() BEGIN SELECT end, start FROM pc_prop_t WHERE end > 0; \
             {grant}; END"
        ),
        16 => "/* only a comment */".into(),
        17 => format!("/*!50000 {grant} */"),
        _ => format!("DO IF(1, 2, 3); {grant}"),
    }
}

/// A plain client connection: the baseline, which sends every script as
/// one multi-statement request.
async fn plain(port: u16) -> Res<mysql_async::Conn> {
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname(env_or("VARVE_PRIV_HOST", "127.0.0.1"))
        .tcp_port(port)
        .user(Some("root"))
        .pass(Some(env_or("VARVE_MYSQL_ROOT_PASSWORD", "varve_dev")));
    Ok(mysql_async::Conn::new(opts).await?)
}

/// Sends `sql` as one request and reads every result, ignoring errors (the
/// end state is compared).
async fn plain_run(conn: &mut mysql_async::Conn, sql: &str) {
    use mysql_async::prelude::Queryable;
    if let Ok(mut query) = conn.query_iter(sql).await {
        while !query.is_empty() {
            if query.collect::<mysql_async::Row>().await.is_err() {
                break;
            }
        }
        let _ = query.drop_result().await;
    }
}

/// `SHOW GRANTS FOR user` as sorted lines, with the user name replaced.
async fn grants_of(conn: &mut mysql_async::Conn, user: &str) -> Res<Vec<String>> {
    use mysql_async::prelude::Queryable;
    let mut lines: Vec<String> = conn.query(format!("SHOW GRANTS FOR {user}")).await?;
    for line in &mut lines {
        *line = line.replace(user, "USER");
    }
    lines.sort();
    Ok(lines)
}

/// Every grant a `privilege.granted` event names is a grant the server has,
/// and Varve leaves the same grants as one plain multi-statement request of
/// the same script: scripts mixing compound statements written without
/// DELIMITER, sql_mode changes, executable comments, empty statements,
/// `end` as a column and failures.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MySQL and MariaDB servers (see the module documentation)"]
async fn no_privilege_event_without_the_grant() -> Res<()> {
    let f = fixture("property")?;
    let s = &f.services;
    for (name, engine, port) in targets() {
        let session = open(s, name, engine, port).await?;
        let run = |sql: String| {
            let id = session.id.clone();
            async move { s.execute(&id, &sql, true, None).await }
        };
        let mut base = plain(port).await?;
        for (db, user) in [("pc_prop_dbv", "pc_prop_v"), ("pc_prop_dbb", "pc_prop_b")] {
            let setup = format!(
                "DROP USER IF EXISTS {user}; CREATE USER {user} IDENTIFIED BY 'x'; \
                 DROP DATABASE IF EXISTS {db}; CREATE DATABASE {db}; \
                 CREATE TABLE {db}.pc_prop_t (x int, `end` int, `start` int)"
            );
            plain_run(&mut base, &setup).await;
        }
        run("USE pc_prop_dbv".into()).await?;
        plain_run(&mut base, "USE pc_prop_dbb").await;
        // The seed depends on the server, not on the port it is reached
        // at, so every environment runs the same scripts.
        let seed = match name {
            "MySQL 8.0" => 33_080,
            "MySQL 8.4" => 33_084,
            "MySQL 9.7" => 33_097,
            "MariaDB 10.6" => 33_106,
            _ => 33_123,
        };
        let mut rng = Rng(0x5eed + seed);
        let mut n = 0;
        let mut logged_ok = 0;
        // Known cases first: the CI failure on MySQL 9.7 (a trigger, then a
        // grant in an executable comment that does not run), then random
        // mixtures.
        let mut fixed = vec![
            vec![fragment(3, 1001, false), fragment(5, 1002, false)],
            vec![
                fragment(3, 1003, false),
                fragment(5, 1004, false),
                fragment(0, 1005, false),
            ],
            vec![
                fragment(5, 1006, false),
                fragment(13, 0, false),
                fragment(0, 1007, false),
            ],
        ]
        .into_iter();
        for _ in 0..43 {
            let script: Vec<String> = match fixed.next() {
                Some(parts) => parts,
                None => {
                    let parts = 2 + rng.below(4);
                    (0..parts)
                        .map(|_| {
                            n += 1;
                            fragment(rng.below(19), n, engine == Engine::Mariadb)
                        })
                        .collect()
                }
            };
            let script = script.join(if rng.below(2) == 0 { "; " } else { ";\n" });
            let _ = run(script.replace("{user}", "pc_prop_v")).await;
            plain_run(&mut base, &script.replace("{user}", "pc_prop_b")).await;
            // Back to a known state for the next script.
            let _ = run("ROLLBACK".into()).await;
            run("SET SESSION sql_mode = DEFAULT".into()).await?;
            plain_run(&mut base, "ROLLBACK; SET SESSION sql_mode = DEFAULT").await;
            let varve = grants_of(&mut base, "pc_prop_v").await?;
            let baseline = grants_of(&mut base, "pc_prop_b").await?;
            assert_eq!(
                varve, baseline,
                "{name}: Varve and a plain request left different grants after:\n{script}"
            );
            let shown = varve.join("\n");
            for logged in logged_grants(s, &session.connection_id)? {
                assert!(
                    shown.contains(&format!("`{logged}`")),
                    "{name}: {logged} was logged as granted but SHOW GRANTS says {shown}\n\
                     after the script:\n{script}"
                );
                logged_ok += 1;
            }
        }
        assert!(logged_ok > 0, "{name}: no grant was logged at all");
        plain_run(
            &mut base,
            "DROP DATABASE pc_prop_dbv; DROP DATABASE pc_prop_dbb; \
             DROP USER pc_prop_v; DROP USER pc_prop_b",
        )
        .await;
        base.disconnect().await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}

/// How many `privilege.unverified` events the connection has.
fn unverified(s: &Services, connection_id: &str) -> Res<usize> {
    let page = s.activity(&ActivityFilter {
        connection_id: Some(connection_id.to_owned()),
        limit: 5000,
        ..Default::default()
    })?;
    Ok(page
        .events
        .iter()
        .filter(|e| e.action == "privilege.unverified")
        .count())
}

/// Every CALL that ran (whatever the procedure does, and also one that
/// failed) is one `privilege.unverified` event per script, and so is every
/// grant table write and `FLUSH PRIVILEGES` (audit round 4): whenever the
/// grants of the test user change, an event says so.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MySQL and MariaDB servers (see the module documentation)"]
async fn calls_and_grant_table_writes_are_always_in_doubt() -> Res<()> {
    let f = fixture("calls")?;
    let s = &f.services;
    for (name, engine, port) in targets() {
        let session = open(s, name, engine, port).await?;
        let run = |sql: &str| {
            let (id, sql) = (session.id.clone(), sql.to_owned());
            async move { s.execute(&id, &sql, true, None).await }
        };
        let mut other = plain(port).await?;
        for db in ["pc_call", "pc_call2", "pc_call_t"] {
            run(&format!("DROP DATABASE IF EXISTS {db}")).await?;
            run(&format!("CREATE DATABASE {db}")).await?;
        }
        run("USE pc_call").await?;
        run("DROP USER IF EXISTS pc_call_u").await?;
        run("CREATE USER pc_call_u IDENTIFIED BY 'x'").await?;
        run("CREATE PROCEDURE noop() SELECT 1").await?;
        run("CREATE PROCEDURE grantp() GRANT SELECT ON pc_call.* TO pc_call_u").await?;
        run("CREATE PROCEDURE grantq() GRANT INSERT ON pc_call.* TO pc_call_u").await?;
        run(
            "CREATE PROCEDURE dynp() BEGIN SET @q = 'GRANT UPDATE ON pc_call.* TO pc_call_u'; \
             PREPARE s FROM @q; EXECUTE s; END",
        )
        .await?;
        run("CREATE PROCEDURE pc_call2.p() GRANT DELETE ON pc_call.* TO pc_call_u").await?;
        // (script, whether it may fail): each must add exactly one
        // unverified event, and the grants of pc_call_u may only change
        // with one.
        let cases: Vec<(String, bool)> = vec![
            ("CALL noop()".into(), false),
            ("CALL noop(); CALL noop()".into(), false),
            ("CALL grantp()".into(), false),
            ("CALL dynp()".into(), false),
            ("CALL missing()".into(), true),
            ("SELECT 1; /*!50000 CALL grantq() */".into(), false),
            // The procedure another database names after USE.
            ("USE pc_call2; CALL p(); USE pc_call".into(), false),
            // Redefined in this session, and by another session.
            (
                "DROP PROCEDURE noop; \
                 CREATE PROCEDURE noop() GRANT ALTER ON pc_call.* TO pc_call_u; CALL noop()"
                    .into(),
                false,
            ),
            ("@other: redefine noop".into(), false),
            ("CALL noop()".into(), false),
            // Grant table writes, qualified, then after USE mysql (in the
            // same script and in an earlier one).
            (
                "INSERT INTO mysql.db (Host, Db, User, Select_priv) \
                 VALUES ('%', 'pc_call_t', 'pc_call_u', 'Y'); FLUSH PRIVILEGES"
                    .into(),
                false,
            ),
            (
                "USE mysql; UPDATE db SET Insert_priv = 'Y' \
                 WHERE User = 'pc_call_u' AND Db = 'pc_call_t'; FLUSH PRIVILEGES"
                    .into(),
                false,
            ),
            ("USE pc_call".into(), false),
            (
                "UPDATE mysql.db SET Update_priv = 'Y' \
                 WHERE User = 'pc_call_u' AND Db = 'pc_call_t'"
                    .into(),
                false,
            ),
            ("FLUSH PRIVILEGES".into(), false),
            ("USE mysql".into(), false),
            (
                "DELETE FROM db WHERE User = 'pc_call_u' AND Db = 'pc_call_t'".into(),
                false,
            ),
            ("FLUSH PRIVILEGES".into(), false),
        ];
        for (sql, may_fail) in cases {
            let grants = grants_of(&mut other, "pc_call_u").await?;
            let before = unverified(s, &session.connection_id)?;
            if sql == "@other: redefine noop" {
                plain_run(
                    &mut other,
                    "DROP PROCEDURE pc_call.noop; CREATE PROCEDURE pc_call.noop() \
                     GRANT DROP ON pc_call.* TO pc_call_u",
                )
                .await;
                continue;
            }
            let result = run(&sql).await;
            assert!(may_fail || result.is_ok(), "{name}: {sql}: {result:?}");
            let after = unverified(s, &session.connection_id)?;
            let silent = matches!(sql.as_str(), "USE pc_call" | "USE mysql");
            assert_eq!(
                after,
                before + usize::from(!silent),
                "{name}: unverified events after {sql}"
            );
            if silent {
                assert_eq!(
                    grants_of(&mut other, "pc_call_u").await?,
                    grants,
                    "{name}: {sql} changed grants without an event"
                );
            }
        }
        // The cases above did change the grants.
        let grants = grants_of(&mut other, "pc_call_u").await?.join("\n");
        for privilege in ["SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "DROP"] {
            assert!(
                grants.contains(privilege),
                "{name}: {privilege} in {grants}"
            );
        }
        run("USE pc_call").await?;
        for db in ["pc_call", "pc_call2", "pc_call_t"] {
            run(&format!("DROP DATABASE {db}")).await?;
        }
        run("DROP USER pc_call_u").await?;
        other.disconnect().await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}
