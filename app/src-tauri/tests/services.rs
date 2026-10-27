//! End-to-end tests of the services the UI calls, against real servers.
//!
//! Needs `dev/docker-compose.yml --profile latest` (PostgreSQL 18, MySQL 9.7,
//! MariaDB 12.3, Redis 8.10) and, for the tunnel test, the compose project in
//! `crates/tunnel/tests/docker-compose.yml`. Run with
//! `cargo test -p varve-app --test services -- --include-ignored`.
//!
//! Endpoints come from environment variables with the defaults of
//! `dev/docker-compose.yml`: `VARVE_SERVICES_HOST` (`127.0.0.1`),
//! `VARVE_SERVICES_PG18_PORT` (54318), `VARVE_SERVICES_MYSQL97_PORT` (33097),
//! `VARVE_SERVICES_MARIADB123_PORT` (33123), `VARVE_SERVICES_REDIS810_PORT`
//! (63081) and, for the tunnel test, `VARVE_SERVICES_SSH_PORT` (22022).

use std::sync::Arc;

use varve_activity::{ActivityFilter, Category};
use varve_app::commands::{CmdError, SecretInput, Services, TestTarget};
use varve_app::sessions::SessionError;
use varve_core::{Edition, Engine};
use varve_driver::{StatementResult, Value};
use varve_records::{ConnectionInput, Environment, SecretKindName, SshAuthMethod, SshSettings};
use varve_secrets::MemoryStore;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    services: Arc<Services>,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(name: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("varve-services-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let services = varve_app::build_services(
        store,
        &dir,
        &host,
        Edition::Community,
        Arc::new(MemoryStore::default()),
    )?;
    Ok(Fixture { services, dir })
}

fn host() -> String {
    std::env::var("VARVE_SERVICES_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

fn port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn pg18_port() -> u16 {
    port("VARVE_SERVICES_PG18_PORT", 54318)
}

fn redis810_port() -> u16 {
    port("VARVE_SERVICES_REDIS810_PORT", 63081)
}

fn secret(value: &str) -> Result<SecretInput, serde_json::Error> {
    serde_json::from_value(serde_json::Value::String(value.into()))
}

fn input(name: &str, engine: Engine, port: u16, environment: Environment) -> ConnectionInput {
    ConnectionInput {
        folder_id: None,
        name: name.into(),
        color: None,
        engine,
        host: host(),
        port: Some(port),
        database: if engine == Engine::Redis {
            None
        } else {
            Some("varve".into())
        },
        username: if engine == Engine::Redis {
            None
        } else {
            Some("varve".into())
        },
        environment,
        tls: Default::default(),
        ssh: Default::default(),
        options: Default::default(),
    }
}

fn first_value(output: &varve_driver::ExecOutput) -> Option<Value> {
    output.results.iter().find_map(|r| match r {
        StatementResult::Rows { rows, .. } => rows.first().and_then(|row| row.first()).cloned(),
        StatementResult::Command { .. } => None,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix (profile latest)"]
async fn every_engine_connects_and_runs_queries() -> TestResult {
    let f = fixture("engines")?;
    let s = &f.services;
    for (name, engine, port, sql, expected) in [
        (
            "PG 18",
            Engine::Postgres,
            pg18_port(),
            "select 40 + 2",
            Value::Int(42),
        ),
        (
            "MySQL 9.7",
            Engine::Mysql,
            port("VARVE_SERVICES_MYSQL97_PORT", 33097),
            "select 40 + 2",
            Value::Int(42),
        ),
        (
            "MariaDB 12.3",
            Engine::Mariadb,
            port("VARVE_SERVICES_MARIADB123_PORT", 33123),
            "select 40 + 2",
            Value::Int(42),
        ),
        (
            "Redis 8.10",
            Engine::Redis,
            redis810_port(),
            "PING",
            Value::Null,
        ),
    ] {
        let profile = s.create_connection(&input(name, engine, port, Environment::Development))?;
        let tested = s
            .test_connection(
                TestTarget::Saved {
                    id: profile.id.clone(),
                },
                Some(secret("varve_dev")?),
                None,
                None,
            )
            .await?;
        assert_eq!(tested.server.engine, engine, "{name}");

        // Save the password in the (in-memory) keychain, then open without typing it.
        s.save_secret(
            &profile.id,
            SecretKindName::DbPassword,
            secret("varve_dev")?,
        )?;
        let session = s.open_session(&profile.id, None, None, None).await?;

        // The session says whether it is encrypted, and the server agrees.
        let encryption = &session.encryption;
        assert_eq!(encryption.mode, varve_driver::TlsMode::Prefer, "{name}");
        let tls = encryption
            .tls
            .ok_or_else(|| format!("{name}: TLS unknown"))?;
        let server_says = match engine {
            Engine::Postgres => {
                let out = s
                    .execute(
                        &session.id,
                        "select ssl from pg_stat_ssl where pid = pg_backend_pid()",
                        false,
                        None,
                    )
                    .await?;
                first_value(&out) == Some(Value::Bool(true))
            }
            Engine::Mysql | Engine::Mariadb => {
                let out = s
                    .execute(
                        &session.id,
                        "SHOW SESSION STATUS LIKE 'Ssl_cipher'",
                        false,
                        None,
                    )
                    .await?;
                out.results.iter().any(|r| match r {
                    StatementResult::Rows { rows, .. } => rows
                        .first()
                        .and_then(|row| row.get(1))
                        .is_some_and(|v| matches!(v, Value::Text(t) if !t.is_empty())),
                    StatementResult::Command { .. } => false,
                })
            }
            // The dev Redis listens on plain TCP only.
            Engine::Redis => false,
        };
        assert_eq!(tls, server_says, "{name}: {encryption:?}");
        // Only a Redis password goes as is over plain TCP.
        assert_eq!(
            encryption.password_unencrypted,
            engine == Engine::Redis && !tls,
            "{name}"
        );
        eprintln!("{name}: {encryption:?}");

        let output = s.execute(&session.id, sql, false, None).await?;
        if engine == Engine::Redis {
            assert!(
                matches!(output.results.first(), Some(StatementResult::Command { tag, .. }) if tag == "PONG"),
                "{name}: {output:?}"
            );
        } else {
            assert_eq!(first_value(&output), Some(expected), "{name}");
        }
        s.close_session(&session.id).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix (profile latest)"]
async fn production_needs_confirmation_and_activity_is_redacted() -> TestResult {
    let f = fixture("safety")?;
    let s = &f.services;
    let profile = s.create_connection(&input(
        "Prod",
        Engine::Postgres,
        pg18_port(),
        Environment::Production,
    ))?;
    let session = s
        .open_session(&profile.id, Some(secret("varve_dev")?), None, None)
        .await?;

    s.execute(&session.id, "create temp table t (id int)", false, None)
        .await?;
    let blocked = s.execute(&session.id, "drop table t", false, None).await;
    assert!(
        matches!(
            blocked,
            Err(CmdError::Session {
                error: SessionError::ConfirmationRequired {
                    environment: Environment::Production,
                    ..
                }
            })
        ),
        "{blocked:?}"
    );
    s.execute(&session.id, "drop table t", true, None).await?;

    // A password in a statement must never reach the activity log.
    let leaked = "Sup3rS3cret-pw";
    let _ = s
        .execute(
            &session.id,
            &format!("alter role varve_missing_role with password '{leaked}'"),
            false,
            None,
        )
        .await;

    let history = s.query_history(&profile.id, 50)?;
    assert!(history.events.iter().any(|e| e.action == "query.failed"));
    let page = s.activity(&ActivityFilter {
        connection_id: Some(profile.id.clone()),
        limit: 200,
        ..ActivityFilter::default()
    })?;
    let actions: Vec<&str> = page.events.iter().map(|e| e.action.as_str()).collect();
    for expected in [
        "connection.opened",
        "query.executed",
        "destructive.blocked",
        "destructive.confirmed",
        "query.failed",
    ] {
        assert!(
            actions.contains(&expected),
            "missing {expected} in {actions:?}"
        );
    }
    let dump = serde_json::to_string(&page.events)?;
    assert!(
        !dump.contains(leaked),
        "the password reached the activity log"
    );
    assert!(
        page.events
            .iter()
            .all(|e| e.category != Category::Secrets || e.details.get("value").is_none())
    );

    s.close_session(&session.id).await?;
    let page = s.activity(&ActivityFilter {
        connection_id: Some(profile.id.clone()),
        limit: 200,
        ..ActivityFilter::default()
    })?;
    assert!(page.events.iter().any(|e| e.action == "connection.closed"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dev database matrix (profile latest)"]
async fn redis_key_browser_reads_keys() -> TestResult {
    let f = fixture("redis")?;
    let s = &f.services;
    let mut redis = input(
        "Cache",
        Engine::Redis,
        redis810_port(),
        Environment::Development,
    );
    redis.options.redis_db = 14;
    let profile = s.create_connection(&redis)?;
    let session = s
        .open_session(&profile.id, Some(secret("varve_dev")?), None, None)
        .await?;
    // FLUSHDB is destructive and the option to confirm is on by default.
    let unconfirmed = s.execute(&session.id, "FLUSHDB", false, None).await;
    assert!(matches!(
        unconfirmed,
        Err(CmdError::Session {
            error: SessionError::ConfirmationRequired { .. }
        })
    ));
    s.execute(
        &session.id,
        "FLUSHDB\nHSET session:1 user kim\nSET cart:9 \"three items\"",
        true,
        None,
    )
    .await?;

    let page = s.redis_scan(&session.id, 0, "session:*", None, 100).await?;
    assert_eq!(page.keys.len(), 1);
    let info = s
        .redis_key_info(&session.id, "session:1")
        .await?
        .ok_or("no key info")?;
    assert_eq!(info.length, Some(1));
    let value = s
        .redis_read_value(
            &session.id,
            "cart:9",
            varve_driver_redis::ValuePage::first(10),
        )
        .await?;
    assert!(value.is_some());
    s.execute(&session.id, "FLUSHDB", true, None).await?;
    s.close_session(&session.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the tunnel compose project (crates/tunnel/tests/docker-compose.yml)"]
async fn ssh_tunnel_asks_to_trust_the_host_key_then_connects() -> TestResult {
    let f = fixture("ssh")?;
    let s = &f.services;
    let mut tunnelled = input("Behind bastion", Engine::Redis, 6379, Environment::Staging);
    tunnelled.host = "redis".into();
    tunnelled.ssh = SshSettings {
        enabled: true,
        host: host(),
        port: port("VARVE_SERVICES_SSH_PORT", 22022),
        username: "varve".into(),
        auth: SshAuthMethod::Password,
        key_path: None,
    };
    let profile = s.create_connection(&tunnelled)?;

    // No SSH password saved: the UI is asked for it.
    let missing = s.open_session(&profile.id, None, None, None).await;
    assert!(
        matches!(
            missing,
            Err(CmdError::SecretRequired {
                secret: SecretKindName::SshPassword
            })
        ),
        "{missing:?}"
    );

    // Unknown host key: the UI shows the fingerprint and the user trusts it.
    let unknown = s
        .open_session(&profile.id, None, Some(secret("varve_dev")?), None)
        .await;
    let (presented, host, port) = match unknown {
        Err(CmdError::Tunnel {
            code,
            presented_key: Some(key),
            fingerprints,
            host: Some(host),
            port: Some(port),
            ..
        }) if code == "host_key_unknown" => {
            assert!(
                fingerprints
                    .first()
                    .is_some_and(|f| f.starts_with("SHA256:"))
            );
            (key, host, port)
        }
        other => return Err(format!("expected an unknown host key, got {other:?}").into()),
    };
    s.trust_ssh_host_key(&host, port, &presented).await?;
    assert!(f.dir.join("known_hosts").exists());

    let session = s
        .open_session(&profile.id, None, Some(secret("varve_dev")?), None)
        .await?;
    let output = s.execute(&session.id, "PING", false, None).await?;
    assert!(
        matches!(output.results.first(), Some(StatementResult::Command { tag, .. }) if tag == "PONG")
    );
    // A second session shares the open tunnel.
    let second = s
        .open_session(&profile.id, None, Some(secret("varve_dev")?), None)
        .await?;

    // The missing password was a prompt, not a tunnel failure; the unknown
    // host key was one; the tunnel opened once for both sessions.
    let events =
        |action: &str| -> Result<Vec<varve_activity::ActivityEvent>, Box<dyn std::error::Error>> {
            Ok(s.activity(&ActivityFilter {
                connection_id: Some(profile.id.clone()),
                actions: vec![action.into()],
                ..Default::default()
            })?
            .events)
        };
    let failed = events("tunnel.failed")?;
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].details["error"]["code"], "host_key_unknown");
    assert_eq!(failed[0].details["ssh_username"], "varve");
    assert_eq!(failed[0].details["auth_method"], "password");
    let opened = events("tunnel.opened")?;
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert_eq!(opened[0].details["ssh_port"], profile.ssh.port);
    assert!(!serde_json::to_string(&failed)?.contains("varve_dev"));
    s.close_session(&second.id).await?;
    s.close_session(&session.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the tunnel compose project (crates/tunnel/tests/docker-compose.yml)"]
async fn ssh_tunnel_connect_errors_name_the_target_and_the_bastion() -> TestResult {
    let f = fixture("ssh-stalled")?;
    let s = &f.services;
    // A non-routable address: the bastion never reaches it, so the driver,
    // connected to the tunnel's local end, gets no answer.
    let mut tunnelled = input(
        "Stalled behind bastion",
        Engine::Redis,
        6379,
        Environment::Staging,
    );
    tunnelled.host = "10.255.255.1".into();
    let bastion_port = port("VARVE_SERVICES_SSH_PORT", 22022);
    tunnelled.ssh = SshSettings {
        enabled: true,
        host: host(),
        port: bastion_port,
        username: "varve".into(),
        auth: SshAuthMethod::Password,
        key_path: None,
    };
    let profile = s.create_connection(&tunnelled)?;
    let unknown = s
        .open_session(&profile.id, None, Some(secret("varve_dev")?), None)
        .await;
    match unknown {
        Err(CmdError::Tunnel {
            code,
            presented_key: Some(key),
            host: Some(host),
            port: Some(port),
            ..
        }) if code == "host_key_unknown" => s.trust_ssh_host_key(&host, port, &key).await?,
        other => return Err(format!("expected an unknown host key, got {other:?}").into()),
    }

    let started = std::time::Instant::now();
    let outcome = s
        .open_session(&profile.id, None, Some(secret("varve_dev")?), None)
        .await;
    let elapsed = started.elapsed();
    let error = match outcome {
        Err(CmdError::Session {
            error: SessionError::Driver { error },
        }) => error,
        other => return Err(format!("expected a driver error, got {other:?}").into()),
    };
    eprintln!("tunnelled connect failed after {elapsed:?}: {error:?}");
    let route = format!(
        "10.255.255.1:6379 through SSH bastion {}:{bastion_port}",
        host()
    );
    assert!(error.message.contains(&route), "{}", error.message);
    if error.kind == varve_driver::ErrorKind::Timeout {
        // Nothing answered through the tunnel: neither the plain probe of
        // `prefer` nor the TLS handshake.
        assert!(
            error
                .message
                .starts_with(&format!("{route} did not answer")),
            "{}",
            error.message
        );
    }
    // The detail keeps the local end for troubleshooting; the message
    // names no other 127.0.0.1 port than the bastion's.
    let local = error
        .detail
        .as_deref()
        .ok_or("no detail with the local end")?;
    assert!(local.contains("tunnel's local end"), "{local}");

    // The same for PostgreSQL, MySQL and MariaDB, at once (each waits for
    // the connect timeout). The bastion's host key is trusted by now.
    let mut profiles = Vec::new();
    for (engine, port) in [
        (Engine::Postgres, 5432),
        (Engine::Mysql, 3306),
        (Engine::Mariadb, 3306),
    ] {
        let mut input = tunnelled.clone();
        input.name = format!("Stalled {engine:?} behind bastion");
        input.engine = engine;
        input.port = Some(port);
        input.database = Some("varve".into());
        input.username = Some("varve".into());
        profiles.push((engine, port, s.create_connection(&input)?.id));
    }
    let open = |id: String| async move {
        s.open_session(
            &id,
            Some(secret("varve_dev")?),
            Some(secret("varve_dev")?),
            None,
        )
        .await
        .err()
        .ok_or_else(|| Box::<dyn std::error::Error>::from("connected"))
    };
    let started = std::time::Instant::now();
    let [(e0, p0, id0), (e1, p1, id1), (e2, p2, id2)]: [(Engine, u16, String); 3] =
        profiles.try_into().map_err(|_| "three profiles")?;
    let (r0, r1, r2) = tokio::join!(open(id0), open(id1), open(id2));
    for (engine, port, outcome) in [(e0, p0, r0?), (e1, p1, r1?), (e2, p2, r2?)] {
        let error = match outcome {
            CmdError::Session {
                error: SessionError::Driver { error },
            } => error,
            other => {
                return Err(format!("{engine:?}: expected a driver error, got {other:?}").into());
            }
        };
        eprintln!("{engine:?} through the tunnel failed: {error:?}");
        let route = format!(
            "10.255.255.1:{port} through SSH bastion {}:{bastion_port}",
            host()
        );
        assert!(
            error.message.contains(&route),
            "{engine:?}: {}",
            error.message
        );
        // No address of the tunnel's local end in the message, only in the
        // detail.
        assert!(
            !error.message.replace(&route, "").contains("127.0.0.1"),
            "{engine:?}: {}",
            error.message
        );
        assert!(
            error
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("tunnel's local end 127.0.0.1:")),
            "{engine:?}: {error:?}"
        );
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(25),
        "took {:?}",
        started.elapsed()
    );
    Ok(())
}
