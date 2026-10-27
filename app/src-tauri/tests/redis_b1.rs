//! End-to-end tests of the Redis value editors, consumer groups, sorted set
//! ranges and Pub/Sub services (m2/redis_edit.rs, m2/pubsub.rs) against
//! Redis 6.2, Redis 8.10 and Valkey 9 (the M2 server matrix by default).
//! Keys use the prefix `b1ipc:` in database 13 and are removed.
//!
//! Run with `cargo test -p varve-app --test redis_b1 -- --include-ignored`.
//! Ports: `VARVE_M2_REDIS62_PORT`, `VARVE_M2_REDIS810_PORT`,
//! `VARVE_M2_VALKEY9_PORT`; host `VARVE_M2_HOST`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use varve_activity::ActivityFilter;
use varve_app::commands::{CmdError, SecretInput, Services};
use varve_app::m2::pubsub::{PubSubEmitter, PubSubEvent, PubSubNames, SubscribeOptions};
use varve_app::m2::redis_edit::RedisEditRequest;
use varve_app::sessions::{SessionError, SessionInfo};
use varve_core::{Edition, Engine};
use varve_driver::Value;
use varve_driver_redis::{
    DigestMissing, EditOutcome, KeyEdit, KeyExpect, KeyType, KeyVersion, RangeBy,
    StreamGroupChange, TrimStrategy, ZsetBound, ZsetRange,
};
use varve_records::{ConnectionInput, ConnectionOptions, Environment};
use varve_secrets::MemoryStore;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// One test at a time: they share database 13.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let dir = std::env::temp_dir().join(format!("varve-b1-{name}-{}", std::process::id()));
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

fn targets() -> Vec<(&'static str, u16)> {
    vec![
        ("Redis 6.2", port("VARVE_M2_REDIS62_PORT", 64062)),
        ("Redis 8.10", port("VARVE_M2_REDIS810_PORT", 64081)),
        ("Valkey 9", port("VARVE_M2_VALKEY9_PORT", 64190)),
    ]
}

async fn open(
    s: &Services,
    name: &str,
    port: u16,
    environment: Environment,
    read_only: bool,
) -> Res<SessionInfo> {
    let input = ConnectionInput {
        folder_id: None,
        name: format!("{name} b1ipc"),
        color: None,
        engine: Engine::Redis,
        host: std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: Some(port),
        database: None,
        username: None,
        environment,
        tls: Default::default(),
        ssh: Default::default(),
        options: ConnectionOptions {
            read_only,
            confirm_destructive: false,
            redis_db: 13,
            ..Default::default()
        },
    };
    let profile = s.create_connection(&input)?;
    let secret: SecretInput = serde_json::from_value(serde_json::json!("varve_dev"))?;
    Ok(s.open_session(&profile.id, Some(secret), None, None)
        .await?)
}

async fn run(s: &Services, session: &SessionInfo, text: &str) -> Res<()> {
    s.execute(&session.id, text, true, None).await?;
    Ok(())
}

fn text(v: &str) -> Value {
    Value::Text(v.into())
}

fn needs_confirmation<T: std::fmt::Debug>(result: &Result<T, CmdError>) -> bool {
    matches!(
        result,
        Err(CmdError::Session {
            error: SessionError::ConfirmationRequired { .. }
        })
    )
}

fn actions(s: &Services, connection_id: &str) -> Res<Vec<String>> {
    let page = s.activity(&ActivityFilter {
        connection_id: Some(connection_id.to_string()),
        limit: 500,
        ..Default::default()
    })?;
    Ok(page.events.into_iter().map(|e| e.action).collect())
}

#[derive(Default)]
struct Collect(Mutex<Vec<(Instant, PubSubEvent)>>);

impl PubSubEmitter for Collect {
    fn emit(&self, event: PubSubEvent) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((Instant::now(), event));
    }
}

impl Collect {
    fn events(&self) -> Vec<PubSubEvent> {
        self.timed().into_iter().map(|(_, e)| e).collect()
    }

    /// Events with the moment each was sent.
    fn timed(&self) -> Vec<(Instant, PubSubEvent)> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn wait(&self, what: &str, done: impl Fn(&[PubSubEvent]) -> bool) -> Res<()> {
        for _ in 0..100 {
            if done(&self.events()) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(format!("timed out waiting for {what}: {:?}", self.events()).into())
    }
}

fn options(max_payload_bytes: u64) -> SubscribeOptions {
    SubscribeOptions {
        max_payload_bytes,
        resubscribe: true,
    }
}

fn batches(events: &[PubSubEvent]) -> Vec<(u64, usize)> {
    events
        .iter()
        .filter_map(|e| match e {
            PubSubEvent::Batch { seq, messages, .. } => Some((*seq, messages.len())),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn redis_edits_through_the_services() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("edits")?;
    let s = &*f.services;
    for (name, port) in targets() {
        let session = open(s, name, port, Environment::Production, false).await?;
        run(s, &session, "FLUSHDB").await?;
        run(s, &session, "HSET b1ipc:h a 1 b 2").await?;

        // A plan with an element removal on production needs confirmation;
        // refused, it stays available; confirmed, it runs once.
        let request = RedisEditRequest {
            key: "b1ipc:h".into(),
            expect: KeyExpect::Present {
                key_type: KeyType::Hash,
                version: None,
            },
            edits: vec![
                KeyEdit::HashSet {
                    field: text("a"),
                    value: text("10"),
                    original: Some(text("1")),
                },
                KeyEdit::HashDelete {
                    field: text("b"),
                    original: text("2"),
                },
            ],
        };
        let planned = s.redis_edit_plan(&session.id, &request).await?;
        assert!(planned.plan.destructive, "{name}");
        assert_eq!(
            planned.plan.commands,
            ["HSET b1ipc:h a 10", "HDEL b1ipc:h b"]
        );
        let refused = s
            .redis_edit_apply(&session.id, &planned.plan_id, false)
            .await;
        assert!(needs_confirmation(&refused), "{name}: {refused:?}");
        let out = s
            .redis_edit_apply(&session.id, &planned.plan_id, true)
            .await?;
        assert!(
            matches!(out, EditOutcome::Applied { .. }),
            "{name}: {out:?}"
        );
        let again = s
            .redis_edit_apply(&session.id, &planned.plan_id, true)
            .await;
        assert!(
            matches!(again, Err(CmdError::NotFound { .. })),
            "{name}: used once"
        );

        // A stale plan is a conflict and is logged as a failed edit.
        let stale = s.redis_edit_plan(&session.id, &request).await?;
        let out = s
            .redis_edit_apply(&session.id, &stale.plan_id, true)
            .await?;
        assert!(
            matches!(out, EditOutcome::Conflict { .. }),
            "{name}: {out:?}"
        );
        let log = actions(s, &session.connection_id)?;
        assert!(
            log.iter().filter(|a| *a == "data.edited").count() >= 2,
            "{name}: {log:?}"
        );
        assert!(log.contains(&"destructive.confirmed".to_string()), "{name}");
        assert!(log.contains(&"destructive.blocked".to_string()), "{name}");

        // Malformed requests are invalid; versions come from the session.
        let bad = RedisEditRequest {
            edits: vec![KeyEdit::Delete],
            ..request.clone()
        };
        assert!(matches!(
            s.redis_edit_plan(&session.id, &bad).await,
            Err(CmdError::Invalid { .. })
        ));
        let version = s.redis_key_version(&session.id, "b1ipc:h").await?;
        assert_eq!(version.as_ref().map(|v| v.length), Some(Some(1)));
        let delete = RedisEditRequest {
            expect: KeyExpect::Present {
                key_type: KeyType::Hash,
                version,
            },
            ..bad
        };
        let planned = s.redis_edit_plan(&session.id, &delete).await?;
        let out = s
            .redis_edit_apply(&session.id, &planned.plan_id, true)
            .await?;
        assert!(matches!(out, EditOutcome::Applied { .. }), "{name}");
        assert!(s.redis_key_version(&session.id, "b1ipc:h").await?.is_none());

        // Sorted set ranges and consumer groups through the services.
        run(s, &session, "ZADD b1ipc:z 1 a 2 b 3 c").await?;
        let page = s
            .redis_zset_range(
                &session.id,
                "b1ipc:z",
                &ZsetRange {
                    by: RangeBy::Score,
                    min: ZsetBound::Inclusive {
                        value: Value::Float(2.0),
                    },
                    max: ZsetBound::Unbounded,
                    reverse: false,
                    offset: 0,
                    count: 10,
                },
            )
            .await?
            .ok_or("no zset")?;
        assert_eq!(page.members.len(), 2, "{name}");
        let planned = s
            .redis_stream_group_plan(
                &session.id,
                "b1ipc:st",
                &StreamGroupChange::GroupCreate {
                    group: "g".into(),
                    id: "$".into(),
                    mk_stream: true,
                    entries_read: None,
                },
            )
            .await?;
        let out = s
            .redis_edit_apply(&session.id, &planned.plan_id, false)
            .await?;
        assert!(
            matches!(out, EditOutcome::Applied { .. }),
            "{name}: {out:?}"
        );
        assert_eq!(
            s.redis_stream_groups(&session.id, "b1ipc:st").await?.len(),
            1
        );
        assert_eq!(
            s.redis_stream_pending_summary(&session.id, "b1ipc:st", "g")
                .await?
                .count,
            0
        );

        // Read-only sessions refuse every change but read.
        let ro = open(s, name, port, Environment::Development, true).await?;
        assert!(matches!(
            s.redis_edit_plan(&ro.id, &request).await,
            Err(CmdError::ReadOnly { .. })
        ));
        assert!(matches!(
            s.redis_stream_group_plan(
                &ro.id,
                "b1ipc:st",
                &StreamGroupChange::GroupDestroy {
                    group: "g".into(),
                    expected_pending: 0
                }
            )
            .await,
            Err(CmdError::ReadOnly { .. })
        ));
        assert!(s.redis_stream_info(&ro.id, "b1ipc:st").await?.is_some());
        // A plan of one session cannot be applied on another.
        let planned = s.redis_edit_plan(&session.id, &request).await?;
        assert!(
            s.redis_edit_apply(&ro.id, &planned.plan_id, true)
                .await
                .is_err()
        );
        run(s, &session, "FLUSHDB").await?;
        s.close_session(&ro.id).await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn pubsub_through_the_services() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("pubsub")?;
    let s = &*f.services;
    for (name, port) in targets() {
        let session = open(s, name, port, Environment::Production, false).await?;
        let emitter = Arc::new(Collect::default());
        let names = PubSubNames {
            channels: vec!["b1ipc:c".into()],
            patterns: vec!["b1ipc:p.*".into()],
            shard_channels: vec![],
        };
        let sub = s
            .redis_pubsub_subscribe(&session.id, &names, options(8), emitter.clone())
            .await?;
        assert_eq!(sub.event, "redis-pubsub");
        emitter
            .wait("both confirmations", |events| {
                events
                    .iter()
                    .filter(|e| matches!(e, PubSubEvent::Subscribed { .. }))
                    .count()
                    == 2
            })
            .await?;

        // Publishing on production asks first, then is logged.
        let refused = s
            .redis_publish(&session.id, "b1ipc:c", &text("hello world"), false)
            .await;
        assert!(needs_confirmation(&refused), "{name}");
        assert_eq!(
            s.redis_publish(&session.id, "b1ipc:c", &text("hello world"), true)
                .await?,
            1
        );
        emitter
            .wait("the message", |events| !batches(events).is_empty())
            .await?;
        let first = emitter
            .events()
            .into_iter()
            .find_map(|e| match e {
                PubSubEvent::Batch { messages, .. } => messages.into_iter().next(),
                _ => None,
            })
            .ok_or("no message")?;
        assert_eq!(
            first.payload,
            text("hello wo"),
            "{name}: shortened to 8 bytes"
        );
        assert!(first.truncated);
        assert_eq!(first.size, "11");
        let log = actions(s, &session.connection_id)?;
        assert!(log.contains(&"query.executed".to_string()), "{name}");
        assert!(log.contains(&"destructive.confirmed".to_string()), "{name}");

        // Backpressure: without acknowledgements at most four batches go
        // out; an ack lets the rest through.
        let publisher = open(s, name, port, Environment::Development, false).await?;
        s.redis_pubsub_ack(&sub.subscription_id, 1)?;
        let script: String = (0..2500)
            .map(|i| format!("PUBLISH b1ipc:p.x m{i}\n"))
            .collect();
        run(s, &publisher, &script).await?;
        emitter
            .wait("four batches", |events| batches(events).len() >= 5)
            .await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let sent = batches(&emitter.events());
        assert_eq!(
            sent.len(),
            5,
            "{name}: one acked plus four in flight: {sent:?}"
        );
        // Acknowledging every batch shown lets all the rest through.
        let mut all = false;
        for _ in 0..200 {
            let sent = batches(&emitter.events());
            if sent.iter().map(|(_, n)| n).sum::<usize>() >= 2501 {
                all = true;
                break;
            }
            if let Some((seq, _)) = sent.last() {
                s.redis_pubsub_ack(&sub.subscription_id, *seq)?;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            all,
            "{name}: not all messages arrived: {:?}",
            batches(&emitter.events())
        );
        let list = s.redis_pubsub_subscriptions(&session.id)?;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].channels, ["b1ipc:c"]);
        assert_eq!(list[0].delivered, 2501, "{name}");

        // Changing names, then unsubscribing closes it once.
        s.redis_pubsub_change(
            &sub.subscription_id,
            &PubSubNames {
                channels: vec!["b1ipc:d".into()],
                ..PubSubNames::default()
            },
            &PubSubNames {
                channels: vec!["b1ipc:c".into()],
                ..PubSubNames::default()
            },
        )
        .await?;
        emitter
            .wait("the unsubscription", |events| {
                events
                    .iter()
                    .any(|e| matches!(e, PubSubEvent::Unsubscribed { .. }))
            })
            .await?;
        s.redis_pubsub_unsubscribe(&sub.subscription_id);
        s.redis_pubsub_unsubscribe(&sub.subscription_id);
        let closed = emitter
            .events()
            .iter()
            .filter(|e| matches!(e, PubSubEvent::Closed { .. }))
            .count();
        assert_eq!(closed, 1);
        assert!(matches!(
            s.redis_pubsub_ack(&sub.subscription_id, 1),
            Err(CmdError::NotFound { .. })
        ));

        // Closing the tab closes its subscriptions; caps hold.
        let second = Arc::new(Collect::default());
        s.redis_pubsub_subscribe(
            &publisher.id,
            &names,
            SubscribeOptions::default(),
            second.clone(),
        )
        .await?;
        assert!(matches!(
            s.redis_pubsub_subscribe(
                &publisher.id,
                &PubSubNames::default(),
                SubscribeOptions::default(),
                second.clone()
            )
            .await,
            Err(CmdError::Invalid { .. })
        ));
        s.close_session(&publisher.id).await?;
        second
            .wait("closed", |events| {
                events
                    .iter()
                    .any(|e| matches!(e, PubSubEvent::Closed { .. }))
            })
            .await?;
        assert!(s.m2.pubsub.is_empty(), "{name}");

        // Read-only sessions subscribe but do not publish.
        let ro = open(s, name, port, Environment::Development, true).await?;
        assert!(matches!(
            s.redis_publish(&ro.id, "b1ipc:c", &text("x"), true).await,
            Err(CmdError::ReadOnly { .. })
        ));
        let third = Arc::new(Collect::default());
        let sub = s
            .redis_pubsub_subscribe(&ro.id, &names, SubscribeOptions::default(), third.clone())
            .await?;
        s.redis_pubsub_unsubscribe(&sub.subscription_id);
        s.close_session(&ro.id).await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}

/// Sets the `pubsub` class of `client-output-buffer-limit` and returns the
/// value to restore.
async fn set_pubsub_limit(s: &Services, session: &SessionInfo, limit: &str) -> Res<String> {
    let config = s
        .redis_config(&session.id, Some("client-output-buffer-limit"))
        .await?;
    let old = config
        .first()
        .and_then(|entry| {
            entry
                .value
                .as_deref()
                .and_then(varve_driver_redis::pubsub_limit)
        })
        .ok_or("no pubsub limit")?;
    run(
        s,
        session,
        &format!("CONFIG SET client-output-buffer-limit \"pubsub {limit}\""),
    )
    .await?;
    Ok(old)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn pubsub_resubscribes_after_the_server_drops_it() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("drop")?;
    let s = &*f.services;
    for (name, port) in targets() {
        let session = open(s, name, port, Environment::Development, false).await?;
        let emitter = Arc::new(Collect::default());
        let names = PubSubNames {
            channels: vec!["b1ipc:drop".into()],
            ..PubSubNames::default()
        };
        let sub = s
            .redis_pubsub_subscribe(
                &session.id,
                &names,
                SubscribeOptions::default(),
                emitter.clone(),
            )
            .await?;
        emitter
            .wait("the confirmation", |events| {
                events
                    .iter()
                    .any(|e| matches!(e, PubSubEvent::Subscribed { .. }))
            })
            .await?;
        // A hard limit of 64 KiB and 20000 messages published at once (a
        // script): the server drops the subscriber, however fast Varve
        // reads. (Valkey shares one payload between its subscribers' replies
        // and counts per message, so the burst is many messages.)
        let old = set_pubsub_limit(s, &session, "65536 65536 0").await?;
        let dropped = async {
            run(
                s,
                &session,
                "EVAL \"for i = 1, 20000 do redis.call('PUBLISH', KEYS[1], string.rep('x', 1024)) \
                 end\" 1 b1ipc:drop",
            )
            .await?;
            emitter
                .wait("reconnecting", |events| {
                    events
                        .iter()
                        .any(|e| matches!(e, PubSubEvent::Reconnecting { .. }))
                })
                .await
        }
        .await;
        run(
            s,
            &session,
            &format!("CONFIG SET client-output-buffer-limit \"pubsub {old}\""),
        )
        .await?;
        dropped.map_err(|e| format!("{name}: {e}"))?;
        let reason = emitter
            .events()
            .into_iter()
            .find_map(|e| match e {
                PubSubEvent::Reconnecting { reason, .. } => Some(reason),
                _ => None,
            })
            .ok_or("no reason")?;
        if name == "Redis 6.2" {
            assert!(
                reason.contains("may have dropped the subscription"),
                "{name}: {reason}"
            );
        } else {
            // Certain when messages were arriving until the drop. A burst
            // from one script can get the subscriber dropped before its
            // first byte was sent: then the server's count cannot tell it
            // from another client's drop, and the cause is given as
            // unknown, naming the limit.
            assert!(
                reason.starts_with(
                    "The server dropped the subscription because its pubsub output buffer limit was exceeded"
                ) || reason.contains("The cause is unknown"),
                "{name}: {reason}"
            );
            assert!(reason.contains("pubsub 65536 65536 0"), "{name}: {reason}");
        }
        assert!(reason.contains("CONFIG SET client-output-buffer-limit"));
        emitter
            .wait("the gap", |events| {
                events.iter().any(|e| matches!(e, PubSubEvent::Gap { .. }))
            })
            .await?;
        emitter
            .wait("the new confirmation", |events| {
                events
                    .iter()
                    .filter(|e| matches!(e, PubSubEvent::Subscribed { .. }))
                    .count()
                    == 2
            })
            .await?;
        assert_eq!(
            s.redis_pubsub_subscriptions(&session.id)?[0].state,
            "subscribed"
        );
        run(s, &session, "PUBLISH b1ipc:drop after").await?;
        emitter
            .wait("a message after the gap", |events| {
                events.iter().any(|e| match e {
                    PubSubEvent::Batch { messages, .. } => {
                        messages.iter().any(|m| m.payload == text("after"))
                    }
                    _ => false,
                })
            })
            .await?;

        // Without resubscribe, a dropped connection closes with the reason.
        let quiet = Arc::new(Collect::default());
        let once = s
            .redis_pubsub_subscribe(
                &session.id,
                &names,
                SubscribeOptions {
                    resubscribe: false,
                    ..SubscribeOptions::default()
                },
                quiet.clone(),
            )
            .await?;
        run(s, &session, "CLIENT KILL TYPE pubsub").await?;
        quiet
            .wait("closed", |events| {
                events
                    .iter()
                    .any(|e| matches!(e, PubSubEvent::Closed { .. }))
            })
            .await?;
        assert!(matches!(
            s.redis_pubsub_ack(&once.subscription_id, 1),
            Err(CmdError::NotFound { .. })
        ));
        // The resubscribing one survives the kill too.
        emitter
            .wait("a second gap", |events| {
                events
                    .iter()
                    .filter(|e| matches!(e, PubSubEvent::Gap { .. }))
                    .count()
                    == 2
            })
            .await?;
        s.redis_pubsub_unsubscribe(&sub.subscription_id);

        // Sharded channels need Redis 7.0; names change only once the
        // server confirms them.
        let sharded = PubSubNames {
            shard_channels: vec!["b1ipc:shard".into()],
            ..PubSubNames::default()
        };
        let result = s
            .redis_pubsub_subscribe(
                &session.id,
                &sharded,
                SubscribeOptions::default(),
                emitter.clone(),
            )
            .await;
        if name == "Redis 6.2" {
            assert!(matches!(result, Err(CmdError::Invalid { .. })), "{name}");
        } else {
            let sub = result?;
            emitter
                .wait("the shard confirmation", |events| {
                    events.iter().any(|e| {
                        matches!(e, PubSubEvent::Subscribed { name_kind, .. }
                            if *name_kind == varve_driver_redis::NameKind::ShardChannel)
                    })
                })
                .await?;
            let info = s.redis_pubsub_subscriptions(&session.id)?;
            assert_eq!(info[0].shard_channels, ["b1ipc:shard"], "{name}");
            s.redis_pubsub_unsubscribe(&sub.subscription_id);
        }
        s.close_session(&session.id).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn unchecked_whole_key_changes_ask_first() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("unchecked")?;
    let s = &*f.services;
    for (name, port) in targets() {
        let admin = open(s, name, port, Environment::Development, false).await?;
        run(
            s,
            &admin,
            "ACL SETUSER b1ipc_np on >pw ~* &* +@all -getrange -lrange -xrange -hscan -sscan -zscan -dump",
        )
        .await?;
        run(s, &admin, "SET b1ipc:np v").await?;
        let input = ConnectionInput {
            folder_id: None,
            name: format!("{name} b1ipc np"),
            color: None,
            engine: Engine::Redis,
            host: std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: Some(port),
            database: None,
            username: Some("b1ipc_np".into()),
            environment: Environment::Development,
            tls: Default::default(),
            ssh: Default::default(),
            options: ConnectionOptions {
                redis_db: 13,
                ..Default::default()
            },
        };
        let profile = s.create_connection(&input)?;
        let secret: SecretInput = serde_json::from_value(serde_json::json!("pw"))?;
        let limited = s
            .open_session(&profile.id, Some(secret), None, None)
            .await?;
        let version = s.redis_key_version(&limited.id, "b1ipc:np").await?;
        assert_eq!(version.as_ref().and_then(|v| v.digest.clone()), None);
        let request = RedisEditRequest {
            key: "b1ipc:np".into(),
            expect: KeyExpect::Present {
                key_type: KeyType::String,
                version,
            },
            edits: vec![KeyEdit::Delete],
        };
        let planned = s.redis_edit_plan(&limited.id, &request).await?;
        assert!(planned.plan.unchecked, "{name}");
        let refused = s
            .redis_edit_apply(&limited.id, &planned.plan_id, false)
            .await;
        match &refused {
            Err(CmdError::Session {
                error: SessionError::ConfirmationRequired { statements, .. },
            }) => assert!(
                statements.iter().any(|t| t.contains("cannot be checked")),
                "{name}: {statements:?}"
            ),
            other => panic!("{name}: {other:?}"),
        }
        let out = s
            .redis_edit_apply(&limited.id, &planned.plan_id, true)
            .await?;
        assert!(matches!(out, EditOutcome::Applied { .. }), "{name}");
        s.close_session(&limited.id).await?;
        run(s, &admin, "ACL DELUSER b1ipc_np").await?;
        s.close_session(&admin.id).await?;
    }
    Ok(())
}

/// A paused server stops answering: the keepalive notices within
/// KEEPALIVE_IDLE plus PING_TIMEOUT and the subscription comes back.
/// One server only (it takes about a minute).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn pubsub_keepalive_finds_a_silent_server() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("keepalive")?;
    let s = &*f.services;
    let (name, port) = targets()[1];
    let session = open(s, name, port, Environment::Development, false).await?;
    let emitter = Arc::new(Collect::default());
    let names = PubSubNames {
        channels: vec!["b1ipc:quiet".into()],
        ..PubSubNames::default()
    };
    let sub = s
        .redis_pubsub_subscribe(
            &session.id,
            &names,
            SubscribeOptions::default(),
            emitter.clone(),
        )
        .await?;
    emitter
        .wait("the confirmation", |events| {
            events
                .iter()
                .any(|e| matches!(e, PubSubEvent::Subscribed { .. }))
        })
        .await?;
    run(s, &session, "CLIENT PAUSE 50000 ALL").await?;
    for _ in 0..140 {
        if emitter
            .events()
            .iter()
            .any(|e| matches!(e, PubSubEvent::Gap { .. }))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let gap = emitter
        .events()
        .into_iter()
        .find_map(|e| match e {
            PubSubEvent::Gap { reason, .. } => Some(reason),
            _ => None,
        })
        .ok_or_else(|| format!("no gap: {:?}", emitter.events()))?;
    assert!(gap.contains("did not answer a PING"), "{gap}");
    // A network that went quiet is not the output buffer limit.
    assert!(!gap.contains("buffer"), "{gap}");
    s.redis_pubsub_unsubscribe(&sub.subscription_id);
    s.close_session(&session.id).await?;
    Ok(())
}

/// Measures a fast burst (100k messages of 2 KiB on a pattern and a
/// channel subscription, the UI acknowledging every batch) with the
/// server's default limits. Prints what happened; asserts that every
/// message is either delivered, counted as dropped, or inside a reported
/// gap whose reason names the output buffer limit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn pubsub_burst_measurement() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("burst")?;
    let s = &*f.services;
    for (name, port) in targets() {
        let session = open(s, name, port, Environment::Development, false).await?;
        let emitter = Arc::new(Collect::default());
        let names = PubSubNames {
            channels: vec!["b1ipc:burst".into()],
            patterns: vec!["b1ipc:b*".into()],
            ..PubSubNames::default()
        };
        let sub = s
            .redis_pubsub_subscribe(
                &session.id,
                &names,
                SubscribeOptions::default(),
                emitter.clone(),
            )
            .await?;
        emitter
            .wait("both confirmations", |events| {
                events
                    .iter()
                    .filter(|e| matches!(e, PubSubEvent::Subscribed { .. }))
                    .count()
                    == 2
            })
            .await?;
        // Ack every batch as it arrives.
        let acker = {
            let emitter = emitter.clone();
            let services = Arc::clone(&f.services);
            let id = sub.subscription_id.clone();
            tokio::spawn(async move {
                for _ in 0..1200 {
                    let last = batches(&emitter.events()).last().map(|(seq, _)| *seq);
                    if let Some(seq) = last {
                        let _ = services.redis_pubsub_ack(&id, seq);
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
        };
        // The worst case: the server publishes the whole burst at once.
        let payload = "p".repeat(2048);
        let started = std::time::Instant::now();
        run(
            s,
            &session,
            &format!(
                "EVAL \"for i = 1, 100000 do redis.call('PUBLISH', KEYS[1], ARGV[1]) end\" 1 \
                 b1ipc:burst {payload}"
            ),
        )
        .await?;
        let published = started.elapsed();
        tokio::time::sleep(Duration::from_secs(8)).await;
        acker.abort();
        let events = emitter.events();
        let delivered: usize = batches(&events).iter().map(|(_, n)| n).sum();
        let dropped: u64 = events
            .iter()
            .map(|e| match e {
                PubSubEvent::Batch { dropped, .. } => *dropped,
                _ => 0,
            })
            .sum();
        let gaps: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                PubSubEvent::Gap { reason, .. } | PubSubEvent::Closed { reason, .. } => {
                    Some(reason.clone())
                }
                _ => None,
            })
            .collect();
        println!(
            "{name}: published 100000 x 2 KiB (200000 deliveries) in {published:?}; delivered \
             {delivered}, dropped {dropped}, gaps {}: {:?}",
            gaps.len(),
            gaps.first()
        );
        if gaps.is_empty() {
            assert_eq!(delivered as u64 + dropped, 200_000, "{name}");
        } else {
            assert!(gaps[0].contains("output buffer limit"), "{name}: {gaps:?}");
        }
        s.redis_pubsub_unsubscribe(&sub.subscription_id);
        s.close_session(&session.id).await?;
    }
    Ok(())
}

/// A session of an ACL user `user` (password `pw`) on database 13.
async fn open_as(s: &Services, name: &str, port: u16, user: &str) -> Res<SessionInfo> {
    let input = ConnectionInput {
        folder_id: None,
        name: format!("{name} b1ipc {user}"),
        color: None,
        engine: Engine::Redis,
        host: std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: Some(port),
        database: None,
        username: Some(user.into()),
        environment: Environment::Development,
        tls: Default::default(),
        ssh: Default::default(),
        options: ConnectionOptions {
            redis_db: 13,
            ..Default::default()
        },
    };
    let profile = s.create_connection(&input)?;
    let secret: SecretInput = serde_json::from_value(serde_json::json!("pw"))?;
    Ok(s.open_session(&profile.id, Some(secret), None, None)
        .await?)
}

fn refused_names(events: &[PubSubEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            PubSubEvent::Error { name, .. } => name.clone(),
            _ => None,
        })
        .collect()
}

fn received(events: &[PubSubEvent], payload: &str) -> bool {
    events.iter().any(|e| match e {
        PubSubEvent::Batch { messages, .. } => messages.iter().any(|m| m.payload == text(payload)),
        _ => false,
    })
}

/// A name the server refuses is dropped and reported by name; the others
/// stay subscribed, also after the connection is lost (the server refuses
/// a SUBSCRIBE of several names as a whole when one is not permitted).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn pubsub_refused_names_are_dropped_and_the_rest_survives_reconnects() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("refused")?;
    let s = &*f.services;
    for (name, port) in targets() {
        let admin = open(s, name, port, Environment::Development, false).await?;
        run(
            s,
            &admin,
            "ACL SETUSER b1ipc_ch on >pw ~* resetchannels &b1ipc:ok* +@all",
        )
        .await?;
        let limited = open_as(s, name, port, "b1ipc_ch").await?;
        let emitter = Arc::new(Collect::default());
        let both = PubSubNames {
            channels: vec!["b1ipc:ok1".into(), "b1ipc:bad1".into()],
            ..PubSubNames::default()
        };
        // The call returns once the server answered for both names.
        let sub = s
            .redis_pubsub_subscribe(
                &limited.id,
                &both,
                SubscribeOptions::default(),
                emitter.clone(),
            )
            .await?;
        let info = s.redis_pubsub_subscriptions(&limited.id)?;
        assert_eq!(info[0].channels, ["b1ipc:ok1"], "{name}");
        assert_eq!(info[0].state, "subscribed");
        assert_eq!(refused_names(&emitter.events()), ["b1ipc:bad1"], "{name}");
        assert!(emitter.events().iter().any(|e| matches!(e,
            PubSubEvent::Error { code: Some(code), name_kind: Some(varve_driver_redis::NameKind::Channel), .. }
                if code == "NOPERM")));

        // A refused name added later is reported and dropped too.
        s.redis_pubsub_change(
            &sub.subscription_id,
            &PubSubNames {
                channels: vec!["b1ipc:bad2".into()],
                ..PubSubNames::default()
            },
            &PubSubNames::default(),
        )
        .await?;
        emitter
            .wait("the second refusal", |events| {
                refused_names(events).len() == 2
            })
            .await?;

        // The connection is lost: the permitted name comes back alone.
        run(s, &admin, "CLIENT KILL USER b1ipc_ch").await?;
        emitter
            .wait("the gap", |events| {
                events.iter().any(|e| matches!(e, PubSubEvent::Gap { .. }))
            })
            .await?;
        let info = s.redis_pubsub_subscriptions(&limited.id)?;
        assert_eq!(info[0].channels, ["b1ipc:ok1"], "{name}");
        assert_eq!(info[0].state, "subscribed", "{name}");
        assert_eq!(
            refused_names(&emitter.events()).len(),
            2,
            "{name}: no new refusal"
        );
        run(s, &admin, "PUBLISH b1ipc:ok1 after").await?;
        emitter
            .wait("a message after the gap", |events| {
                received(events, "after")
            })
            .await?;

        // Every name refused when subscribing again: the subscription
        // closes and says so (it is not listed as subscribed).
        run(s, &admin, "ACL SETUSER b1ipc_ch resetchannels").await?;
        run(s, &admin, "CLIENT KILL USER b1ipc_ch").await?;
        emitter
            .wait("closed", |events| {
                events
                    .iter()
                    .any(|e| matches!(e, PubSubEvent::Closed { .. }))
            })
            .await?;
        let reason = emitter
            .events()
            .into_iter()
            .find_map(|e| match e {
                PubSubEvent::Closed { reason, .. } => Some(reason),
                _ => None,
            })
            .ok_or("no reason")?;
        assert!(
            reason.contains("refused for every name"),
            "{name}: {reason}"
        );
        assert!(s.redis_pubsub_subscriptions(&limited.id)?.is_empty());

        // A first subscribe whose names are all refused fails.
        let refused = s
            .redis_pubsub_subscribe(
                &limited.id,
                &PubSubNames {
                    channels: vec!["b1ipc:bad3".into()],
                    ..PubSubNames::default()
                },
                SubscribeOptions::default(),
                Arc::new(Collect::default()),
            )
            .await;
        match &refused {
            Err(CmdError::Session {
                error: SessionError::Driver { error },
            }) => assert_eq!(error.code.as_deref(), Some("NOPERM"), "{name}"),
            other => panic!("{name}: {other:?}"),
        }
        assert!(s.redis_pubsub_subscriptions(&limited.id)?.is_empty());

        // A change that leaves no name (the last permitted one removed,
        // the added one refused) closes the subscription.
        run(s, &admin, "ACL SETUSER b1ipc_ch &b1ipc:ok*").await?;
        let last = Arc::new(Collect::default());
        let sub = s
            .redis_pubsub_subscribe(
                &limited.id,
                &PubSubNames {
                    channels: vec!["b1ipc:ok1".into()],
                    ..PubSubNames::default()
                },
                SubscribeOptions::default(),
                last.clone(),
            )
            .await?;
        s.redis_pubsub_change(
            &sub.subscription_id,
            &PubSubNames {
                channels: vec!["b1ipc:bad9".into()],
                ..PubSubNames::default()
            },
            &PubSubNames {
                channels: vec!["b1ipc:ok1".into()],
                ..PubSubNames::default()
            },
        )
        .await?;
        last.wait("closed", |events| {
            events.iter().any(|e| {
                matches!(e, PubSubEvent::Closed { reason, .. } if reason.contains("refused every name"))
            })
        })
        .await?;
        assert!(
            s.redis_pubsub_subscriptions(&limited.id)?.is_empty(),
            "{name}"
        );
        s.close_session(&limited.id).await?;
        run(s, &admin, "ACL DELUSER b1ipc_ch").await?;
        s.close_session(&admin.id).await?;
    }
    Ok(())
}

/// Waits until the background digest read of `version` finished.
async fn digest_read(s: &Services, session: &SessionInfo, version: &KeyVersion) -> Res<()> {
    let id = version.digest_read.as_deref().ok_or("no background read")?;
    for _ in 0..200 {
        if s.redis_key_digest_finished(&session.id, id) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(format!("the background read {id} did not finish").into())
}

/// Opens `key` in the viewer's way: its version, deferred for a large key,
/// with the background read finished.
async fn opened(s: &Services, session: &SessionInfo, key: &str) -> Res<KeyExpect> {
    let version = s.redis_key_version(&session.id, key).await?.ok_or("gone")?;
    assert_eq!(
        version.digest_missing,
        Some(DigestMissing::Deferred),
        "{key} is large"
    );
    digest_read(s, session, &version).await?;
    Ok(KeyExpect::Present {
        key_type: version.key_type.clone(),
        version: Some(version),
    })
}

/// Plans and applies (confirmed), returning the outcome.
async fn plan_apply(
    s: &Services,
    session: &SessionInfo,
    key: &str,
    expect: KeyExpect,
    edit: KeyEdit,
) -> Res<EditOutcome> {
    let planned = s
        .redis_edit_plan(
            &session.id,
            &RedisEditRequest {
                key: key.into(),
                expect,
                edits: vec![edit],
            },
        )
        .await?;
    assert!(
        !planned.plan.unchecked,
        "{key}: {:?}",
        planned.plan.warnings
    );
    Ok(s.redis_edit_apply(&session.id, &planned.plan_id, true)
        .await?)
}

fn version_changed(outcome: &EditOutcome) -> bool {
    matches!(
        outcome,
        EditOutcome::Conflict {
            reason: varve_driver_redis::ConflictReason::VersionChanged,
            ..
        }
    )
}

/// Large keys change by same-length writes (bitmaps, counters, capped lists
/// and streams). A change between opening the key and planning a whole-key
/// change is a conflict: the digest read right after opening catches it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn same_length_changes_after_opening_big_keys_are_conflicts() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("afteropen")?;
    let s = &*f.services;
    for (name, port) in targets() {
        let session = open(s, name, port, Environment::Development, false).await?;
        let other = open(s, name, port, Environment::Development, false).await?;
        run(s, &session, "FLUSHDB").await?;
        run(
            s,
            &session,
            "SETRANGE b1ipc:bits 2097151 x\n\
             EVAL \"for i = 1, 20000 do redis.call('HSET', KEYS[1], 'f' .. i, 0) end\" 1 b1ipc:ctr\n\
             EVAL \"for i = 1, 12000 do redis.call('RPUSH', KEYS[1], i % 10) end\" 1 b1ipc:cap\n\
             EVAL \"for i = 1, 12000 do redis.call('XADD', KEYS[1], '*', 'f', 'v') end\" 1 b1ipc:xs",
        )
        .await?;
        // A bitmap after SETBIT, then delete.
        let expect = opened(s, &session, "b1ipc:bits").await?;
        run(s, &other, "SETBIT b1ipc:bits 7 1").await?;
        let out = plan_apply(s, &session, "b1ipc:bits", expect, KeyEdit::Delete).await?;
        assert!(version_changed(&out), "{name}: bitmap: {out:?}");
        // A counter hash after HINCRBY, then rename.
        let expect = opened(s, &session, "b1ipc:ctr").await?;
        run(s, &other, "HINCRBY b1ipc:ctr f1 1").await?;
        let rename = KeyEdit::Rename {
            new_key: "b1ipc:ctr2".into(),
            replace: false,
        };
        let out = plan_apply(s, &session, "b1ipc:ctr", expect, rename).await?;
        assert!(version_changed(&out), "{name}: hash: {out:?}");
        // A capped list after LPUSH and LTRIM, then delete.
        let expect = opened(s, &session, "b1ipc:cap").await?;
        run(s, &other, "LPUSH b1ipc:cap 9\nLTRIM b1ipc:cap 0 11999").await?;
        let out = plan_apply(s, &session, "b1ipc:cap", expect, KeyEdit::Delete).await?;
        assert!(version_changed(&out), "{name}: list: {out:?}");
        // A capped stream after XADD MAXLEN, then trim.
        let expect = opened(s, &session, "b1ipc:xs").await?;
        run(s, &other, "XADD b1ipc:xs MAXLEN = 12000 * f v").await?;
        let trim = KeyEdit::StreamTrim {
            strategy: TrimStrategy::MaxLen,
            threshold: "11000".into(),
            approximate: false,
        };
        let out = plan_apply(s, &session, "b1ipc:xs", expect, trim).await?;
        assert!(version_changed(&out), "{name}: stream: {out:?}");
        // Unchanged, the same kind of plan applies.
        let expect = opened(s, &session, "b1ipc:cap").await?;
        let out = plan_apply(s, &session, "b1ipc:cap", expect, KeyEdit::Delete).await?;
        assert!(
            matches!(out, EditOutcome::Applied { .. }),
            "{name}: {out:?}"
        );
        run(s, &session, "FLUSHDB").await?;
        s.close_session(&other.id).await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}

/// Without the background digest (cancelled, or not finished), a whole-key
/// plan of a large key is unchecked: it asks first in every environment and
/// the question carries the warning; rename and trim too.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn whole_key_plans_without_the_digest_ask_first() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("nodigest")?;
    let s = &*f.services;
    for (name, port) in targets() {
        let session = open(s, name, port, Environment::Development, false).await?;
        run(
            s,
            &session,
            "DEL b1ipc:nd b1ipc:ndx\nSETRANGE b1ipc:nd 2097151 x\n\
             EVAL \"for i = 1, 12000 do redis.call('XADD', KEYS[1], '*', 'f', 'v') end\" 1 b1ipc:ndx",
        )
        .await?;
        for (key, edit) in [
            (
                "b1ipc:nd",
                KeyEdit::Rename {
                    new_key: "b1ipc:nd2".into(),
                    replace: false,
                },
            ),
            ("b1ipc:nd", KeyEdit::Delete),
            (
                "b1ipc:ndx",
                KeyEdit::StreamTrim {
                    strategy: TrimStrategy::MaxLen,
                    threshold: "10".into(),
                    approximate: false,
                },
            ),
        ] {
            let version = s.redis_key_version(&session.id, key).await?.ok_or("gone")?;
            let id = version.digest_read.clone().ok_or("no background read")?;
            s.redis_key_digest_cancel(&session.id, &id);
            let planned = s
                .redis_edit_plan(
                    &session.id,
                    &RedisEditRequest {
                        key: key.into(),
                        expect: KeyExpect::Present {
                            key_type: version.key_type.clone(),
                            version: Some(version),
                        },
                        edits: vec![edit],
                    },
                )
                .await?;
            assert!(planned.plan.unchecked, "{name}: {key}");
            let refused = s
                .redis_edit_apply(&session.id, &planned.plan_id, false)
                .await;
            match &refused {
                Err(CmdError::Session {
                    error: SessionError::ConfirmationRequired { statements, .. },
                }) => assert!(
                    // Cancelled: the exact reason, not "had not finished".
                    statements.iter().any(|t| t.contains("no longer kept")),
                    "{name}: {statements:?}"
                ),
                other => panic!("{name}: {key}: {other:?}"),
            }
        }
        run(s, &session, "DEL b1ipc:nd b1ipc:ndx").await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}

/// Opening a key never waits for, nor holds, the session: its version and
/// background digest read run on connections of their own, opened after the
/// session lock is released, so the console answers meanwhile.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn key_versions_leave_the_console_free() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("bounded")?;
    let s = &*f.services;
    let host = std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    for (name, port) in targets() {
        // New connections are slow to open (a remote bastion), the session's
        // own is not.
        let proxy = Proxy::start(host.clone(), port).await?;
        let session = open(s, name, proxy.port, Environment::Development, false).await?;
        run(s, &session, "DEL b1ipc:huge\nSET b1ipc:small v").await?;
        proxy.delay_new(800);
        let services = Arc::clone(&f.services);
        let id = session.id.clone();
        let version = tokio::spawn(async move {
            let version = services.redis_key_version(&id, "b1ipc:small").await;
            (version, Instant::now())
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        run(s, &session, "PING").await?;
        let pinged = Instant::now();
        let (version, versioned_at) = version.await?;
        assert!(version?.is_some(), "{name}");
        assert!(
            pinged < versioned_at,
            "{name}: the console waited for the version's connection"
        );
        proxy.delay_new(0);

        // A 128 MiB key: deferred at once; its full read runs in the
        // background while the console answers.
        run(s, &session, "SETRANGE b1ipc:huge 134217727 x").await?;
        let version = s
            .redis_key_version(&session.id, "b1ipc:huge")
            .await?
            .ok_or("gone")?;
        assert_eq!(version.digest, None, "{name}");
        assert_eq!(version.digest_missing, Some(DigestMissing::Deferred));
        let read = version.digest_read.clone().ok_or("no background read")?;
        run(s, &session, "PING").await?;
        assert!(
            !s.redis_key_digest_finished(&session.id, &read),
            "{name}: the console waited for the background read"
        );
        digest_read(s, &session, &version).await?;
        let planned = s
            .redis_edit_plan(
                &session.id,
                &RedisEditRequest {
                    key: "b1ipc:huge".into(),
                    expect: KeyExpect::Present {
                        key_type: KeyType::String,
                        version: Some(version),
                    },
                    edits: vec![KeyEdit::Delete],
                },
            )
            .await?;
        assert!(!planned.plan.unchecked, "{name}");
        assert!(
            planned
                .plan
                .checks
                .iter()
                .any(|c| c.contains("unchanged since it was read")),
            "{name}: {:?}",
            planned.plan.checks
        );
        run(s, &session, "DEL b1ipc:huge b1ipc:small").await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}

/// A subscription lost while another client was dropped for its output
/// buffer, but which was not receiving messages itself, is not said to
/// have been dropped for the buffer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn pubsub_blames_the_buffer_only_for_its_own_drop() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("blame")?;
    let s = &*f.services;
    for (name, port) in targets() {
        let session = open(s, name, port, Environment::Development, false).await?;
        let quiet = Arc::new(Collect::default());
        let loud = Arc::new(Collect::default());
        let mut subs = Vec::new();
        for (channel, emitter) in [("b1ipc:quiet", &quiet), ("b1ipc:loud", &loud)] {
            let names = PubSubNames {
                channels: vec![channel.into()],
                ..PubSubNames::default()
            };
            subs.push(
                s.redis_pubsub_subscribe(
                    &session.id,
                    &names,
                    SubscribeOptions::default(),
                    Arc::clone(emitter) as Arc<dyn PubSubEmitter>,
                )
                .await?,
            );
        }
        let reconnecting = |events: &[PubSubEvent]| {
            events
                .iter()
                .any(|e| matches!(e, PubSubEvent::Reconnecting { .. }))
        };
        let old = set_pubsub_limit(s, &session, "65536 65536 0").await?;
        let dropped = async {
            run(
                s,
                &session,
                "EVAL \"for i = 1, 20000 do redis.call('PUBLISH', KEYS[1], string.rep('x', 1024)) \
                 end\" 1 b1ipc:loud",
            )
            .await?;
            loud.wait("the loud one reconnecting", reconnecting).await?;
            // Right after: the quiet one is killed (the loud one waits for
            // its first attempt).
            run(s, &session, "CLIENT KILL TYPE pubsub").await?;
            quiet.wait("the quiet one reconnecting", reconnecting).await
        }
        .await;
        run(
            s,
            &session,
            &format!("CONFIG SET client-output-buffer-limit \"pubsub {old}\""),
        )
        .await?;
        dropped.map_err(|e| format!("{name}: {e}"))?;
        let reason = quiet
            .events()
            .into_iter()
            .find_map(|e| match e {
                PubSubEvent::Reconnecting { reason, .. } => Some(reason),
                _ => None,
            })
            .ok_or("no reason")?;
        assert!(
            reason.starts_with("The connection ended:"),
            "{name}: {reason}"
        );
        // Never the certain claim. Redis 6.2 does not count such drops, so
        // there the reason stays the hedge it always was.
        if name != "Redis 6.2" {
            assert!(reason.contains("The cause is unknown"), "{name}: {reason}");
        }
        for sub in subs {
            s.redis_pubsub_unsubscribe(&sub.subscription_id);
        }
        s.close_session(&session.id).await?;
    }
    Ok(())
}

/// A TCP proxy in front of a server that can stop answering: new
/// connections are accepted and never answered, and open ones can be cut.
struct Proxy {
    port: u16,
    hole: Arc<std::sync::atomic::AtomicBool>,
    /// Milliseconds a new connection waits before it is forwarded.
    delay: Arc<std::sync::atomic::AtomicU64>,
    conns: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
    accept: tokio::task::AbortHandle,
}

impl Proxy {
    async fn start(host: String, target: u16) -> Res<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let hole = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let delay = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let conns: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
        let accept = {
            let (hole, conns, delay) = (Arc::clone(&hole), Arc::clone(&conns), Arc::clone(&delay));
            tokio::spawn(async move {
                while let Ok((mut inbound, _)) = listener.accept().await {
                    let task = if hole.load(std::sync::atomic::Ordering::SeqCst) {
                        tokio::spawn(async move {
                            use tokio::io::AsyncReadExt;
                            let mut sink = [0u8; 4096];
                            while matches!(inbound.read(&mut sink).await, Ok(n) if n > 0) {}
                        })
                    } else {
                        let host = host.clone();
                        let wait = delay.load(std::sync::atomic::Ordering::SeqCst);
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(wait)).await;
                            if let Ok(mut out) =
                                tokio::net::TcpStream::connect((host.as_str(), target)).await
                            {
                                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut out).await;
                            }
                        })
                    };
                    conns
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(task.abort_handle());
                }
            })
            .abort_handle()
        };
        Ok(Self {
            port,
            hole,
            delay,
            conns,
            accept,
        })
    }

    fn delay_new(&self, ms: u64) {
        self.delay.store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    fn silent(&self, silent: bool) {
        self.hole.store(silent, std::sync::atomic::Ordering::SeqCst);
    }

    /// Closes every open connection.
    fn cut(&self) {
        for task in self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.accept.abort();
        self.cut();
    }
}

/// Against a server that accepts connections and never answers, the
/// attempts keep their schedule (1, 3, 7, 15 s after the loss), and the
/// subscription comes back once the server answers again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn pubsub_reconnects_on_schedule_against_a_silent_server() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("schedule")?;
    let s = &*f.services;
    let host = std::env::var("VARVE_M2_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    for (name, port) in targets() {
        let proxy = Proxy::start(host.clone(), port).await?;
        let session = open(s, name, proxy.port, Environment::Development, false).await?;
        let publisher = open(s, name, port, Environment::Development, false).await?;
        let emitter = Arc::new(Collect::default());
        let sub = s
            .redis_pubsub_subscribe(
                &session.id,
                &PubSubNames {
                    channels: vec!["b1ipc:sched".into()],
                    ..PubSubNames::default()
                },
                SubscribeOptions::default(),
                emitter.clone(),
            )
            .await?;
        proxy.silent(true);
        let lost = Instant::now();
        proxy.cut();
        let mut answered = false;
        let mut gap_at = None;
        while lost.elapsed() < Duration::from_secs(25) {
            if !answered && lost.elapsed() >= Duration::from_secs(10) {
                proxy.silent(false);
                answered = true;
            }
            gap_at = emitter
                .timed()
                .into_iter()
                .find_map(|(at, e)| matches!(e, PubSubEvent::Gap { .. }).then_some(at));
            if gap_at.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let attempts: Vec<(u32, f32)> = emitter
            .timed()
            .into_iter()
            .filter_map(|(at, e)| match e {
                PubSubEvent::Reconnecting { attempt, .. } => {
                    Some((attempt, (at - lost).as_secs_f32()))
                }
                _ => None,
            })
            .collect();
        let gap_at = gap_at.ok_or_else(|| format!("{name}: no gap: {attempts:?}"))?;
        // Attempt n is announced when attempt n-1 is cut off, at the start
        // of attempt n's slot; the first after the report (at most 1 s).
        let expected = [
            (1u32, 0.0f32, 1.6f32),
            (2, 2.9, 3.8),
            (3, 6.9, 7.8),
            (4, 14.9, 15.8),
        ];
        assert_eq!(
            attempts.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            [1, 2, 3, 4],
            "{name}: {attempts:?}"
        );
        for ((n, at), (_, from, to)) in attempts.iter().zip(expected) {
            assert!(
                (from..=to).contains(at),
                "{name}: attempt {n} at {at} s: {attempts:?}"
            );
        }
        let gap_after = (gap_at - lost).as_secs_f32();
        assert!(
            (15.0..17.0).contains(&gap_after),
            "{name}: gap at {gap_after} s"
        );
        run(s, &publisher, "PUBLISH b1ipc:sched back").await?;
        emitter
            .wait("a message after the gap", |events| received(events, "back"))
            .await?;
        s.redis_pubsub_unsubscribe(&sub.subscription_id);
        s.close_session(&publisher.id).await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}

/// After a console SELECT, a plan from the viewer of the old database is
/// refused: it never runs on the same-named key of the new database.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the M2 database matrix"]
async fn plans_after_a_console_select_are_refused() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let f = fixture("select")?;
    let s = &*f.services;
    for (name, port) in targets() {
        let session = open(s, name, port, Environment::Development, false).await?;
        run(
            s,
            &session,
            "SELECT 3\nSET b1ipc:same three\nSELECT 13\nSET b1ipc:same thirteen",
        )
        .await?;
        let version = s
            .redis_key_version(&session.id, "b1ipc:same")
            .await?
            .ok_or("gone")?;
        run(s, &session, "SELECT 3").await?;
        let refused = s
            .redis_edit_plan(
                &session.id,
                &RedisEditRequest {
                    key: "b1ipc:same".into(),
                    expect: KeyExpect::Present {
                        key_type: KeyType::String,
                        version: Some(version),
                    },
                    edits: vec![KeyEdit::Delete],
                },
            )
            .await;
        assert!(
            matches!(&refused, Err(CmdError::Invalid { message }) if message.contains("read on database 13")),
            "{name}: {refused:?}"
        );
        run(s, &session, "DEL b1ipc:same\nSELECT 13\nDEL b1ipc:same").await?;
        s.close_session(&session.id).await?;
    }
    Ok(())
}
