//! The M1 connection and data events of design doc 9.2 that the session
//! events alone do not give: `tls.downgraded`, `tunnel.failed` and
//! `data.exported`. (`tunnel.opened` needs an SSH server and is checked in
//! `tests/services.rs`.)
//!
//! The tests marked ignored need the M2 database matrix: PostgreSQL 18 on
//! `VARVE_M2_PG18_PORT` (default 55518, no TLS), MySQL 8.4 on
//! `VARVE_M2_MYSQL84_PORT` (35084, TLS on) and Redis 8.10 on
//! `VARVE_M2_REDIS810_PORT` (64081, no TLS), host `VARVE_M2_HOST`
//! (`127.0.0.1`). Run them with
//! `cargo test -p varve-app --test connection_events -- --include-ignored`.

use std::net::TcpListener;
use std::sync::Arc;

use varve_activity::{ActivityEvent, ActivityFilter};
use varve_app::commands::{CmdError, ExportRecord, SecretInput, Services, TestTarget};
use varve_core::Engine;
use varve_driver::TlsMode;
use varve_records::{
    ConnectionInput, ConnectionOptions, Environment, SshAuthMethod, SshSettings, TlsSettings,
};
use varve_secrets::MemoryStore;

type Res<T> = Result<T, Box<dyn std::error::Error>>;
type TestResult = Res<()>;

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
    let dir = std::env::temp_dir().join(format!(
        "varve-connection-events-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let services = varve_app::build_services(store, &dir, &host, Arc::new(MemoryStore::default()))?;
    Ok(Fixture { services, dir })
}

fn host() -> String {
    std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn secret(value: &str) -> Res<SecretInput> {
    Ok(serde_json::from_value(serde_json::Value::String(
        value.into(),
    ))?)
}

fn input(name: &str, engine: Engine, port: u16, tls: TlsMode) -> ConnectionInput {
    let (database, username) = match engine {
        Engine::Postgres => (Some("varve".to_string()), Some("varve".to_string())),
        Engine::Mysql | Engine::Mariadb => (None, Some("root".to_string())),
        Engine::Redis => (None, None),
    };
    ConnectionInput {
        folder_id: None,
        name: name.into(),
        color: None,
        engine,
        host: host(),
        port: Some(port),
        database,
        username,
        environment: Environment::Development,
        tls: TlsSettings {
            mode: tls,
            ..TlsSettings::default()
        },
        ssh: SshSettings::default(),
        options: ConnectionOptions {
            confirm_destructive: false,
            ..Default::default()
        },
    }
}

fn events(s: &Services, connection_id: &str, action: &str) -> Res<Vec<ActivityEvent>> {
    Ok(s.activity(&ActivityFilter {
        connection_id: Some(connection_id.to_string()),
        actions: vec![action.into()],
        ..Default::default()
    })?
    .events)
}

/// `prefer` that ends without TLS records `tls.downgraded` once per user
/// connect (not again for the metadata and admin sessions that follow),
/// with whether the password went unencrypted; `prefer` that got TLS, and
/// `disable`, record nothing. A test connection records it only when the
/// password crossed in clear.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn prefer_without_tls_is_recorded_as_a_downgrade() -> TestResult {
    let f = fixture("downgrade")?;
    let s = &f.services;
    let cases = [
        (
            "pg prefer",
            input(
                "pg",
                Engine::Postgres,
                port("VARVE_M2_PG18_PORT", 55518),
                TlsMode::Prefer,
            ),
            Some(false),
        ),
        (
            "redis prefer",
            input(
                "redis",
                Engine::Redis,
                port("VARVE_M2_REDIS810_PORT", 64081),
                TlsMode::Prefer,
            ),
            // Redis AUTH on plain TCP sends the password as is.
            Some(true),
        ),
        (
            "mysql prefer",
            input(
                "mysql",
                Engine::Mysql,
                port("VARVE_M2_MYSQL84_PORT", 35084),
                TlsMode::Prefer,
            ),
            None,
        ),
        (
            "pg disable",
            input(
                "pg-off",
                Engine::Postgres,
                port("VARVE_M2_PG18_PORT", 55518),
                TlsMode::Disable,
            ),
            None,
        ),
    ];
    for (label, input, downgraded) in cases {
        let profile = s.create_connection(&input)?;
        let session = s
            .open_session(&profile.id, Some(secret("varve_dev")?), None, None)
            .await?;
        // The metadata and admin sessions reach the server the same way.
        if profile.engine == Engine::Redis {
            s.redis_info(&session.id).await?;
        } else {
            s.catalog_databases(&session.id).await?;
            s.process_list(&session.id).await?;
        }
        let recorded = events(s, &profile.id, "tls.downgraded")?;
        match downgraded {
            Some(password_unencrypted) => {
                assert_eq!(recorded.len(), 1, "{label}: once per connect: {recorded:?}");
                let event = &recorded[0];
                assert_eq!(event.details["tls"], false, "{label}");
                assert_eq!(event.details["tls_mode"], "prefer", "{label}");
                assert_eq!(
                    event.details["password_unencrypted"], password_unencrypted,
                    "{label}"
                );
            }
            None => assert!(recorded.is_empty(), "{label}: {recorded:?}"),
        }
        s.close_session(&session.id).await?;

        // Test connection: only when the password crossed in clear.
        s.test_connection(
            TestTarget::Saved {
                id: profile.id.clone(),
            },
            Some(secret("varve_dev")?),
            None,
            None,
        )
        .await?;
        let tested: Vec<_> = events(s, &profile.id, "tls.downgraded")?
            .into_iter()
            .filter(|e| e.details.get("test_connection").is_some())
            .collect();
        let expected = usize::from(downgraded == Some(true));
        assert_eq!(tested.len(), expected, "{label} test: {tested:?}");
        if let Some(event) = tested.first() {
            assert_eq!(event.details["password_unencrypted"], true, "{label}");
        }
        s.delete_connection(&profile.id).await?;
    }
    Ok(())
}

/// A bastion that cannot be reached records `tunnel.failed` with the code
/// and the SSH details, never the password; a missing password is a prompt
/// and records nothing. No server is needed.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_bastion_records_tunnel_failed() -> TestResult {
    let f = fixture("tunnel")?;
    let s = &f.services;
    let closed = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let mut tunnelled = input("tunnelled", Engine::Postgres, 5432, TlsMode::Prefer);
    tunnelled.ssh = SshSettings {
        enabled: true,
        host: "127.0.0.1".into(),
        port: closed,
        username: "ops".into(),
        auth: SshAuthMethod::Password,
        key_path: None,
    };
    let profile = s.create_connection(&tunnelled)?;

    let missing = s.open_session(&profile.id, None, None, None).await.err();
    assert!(
        matches!(missing, Some(CmdError::SecretRequired { .. })),
        "{missing:?}"
    );
    assert!(events(s, &profile.id, "tunnel.failed")?.is_empty());

    let error = s
        .open_session(&profile.id, None, Some(secret("Bastion-Pass-1")?), None)
        .await
        .err();
    assert!(
        matches!(&error, Some(CmdError::Tunnel { code, .. }) if code == "connect"),
        "{error:?}"
    );
    let failed = events(s, &profile.id, "tunnel.failed")?;
    assert_eq!(failed.len(), 1, "{failed:?}");
    let event = &failed[0];
    assert_eq!(event.details["ssh_host"], "127.0.0.1");
    assert_eq!(event.details["ssh_port"], closed);
    assert_eq!(event.details["ssh_username"], "ops");
    assert_eq!(event.details["auth_method"], "password");
    assert_eq!(event.details["error"]["code"], "connect");
    assert!(!serde_json::to_string(event)?.contains("Bastion-Pass-1"));
    assert!(events(s, &profile.id, "tunnel.opened")?.is_empty());
    Ok(())
}

/// `record_export` writes `data.exported` with the file name only (never
/// the path), the format, size and rows, linked to the connection.
#[tokio::test(flavor = "multi_thread")]
async fn result_exports_are_recorded_without_the_path() -> TestResult {
    let f = fixture("export")?;
    let s = &f.services;
    let profile = s.create_connection(&input("pg", Engine::Postgres, 5432, TlsMode::Prefer))?;
    s.record_export(&ExportRecord {
        connection_id: Some(profile.id.clone()),
        objects: vec!["Orders query".into()],
        file_name: "/Users/someone/private/orders.csv".into(),
        format: "CSV".into(),
        size_bytes: Some(2048),
        rows: Some(17),
    })?;
    let exported = events(s, &profile.id, "data.exported")?;
    assert_eq!(exported.len(), 1, "{exported:?}");
    let event = &exported[0];
    assert_eq!(event.details["file_name"], "orders.csv");
    assert_eq!(event.details["format"], "csv");
    assert_eq!(event.details["size_bytes"], 2048);
    assert_eq!(event.details["rows"], 17);
    assert_eq!(event.details["objects"][0], "Orders query");
    assert!(!serde_json::to_string(event)?.contains("/Users/someone"));

    // A connection that is gone is not linked; bad input is refused.
    s.record_export(&ExportRecord {
        connection_id: Some("gone".into()),
        objects: vec![],
        file_name: "keys.csv".into(),
        format: "csv".into(),
        size_bytes: None,
        rows: Some(3),
    })?;
    for (file_name, format) in [("", "csv"), ("  ", "json"), ("x.xlsx", "xlsx")] {
        let refused = s.record_export(&ExportRecord {
            connection_id: None,
            objects: vec![],
            file_name: file_name.into(),
            format: format.into(),
            size_bytes: None,
            rows: None,
        });
        assert!(
            matches!(refused, Err(CmdError::Invalid { .. })),
            "{file_name} {format}: {refused:?}"
        );
    }
    let all = s.activity(&ActivityFilter {
        actions: vec!["data.exported".into()],
        ..Default::default()
    })?;
    assert_eq!(all.events.len(), 2);
    Ok(())
}
