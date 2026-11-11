use std::sync::Arc;

use varve_core::Engine;
use varve_driver::TlsMode;
use varve_secrets::{
    CountingStore, MemoryStore, Secret, SecretCache, SecretKey, SecretKind, SecretStore,
};
use varve_store::{Store, core_migrations};

use crate::{
    ConnectionInput, Environment, FolderDeleteMode, PERSONAL_WORKSPACE, RecordError, Records,
    ReorderKind, SavedQueryInput, SecretKindName, SshAuthMethod, TunnelEndpoint, connect_params,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn records() -> Result<Records, Box<dyn std::error::Error>> {
    let store = Store::open_in_memory()?;
    store.migrate(&[core_migrations()])?;
    Ok(Records::new(Arc::new(store)))
}

fn pg(name: &str) -> ConnectionInput {
    ConnectionInput {
        folder_id: None,
        name: name.into(),
        color: None,
        engine: Engine::Postgres,
        host: "db.example.internal".into(),
        port: None,
        database: Some("shop".into()),
        username: Some("kim".into()),
        environment: Environment::Development,
        tls: Default::default(),
        ssh: Default::default(),
        options: Default::default(),
    }
}

const WS: &str = PERSONAL_WORKSPACE;

#[test]
fn connection_round_trip_with_defaults() -> TestResult {
    let r = records()?;
    let created = r.create_connection(WS, &pg("  Orders primary  "))?;
    assert_eq!(created.name, "Orders primary");
    assert_eq!(created.port, 5432);
    assert_eq!(created.version, 1);
    assert!(created.saved_secrets.is_empty());
    assert_eq!(r.connection(&created.id)?, created);
    assert_eq!(r.connections(WS)?.len(), 1);
    Ok(())
}

#[test]
fn validation_messages_are_specific() -> TestResult {
    let r = records()?;
    let cases: Vec<(ConnectionInput, &str)> = vec![
        (
            ConnectionInput {
                name: " ".into(),
                ..pg("x")
            },
            "needs a name",
        ),
        (
            ConnectionInput {
                host: "bad host".into(),
                ..pg("x")
            },
            "cannot contain spaces",
        ),
        (
            ConnectionInput {
                port: Some(0),
                ..pg("x")
            },
            "between 1 and 65535",
        ),
        (
            ConnectionInput {
                engine: Engine::Redis,
                ..pg("x")
            },
            "Redis has no database name",
        ),
        (
            ConnectionInput {
                color: Some("Bright Red".into()),
                ..pg("x")
            },
            "lowercase tokens",
        ),
    ];
    for (input, expected) in cases {
        match r.create_connection(WS, &input) {
            Err(RecordError::Invalid(message)) => assert!(message.contains(expected), "{message}"),
            other => panic!("expected invalid input ({expected}), got {other:?}"),
        }
    }
    let mut ssh = pg("x");
    ssh.ssh.enabled = true;
    ssh.ssh.host = "bastion".into();
    ssh.ssh.username = "kim".into();
    ssh.ssh.auth = SshAuthMethod::KeyFile;
    assert!(
        matches!(r.create_connection(WS, &ssh), Err(RecordError::Invalid(m)) if m.contains("private key"))
    );
    let mut tls = pg("x");
    tls.tls.client_cert_path = Some("/c.pem".into());
    assert!(
        matches!(r.create_connection(WS, &tls), Err(RecordError::Invalid(m)) if m.contains("needs its key"))
    );
    Ok(())
}

fn redis(init: &str) -> ConnectionInput {
    let mut input = ConnectionInput {
        engine: Engine::Redis,
        database: None,
        ..pg("cache")
    };
    input.options.init_sql = Some(init.into());
    input
}

#[test]
fn redis_init_commands_that_break_the_connection_are_refused() -> TestResult {
    let r = records()?;
    for (init, expected) in [
        (
            "MULTI",
            "Init command on line 1: MULTI cannot be an init command",
        ),
        (
            "SELECT 2\nCLIENT REPLY OFF",
            "Init command on line 2: CLIENT cannot be an init command: it is not a connection setup command",
        ),
        (
            "SELECT 1\n\nsubscribe news",
            "Init command on line 3: SUBSCRIBE",
        ),
        (
            "SET k \"open",
            "Init command on line 1: unterminated double quote",
        ),
        (
            "SELECT 2\nSHUTDOWN NOSAVE",
            "Init command on line 2: SHUTDOWN cannot be an init command: it is not a connection setup command",
        ),
        (
            "FLUSHALL",
            "Init command on line 1: FLUSHALL cannot be an init command",
        ),
        (
            "CLIENT PAUSE 3000 ALL",
            "Init command on line 1: CLIENT cannot be an init command",
        ),
        (
            "config set maxmemory 1",
            "Init command on line 1: CONFIG cannot be an init command",
        ),
        // Only connection setup: writes and reads are refused too.
        (
            "DEL k",
            "Init command on line 1: DEL cannot be an init command",
        ),
        (
            "SAVE",
            "Init command on line 1: SAVE cannot be an init command",
        ),
        (
            "SELECT 1\nMOVE k 2",
            "Init command on line 2: MOVE cannot be an init command",
        ),
        (
            "CONFIG GET maxmemory",
            "Init command on line 1: CONFIG cannot be an init command",
        ),
        (
            "XREAD STREAMS BLOCK 0",
            "Init command on line 1: XREAD cannot be an init command",
        ),
        (
            "SELECT 70000",
            "Init command on line 1: SELECT cannot be an init command: it selects a database number outside 0 to 65535",
        ),
        (
            "AUTH app secret",
            "Init command on line 1: AUTH cannot be an init command: it puts a password in the init text",
        ),
        (
            "SELECT 1\nHELLO 2 AUTH app secret",
            "Init command on line 2: HELLO cannot be an init command: it puts a password",
        ),
        (
            "HELLO 3",
            "Init command on line 1: HELLO cannot be an init command: it switches the protocol",
        ),
    ] {
        match r.create_connection(WS, &redis(init)) {
            Err(RecordError::Invalid(message)) => {
                assert!(message.starts_with(expected), "{init}: {message}");
            }
            other => panic!("{init}: expected invalid input, got {other:?}"),
        }
    }
    for init in [
        "SELECT 2",
        "HELLO 2",
        "SELECT 2\nHELLO 2\nCLIENT SETNAME app",
        "CLIENT NO-EVICT on\nCLIENT NO-TOUCH on\nCLIENT SETINFO LIB-NAME app",
        "READONLY",
        "HELLO 2 SETNAME auth",
        "SELECT 65535",
    ] {
        r.create_connection(WS, &redis(init))?;
    }
    // Saving a change goes through the same check.
    let saved = r.create_connection(WS, &redis("SELECT 2"))?;
    let error = r.update_connection(&saved.id, &redis("MULTI"));
    assert!(
        matches!(&error, Err(RecordError::Invalid(m)) if m.contains("MULTI cannot be")),
        "{error:?}"
    );
    // Other engines are not checked as Redis.
    let mut postgres = pg("pg");
    postgres.options.init_sql = Some("MULTI".into());
    r.create_connection(WS, &postgres)?;
    Ok(())
}

#[test]
fn production_always_confirms_destructive_statements() -> TestResult {
    let r = records()?;
    let mut input = pg("Prod");
    input.environment = Environment::Production;
    input.options.confirm_destructive = false;
    let created = r.create_connection(WS, &input)?;
    assert!(created.options.confirm_destructive);
    Ok(())
}

#[test]
fn update_bumps_version_and_duplicate_drops_secrets() -> TestResult {
    let r = records()?;
    let created = r.create_connection(WS, &pg("A"))?;
    r.set_saved_secrets(&created.id, &[SecretKindName::DbPassword])?;
    let updated = r.update_connection(
        &created.id,
        &ConnectionInput {
            port: Some(6543),
            ..pg("A2")
        },
    )?;
    assert_eq!(updated.version, 2);
    assert_eq!(updated.port, 6543);
    assert_eq!(updated.saved_secrets, vec![SecretKindName::DbPassword]);
    let copy = r.duplicate_connection(&created.id)?;
    assert_eq!(copy.name, "A2 copy");
    assert!(copy.saved_secrets.is_empty());
    assert_ne!(copy.id, created.id);
    Ok(())
}

#[test]
fn folders_nest_and_refuse_cycles() -> TestResult {
    let r = records()?;
    let a = r.create_folder(WS, None, "Webshop", Some("violet"))?;
    let b = r.create_folder(WS, Some(&a.id), "Billing", None)?;
    let c = r.create_folder(WS, Some(&b.id), "Archive", None)?;
    assert!(matches!(
        r.move_folder(&a.id, Some(&c.id)),
        Err(RecordError::Cycle)
    ));
    assert!(matches!(
        r.move_folder(&a.id, Some(&a.id)),
        Err(RecordError::Cycle)
    ));
    let moved = r.move_folder(&c.id, None)?;
    assert_eq!(moved.parent_id, None);
    assert!(matches!(
        r.create_folder(WS, Some("missing"), "x", None),
        Err(RecordError::NotFound { .. })
    ));
    Ok(())
}

#[test]
fn deleting_a_folder_moves_contents_to_the_parent_by_default() -> TestResult {
    let r = records()?;
    let top = r.create_folder(WS, None, "Top", None)?;
    let inner = r.create_folder(WS, Some(&top.id), "Inner", None)?;
    let sub = r.create_folder(WS, Some(&inner.id), "Sub", None)?;
    let conn = r.create_connection(
        WS,
        &ConnectionInput {
            folder_id: Some(inner.id.clone()),
            ..pg("C")
        },
    )?;
    let query = r.create_query(
        WS,
        &SavedQueryInput {
            folder_id: Some(inner.id.clone()),
            connection_id: Some(conn.id.clone()),
            database: None,
            name: "Q".into(),
            sql: "select 1".into(),
            tags: vec![],
        },
    )?;
    let contents = r.folder_contents(&inner.id)?;
    assert_eq!(
        (contents.folders, contents.connections, contents.queries),
        (1, 1, 1)
    );

    let deleted = r.delete_folder(&inner.id, FolderDeleteMode::MoveContentsToParent)?;
    assert!(deleted.is_empty());
    assert_eq!(
        r.connection(&conn.id)?.folder_id.as_deref(),
        Some(top.id.as_str())
    );
    assert_eq!(
        r.query(&query.id)?.folder_id.as_deref(),
        Some(top.id.as_str())
    );
    assert_eq!(
        r.folder(&sub.id)?.parent_id.as_deref(),
        Some(top.id.as_str())
    );
    Ok(())
}

fn query_in(folder: Option<&str>, name: &str) -> SavedQueryInput {
    SavedQueryInput {
        folder_id: folder.map(str::to_string),
        connection_id: None,
        database: None,
        name: name.into(),
        sql: "select 1".into(),
        tags: vec![],
    }
}

fn connection_in(folder: Option<&str>, name: &str) -> ConnectionInput {
    ConnectionInput {
        folder_id: folder.map(str::to_string),
        ..pg(name)
    }
}

#[test]
fn moved_contents_follow_the_parents_own_items() -> TestResult {
    let r = records()?;
    let top = r.create_folder(WS, None, "Top", None)?;
    let t = Some(top.id.as_str());
    // Names sort the other way round, so only sort_order can give the
    // expected order.
    let a = r.create_connection(WS, &connection_in(t, "Z first"))?;
    let b = r.create_connection(WS, &connection_in(t, "Y second"))?;
    let qa = r.create_query(WS, &query_in(t, "Z first"))?;
    let fa = r.create_folder(WS, t, "Z first", None)?;
    let inner = r.create_folder(WS, t, "Inner", None)?;
    let i = Some(inner.id.as_str());
    let x = r.create_connection(WS, &connection_in(i, "B third"))?;
    let y = r.create_connection(WS, &connection_in(i, "A fourth"))?;
    let qx = r.create_query(WS, &query_in(i, "A second"))?;
    let fx = r.create_folder(WS, i, "A second", None)?;

    r.delete_folder(&inner.id, FolderDeleteMode::MoveContentsToParent)?;
    let orders_of = |ids: &[&str]| -> Result<Vec<i64>, RecordError> {
        ids.iter()
            .map(|id| Ok(r.connection(id)?.sort_order))
            .collect()
    };
    let orders = orders_of(&[&a.id, &b.id, &x.id, &y.id])?;
    assert!(
        orders.windows(2).all(|w| w[0] < w[1]),
        "connections out of order: {orders:?}"
    );
    let in_top: Vec<String> = r
        .connections(WS)?
        .into_iter()
        .filter(|c| c.folder_id.as_deref() == t)
        .map(|c| c.name)
        .collect();
    assert_eq!(in_top, ["Z first", "Y second", "B third", "A fourth"]);
    assert!(r.query(&qa.id)?.sort_order < r.query(&qx.id)?.sort_order);
    assert!(r.folder(&fa.id)?.sort_order < r.folder(&fx.id)?.sort_order);
    Ok(())
}

#[test]
fn editing_the_folder_of_a_record_puts_it_last_there() -> TestResult {
    let r = records()?;
    let target = r.create_folder(WS, None, "Target", None)?;
    let t = Some(target.id.as_str());
    let first = r.create_connection(WS, &connection_in(t, "B"))?;
    let second = r.create_connection(WS, &connection_in(t, "C"))?;
    let moving = r.create_connection(WS, &connection_in(None, "A"))?;
    let stays = r.create_connection(WS, &connection_in(None, "D"))?;

    let moved = r.update_connection(&moving.id, &connection_in(t, "A"))?;
    assert!(moved.sort_order > r.connection(&second.id)?.sort_order);
    assert!(r.connection(&first.id)?.sort_order < r.connection(&second.id)?.sort_order);
    // Same folder: the order is kept.
    let edited = r.update_connection(&stays.id, &connection_in(None, "D renamed"))?;
    assert_eq!(edited.sort_order, stays.sort_order);

    let q1 = r.create_query(WS, &query_in(t, "B"))?;
    let q2 = r.create_query(WS, &query_in(None, "A"))?;
    let moved = r.update_query(&q2.id, &query_in(t, "A"))?;
    assert!(moved.sort_order > q1.sort_order);
    let kept = r.update_query(&q1.id, &query_in(t, "B renamed"))?;
    assert_eq!(kept.sort_order, q1.sort_order);
    Ok(())
}

#[test]
fn redis_database_numbers_above_fifteen_are_accepted() -> TestResult {
    let r = records()?;
    for db in [16_u16, 63, u16::MAX] {
        let mut input = redis("");
        input.options.init_sql = None;
        input.options.redis_db = db;
        assert_eq!(r.create_connection(WS, &input)?.options.redis_db, db);
    }
    let mut pg_with_db = pg("x");
    pg_with_db.options.redis_db = 16;
    assert!(matches!(
        r.create_connection(WS, &pg_with_db),
        Err(RecordError::Invalid(m)) if m.contains("only applies to Redis")
    ));
    Ok(())
}

#[test]
fn deleting_a_folder_with_contents_removes_everything_below_it() -> TestResult {
    let r = records()?;
    let top = r.create_folder(WS, None, "Legacy", None)?;
    let sub = r.create_folder(WS, Some(&top.id), "Old", None)?;
    let c1 = r.create_connection(
        WS,
        &ConnectionInput {
            folder_id: Some(top.id.clone()),
            ..pg("C1")
        },
    )?;
    let c2 = r.create_connection(
        WS,
        &ConnectionInput {
            folder_id: Some(sub.id.clone()),
            ..pg("C2")
        },
    )?;
    let outside = r.create_connection(WS, &pg("Outside"))?;
    r.create_query(
        WS,
        &SavedQueryInput {
            folder_id: Some(sub.id.clone()),
            connection_id: None,
            database: None,
            name: "Q".into(),
            sql: "select 1".into(),
            tags: vec![],
        },
    )?;

    let mut deleted: Vec<String> = r
        .delete_folder(&top.id, FolderDeleteMode::DeleteContents)?
        .into_iter()
        .map(|c| c.id)
        .collect();
    deleted.sort();
    let mut expected = vec![c1.id, c2.id];
    expected.sort();
    assert_eq!(deleted, expected);
    assert!(r.folders(WS)?.is_empty());
    assert!(r.queries(WS)?.is_empty());
    assert_eq!(r.connections(WS)?.len(), 1);
    assert_eq!(r.connections(WS)?[0].id, outside.id);
    Ok(())
}

#[test]
fn deleting_a_connection_detaches_its_saved_queries() -> TestResult {
    let r = records()?;
    let conn = r.create_connection(WS, &pg("C"))?;
    let q = r.create_query(
        WS,
        &SavedQueryInput {
            folder_id: None,
            connection_id: Some(conn.id.clone()),
            database: Some("shop".into()),
            name: "Q".into(),
            sql: "select 1".into(),
            tags: vec!["b".into(), " a ".into(), "b".into()],
        },
    )?;
    assert_eq!(q.tags, vec!["a".to_string(), "b".to_string()]);
    r.delete_connection(&conn.id)?;
    assert_eq!(r.query(&q.id)?.connection_id, None);
    Ok(())
}

#[test]
fn reorder_requires_siblings() -> TestResult {
    let r = records()?;
    let f = r.create_folder(WS, None, "F", None)?;
    let a = r.create_connection(WS, &pg("A"))?;
    let b = r.create_connection(WS, &pg("B"))?;
    let c = r.create_connection(
        WS,
        &ConnectionInput {
            folder_id: Some(f.id),
            ..pg("C")
        },
    )?;
    r.reorder(ReorderKind::Connections, &[b.id.clone(), a.id.clone()])?;
    assert_eq!(r.connection(&b.id)?.sort_order, 0);
    assert_eq!(r.connection(&a.id)?.sort_order, 1);
    assert!(matches!(
        r.reorder(ReorderKind::Connections, &[a.id, c.id]),
        Err(RecordError::Invalid(_))
    ));
    Ok(())
}

/// A keychain that cannot be reached (no Secret Service running).
struct UnreachableKeychain;

impl SecretStore for UnreachableKeychain {
    fn set(&self, _: &SecretKey, _: &Secret) -> Result<(), varve_secrets::SecretError> {
        Err(varve_secrets::SecretError::Unavailable("no store".into()))
    }

    fn get(&self, _: &SecretKey) -> Result<Option<Secret>, varve_secrets::SecretError> {
        Err(varve_secrets::SecretError::Unavailable("no store".into()))
    }

    fn delete(&self, _: &SecretKey) -> Result<(), varve_secrets::SecretError> {
        Err(varve_secrets::SecretError::Unavailable("no store".into()))
    }
}

#[test]
fn an_unreachable_keychain_is_reported_as_such() -> TestResult {
    let r = records()?;
    let profile = r.create_connection(WS, &pg("Saved"))?;
    let profile = r.set_saved_secrets(&profile.id, &[SecretKindName::DbPassword])?;
    match connect_params(&profile, &UnreachableKeychain, None, None, None) {
        Err(RecordError::KeychainUnavailable(message)) => {
            assert!(message.contains("not available"), "{message}");
        }
        other => panic!("expected an unavailable keychain, got {other:?}"),
    }
    // A typed password does not need the keychain.
    let params = connect_params(
        &profile,
        &UnreachableKeychain,
        Some(Secret::new("typed".into())),
        None,
        None,
    )?;
    assert_eq!(params.password.as_ref().map(Secret::expose), Some("typed"));
    Ok(())
}

#[test]
fn connect_params_use_keychain_and_tunnel() -> TestResult {
    let r = records()?;
    let mut input = pg("Tunnelled");
    input.tls.mode = TlsMode::VerifyFull;
    input.options.read_only = true;
    input.options.statement_timeout_ms = Some(5_000);
    let profile = r.create_connection(WS, &input)?;
    let secrets = MemoryStore::default();
    secrets.set(
        &SecretKey::new(profile.id.clone(), SecretKind::DbPassword)?,
        &Secret::new("s3cret".into()),
    )?;

    // Not marked as saved: no password even though the keychain has one.
    let params = connect_params(&profile, &secrets, None, None, None)?;
    assert!(params.password.is_none());

    let profile = r.set_saved_secrets(&profile.id, &[SecretKindName::DbPassword])?;
    let endpoint = TunnelEndpoint {
        host: "127.0.0.1".into(),
        port: 61_000,
    };
    let params = connect_params(&profile, &secrets, None, None, Some(&endpoint))?;
    assert_eq!(params.password.as_ref().map(Secret::expose), Some("s3cret"));
    assert_eq!((params.host.as_str(), params.port), ("127.0.0.1", 61_000));
    assert_eq!(
        params.tls.server_name.as_deref(),
        Some("db.example.internal")
    );
    assert!(params.read_only);
    assert_eq!(
        params.statement_timeout,
        Some(std::time::Duration::from_secs(5))
    );

    // A prompted password wins over the keychain.
    let params = connect_params(
        &profile,
        &secrets,
        Some(Secret::new("typed".into())),
        None,
        None,
    )?;
    assert_eq!(params.password.as_ref().map(Secret::expose), Some("typed"));
    assert_eq!(params.tls.server_name, None);
    Ok(())
}

#[test]
fn connect_params_resolve_the_tls_key_passphrase() -> TestResult {
    let r = records()?;
    let mut input = pg("Client key");
    input.tls.mode = TlsMode::Require;
    input.tls.client_cert_path = Some("/certs/client.crt".into());
    input.tls.client_key_path = Some("/certs/client.key".into());
    let profile = r.create_connection(WS, &input)?;
    let store = Arc::new(CountingStore::new(MemoryStore::default()));
    store.set(
        &SecretKey::new(profile.id.clone(), SecretKind::TlsKeyPassphrase)?,
        &Secret::new("key-pass".into()),
    )?;
    let cache = SecretCache::new(Arc::clone(&store) as Arc<dyn SecretStore>, true);

    // Not marked as saved: never read.
    let params = connect_params(&profile, &cache, None, None, None)?;
    assert!(params.tls.client_key_passphrase.is_none());
    assert_eq!(store.reads(), 0);

    let profile = r.set_saved_secrets(&profile.id, &[SecretKindName::TlsKeyPassphrase])?;
    for _ in 0..3 {
        let params = connect_params(&profile, &cache, None, None, None)?;
        assert_eq!(
            params
                .tls
                .client_key_passphrase
                .as_ref()
                .map(Secret::expose),
            Some("key-pass")
        );
        // The database password is not saved: no read for it.
        assert!(params.password.is_none());
    }
    assert_eq!(store.reads(), 1, "one keychain read per run");

    // A typed passphrase wins and reads nothing.
    let params = connect_params(
        &profile,
        store.as_ref(),
        None,
        Some(Secret::new("typed".into())),
        None,
    )?;
    assert_eq!(
        params
            .tls
            .client_key_passphrase
            .as_ref()
            .map(Secret::expose),
        Some("typed")
    );
    assert_eq!(store.reads(), 1);

    // TLS off or no client key: the passphrase is neither read nor passed.
    // (Editing refuses TLS off with key files; resolving still checks it.)
    let changes: [fn(&mut crate::ConnectionProfile); 2] = [
        |p| p.tls.mode = TlsMode::Disable,
        |p| {
            p.tls.client_cert_path = None;
            p.tls.client_key_path = None;
        },
    ];
    for change in changes {
        let mut other = profile.clone();
        change(&mut other);
        let params = connect_params(
            &other,
            store.as_ref(),
            None,
            Some(Secret::new("typed".into())),
            None,
        )?;
        assert!(params.tls.client_key_passphrase.is_none());
        let params = connect_params(&other, store.as_ref(), None, None, None)?;
        assert!(params.tls.client_key_passphrase.is_none());
    }
    assert_eq!(store.reads(), 1);
    // Never printed.
    assert!(
        !format!(
            "{params:?}",
            params = connect_params(&profile, &cache, None, None, None)?
        )
        .contains("key-pass")
    );
    Ok(())
}
