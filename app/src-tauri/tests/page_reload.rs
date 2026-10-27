//! A reloaded page leaves no Redis subscriber connections behind
//! (m2/reload.rs), against Redis 8.10 and Valkey 9 of the M2 matrix.
//!
//! Run with `cargo test -p varve-app --test page_reload -- --include-ignored`.
//! The page's sessions are listed when the page starts loading and closed
//! (`Services::begin_page` and `close_unloaded_page`, as `on_page_load`
//! calls them); the test subscribes like a Pub/Sub tab, simulates the
//! reload, and counts the subscriber connections with `CLIENT LIST TYPE
//! pubsub` and `PUBSUB NUMSUB`. PostgreSQL (18): the report names the
//! sessions whose open transaction the close rolled back, and a session
//! still being opened when the reload starts is closed once it is open.

use std::collections::BTreeSet;
use std::sync::Arc;

use varve_app::commands::{SecretInput, Services};
use varve_app::m2::pubsub::{PubSubEmitter, PubSubEvent, PubSubNames, SubscribeOptions};
use varve_app::sessions::SessionInfo;
use varve_core::{Edition, Engine};
use varve_driver::{StatementResult, Value};
use varve_records::{ConnectionInput, ConnectionOptions, Environment};
use varve_secrets::MemoryStore;

type Res<T = ()> = Result<T, Box<dyn std::error::Error>>;

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
    let dir = std::env::temp_dir().join(format!("varve-reload-{name}-{}", std::process::id()));
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

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Ignore;

impl PubSubEmitter for Ignore {
    fn emit(&self, _event: PubSubEvent) {}
}

async fn open(s: &Services, name: &str, port: u16) -> Res<SessionInfo> {
    let input = ConnectionInput {
        folder_id: None,
        name: format!("{name} m2_uireload"),
        color: None,
        engine: Engine::Redis,
        host: std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: Some(port),
        database: None,
        username: None,
        environment: Environment::Development,
        tls: Default::default(),
        ssh: Default::default(),
        options: ConnectionOptions::default(),
    };
    let profile = s.create_connection(&input)?;
    let secret: SecretInput = serde_json::from_value(serde_json::json!("varve_dev"))?;
    Ok(s.open_session(&profile.id, Some(secret), None, None)
        .await?)
}

/// The text of the first value of a command's reply.
async fn reply(s: &Services, session: &SessionInfo, command: &str) -> Res<Vec<Value>> {
    let out = s.execute(&session.id, command, true, None).await?;
    Ok(out
        .results
        .into_iter()
        .flat_map(|r| match r {
            StatementResult::Rows { rows, .. } => rows.into_iter().flatten().collect(),
            StatementResult::Command { .. } => Vec::new(),
        })
        .collect())
}

fn plain(values: &[Value]) -> String {
    values
        .iter()
        .filter_map(Value::to_plain_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ids of the connections in Pub/Sub mode.
async fn pubsub_clients(s: &Services, probe: &SessionInfo) -> Res<BTreeSet<String>> {
    let list = plain(&reply(s, probe, "CLIENT LIST TYPE pubsub").await?);
    Ok(list
        .lines()
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("id="))
                .map(str::to_owned)
        })
        .collect())
}

/// Subscribers of `channel` (`PUBSUB NUMSUB`).
async fn numsub(s: &Services, probe: &SessionInfo, channel: &str) -> Res<i64> {
    let values = reply(s, probe, &format!("PUBSUB NUMSUB {channel}")).await?;
    let text = plain(&values);
    Ok(text
        .lines()
        .filter_map(|l| l.trim().parse::<i64>().ok())
        .next_back()
        .unwrap_or(0))
}

async fn reload_closes_subscribers(name: &str, port: u16) -> Res {
    let f = fixture(name.replace(' ', "-").as_str())?;
    let s = &f.services;
    let channel = format!(
        "m2_uireload:{}:{}",
        name.replace(' ', "_"),
        std::process::id()
    );
    // The probe is opened by the next page, after the reload started.
    let tab = open(s, name, port).await?;
    let other_tab = open(s, name, port).await?;
    let before = {
        let probe = open(s, name, port).await?;
        let ids = pubsub_clients(s, &probe).await?;
        s.close_session(&probe.id).await?;
        ids
    };
    let names = PubSubNames {
        channels: vec![channel.clone()],
        ..Default::default()
    };
    let options = SubscribeOptions {
        max_payload_bytes: 65536,
        resubscribe: true,
    };
    for session in [&tab, &other_tab] {
        s.redis_pubsub_subscribe(&session.id, &names, options, Arc::new(Ignore))
            .await?;
    }
    s.redis_pubsub_subscribe(&tab.id, &names, options, Arc::new(Ignore))
        .await?;

    // The page starts loading: its sessions are listed at once.
    let unloaded = s.begin_page();
    assert_eq!(unloaded.len(), 2, "{name}");
    // The new page opens a session before the old ones are closed.
    let probe = open(s, name, port).await?;
    let ours: BTreeSet<String> = pubsub_clients(s, &probe)
        .await?
        .difference(&before)
        .cloned()
        .collect();
    assert!(ours.len() >= 3, "{name}: subscriber connections {ours:?}");
    assert_eq!(numsub(s, &probe, &channel).await?, 3, "{name}");

    assert_eq!(s.close_unloaded_page(unloaded).await, 2, "{name}");
    let report = s.reload_report().await;
    assert_eq!(report.sessions.len(), 2, "{name}");
    // The subscriber connections are gone; the new page's session is not.
    let mut left = ours.clone();
    for _ in 0..50 {
        let now = pubsub_clients(s, &probe).await?;
        left = ours.intersection(&now).cloned().collect();
        if left.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(left.is_empty(), "{name}: still connected {left:?}");
    assert_eq!(numsub(s, &probe, &channel).await?, 0, "{name}");
    assert!(s.sessions.info(&probe.id).is_ok(), "{name}");
    assert!(s.sessions.info(&tab.id).is_err(), "{name}");
    s.close_session(&probe.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn a_reload_closes_the_old_page_s_subscriber_connections() -> Res {
    reload_closes_subscribers("Redis 8.10", port("VARVE_M2_REDIS810_PORT", 64081)).await?;
    reload_closes_subscribers("Valkey 9", port("VARVE_M2_VALKEY9_PORT", 64190)).await
}

// ---------------------------------------------------------------------------
// PostgreSQL: the report, and sessions still opening

async fn open_pg(s: &Services, port: u16) -> Res<SessionInfo> {
    let input = ConnectionInput {
        folder_id: None,
        name: format!("PG m2_uireload {port}"),
        color: None,
        engine: Engine::Postgres,
        host: "127.0.0.1".into(),
        port: Some(port),
        database: Some("varve".into()),
        username: Some("varve".into()),
        environment: Environment::Development,
        tls: Default::default(),
        ssh: Default::default(),
        options: ConnectionOptions::default(),
    };
    let profile = s.create_connection(&input)?;
    let secret: SecretInput = serde_json::from_value(serde_json::json!("varve_dev"))?;
    Ok(s.open_session(&profile.id, Some(secret), None, None)
        .await?)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn a_reload_reports_the_transactions_it_rolled_back() -> Res {
    let f = fixture("report")?;
    let s = &f.services;
    let pg = port("VARVE_M2_PG18_PORT", 55518);
    let busy = open_pg(s, pg).await?;
    let idle = open_pg(s, pg).await?;
    s.execute(
        &busy.id,
        "BEGIN; CREATE TEMP TABLE m2_uireload (a int)",
        true,
        None,
    )
    .await?;
    let unloaded = s.begin_page();
    assert_eq!(unloaded.len(), 2);
    assert_eq!(s.close_unloaded_page(unloaded).await, 2);
    let report = s.reload_report().await;
    let state = |id: &str| {
        report
            .sessions
            .iter()
            .find(|r| r.connection_id == id)
            .map(|r| r.in_transaction)
    };
    assert_eq!(state(&busy.connection_id), Some(Some(true)));
    assert_eq!(state(&idle.connection_id), Some(Some(false)));
    // Read once.
    assert!(s.reload_report().await.sessions.is_empty());
    Ok(())
}

/// A TCP relay to `target` that holds the first connection until `go`, and
/// sets `closed` when the client side of it ended.
fn held_relay(
    target: u16,
) -> Res<(
    u16,
    std::sync::mpsc::Sender<()>,
    Arc<std::sync::atomic::AtomicBool>,
)> {
    use std::io::copy;
    use std::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = Arc::clone(&closed);
    std::thread::spawn(move || {
        let Ok((mut client, _)) = listener.accept() else {
            return;
        };
        let _ = wait.recv();
        let Ok(mut server) = TcpStream::connect(("127.0.0.1", target)) else {
            return;
        };
        let (Ok(mut client_r), Ok(mut server_w)) = (client.try_clone(), server.try_clone()) else {
            return;
        };
        std::thread::spawn(move || {
            let _ = copy(&mut server, &mut client);
        });
        let _ = copy(&mut client_r, &mut server_w);
        let _ = server_w.shutdown(std::net::Shutdown::Both);
        done.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    Ok((port, go, closed))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn a_session_still_opening_when_the_page_reloads_is_closed() -> Res {
    let f = fixture("opening")?;
    let s = Arc::clone(&f.services);
    let (relay, go, closed) = held_relay(port("VARVE_M2_PG18_PORT", 55518))?;
    let opening = {
        let s = Arc::clone(&s);
        tokio::spawn(async move { open_pg(&s, relay).await.map_err(|e| e.to_string()) })
    };
    // The open waits for the server's answer, which the relay holds back.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let unloaded = s.begin_page();
    assert!(unloaded.is_empty(), "nothing was open yet");
    assert_eq!(s.close_unloaded_page(unloaded).await, 0);
    go.send(())?;
    let result = opening.await?;
    assert!(
        result.as_ref().is_err_and(|e| e.contains("reloaded")),
        "{result:?}"
    );
    assert!(s.sessions().is_empty(), "the late session was kept");
    for _ in 0..50 {
        if closed.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        closed.load(std::sync::atomic::Ordering::SeqCst),
        "the connection stayed open"
    );
    assert_eq!(s.reload_report().await.opening.len(), 1);
    // A session the new page opens is its own.
    let fresh = open_pg(&s, port("VARVE_M2_PG18_PORT", 55518)).await?;
    s.close_session(&fresh.id).await?;
    Ok(())
}
