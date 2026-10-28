//! The Varve desktop app.
//!
//! Varve Community and Varve Pro are the same app: each edition is a small
//! binary crate that calls [`run`] with its own Tauri context (product name,
//! bundle id, frontend) and its list of extensions. Community passes none.

pub mod activity_bridge;
pub mod commands;
pub mod glass;
mod ipc;
pub mod keychain_cleanup;
pub mod m2;
pub mod permissions;
pub mod quit;
pub mod secret_versions;
pub mod sessions;
pub mod settings;
pub mod tunnels;
pub mod updates;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::Manager;
use varve_activity::{Recorder, RecorderSettings};
use varve_core::Edition;
use varve_driver::Driver;
use varve_extension::{
    AppContext, Extension, ExtensionError, ExtensionHost, ExtensionInfo, HostBuilder,
};
use varve_records::Records;
use varve_secrets::{KeychainStore, MemoryStore, SecretCache, SecretStore};
use varve_store::{MigrationSet, Store, StoreError, core_migrations};

use crate::activity_bridge::ActivityBridge;
use crate::commands::Services;
use crate::sessions::{SessionEvent, SessionManager};
use crate::tunnels::Tunnels;

/// Environment variable that overrides the data directory, for development
/// and tests.
pub const DATA_DIR_ENV: &str = "VARVE_DATA_DIR";

/// Environment variable that selects the secret store: `memory` keeps
/// secrets in process memory only (development and automated tests on
/// machines without a keychain). Anything else uses the OS keychain.
pub const SECRETS_ENV: &str = "VARVE_SECRETS";

/// Errors that stop the app from starting.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// An extension failed to register.
    #[error("extension failed to register: {0}")]
    Extension(#[from] ExtensionError),
    /// `app.db` could not be opened or migrated.
    #[error("local database error: {0}")]
    Store(#[from] StoreError),
    /// Tauri failed to start or run.
    #[error("window system error: {0}")]
    Tauri(#[from] tauri::Error),
    /// The data directory could not be found or created.
    #[error("data directory error: {0}")]
    DataDir(String),
}

/// State shared with every command.
pub(crate) struct AppState {
    pub(crate) ctx: AppContext,
    pub(crate) host: ExtensionHost,
}

/// What the UI needs to know about the running app.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppInfo {
    edition: Edition,
    product_name: &'static str,
    version: &'static str,
    data_dir: String,
    extensions: Vec<ExtensionInfo>,
    capabilities: Vec<String>,
    commands: Vec<String>,
    /// A debug build (`tauri dev`): the webview's own menus and reload stay.
    debug_build: bool,
    /// The native glass behind the window; the UI is translucent only over it.
    glass: glass::Glass,
}

/// Directory that holds `app.db`.
pub(crate) struct DataDir(pub(crate) PathBuf);

/// Registers the extensions and returns the host. Runs before any window
/// opens, so a broken extension stops the app with a clear error.
///
/// # Errors
/// Returns the first registration error.
pub fn build_host(extensions: &[Box<dyn Extension>]) -> Result<ExtensionHost, ExtensionError> {
    let mut builder = HostBuilder::new();
    for extension in extensions {
        builder.add(extension.as_ref())?;
    }
    Ok(builder.build())
}

/// Opens `app.db` in `dir` and applies core and extension migrations.
///
/// # Errors
/// Returns an error when the directory cannot be created or a migration fails.
pub fn open_store(dir: &std::path::Path, host: &ExtensionHost) -> Result<Store, AppError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| AppError::DataDir(format!("{}: {e}", dir.display())))?;
    let store = Store::open(&dir.join("app.db"))?;
    let mut sets: Vec<MigrationSet> = vec![core_migrations()];
    sets.extend(host.migration_sets().iter().cloned());
    store.migrate(&sets)?;
    Ok(store)
}

/// The data directory shared by every edition: `<OS data dir>/Varve`.
///
/// It does not depend on the bundle id, so Community and Pro read the same
/// `app.db` (design doc section 2.3). [`DATA_DIR_ENV`] overrides it.
fn data_dir(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    let base = app
        .path()
        .data_dir()
        .map_err(|e| AppError::DataDir(e.to_string()))?;
    Ok(base.join("Varve"))
}

/// Reads a value from `core_meta`.
pub(crate) fn meta_get(store: &Store, key: &str) -> Result<Option<String>, StoreError> {
    store.with_conn(|c| {
        let mut stmt = c.prepare("SELECT value FROM core_meta WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    })
}

/// Writes a value to `core_meta`.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn meta_set(store: &Store, key: &str, value: &str) -> Result<(), StoreError> {
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO core_meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    })
}

/// The id of this device, created on first start.
///
/// # Errors
/// Storage errors.
pub fn device_id(store: &Store) -> Result<String, StoreError> {
    if let Some(id) = meta_get(store, "device_id")? {
        return Ok(id);
    }
    let id = uuid::Uuid::now_v7().to_string();
    store.with_conn(|c| {
        c.execute(
            "INSERT OR IGNORE INTO core_meta (key, value) VALUES ('device_id', ?1)",
            [&id],
        )?;
        Ok(())
    })?;
    Ok(meta_get(store, "device_id")?.unwrap_or(id))
}

/// The database drivers of Varve Community.
#[must_use]
pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(varve_driver_postgres::PostgresDriver::new()),
        Arc::new(varve_driver_mysql::MysqlDriver::new()),
        Arc::new(varve_driver_redis::RedisDriver::new()),
    ]
}

/// Builds every service the commands use.
///
/// # Errors
/// Storage errors.
pub fn build_services(
    store: Arc<Store>,
    dir: &std::path::Path,
    host: &ExtensionHost,
    edition: Edition,
    secrets: Arc<dyn SecretStore>,
) -> Result<Arc<Services>, AppError> {
    let device = device_id(&store)?;
    let recorder = Arc::new(Recorder::new(
        Arc::clone(&store),
        RecorderSettings {
            actor_id: varve_activity::LOCAL_ACTOR.to_string(),
            device_id: device.clone(),
            text_level: settings::load(&store)?.text_level,
        },
    ));
    let ctx = AppContext {
        store: Arc::clone(&store),
        edition,
    };

    // Activity events reach extensions through the on_activity hook.
    let hook_host = host.clone();
    let hook_ctx = ctx.clone();
    recorder.on_recorded(move |event| {
        hook_host.emit_activity(
            &hook_ctx,
            &varve_extension::ActivityEvent {
                action: event.action.clone(),
                details: event.details.clone(),
            },
        );
    });

    let records = Records::new(Arc::clone(&store));
    let bridge = ActivityBridge::new(Arc::clone(&recorder), records.clone());
    let connect_host = host.clone();
    let connect_ctx = ctx;
    let catalogs = Arc::new(m2::CatalogCache::default());
    let sink_catalogs = Arc::clone(&catalogs);
    let m2_state = Arc::new(m2::M2State::new(Arc::clone(&store)));
    let sink_m2 = Arc::clone(&m2_state);
    let sink: crate::sessions::EventSink = Arc::new(move |event: SessionEvent| {
        if let SessionEvent::Closed { info, .. } = &event {
            sink_catalogs.forget(&info.id);
            sink_m2.plans.forget_session(&info.id);
            // B2: stop the tab's LISTEN / NOTIFY listener.
            sink_m2.notify.forget_session(&info.id);
            // B1: close the tab's Redis Pub/Sub subscriptions.
            sink_m2.pubsub.forget_session(&info.id);
            // B1: stop the tab's background key digest reads.
            sink_m2.digests.forget_session(&info.id);
            // Stop the tab's bulk TTL jobs and memory analyses.
            sink_m2.bulk_ttl.forget_session(&info.id);
            sink_m2.memory_runs.forget_session(&info.id);
        }
        if let SessionEvent::Opened { info, .. } = &event {
            connect_host.emit_connect(
                &connect_ctx,
                &varve_extension::ConnectEvent {
                    connection_id: info.connection_id.clone(),
                    engine: info.engine,
                },
            );
        }
        bridge.handle(&event);
    });
    let sessions = Arc::new(SessionManager::new(drivers(), sink));
    let tunnels = Tunnels::new(dir.join("known_hosts"), device, Arc::clone(&recorder));
    // Every keychain read goes through the cache (design: one keychain
    // access per secret per app run, shared by all sessions of a profile).
    let secret_cache = Arc::new(SecretCache::new(
        secrets,
        settings::load_secrets(&store)?.remember_for_session,
    ));
    let services = Arc::new(Services {
        store,
        records,
        secrets: Arc::clone(&secret_cache) as Arc<dyn SecretStore>,
        secret_cache,
        sessions,
        tunnels,
        activity: recorder,
        catalogs,
        metadata: Arc::new(m2::metadata::MetadataPool::default()),
        m2: m2_state,
        keychain_cleanup: keychain_cleanup::BackgroundCleanup::default(),
    });
    services.migrate_shared_secret_versions();
    Ok(services)
}

/// Closes object browser metadata sessions that were not used for
/// [`m2::metadata::METADATA_IDLE`], checking once a minute.
fn spawn_metadata_reaper(services: Arc<Services>) {
    tauri::async_runtime::spawn(async move {
        let mut minute = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            minute.tick().await;
            let closed = services
                .close_idle_metadata(m2::metadata::METADATA_IDLE)
                .await;
            if closed > 0 {
                tracing::debug!(closed, "closed idle metadata sessions");
            }
        }
    });
}

/// Applies the stored retention period now and then once a day while the
/// app runs (design doc 9.5).
fn spawn_retention(services: Arc<Services>) {
    tauri::async_runtime::spawn(async move {
        let mut daily = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
        loop {
            daily.tick().await;
            if let Err(error) = services.apply_stored_retention() {
                tracing::warn!(%error, "could not apply the activity retention period");
            }
        }
    });
}

/// The secret store selected by [`SECRETS_ENV`].
#[must_use]
pub fn secret_store() -> Arc<dyn SecretStore> {
    if std::env::var(SECRETS_ENV).is_ok_and(|v| v == "memory") {
        tracing::warn!("secrets are kept in memory only ({SECRETS_ENV}=memory)");
        Arc::new(MemoryStore::default())
    } else {
        Arc::new(KeychainStore)
    }
}

/// Starts the app.
///
/// # Errors
/// Returns an error when an extension fails to register, `app.db` cannot be
/// opened or migrated, or Tauri fails.
pub fn run(
    context: tauri::Context<tauri::Wry>,
    edition: Edition,
    extensions: Vec<Box<dyn Extension>>,
) -> Result<(), AppError> {
    init_tracing();
    let host = build_host(&extensions)?;
    tracing::info!(edition = ?edition, extensions = host.extensions().len(), "starting Varve");

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init());
    // Windows and Linux update through the Tauri updater when the config
    // has a feed; macOS uses Sparkle (updates.rs).
    #[cfg(not(target_os = "macos"))]
    let builder = match updates::plugin(context.config()) {
        Some(plugin) => builder.plugin(plugin),
        None => builder,
    };
    builder
        .setup(move |app| {
            // The quit gate first: nothing later in setup may leave the app
            // without it.
            app.manage(quit::QuitGate::default());
            app.manage(updates::UpdateListener::default());
            // Before the menu: on macOS it offers "Check for Updates…" only
            // when Sparkle runs.
            updates::init(app.handle());
            #[cfg(target_os = "macos")]
            app.set_menu(quit::app_menu(app.handle())?)?;
            let dir = data_dir(app.handle())?;
            let store = Arc::new(open_store(&dir, &host)?);
            tracing::info!(path = %dir.display(), "app.db ready");
            let services =
                build_services(Arc::clone(&store), &dir, &host, edition, secret_store())?;
            if let Err(error) =
                varve_activity::ActivityEvent::app_started(env!("CARGO_PKG_VERSION"))
                    .build()
                    .map_err(|e| e.to_string())
                    .and_then(|event| services.activity.record(event).map_err(|e| e.to_string()))
            {
                tracing::warn!(%error, "could not record app.started");
            }
            spawn_retention(Arc::clone(&services));
            spawn_metadata_reaper(Arc::clone(&services));
            // Off the main thread: a keychain prompt must not hold up the
            // window.
            services.retry_keychain_cleanup_in_background();
            app.manage(AppState {
                ctx: AppContext { store, edition },
                host: host.clone(),
            });
            app.manage(services);
            app.manage(DataDir(dir));
            let transparent = app
                .config()
                .app
                .windows
                .iter()
                .any(|window| window.label == "main" && window.transparent);
            let window_glass = app
                .get_webview_window("main")
                .map_or(glass::Glass::None, |window| {
                    glass::apply(&window, transparent)
                });
            app.manage(glass::WindowGlass(window_glass));
            Ok(())
        })
        .on_page_load(|webview, payload| {
            if payload.event() != tauri::webview::PageLoadEvent::Started
                || webview.label() != "main"
            {
                return;
            }
            // A (re)loading page has no quit handler yet, and does not
            // listen for Sparkle's relaunch question yet.
            if let Some(gate) = webview.try_state::<quit::QuitGate>() {
                gate.reset_ui();
            }
            if let Some(listener) = webview.try_state::<updates::UpdateListener>() {
                listener.set(false);
            }
            // Its tabs are gone: a new page begins (sessions opened from now
            // on are its own) and the old page's sessions are closed. The new
            // page reads what was closed with `reload_report`.
            if let Some(services) = webview.try_state::<Arc<Services>>() {
                let services = Arc::clone(&services);
                let unloaded = services.begin_page();
                if !unloaded.is_empty() {
                    tauri::async_runtime::spawn(async move {
                        let closed = services.close_unloaded_page(unloaded).await;
                        tracing::info!(closed, "closed the sessions of the previous page");
                    });
                }
            }
        })
        .invoke_handler(ipc::handler())
        .build(context)?
        .run(|app, event| quit::on_run_event(app, &event));
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("VARVE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // A second call (for example in tests) keeps the first subscriber.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn community_host_is_empty_and_store_migrates() -> Result<(), Box<dyn std::error::Error>> {
        let host = build_host(&[])?;
        assert!(host.extensions().is_empty());
        let dir = std::env::temp_dir().join(format!("varve-app-test-{}", std::process::id()));
        let store = open_store(&dir, &host)?;
        let tables: i64 = store.with_conn(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'core_meta'",
                [],
                |r| r.get(0),
            )?)
        })?;
        assert_eq!(tables, 1);
        drop(store);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
