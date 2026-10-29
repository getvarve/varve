//! Pub/Sub through an SSH tunnel: a subscription whose connection is lost
//! comes back through the tunnel (m2/pubsub.rs). Uses the compose project
//! of the tunnel tests (`crates/tunnel/tests/docker-compose.yml`: an SSH
//! server forwarding to a Redis 7.4 that is only reachable through it):
//!
//! ```text
//! VARVE_TUNNEL_SSH_PORT=22022 docker compose -p <name> -f crates/tunnel/tests/docker-compose.yml up -d --wait
//! cargo test -p varve-app --test redis_b1_tunnel -- --include-ignored
//! docker compose -p <name> -f crates/tunnel/tests/docker-compose.yml down -v
//! ```
//!
//! The SSH endpoint comes from `VARVE_B1_SSH_HOST` (`127.0.0.1`) and
//! `VARVE_B1_SSH_PORT` (22022).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use varve_app::commands::{CmdError, SecretInput, Services};
use varve_app::m2::pubsub::{PubSubEmitter, PubSubEvent, PubSubNames, SubscribeOptions};
use varve_core::Engine;
use varve_driver::Value;
use varve_records::{ConnectionInput, Environment, SshAuthMethod, SshSettings};
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
    let dir = std::env::temp_dir().join(format!("varve-b1t-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let host = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &host)?);
    let services = varve_app::build_services(store, &dir, &host, Arc::new(MemoryStore::default()))?;
    Ok(Fixture { services, dir })
}

fn secret(value: &str) -> Result<SecretInput, serde_json::Error> {
    serde_json::from_value(serde_json::Value::String(value.into()))
}

#[derive(Default)]
struct Collect(Mutex<Vec<PubSubEvent>>);

impl PubSubEmitter for Collect {
    fn emit(&self, event: PubSubEvent) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

impl Collect {
    fn events(&self) -> Vec<PubSubEvent> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn wait(
        &self,
        what: &str,
        done: impl Fn(&[PubSubEvent]) -> bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for _ in 0..200 {
            if done(&self.events()) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(format!("timed out waiting for {what}: {:?}", self.events()).into())
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the tunnel compose project (crates/tunnel/tests/docker-compose.yml)"]
async fn pubsub_resubscribes_through_an_ssh_tunnel() -> TestResult {
    let f = fixture("ssh")?;
    let s = &*f.services;
    let input = ConnectionInput {
        folder_id: None,
        name: "Behind bastion b1".into(),
        color: None,
        engine: Engine::Redis,
        host: "redis".into(),
        port: Some(6379),
        database: None,
        username: None,
        environment: Environment::Development,
        tls: Default::default(),
        ssh: SshSettings {
            enabled: true,
            host: std::env::var("VARVE_B1_SSH_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("VARVE_B1_SSH_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(22022),
            username: "varve".into(),
            auth: SshAuthMethod::Password,
            key_path: None,
        },
        options: Default::default(),
    };
    let profile = s.create_connection(&input)?;
    // Trust the test server's host key, then connect through the tunnel.
    match s
        .open_session(&profile.id, None, Some(secret("varve_dev")?), None)
        .await
    {
        Err(CmdError::Tunnel {
            code,
            presented_key: Some(key),
            host: Some(host),
            port: Some(port),
            ..
        }) if code == "host_key_unknown" => s.trust_ssh_host_key(&host, port, &key).await?,
        other => return Err(format!("expected an unknown host key, got {other:?}").into()),
    }
    let session = s
        .open_session(&profile.id, None, Some(secret("varve_dev")?), None)
        .await?;

    let emitter = Arc::new(Collect::default());
    let sub = s
        .redis_pubsub_subscribe(
            &session.id,
            &PubSubNames {
                channels: vec!["b1t:c".into()],
                ..PubSubNames::default()
            },
            SubscribeOptions::default(),
            emitter.clone(),
        )
        .await?;
    // Subscribing returns once the server confirmed the name.
    assert_eq!(
        s.redis_pubsub_subscriptions(&session.id)?[0].channels,
        ["b1t:c"]
    );

    // The server drops the subscriber: it reconnects through the tunnel.
    s.execute(&session.id, "CLIENT KILL TYPE pubsub", true, None)
        .await?;
    emitter
        .wait("the gap", |events| {
            events.iter().any(|e| matches!(e, PubSubEvent::Gap { .. }))
        })
        .await?;
    let info = s.redis_pubsub_subscriptions(&session.id)?;
    assert_eq!(info[0].channels, ["b1t:c"]);
    assert_eq!(info[0].state, "subscribed");
    s.execute(&session.id, "PUBLISH b1t:c through", true, None)
        .await?;
    emitter
        .wait("the message", |events| {
            events.iter().any(|e| match e {
                PubSubEvent::Batch { messages, .. } => messages
                    .iter()
                    .any(|m| m.payload == Value::Text("through".into())),
                _ => false,
            })
        })
        .await?;
    s.redis_pubsub_unsubscribe(&sub.subscription_id);
    s.close_session(&session.id).await?;
    Ok(())
}
