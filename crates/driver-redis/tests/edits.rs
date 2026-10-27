//! Value editors, consumer groups, sorted set ranges and Pub/Sub against
//! the server matrix: Redis 6.2, Redis 8.10 and Valkey 9.
//!
//! Run with `cargo test -p varve-driver-redis --test edits -- --include-ignored`.
//! Servers come from `VARVE_REDIS_SERVERS` like in `tests/matrix.rs`
//! (`redis-6.2@127.0.0.1:63062,...`), by default the dev ports; the password
//! from `VARVE_REDIS_PASSWORD`. Tests use logical database 14 and keys
//! under `b1:`, which they remove.

use std::time::Duration;

use varve_core::Engine;
use varve_driver::{ConnectParams, DriverError, ErrorKind, ExecOptions, Session, TlsMode, Value};
use varve_driver_redis::{
    ConflictReason, EditError, EditOutcome, FieldValue, FullDigest, KeyEdit, KeyExpect, KeyType,
    KeyVersion, ListEnd, NameKind, PendingQuery, PubSubItem, RangeBy, RedisSession,
    StreamGroupChange, TrimStrategy, ValueData, ValuePage, ZsetBound, ZsetRange,
};
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const TEST_DB: u16 = 14;

/// The tests share database 14, so they run one at a time.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Server {
    label: String,
    major: u32,
    valkey: bool,
    host: String,
    port: u16,
}

fn servers() -> Vec<Server> {
    let list = std::env::var("VARVE_REDIS_SERVERS").unwrap_or_else(|_| {
        "redis-6.2@127.0.0.1:63062,redis-8.10@127.0.0.1:63081,valkey-9@127.0.0.1:63190".into()
    });
    list.split(',')
        .map(|item| {
            let (label, endpoint) = item
                .trim()
                .split_once('@')
                .unwrap_or_else(|| panic!("`{item}` must look like redis-7.2@host:port"));
            let (host, port) = endpoint
                .rsplit_once(':')
                .unwrap_or_else(|| panic!("`{endpoint}` must be host:port"));
            let (family, version) = label.split_once('-').unwrap_or((label, "0"));
            Server {
                label: label.to_owned(),
                major: version
                    .split('.')
                    .next()
                    .and_then(|m| m.parse().ok())
                    .unwrap_or(0),
                valkey: family == "valkey",
                host: host.to_owned(),
                port: port
                    .parse()
                    .unwrap_or_else(|_| panic!("bad port in {endpoint}")),
            }
        })
        .collect()
}

fn params(server: &Server, read_only: bool) -> ConnectParams {
    let mut p = ConnectParams::new(Engine::Redis, &server.host, server.port);
    p.password = Some(Secret::new(
        std::env::var("VARVE_REDIS_PASSWORD").unwrap_or_else(|_| "varve_dev".into()),
    ));
    p.redis_db = TEST_DB;
    p.read_only = read_only;
    p.tls.mode = TlsMode::Disable;
    p.connect_timeout = Duration::from_secs(5);
    p.statement_timeout = Some(Duration::from_secs(20));
    p.application_name = "Varve B1".into();
    p
}

async fn connect(server: &Server, read_only: bool) -> Result<RedisSession, DriverError> {
    RedisSession::connect(&params(server, read_only)).await
}

async fn run(session: &RedisSession, text: &str) -> Result<(), DriverError> {
    session
        .execute(text, &ExecOptions::default())
        .await
        .map(|_| ())
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn present(key_type: KeyType) -> KeyExpect {
    KeyExpect::Present {
        key_type,
        version: None,
    }
}

async fn apply(
    session: &RedisSession,
    key: &str,
    expect: &KeyExpect,
    edits: &[KeyEdit],
) -> Result<EditOutcome, Box<dyn std::error::Error>> {
    let program = session.plan_edit(key.as_bytes(), expect, edits).await?;
    Ok(session.apply_edit(&program).await?)
}

fn applied(outcome: &EditOutcome) -> bool {
    matches!(outcome, EditOutcome::Applied { .. })
}

fn conflict(outcome: &EditOutcome) -> Option<ConflictReason> {
    match outcome {
        EditOutcome::Conflict { reason, .. } => Some(*reason),
        _ => None,
    }
}

/// The key's expectation with the version read now.
async fn versioned(
    session: &RedisSession,
    key: &str,
) -> Result<KeyExpect, Box<dyn std::error::Error>> {
    let version = session
        .key_version(key.as_bytes())
        .await?
        .ok_or("the key does not exist")?;
    Ok(KeyExpect::Present {
        key_type: version.key_type.clone(),
        version: Some(version),
    })
}

/// The key's expectation as the app has it once the background digest
/// read finished: the open version, with the full digest read right after.
async fn versioned_full(
    session: &RedisSession,
    key: &str,
) -> Result<KeyExpect, Box<dyn std::error::Error>> {
    let mut version = session
        .key_version(key.as_bytes())
        .await?
        .ok_or("the key does not exist")?;
    if version.digest_missing == Some(varve_driver_redis::DigestMissing::Deferred) {
        let reader = session.version_reader().await;
        match reader
            .read_full(key.as_bytes(), &version, reader.full_read_limit())
            .await?
        {
            FullDigest::Done(digest) => {
                version.digest = Some(digest);
                version.digest_missing = None;
            }
            other => return Err(format!("no digest for {key}: {other:?}").into()),
        }
    }
    Ok(KeyExpect::Present {
        key_type: version.key_type.clone(),
        version: Some(version),
    })
}

/// The next Pub/Sub item, payloads shortened to 16 bytes, within 5 s.
async fn next(
    reader: &mut varve_driver_redis::PubSubReader,
) -> Result<Result<PubSubItem, DriverError>, tokio::time::error::Elapsed> {
    tokio::time::timeout(Duration::from_secs(5), reader.next(16)).await
}

async fn clean(session: &RedisSession) -> Result<(), DriverError> {
    // FLUSHDB is renamed away nowhere in this matrix, but the test database
    // is shared by these tests only.
    run(session, "FLUSHDB").await
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn value_edits_of_every_type() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let s = connect(&server, false).await?;
        clean(&s).await?;
        let label = &server.label;

        // Strings: create, edit (TTL kept), conflict on a stale original.
        let out = apply(
            &s,
            "b1:s",
            &KeyExpect::Absent,
            &[
                KeyEdit::StringSet {
                    value: text("one"),
                    original: None,
                },
                KeyEdit::Expire { ttl_ms: 600_000 },
            ],
        )
        .await?;
        assert!(applied(&out), "{label}: {out:?}");
        let again = apply(
            &s,
            "b1:s",
            &KeyExpect::Absent,
            &[KeyEdit::StringSet {
                value: text("x"),
                original: None,
            }],
        )
        .await?;
        assert_eq!(
            conflict(&again),
            Some(ConflictReason::TypeChanged),
            "{label}"
        );
        let out = apply(
            &s,
            "b1:s",
            &present(KeyType::String),
            &[KeyEdit::StringSet {
                value: Value::Bytes(vec![0, 255, 1]),
                original: Some(text("one")),
            }],
        )
        .await?;
        assert!(applied(&out), "{label}: {out:?}");
        let info = s.key_info(b"b1:s").await?.ok_or("gone")?;
        assert!(info.ttl_ms.is_some(), "{label}: KEEPTTL kept the TTL");
        let stale = apply(
            &s,
            "b1:s",
            &present(KeyType::String),
            &[KeyEdit::StringSet {
                value: text("lost"),
                original: Some(text("one")),
            }],
        )
        .await?;
        assert_eq!(
            conflict(&stale),
            Some(ConflictReason::ValueChanged),
            "{label}"
        );

        // Hashes: add, edit, delete; a stale original and an add of an
        // existing field conflict, and nothing of the plan ran.
        run(&s, "HSET b1:h a 1 b 2").await?;
        let out = apply(
            &s,
            "b1:h",
            &present(KeyType::Hash),
            &[
                KeyEdit::HashSet {
                    field: text("c"),
                    value: text("3"),
                    original: None,
                },
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
        )
        .await?;
        assert!(applied(&out), "{label}: {out:?}");
        let out = apply(
            &s,
            "b1:h",
            &present(KeyType::Hash),
            &[
                KeyEdit::HashSet {
                    field: text("d"),
                    value: text("4"),
                    original: None,
                },
                KeyEdit::HashSet {
                    field: text("c"),
                    value: text("x"),
                    original: None,
                },
            ],
        )
        .await?;
        match &out {
            EditOutcome::Conflict { edit, reason, .. } => {
                assert_eq!(*edit, Some(1), "{label}");
                assert_eq!(*reason, ConflictReason::ValueChanged);
            }
            other => panic!("{label}: {other:?}"),
        }
        let page = s
            .read_value(b"b1:h", &ValuePage::first(100))
            .await?
            .ok_or("hash gone")?;
        let ValueData::Hash { mut entries } = page.data else {
            panic!("{label}: not a hash");
        };
        entries.sort_by(|a, b| format!("{:?}", a.field).cmp(&format!("{:?}", b.field)));
        let flat: Vec<(Value, Value)> = entries.into_iter().map(|e| (e.field, e.value)).collect();
        assert_eq!(
            flat,
            [(text("a"), text("10")), (text("c"), text("3"))],
            "{label}: d was not added"
        );

        // Lists: indexes are those read, even with a push and a removal.
        run(&s, "RPUSH b1:l a b c d").await?;
        let out = apply(
            &s,
            "b1:l",
            &present(KeyType::List),
            &[
                KeyEdit::ListPush {
                    end: ListEnd::Head,
                    values: vec![text("new")],
                },
                KeyEdit::ListRemove {
                    index: 1,
                    original: text("b"),
                },
                KeyEdit::ListSet {
                    index: 3,
                    value: text("D"),
                    original: text("d"),
                },
            ],
        )
        .await?;
        assert!(applied(&out), "{label}: {out:?}");
        let page = s
            .read_value(b"b1:l", &ValuePage::first(100))
            .await?
            .ok_or("list gone")?;
        let ValueData::List { items } = page.data else {
            panic!("{label}: not a list");
        };
        let values: Vec<Value> = items.into_iter().map(|i| i.value).collect();
        assert_eq!(
            values,
            [text("new"), text("a"), text("c"), text("D")],
            "{label}"
        );

        // Sets and sorted sets.
        let out = apply(
            &s,
            "b1:set",
            &KeyExpect::Absent,
            &[KeyEdit::SetAdd {
                members: vec![text("x"), text("y")],
            }],
        )
        .await?;
        assert!(applied(&out), "{label}");
        let out = apply(
            &s,
            "b1:set",
            &present(KeyType::Set),
            &[KeyEdit::SetRemove { member: text("y") }],
        )
        .await?;
        assert!(applied(&out), "{label}");
        let out = apply(
            &s,
            "b1:set",
            &present(KeyType::Set),
            &[KeyEdit::SetRemove { member: text("y") }],
        )
        .await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::ValueChanged),
            "{label}"
        );

        run(&s, "ZADD b1:z 1 a 2 b").await?;
        let out = apply(
            &s,
            "b1:z",
            &present(KeyType::Zset),
            &[
                KeyEdit::ZsetAdd {
                    member: text("c"),
                    score: Value::Other("inf".into()),
                },
                KeyEdit::ZsetScore {
                    member: text("a"),
                    score: Value::Float(1.5),
                    original: Value::Float(1.0),
                },
                KeyEdit::ZsetRemove {
                    member: text("b"),
                    original: Value::Float(2.0),
                },
            ],
        )
        .await?;
        assert!(applied(&out), "{label}: {out:?}");
        let out = apply(
            &s,
            "b1:z",
            &present(KeyType::Zset),
            &[KeyEdit::ZsetScore {
                member: text("a"),
                score: Value::Float(9.0),
                original: Value::Float(1.0),
            }],
        )
        .await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::ValueChanged),
            "{label}"
        );

        // Streams: add (the reply is the id), delete, trim with a version.
        let out = apply(
            &s,
            "b1:x",
            &KeyExpect::Absent,
            &[KeyEdit::StreamAdd {
                id: Some("5-1".into()),
                fields: vec![FieldValue {
                    field: text("f"),
                    value: text("v"),
                }],
            }],
        )
        .await?;
        match &out {
            EditOutcome::Applied { replies } => assert_eq!(replies, &[text("5-1")], "{label}"),
            other => panic!("{label}: {other:?}"),
        }
        for id in ["6-0", "7-0", "8-0"] {
            run(&s, &format!("XADD b1:x {id} f v")).await?;
        }
        let out = apply(
            &s,
            "b1:x",
            &present(KeyType::Stream),
            &[KeyEdit::StreamDelete { id: "6-0".into() }],
        )
        .await?;
        assert!(applied(&out), "{label}");
        let version = s.key_version(b"b1:x").await?.ok_or("stream gone")?;
        assert_eq!(version.length, Some(3));
        assert!(version.digest.is_some(), "{label}: small keys get a digest");
        let trim = KeyExpect::Present {
            key_type: KeyType::Stream,
            version: Some(version.clone()),
        };
        run(&s, "XADD b1:x 9-0 f v").await?;
        let out = apply(
            &s,
            "b1:x",
            &trim,
            &[KeyEdit::StreamTrim {
                strategy: TrimStrategy::MaxLen,
                threshold: "1".into(),
                approximate: false,
            }],
        )
        .await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}"
        );
        let version = s.key_version(b"b1:x").await?.ok_or("stream gone")?;
        let out = apply(
            &s,
            "b1:x",
            &KeyExpect::Present {
                key_type: KeyType::Stream,
                version: Some(version),
            },
            &[KeyEdit::StreamTrim {
                strategy: TrimStrategy::MinId,
                threshold: "8-0".into(),
                approximate: false,
            }],
        )
        .await?;
        assert!(applied(&out), "{label}: {out:?}");
        assert_eq!(s.key_info(b"b1:x").await?.and_then(|i| i.length), Some(2));

        // Rename and delete need a version; a target that exists conflicts.
        run(&s, "SET b1:target t").await?;
        let version = s.key_version(b"b1:set").await?;
        let expect = KeyExpect::Present {
            key_type: KeyType::Set,
            version,
        };
        let out = apply(
            &s,
            "b1:set",
            &expect,
            &[KeyEdit::Rename {
                new_key: "b1:target".into(),
                replace: false,
            }],
        )
        .await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::TargetExists),
            "{label}"
        );
        let out = apply(
            &s,
            "b1:set",
            &expect,
            &[KeyEdit::Rename {
                new_key: "b1:set2".into(),
                replace: false,
            }],
        )
        .await?;
        assert!(applied(&out), "{label}");
        let version = s.key_version(b"b1:set2").await?;
        let out = apply(
            &s,
            "b1:set2",
            &KeyExpect::Present {
                key_type: KeyType::Set,
                version,
            },
            &[KeyEdit::Delete],
        )
        .await?;
        assert!(applied(&out), "{label}");
        assert!(s.key_info(b"b1:set2").await?.is_none());
        let out = apply(
            &s,
            "b1:h",
            &versioned(&s, "b1:h").await?,
            &[KeyEdit::Persist],
        )
        .await?;
        assert!(applied(&out), "{label}");

        // JSON where the server has it.
        if !server.valkey && server.major >= 8 {
            let out = apply(
                &s,
                "b1:j",
                &KeyExpect::Absent,
                &[KeyEdit::JsonSet {
                    document: r#"{"a":1}"#.into(),
                    original: None,
                }],
            )
            .await?;
            assert!(applied(&out), "{label}: {out:?}");
            let page = s
                .read_value(b"b1:j", &ValuePage::first(1))
                .await?
                .ok_or("json gone")?;
            let ValueData::Json { document } = page.data else {
                panic!("{label}: not JSON");
            };
            let original = match document {
                Value::Json(text) => text.as_str().to_string(),
                Value::Text(text) => text,
                other => panic!("{label}: {other:?}"),
            };
            let out = apply(
                &s,
                "b1:j",
                &present(KeyType::Json),
                &[KeyEdit::JsonSet {
                    document: r#"{"a":2}"#.into(),
                    original: Some(original.clone()),
                }],
            )
            .await?;
            assert!(applied(&out), "{label}: {out:?}");
            let out = apply(
                &s,
                "b1:j",
                &present(KeyType::Json),
                &[KeyEdit::JsonSet {
                    document: r#"{"a":3}"#.into(),
                    original: Some(original),
                }],
            )
            .await?;
            assert_eq!(
                conflict(&out),
                Some(ConflictReason::ValueChanged),
                "{label}"
            );
        }
        clean(&s).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn edits_keep_the_session_guarantees() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        clean(&s).await?;
        run(&s, "SET b1:k v").await?;

        // A console WATCH and MULTI stay untouched: the edit runs on its own
        // connection, and the user's transaction still commits.
        run(&s, "WATCH b1:other\nMULTI\nSET b1:queued 1").await?;
        let out = apply(
            &s,
            "b1:k",
            &present(KeyType::String),
            &[KeyEdit::StringSet {
                value: text("w"),
                original: Some(text("v")),
            }],
        )
        .await?;
        assert!(applied(&out), "{label}: {out:?}");
        run(&s, "EXEC").await?;
        let check = s.execute("GET b1:queued", &ExecOptions::default()).await?;
        assert!(format!("{check:?}").contains('1'), "{label}: {check:?}");

        // The edit connection is named and gone afterwards.
        let clients = s.execute("CLIENT LIST", &ExecOptions::default()).await?;
        assert!(
            !format!("{clients:?}").contains("Varve-B1-edit"),
            "{label}: the edit connection was closed"
        );

        // A change by another client after the plan was made is a
        // conflict, and nothing of the plan runs. (A change between the
        // checks and EXEC, which the WATCH catches, is covered by the unit
        // tests with a scripted server.)
        let other = connect(&server, false).await?;
        let program = s
            .plan_edit(
                b"b1:k",
                &present(KeyType::String),
                &[KeyEdit::StringSet {
                    value: text("mine"),
                    original: Some(text("w")),
                }],
            )
            .await?;
        run(&other, "SET b1:k theirs").await?;
        let out = s.apply_edit(&program).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::ValueChanged),
            "{label}"
        );

        // Read-only sessions refuse plans and applies, and the risk comes
        // from the server.
        let ro = connect(&server, true).await?;
        let refused = ro
            .plan_edit(b"b1:k", &present(KeyType::String), &[KeyEdit::Persist])
            .await;
        assert!(
            matches!(refused, Err(EditError::Driver(ref e)) if e.kind == ErrorKind::ReadOnly),
            "{label}: {refused:?}"
        );
        let refused = ro.apply_edit(&program).await;
        assert_eq!(refused.err().map(|e| e.kind), Some(ErrorKind::ReadOnly));
        let program = s
            .plan_edit(b"b1:k", &versioned(&s, "b1:k").await?, &[KeyEdit::Persist])
            .await?;
        assert_eq!(
            program.view().risk,
            varve_driver_redis::CommandRisk::Write,
            "{label}"
        );
        assert!(!program.view().destructive);

        // An oversized element value is never held when compared: a 20 MiB
        // field against a small original is a conflict, read in pieces.
        let big = "x".repeat(20 * 1024 * 1024);
        run(&other, &format!("HSET b1:big f {big}")).await?;
        let out = apply(
            &s,
            "b1:big",
            &present(KeyType::Hash),
            &[KeyEdit::HashDelete {
                field: text("f"),
                original: text("small"),
            }],
        )
        .await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::ValueChanged),
            "{label}"
        );
        // Opening the key does not read the 20 MiB value: its digest is
        // read in the background (by the app); without it a whole-key
        // change is unchecked.
        let version: KeyVersion = s.key_version(b"b1:big").await?.ok_or("gone")?;
        assert_eq!(version.digest, None, "{label}");
        assert_eq!(
            version.digest_missing,
            Some(varve_driver_redis::DigestMissing::Deferred)
        );
        assert_eq!(version.length, Some(1));
        let program = s
            .plan_edit(
                b"b1:big",
                &KeyExpect::Present {
                    key_type: KeyType::Hash,
                    version: Some(version),
                },
                &[KeyEdit::Delete],
            )
            .await?;
        assert!(program.view().unchecked, "{label}");
        clean(&s).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn sorted_set_ranges() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        clean(&s).await?;
        run(&s, "ZADD b1:z 1 a 2 b 3 c 4 d 5 e +inf f").await?;
        let inclusive = |v: f64| ZsetBound::Inclusive {
            value: Value::Float(v),
        };
        let mut range = ZsetRange {
            by: RangeBy::Score,
            min: ZsetBound::Exclusive {
                value: Value::Float(1.0),
            },
            max: ZsetBound::Unbounded,
            reverse: false,
            offset: 0,
            count: 2,
        };
        let mut seen = Vec::new();
        loop {
            let page = s.zset_range(b"b1:z", &range).await?.ok_or("gone")?;
            seen.extend(
                page.members
                    .iter()
                    .map(|m| (m.member.clone(), m.score.clone())),
            );
            match page.next_offset {
                Some(next) => range.offset = next.parse()?,
                None => break,
            }
        }
        let members: Vec<Value> = seen.iter().map(|(m, _)| m.clone()).collect();
        assert_eq!(
            members,
            ["b", "c", "d", "e", "f"].map(text),
            "{label}: exclusive min, paged"
        );
        assert_eq!(seen[4].1, Value::Other("inf".into()), "{label}");

        let rev = ZsetRange {
            by: RangeBy::Score,
            min: inclusive(2.0),
            max: inclusive(4.0),
            reverse: true,
            offset: 0,
            count: 10,
        };
        let page = s.zset_range(b"b1:z", &rev).await?.ok_or("gone")?;
        let members: Vec<Value> = page.members.into_iter().map(|m| m.member).collect();
        assert_eq!(members, ["d", "c", "b"].map(text), "{label}");
        assert_eq!(page.next_offset, None);

        run(&s, "ZADD b1:lex 0 apple 0 banana 0 cherry 0 date").await?;
        let lex = ZsetRange {
            by: RangeBy::Lex,
            min: ZsetBound::Inclusive {
                value: text("banana"),
            },
            max: ZsetBound::Exclusive {
                value: text("date"),
            },
            reverse: false,
            offset: 0,
            count: 10,
        };
        let page = s.zset_range(b"b1:lex", &lex).await?.ok_or("gone")?;
        let members: Vec<Value> = page.members.iter().map(|m| m.member.clone()).collect();
        assert_eq!(members, ["banana", "cherry"].map(text), "{label}");
        assert!(page.members.iter().all(|m| m.score == Value::Null));
        let lex_rev = ZsetRange {
            reverse: true,
            min: ZsetBound::Unbounded,
            max: ZsetBound::Unbounded,
            count: 1,
            ..lex
        };
        let page = s.zset_range(b"b1:lex", &lex_rev).await?.ok_or("gone")?;
        assert_eq!(page.members.len(), 1);
        assert_eq!(page.members[0].member, text("date"), "{label}");
        assert_eq!(page.next_offset.as_deref(), Some("1"));

        assert!(s.zset_range(b"b1:none", &rev).await?.is_none());
        run(&s, "SET b1:str x").await?;
        let wrong = s.zset_range(b"b1:str", &rev).await;
        assert_eq!(
            wrong.err().and_then(|e| e.code).as_deref(),
            Some("WRONGTYPE"),
            "{label}"
        );
        let bad = ZsetRange {
            min: ZsetBound::Inclusive {
                value: Value::Bool(true),
            },
            ..rev
        };
        let bad = s.zset_range(b"b1:z", &bad).await;
        assert_eq!(
            bad.err().and_then(|e| e.code).as_deref(),
            Some("INVALID_RANGE")
        );
        clean(&s).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn stream_consumer_groups() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        clean(&s).await?;
        let modern = server.valkey || server.major >= 7;

        let plan = |change: StreamGroupChange| {
            let s = &s;
            async move {
                let program = s.plan_stream_group(b"b1:st", &change).await?;
                Ok::<_, Box<dyn std::error::Error>>(s.apply_edit(&program).await?)
            }
        };
        let create = StreamGroupChange::GroupCreate {
            group: "g".into(),
            id: "0".into(),
            mk_stream: true,
            entries_read: None,
        };
        assert!(applied(&plan(create.clone()).await?), "{label}");
        assert!(
            conflict(&plan(create).await?).is_some(),
            "{label}: the group exists now"
        );
        for id in ["1-0", "2-0", "3-0"] {
            run(&s, &format!("XADD b1:st {id} f {}", "v".repeat(100))).await?;
        }
        run(&s, "XREADGROUP GROUP g alice COUNT 3 STREAMS b1:st >").await?;

        let info = s.stream_info(b"b1:st").await?.ok_or("gone")?;
        assert_eq!(info.length, 3);
        assert_eq!(info.groups, 1);
        assert_eq!(info.first_entry_id.as_deref(), Some("1-0"));
        assert_eq!(info.last_entry_id.as_deref(), Some("3-0"));
        assert_eq!(info.entries_added.is_some(), modern, "{label}");
        let groups = s.stream_groups(b"b1:st").await?;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].pending, 3);
        assert_eq!(groups[0].entries_read.is_some(), modern, "{label}");
        let consumers = s.stream_consumers(b"b1:st", "g").await?;
        assert_eq!(consumers.len(), 1);
        assert_eq!(consumers[0].name, "alice");
        let summary = s.stream_pending_summary(b"b1:st", "g").await?;
        assert_eq!(summary.count, 3);
        assert_eq!(summary.consumers[0].pending, 3);

        // Paging through pending entries.
        let mut query = PendingQuery {
            start: None,
            count: 2,
            consumer: None,
            min_idle_ms: None,
        };
        let first = s.stream_pending(b"b1:st", "g", &query).await?;
        assert_eq!(first.entries.len(), 2);
        assert_eq!(first.next_start.as_deref(), Some("(2-0"), "{label}");
        query.start = first.next_start;
        let second = s.stream_pending(b"b1:st", "g", &query).await?;
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.entries[0].id, "3-0");
        assert_eq!(second.next_start, None);
        query.min_idle_ms = Some(3_600_000);
        query.start = None;
        assert!(
            s.stream_pending(b"b1:st", "g", &query)
                .await?
                .entries
                .is_empty()
        );

        // Ack, claim, autoclaim, setid, delconsumer, destroy.
        let ack = StreamGroupChange::Ack {
            group: "g".into(),
            ids: vec!["1-0".into()],
        };
        assert!(applied(&plan(ack.clone()).await?), "{label}");
        assert_eq!(
            conflict(&plan(ack).await?),
            Some(ConflictReason::ValueChanged),
            "{label}: no longer pending"
        );
        let claim = StreamGroupChange::Claim {
            group: "g".into(),
            consumer: "bob".into(),
            min_idle_ms: 0,
            ids: vec!["2-0".into()],
            just_id: true,
        };
        assert!(applied(&plan(claim).await?), "{label}");
        let auto = StreamGroupChange::AutoClaim {
            group: "g".into(),
            consumer: "carol".into(),
            min_idle_ms: 0,
            start: "0-0".into(),
            count: 10,
            just_id: true,
        };
        match plan(auto).await? {
            EditOutcome::Applied { replies } => {
                let Value::Array(parts) = &replies[0] else {
                    panic!("{label}: {replies:?}");
                };
                assert_eq!(parts[0], text("0-0"), "{label}");
            }
            other => panic!("{label}: {other:?}"),
        }
        let del = StreamGroupChange::GroupDelConsumer {
            group: "g".into(),
            consumer: "alice".into(),
            expected_pending: 5,
        };
        assert!(
            conflict(&plan(del).await?).is_some(),
            "{label}: stale count"
        );
        let pending_alice = s
            .stream_pending_summary(b"b1:st", "g")
            .await?
            .consumers
            .iter()
            .find(|c| c.name == "alice")
            .map_or(0, |c| c.pending);
        let del = StreamGroupChange::GroupDelConsumer {
            group: "g".into(),
            consumer: "alice".into(),
            expected_pending: pending_alice,
        };
        let program = s.plan_stream_group(b"b1:st", &del).await?;
        assert!(program.view().destructive);
        assert!(applied(&s.apply_edit(&program).await?), "{label}");
        let setid = StreamGroupChange::GroupSetId {
            group: "g".into(),
            id: "$".into(),
            entries_read: None,
        };
        assert!(applied(&plan(setid).await?), "{label}");
        let pending = s.stream_pending_summary(b"b1:st", "g").await?.count;
        let destroy = StreamGroupChange::GroupDestroy {
            group: "g".into(),
            expected_pending: pending,
        };
        assert!(applied(&plan(destroy).await?), "{label}");
        let missing = s.stream_consumers(b"b1:st", "g").await;
        assert!(missing.is_err(), "{label}: the group is gone");

        // Read-only sessions refuse group changes but read.
        let ro = connect(&server, true).await?;
        let refused = ro
            .plan_stream_group(
                b"b1:st",
                &StreamGroupChange::Ack {
                    group: "g".into(),
                    ids: vec!["1-0".into()],
                },
            )
            .await;
        assert!(matches!(refused, Err(EditError::Driver(ref e)) if e.kind == ErrorKind::ReadOnly));
        assert!(ro.stream_info(b"b1:st").await?.is_some());
        assert!(ro.stream_info(b"b1:nothing").await?.is_none());
        assert!(ro.stream_groups(b"b1:nothing").await?.is_empty());
        clean(&s).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn pubsub_on_a_dedicated_connection() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        let (mut reader, mut writer) = s.pubsub_opener().await.open().await?;
        writer
            .subscribe(NameKind::Channel, &[b"b1:news".to_vec()])
            .await?;
        writer
            .subscribe(NameKind::Pattern, &[b"b1:p.*".to_vec()])
            .await?;
        assert!(
            matches!(
                next(&mut reader).await??,
                PubSubItem::Subscribed {
                    kind: NameKind::Channel,
                    count: 1,
                    ..
                }
            ),
            "{label}"
        );
        assert!(matches!(
            next(&mut reader).await??,
            PubSubItem::Subscribed {
                kind: NameKind::Pattern,
                count: 2,
                ..
            }
        ));
        // The session keeps working: it publishes, and long payloads arrive
        // shortened with their size.
        assert_eq!(s.publish(b"b1:news", b"hello").await?, 1, "{label}");
        let long = "z".repeat(1000);
        assert_eq!(s.publish(b"b1:p.x", long.as_bytes()).await?, 1);
        match next(&mut reader).await?? {
            PubSubItem::Message {
                pattern,
                channel,
                payload,
                size,
            } => {
                assert_eq!(pattern, None);
                assert_eq!(channel, b"b1:news");
                assert_eq!(payload, b"hello");
                assert_eq!(size, 5);
            }
            other => panic!("{label}: {other:?}"),
        }
        match next(&mut reader).await?? {
            PubSubItem::Message {
                pattern,
                payload,
                size,
                ..
            } => {
                assert_eq!(pattern.as_deref(), Some(b"b1:p.*".as_slice()));
                assert_eq!(payload.len(), 16);
                assert_eq!(size, 1000);
            }
            other => panic!("{label}: {other:?}"),
        }
        // Still in step after a shortened payload.
        writer.ping().await?;
        assert_eq!(next(&mut reader).await??, PubSubItem::Pong);
        writer
            .unsubscribe(NameKind::Channel, &[b"b1:news".to_vec()])
            .await?;
        assert!(matches!(
            next(&mut reader).await??,
            PubSubItem::Unsubscribed { count: 1, .. }
        ));
        assert_eq!(s.publish(b"b1:news", b"nobody").await?, 0);
        // The console still refuses SUBSCRIBE, and read-only sessions refuse
        // PUBLISH but may subscribe.
        let console = s.execute("SUBSCRIBE x", &ExecOptions::default()).await;
        assert_eq!(console.err().map(|e| e.kind), Some(ErrorKind::Unsupported));
        let ro = connect(&server, true).await?;
        assert_eq!(
            ro.publish(b"b1:news", b"x").await.err().map(|e| e.kind),
            Some(ErrorKind::ReadOnly)
        );
        let (mut ro_reader, mut ro_writer) = ro.pubsub_opener().await.open().await?;
        ro_writer
            .subscribe(NameKind::Channel, &[b"b1:ro".to_vec()])
            .await?;
        assert!(matches!(
            next(&mut ro_reader).await??,
            PubSubItem::Subscribed { .. }
        ));
        // A console MULTI makes PUBLISH refuse instead of queueing.
        run(&s, "MULTI").await?;
        let queued = s.publish(b"b1:news", b"x").await;
        assert_eq!(
            queued.err().and_then(|e| e.code).as_deref(),
            Some("MULTI_OPEN"),
            "{label}"
        );
        run(&s, "DISCARD").await?;
        // Dropping both halves closes the connection.
        drop((reader, writer, ro_reader, ro_writer));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let clients = s.execute("CLIENT LIST", &ExecOptions::default()).await?;
        let pubsub_clients = format!("{clients:?}").matches("Varve-B1-pubsub").count();
        assert_eq!(pubsub_clients, 0, "{label}");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn same_length_changes_of_big_keys_are_conflicts() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        let other = connect(&server, false).await?;
        clean(&s).await?;

        // A 5 MiB string changed in place (same length) after the plan:
        // delete conflicts. (Its digest is taken when the plan is made.)
        run(
            &other,
            &format!("SET b1:big:s {}", "a".repeat(5 * 1024 * 1024)),
        )
        .await?;
        let expect = versioned_full(&s, "b1:big:s").await?;
        run(&other, "SETRANGE b1:big:s 0 ZZZZ").await?;
        let program = s
            .plan_edit(b"b1:big:s", &expect, &[KeyEdit::Delete])
            .await?;
        assert!(!program.view().unchecked, "{label}");
        let out = s.apply_edit(&program).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}: string"
        );

        // A 20000-field hash with one field changed: delete conflicts.
        for chunk in 0..20 {
            let fields: String = (0..1000)
                .map(|i| format!(" f{} v", chunk * 1000 + i))
                .collect();
            run(&other, &format!("HSET b1:big:h{fields}")).await?;
        }
        let expect = versioned_full(&s, "b1:big:h").await?;
        run(&other, "HSET b1:big:h f1 CHANGED").await?;
        let program = s
            .plan_edit(b"b1:big:h", &expect, &[KeyEdit::Delete])
            .await?;
        let out = s.apply_edit(&program).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}: hash"
        );

        // 9000 fields of 1 KiB, one replaced by another (same length): a
        // rename conflicts.
        let value = "v".repeat(1024);
        for chunk in 0..9 {
            let fields: String = (0..1000)
                .map(|i| format!(" g{} {value}", chunk * 1000 + i))
                .collect();
            run(&other, &format!("HSET b1:big:k{fields}")).await?;
        }
        let expect = versioned_full(&s, "b1:big:k").await?;
        run(
            &other,
            &format!("HDEL b1:big:k g5\nHSET b1:big:k g9999 {value}"),
        )
        .await?;
        let program = s
            .plan_edit(
                b"b1:big:k",
                &expect,
                &[KeyEdit::Rename {
                    new_key: "b1:big:moved".into(),
                    replace: false,
                }],
            )
            .await?;
        let out = s.apply_edit(&program).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}: rename"
        );

        // Unchanged, the same plans apply; a list and a stream too.
        let expect = versioned_full(&s, "b1:big:h").await?;
        assert!(applied(
            &apply(&s, "b1:big:h", &expect, &[KeyEdit::Delete]).await?
        ));
        run(&other, "RPUSH b1:big:l a b c").await?;
        let expect = versioned(&s, "b1:big:l").await?;
        run(&other, "LSET b1:big:l 1 B").await?;
        let out = apply(&s, "b1:big:l", &expect, &[KeyEdit::Delete]).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}: list"
        );
        run(&other, "XADD b1:big:x 1-0 f v\nXADD b1:big:x 2-0 f v").await?;
        let expect = versioned(&s, "b1:big:x").await?;
        run(&other, "XDEL b1:big:x 1-0\nXADD b1:big:x 3-0 f v").await?;
        let out = apply(
            &s,
            "b1:big:x",
            &expect,
            &[KeyEdit::StreamTrim {
                strategy: TrimStrategy::MaxLen,
                threshold: "1".into(),
                approximate: false,
            }],
        )
        .await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}: stream"
        );
        // Sets and sorted sets: a member swapped for another.
        run(&other, "SADD b1:big:set a b\nZADD b1:big:z 1 a 2 b").await?;
        let set = versioned(&s, "b1:big:set").await?;
        let zset = versioned(&s, "b1:big:z").await?;
        run(
            &other,
            "SREM b1:big:set a\nSADD b1:big:set c\nZADD b1:big:z 3 a",
        )
        .await?;
        let out = apply(&s, "b1:big:set", &set, &[KeyEdit::Delete]).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}: set"
        );
        let out = apply(&s, "b1:big:z", &zset, &[KeyEdit::Delete]).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}: zset"
        );
        clean(&s).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn ttl_changes_are_conflicts() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        let other = connect(&server, false).await?;
        clean(&s).await?;
        run(&s, "SET b1:t v PX 500000").await?;
        let expect = versioned(&s, "b1:t").await?;
        run(&other, "PEXPIRE b1:t 999999").await?;
        let out = apply(&s, "b1:t", &expect, &[KeyEdit::Expire { ttl_ms: 50_000 }]).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}: expire"
        );
        let out = apply(&s, "b1:t", &expect, &[KeyEdit::Persist]).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}: persist"
        );
        // Read again, the same change applies; a key without a TTL too.
        let expect = versioned(&s, "b1:t").await?;
        assert!(applied(
            &apply(&s, "b1:t", &expect, &[KeyEdit::Expire { ttl_ms: 50_000 }]).await?
        ));
        run(&s, "SET b1:t2 v").await?;
        let expect = versioned(&s, "b1:t2").await?;
        run(&other, "EXPIRE b1:t2 100").await?;
        let out = apply(&s, "b1:t2", &expect, &[KeyEdit::Expire { ttl_ms: 50_000 }]).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}: no TTL before"
        );
        // A plain value change does not block a TTL change.
        let expect = versioned(&s, "b1:t").await?;
        run(&other, "SET b1:t w KEEPTTL").await?;
        assert!(
            applied(&apply(&s, "b1:t", &expect, &[KeyEdit::Persist]).await?),
            "{label}"
        );
        // Expire and persist need the version.
        let refused = s
            .plan_edit(b"b1:t", &present(KeyType::String), &[KeyEdit::Persist])
            .await;
        assert!(matches!(refused, Err(EditError::Invalid(_))), "{label}");
        clean(&s).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn group_checks_and_changes_are_atomic() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        let other = connect(&server, false).await?;
        clean(&s).await?;
        run(
            &s,
            "XADD b1:g 1-0 f v\nXADD b1:g 2-0 f v\nXGROUP CREATE b1:g g 0",
        )
        .await?;
        // Planned with 0 pending; a consumer reads before the apply (which
        // a WATCH would not notice): the script refuses.
        let destroy = s
            .plan_stream_group(
                b"b1:g",
                &StreamGroupChange::GroupDestroy {
                    group: "g".into(),
                    expected_pending: 0,
                },
            )
            .await?;
        assert!(
            destroy.view().commands[0].starts_with("XGROUP DESTROY b1:g g"),
            "{label}"
        );
        assert_eq!(destroy.command_names(), ["XGROUP"]);
        assert!(destroy.view().destructive, "{label}");
        run(&other, "XREADGROUP GROUP g alice COUNT 1 STREAMS b1:g >").await?;
        match s.apply_edit(&destroy).await? {
            EditOutcome::Conflict { message, .. } => {
                assert!(message.contains("now has 1 pending"), "{label}: {message}");
            }
            other => panic!("{label}: {other:?}"),
        }
        let del = s
            .plan_stream_group(
                b"b1:g",
                &StreamGroupChange::GroupDelConsumer {
                    group: "g".into(),
                    consumer: "alice".into(),
                    expected_pending: 1,
                },
            )
            .await?;
        run(&other, "XREADGROUP GROUP g alice COUNT 1 STREAMS b1:g >").await?;
        assert!(
            matches!(s.apply_edit(&del).await?, EditOutcome::Conflict { .. }),
            "{label}"
        );
        let del = s
            .plan_stream_group(
                b"b1:g",
                &StreamGroupChange::GroupDelConsumer {
                    group: "g".into(),
                    consumer: "alice".into(),
                    expected_pending: 2,
                },
            )
            .await?;
        assert!(applied(&s.apply_edit(&del).await?), "{label}");
        let destroy = s
            .plan_stream_group(
                b"b1:g",
                &StreamGroupChange::GroupDestroy {
                    group: "g".into(),
                    expected_pending: 0,
                },
            )
            .await?;
        assert!(applied(&s.apply_edit(&destroy).await?), "{label}");
        match s.apply_edit(&destroy).await? {
            EditOutcome::Conflict { message, .. } => {
                assert!(message.contains("no longer exists"), "{label}: {message}");
            }
            other => panic!("{label}: {other:?}"),
        }
        // Conflict texts say what changed.
        let ack = s
            .plan_stream_group(
                b"b1:g",
                &StreamGroupChange::Ack {
                    group: "nogroup".into(),
                    ids: vec!["1-0".into()],
                },
            )
            .await?;
        match s.apply_edit(&ack).await? {
            EditOutcome::Conflict { message, .. } => {
                assert!(
                    message.contains("is no longer pending"),
                    "{label}: {message}"
                );
            }
            other => panic!("{label}: {other:?}"),
        }
        clean(&s).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn side_connections_keep_their_names_and_users_without_piece_reads() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let admin = connect(&server, false).await?;
        clean(&admin).await?;
        // An init CLIENT SETNAME does not rename the Pub/Sub connection.
        let mut p = params(&server, false);
        p.init_sql = Some("CLIENT SETNAME custominit".into());
        let named = RedisSession::connect(&p).await?;
        let (_reader, _writer) = named.pubsub_opener().await.open().await?;
        let clients = admin
            .execute("CLIENT LIST", &ExecOptions::default())
            .await?;
        let text = format!("{clients:?}");
        assert!(text.contains("name=Varve-B1-pubsub"), "{label}: {text}");

        // An ACL user that may not read a key in pieces gets no digest, and
        // a whole-key change is marked unchecked with the reason.
        run(
            &admin,
            "ACL SETUSER b1nopiece on >pw ~* &* +@all -getrange -lrange -xrange -hscan -sscan -zscan -dump",
        )
        .await?;
        run(&admin, "SET b1:np value").await?;
        let mut p = params(&server, false);
        p.username = Some("b1nopiece".into());
        p.password = Some(Secret::new("pw".to_string()));
        let limited = RedisSession::connect(&p).await?;
        let version = limited.key_version(b"b1:np").await?.ok_or("gone")?;
        assert_eq!(version.digest, None, "{label}");
        assert_eq!(
            version.digest_missing,
            Some(varve_driver_redis::DigestMissing::NotPermitted)
        );
        let program = limited
            .plan_edit(
                b"b1:np",
                &KeyExpect::Present {
                    key_type: KeyType::String,
                    version: Some(version),
                },
                &[KeyEdit::Delete],
            )
            .await?;
        assert!(program.view().unchecked, "{label}");
        assert!(program.view().warnings[0].contains("may not read the key in pieces"));
        assert!(applied(&limited.apply_edit(&program).await?), "{label}");
        run(&admin, "ACL DELUSER b1nopiece").await?;

        // Strings over 4 MiB can be edited: the original is kept as a digest.
        let five = "a".repeat(5 * 1024 * 1024);
        run(&admin, &format!("SET b1:five {five}")).await?;
        let out = apply(
            &admin,
            "b1:five",
            &present(KeyType::String),
            &[KeyEdit::StringSet {
                value: Value::Text("b".repeat(5 * 1024 * 1024)),
                original: Some(Value::Text(five)),
            }],
        )
        .await?;
        assert!(applied(&out), "{label}: {out:?}");
        clean(&admin).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn sharded_pubsub_and_buffer_reports() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        let opener = s.pubsub_opener().await;
        let report = opener.output_buffer_report().await?;
        let modern = server.valkey || server.major >= 7;
        assert_eq!(
            report.disconnections.is_some(),
            modern,
            "{label}: {report:?}"
        );
        assert!(report.pubsub_limit.is_some(), "{label}: {report:?}");
        assert_eq!(opener.has_sharded(), modern);
        let (mut reader, mut writer) = opener.open().await?;
        writer
            .subscribe(NameKind::ShardChannel, &[b"b1:shard".to_vec()])
            .await?;
        if modern {
            assert!(matches!(
                next(&mut reader).await??,
                PubSubItem::Subscribed {
                    kind: NameKind::ShardChannel,
                    ..
                }
            ));
            run(&s, "SPUBLISH b1:shard hi").await?;
            match next(&mut reader).await?? {
                PubSubItem::Message {
                    channel, payload, ..
                } => {
                    assert_eq!(channel, b"b1:shard");
                    assert_eq!(payload, b"hi");
                }
                other => panic!("{label}: {other:?}"),
            }
        } else {
            assert!(
                matches!(next(&mut reader).await??, PubSubItem::Error(_)),
                "{label}"
            );
        }
    }
    Ok(())
}

/// Opening a key reads it only when it is small; a larger key is deferred,
/// its full digest is read on a connection of its own, and a whole-key
/// plan without it is unchecked.
#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn opening_a_key_reads_only_small_keys() -> TestResult {
    use varve_driver_redis::{DigestMissing, OPEN_DIGEST_ELEMENTS};
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        let other = connect(&server, false).await?;
        clean(&s).await?;

        // Small keys get their digest when opened.
        run(&other, "SET b1:open:small v\nRPUSH b1:open:list a b c").await?;
        for key in ["b1:open:small", "b1:open:list"] {
            let version = s.key_version(key.as_bytes()).await?.ok_or("gone")?;
            assert!(version.digest.is_some(), "{label}: {key}");
        }

        // A 64 MiB string (zero filled at once) is not read when opened.
        run(&other, "SETRANGE b1:open:big 67108863 x").await?;
        let started = std::time::Instant::now();
        let version = s.key_version(b"b1:open:big").await?.ok_or("gone")?;
        let took = started.elapsed();
        assert_eq!(version.digest, None, "{label}");
        assert_eq!(version.digest_missing, Some(DigestMissing::Deferred));
        assert_eq!(version.length, Some(64 * 1024 * 1024));
        assert!(took < Duration::from_millis(500), "{label}: {took:?}");
        // Nor a list of more elements than the open limit.
        let limit = usize::try_from(OPEN_DIGEST_ELEMENTS)?;
        let script = format!(
            "EVAL \"for i = 1, {} do redis.call('RPUSH', KEYS[1], i) end\" 1 b1:open:many",
            limit + 1
        );
        run(&other, &script).await?;
        let many = s.key_version(b"b1:open:many").await?.ok_or("gone")?;
        assert_eq!(
            many.digest_missing,
            Some(DigestMissing::Deferred),
            "{label}"
        );

        // Planned without the full digest: unchecked, with the reason; a
        // TTL change does not need it.
        let deferred = KeyExpect::Present {
            key_type: KeyType::String,
            version: Some(version.clone()),
        };
        let program = s
            .plan_edit(b"b1:open:big", &deferred, &[KeyEdit::Delete])
            .await?;
        assert!(program.view().unchecked, "{label}");
        assert!(
            program.view().warnings[0].contains("had not finished"),
            "{label}: {:?}",
            program.view().warnings
        );
        let ttl = s
            .plan_edit(b"b1:open:big", &deferred, &[KeyEdit::Persist])
            .await?;
        assert!(!ttl.view().unchecked, "{label}");

        // The full digest, read right after opening, catches a same-length
        // change made before the plan.
        let reader = s.version_reader().await;
        let full = reader
            .read_full(b"b1:open:big", &version, reader.full_read_limit())
            .await?;
        let FullDigest::Done(digest) = full else {
            return Err(format!("{label}: {full:?}").into());
        };
        let mut checked = version.clone();
        checked.digest = Some(digest);
        checked.digest_missing = None;
        run(&other, "SETRANGE b1:open:big 0 Z").await?;
        let program = s
            .plan_edit(
                b"b1:open:big",
                &KeyExpect::Present {
                    key_type: KeyType::String,
                    version: Some(checked),
                },
                &[KeyEdit::Delete],
            )
            .await?;
        assert!(!program.view().unchecked, "{label}");
        let out = s.apply_edit(&program).await?;
        assert_eq!(
            conflict(&out),
            Some(ConflictReason::VersionChanged),
            "{label}"
        );

        // The full read says when the key changed in length or is gone.
        run(&other, "APPEND b1:open:big y").await?;
        let changed = reader
            .read_full(b"b1:open:big", &version, reader.full_read_limit())
            .await?;
        assert!(
            matches!(&changed, FullDigest::Changed(m) if m.contains("length differs")),
            "{label}: {changed:?}"
        );
        run(&other, "DEL b1:open:big").await?;
        let gone = reader
            .read_full(b"b1:open:big", &version, reader.full_read_limit())
            .await?;
        assert!(
            matches!(&gone, FullDigest::Changed(m) if m.contains("no longer exists")),
            "{label}: {gone:?}"
        );
        clean(&s).await?;
    }
    Ok(())
}

/// A full read that gets no answer in time is a stall, not a key too
/// large to read.
#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn a_stalled_full_read_says_so() -> TestResult {
    use varve_driver_redis::DigestMissing;
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        let admin = connect(&server, false).await?;
        clean(&s).await?;
        run(&s, "SETRANGE b1:stall 2097151 x").await?;
        let version = s.key_version(b"b1:stall").await?.ok_or("gone")?;
        let reader = s.version_reader().await;
        run(&admin, "CLIENT PAUSE 1500 ALL").await?;
        let found = reader
            .read_full(b"b1:stall", &version, Duration::from_millis(400))
            .await?;
        assert_eq!(
            found,
            FullDigest::Missing(DigestMissing::Stalled),
            "{label}"
        );
        tokio::time::sleep(Duration::from_millis(1300)).await;
        clean(&s).await?;
    }
    Ok(())
}

/// A JSON key is read with DUMP when opened only when the O(1) size checks
/// (and JSON.DEBUG MEMORY) show it is small; in the full read only after
/// the same checks with larger limits, and a large one is not read. A JSON
/// plan warns about missing JSON commands only on servers without them.
#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn json_keys_are_dumped_only_when_small() -> TestResult {
    use varve_driver_redis::{DigestMissing, JSON_DIGEST_ELEMENTS, JSON_UNAVAILABLE};
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        let admin = connect(&server, false).await?;
        clean(&s).await?;
        let create = [KeyEdit::JsonSet {
            document: "[1]".into(),
            original: None,
        }];
        let program = s.plan_edit(b"b1:jnew", &KeyExpect::Absent, &create).await?;
        let has_json = s.has_json_commands().await? == Some(true);
        // Only Redis 8 ships the JSON type in this matrix.
        assert_eq!(has_json, !server.valkey && server.major >= 8, "{label}");
        assert_eq!(
            program
                .view()
                .warnings
                .iter()
                .any(|w| w == JSON_UNAVAILABLE),
            !has_json,
            "{label}: {:?}",
            program.view().warnings
        );
        if !has_json {
            continue;
        }
        run(&s, "JSON.SET b1:json $ [1,2,3]").await?;
        let script = format!(
            "EVAL \"redis.call('JSON.SET', KEYS[1], '$', '[]') for i = 1, {} do \
             redis.call('JSON.ARRAPPEND', KEYS[1], '$', i) end\" 1 b1:jbig",
            JSON_DIGEST_ELEMENTS + 1
        );
        run(&s, &script).await?;
        // A small document gets its digest when opened, so an early plan
        // is checked.
        let small = s.key_version(b"b1:json").await?.ok_or("gone")?;
        assert!(small.digest.is_some(), "{label}: {small:?}");
        let program = s
            .plan_edit(
                b"b1:json",
                &KeyExpect::Present {
                    key_type: KeyType::Json,
                    version: Some(small),
                },
                &[KeyEdit::Delete],
            )
            .await?;
        assert!(!program.view().unchecked, "{label}");
        assert!(program.view().warnings.is_empty(), "{label}");
        // A large one is not dumped when opened.
        run(
            &admin,
            "CONFIG SET slowlog-log-slower-than 0\nSLOWLOG RESET",
        )
        .await?;
        let big = s.key_version(b"b1:jbig").await?.ok_or("gone")?;
        let log = admin
            .execute("SLOWLOG GET 128", &ExecOptions::default())
            .await?;
        run(
            &admin,
            "CONFIG SET slowlog-log-slower-than 10000\nSLOWLOG RESET",
        )
        .await?;
        let log = format!("{log:?}");
        assert!(!log.contains("DUMP"), "{label}: DUMP when opening: {log}");
        assert_eq!(big.digest_missing, Some(DigestMissing::Deferred), "{label}");
        let reader = s.version_reader().await;
        let found = reader
            .read_full(b"b1:jbig", &big, reader.full_read_limit())
            .await?;
        assert_eq!(
            found,
            FullDigest::Missing(DigestMissing::TooLarge),
            "{label}"
        );
        clean(&s).await?;
    }
    Ok(())
}

/// A plan from a version read on another database than the session's
/// current one (a console SELECT meanwhile) is refused.
#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn plans_from_another_database_are_refused() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let s = connect(&server, false).await?;
        clean(&s).await?;
        run(&s, "SET b1:dbk v").await?;
        let version = s.key_version(b"b1:dbk").await?.ok_or("gone")?;
        assert_eq!(version.db, Some(TEST_DB), "{label}");
        run(&s, "SELECT 3").await?;
        let refused = s
            .plan_edit(
                b"b1:dbk",
                &KeyExpect::Present {
                    key_type: KeyType::String,
                    version: Some(version),
                },
                &[KeyEdit::Delete],
            )
            .await;
        run(&s, &format!("SELECT {TEST_DB}")).await?;
        assert!(
            matches!(&refused, Err(EditError::Invalid(m)) if m.contains("read on database 14")),
            "{label}: {refused:?}"
        );
        clean(&s).await?;
    }
    Ok(())
}

/// Consumer group scripts with an ACL user that may run EVAL but not one
/// of the commands inside: NOPERM with the hint, and nothing changed.
#[tokio::test]
#[ignore = "needs the dev database matrix"]
async fn group_scripts_report_acl_refusals() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let label = &server.label;
        let admin = connect(&server, false).await?;
        clean(&admin).await?;
        run(
            &admin,
            "XADD b1:ga 1-0 f v\nXGROUP CREATE b1:ga g 0\nXREADGROUP GROUP g alice COUNT 1 STREAMS b1:ga >",
        )
        .await?;
        for (user, denied) in [
            ("b1ga_nopending", "-xpending"),
            ("b1ga_noxgroup", "-xgroup"),
        ] {
            run(
                &admin,
                &format!("ACL SETUSER {user} on >pw ~* &* +@all {denied}"),
            )
            .await?;
            let mut p = params(&server, false);
            p.username = Some(user.into());
            p.password = Some(Secret::new("pw".to_string()));
            let limited = RedisSession::connect(&p).await?;
            for change in [
                StreamGroupChange::GroupDestroy {
                    group: "g".into(),
                    expected_pending: 1,
                },
                StreamGroupChange::GroupDelConsumer {
                    group: "g".into(),
                    consumer: "alice".into(),
                    expected_pending: 1,
                },
            ] {
                let program = limited.plan_stream_group(b"b1:ga", &change).await?;
                match limited.apply_edit(&program).await {
                    Err(error) => {
                        assert_eq!(
                            error.code.as_deref(),
                            Some("NOPERM"),
                            "{label} {user}: {error:?}"
                        );
                        assert!(
                            error
                                .hint
                                .as_deref()
                                .is_some_and(|h| h.contains("XPENDING")),
                            "{label} {user}: {error:?}"
                        );
                        assert_eq!(error.detail.as_deref(), Some("Nothing was changed"));
                    }
                    Ok(other) => panic!("{label} {user}: {other:?}"),
                }
            }
            // The group and its consumer are still there.
            let groups = admin
                .execute("XINFO CONSUMERS b1:ga g", &ExecOptions::default())
                .await?;
            assert!(format!("{groups:?}").contains("alice"), "{label} {user}");
            run(&admin, &format!("ACL DELUSER {user}")).await?;
        }
        clean(&admin).await?;
    }
    Ok(())
}
