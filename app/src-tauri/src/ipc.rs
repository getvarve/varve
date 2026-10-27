//! Tauri command wrappers. Each one only unpacks arguments and calls
//! [`Services`]; the logic lives there so it can be tested without a window.

use std::path::PathBuf;
use std::sync::Arc;

use tauri::State;
use varve_activity::{ActivityFilter, Page};
use varve_driver::ExecOutput;
use varve_driver_redis::{DbKeyspace, KeyInfo, KeyType, ScanPage, ValueChunk, ValuePage};
use varve_records::{
    ConnectionInput, ConnectionProfile, Folder, FolderContents, FolderDeleteMode, ReorderKind,
    SavedQuery, SavedQueryInput, SecretKindName,
};

use varve_catalog::data_edit::{
    ApplyOutcome, KeyInfo as GridKeyInfo, PlanOptions, RowChange, RowEditPlan, TableTarget,
};
use varve_catalog::explain::{ExplainCapabilities, ExplainOptions, Plan};
use varve_catalog::{
    Capabilities, Container, EditPlan, KindCount, ObjectDetails, ObjectKind, ObjectRef,
    ObjectSummary,
};
use varve_driver::Value as DriverValue;
use varve_live_stats::redis::{
    ClientInfo, ConfigEntry, Databases, FunctionLibrary, LatencyReport, LatencySample,
    MemoryAnalysis, MemoryScanOptions, PubSubSnapshot, RedisInfo, SlowLog,
};
use varve_live_stats::sql::{
    GaleraStatus, KillOutcome, KillTarget, ProcessList, ServerStatus, ServerVariable,
    VariableChange,
};

use crate::commands::{
    CmdError, ExportFormat, ExportRecord, SecretInput, Services, TestResult, TestTarget, Tree,
    run_blocking,
};
use crate::keychain_cleanup::PendingDeletion;
use crate::m2::PendingRestore;
use crate::m2::ddl::DdlApplied;
use crate::m2::editors::{EditApplied, EngineChange};
use crate::m2::impact::DropImpact;
use crate::m2::live_redis::{ConfigRequest, FunctionRequest, RedisPlanView, RedisTopology};
use crate::m2::plans::Planned;
use crate::sessions::SessionInfo;
use crate::settings::{ActivitySettings, SecretSettings};

use crate::{AppInfo, AppState, DataDir};
use serde_json::Value;
use varve_extension::CommandError;

type Svc<'a> = State<'a, Arc<Services>>;
type R<T> = Result<T, CmdError>;

#[tauri::command]
fn app_info(state: tauri::State<'_, AppState>, data_dir: tauri::State<'_, DataDir>) -> AppInfo {
    AppInfo {
        edition: state.ctx.edition,
        product_name: state.ctx.edition.product_name(),
        version: env!("CARGO_PKG_VERSION"),
        data_dir: data_dir.0.display().to_string(),
        extensions: state.host.extensions().to_vec(),
        capabilities: state.host.capabilities(),
        commands: state.host.command_names(),
        debug_build: cfg!(debug_assertions),
    }
}

/// Calls an extension command. Extensions cannot add Tauri commands of their
/// own; every call goes through this one entry point, by full command name.
#[tauri::command]
async fn ext_invoke(
    state: tauri::State<'_, AppState>,
    command: String,
    payload: Option<Value>,
) -> Result<Value, CommandError> {
    let ctx = state.ctx.clone();
    let host = state.host.clone();
    host.invoke(ctx, &command, payload.unwrap_or(Value::Null))
        .await
}

#[tauri::command]
fn tree(svc: Svc<'_>) -> R<Tree> {
    svc.tree()
}

#[tauri::command]
fn create_folder(
    svc: Svc<'_>,
    parent_id: Option<String>,
    name: String,
    color: Option<String>,
) -> R<Folder> {
    svc.create_folder(parent_id.as_deref(), &name, color.as_deref())
}

#[tauri::command]
fn update_folder(svc: Svc<'_>, id: String, name: String, color: Option<String>) -> R<Folder> {
    svc.update_folder(&id, &name, color.as_deref())
}

#[tauri::command]
async fn move_folder(svc: Svc<'_>, id: String, parent_id: Option<String>) -> R<Folder> {
    let svc = Arc::clone(&svc);
    run_blocking(move || svc.move_folder(&id, parent_id.as_deref())).await
}

#[tauri::command]
fn folder_contents(svc: Svc<'_>, id: String) -> R<FolderContents> {
    svc.folder_contents(&id)
}

#[tauri::command]
async fn delete_folder(svc: Svc<'_>, id: String, mode: FolderDeleteMode) -> R<()> {
    svc.delete_folder(&id, mode).await
}

#[derive(serde::Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum ReorderTarget {
    Folders,
    Connections,
    Queries,
}

#[tauri::command]
fn reorder(svc: Svc<'_>, kind: ReorderTarget, ids: Vec<String>) -> R<()> {
    let kind = match kind {
        ReorderTarget::Folders => ReorderKind::Folders,
        ReorderTarget::Connections => ReorderKind::Connections,
        ReorderTarget::Queries => ReorderKind::Queries,
    };
    svc.reorder(kind, &ids)
}

#[tauri::command]
async fn create_connection(svc: Svc<'_>, input: ConnectionInput) -> R<ConnectionProfile> {
    let svc = Arc::clone(&svc);
    run_blocking(move || svc.create_connection(&input)).await
}

#[tauri::command]
async fn update_connection(
    svc: Svc<'_>,
    id: String,
    input: ConnectionInput,
) -> R<ConnectionProfile> {
    svc.update_connection(&id, &input).await
}

#[tauri::command]
async fn move_connection(
    svc: Svc<'_>,
    id: String,
    folder_id: Option<String>,
) -> R<ConnectionProfile> {
    let svc = Arc::clone(&svc);
    run_blocking(move || svc.move_connection(&id, folder_id.as_deref())).await
}

#[tauri::command]
async fn duplicate_connection(svc: Svc<'_>, id: String) -> R<ConnectionProfile> {
    let svc = Arc::clone(&svc);
    run_blocking(move || svc.duplicate_connection(&id)).await
}

#[tauri::command]
async fn delete_connection(svc: Svc<'_>, id: String) -> R<()> {
    svc.delete_connection(&id).await
}

#[tauri::command]
async fn save_secret(
    svc: Svc<'_>,
    id: String,
    kind: SecretKindName,
    value: SecretInput,
    expected_version: Option<i64>,
) -> R<ConnectionProfile> {
    let svc = Arc::clone(&svc);
    run_blocking(move || svc.save_secret_for(&id, kind, value, expected_version)).await
}

#[tauri::command]
async fn forget_secret(svc: Svc<'_>, id: String, kind: SecretKindName) -> R<ConnectionProfile> {
    let svc = Arc::clone(&svc);
    run_blocking(move || svc.forget_secret(&id, kind)).await
}

#[tauri::command]
async fn trust_ssh_host_key(svc: Svc<'_>, host: String, port: u16, presented_key: String) -> R<()> {
    svc.trust_ssh_host_key(&host, port, &presented_key).await
}

#[tauri::command]
fn create_query(svc: Svc<'_>, input: SavedQueryInput) -> R<SavedQuery> {
    svc.create_query(&input)
}

#[tauri::command]
fn update_query(svc: Svc<'_>, id: String, input: SavedQueryInput) -> R<SavedQuery> {
    svc.update_query(&id, &input)
}

#[tauri::command]
fn delete_query(svc: Svc<'_>, id: String) -> R<()> {
    svc.delete_query(&id)
}

#[tauri::command]
async fn test_connection(
    svc: Svc<'_>,
    target: TestTarget,
    password: Option<SecretInput>,
    ssh_secret: Option<SecretInput>,
    tls_passphrase: Option<SecretInput>,
) -> R<TestResult> {
    svc.test_connection(target, password, ssh_secret, tls_passphrase)
        .await
}

#[tauri::command]
async fn open_session(
    svc: Svc<'_>,
    id: String,
    password: Option<SecretInput>,
    ssh_secret: Option<SecretInput>,
    tls_passphrase: Option<SecretInput>,
) -> R<SessionInfo> {
    svc.open_session(&id, password, ssh_secret, tls_passphrase)
        .await
}

#[tauri::command]
async fn execute(
    svc: Svc<'_>,
    session_id: String,
    sql: String,
    confirmed: bool,
    max_rows: Option<usize>,
) -> R<ExecOutput> {
    svc.execute(&session_id, &sql, confirmed, max_rows).await
}

#[tauri::command]
async fn cancel(svc: Svc<'_>, session_id: String) -> R<()> {
    svc.cancel(&session_id).await
}

#[tauri::command]
async fn close_session(svc: Svc<'_>, session_id: String) -> R<()> {
    svc.close_session(&session_id).await
}

#[tauri::command]
fn sessions(svc: Svc<'_>) -> Vec<SessionInfo> {
    svc.sessions()
}

/// What the last reload of the window closed (read once by the new page).
#[tauri::command]
async fn reload_report(svc: Svc<'_>) -> R<crate::sessions::ReloadReport> {
    Ok(svc.reload_report().await)
}

/// Profiles of user sessions with an open transaction (asked before a reload).
#[tauri::command]
async fn open_transactions(svc: Svc<'_>) -> R<Vec<String>> {
    Ok(svc.open_transactions().await)
}

#[tauri::command]
fn activity(svc: Svc<'_>, filter: ActivityFilter) -> R<Page> {
    svc.activity(&filter)
}

#[tauri::command]
fn query_history(svc: Svc<'_>, connection_id: String, limit: u32) -> R<Page> {
    svc.query_history(&connection_id, limit)
}

#[tauri::command]
fn record_export(svc: Svc<'_>, record: ExportRecord) -> R<()> {
    svc.record_export(&record)
}

#[tauri::command]
fn clear_activity(svc: Svc<'_>, connection_id: Option<String>) -> R<u64> {
    svc.clear_activity(connection_id.as_deref())
}

#[tauri::command]
fn activity_settings(svc: Svc<'_>) -> R<ActivitySettings> {
    svc.activity_settings()
}

#[tauri::command]
fn set_activity_settings(svc: Svc<'_>, settings: ActivitySettings) -> R<u64> {
    svc.set_activity_settings(settings)
}

#[tauri::command]
fn secret_settings(svc: Svc<'_>) -> R<SecretSettings> {
    svc.secret_settings()
}

#[tauri::command]
fn set_secret_settings(svc: Svc<'_>, settings: SecretSettings) -> R<()> {
    svc.set_secret_settings(settings)
}

/// Drops the secrets cached since the last keychain read; resolves how
/// many were dropped.
#[tauri::command]
fn forget_cached_secrets(svc: Svc<'_>) -> usize {
    svc.forget_cached_secrets()
}

/// Exports only to a file the user picked in the save dialog: the dialog
/// plugin adds that path to the fs scope, so any other path is refused.
#[tauri::command]
fn export_activity(
    app: tauri::AppHandle,
    svc: Svc<'_>,
    filter: ActivityFilter,
    path: PathBuf,
    format: ExportFormat,
) -> R<u64> {
    use tauri_plugin_fs::FsExt;
    if !app.fs_scope().is_allowed(&path) {
        return Err(CmdError::Invalid {
            message: "choose the export file in the save dialog".into(),
        });
    }
    svc.export_activity(&filter, &path, format)
}

#[tauri::command]
async fn redis_scan(
    svc: Svc<'_>,
    session_id: String,
    cursor: String,
    pattern: String,
    key_type: Option<KeyType>,
    count: u32,
) -> R<ScanPage> {
    // Cursors travel as decimal strings: JavaScript numbers cannot hold
    // every u64.
    let cursor = cursor.parse::<u64>().map_err(|_| CmdError::Invalid {
        message: format!("{cursor} is not a SCAN cursor"),
    })?;
    svc.redis_scan(&session_id, cursor, &pattern, key_type, count)
        .await
}

#[tauri::command]
async fn redis_key_info(svc: Svc<'_>, session_id: String, key: String) -> R<Option<KeyInfo>> {
    svc.redis_key_info(&session_id, &key).await
}

#[tauri::command]
async fn redis_read_value(
    svc: Svc<'_>,
    session_id: String,
    key: String,
    page: ValuePage,
) -> R<Option<ValueChunk>> {
    svc.redis_read_value(&session_id, &key, page).await
}

#[tauri::command]
async fn redis_select_db(svc: Svc<'_>, session_id: String, db: u16) -> R<()> {
    svc.redis_select_db(&session_id, db).await
}

#[tauri::command]
async fn redis_keyspace(svc: Svc<'_>, session_id: String) -> R<Vec<DbKeyspace>> {
    svc.redis_keyspace(&session_id).await
}

// M2: object browser

#[tauri::command]
async fn catalog_capabilities(svc: Svc<'_>, session_id: String) -> R<Capabilities> {
    svc.catalog_capabilities(&session_id).await
}

#[tauri::command]
async fn catalog_databases(svc: Svc<'_>, session_id: String) -> R<Vec<ObjectSummary>> {
    svc.catalog_databases(&session_id).await
}

#[tauri::command]
async fn catalog_schemas(
    svc: Svc<'_>,
    session_id: String,
    database: String,
) -> R<Vec<ObjectSummary>> {
    svc.catalog_schemas(&session_id, &database).await
}

#[tauri::command]
async fn catalog_kinds(
    svc: Svc<'_>,
    session_id: String,
    container: Container,
    include_system: Option<bool>,
) -> R<Vec<KindCount>> {
    svc.catalog_kinds(&session_id, &container, include_system)
        .await
}

#[tauri::command]
async fn catalog_list(
    svc: Svc<'_>,
    session_id: String,
    container: Container,
    kind: ObjectKind,
    include_system: Option<bool>,
) -> R<Vec<ObjectSummary>> {
    svc.catalog_list(&session_id, &container, kind, include_system)
        .await
}

#[tauri::command]
async fn catalog_describe(svc: Svc<'_>, session_id: String, object: ObjectRef) -> R<ObjectDetails> {
    svc.catalog_describe(&session_id, &object).await
}

#[tauri::command]
async fn catalog_ddl(svc: Svc<'_>, session_id: String, object: ObjectRef) -> R<String> {
    svc.catalog_ddl(&session_id, &object).await
}

#[tauri::command]
async fn catalog_drop_impact(svc: Svc<'_>, session_id: String, object: ObjectRef) -> R<DropImpact> {
    svc.catalog_drop_impact(&session_id, &object).await
}

#[tauri::command]
async fn catalog_available_extensions(
    svc: Svc<'_>,
    session_id: String,
) -> R<Vec<varve_catalog::postgres::programs::PgAvailableExtension>> {
    svc.catalog_available_extensions(&session_id).await
}

#[tauri::command]
async fn catalog_engines(
    svc: Svc<'_>,
    session_id: String,
) -> R<Vec<varve_catalog::mysql::MyEngine>> {
    svc.catalog_engines(&session_id).await
}

#[tauri::command]
async fn catalog_row_count(
    svc: Svc<'_>,
    session_id: String,
    database: String,
    table: String,
) -> R<Option<u64>> {
    svc.catalog_row_count(&session_id, &database, &table).await
}

#[tauri::command]
async fn open_database_session(
    svc: Svc<'_>,
    id: String,
    database: String,
    password: Option<SecretInput>,
    ssh_secret: Option<SecretInput>,
    tls_passphrase: Option<SecretInput>,
) -> R<SessionInfo> {
    svc.open_database_session(&id, &database, password, ssh_secret, tls_passphrase)
        .await
}

// M2: editors, EXPLAIN, grid editing

#[tauri::command]
fn edit_plan(svc: Svc<'_>, session_id: String, change: EngineChange) -> R<Planned<EditPlan>> {
    svc.edit_plan(&session_id, &change)
}

#[tauri::command]
async fn edit_apply(
    svc: Svc<'_>,
    session_id: String,
    plan_id: String,
    confirmed: bool,
) -> R<EditApplied> {
    svc.edit_apply(&session_id, &plan_id, confirmed).await
}

#[tauri::command]
async fn ddl_apply(
    svc: Svc<'_>,
    session_id: String,
    ddl: String,
    database: Option<String>,
    confirmed: bool,
) -> R<DdlApplied> {
    svc.ddl_apply(&session_id, &ddl, database.as_deref(), confirmed)
        .await
}

#[tauri::command]
fn pending_keychain_deletions(svc: Svc<'_>) -> Vec<PendingDeletion> {
    svc.pending_keychain_deletions()
}

#[tauri::command]
async fn retry_keychain_cleanup(svc: Svc<'_>) -> R<Vec<String>> {
    let svc = Arc::clone(&svc);
    run_blocking(move || Ok(svc.retry_keychain_cleanup())).await
}

#[tauri::command]
fn ddl_pending_restores(svc: Svc<'_>) -> Vec<PendingRestore> {
    svc.ddl_pending_restores()
}

#[tauri::command]
async fn ddl_restore_collation(
    svc: Svc<'_>,
    session_id: String,
    database: String,
    confirmed: Option<bool>,
) -> R<()> {
    svc.ddl_restore_collation(&session_id, &database, confirmed.unwrap_or(false))
        .await
}

#[tauri::command]
async fn ddl_dismiss_pending_restore(svc: Svc<'_>, server: String, database: String) -> R<()> {
    svc.ddl_dismiss_pending_restore(&server, &database).await
}

#[tauri::command]
fn explain_capabilities(svc: Svc<'_>, session_id: String) -> R<ExplainCapabilities> {
    svc.explain_capabilities(&session_id)
}

#[tauri::command]
async fn explain(
    svc: Svc<'_>,
    session_id: String,
    statement: String,
    options: ExplainOptions,
    confirmed: bool,
) -> R<Plan> {
    svc.explain(&session_id, &statement, &options, confirmed)
        .await
}

#[tauri::command]
async fn editable_key(
    svc: Svc<'_>,
    session_id: String,
    target: TableTarget,
) -> R<Option<GridKeyInfo>> {
    svc.editable_key(&session_id, &target).await
}

#[tauri::command]
fn row_edit_plan(
    svc: Svc<'_>,
    session_id: String,
    target: TableTarget,
    changes: Vec<RowChange>,
    options: PlanOptions,
) -> R<Planned<RowEditPlan>> {
    svc.row_edit_plan(&session_id, &target, &changes, &options)
}

#[tauri::command]
async fn row_edit_apply(
    svc: Svc<'_>,
    session_id: String,
    plan_id: String,
    confirmed: bool,
) -> R<ApplyOutcome> {
    svc.row_edit_apply(&session_id, &plan_id, confirmed).await
}

// M2: SQL live panels

#[tauri::command]
async fn process_list(svc: Svc<'_>, session_id: String) -> R<ProcessList> {
    svc.process_list(&session_id).await
}

#[tauri::command]
async fn kill_process(
    svc: Svc<'_>,
    session_id: String,
    id: i64,
    target: KillTarget,
) -> R<KillOutcome> {
    svc.kill_process(&session_id, id, target).await
}

#[tauri::command]
async fn server_variables(svc: Svc<'_>, session_id: String) -> R<Vec<ServerVariable>> {
    svc.server_variables(&session_id).await
}

#[tauri::command]
async fn variable_change_plan(
    svc: Svc<'_>,
    session_id: String,
    name: String,
    change: VariableChange,
) -> R<Planned<EditPlan>> {
    svc.variable_change_plan(&session_id, &name, &change).await
}

#[tauri::command]
async fn variable_change_apply(
    svc: Svc<'_>,
    session_id: String,
    plan_id: String,
    confirmed: bool,
) -> R<()> {
    svc.variable_change_apply(&session_id, &plan_id, confirmed)
        .await
}

#[tauri::command]
async fn server_status(svc: Svc<'_>, session_id: String) -> R<ServerStatus> {
    svc.server_status(&session_id).await
}

#[tauri::command]
async fn galera_status(svc: Svc<'_>, session_id: String) -> R<GaleraStatus> {
    svc.galera_status(&session_id).await
}

// M2: Redis live panels

#[tauri::command]
async fn redis_info(svc: Svc<'_>, session_id: String) -> R<RedisInfo> {
    svc.redis_info(&session_id).await
}

#[tauri::command]
async fn redis_databases(svc: Svc<'_>, session_id: String) -> R<Databases> {
    svc.redis_databases(&session_id).await
}

#[tauri::command]
async fn redis_slowlog(svc: Svc<'_>, session_id: String, count: Option<u32>) -> R<SlowLog> {
    svc.redis_slowlog(&session_id, count).await
}

#[tauri::command]
async fn redis_slowlog_reset(svc: Svc<'_>, session_id: String) -> R<()> {
    svc.redis_slowlog_reset(&session_id).await
}

#[tauri::command]
async fn redis_latency(svc: Svc<'_>, session_id: String) -> R<LatencyReport> {
    svc.redis_latency(&session_id).await
}

#[tauri::command]
async fn redis_latency_history(
    svc: Svc<'_>,
    session_id: String,
    event: String,
) -> R<Vec<LatencySample>> {
    svc.redis_latency_history(&session_id, &event).await
}

#[tauri::command]
async fn redis_latency_reset(
    svc: Svc<'_>,
    session_id: String,
    events: Option<Vec<String>>,
) -> R<i64> {
    svc.redis_latency_reset(&session_id, &events.unwrap_or_default())
        .await
}

#[tauri::command]
async fn redis_clients(svc: Svc<'_>, session_id: String) -> R<Vec<ClientInfo>> {
    svc.redis_clients(&session_id).await
}

#[tauri::command]
async fn redis_client_kill(svc: Svc<'_>, session_id: String, id: i64) -> R<()> {
    svc.redis_client_kill(&session_id, id).await
}

#[tauri::command]
async fn redis_config(
    svc: Svc<'_>,
    session_id: String,
    pattern: Option<String>,
) -> R<Vec<ConfigEntry>> {
    svc.redis_config(&session_id, pattern.as_deref()).await
}

#[tauri::command]
async fn redis_config_plan(
    svc: Svc<'_>,
    session_id: String,
    request: ConfigRequest,
) -> R<Planned<RedisPlanView>> {
    svc.redis_config_plan(&session_id, &request).await
}

#[tauri::command]
fn redis_function_plan(
    svc: Svc<'_>,
    session_id: String,
    request: FunctionRequest,
) -> R<Planned<RedisPlanView>> {
    svc.redis_function_plan(&session_id, &request)
}

#[tauri::command]
async fn redis_plan_apply(
    svc: Svc<'_>,
    session_id: String,
    plan_id: String,
    confirmed: bool,
) -> R<Vec<DriverValue>> {
    svc.redis_plan_apply(&session_id, &plan_id, confirmed).await
}

#[tauri::command]
async fn redis_memory_analysis(
    svc: Svc<'_>,
    session_id: String,
    options: Option<MemoryScanOptions>,
    run_id: Option<String>,
) -> R<MemoryAnalysis> {
    svc.redis_memory_analysis(&session_id, options.as_ref(), run_id.as_deref())
        .await
}

#[tauri::command]
fn redis_memory_analysis_stop(svc: Svc<'_>, session_id: String, run_id: String) {
    svc.redis_memory_analysis_stop(&session_id, &run_id);
}

// ---- Redis modules, console help and bulk TTL (m2/redis_modules.rs,
// m2/redis_bulk.rs) ----

#[tauri::command]
async fn redis_modules(svc: Svc<'_>, session_id: String) -> R<varve_driver_redis::ModuleReport> {
    svc.redis_modules(&session_id).await
}

#[tauri::command]
async fn redis_search_indexes(svc: Svc<'_>, session_id: String) -> R<Vec<String>> {
    svc.redis_search_indexes(&session_id).await
}

#[tauri::command]
async fn redis_search_index(
    svc: Svc<'_>,
    session_id: String,
    name: String,
) -> R<Option<varve_driver_redis::SearchIndex>> {
    svc.redis_search_index(&session_id, &name).await
}

#[tauri::command]
async fn redis_ts_range(
    svc: Svc<'_>,
    session_id: String,
    key: String,
    query: varve_driver_redis::TsRangeQuery,
) -> R<Option<varve_driver_redis::TsRangePage>> {
    svc.redis_ts_range(&session_id, &key, &query).await
}

#[tauri::command]
async fn redis_module_query(
    svc: Svc<'_>,
    session_id: String,
    key: String,
    query: varve_driver_redis::ModuleQuery,
) -> R<Option<varve_driver_redis::QueryResult>> {
    svc.redis_module_query(&session_id, &key, &query).await
}

#[tauri::command]
async fn redis_command_help(
    svc: Svc<'_>,
    session_id: String,
    name: String,
) -> R<varve_driver_redis::CommandHelp> {
    svc.redis_command_help(&session_id, &name).await
}

#[tauri::command]
async fn redis_bulk_ttl_preview(
    svc: Svc<'_>,
    session_id: String,
    filter: varve_driver_redis::BulkFilter,
) -> R<varve_driver_redis::BulkCount> {
    svc.redis_bulk_ttl_preview(&session_id, &filter).await
}

#[tauri::command]
async fn redis_bulk_ttl_start(
    svc: Svc<'_>,
    session_id: String,
    request: crate::m2::redis_bulk::BulkTtlRequest,
    confirmed: Option<bool>,
) -> R<crate::m2::redis_bulk::BulkTtlJob> {
    svc.redis_bulk_ttl_start(&session_id, &request, confirmed.unwrap_or(false))
        .await
}

#[tauri::command]
fn redis_bulk_ttl_jobs(
    svc: Svc<'_>,
    session_id: String,
) -> Vec<crate::m2::redis_bulk::BulkTtlStatus> {
    svc.redis_bulk_ttl_jobs(&session_id)
}

#[tauri::command]
fn redis_bulk_ttl_status(
    svc: Svc<'_>,
    session_id: String,
    job_id: String,
) -> R<crate::m2::redis_bulk::BulkTtlStatus> {
    svc.redis_bulk_ttl_status(&session_id, &job_id)
}

#[tauri::command]
fn redis_bulk_ttl_stop(
    svc: Svc<'_>,
    session_id: String,
    job_id: String,
) -> R<crate::m2::redis_bulk::BulkTtlStatus> {
    svc.redis_bulk_ttl_stop(&session_id, &job_id)
}

#[tauri::command]
async fn redis_topology(svc: Svc<'_>, session_id: String) -> R<RedisTopology> {
    svc.redis_topology(&session_id).await
}

#[tauri::command]
async fn redis_functions(
    svc: Svc<'_>,
    session_id: String,
    library_pattern: Option<String>,
    with_code: Option<bool>,
) -> R<Vec<FunctionLibrary>> {
    svc.redis_functions(
        &session_id,
        library_pattern.as_deref(),
        with_code.unwrap_or(false),
    )
    .await
}

#[tauri::command]
async fn redis_pubsub(
    svc: Svc<'_>,
    session_id: String,
    pattern: Option<String>,
    max_channels: Option<usize>,
) -> R<PubSubSnapshot> {
    svc.redis_pubsub(&session_id, pattern.as_deref(), max_channels)
        .await
}

// ---- B2: PostgreSQL LISTEN / NOTIFY (m2/notify.rs) ----

/// Sends a tab's listener events to the window.
struct WindowNotifyEmitter(tauri::AppHandle);

impl crate::m2::notify::NotifyEmitter for WindowNotifyEmitter {
    fn messages(&self, batch: crate::m2::notify::NotifyBatch) {
        use tauri::Emitter;
        if let Err(error) = self.0.emit(crate::m2::notify::MESSAGES_EVENT, batch) {
            tracing::debug!(%error, "could not send notifications to the window");
        }
    }

    fn status(&self, state: crate::m2::notify::NotifyState) {
        use tauri::Emitter;
        if let Err(error) = self.0.emit(crate::m2::notify::STATUS_EVENT, state) {
            tracing::debug!(%error, "could not send the listener state to the window");
        }
    }
}

fn notify_emitter(app: tauri::AppHandle) -> Arc<dyn crate::m2::notify::NotifyEmitter> {
    Arc::new(WindowNotifyEmitter(app))
}

#[tauri::command]
fn notify_state(svc: Svc<'_>, session_id: String) -> R<crate::m2::notify::NotifyState> {
    svc.notify_state(&session_id)
}

#[tauri::command]
async fn notify_listen(
    app: tauri::AppHandle,
    svc: Svc<'_>,
    session_id: String,
    channel: String,
) -> R<crate::m2::notify::NotifyState> {
    svc.inner()
        .notify_listen(&session_id, &channel, notify_emitter(app))
        .await
}

#[tauri::command]
async fn notify_unlisten(
    app: tauri::AppHandle,
    svc: Svc<'_>,
    session_id: String,
    channel: String,
) -> R<crate::m2::notify::NotifyState> {
    svc.notify_unlisten(&session_id, &channel, notify_emitter(app))
        .await
}

#[tauri::command]
async fn notify_send(
    app: tauri::AppHandle,
    svc: Svc<'_>,
    session_id: String,
    channel: String,
    payload: Option<String>,
    confirmed: Option<bool>,
) -> R<crate::m2::notify::NotifySent> {
    svc.inner()
        .notify_send(
            &session_id,
            &channel,
            payload.as_deref().unwrap_or_default(),
            confirmed.unwrap_or(false),
            notify_emitter(app),
        )
        .await
}

#[tauri::command]
fn notify_ack(svc: Svc<'_>, session_id: String, listener_id: u64, seq: u64) -> R<()> {
    svc.notify_ack(&session_id, listener_id, seq)
}

#[tauri::command]
async fn notify_stop(svc: Svc<'_>, session_id: String) -> R<()> {
    svc.notify_stop(&session_id).await
}

// ---- end B2 ----

// ---- B1: Redis value editors, consumer groups, sorted set ranges, Pub/Sub
// (m2/redis_edit.rs, m2/pubsub.rs) ----

#[tauri::command]
async fn redis_key_version(
    svc: Svc<'_>,
    session_id: String,
    key: String,
) -> R<Option<varve_driver_redis::KeyVersion>> {
    svc.redis_key_version(&session_id, &key).await
}

#[tauri::command]
fn redis_key_digest_cancel(svc: Svc<'_>, session_id: String, read_id: String) {
    svc.redis_key_digest_cancel(&session_id, &read_id);
}

#[tauri::command]
async fn redis_edit_plan(
    svc: Svc<'_>,
    session_id: String,
    request: crate::m2::redis_edit::RedisEditRequest,
) -> R<Planned<varve_driver_redis::RedisEditPlan>> {
    svc.redis_edit_plan(&session_id, &request).await
}

#[tauri::command]
async fn redis_edit_apply(
    svc: Svc<'_>,
    session_id: String,
    plan_id: String,
    confirmed: bool,
) -> R<varve_driver_redis::EditOutcome> {
    svc.redis_edit_apply(&session_id, &plan_id, confirmed).await
}

#[tauri::command]
async fn redis_zset_range(
    svc: Svc<'_>,
    session_id: String,
    key: String,
    range: varve_driver_redis::ZsetRange,
) -> R<Option<varve_driver_redis::ZsetRangePage>> {
    svc.redis_zset_range(&session_id, &key, &range).await
}

#[tauri::command]
async fn redis_stream_info(
    svc: Svc<'_>,
    session_id: String,
    key: String,
) -> R<Option<varve_driver_redis::StreamInfo>> {
    svc.redis_stream_info(&session_id, &key).await
}

#[tauri::command]
async fn redis_stream_groups(
    svc: Svc<'_>,
    session_id: String,
    key: String,
) -> R<Vec<varve_driver_redis::StreamGroup>> {
    svc.redis_stream_groups(&session_id, &key).await
}

#[tauri::command]
async fn redis_stream_consumers(
    svc: Svc<'_>,
    session_id: String,
    key: String,
    group: String,
) -> R<Vec<varve_driver_redis::StreamConsumer>> {
    svc.redis_stream_consumers(&session_id, &key, &group).await
}

#[tauri::command]
async fn redis_stream_pending_summary(
    svc: Svc<'_>,
    session_id: String,
    key: String,
    group: String,
) -> R<varve_driver_redis::PendingSummary> {
    svc.redis_stream_pending_summary(&session_id, &key, &group)
        .await
}

#[tauri::command]
async fn redis_stream_pending(
    svc: Svc<'_>,
    session_id: String,
    key: String,
    group: String,
    page: varve_driver_redis::PendingQuery,
) -> R<varve_driver_redis::PendingPage> {
    svc.redis_stream_pending(&session_id, &key, &group, &page)
        .await
}

#[tauri::command]
async fn redis_stream_group_plan(
    svc: Svc<'_>,
    session_id: String,
    key: String,
    change: varve_driver_redis::StreamGroupChange,
) -> R<Planned<varve_driver_redis::RedisEditPlan>> {
    svc.redis_stream_group_plan(&session_id, &key, &change)
        .await
}

/// Sends Pub/Sub events to the windows.
struct WindowPubSubEmitter(tauri::AppHandle);

impl crate::m2::pubsub::PubSubEmitter for WindowPubSubEmitter {
    fn emit(&self, event: crate::m2::pubsub::PubSubEvent) {
        use tauri::Emitter;
        if let Err(error) = self.0.emit(crate::m2::pubsub::EVENT, event) {
            tracing::warn!(%error, "could not send a Pub/Sub event");
        }
    }
}

// The arguments are the IPC contract (M2-UI-API.md), one per field.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn redis_pubsub_subscribe(
    app: tauri::AppHandle,
    svc: Svc<'_>,
    session_id: String,
    channels: Vec<String>,
    patterns: Vec<String>,
    shard_channels: Option<Vec<String>>,
    max_payload_bytes: Option<u64>,
    resubscribe: Option<bool>,
) -> R<crate::m2::pubsub::PubSubSubscription> {
    let names = crate::m2::pubsub::PubSubNames {
        channels,
        patterns,
        shard_channels: shard_channels.unwrap_or_default(),
    };
    let defaults = crate::m2::pubsub::SubscribeOptions::default();
    let options = crate::m2::pubsub::SubscribeOptions {
        max_payload_bytes: max_payload_bytes.unwrap_or(defaults.max_payload_bytes),
        resubscribe: resubscribe.unwrap_or(defaults.resubscribe),
    };
    svc.redis_pubsub_subscribe(
        &session_id,
        &names,
        options,
        Arc::new(WindowPubSubEmitter(app)),
    )
    .await
}

#[tauri::command]
async fn redis_pubsub_change(
    svc: Svc<'_>,
    subscription_id: String,
    add: crate::m2::pubsub::PubSubNames,
    remove: crate::m2::pubsub::PubSubNames,
) -> R<()> {
    svc.redis_pubsub_change(&subscription_id, &add, &remove)
        .await
}

#[tauri::command]
fn redis_pubsub_ack(svc: Svc<'_>, subscription_id: String, seq: u64) -> R<()> {
    svc.redis_pubsub_ack(&subscription_id, seq)
}

#[tauri::command]
fn redis_pubsub_unsubscribe(svc: Svc<'_>, subscription_id: String) {
    svc.redis_pubsub_unsubscribe(&subscription_id);
}

#[tauri::command]
fn redis_pubsub_subscriptions(
    svc: Svc<'_>,
    session_id: String,
) -> R<Vec<crate::m2::pubsub::PubSubSubscriptionInfo>> {
    svc.redis_pubsub_subscriptions(&session_id)
}

#[tauri::command]
async fn redis_publish(
    svc: Svc<'_>,
    session_id: String,
    channel: String,
    message: DriverValue,
    confirmed: bool,
) -> R<i64> {
    svc.redis_publish(&session_id, &channel, &message, confirmed)
        .await
}

// ---- end B1 ----

/// Every command of the app.
pub(crate) fn handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        crate::quit::confirm_quit,
        crate::quit::cancel_quit,
        crate::quit::quit_ready,
        crate::quit::quit_acknowledged,
        app_info,
        ext_invoke,
        tree,
        create_folder,
        update_folder,
        move_folder,
        folder_contents,
        delete_folder,
        reorder,
        create_connection,
        update_connection,
        move_connection,
        duplicate_connection,
        delete_connection,
        save_secret,
        forget_secret,
        trust_ssh_host_key,
        create_query,
        update_query,
        delete_query,
        test_connection,
        open_session,
        execute,
        cancel,
        close_session,
        sessions,
        reload_report,
        open_transactions,
        activity,
        query_history,
        clear_activity,
        record_export,
        activity_settings,
        set_activity_settings,
        secret_settings,
        set_secret_settings,
        forget_cached_secrets,
        export_activity,
        redis_scan,
        redis_key_info,
        redis_read_value,
        redis_select_db,
        redis_keyspace,
        catalog_capabilities,
        catalog_databases,
        catalog_schemas,
        catalog_kinds,
        catalog_list,
        catalog_describe,
        catalog_ddl,
        catalog_drop_impact,
        catalog_engines,
        catalog_row_count,
        catalog_available_extensions,
        open_database_session,
        edit_plan,
        edit_apply,
        ddl_apply,
        pending_keychain_deletions,
        retry_keychain_cleanup,
        ddl_pending_restores,
        ddl_restore_collation,
        ddl_dismiss_pending_restore,
        explain_capabilities,
        explain,
        editable_key,
        row_edit_plan,
        row_edit_apply,
        process_list,
        kill_process,
        server_variables,
        variable_change_plan,
        variable_change_apply,
        server_status,
        galera_status,
        redis_info,
        redis_databases,
        redis_slowlog,
        redis_slowlog_reset,
        redis_latency,
        redis_latency_history,
        redis_latency_reset,
        redis_clients,
        redis_client_kill,
        redis_config,
        redis_config_plan,
        redis_function_plan,
        redis_plan_apply,
        redis_memory_analysis,
        redis_memory_analysis_stop,
        redis_modules,
        redis_search_indexes,
        redis_search_index,
        redis_ts_range,
        redis_module_query,
        redis_command_help,
        redis_bulk_ttl_preview,
        redis_bulk_ttl_start,
        redis_bulk_ttl_jobs,
        redis_bulk_ttl_status,
        redis_bulk_ttl_stop,
        redis_topology,
        redis_functions,
        redis_pubsub,
        // B2: PostgreSQL LISTEN / NOTIFY.
        notify_state,
        notify_listen,
        notify_unlisten,
        notify_send,
        notify_ack,
        notify_stop,
        // B1: Redis value editors, consumer groups, ranges, Pub/Sub.
        redis_key_version,
        redis_key_digest_cancel,
        redis_edit_plan,
        redis_edit_apply,
        redis_zset_range,
        redis_stream_info,
        redis_stream_groups,
        redis_stream_consumers,
        redis_stream_pending_summary,
        redis_stream_pending,
        redis_stream_group_plan,
        redis_pubsub_subscribe,
        redis_pubsub_change,
        redis_pubsub_ack,
        redis_pubsub_unsubscribe,
        redis_pubsub_subscriptions,
        redis_publish,
    ]
}
