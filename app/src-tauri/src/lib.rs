//! The Varve desktop app.
//!
//! Varve Community and Varve Pro are the same app: each edition is a small
//! binary crate that calls [`run`] with its own Tauri context (product name,
//! bundle id, frontend) and its list of extensions. Community passes none.

pub mod sessions;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tauri::Manager;
use varve_core::Edition;
use varve_extension::{
    AppContext, CommandError, Extension, ExtensionError, ExtensionHost, ExtensionInfo, HostBuilder,
};
use varve_store::{MigrationSet, Store, StoreError, core_migrations};

/// Environment variable that overrides the data directory, for development
/// and tests.
pub const DATA_DIR_ENV: &str = "VARVE_DATA_DIR";

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
struct AppState {
    ctx: AppContext,
    host: ExtensionHost,
}

/// What the UI needs to know about the running app.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    edition: Edition,
    product_name: &'static str,
    version: &'static str,
    data_dir: String,
    extensions: Vec<ExtensionInfo>,
    capabilities: Vec<String>,
    commands: Vec<String>,
}

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

/// Directory that holds `app.db`.
struct DataDir(PathBuf);

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

    tauri::Builder::default()
        .setup(move |app| {
            let dir = data_dir(app.handle())?;
            let store = open_store(&dir, &host)?;
            tracing::info!(path = %dir.display(), "app.db ready");
            app.manage(AppState {
                ctx: AppContext {
                    store: Arc::new(store),
                    edition,
                },
                host: host.clone(),
            });
            app.manage(DataDir(dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![app_info, ext_invoke])
        .run(context)?;
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
