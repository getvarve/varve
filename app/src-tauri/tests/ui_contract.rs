//! The UI contract: every JSON shape the UI sends or receives, checked on
//! both sides with the real types.
//!
//! ```sh
//! cargo test -p varve-app --test ui_contract
//! VARVE_UPDATE_CONTRACT=1 cargo test -p varve-app --test ui_contract   # after a contract change
//! ```
//!
//! Samples live in `packages/ui/src/api/__contract__/samples/NN-*.json`
//! (`{id, rust, ts, dir, value}`): `dir: "arg"` is what a UI call site sends,
//! `dir: "result"` what a command returns. Each sample is deserialized into
//! the Rust type the command uses; an argument Rust refuses or whose keys
//! Rust silently ignores fails here. The JSON Rust writes back (for results)
//! or the argument as sent goes into `contract.generated.ts` as
//! `export const sN = <json> satisfies <UI type>;`, which `vue-tsc` checks
//! with excess-property checks, so a field the UI misnames, lacks or types
//! wrongly fails the UI typecheck. Types that are only serialized (plans,
//! apply results, events, errors) are built here from the app types. The
//! index describe samples of `crates/catalog/tests/ui_index_round_trip.rs`
//! are typed as the designer's `TableDetails` too.
//!
//! This test fails when `contract.generated.ts` differs from what the
//! samples give; rerun with `VARVE_UPDATE_CONTRACT=1` to write it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as J;

use varve_app::commands::CmdError;
use varve_app::m2::ddl::DdlApplied;
use varve_app::m2::editors::{EditApplied, EngineChange};
use varve_app::m2::impact::DropImpact;
use varve_app::m2::live_redis::{ConfigRequest, FunctionRequest, RedisPlanView};
use varve_app::m2::notify::{NotifyBatch, NotifyMessage, NotifyState, NotifyStatus};
use varve_app::m2::plans::Planned;
use varve_app::m2::pubsub::{PubSubEvent, PubSubMessage, PubSubNames, PubSubSubscriptionInfo};
use varve_app::m2::redis_edit::RedisEditRequest;
use varve_app::sessions::{Encryption, SessionError, SessionInfo, SessionPurpose};
use varve_app::updates::{UpdateCheck, UpdateInfo, UpdateProgress};
use varve_catalog::data_edit::{
    ApplyOutcome, KeyInfo as GridKeyInfo, PlanOptions, RowChange, RowEditPlan, TableTarget,
};
use varve_catalog::explain::{ExplainCapabilities, ExplainOptions, Plan};
use varve_catalog::mysql::MyEngine;
use varve_catalog::postgres::programs::PgAvailableExtension;
use varve_catalog::{
    Capabilities, CatalogError, Container, EditPlan, KindCount, ObjectDetails, ObjectRef,
    ObjectSummary,
};
use varve_driver::{DriverError, ErrorKind, ExecOutput, StatementResult, Value};
use varve_driver_redis::{
    CommandRisk, ConflictReason, EditOutcome, KeyVersion, NameKind, PendingPage, PendingQuery,
    PendingSummary, RedisEditPlan, StreamConsumer, StreamGroup, StreamGroupChange, StreamInfo,
    ZsetRange, ZsetRangePage,
};
use varve_live_stats::redis::{
    ClientInfo, ConfigEntry, Databases, FunctionLibrary, LatencyReport, LatencySample,
    MemoryAnalysis, MemoryScanOptions, PubSubSnapshot, SlowLog,
};
use varve_live_stats::sql::{
    GaleraStatus, KillOutcome, KillTarget, LiveError, ProcessList, ServerStatus, ServerVariable,
    VariableChange,
};
use varve_records::{Environment, SecretKindName};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// UI types that mirror only the fields the UI reads (the table designer's
/// `TableDetails`, the object editors' `ObjectDetails`): their samples may
/// hold more keys, everything typed must still match. Every other result is
/// checked with excess-property checks.
const SUBSET_TYPES: &[&str] = &["grid.TableDetails", "ddl.ObjectDetails"];

/// The UI modules a `ts` type may name (`grid.TableDetails`).
const MODULES: &[&str] = &[
    "activity", "catalog", "ddl", "errors", "explain", "grid", "live", "notify", "plans", "redis",
    "sessions", "updates",
];

fn contract_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/ui/src/api/__contract__")
}

fn rt<T: DeserializeOwned + Serialize>(v: J) -> Result<J, String> {
    let x: T = serde_json::from_value(v).map_err(|e| e.to_string())?;
    serde_json::to_value(&x).map_err(|e| e.to_string())
}

/// Deserializes a sample into the Rust type named by `name` and writes it back.
fn roundtrip(name: &str, v: J) -> Result<J, String> {
    match name {
        "Value" => rt::<Value>(v),
        "ExecOutput" => rt::<ExecOutput>(v),
        "DriverError" => rt::<DriverError>(v),
        "ObjectRef" => rt::<ObjectRef>(v),
        "Container" => rt::<Container>(v),
        "Vec<ObjectSummary>" => rt::<Vec<ObjectSummary>>(v),
        "Vec<KindCount>" => rt::<Vec<KindCount>>(v),
        "Capabilities" => rt::<Capabilities>(v),
        "UpdateInfo" => rt::<UpdateInfo>(v),
        "UpdateCheck" => rt::<UpdateCheck>(v),
        "UpdateProgress" => rt::<UpdateProgress>(v),
        "ObjectDetails" => rt::<ObjectDetails>(v),
        "EditPlan" => rt::<EditPlan>(v),
        "EngineChange" => rt::<EngineChange>(v),
        "ExplainCapabilities" => rt::<ExplainCapabilities>(v),
        "ExplainOptions" => rt::<ExplainOptions>(v),
        "Plan" => rt::<Plan>(v),
        "TableTarget" => rt::<TableTarget>(v),
        "Option<GridKeyInfo>" => rt::<Option<GridKeyInfo>>(v),
        "Vec<RowChange>" => rt::<Vec<RowChange>>(v),
        "PlanOptions" => rt::<PlanOptions>(v),
        "RowEditPlan" => rt::<RowEditPlan>(v),
        "ApplyOutcome" => rt::<ApplyOutcome>(v),
        "ProcessList" => rt::<ProcessList>(v),
        "KillTarget" => rt::<KillTarget>(v),
        "KillOutcome" => rt::<KillOutcome>(v),
        "Vec<ServerVariable>" => rt::<Vec<ServerVariable>>(v),
        "VariableChange" => rt::<VariableChange>(v),
        "ServerStatus" => rt::<ServerStatus>(v),
        "GaleraStatus" => rt::<GaleraStatus>(v),
        "Databases" => rt::<Databases>(v),
        "SlowLog" => rt::<SlowLog>(v),
        "LatencyReport" => rt::<LatencyReport>(v),
        "Vec<LatencySample>" => rt::<Vec<LatencySample>>(v),
        "Vec<ClientInfo>" => rt::<Vec<ClientInfo>>(v),
        "Vec<ConfigEntry>" => rt::<Vec<ConfigEntry>>(v),
        "ConfigRequest" => rt::<ConfigRequest>(v),
        "FunctionRequest" => rt::<FunctionRequest>(v),
        "MemoryScanOptions" => rt::<MemoryScanOptions>(v),
        "MemoryAnalysis" => rt::<MemoryAnalysis>(v),
        "Vec<FunctionLibrary>" => rt::<Vec<FunctionLibrary>>(v),
        "PubSubSnapshot" => rt::<PubSubSnapshot>(v),
        "Option<KeyVersion>" => rt::<Option<KeyVersion>>(v),
        "RedisEditRequest" => rt::<RedisEditRequest>(v),
        "ZsetRange" => rt::<ZsetRange>(v),
        "Option<ZsetRangePage>" => rt::<Option<ZsetRangePage>>(v),
        "Option<StreamInfo>" => rt::<Option<StreamInfo>>(v),
        "Vec<StreamGroup>" => rt::<Vec<StreamGroup>>(v),
        "Vec<StreamConsumer>" => rt::<Vec<StreamConsumer>>(v),
        "PendingSummary" => rt::<PendingSummary>(v),
        "PendingQuery" => rt::<PendingQuery>(v),
        "PendingPage" => rt::<PendingPage>(v),
        "StreamGroupChange" => rt::<StreamGroupChange>(v),
        "PubSubNames" => rt::<PubSubNames>(v),
        "DropImpact" => rt::<DropImpact>(v),
        "ExportRecord" => rt::<varve_app::commands::ExportRecord>(v),
        "Vec<PgAvailableExtension>" => rt::<Vec<PgAvailableExtension>>(v),
        "Vec<MyEngine>" => rt::<Vec<MyEngine>>(v),
        "Option<u64>" => rt::<Option<u64>>(v),
        "Option<ValueChunk>" => rt::<Option<varve_driver_redis::ValueChunk>>(v),
        "ModuleReport" => rt::<varve_driver_redis::ModuleReport>(v),
        "Option<SearchIndex>" => rt::<Option<varve_driver_redis::SearchIndex>>(v),
        "TsRangeQuery" => rt::<varve_driver_redis::TsRangeQuery>(v),
        "Option<TsRangePage>" => rt::<Option<varve_driver_redis::TsRangePage>>(v),
        "ModuleQuery" => rt::<varve_driver_redis::ModuleQuery>(v),
        "Option<QueryResult>" => rt::<Option<varve_driver_redis::QueryResult>>(v),
        "CommandHelp" => rt::<varve_driver_redis::CommandHelp>(v),
        "BulkFilter" => rt::<varve_driver_redis::BulkFilter>(v),
        "BulkCount" => rt::<varve_driver_redis::BulkCount>(v),
        "BulkTtlRequest" => rt::<varve_app::m2::redis_bulk::BulkTtlRequest>(v),
        "BulkTtlJob" => rt::<varve_app::m2::redis_bulk::BulkTtlJob>(v),
        "BulkTtlStatus" => rt::<varve_app::m2::redis_bulk::BulkTtlStatus>(v),
        "Vec<BulkTtlStatus>" => rt::<Vec<varve_app::m2::redis_bulk::BulkTtlStatus>>(v),
        _ => Err(format!("unknown Rust type key {name}")),
    }
}

/// Keys present in `input` but missing from `output`: fields Rust ignored.
fn dropped(input: &J, output: &J, path: &str, out: &mut Vec<String>) {
    match (input, output) {
        (J::Object(a), J::Object(b)) => {
            for (k, v) in a {
                match b.get(k) {
                    None => out.push(format!("{path}.{k}")),
                    Some(w) => dropped(v, w, &format!("{path}.{k}"), out),
                }
            }
        }
        (J::Array(a), J::Array(b)) => {
            for (i, (v, w)) in a.iter().zip(b.iter()).enumerate() {
                dropped(v, w, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

fn to_json<T: Serialize>(value: &T) -> TestResult<J> {
    Ok(serde_json::to_value(value)?)
}

fn driver_error() -> DriverError {
    let mut error = DriverError::new(ErrorKind::Query, "duplicate key value").with_code("23505");
    error.detail = Some("Key (id)=(1) already exists.".into());
    error
}

fn output() -> ExecOutput {
    ExecOutput {
        results: vec![StatementResult::Command {
            tag: "ALTER TABLE".into(),
            rows_affected: None,
        }],
        notices: vec![],
        elapsed_ms: 3,
    }
}

/// Results of types that are only serialized, built from the app types.
fn constructed() -> TestResult<Vec<(String, &'static str, J)>> {
    let redis_plan = RedisEditPlan {
        key: "k".into(),
        db: 0,
        commands: vec!["SET k v KEEPTTL".into()],
        checks: vec!["k is a string".into()],
        risk: CommandRisk::Write,
        destructive: false,
        unchecked: true,
        warnings: vec![],
    };
    let mut v: Vec<(String, &'static str, J)> = vec![
        (
            "Planned<RedisEditPlan>".into(),
            "redis.Planned<redis.RedisEditPlan>",
            to_json(&Planned {
                plan_id: "p3".into(),
                plan: redis_plan,
            })?,
        ),
        (
            "Planned<RedisPlanView>".into(),
            "live.Planned<live.RedisPlanView>",
            to_json(&Planned {
                plan_id: "p4".into(),
                plan: RedisPlanView {
                    display: vec!["CONFIG SET requirepass ********".into()],
                    destructive: true,
                    sensitive: true,
                    warnings: vec!["clients must log in again".into()],
                },
            })?,
        ),
        (
            "Planned<EditPlan>".into(),
            "ddl.Planned<ddl.EditPlan>",
            to_json(&Planned {
                plan_id: "p5".into(),
                plan: EditPlan {
                    statements: vec!["DROP INDEX \"s\".\"i\"".into()],
                    transactional: false,
                    destructive: true,
                    warnings: vec![],
                },
            })?,
        ),
        (
            "EditApplied".into(),
            "ddl.EditApplied",
            to_json(&EditApplied {
                statements: 2,
                transactional: true,
                outputs: vec![output()],
                duration_ms: 4,
            })?,
        ),
        (
            "DdlApplied".into(),
            "ddl.DdlApplied",
            to_json(&DdlApplied {
                output: output(),
                session: None,
                collation_database: Some("shop".into()),
                warnings: vec!["The collation of shop is still latin1_bin.".into()],
                duration_ms: 9,
            })?,
        ),
        (
            "SessionInfo".into(),
            "sessions.SessionInfo",
            to_json(&SessionInfo {
                id: "s1".into(),
                connection_id: "c1".into(),
                engine: varve_core::Engine::Redis,
                server: varve_driver::ServerInfo {
                    engine: varve_core::Engine::Redis,
                    version: "8.10.2".into(),
                    major: 8,
                    minor: 10,
                    patch: Some(2),
                },
                environment: Environment::Staging,
                database: None,
                read_only: false,
                confirm_destructive: false,
                purpose: SessionPurpose::User,
                opened_at: 1_760_000_000_000,
                encryption: Encryption {
                    tls: Some(false),
                    mode: varve_driver::TlsMode::Prefer,
                    password_unencrypted: true,
                },
                identity: "{}".into(),
                server_identity: None,
            })?,
        ),
        (
            "NotifyState".into(),
            "notify.NotifyState",
            to_json(&NotifyState {
                session_id: "s1".into(),
                listener_id: Some(2),
                version: 3,
                status: NotifyStatus::Reconnecting,
                channels: vec!["jobs".into()],
                backend_pid: Some(77),
                attempt: 2,
                retry_in_ms: Some(2000),
                error: Some(driver_error()),
                dropped: 1,
                gap: true,
            })?,
        ),
        (
            "NotifyBatch".into(),
            "notify.NotifyBatch",
            to_json(&NotifyBatch {
                session_id: "s1".into(),
                listener_id: 2,
                seq: 5,
                messages: vec![NotifyMessage {
                    channel: "jobs".into(),
                    payload: "{}".into(),
                    pid: 77,
                    own: false,
                    lossy: false,
                    received_at: 1_760_000_000_000,
                }],
                dropped: 0,
            })?,
        ),
        (
            "PubSubSubscriptionInfo[]".into(),
            "redis.PubSubSubscriptionInfo[]",
            to_json(&vec![PubSubSubscriptionInfo {
                subscription_id: "sub1".into(),
                channels: vec!["orders".into()],
                patterns: vec!["audit.*".into()],
                shard_channels: vec![],
                state: "reconnecting",
                delivered: 4,
                dropped: 0,
            }])?,
        ),
    ];
    let outcomes = vec![
        EditOutcome::Applied {
            replies: vec![Value::Text("OK".into()), Value::Int(1)],
        },
        EditOutcome::Conflict {
            edit: Some(0),
            reason: ConflictReason::ValueChanged,
            message: "changed".into(),
        },
        EditOutcome::Failed {
            command: 1,
            error: DriverError::new(ErrorKind::Query, "OOM").with_code("OOM"),
        },
    ];
    for o in outcomes {
        let j = to_json(&o)?;
        let status = j["status"].as_str().unwrap_or("?").to_owned();
        v.push((format!("EditOutcome.{status}"), "redis.RedisEditOutcome", j));
    }
    let events = vec![
        PubSubEvent::Batch {
            subscription_id: "sub1".into(),
            seq: 1,
            messages: vec![PubSubMessage {
                channel: "orders".into(),
                pattern: Some("ord*".into()),
                payload: Value::Text("hi".into()),
                size: "2".into(),
                truncated: false,
                received_at: 1_760_000_000_000,
            }],
            dropped: 0,
        },
        PubSubEvent::Subscribed {
            subscription_id: "sub1".into(),
            name_kind: NameKind::ShardChannel,
            name: "orders".into(),
            count: 1,
        },
        PubSubEvent::Error {
            subscription_id: "sub1".into(),
            message: "NOPERM".into(),
            code: Some("NOPERM".into()),
            name_kind: Some(NameKind::Channel),
            name: Some("bad1".into()),
        },
        PubSubEvent::Reconnecting {
            subscription_id: "sub1".into(),
            attempt: 1,
            reason: "lost".into(),
        },
        PubSubEvent::Gap {
            subscription_id: "sub1".into(),
            reason: "lost".into(),
            lost_at: 1,
            resumed_at: 2,
        },
        PubSubEvent::Closed {
            subscription_id: "sub1".into(),
            reason: "unsubscribed".into(),
        },
    ];
    for e in events {
        let j = to_json(&e)?;
        let kind = j["kind"].as_str().unwrap_or("?").to_owned();
        v.push((format!("PubSubEvent.{kind}"), "redis.PubSubEvent", j));
    }
    let errors = vec![
        CmdError::Invalid {
            message: "bad".into(),
        },
        CmdError::NotFound {
            message: "This plan is no longer available.".into(),
        },
        CmdError::Session {
            error: SessionError::Driver {
                error: driver_error(),
            },
        },
        CmdError::Session {
            error: SessionError::NotFound {
                session_id: "s1".into(),
            },
        },
        CmdError::Session {
            error: SessionError::ConfirmationRequired {
                environment: Environment::Production,
                statements: vec!["AlterDatabase".into()],
            },
        },
        CmdError::Session {
            error: SessionError::OpenTransaction {
                message: "open".into(),
            },
        },
        CmdError::SecretRequired {
            secret: SecretKindName::DbPassword,
        },
        // An encrypted TLS client key without a passphrase (the UI asks for
        // it), and one whose passphrase is wrong (the UI asks again).
        CmdError::SecretRequired {
            secret: SecretKindName::TlsKeyPassphrase,
        },
        CmdError::Session {
            error: SessionError::Driver {
                error: DriverError::new(
                    ErrorKind::Tls,
                    "the passphrase does not decrypt the client key file",
                )
                .with_code(varve_driver::tls_key::PASSPHRASE_WRONG),
            },
        },
        CmdError::ReconnectRequired {
            message: "reconnect".into(),
        },
        CmdError::ReadOnly {
            message: "read only".into(),
        },
        CmdError::Catalog {
            error: CatalogError::Unsupported {
                message: "no".into(),
            },
        },
        CmdError::Live {
            error: LiveError::Unsupported {
                message: "This node is not part of a Galera cluster.".into(),
            },
        },
        CmdError::Internal {
            message: "x".into(),
        },
    ];
    for (i, e) in errors.iter().enumerate() {
        let j = to_json(e)?;
        let kind = j["kind"].as_str().unwrap_or("?").to_owned();
        v.push((format!("CmdError.{kind}.{i}"), "errors.CmdError", j));
    }
    Ok(v)
}

/// `{id, rust, ts, dir, value}` samples of one file.
fn samples_of(path: &std::path::Path) -> TestResult<Vec<J>> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)
        .map_err(|e| format!("{}: {e}", path.display()))?)
}

fn field<'a>(sample: &'a J, name: &str) -> TestResult<&'a str> {
    sample[name]
        .as_str()
        .ok_or_else(|| format!("sample without {name}: {sample}").into())
}

/// The `import type * as m from '../m'` lines for the modules `ts` names.
fn imports(types: &[&str]) -> String {
    let used: BTreeSet<&str> = MODULES
        .iter()
        .copied()
        .filter(|m| types.iter().any(|t| t.contains(&format!("{m}."))))
        .collect();
    used.iter()
        .map(|m| format!("import type * as {m} from '../{m}';\n"))
        .collect()
}

#[test]
fn ui_contract() -> TestResult {
    let samples_dir = contract_dir().join("samples");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&samples_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort();

    let mut failures = Vec::new();
    // (id, ts type, JSON the UI must accept or sends)
    let mut entries: Vec<(String, String, J)> = Vec::new();
    for path in &files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        // Real describe output: the index round trip test's tables (typed
        // as the designer's TableDetails) and ui_contract_samples.rs's
        // objects (typed as the editors' ObjectDetails).
        let described = name
            .split_once("-index-describe.")
            .and_then(|(_, rest)| rest.strip_suffix(".json"))
            .map(|rest| (format!("TableDetails.{rest}"), "grid.TableDetails"))
            .or_else(|| {
                name.strip_prefix("describe.")
                    .and_then(|rest| rest.strip_suffix(".json"))
                    .map(|rest| (format!("ObjectDetails.{rest}"), "ddl.ObjectDetails"))
            });
        if let Some((id, ty)) = described {
            // Real describe output of the index round trip test.
            let value: J = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            match roundtrip("ObjectDetails", value.clone()) {
                Ok(out) => {
                    let mut ignored = Vec::new();
                    dropped(&value, &out, "", &mut ignored);
                    if !ignored.is_empty() {
                        failures.push(format!("RESULT {name}: Rust ignores keys {ignored:?}"));
                    }
                    entries.push((id, ty.into(), out));
                }
                Err(e) => failures.push(format!("RESULT {name}: Rust rejects: {e}")),
            }
            continue;
        }
        if !name.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            continue;
        }
        for sample in samples_of(path)? {
            let id = field(&sample, "id")?.to_owned();
            let rust = field(&sample, "rust")?;
            let ts = field(&sample, "ts")?.to_owned();
            let dir = field(&sample, "dir")?;
            let value = sample["value"].clone();
            match roundtrip(rust, value.clone()) {
                Ok(out) if dir == "arg" => {
                    let mut ignored = Vec::new();
                    dropped(&value, &out, "", &mut ignored);
                    if !ignored.is_empty() {
                        failures.push(format!("ARG {id} ({rust}): Rust ignores keys {ignored:?}"));
                    }
                    entries.push((id, ts, value));
                }
                Ok(out) => {
                    // A result key Rust does not read is a field the backend no longer sends.
                    let mut ignored = Vec::new();
                    dropped(&value, &out, "", &mut ignored);
                    if !ignored.is_empty() {
                        failures.push(format!(
                            "RESULT {id} ({rust}): Rust ignores keys {ignored:?}"
                        ));
                    }
                    entries.push((id, ts, out));
                }
                Err(e) => failures.push(format!(
                    "{} {id} ({rust}): Rust rejects: {e}",
                    dir.to_uppercase()
                )),
            }
        }
    }
    for (id, ts, value) in constructed()? {
        entries.push((id, ts.into(), value));
    }
    let mut seen = BTreeSet::new();
    for (id, ..) in &entries {
        if !seen.insert(id.clone()) {
            failures.push(format!("duplicate sample id {id}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} contract failures:\n{}",
        failures.len(),
        failures.join("\n")
    );

    let types: Vec<&str> = entries.iter().map(|(_, ts, _)| ts.as_str()).collect();
    let mut ts = String::from(
        "/* Generated by app/src-tauri/tests/ui_contract.rs from samples/. Do not edit:\n   \
         VARVE_UPDATE_CONTRACT=1 cargo test -p varve-app --test ui_contract */\n",
    );
    ts.push_str(&imports(&types));
    ts.push_str(
        "\n/** A value of a type the UI mirrors only partly: extra keys allowed, typed keys checked. */\n\
         function subsetOf<T>() {\n  return <V extends T>(value: V): V => value;\n}\n",
    );
    for (i, (id, ty, value)) in entries.iter().enumerate() {
        let json = serde_json::to_string_pretty(value)?;
        if SUBSET_TYPES.contains(&ty.as_str()) {
            ts.push_str(&format!(
                "\n// {id}\nexport const s{i} = subsetOf<{ty}>()({json});\n"
            ));
        } else {
            ts.push_str(&format!(
                "\n// {id}\nexport const s{i} = {json} satisfies {ty};\n"
            ));
        }
    }
    ts.push_str(&format!(
        "\n/** Every sample id in this file. */\nexport const CONTRACT_IDS: readonly string[] = {};\n",
        serde_json::to_string_pretty(&entries.iter().map(|(id, ..)| id).collect::<Vec<_>>())?
    ));

    let target = contract_dir().join("contract.generated.ts");
    if std::env::var("VARVE_UPDATE_CONTRACT").is_ok_and(|v| v == "1") {
        std::fs::write(&target, &ts)?;
    }
    let current = std::fs::read_to_string(&target).unwrap_or_default();
    println!(
        "{} samples checked ({} files), 0 Rust-side failures",
        entries.len(),
        files.len()
    );
    assert!(
        current == ts,
        "{} is out of date; run VARVE_UPDATE_CONTRACT=1 cargo test -p varve-app --test ui_contract and check it with vue-tsc",
        target.display()
    );
    Ok(())
}
