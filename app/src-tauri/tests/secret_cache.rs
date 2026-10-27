//! How often the keychain is asked for a connection's secrets.
//!
//! A keychain double counts every read that reaches it. The cache in front
//! of it (`varve_secrets::SecretCache`) must make one connection cost one
//! read per secret kind for the user session, the metadata session, the
//! admin session and the SSH tunnel together, and forget a profile's
//! secrets when it is edited, deleted, or its secrets change.
//!
//! The tests that connect need the M2 database matrix (PostgreSQL 18 on
//! `VARVE_M2_PG18_PORT`, default 55518, and MySQL 8.4 on
//! `VARVE_M2_MYSQL84_PORT`, default 35084; host `VARVE_M2_HOST`, default
//! `127.0.0.1`). Run them with
//! `cargo test -p varve-app --test secret_cache -- --include-ignored`.

use std::net::TcpListener;
use std::sync::Arc;

use varve_activity::ActivityFilter;
use varve_app::commands::{CmdError, SecretInput, Services, TestTarget};
use varve_app::sessions::SessionError;
use varve_core::{Edition, Engine};
use varve_driver::ErrorKind;
use varve_driver::tls_key::{PASSPHRASE_REQUIRED, PASSPHRASE_WRONG};
use varve_records::{
    ConnectionInput, ConnectionOptions, Environment, FolderDeleteMode, SecretKindName,
    SshAuthMethod, SshSettings, TlsSettings,
};
use varve_secrets::{CountingStore, MemoryStore, Secret, SecretKey, SecretKind, SecretStore};

type Res<T> = Result<T, Box<dyn std::error::Error>>;
type TestResult = Res<()>;

/// The keychain double behind the services.
type Keychain = Arc<CountingStore<MemoryStore>>;

struct Fixture {
    services: Arc<Services>,
    keychain: Keychain,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(name: &str) -> Res<Fixture> {
    let dir =
        std::env::temp_dir().join(format!("varve-secret-cache-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let keychain = Arc::new(CountingStore::new(MemoryStore::default()));
    let services = services_on(&dir, &keychain)?;
    Ok(Fixture {
        services,
        keychain,
        dir,
    })
}

/// Services on a data directory with a keychain double, as after an app
/// (re)start: the cache starts empty, the stored settings apply.
fn services_on(dir: &std::path::Path, keychain: &Keychain) -> Res<Arc<Services>> {
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(dir, &host)?);
    Ok(varve_app::build_services(
        store,
        dir,
        &host,
        Edition::Community,
        Arc::clone(keychain) as Arc<dyn SecretStore>,
    )?)
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

fn input(name: &str, engine: Engine, port: u16) -> ConnectionInput {
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
        tls: Default::default(),
        ssh: Default::default(),
        options: ConnectionOptions {
            confirm_destructive: false,
            ..Default::default()
        },
    }
}

/// A profile whose SSH bastion is a closed local port: the tunnel reads
/// its secret and then fails to connect, without any server.
fn ssh_input(name: &str) -> Res<ConnectionInput> {
    // Bound and released: nothing listens there afterwards.
    let closed = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let mut input = input(name, Engine::Postgres, 5432);
    input.ssh = SshSettings {
        enabled: true,
        host: "127.0.0.1".into(),
        port: closed,
        username: "ops".into(),
        auth: SshAuthMethod::Password,
        key_path: None,
    };
    Ok(input)
}

fn key(profile_id: &str, kind: SecretKind) -> Res<SecretKey> {
    Ok(SecretKey::new(profile_id.to_string(), kind)?)
}

/// How many logins of a connection the server or bastion refused, from
/// the activity log.
fn refused_logins(s: &Services, connection_id: &str) -> Res<usize> {
    let page = s.activity(&ActivityFilter {
        connection_id: Some(connection_id.to_string()),
        actions: vec!["connection.failed".into()],
        ..Default::default()
    })?;
    Ok(page.events.len())
}

fn targets() -> [(Engine, u16); 2] {
    [
        (Engine::Postgres, port("VARVE_M2_PG18_PORT", 55518)),
        (Engine::Mysql, port("VARVE_M2_MYSQL84_PORT", 35084)),
    ]
}

/// One connection with its user, metadata and admin sessions costs one
/// keychain read of the password; a second user session costs none.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn one_connection_reads_the_keychain_once() -> TestResult {
    for (engine, port) in targets() {
        let f = fixture(&format!("once-{engine:?}"))?;
        let s = &f.services;
        let profile = s.create_connection(&input("once", engine, port))?;
        s.save_secret(
            &profile.id,
            SecretKindName::DbPassword,
            secret("varve_dev")?,
        )?;
        assert_eq!(f.keychain.writes(), 1);
        // As after a restart: the secret was saved earlier and is not
        // cached yet.
        let s = services_on(&f.dir, &f.keychain)?;
        let before = f.keychain.reads();

        let user = s.open_session(&profile.id, None, None, None).await?;
        assert_eq!(f.keychain.reads() - before, 1, "{engine:?}: user session");
        s.catalog_databases(&user.id).await?;
        assert_eq!(
            f.keychain.reads() - before,
            1,
            "{engine:?}: metadata session"
        );
        s.process_list(&user.id).await?;
        assert_eq!(f.keychain.reads() - before, 1, "{engine:?}: admin session");
        let second = s.open_session(&profile.id, None, None, None).await?;
        assert_eq!(
            f.keychain.reads() - before,
            1,
            "{engine:?}: second user session"
        );
        assert_eq!(s.secret_cache.hits(), 3);

        // Remembering off: every session reads the keychain again.
        s.set_secret_settings(varve_app::settings::SecretSettings {
            remember_for_session: false,
        })?;
        assert!(s.secret_cache.is_empty());
        let third = s.open_session(&profile.id, None, None, None).await?;
        assert_eq!(f.keychain.reads() - before, 2, "{engine:?}: not remembered");
        assert!(s.secret_cache.is_empty());

        for id in [user.id, second.id, third.id] {
            s.close_session(&id).await?;
        }
        s.delete_connection(&profile.id).await?;
    }
    Ok(())
}

/// A password changed in the keychain after it was cached (another Varve
/// build, Keychain Access) is read again once when the server refuses the
/// cached one, and the connection succeeds.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn a_refused_cached_password_is_read_again() -> TestResult {
    for (engine, port) in targets() {
        let f = fixture(&format!("stale-{engine:?}"))?;
        let s = &f.services;
        let profile = s.create_connection(&input("stale", engine, port))?;
        s.save_secret(&profile.id, SecretKindName::DbPassword, secret("wrong")?)?;
        // Changed elsewhere: the cache still holds "wrong".
        f.keychain.inner().set(
            &key(&profile.id, SecretKind::DbPassword)?,
            &Secret::new("varve_dev".into()),
        )?;
        let before = f.keychain.reads();
        let user = s.open_session(&profile.id, None, None, None).await?;
        assert_eq!(f.keychain.reads() - before, 1, "{engine:?}: one fresh read");
        // Cached again from the fresh read.
        s.catalog_databases(&user.id).await?;
        assert_eq!(f.keychain.reads() - before, 1);
        s.close_session(&user.id).await?;

        // A password that is wrong in the keychain as well fails once
        // more, with one extra read, then gives up.
        s.save_secret(&profile.id, SecretKindName::DbPassword, secret("wrong")?)?;
        let before = f.keychain.reads();
        let error = s.open_session(&profile.id, None, None, None).await.err();
        assert!(
            matches!(error, Some(CmdError::Session { .. })),
            "{engine:?}: {error:?}"
        );
        assert_eq!(f.keychain.reads() - before, 1, "{engine:?}: retried once");
        assert!(
            s.secret_cache.is_empty(),
            "{engine:?}: nothing stale is kept"
        );
        s.delete_connection(&profile.id).await?;
    }
    Ok(())
}

/// A wrong saved password on a cold cache costs one keychain read and one
/// refused login per click: the fresh read is never retried, and the
/// refused value is not kept for the next click.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn a_wrong_password_on_a_cold_cache_is_not_retried() -> TestResult {
    for (engine, port) in targets() {
        let f = fixture(&format!("cold-{engine:?}"))?;
        let s = &f.services;
        let profile = s.create_connection(&input("cold", engine, port))?;
        s.save_secret(&profile.id, SecretKindName::DbPassword, secret("wrong")?)?;
        // As after a restart.
        let s = services_on(&f.dir, &f.keychain)?;
        assert!(!s.secret_cache.has_profile(&profile.id));
        let before = f.keychain.reads();
        for click in 1..=2 {
            let error = s.open_session(&profile.id, None, None, None).await.err();
            assert!(
                matches!(error, Some(CmdError::Session { .. })),
                "{engine:?} click {click}: {error:?}"
            );
            assert_eq!(
                f.keychain.reads() - before,
                click,
                "{engine:?} click {click}: one read per click"
            );
            assert_eq!(
                refused_logins(&s, &profile.id)?,
                click as usize,
                "{engine:?} click {click}: one login per click"
            );
            assert!(
                !s.secret_cache.has_profile(&profile.id),
                "{engine:?} click {click}: the refused value is not kept"
            );
        }
        s.delete_connection(&profile.id).await?;
    }
    Ok(())
}

/// A warm SSH secret is no reason to retry a cold, refused database
/// password: the retry is decided per secret kind.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn a_warm_ssh_secret_does_not_retry_a_cold_password() -> TestResult {
    for (engine, port) in targets() {
        let f = fixture(&format!("sshwarm-{engine:?}"))?;
        let s = &f.services;
        let profile = s.create_connection(&input("sshwarm", engine, port))?;
        s.save_secret(&profile.id, SecretKindName::DbPassword, secret("wrong")?)?;
        s.save_secret(&profile.id, SecretKindName::SshPassword, secret("bastion")?)?;
        // The SSH password is cached, the database password is not.
        s.secret_cache.forget_profile(&profile.id);
        s.secrets.get(&key(&profile.id, SecretKind::SshPassword)?)?;
        assert!(
            s.secret_cache
                .has(&key(&profile.id, SecretKind::SshPassword)?)
        );
        assert!(
            !s.secret_cache
                .has(&key(&profile.id, SecretKind::DbPassword)?)
        );
        let before = f.keychain.reads();
        let error = s.open_session(&profile.id, None, None, None).await.err();
        assert!(
            matches!(error, Some(CmdError::Session { .. })),
            "{engine:?}: {error:?}"
        );
        assert_eq!(f.keychain.reads() - before, 1, "{engine:?}: one read");
        assert_eq!(refused_logins(s, &profile.id)?, 1, "{engine:?}: one login");
        s.delete_connection(&profile.id).await?;
    }
    Ok(())
}

/// Deleting a folder forgets the cached secrets of the connections in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_delete_forgets_the_cached_secrets() -> TestResult {
    let f = fixture("folder")?;
    let s = &f.services;
    let folder = s.create_folder(None, "team", None)?;
    let mut inside = input("inside", Engine::Postgres, 5432);
    inside.folder_id = Some(folder.id.clone());
    let inside = s.create_connection(&inside)?;
    let outside = s.create_connection(&input("outside", Engine::Postgres, 5432))?;
    for profile in [&inside, &outside] {
        s.save_secret(&profile.id, SecretKindName::DbPassword, secret("pw")?)?;
    }
    assert_eq!(s.secret_cache.len(), 2);
    s.delete_folder(&folder.id, FolderDeleteMode::DeleteContents)
        .await?;
    assert!(!s.secret_cache.has_profile(&inside.id));
    assert!(s.secret_cache.has_profile(&outside.id));
    assert!(
        f.keychain
            .inner()
            .get(&key(&inside.id, SecretKind::DbPassword)?)?
            .is_none()
    );
    Ok(())
}

/// The SSH password of a tunnel is read once, however often the tunnel is
/// opened; a typed one is never read at all.
#[tokio::test(flavor = "multi_thread")]
async fn the_tunnel_reads_its_secret_once() -> TestResult {
    let f = fixture("tunnel")?;
    let s = &f.services;
    let profile = s.create_connection(&ssh_input("tunnel")?)?;
    s.save_secret(&profile.id, SecretKindName::SshPassword, secret("bastion")?)?;
    s.save_secret(&profile.id, SecretKindName::DbPassword, secret("db")?)?;
    // As after a restart: saved, not cached.
    assert_eq!(s.forget_cached_secrets(), 2);
    assert_eq!(f.keychain.reads(), 0);

    for attempt in 0..3 {
        let error = s.open_session(&profile.id, None, None, None).await.err();
        assert!(
            matches!(&error, Some(CmdError::Tunnel { code, .. }) if code == "connect"),
            "attempt {attempt}: {error:?}"
        );
    }
    assert_eq!(f.keychain.reads(), 1, "the SSH password");
    for attempt in 0..2 {
        let typed = s
            .open_session(&profile.id, None, Some(secret("typed")?), None)
            .await
            .err();
        assert!(
            matches!(&typed, Some(CmdError::Tunnel { code, .. }) if code == "connect"),
            "typed attempt {attempt}: {typed:?}"
        );
    }
    assert_eq!(f.keychain.reads(), 1, "a typed secret is not read");
    Ok(())
}

/// Reads of a secret saved through the app never reach the keychain, and
/// a profile edit, a forgotten secret, a deleted profile and a forget
/// request each make the next read go to the keychain again.
#[tokio::test(flavor = "multi_thread")]
async fn profile_changes_forget_the_cached_secrets() -> TestResult {
    let f = fixture("invalidate")?;
    let s = &f.services;
    let profile = s.create_connection(&input("inv", Engine::Postgres, 5432))?;
    let db = key(&profile.id, SecretKind::DbPassword)?;
    let read = |expected: &str| -> Res<()> {
        let found = s.secrets.get(&db)?;
        assert_eq!(found.as_ref().map(Secret::expose), Some(expected));
        Ok(())
    };

    // Saved: cached by the write, so reads cost nothing.
    s.save_secret(&profile.id, SecretKindName::DbPassword, secret("one")?)?;
    read("one")?;
    read("one")?;
    assert_eq!(f.keychain.reads(), 0);

    // Edited (even a rename): read again once.
    let mut renamed = input("renamed", Engine::Postgres, 5432);
    renamed.name = "renamed".into();
    s.update_connection(&profile.id, &renamed).await?;
    read("one")?;
    read("one")?;
    assert_eq!(f.keychain.reads(), 1);

    // Forgotten: gone from the keychain and the cache.
    s.forget_secret(&profile.id, SecretKindName::DbPassword)?;
    assert!(s.secrets.get(&db)?.is_none());
    assert_eq!(f.keychain.reads(), 2);
    assert_eq!(f.keychain.deletes(), 1);

    // Saved again, then forgotten on request.
    s.save_secret(&profile.id, SecretKindName::DbPassword, secret("two")?)?;
    read("two")?;
    assert_eq!(f.keychain.reads(), 2);
    assert_eq!(s.forget_cached_secrets(), 1);
    read("two")?;
    assert_eq!(f.keychain.reads(), 3);

    // Changed elsewhere: the cache answers until it is forgotten (a
    // refused connection does that, see the matrix test).
    f.keychain.inner().set(&db, &Secret::new("three".into()))?;
    read("two")?;
    assert!(s.secret_cache.forget_profile(&profile.id));
    read("three")?;
    assert_eq!(f.keychain.reads(), 4);

    // Deleted: nothing cached, nothing in the keychain.
    s.delete_connection(&profile.id).await?;
    assert!(s.secret_cache.is_empty());
    assert!(f.keychain.inner().get(&db)?.is_none());
    Ok(())
}

/// The setting survives a restart and applies at once.
#[tokio::test(flavor = "multi_thread")]
async fn the_remember_setting_is_stored() -> TestResult {
    let f = fixture("setting")?;
    let s = &f.services;
    assert!(s.secret_settings()?.remember_for_session);
    assert!(s.secret_cache.remember());
    let profile = s.create_connection(&input("setting", Engine::Postgres, 5432))?;
    s.save_secret(&profile.id, SecretKindName::DbPassword, secret("pw")?)?;
    assert_eq!(s.secret_cache.len(), 1);

    s.set_secret_settings(varve_app::settings::SecretSettings {
        remember_for_session: false,
    })?;
    assert!(!s.secret_settings()?.remember_for_session);
    assert!(
        s.secret_cache.is_empty(),
        "switching off forgets everything"
    );

    let restarted = services_on(&f.dir, &f.keychain)?;
    assert!(!restarted.secret_cache.remember());
    let db = key(&profile.id, SecretKind::DbPassword)?;
    restarted.secrets.get(&db)?;
    restarted.secrets.get(&db)?;
    assert_eq!(f.keychain.reads(), 2, "not remembered after the restart");
    Ok(())
}

/// A throwaway EC key encrypted with `openssl pkcs8 -topk8 -scrypt`
/// (passphrase `test-pass`). It protects nothing.
const ENCRYPTED_KEY: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----
MIHsMFcGCSqGSIb3DQEFDTBKMCkGCSsGAQQB2kcECzAcBBA7xzxOpgKLQcArKZQ2
PrZBAgJAAAIBCAIBATAdBglghkgBZQMEASoEEPln28D+ZwGiEH/GtmHSaj4EgZAS
bMwoIPXjcHZBH1YEO5Wy42Ppe1zK1h0oq4hRd4dNme5n2KAFXjoVNDc10akWl93R
awU/O70KF23x71w1ETLzZmbfsGWltQWH8MTui82Pbbqnpn6wXKvF4LKKCjwKHORA
SX1Qf/PxSDHl5a/6MGAtprcN6V6PhDibaqlB5VTLNnIQOOChyX/Ivi9PEwomhAQ=
-----END ENCRYPTED PRIVATE KEY-----
";

/// A MySQL profile on a closed local port with an encrypted client key:
/// the driver decrypts the key (MySQL does so before it connects) and then
/// fails to connect, so no server is needed.
fn tls_key_input(f: &Fixture, name: &str) -> Res<ConnectionInput> {
    let closed = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    std::fs::create_dir_all(&f.dir)?;
    let key = f.dir.join("client.key");
    std::fs::write(&key, ENCRYPTED_KEY)?;
    let cert = f.dir.join("client.crt");
    std::fs::write(&cert, "not checked before the handshake")?;
    let mut input = input(name, Engine::Mysql, closed);
    input.tls = TlsSettings {
        mode: varve_driver::TlsMode::Require,
        ca_path: None,
        client_cert_path: Some(cert.display().to_string()),
        client_key_path: Some(key.display().to_string()),
    };
    Ok(input)
}

/// The driver error code of a failed connect, or `None` for other errors.
fn driver_code(error: Option<&CmdError>) -> Option<(ErrorKind, Option<String>)> {
    match error {
        Some(CmdError::Session {
            error: SessionError::Driver { error },
        }) => Some((error.kind, error.code.clone())),
        _ => None,
    }
}

/// True when no activity event of the connection mentions `text`.
fn activity_free_of(s: &Services, connection_id: &str, text: &str) -> Res<bool> {
    let page = s.activity(&varve_activity::ActivityFilter {
        connection_id: Some(connection_id.to_string()),
        ..Default::default()
    })?;
    Ok(!serde_json::to_string(&page.events)?.contains(text))
}

/// The TLS key passphrase: asked for when the key is encrypted and none is
/// saved, a wrong one is reported with its code, a saved one is read from
/// the keychain once per run, and a refused cached one is read again once.
#[tokio::test(flavor = "multi_thread")]
async fn the_tls_key_passphrase_is_asked_for_and_read_once() -> TestResult {
    let f = fixture("tlskey")?;
    let s = &f.services;
    let profile = s.create_connection(&tls_key_input(&f, "tlskey")?)?;

    // Nothing saved or typed: the UI is asked to prompt.
    let error = s.open_session(&profile.id, None, None, None).await.err();
    assert!(
        matches!(
            error,
            Some(CmdError::SecretRequired {
                secret: SecretKindName::TlsKeyPassphrase
            })
        ),
        "{error:?}"
    );
    let error = s
        .test_connection(
            TestTarget::Saved {
                id: profile.id.clone(),
            },
            None,
            None,
            None,
        )
        .await
        .err();
    assert!(
        matches!(error, Some(CmdError::SecretRequired { .. })),
        "{error:?}"
    );
    // A prompt, not a failed connect.
    assert_eq!(
        refused_logins(s, &profile.id)?,
        0,
        "nothing logged for the prompt"
    );

    // A wrong typed passphrase: a TLS error with its code, never the value.
    let error = s
        .open_session(&profile.id, None, None, Some(secret("Wr0ngPass")?))
        .await
        .err();
    assert_eq!(
        driver_code(error.as_ref()),
        Some((ErrorKind::Tls, Some(PASSPHRASE_WRONG.to_string()))),
        "{error:?}"
    );
    assert!(!format!("{error:?}").contains("Wr0ngPass"));
    assert_eq!(
        refused_logins(s, &profile.id)?,
        1,
        "a wrong passphrase is a failure"
    );
    // The right one decrypts the key; the closed port fails the connect.
    let error = s
        .open_session(&profile.id, None, None, Some(secret("test-pass")?))
        .await
        .err();
    assert_eq!(
        driver_code(error.as_ref()).map(|(kind, _)| kind),
        Some(ErrorKind::Connect),
        "{error:?}"
    );
    assert_ne!(
        driver_code(error.as_ref())
            .and_then(|(_, code)| code)
            .as_deref(),
        Some(PASSPHRASE_REQUIRED)
    );
    // No session uses the decrypted key: it is not kept.
    let key_path = f.dir.join("client.key").display().to_string();
    assert!(!varve_driver::tls_key::is_key_cached(&key_path));
    // Nor after a test connection with a typed passphrase.
    let error = s
        .test_connection(
            TestTarget::Saved {
                id: profile.id.clone(),
            },
            None,
            None,
            Some(secret("test-pass")?),
        )
        .await
        .err();
    assert_eq!(
        driver_code(error.as_ref()).map(|(kind, _)| kind),
        Some(ErrorKind::Connect),
        "{error:?}"
    );
    assert!(!varve_driver::tls_key::is_key_cached(&key_path));

    // Saved: one keychain read per run, however many connects.
    s.save_secret(
        &profile.id,
        SecretKindName::TlsKeyPassphrase,
        secret("test-pass")?,
    )?;
    let s = services_on(&f.dir, &f.keychain)?;
    let before = f.keychain.reads();
    for attempt in 1..=3 {
        let error = s.open_session(&profile.id, None, None, None).await.err();
        assert_eq!(
            driver_code(error.as_ref()).map(|(kind, _)| kind),
            Some(ErrorKind::Connect),
            "attempt {attempt}: {error:?}"
        );
    }
    assert_eq!(
        f.keychain.reads() - before,
        1,
        "one read for three connects"
    );

    // Changed in the keychain after it was cached: the refused cached
    // value is read again once, and the fresh one works.
    s.save_secret(
        &profile.id,
        SecretKindName::TlsKeyPassphrase,
        secret("St4leValue")?,
    )?;
    f.keychain.inner().set(
        &key(&profile.id, SecretKind::TlsKeyPassphrase)?,
        &Secret::new("test-pass".into()),
    )?;
    let before = f.keychain.reads();
    let error = s.open_session(&profile.id, None, None, None).await.err();
    assert_eq!(
        driver_code(error.as_ref()).map(|(kind, _)| kind),
        Some(ErrorKind::Connect),
        "{error:?}"
    );
    assert_eq!(f.keychain.reads() - before, 1, "one fresh read");

    // Wrong in the keychain on a cold cache: one read, no retry, nothing
    // kept.
    f.keychain.inner().set(
        &key(&profile.id, SecretKind::TlsKeyPassphrase)?,
        &Secret::new("still-wrong".into()),
    )?;
    let s = services_on(&f.dir, &f.keychain)?;
    let before = f.keychain.reads();
    let error = s.open_session(&profile.id, None, None, None).await.err();
    assert_eq!(
        driver_code(error.as_ref()),
        Some((ErrorKind::Tls, Some(PASSPHRASE_WRONG.to_string()))),
        "{error:?}"
    );
    assert_eq!(f.keychain.reads() - before, 1, "not retried");
    assert!(!s.secret_cache.has_profile(&profile.id));

    // No passphrase reaches the activity log.
    for text in ["Wr0ngPass", "test-pass", "St4leValue", "still-wrong"] {
        assert!(activity_free_of(&s, &profile.id, text)?, "{text} logged");
    }
    // Editing the key file forgets the cached passphrase and unlists it.
    let mut edited = tls_key_input(&f, "tlskey")?;
    edited.tls.client_key_path = Some(f.dir.join("other.key").display().to_string());
    let edited = s.update_connection(&profile.id, &edited).await?;
    assert!(
        !edited
            .saved_secrets
            .contains(&SecretKindName::TlsKeyPassphrase)
    );
    assert!(!s.secret_cache.has_profile(&profile.id));
    s.delete_connection(&profile.id).await?;
    Ok(())
}
