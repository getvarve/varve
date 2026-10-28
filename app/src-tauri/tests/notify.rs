//! End-to-end tests of the PostgreSQL LISTEN / NOTIFY panel services
//! (`m2/notify.rs`) against PostgreSQL 13 and 18.
//!
//! Endpoints: `VARVE_LISTEN_PG13_PORT` / `VARVE_LISTEN_PG18_PORT` (defaults
//! 56713 and 56718) on `VARVE_LISTEN_HOST` (default 127.0.0.1), user
//! `varve`, password `varve_dev`, database `varve`. Run with
//! `cargo test -p varve-app --test notify -- --include-ignored`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use varve_activity::{ActivityEvent, ActivityFilter};
use varve_app::commands::{CmdError, SecretInput, Services};
use varve_app::m2::notify::{
    MAX_BATCH, MAX_UNACKED, NotifyBatch, NotifyEmitter, NotifyMessage, NotifyState, NotifyStatus,
};
use varve_app::sessions::{SessionError, SessionInfo};
use varve_core::Engine;
use varve_driver::{StatementResult, Value};
use varve_records::{ConnectionInput, ConnectionOptions, Environment};
use varve_secrets::MemoryStore;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Res<T> = Result<T, Box<dyn std::error::Error>>;

const WAIT: Duration = Duration::from_secs(10);

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
    let dir = std::env::temp_dir().join(format!("varve-notify-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let services = varve_app::build_services(store, &dir, &host, Arc::new(MemoryStore::default()))?;
    Ok(Fixture { services, dir })
}

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn servers() -> Vec<(u32, u16)> {
    vec![
        (13, port("VARVE_LISTEN_PG13_PORT", 56713)),
        (18, port("VARVE_LISTEN_PG18_PORT", 56718)),
    ]
}

fn input(port: u16, environment: Environment, read_only: bool) -> ConnectionInput {
    ConnectionInput {
        folder_id: None,
        name: format!("notify {port}"),
        color: None,
        engine: Engine::Postgres,
        host: std::env::var("VARVE_LISTEN_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: Some(port),
        database: Some("varve".into()),
        username: Some("varve".into()),
        environment,
        tls: Default::default(),
        ssh: Default::default(),
        options: ConnectionOptions {
            read_only,
            confirm_destructive: false,
            ..Default::default()
        },
    }
}

fn secret(value: &str) -> Res<SecretInput> {
    Ok(serde_json::from_value(serde_json::Value::String(
        value.into(),
    ))?)
}

async fn open(s: &Services, input: &ConnectionInput) -> Res<SessionInfo> {
    let profile = s.create_connection(input)?;
    Ok(
        s.open_session(&profile.id, Some(secret("varve_dev")?), None, None)
            .await?,
    )
}

async fn run(s: &Services, session: &SessionInfo, sql: &str) -> Res<Option<Value>> {
    let output = s.execute(&session.id, sql, true, None).await?;
    Ok(output.results.iter().find_map(|r| match r {
        StatementResult::Rows { rows, .. } => rows.first().and_then(|row| row.first()).cloned(),
        StatementResult::Command { .. } => None,
    }))
}

/// Collects what the listener would send to the window.
#[derive(Default)]
struct Collector {
    batches: Mutex<Vec<NotifyBatch>>,
    states: Mutex<Vec<NotifyState>>,
}

impl NotifyEmitter for Collector {
    fn messages(&self, batch: NotifyBatch) {
        if let Ok(mut batches) = self.batches.lock() {
            batches.push(batch);
        }
    }

    fn status(&self, state: NotifyState) {
        if let Ok(mut states) = self.states.lock() {
            states.push(state);
        }
    }
}

impl Collector {
    fn messages(&self) -> Vec<NotifyMessage> {
        self.batches
            .lock()
            .map(|b| b.iter().flat_map(|b| b.messages.clone()).collect())
            .unwrap_or_default()
    }

    async fn wait_messages(&self, count: usize) -> Res<Vec<NotifyMessage>> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let messages = self.messages();
            if messages.len() >= count {
                return Ok(messages);
            }
            if tokio::time::Instant::now() > deadline {
                return Err(format!("only {} of {count} messages arrived", messages.len()).into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_state(&self, test: impl Fn(&NotifyState) -> bool) -> Res<NotifyState> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let found = self
                .states
                .lock()
                .ok()
                .and_then(|s| s.iter().find(|s| test(s)).cloned());
            if let Some(state) = found {
                return Ok(state);
            }
            if tokio::time::Instant::now() > deadline {
                let states = self
                    .states
                    .lock()
                    .map(|s| format!("{s:?}"))
                    .unwrap_or_default();
                return Err(format!("no matching state in {states}").into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn emitter(collector: &Arc<Collector>) -> Arc<dyn NotifyEmitter> {
    Arc::clone(collector) as Arc<dyn NotifyEmitter>
}

fn events(s: &Services, connection_id: &str) -> Res<Vec<ActivityEvent>> {
    Ok(s.activity(&ActivityFilter {
        connection_id: Some(connection_id.to_string()),
        limit: 500,
        ..Default::default()
    })?
    .events)
}

fn int_of(value: Option<Value>) -> Option<i64> {
    match value? {
        Value::Int(i) => Some(i),
        Value::Text(t) => t.parse().ok(),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the listen test servers"]
async fn listens_receives_sends_and_logs() -> TestResult {
    for (major, port) in servers() {
        let fx = fixture(&format!("basic{major}"))?;
        let s = &fx.services;
        let tab = open(s, &input(port, Environment::Development, false)).await?;
        let collector = Arc::new(Collector::default());
        assert_eq!(s.notify_state(&tab.id)?.status, NotifyStatus::Stopped);

        let state = s
            .notify_listen(&tab.id, "Order Events", emitter(&collector))
            .await?;
        assert_eq!(state.status, NotifyStatus::Connected, "PostgreSQL {major}");
        assert_eq!(state.channels, ["Order Events"]);
        let listener_pid = state.backend_pid.ok_or("no backend pid")?;
        let tab_pid = int_of(run(s, &tab, "SELECT pg_backend_pid()").await?).ok_or("no tab pid")?;
        assert_ne!(i64::from(listener_pid), tab_pid, "never the tab's session");

        // Sent from the tab's own session inside its transaction: delivered
        // only at commit, which the listener (another connection) can see.
        run(s, &tab, "BEGIN").await?;
        run(s, &tab, "NOTIFY \"Order Events\", 'from the tab'").await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(collector.messages().is_empty(), "held until commit");
        run(s, &tab, "COMMIT").await?;
        let messages = collector.wait_messages(1).await?;
        assert_eq!(messages[0].payload, "from the tab");
        assert_eq!(i64::from(messages[0].pid), tab_pid);
        assert!(!messages[0].own);
        assert!(messages[0].received_at > 0);

        // Sent from the panel: outside the tab's transaction, marked own.
        run(s, &tab, "BEGIN").await?;
        let sent = s
            .notify_send(
                &tab.id,
                "Order Events",
                "it's me",
                false,
                emitter(&collector),
            )
            .await?;
        assert_eq!(sent.payload_bytes, 7);
        let messages = collector.wait_messages(2).await?;
        assert_eq!(messages[1].payload, "it's me");
        assert!(messages[1].own);
        assert_eq!(messages[1].pid, listener_pid);
        run(s, &tab, "ROLLBACK").await?;

        // Invalid input is refused before anything runs.
        let long = "x".repeat(64);
        assert!(matches!(
            s.notify_listen(&tab.id, &long, emitter(&collector)).await,
            Err(CmdError::Invalid { .. })
        ));
        assert!(matches!(
            s.notify_send(&tab.id, "c", &"p".repeat(8000), false, emitter(&collector))
                .await,
            Err(CmdError::Invalid { .. })
        ));

        let state = s
            .notify_unlisten(&tab.id, "Order Events", emitter(&collector))
            .await?;
        assert!(state.channels.is_empty());
        run(s, &tab, "NOTIFY \"Order Events\", 'late'").await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(collector.messages().len(), 2, "nothing after UNLISTEN");

        s.notify_stop(&tab.id).await?;
        assert_eq!(s.notify_state(&tab.id)?.status, NotifyStatus::Stopped);
        assert!(s.m2.notify.is_empty());
        let left = int_of(
            run(
                s,
                &tab,
                &format!("SELECT count(*) FROM pg_stat_activity WHERE pid = {listener_pid}"),
            )
            .await?,
        );
        assert_eq!(left, Some(0), "notify_stop waited for the server");
        // Every state carries a version; later states have higher ones.
        let versions: Vec<u64> = collector
            .states
            .lock()
            .map(|s| s.iter().map(|s| s.version).collect())
            .unwrap_or_default();
        assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");

        let logged = events(s, &tab.connection_id)?;
        let kinds: Vec<String> = logged
            .iter()
            .filter(|e| e.details.get("purpose").and_then(|p| p.as_str()) == Some("listen"))
            .map(|e| {
                let kind = e
                    .details
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or_default();
                format!("{}:{kind}", e.action)
            })
            .collect();
        for expected in [
            "connection.opened:",
            "query.executed:listen",
            "query.executed:notify",
            "query.executed:unlisten",
            "connection.closed:",
        ] {
            assert!(
                kinds.iter().any(|k| k == expected),
                "{expected} in {kinds:?}"
            );
        }
        // The payload literal is not stored at the default text level.
        let notify = logged
            .iter()
            .find(|e| e.details.get("kind").and_then(|k| k.as_str()) == Some("notify"))
            .ok_or("no notify event")?;
        let text = notify
            .details
            .get("statement")
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        assert!(!text.contains("it's me"), "{text}");
        s.close_session(&tab.id).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the listen test servers"]
async fn read_only_and_production_rules() -> TestResult {
    let (_, port) = servers()[0];
    let fx = fixture("rules")?;
    let s = &fx.services;
    let collector = Arc::new(Collector::default());

    let ro = open(s, &input(port, Environment::Development, true)).await?;
    s.notify_listen(&ro.id, "ro", emitter(&collector)).await?;
    let refused = s
        .notify_send(&ro.id, "ro", "x", true, emitter(&collector))
        .await;
    assert!(
        matches!(refused, Err(CmdError::ReadOnly { .. })),
        "{refused:?}"
    );
    s.close_session(&ro.id).await?;

    let prod = open(s, &input(port, Environment::Production, false)).await?;
    let asked = s
        .notify_send(&prod.id, "jobs", "go", false, emitter(&collector))
        .await;
    assert!(
        matches!(
            asked,
            Err(CmdError::Session {
                error: SessionError::ConfirmationRequired { .. }
            })
        ),
        "{asked:?}"
    );
    assert_eq!(
        s.notify_state(&prod.id)?.status,
        NotifyStatus::Stopped,
        "nothing was started before the confirmation"
    );
    s.notify_listen(&prod.id, "jobs", emitter(&collector))
        .await?;
    s.notify_send(&prod.id, "jobs", "go", true, emitter(&collector))
        .await?;
    assert_eq!(collector.wait_messages(1).await?[0].payload, "go");
    let actions: Vec<String> = events(s, &prod.connection_id)?
        .into_iter()
        .map(|e| e.action)
        .collect();
    assert!(actions.iter().any(|a| a == "destructive.blocked"));
    assert!(actions.iter().any(|a| a == "destructive.confirmed"));
    s.close_session(&prod.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the listen test servers"]
async fn closing_the_tab_stops_its_listener() -> TestResult {
    for (major, port) in servers() {
        let fx = fixture(&format!("close{major}"))?;
        let s = &fx.services;
        let tab = open(s, &input(port, Environment::Development, false)).await?;
        let watcher = open(s, &input(port, Environment::Development, false)).await?;
        let collector = Arc::new(Collector::default());
        let pid = s
            .notify_listen(&tab.id, "c", emitter(&collector))
            .await?
            .backend_pid
            .ok_or("no pid")?;
        s.close_session(&tab.id).await?;
        let stopped = collector
            .wait_state(|st| st.status == NotifyStatus::Stopped)
            .await?;
        assert!(stopped.error.is_some(), "says why: {stopped:?}");
        let mut gone = false;
        for _ in 0..50 {
            let left = int_of(
                run(
                    s,
                    &watcher,
                    &format!("SELECT count(*) FROM pg_stat_activity WHERE pid = {pid}"),
                )
                .await?,
            );
            if left == Some(0) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(gone, "PostgreSQL {major}: the listener connection closed");
        assert!(s.m2.notify.is_empty());
        s.close_session(&watcher.id).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the listen test servers"]
async fn reconnects_after_the_backend_is_terminated() -> TestResult {
    let (_, port) = servers()[1];
    let fx = fixture("reconnect")?;
    let s = &fx.services;
    let tab = open(s, &input(port, Environment::Development, false)).await?;
    let collector = Arc::new(Collector::default());
    let first = s
        .notify_listen(&tab.id, "again", emitter(&collector))
        .await?
        .backend_pid
        .ok_or("no pid")?;
    run(s, &tab, &format!("SELECT pg_terminate_backend({first})")).await?;
    collector
        .wait_state(|st| st.status == NotifyStatus::Reconnecting && st.error.is_some())
        .await?;
    let back = collector
        .wait_state(|st| st.status == NotifyStatus::Connected && st.gap)
        .await?;
    assert_ne!(back.backend_pid, Some(first));
    assert_eq!(back.channels, ["again"]);
    run(s, &tab, "NOTIFY again, 'after'").await?;
    assert_eq!(collector.wait_messages(1).await?[0].payload, "after");
    let actions: Vec<String> = events(s, &tab.connection_id)?
        .into_iter()
        .map(|e| e.action)
        .collect();
    assert!(actions.iter().any(|a| a == "connection.lost"));
    s.notify_stop(&tab.id).await?;
    s.close_session(&tab.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the listen test servers"]
async fn bursts_arrive_in_capped_batches() -> TestResult {
    let (_, port) = servers()[0];
    let fx = fixture("burst")?;
    let s = &fx.services;
    let tab = open(s, &input(port, Environment::Development, false)).await?;
    let collector = Arc::new(Collector::default());
    s.notify_listen(&tab.id, "burst", emitter(&collector))
        .await?;
    run(
        s,
        &tab,
        "SELECT count(pg_notify('burst', g::text)) FROM generate_series(1, 1200) AS g",
    )
    .await?;
    let messages = collector.wait_messages(1200).await?;
    assert_eq!(messages.len(), 1200);
    assert_eq!(messages[0].payload, "1");
    assert_eq!(messages[1199].payload, "1200");
    let batches = collector
        .batches
        .lock()
        .map(|b| b.clone())
        .unwrap_or_default();
    assert!(batches.len() >= 3);
    assert!(batches.iter().all(|b| b.messages.len() <= MAX_BATCH));
    assert!(batches.iter().all(|b| b.dropped == 0));
    s.notify_stop(&tab.id).await?;
    s.close_session(&tab.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the listen test servers"]
async fn unacknowledged_batches_hold_back_the_rest() -> TestResult {
    let (_, port) = servers()[1];
    let fx = fixture("ack")?;
    let s = &fx.services;
    let tab = open(s, &input(port, Environment::Development, false)).await?;
    let collector = Arc::new(Collector::default());
    s.notify_listen(&tab.id, "flood", emitter(&collector))
        .await?;
    let total = 3000;
    run(
        s,
        &tab,
        &format!("SELECT count(pg_notify('flood', g::text)) FROM generate_series(1, {total}) AS g"),
    )
    .await?;
    let held = usize::try_from(MAX_UNACKED)? * MAX_BATCH;
    collector.wait_messages(held).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        collector.messages().len(),
        held,
        "the rest waits for an ack"
    );
    let last = collector
        .batches
        .lock()
        .map(|b| b.last().map(|b| b.seq).unwrap_or_default())
        .unwrap_or_default();
    assert_eq!(last, MAX_UNACKED);
    let listener = collector
        .batches
        .lock()
        .map(|b| b.last().map(|b| b.listener_id).unwrap_or_default())
        .unwrap_or_default();
    // An ack for another listener, or beyond what was sent, lifts nothing.
    s.notify_ack(&tab.id, listener + 1000, last)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(collector.messages().len(), held, "another listener's ack");
    s.notify_ack(&tab.id, listener, 1)?;
    collector.wait_messages(held + MAX_BATCH).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        collector.messages().len(),
        held + MAX_BATCH,
        "one acked batch lets one more through"
    );
    s.notify_ack(&tab.id, listener, last)?;
    let messages = collector.wait_messages(total).await?;
    assert_eq!(messages.len(), total);
    let payloads: Vec<String> = messages.iter().map(|m| m.payload.clone()).collect();
    let expected: Vec<String> = (1..=total).map(|n| n.to_string()).collect();
    assert_eq!(payloads, expected, "in order, none lost");
    s.notify_stop(&tab.id).await?;
    s.close_session(&tab.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the listen test servers"]
async fn a_late_ack_of_a_stopped_listener_does_not_lift_the_limit_of_the_next() -> TestResult {
    let (_, port) = servers()[0];
    let fx = fixture("lateack")?;
    let s = &fx.services;
    let tab = open(s, &input(port, Environment::Development, false)).await?;
    let collector = Arc::new(Collector::default());
    let first = s.notify_listen(&tab.id, "l", emitter(&collector)).await?;
    s.notify_stop(&tab.id).await?;
    let second = s.notify_listen(&tab.id, "l", emitter(&collector)).await?;
    let (old, new) = (
        first.listener_id.ok_or("no id")?,
        second.listener_id.ok_or("no id")?,
    );
    assert_ne!(old, new);
    // A late ack of the old listener, with a large seq.
    s.notify_ack(&tab.id, old, 1000)?;
    // Even an ack of the new listener beyond what it sent counts for nothing yet.
    s.notify_ack(&tab.id, new, 1000)?;
    run(
        s,
        &tab,
        "SELECT count(pg_notify('l', g::text)) FROM generate_series(1, 3000) AS g",
    )
    .await?;
    let held = usize::try_from(MAX_UNACKED)? * MAX_BATCH;
    collector.wait_messages(held).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(collector.messages().len(), held, "the limit still holds");
    s.notify_stop(&tab.id).await?;
    s.close_session(&tab.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the listen test servers"]
async fn a_profile_moved_to_another_server_stops_the_listener() -> TestResult {
    let (_, port) = servers()[0];
    let (_, other) = servers()[1];
    let fx = fixture("moved")?;
    let s = &fx.services;
    let tab = open(s, &input(port, Environment::Development, false)).await?;
    let collector = Arc::new(Collector::default());
    s.notify_listen(&tab.id, "c", emitter(&collector)).await?;
    let moved = input(other, Environment::Development, false);
    s.update_connection(&tab.connection_id, &moved).await?;
    let stopped = collector
        .wait_state(|st| st.status == NotifyStatus::Stopped)
        .await?;
    let reason = stopped.error.map(|e| e.message).unwrap_or_default();
    assert!(reason.contains("reconnect"), "{reason}");
    // A new listen on the old tab is refused the same way.
    let again = s.notify_listen(&tab.id, "c", emitter(&collector)).await;
    assert!(
        matches!(again, Err(CmdError::ReconnectRequired { .. })),
        "{again:?}"
    );
    s.close_session(&tab.id).await?;
    Ok(())
}
