//! Modules (TimeSeries, Bloom, Cuckoo, Count-Min, Top-K, t-digest, Search),
//! console help (`COMMAND DOCS`) and bulk TTL changes against real servers.
//!
//! Run with `cargo test -p varve-driver-redis --test modules -- --include-ignored`.
//!
//! - The server matrix (`VARVE_REDIS_SERVERS`, as in `tests/edits.rs`; by
//!   default the dev ports of Redis 6.2, Redis 8.10 and Valkey 9) runs
//!   module detection, help and bulk TTL changes.
//! - The module tests need a server with the modules: `VARVE_REDIS_MODULES`
//!   (`host:port`, default `127.0.0.1:64381`), for example Redis 8 (which
//!   bundles JSON, Search, TimeSeries and Bloom) or Redis Stack.
//!
//! Tests use logical database 12 and keys under `mod:`, which they remove
//! (Search indexes are dropped too).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use varve_core::Engine;
use varve_driver::{ConnectParams, DriverError, ErrorKind, ExecOptions, Session, TlsMode, Value};
use varve_driver_redis::{
    BulkEnd, BulkFilter, BulkProgress, BulkTtlAction, CmsIncrement, ConflictReason, DigestMissing,
    EditOutcome, FullDigest, KeyEdit, KeyExpect, KeyType, ModuleFeature, ModuleKind, ModuleQuery,
    RedisSession, TsLabel, TsRangeQuery, ValueData, ValuePage,
};
use varve_secrets::Secret;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const TEST_DB: u16 = 12;

/// The tests share database 12, so they run one at a time.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Server {
    label: String,
    major: u32,
    valkey: bool,
    host: String,
    port: u16,
}

fn parse_server(item: &str) -> Server {
    let (label, endpoint) = item
        .trim()
        .split_once('@')
        .unwrap_or(("modules", item.trim()));
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
}

fn servers() -> Vec<Server> {
    std::env::var("VARVE_REDIS_SERVERS")
        .unwrap_or_else(|_| {
            "redis-6.2@127.0.0.1:63062,redis-8.10@127.0.0.1:63081,valkey-9@127.0.0.1:63190".into()
        })
        .split(',')
        .map(parse_server)
        .collect()
}

fn module_server() -> Server {
    let mut server = parse_server(
        &std::env::var("VARVE_REDIS_MODULES").unwrap_or_else(|_| "127.0.0.1:64381".into()),
    );
    server.label = "modules".into();
    server.major = 8;
    server
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
    p.application_name = "Varve modules".into();
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

/// Removes the test keys (and, on database 0, the test indexes).
async fn clean(session: &RedisSession) -> Result<(), DriverError> {
    if session.current_db().await == 0 && session.modules().await?.has(ModuleFeature::Search) {
        for index in session.search_indexes().await? {
            // A name that is not valid UTF-8 is dropped by its test.
            if index.starts_with("mod:") && !index.contains('\u{FFFD}') {
                run(session, &format!("FT.DROPINDEX \"{index}\"")).await?;
            }
        }
    }
    let mut cursor = 0;
    loop {
        let page = session.scan_keys(cursor, "mod:*", None, 1000).await?;
        for key in page.keys {
            run(session, &format!("UNLINK \"{}\"", key.display)).await?;
        }
        cursor = page.cursor;
        if cursor == 0 {
            break;
        }
    }
    Ok(())
}

/// The key's expectation with its version, the full digest filled in when
/// it was deferred (as the app does in the background).
async fn versioned(
    session: &RedisSession,
    key: &str,
) -> Result<KeyExpect, Box<dyn std::error::Error>> {
    let mut version = session
        .key_version(key.as_bytes())
        .await?
        .ok_or("the key does not exist")?;
    if version.digest_missing == Some(DigestMissing::Deferred) {
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

fn present(type_name: &str) -> KeyExpect {
    KeyExpect::Present {
        key_type: KeyType::Other(type_name.into()),
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

fn conflict(outcome: &EditOutcome) -> Option<ConflictReason> {
    match outcome {
        EditOutcome::Conflict { reason, .. } => Some(*reason),
        _ => None,
    }
}

async fn with_modules<F>(test: F) -> TestResult
where
    F: AsyncFnOnce(&RedisSession) -> TestResult,
{
    let _guard = DB_LOCK.lock().await;
    let server = module_server();
    let session = connect(&server, false).await?;
    clean(&session).await?;
    let outcome = test(&session).await;
    clean(&session).await?;
    outcome
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn modules_are_detected_per_server() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let session = connect(&server, true).await?;
        let report = session.modules().await?;
        assert_eq!(report.features.len(), ModuleFeature::ALL.len());
        let bundled = !server.valkey && server.major >= 8;
        for feature in &report.features {
            assert_eq!(
                feature.available, bundled,
                "{}: {:?}",
                server.label, feature
            );
            assert_eq!(feature.reason.is_none(), feature.available);
        }
        // The scan filter's module key types: what the bundled modules store, nothing elsewhere.
        if bundled {
            for name in [
                "ReJSON-RL",
                "TSDB-TYPE",
                "MBbloom--",
                "MBbloomCF",
                "CMSk-TYPE",
                "TopK-TYPE",
                "TDIS-TYPE",
            ] {
                assert!(
                    report.key_types.iter().any(|t| t == name),
                    "{}: {:?}",
                    server.label,
                    report.key_types
                );
                // SCAN ... TYPE takes each of them.
                run(&session, &format!("SCAN 0 MATCH varve:none:* TYPE {name}")).await?;
            }
        } else {
            assert!(
                report.key_types.is_empty(),
                "{}: {:?}",
                server.label,
                report.key_types
            );
        }
        if !bundled {
            let reason = report.features[2].reason.clone().unwrap_or_default();
            assert!(reason.contains("TS.*"), "{}: {reason}", server.label);
            if server.valkey {
                assert!(reason.contains("Valkey"), "{reason}");
            }
            let error = session
                .search_indexes()
                .await
                .err()
                .ok_or("FT._LIST worked without Search")?;
            assert_eq!(error.kind, ErrorKind::Unsupported);
        }
        let modules = report.modules.ok_or("MODULE LIST was refused")?;
        assert_eq!(
            modules.iter().any(|m| m.name == "timeseries"),
            bundled,
            "{}",
            server.label
        );
    }
    let report = connect(&module_server(), true).await?.modules().await?;
    assert!(ModuleFeature::ALL.iter().all(|f| report.has(*f)));
    assert_eq!(
        report.key_types.len(),
        ModuleFeature::ALL.len() - 1,
        "{:?}",
        report.key_types
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn time_series_pages_and_edits() -> TestResult {
    with_modules(async |s| {
        run(
            s,
            "TS.CREATE mod:ts RETENTION 0 DUPLICATE_POLICY BLOCK LABELS sensor a room b",
        )
        .await?;
        for t in 1..=25 {
            run(s, &format!("TS.ADD mod:ts {} {}.5", t * 1000, t)).await?;
        }
        // The browser page: info and the oldest samples.
        let chunk = s
            .read_value(b"mod:ts", &ValuePage::first(10))
            .await?
            .ok_or("no series")?;
        assert_eq!(chunk.total, Some(25));
        let ValueData::TimeSeries { info, samples } = &chunk.data else {
            return Err(format!("not a series: {:?}", chunk.data).into());
        };
        assert_eq!(info.first_timestamp, Some(1000));
        assert_eq!(info.last_timestamp, Some(25_000));
        assert_eq!(info.meta.labels.len(), 2);
        assert_eq!(info.meta.duplicate_policy.as_deref(), Some("block"));
        assert_eq!(samples.len(), 10);
        assert_eq!(samples[0].value, Value::Float(1.5));
        assert_eq!(chunk.next_cursor.as_deref(), Some("10001"));
        let second = s
            .read_value(
                b"mod:ts",
                &ValuePage {
                    cursor: "10001".into(),
                    count: 10,
                },
            )
            .await?
            .ok_or("no series")?;
        let ValueData::TimeSeries { samples, .. } = &second.data else {
            return Err("not a series".into());
        };
        assert_eq!(samples[0].timestamp, 11_000);

        // Ranges: a window, newest first, paged.
        let mut query = TsRangeQuery {
            from: Some(5000),
            to: Some(12_000),
            reverse: true,
            count: 5,
            cursor: None,
        };
        let page = s.ts_range(b"mod:ts", &query).await?.ok_or("no series")?;
        let stamps: Vec<i64> = page.samples.iter().map(|x| x.timestamp).collect();
        assert_eq!(stamps, [12_000, 11_000, 10_000, 9000, 8000]);
        query.cursor = page.next.clone();
        let page = s.ts_range(b"mod:ts", &query).await?.ok_or("no series")?;
        let stamps: Vec<i64> = page.samples.iter().map(|x| x.timestamp).collect();
        assert_eq!(stamps, [7000, 6000, 5000]);
        assert_eq!(page.next, None);
        assert!(s.ts_range(b"mod:none", &query).await?.is_none());
        query.from = Some(20);
        query.to = Some(10);
        assert!(s.ts_range(b"mod:ts", &query).await.is_err());

        // Add a sample; a second add at the same time is a conflict.
        let add = KeyEdit::TsAdd {
            timestamp: Some("30000".into()),
            value: text("7.25"),
        };
        let outcome = apply(
            s,
            "mod:ts",
            &present("TSDB-TYPE"),
            std::slice::from_ref(&add),
        )
        .await?;
        assert!(
            matches!(outcome, EditOutcome::Applied { .. }),
            "{outcome:?}"
        );
        let outcome = apply(s, "mod:ts", &present("TSDB-TYPE"), &[add]).await?;
        assert_eq!(conflict(&outcome), Some(ConflictReason::ValueChanged));
        let now = apply(
            s,
            "mod:ts",
            &present("TSDB-TYPE"),
            &[KeyEdit::TsAdd {
                timestamp: None,
                value: text("nan"),
            }],
        )
        .await?;
        assert!(matches!(now, EditOutcome::Applied { .. }), "{now:?}");
        assert!(
            s.plan_edit(
                b"mod:ts",
                &present("TSDB-TYPE"),
                &[KeyEdit::TsAdd {
                    timestamp: None,
                    value: text("inf"),
                }],
            )
            .await
            .is_err()
        );

        // Delete a range: checked against the version (a digest of the
        // samples), a change meanwhile is a conflict.
        let expect = versioned(s, "mod:ts").await?;
        let KeyExpect::Present {
            version: Some(version),
            ..
        } = &expect
        else {
            return Err("no version".into());
        };
        assert_eq!(version.length, Some(27));
        assert!(version.digest.is_some(), "{version:?}");
        let delete = KeyEdit::TsDeleteRange {
            from: "1000".into(),
            to: "3000".into(),
        };
        let program = s
            .plan_edit(b"mod:ts", &expect, std::slice::from_ref(&delete))
            .await?;
        assert!(program.view().destructive);
        assert!(!program.view().unchecked);
        // The same length, another value: only the digest sees it.
        run(s, "TS.ADD mod:ts 1000 99 ON_DUPLICATE LAST").await?;
        let outcome = s.apply_edit(&program).await?;
        assert_eq!(conflict(&outcome), Some(ConflictReason::VersionChanged));
        let expect = versioned(s, "mod:ts").await?;
        let outcome = apply(s, "mod:ts", &expect, std::slice::from_ref(&delete)).await?;
        assert!(
            matches!(outcome, EditOutcome::Applied { .. }),
            "{outcome:?}"
        );
        let page = s
            .ts_range(
                b"mod:ts",
                &TsRangeQuery {
                    from: None,
                    to: Some(3000),
                    reverse: false,
                    count: 10,
                    cursor: None,
                },
            )
            .await?
            .ok_or("no series")?;
        assert!(page.samples.is_empty());
        // Without a version the delete is refused; without a digest it is
        // unchecked and asks.
        assert!(
            s.plan_edit(
                b"mod:ts",
                &present("TSDB-TYPE"),
                std::slice::from_ref(&delete)
            )
            .await
            .is_err()
        );
        let mut expect = versioned(s, "mod:ts").await?;
        if let KeyExpect::Present {
            version: Some(v), ..
        } = &mut expect
        {
            v.digest = None;
            v.digest_missing = Some(DigestMissing::TooLarge);
        }
        let program = s.plan_edit(b"mod:ts", &expect, &[delete]).await?;
        assert!(program.view().unchecked);

        // Alter: retention, labels and policy against what was read.
        let info = match s.read_value(b"mod:ts", &ValuePage::first(1)).await? {
            Some(chunk) => match chunk.data {
                ValueData::TimeSeries { info, .. } => info,
                other => return Err(format!("not a series: {other:?}").into()),
            },
            None => return Err("no series".into()),
        };
        let alter = KeyEdit::TsAlter {
            retention_ms: Some(3_600_000),
            labels: Some(vec![TsLabel {
                name: "sensor".into(),
                value: "c".into(),
            }]),
            duplicate_policy: Some("last".into()),
            original: info.meta.clone(),
        };
        let program = s
            .plan_edit(
                b"mod:ts",
                &present("TSDB-TYPE"),
                std::slice::from_ref(&alter),
            )
            .await?;
        // From keep-forever to an hour drops old samples.
        assert!(program.view().destructive);
        assert!(!program.view().warnings.is_empty());
        run(s, "TS.ALTER mod:ts LABELS sensor z").await?;
        let outcome = s.apply_edit(&program).await?;
        assert_eq!(conflict(&outcome), Some(ConflictReason::ValueChanged));
        run(s, "TS.ALTER mod:ts LABELS sensor a room b").await?;
        let outcome = apply(s, "mod:ts", &present("TSDB-TYPE"), &[alter]).await?;
        assert!(
            matches!(outcome, EditOutcome::Applied { .. }),
            "{outcome:?}"
        );
        let info = match s.read_value(b"mod:ts", &ValuePage::first(1)).await? {
            Some(chunk) => match chunk.data {
                ValueData::TimeSeries { info, .. } => info,
                other => return Err(format!("not a series: {other:?}").into()),
            },
            None => return Err("no series".into()),
        };
        assert_eq!(info.meta.retention_ms, 3_600_000);
        assert_eq!(info.meta.duplicate_policy.as_deref(), Some("last"));
        assert_eq!(info.meta.labels.len(), 1);
        // Edits of another kind or on another type are refused.
        assert!(
            s.plan_edit(
                b"mod:ts",
                &present("TSDB-TYPE"),
                &[KeyEdit::BloomAdd {
                    items: vec![text("a")]
                }]
            )
            .await
            .is_err()
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn probabilistic_keys_read_query_and_add() -> TestResult {
    with_modules(async |s| {
        run(s, "BF.RESERVE mod:bf 0.01 1000").await?;
        run(s, "CF.RESERVE mod:cf 1000").await?;
        run(s, "CMS.INITBYDIM mod:cms 100 5").await?;
        run(s, "TOPK.RESERVE mod:topk 3").await?;
        run(s, "TDIGEST.CREATE mod:td").await?;

        let outcome = apply(
            s,
            "mod:bf",
            &present("MBbloom--"),
            &[KeyEdit::BloomAdd {
                items: vec![text("a"), Value::Bytes(vec![0xff, 0])],
            }],
        )
        .await?;
        assert!(matches!(outcome, EditOutcome::Applied { .. }), "{outcome:?}");
        let result = s
            .module_query(
                b"mod:bf",
                &ModuleQuery::BloomExists {
                    items: vec![text("a"), Value::Bytes(vec![0xff, 0]), text("zzz")],
                },
            )
            .await?
            .ok_or("no filter")?;
        let found: Vec<Value> = result.rows.iter().map(|r| r.output.clone()).collect();
        assert_eq!(found, [Value::Bool(true), Value::Bool(true), Value::Bool(false)]);

        let outcome = apply(
            s,
            "mod:cf",
            &present("MBbloomCF"),
            &[KeyEdit::CuckooAdd {
                items: vec![text("x"), text("x")],
                nx: false,
            }],
        )
        .await?;
        assert!(matches!(outcome, EditOutcome::Applied { .. }), "{outcome:?}");
        let counted = s
            .module_query(
                b"mod:cf",
                &ModuleQuery::CuckooCount {
                    items: vec![text("x"), text("y")],
                },
            )
            .await?
            .ok_or("no filter")?;
        assert_eq!(counted.rows[0].output, Value::Int(2));
        assert_eq!(counted.rows[1].output, Value::Int(0));
        let delete = KeyEdit::CuckooDelete { item: text("x") };
        let program = s
            .plan_edit(b"mod:cf", &present("MBbloomCF"), std::slice::from_ref(&delete))
            .await?;
        assert!(program.view().destructive);
        assert!(matches!(s.apply_edit(&program).await?, EditOutcome::Applied { .. }));
        assert!(matches!(
            apply(s, "mod:cf", &present("MBbloomCF"), std::slice::from_ref(&delete)).await?,
            EditOutcome::Applied { .. }
        ));
        // Both copies are gone: the check sees it.
        let outcome = apply(s, "mod:cf", &present("MBbloomCF"), &[delete]).await?;
        assert_eq!(conflict(&outcome), Some(ConflictReason::ValueChanged));

        let outcome = apply(
            s,
            "mod:cms",
            &present("CMSk-TYPE"),
            &[KeyEdit::CmsIncrBy {
                items: vec![CmsIncrement {
                    item: text("page"),
                    increment: 5,
                }],
            }],
        )
        .await?;
        assert!(matches!(outcome, EditOutcome::Applied { .. }), "{outcome:?}");
        let cms = s
            .module_query(
                b"mod:cms",
                &ModuleQuery::CmsQuery {
                    items: vec![text("page")],
                },
            )
            .await?
            .ok_or("no sketch")?;
        assert_eq!(cms.rows[0].output, Value::Int(5));

        let outcome = apply(
            s,
            "mod:topk",
            &present("TopK-TYPE"),
            &[KeyEdit::TopkAdd {
                items: vec![text("a"), text("b"), text("a")],
            }],
        )
        .await?;
        assert!(matches!(outcome, EditOutcome::Applied { .. }), "{outcome:?}");
        let chunk = s
            .read_value(b"mod:topk", &ValuePage::first(10))
            .await?
            .ok_or("no top-k")?;
        let ValueData::Probabilistic {
            module_kind,
            fields,
            top,
        } = &chunk.data
        else {
            return Err("not probabilistic".into());
        };
        assert_eq!(*module_kind, ModuleKind::TopK);
        assert!(fields.iter().any(|f| f.name == "k"));
        assert_eq!(top[0].item, text("a"));
        assert_eq!(top[0].count, 2);

        let outcome = apply(
            s,
            "mod:td",
            &present("TDIS-TYPE"),
            &[KeyEdit::TdigestAdd {
                values: vec![text("1"), text("2"), text("3"), Value::Float(4.0)],
            }],
        )
        .await?;
        assert!(matches!(outcome, EditOutcome::Applied { .. }), "{outcome:?}");
        let chunk = s
            .read_value(b"mod:td", &ValuePage::first(10))
            .await?
            .ok_or("no digest")?;
        let ValueData::Probabilistic { fields, .. } = &chunk.data else {
            return Err("not probabilistic".into());
        };
        let min = fields.iter().find(|f| f.name == "Min").ok_or("no Min")?;
        assert_eq!(min.value, Value::Float(1.0));
        let quantile = s
            .module_query(
                b"mod:td",
                &ModuleQuery::TdigestQuantile {
                    quantiles: vec![text("0"), text("1")],
                },
            )
            .await?
            .ok_or("no digest")?;
        assert_eq!(quantile.rows[1].output, Value::Float(4.0));
        let mean = s
            .module_query(
                b"mod:td",
                &ModuleQuery::TdigestTrimmedMean {
                    low: text("0"),
                    high: text("1"),
                },
            )
            .await?
            .ok_or("no digest")?;
        assert_eq!(mean.rows[0].output, Value::Float(2.5));
        for (key, kind) in [
            ("mod:bf", ModuleKind::Bloom),
            ("mod:cf", ModuleKind::Cuckoo),
            ("mod:cms", ModuleKind::CountMin),
        ] {
            let chunk = s
                .read_value(key.as_bytes(), &ValuePage::first(10))
                .await?
                .ok_or("no key")?;
            assert!(
                matches!(&chunk.data, ValueData::Probabilistic { module_kind, fields, .. }
                    if *module_kind == kind && !fields.is_empty()),
                "{key}: {:?}",
                chunk.data
            );
        }
        // A query of another kind is refused; a missing key is None.
        let wrong = s
            .module_query(
                b"mod:bf",
                &ModuleQuery::CmsQuery {
                    items: vec![text("a")],
                },
            )
            .await
            .err()
            .ok_or("a CMS query ran on a Bloom filter")?;
        assert_eq!(wrong.code.as_deref(), Some("WRONGTYPE"));
        assert!(
            s.module_query(
                b"mod:none",
                &ModuleQuery::BloomExists {
                    items: vec![text("a")]
                }
            )
            .await?
            .is_none()
        );

        // A read-only session reads and queries, and refuses every edit.
        let ro = connect(&module_server(), true).await?;
        assert!(ro.read_value(b"mod:bf", &ValuePage::first(1)).await?.is_some());
        assert!(
            ro.module_query(
                b"mod:bf",
                &ModuleQuery::BloomExists {
                    items: vec![text("a")]
                }
            )
            .await?
            .is_some()
        );
        let refused = ro
            .plan_edit(
                b"mod:bf",
                &present("MBbloom--"),
                &[KeyEdit::BloomAdd {
                    items: vec![text("b")],
                }],
            )
            .await;
        assert!(
            matches!(&refused, Err(varve_driver_redis::EditError::Driver(e)) if e.kind == ErrorKind::ReadOnly),
            "{refused:?}"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn search_indexes_list_and_rebuild_their_definition() -> TestResult {
    // Search indexes exist on database 0 only (the module fixture is ours).
    let _guard = DB_LOCK.lock().await;
    let mut db0 = params(&module_server(), false);
    db0.redis_db = 0;
    let s = &RedisSession::connect(&db0).await?;
    clean(s).await?;
    let outcome: TestResult = async {
        // Every attribute type with the options FT.INFO reports, and the
        // index settings it exposes.
        run(
            s,
            "FT.CREATE mod:idx ON HASH PREFIX 2 mod:user: \"mod:u \\\"x\\\"\" FILTER \"@age>0\" \
             LANGUAGE german LANGUAGE_FIELD lang SCORE 0.5 SCORE_FIELD sc PAYLOAD_FIELD pl \
             MAXTEXTFIELDS NOOFFSETS NOFREQS STOPWORDS 2 foo bar SCHEMA \
             name TEXT NOSTEM WEIGHT 2 PHONETIC dm:en SORTABLE UNF \
             body TEXT WITHSUFFIXTRIE INDEXEMPTY INDEXMISSING \
             hidden TEXT SORTABLE NOINDEX \
             tags TAG SEPARATOR ; CASESENSITIVE WITHSUFFIXTRIE SORTABLE \
             plain TAG \
             age NUMERIC INDEXMISSING SORTABLE \
             loc GEO SORTABLE \
             flat GEOSHAPE FLAT \
             sphere GEOSHAPE SPHERICAL \
             hv VECTOR HNSW 12 TYPE FLOAT32 DIM 4 DISTANCE_METRIC COSINE M 8 EF_CONSTRUCTION 100 EF_RUNTIME 20 \
             fv VECTOR FLAT 8 TYPE FLOAT64 DIM 2 DISTANCE_METRIC L2 BLOCK_SIZE 512",
        )
        .await?;
        run(
            s,
            "FT.CREATE mod:jidx ON JSON PREFIX 1 mod:doc: NOFIELDS STOPWORDS 0 SCHEMA \
             $.title AS title TEXT NOSTEM $.n AS n NUMERIC $.t[*] AS t TAG CASESENSITIVE \
             $.v AS v VECTOR FLAT 6 TYPE FLOAT32 DIM 2 DISTANCE_METRIC IP",
        )
        .await?;
        run(s, "FT.CREATE mod:plain ON HASH SCHEMA t TEXT").await?;
        run(s, "FT.CREATE mod:all ON HASH INDEXALL ENABLE SCHEMA t TEXT").await?;
        run(
            s,
            "FT.CREATE mod:svs ON HASH SCHEMA v VECTOR SVS-VAMANA 10 TYPE FLOAT32 DIM 16 \
             DISTANCE_METRIC L2 GRAPH_MAX_DEGREE 40 CONSTRUCTION_WINDOW_SIZE 200",
        )
        .await?;
        run(
            s,
            "FT.CREATE mod:svsq ON HASH SCHEMA v VECTOR SVS-VAMANA 8 TYPE FLOAT32 DIM 16 \
             DISTANCE_METRIC L2 COMPRESSION LVQ8",
        )
        .await?;
        run(s, "HSET mod:user:1 name Ann age 30 tags a;b").await?;
        let names = s.search_indexes().await?;
        for name in ["mod:idx", "mod:jidx", "mod:plain", "mod:all", "mod:svs", "mod:svsq"] {
            assert!(names.contains(&name.to_string()), "{names:?}");
        }
        // A compression FT.INFO reports in a form FT.CREATE does not take
        // (GlobalSQ8 on some builds) gives no definition, with the reason.
        let svsq = s.search_index("mod:svsq").await?.ok_or("no index")?;
        if svsq.ddl.is_none() {
            assert!(svsq.notes[0].contains("compression"), "{:?}", svsq.notes);
        }
        let all = s.search_index("mod:all").await?.ok_or("no index")?;
        assert!(all.indexes_all);
        assert!(s.search_index("mod:none").await?.is_none());
        let mut names = vec!["mod:idx", "mod:jidx", "mod:plain", "mod:all", "mod:svs"];
        if svsq.ddl.is_some() {
            names.push("mod:svsq");
        }
        for name in names {
            let index = s.search_index(name).await?.ok_or("no index")?;
            let ddl = index.ddl.clone().ok_or(format!("no definition: {:?}", index.notes))?;
            assert!(!ddl.contains('\n'), "{ddl}");
            assert!(index.notes[0].contains("cannot be recovered"));
            // Round trip through the console: the copied line, as is.
            run(s, &format!("FT.DROPINDEX \"{name}\"")).await?;
            run(s, &ddl).await.map_err(|e| format!("{ddl}: {}", e.message))?;
            let again = s.search_index(name).await?.ok_or("index not recreated")?;
            // Every setting the rebuild reads (all but statistics).
            assert_eq!(settings(&again), settings(&index), "{ddl}");
        }
        let index = s.search_index("mod:idx").await?.ok_or("no index")?;
        assert_eq!(index.prefixes, ["mod:user:", "mod:u \"x\""]);
        assert_eq!(index.language_field.as_deref(), Some("lang"));
        assert_eq!(index.score_field.as_deref(), Some("sc"));
        let display = index.ddl_display.clone().unwrap_or_default();
        assert!(display.contains("\n  name TEXT NOSTEM WEIGHT 2 SORTABLE UNF"), "{display}");
        assert!(display.contains("\n  flat GEOSHAPE FLAT"), "{display}");
        assert!(display.contains("LANGUAGE german LANGUAGE_FIELD lang"), "{display}");
        // A read-only session reads them too, also on another database.
        let ro = connect(&module_server(), true).await?;
        assert!(ro.search_index("mod:idx").await?.is_some());
        Ok(())
    }
    .await;
    clean(s).await?;
    outcome
}

/// An index without its statistics: what a rebuild must give back.
fn settings(index: &varve_driver_redis::SearchIndex) -> varve_driver_redis::SearchIndex {
    let mut out = index.clone();
    out.stats.clear();
    out.raw.clear();
    out.num_docs = None;
    out.indexing_failures = None;
    out.percent_indexed = None;
    out
}

/// valkey-bundle (valkey-search, valkey-bloom, valkey-json): `VARVE_VALKEY_BUNDLE`
/// (`host:port`, default `127.0.0.1:64395`).
fn valkey_bundle() -> Server {
    let mut server = parse_server(
        &std::env::var("VARVE_VALKEY_BUNDLE").unwrap_or_else(|_| "127.0.0.1:64395".into()),
    );
    server.label = "valkey-bundle".into();
    server.valkey = true;
    server.major = 9;
    server
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn valkey_search_indexes_rebuild_or_show_the_raw_reply() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let mut db0 = params(&valkey_bundle(), false);
    db0.redis_db = 0;
    let s = &RedisSession::connect(&db0).await?;
    clean(s).await?;
    let outcome: TestResult = async {
        let report = s.modules().await?;
        assert!(report.has(ModuleFeature::Search) && report.has(ModuleFeature::Bloom));
        assert!(!report.has(ModuleFeature::TimeSeries));
        run(
            s,
            "FT.CREATE mod:vs ON HASH PREFIX 1 mod:p: SCHEMA t TAG SEPARATOR ; CASESENSITIVE \
             u TAG n NUMERIC hv VECTOR HNSW 10 TYPE FLOAT32 DIM 3 DISTANCE_METRIC COSINE M 8 \
             EF_CONSTRUCTION 64 fv VECTOR FLAT 6 TYPE FLOAT32 DIM 2 DISTANCE_METRIC L2",
        )
        .await?;
        let index = s.search_index("mod:vs").await?.ok_or("no index")?;
        assert_eq!(index.engine, varve_driver_redis::SearchEngine::ValkeySearch);
        let ddl = index
            .ddl
            .clone()
            .ok_or(format!("no definition: {:?}", index.notes))?;
        let tags: Vec<_> = index
            .attributes
            .iter()
            .filter(|a| a.field_type == "TAG")
            .collect();
        assert!(
            tags.iter()
                .any(|a| a.options == ["SEPARATOR", ";", "CASESENSITIVE"]),
            "{ddl}"
        );
        assert!(
            tags.iter()
                .any(|a| !a.options.iter().any(|o| o == "CASESENSITIVE")),
            "{ddl}"
        );
        assert!(ddl.contains("VECTOR HNSW"), "{ddl}");
        run(s, "FT.DROPINDEX mod:vs").await?;
        run(s, &ddl)
            .await
            .map_err(|e| format!("{ddl}: {}", e.message))?;
        let again = s
            .search_index("mod:vs")
            .await?
            .ok_or("index not recreated")?;
        assert_eq!(settings(&again), settings(&index), "{ddl}");
        // valkey-bloom keys read like RedisBloom's.
        run(s, "BF.ADD mod:bf a").await?;
        let found = s
            .module_query(
                b"mod:bf",
                &ModuleQuery::BloomExists {
                    items: vec![text("a"), text("b")],
                },
            )
            .await?
            .ok_or("no filter")?;
        assert_eq!(found.rows[0].output, Value::Bool(true));
        assert!(
            s.read_value(b"mod:bf", &ValuePage::first(1))
                .await?
                .is_some()
        );
        // The scan filter offers valkey-bloom's own type name, and SCAN ... TYPE finds the key by it.
        assert!(
            report.key_types.iter().any(|t| t == "bloomfltr"),
            "{:?}",
            report.key_types
        );
        assert!(!report.key_types.iter().any(|t| t == "MBbloom--"));
        let mut cursor = 0;
        let mut found_bf = false;
        loop {
            let page = s
                .scan_keys(
                    cursor,
                    "mod:bf",
                    Some(KeyType::Other("bloomfltr".into())),
                    1000,
                )
                .await?;
            found_bf |= page.keys.iter().any(|k| k.display == "mod:bf");
            cursor = page.cursor;
            if found_bf || cursor == 0 {
                break;
            }
        }
        assert!(found_bf, "SCAN TYPE bloomfltr did not find mod:bf");
        run(s, "FT.DROPINDEX mod:vs").await?;
        Ok(())
    }
    .await;
    clean(s).await?;
    outcome
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn command_help_uses_command_docs_or_falls_back() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let s = connect(&server, true).await?;
        let help = s.command_help("set").await?;
        assert!(help.known, "{}", server.label);
        assert!(help.flags.iter().any(|f| f == "write"));
        assert_eq!(help.arity, Some(-3));
        let sub = s.command_help("config get").await?;
        assert!(sub.known, "{}", server.label);
        if server.major >= 7 || server.valkey {
            assert!(help.from_docs, "{}: {help:?}", server.label);
            let syntax = help.syntax.clone().unwrap_or_default();
            assert!(syntax.starts_with("SET key value"), "{syntax}");
            assert!(syntax.contains("NX | XX"), "{syntax}");
            assert_eq!(help.group.as_deref(), Some("string"));
            assert!(sub.from_docs, "{sub:?}");
            assert!(sub.syntax.unwrap_or_default().starts_with("CONFIG GET"));
            let container = s.command_help("CONFIG").await?;
            assert!(container.subcommands.iter().any(|c| c == "CONFIG SET"));
        } else {
            assert!(!help.from_docs);
            assert!(
                help.note.unwrap_or_default().contains("COMMAND DOCS"),
                "{}",
                server.label
            );
        }
        // A second word that is an argument gives the command's help.
        let with_key = s.command_help("GET mykey").await?;
        assert!(with_key.known, "{}: {with_key:?}", server.label);
        assert_eq!(with_key.name, "GET", "{}", server.label);
        let unknown = s.command_help("NOSUCHCOMMAND").await?;
        assert!(!unknown.known);
        assert!(unknown.note.is_some());
        // While a console MULTI block is open the lookup does not join it.
        let rw = connect(&server, false).await?;
        run(&rw, "MULTI").await?;
        let during = rw.command_help("GET").await?;
        assert!(during.known);
        run(&rw, "DISCARD").await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn bulk_ttl_counts_applies_and_stops() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    for server in servers() {
        let s = connect(&server, false).await?;
        clean(&s).await?;
        let outcome: TestResult = async {
            run(
                &s,
                "EVAL \"for i=1,3000 do redis.call('SET', 'mod:bulk:' .. i, 'v') end for i=1,10 do redis.call('HSET', 'mod:bulkh:' .. i, 'f', 'v') end return 1\" 0",
            )
            .await?;
            let runner = s.bulk_ttl().await?;
            assert_eq!(runner.db(), TEST_DB);
            let all = BulkFilter {
                pattern: "mod:bulk*".into(),
                key_type: None,
            };
            let count = runner.count(&all, 1_000_000, Duration::from_secs(10)).await?;
            assert!(count.complete);
            assert_eq!(count.keys, 3010, "{}", server.label);
            assert!(!count.sample.is_empty());
            let hashes = BulkFilter {
                pattern: "mod:bulk*".into(),
                key_type: Some(KeyType::Hash),
            };
            assert_eq!(
                runner.count(&hashes, 1_000_000, Duration::from_secs(10)).await?.keys,
                10
            );
            let limited = runner.count(&all, 10, Duration::from_secs(10)).await?;
            assert!(!limited.complete && limited.keys >= 10);

            // Expire the hashes only.
            let progress = BulkProgress::default();
            let end = runner
                .apply(
                    &hashes,
                    BulkTtlAction::Expire { ttl_ms: 600_000 },
                    &AtomicBool::new(false),
                    &progress,
                )
                .await?;
            assert_eq!(end, BulkEnd::Complete);
            let done = progress.snapshot();
            assert_eq!(done.changed, 10);
            assert_eq!(done.matched, 10);
            let info = s.key_info(b"mod:bulkh:1").await?.ok_or("no key")?;
            assert!(info.ttl_ms.is_some_and(|t| t > 590_000));
            assert!(s.key_info(b"mod:bulk:1").await?.ok_or("no key")?.ttl_ms.is_none());

            // Persist: only keys with a TTL change.
            let progress = BulkProgress::default();
            runner
                .apply(&all, BulkTtlAction::Persist, &AtomicBool::new(false), &progress)
                .await?;
            let done = progress.snapshot();
            assert_eq!(done.changed, 10);
            assert_eq!(done.skipped, done.matched - 10);
            assert!(s.key_info(b"mod:bulkh:1").await?.ok_or("no key")?.ttl_ms.is_none());

            // A set stop flag ends the run before the next step.
            let stop = AtomicBool::new(true);
            let progress = BulkProgress::default();
            let end = runner
                .apply(&all, BulkTtlAction::Expire { ttl_ms: 1000 }, &stop, &progress)
                .await?;
            assert_eq!(end, BulkEnd::Stopped);
            assert_eq!(progress.snapshot().changed, 0);
            // Stopped while running: some steps ran, not all.
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let progress = std::sync::Arc::new(BulkProgress::default());
            let watcher = {
                let stop = stop.clone();
                let progress = progress.clone();
                tokio::spawn(async move {
                    while progress.steps.load(Ordering::SeqCst) < 1 {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    stop.store(true, Ordering::SeqCst);
                })
            };
            let end = runner
                .apply(
                    &all,
                    BulkTtlAction::Expire { ttl_ms: 900_000 },
                    &stop,
                    &progress,
                )
                .await?;
            watcher.await?;
            let done = progress.snapshot();
            assert_eq!(end, BulkEnd::Stopped, "{}: {done:?}", server.label);
            assert!(done.changed > 0 && done.changed < 3010, "{done:?}");
            assert!(
                runner
                    .apply(
                        &all,
                        BulkTtlAction::Expire { ttl_ms: 0 },
                        &AtomicBool::new(false),
                        &BulkProgress::default()
                    )
                    .await
                    .is_err()
            );
            // Read-only sessions refuse.
            let ro = connect(&server, true).await?;
            let refused = ro.bulk_ttl().await.err().ok_or("read-only bulk")?;
            assert_eq!(refused.kind, ErrorKind::ReadOnly);
            Ok(())
        }
        .await;
        clean(&s).await?;
        outcome.map_err(|e| format!("{}: {e}", server.label))?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn opening_a_series_never_breaks_a_console_watch() -> TestResult {
    with_modules(async |s| {
        run(s, "TS.CREATE mod:tsw").await?;
        run(s, "TS.ADD mod:tsw 1 1").await?;
        run(s, "WATCH mod:tsw").await?;
        // TS.INFO would end the WATCH: the viewer refuses and says why.
        let refused = s
            .read_value(b"mod:tsw", &ValuePage::first(10))
            .await
            .err()
            .ok_or("the series was read with TS.INFO under a WATCH")?;
        assert_eq!(
            refused.code.as_deref(),
            Some(varve_driver_redis::WATCH_OPEN)
        );
        // Ranges do not touch the WATCH.
        let query = TsRangeQuery {
            from: None,
            to: None,
            reverse: false,
            count: 10,
            cursor: None,
        };
        assert!(s.ts_range(b"mod:tsw", &query).await?.is_some());
        // The transaction still commits.
        run(s, "MULTI").await?;
        run(s, "SET mod:after 1").await?;
        run(s, "EXEC").await?;
        let info = s.key_info(b"mod:after").await?;
        assert!(info.is_some(), "the WATCH was broken, EXEC did not run");
        // Without a WATCH the series opens again.
        assert!(
            s.read_value(b"mod:tsw", &ValuePage::first(10))
                .await?
                .is_some()
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn help_tells_a_permission_refusal_apart() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let admin = connect(&module_server(), false).await?;
    run(
        &admin,
        "ACL SETUSER mod_nodocs on >mod_pw ~* &* +@all -command|docs",
    )
    .await?;
    let outcome: TestResult = async {
        let mut p = params(&module_server(), true);
        p.username = Some("mod_nodocs".into());
        p.password = Some(Secret::new("mod_pw".to_string()));
        let limited = RedisSession::connect(&p).await?;
        let help = limited.command_help("GET").await?;
        let note = help.note.unwrap_or_default();
        assert!(note.contains("may not run COMMAND DOCS"), "{note}");
        assert!(!note.contains("Redis 7.0 or newer"), "{note}");
        assert!(help.known, "COMMAND INFO is still permitted");
        Ok(())
    }
    .await;
    run(&admin, "ACL DELUSER mod_nodocs").await?;
    outcome
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn search_names_that_are_not_utf8_do_not_break_the_panel() -> TestResult {
    let _guard = DB_LOCK.lock().await;
    let mut db0 = params(&module_server(), false);
    db0.redis_db = 0;
    let s = &RedisSession::connect(&db0).await?;
    clean(s).await?;
    let outcome: TestResult = async {
        run(s, "FT.CREATE \"mod:bad\\xff\" ON HASH SCHEMA t TEXT").await?;
        run(
            s,
            "FT.CREATE mod:pfx ON HASH PREFIX 1 \"mod:p\\xfe:\" SCHEMA t TEXT",
        )
        .await?;
        let names = s.search_indexes().await?;
        assert!(names.iter().any(|n| n.starts_with("mod:bad")), "{names:?}");
        let pfx = s.search_index("mod:pfx").await?.ok_or("no index")?;
        assert_eq!(pfx.ddl, None, "{:?}", pfx.ddl);
        assert!(pfx.notes[0].contains("UTF-8"), "{:?}", pfx.notes);
        // The session is still usable.
        s.ping().await?;
        assert!(s.search_index("mod:pfx").await?.is_some());
        Ok(())
    }
    .await;
    let _ = run(s, "FT.DROPINDEX \"mod:bad\\xff\"").await;
    clean(s).await?;
    outcome
}

#[tokio::test]
#[ignore = "needs the M2 database matrix"]
async fn a_watch_in_another_session_blocks_reading_the_series() -> TestResult {
    with_modules(async |s| {
        run(s, "TS.CREATE mod:tsx").await?;
        run(s, "TS.CREATE mod:tsy").await?;
        // The console tab: its own session with a WATCH on one series.
        let console = connect(&module_server(), false).await?;
        run(&console, "WATCH mod:tsx").await?;
        let refused = s
            .read_value(b"mod:tsx", &ValuePage::first(1))
            .await
            .err()
            .ok_or("the series was read with TS.INFO under another tab's WATCH")?;
        assert_eq!(
            refused.code.as_deref(),
            Some(varve_driver_redis::WATCH_OPEN)
        );
        // Another key is not held.
        assert!(
            s.read_value(b"mod:tsy", &ValuePage::first(1))
                .await?
                .is_some()
        );
        // The console's transaction still commits.
        run(&console, "MULTI").await?;
        run(&console, "SET mod:x2 1").await?;
        run(&console, "EXEC").await?;
        assert!(s.key_info(b"mod:x2").await?.is_some());
        // EXEC ended the WATCH: the series reads again.
        assert!(
            s.read_value(b"mod:tsx", &ValuePage::first(1))
                .await?
                .is_some()
        );
        // A closed session releases its WATCH.
        run(&console, "WATCH mod:tsx").await?;
        drop(console);
        assert!(
            s.read_value(b"mod:tsx", &ValuePage::first(1))
                .await?
                .is_some()
        );
        Ok(())
    })
    .await
}
