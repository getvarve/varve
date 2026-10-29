//! In-app updates.
//!
//! - macOS uses Sparkle (`crates/sparkle`): its own windows, its "Check for
//!   Updates…" menu item and its scheduled checks. Sparkle asks before it
//!   relaunches; the UI runs the unsaved-work check and continues with
//!   `update_relaunch`. Declined, the relaunch stays held
//!   (`relaunchPending`), and the UI offers it again; it also installs when
//!   the app quits.
//! - Windows and Linux use the Tauri updater with the release's
//!   `latest.json`. The UI runs the flow: `update_check`, `update_download`
//!   (with `varve://update-progress` events), then, after the unsaved-work
//!   check, `update_install`, which installs and restarts.
//! - A build without an update feed (a dev build, an app built on varve-app
//!   that ships no feed) reports `none`, and the UI hides everything.
//! - So does a `.deb`, `.rpm` or `.msi` install: it updates by installing the
//!   new package. The feed's msi and deb entries serve only installs from
//!   0.1.7 and older (scripts/update-feeds.mjs).
//!
//! Every restart for an update goes through the quit gate's cleanup: the
//! cached secrets are wiped before the process goes away.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Runtime};

use crate::commands::CmdError;

/// Event with an [`UpdateProgress`] while `update_download` runs.
pub const UPDATE_PROGRESS: &str = "varve://update-progress";

/// Event sent when Sparkle wants to relaunch for an update; the UI answers
/// with `update_relaunch`.
pub const UPDATE_RELAUNCH_REQUESTED: &str = "varve://update-relaunch-requested";

/// Menu item id of "Check for Updates…" in the macOS app menu.
pub const CHECK_FOR_UPDATES_MENU_ID: &str = "varve-check-for-updates";

/// How this build updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mechanism {
    /// macOS: Sparkle owns the windows and the checks.
    Sparkle,
    /// Windows and Linux: the UI runs the flow over the Tauri updater.
    Tauri,
    /// No update feed in this build, or a package install (deb, rpm, msi).
    None,
}

/// Whether the Tauri updater may update a build packaged as `bundle`.
///
/// A package manager install (deb, rpm, msi) updates by installing the new
/// package; the feed's msi and deb entries serve only installs from 0.1.7 and
/// older. An unknown bundle (`cargo run`, `tauri dev`) keeps the updater.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn updates_in_app(bundle: Option<tauri::utils::config::BundleType>) -> bool {
    use tauri::utils::config::BundleType;
    !matches!(
        bundle,
        Some(BundleType::Deb | BundleType::Rpm | BundleType::Msi)
    )
}

/// What the UI needs to show the update settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// How this build updates.
    pub mechanism: Mechanism,
    /// The running version.
    pub current_version: String,
    /// Whether updates are checked for automatically.
    pub auto_check: bool,
    /// Sparkle holds a downloaded update's relaunch back for the UI.
    pub relaunch_pending: bool,
}

/// The answer of `update_check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdateCheck {
    /// Sparkle opened its own window.
    Sparkle,
    /// No newer version.
    UpToDate,
    /// A newer version, with its release notes (Markdown) and date
    /// (RFC 3339).
    Available {
        /// The new version.
        version: String,
        /// Its release notes, Markdown.
        notes: Option<String>,
        /// Its release date, RFC 3339.
        date: Option<String>,
    },
}

/// Bytes downloaded so far; `total` is unknown when the server sends no
/// length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProgress {
    /// Bytes downloaded so far.
    pub downloaded: u64,
    /// The download size, when the server sends it.
    pub total: Option<u64>,
}

/// Wipes the cached secrets and lets the quit gate through, so the restart
/// that follows leaves nothing behind and is not held back.
fn prepare_restart<R: Runtime>(app: &AppHandle<R>) {
    use tauri::Manager;
    if let Some(gate) = app.try_state::<crate::quit::QuitGate>() {
        gate.confirm();
    }
    if let Some(services) = app.try_state::<std::sync::Arc<crate::commands::Services>>() {
        services.secret_cache.forget_all();
        varve_driver::tls_key::forget_cached_keys();
    }
}

/// Whether the page's UI listens for `varve://update-relaunch-requested`; a
/// page load resets it, like the quit gate's ready flag.
#[derive(Debug, Default)]
pub struct UpdateListener(std::sync::atomic::AtomicBool);

impl UpdateListener {
    /// True once the current page listens.
    #[must_use]
    pub fn is_listening(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The page listens (true), or a new page starts loading (false).
    pub fn set(&self, listening: bool) {
        self.0.store(listening, std::sync::atomic::Ordering::SeqCst);
    }
}

fn not_available(what: &str) -> CmdError {
    CmdError::Internal {
        message: format!("{what} is not available in this build"),
    }
}

fn internal(message: impl std::fmt::Display) -> CmdError {
    CmdError::Internal {
        message: message.to_string(),
    }
}

#[cfg(target_os = "macos")]
pub use mac::{handle_menu_event, init, menu_item_enabled};

#[cfg(not(target_os = "macos"))]
pub use tauri_updater::{init, plugin};

#[cfg(target_os = "macos")]
mod mac {
    use tauri::{AppHandle, Emitter, Manager, Runtime};

    use super::{
        Mechanism, UPDATE_RELAUNCH_REQUESTED, UpdateCheck, UpdateInfo, internal, not_available,
    };
    use crate::commands::CmdError;

    /// The appcast for this build's architecture.
    const FEED_URL: &str = if cfg!(target_arch = "aarch64") {
        "https://github.com/getvarve/varve/releases/latest/download/appcast-arm64.xml"
    } else {
        "https://github.com/getvarve/varve/releases/latest/download/appcast-x64.xml"
    };

    /// Starts Sparkle when the bundle carries it. Main thread, in setup,
    /// before the menu is built (the menu shows "Check for Updates…" only
    /// when Sparkle runs).
    pub fn init<R: Runtime>(app: &AppHandle<R>) {
        let asker = app.clone();
        let cleaner = app.clone();
        varve_sparkle::start(
            FEED_URL,
            Box::new(move || {
                // Only a page that listens for the question can answer it.
                let listening = asker
                    .try_state::<super::UpdateListener>()
                    .is_some_and(|listener| listener.is_listening());
                listening
                    && asker
                        .get_webview_window("main")
                        .is_some_and(|window| window.emit(UPDATE_RELAUNCH_REQUESTED, ()).is_ok())
            }),
            // Sparkle quits past the quit gate: wipe the secrets either way.
            Box::new(move || super::prepare_restart(&cleaner)),
        );
    }

    /// True when the app menu should offer "Check for Updates…".
    pub fn menu_item_enabled() -> bool {
        varve_sparkle::is_running()
    }

    /// Handles "Check for Updates…" from the app menu (main thread).
    pub fn handle_menu_event<R: Runtime>(app: &AppHandle<R>, id: &str) {
        if id == super::CHECK_FOR_UPDATES_MENU_ID {
            check_or_restart(app);
        }
    }

    /// Opens Sparkle's window, or, while a downloaded update's relaunch is
    /// held, asks the page to continue it: Sparkle's own window would resume
    /// the install and relaunch without the unsaved-work check. Main thread.
    fn check_or_restart<R: Runtime>(app: &AppHandle<R>) -> bool {
        if varve_sparkle::relaunch_pending() {
            let listening = app
                .try_state::<super::UpdateListener>()
                .is_some_and(|listener| listener.is_listening());
            if listening
                && let Some(window) = app.get_webview_window("main")
                && window.emit(UPDATE_RELAUNCH_REQUESTED, ()).is_ok()
            {
                return true;
            }
        }
        varve_sparkle::check_for_updates()
    }

    /// Runs `f` on the main thread, where Sparkle lives, for a command.
    async fn on_main<R: Runtime, T: Send + 'static>(
        app: &AppHandle<R>,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, CmdError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.run_on_main_thread(move || {
            let _ = tx.send(f());
        })
        .map_err(internal)?;
        rx.await.map_err(internal)
    }

    pub(super) async fn info<R: Runtime>(app: &AppHandle<R>) -> Result<UpdateInfo, CmdError> {
        let current_version = app.package_info().version.to_string();
        let (auto_check, relaunch_pending) = on_main(app, || {
            (
                varve_sparkle::automatically_checks(),
                varve_sparkle::relaunch_pending(),
            )
        })
        .await?;
        Ok(match auto_check {
            Some(auto_check) => UpdateInfo {
                mechanism: Mechanism::Sparkle,
                current_version,
                auto_check,
                relaunch_pending,
            },
            None => UpdateInfo {
                mechanism: Mechanism::None,
                current_version,
                auto_check: false,
                relaunch_pending: false,
            },
        })
    }

    pub(super) async fn check<R: Runtime>(app: &AppHandle<R>) -> Result<UpdateCheck, CmdError> {
        let handle = app.clone();
        if on_main(app, move || check_or_restart(&handle)).await? {
            Ok(UpdateCheck::Sparkle)
        } else {
            Err(not_available("Checking for updates"))
        }
    }

    pub(super) async fn set_auto_check<R: Runtime>(
        app: &AppHandle<R>,
        auto_check: bool,
    ) -> Result<(), CmdError> {
        if on_main(app, move || {
            varve_sparkle::set_automatically_checks(auto_check)
        })
        .await?
        {
            Ok(())
        } else {
            Err(not_available("Automatic update checks"))
        }
    }

    pub(super) async fn relaunch<R: Runtime>(
        app: &AppHandle<R>,
        proceed: bool,
    ) -> Result<(), CmdError> {
        // Declined: leave the relaunch held (see crates/sparkle).
        if !proceed {
            return Ok(());
        }
        let handle = app.clone();
        let continued = on_main(app, move || {
            // Sparkle terminates the app next: let the quit gate through.
            varve_sparkle::continue_relaunch(|| super::prepare_restart(&handle))
        })
        .await?;
        if continued {
            Ok(())
        } else {
            Err(internal("No update is waiting to relaunch"))
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod tauri_updater {
    use std::sync::Mutex;

    use tauri::{AppHandle, Emitter, Manager, Runtime};
    use tauri_plugin_updater::{Update, UpdaterExt};
    use time::format_description::well_known::Rfc3339;

    use super::{
        Mechanism, UPDATE_PROGRESS, UpdateCheck, UpdateInfo, UpdateProgress, internal,
        not_available, updates_in_app,
    };
    use crate::commands::CmdError;

    const AUTO_CHECK_KEY: &str = "updates.auto_check";

    /// The update found by the last check and, once downloaded, its bytes.
    #[derive(Default)]
    pub struct Pending {
        update: Option<Update>,
        bytes: Option<Vec<u8>>,
    }

    /// Updater state, managed only when the plugin is registered.
    #[derive(Default)]
    pub struct UpdaterState(Mutex<Pending>);

    impl UpdaterState {
        fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    /// Whether this build updates in app: the config carries an updater feed
    /// (`plugins.updater` in tauri.conf.json) and the app is not a package
    /// install. Otherwise the app has no updates instead of failing to start.
    fn enabled(config: &tauri::Config) -> bool {
        config.plugins.0.contains_key("updater")
            && updates_in_app(tauri::utils::platform::bundle_type())
    }

    /// The updater plugin, when this build updates in app.
    pub fn plugin<R: Runtime>(
        config: &tauri::Config,
    ) -> Option<tauri::plugin::TauriPlugin<R, tauri_plugin_updater::Config>> {
        enabled(config).then(|| tauri_plugin_updater::Builder::new().build())
    }

    /// Manages the updater state when the plugin is registered.
    pub fn init<R: Runtime>(app: &AppHandle<R>) {
        if enabled(app.config()) {
            app.manage(UpdaterState::default());
        }
    }

    fn state<R: Runtime>(app: &AppHandle<R>) -> Result<tauri::State<'_, UpdaterState>, CmdError> {
        app.try_state::<UpdaterState>()
            .ok_or_else(|| not_available("Updating"))
    }

    fn auto_check<R: Runtime>(app: &AppHandle<R>) -> bool {
        let Some(app_state) = app.try_state::<crate::AppState>() else {
            return true;
        };
        // On unless the user turned it off.
        crate::meta_get(&app_state.ctx.store, AUTO_CHECK_KEY)
            .ok()
            .flatten()
            .is_none_or(|value| value != "false")
    }

    pub(super) async fn info<R: Runtime>(app: &AppHandle<R>) -> Result<UpdateInfo, CmdError> {
        let version = app.package_info().version.to_string();
        Ok(if app.try_state::<UpdaterState>().is_some() {
            UpdateInfo {
                mechanism: Mechanism::Tauri,
                current_version: version,
                auto_check: auto_check(app),
                relaunch_pending: false,
            }
        } else {
            UpdateInfo {
                mechanism: Mechanism::None,
                current_version: version,
                auto_check: false,
                relaunch_pending: false,
            }
        })
    }

    pub(super) async fn check<R: Runtime>(app: &AppHandle<R>) -> Result<UpdateCheck, CmdError> {
        let state = state(app)?;
        let update = app
            .updater()
            .map_err(internal)?
            .check()
            .await
            .map_err(internal)?;
        let answer = match &update {
            Some(update) => UpdateCheck::Available {
                version: update.version.clone(),
                notes: update.body.clone(),
                date: update.date.and_then(|date| date.format(&Rfc3339).ok()),
            },
            None => UpdateCheck::UpToDate,
        };
        let mut pending = state.lock();
        pending.update = update;
        pending.bytes = None;
        Ok(answer)
    }

    pub(super) async fn download<R: Runtime>(app: &AppHandle<R>) -> Result<(), CmdError> {
        let update = state(app)?
            .lock()
            .update
            .clone()
            .ok_or_else(|| internal("Check for updates first"))?;
        let mut downloaded: u64 = 0;
        let mut last_sent: Option<std::time::Instant> = None;
        let emitter = app.clone();
        let bytes = update
            .download(
                move |chunk, total| {
                    downloaded += chunk as u64;
                    // A few events a second are enough for a progress bar; one
                    // per chunk would flood the IPC channel.
                    let done = total.is_some_and(|total| downloaded >= total);
                    let due = last_sent.is_none_or(|at| at.elapsed().as_millis() >= 250);
                    if done || due {
                        last_sent = Some(std::time::Instant::now());
                        let _ = emitter.emit(UPDATE_PROGRESS, UpdateProgress { downloaded, total });
                    }
                },
                || {},
            )
            .await
            .map_err(internal)?;
        let state = state(app)?;
        let mut pending = state.lock();
        // A check that ran meanwhile may have found another version.
        if pending.update.as_ref().map(|u| &u.version) == Some(&update.version) {
            pending.bytes = Some(bytes);
        }
        Ok(())
    }

    pub(super) async fn install<R: Runtime>(app: &AppHandle<R>) -> Result<(), CmdError> {
        let (update, bytes) = {
            let state = state(app)?;
            let mut pending = state.lock();
            match (pending.update.clone(), pending.bytes.take()) {
                (Some(update), Some(bytes)) => (update, bytes),
                _ => return Err(internal("Download the update first")),
            }
        };
        // On Windows the installer takes over and the app exits inside
        // `install`, so the secrets go and the gate opens before it. Elsewhere
        // only an install that worked restarts the app.
        #[cfg(windows)]
        super::prepare_restart(app);
        if let Err(error) = update.install(bytes) {
            #[cfg(windows)]
            if let Some(gate) = app.try_state::<crate::quit::QuitGate>() {
                // Still running: quitting asks about unsaved work again.
                gate.unconfirm();
            }
            return Err(internal(error));
        }
        #[cfg(not(windows))]
        super::prepare_restart(app);
        app.restart();
    }

    pub(super) async fn set_auto_check<R: Runtime>(
        app: &AppHandle<R>,
        auto_check: bool,
    ) -> Result<(), CmdError> {
        state(app)?;
        let app_state = app
            .try_state::<crate::AppState>()
            .ok_or_else(|| not_available("Automatic update checks"))?;
        crate::meta_set(
            &app_state.ctx.store,
            AUTO_CHECK_KEY,
            if auto_check { "true" } else { "false" },
        )
        .map_err(internal)
    }

    pub(super) async fn relaunch<R: Runtime>(
        _app: &AppHandle<R>,
        _proceed: bool,
    ) -> Result<(), CmdError> {
        Err(not_available("Relaunching through Sparkle"))
    }
}

#[cfg(target_os = "macos")]
use mac as platform;
#[cfg(not(target_os = "macos"))]
use tauri_updater as platform;

/// How this build updates, its version and the auto-check setting.
#[tauri::command]
pub async fn update_info<R: Runtime>(app: AppHandle<R>) -> Result<UpdateInfo, CmdError> {
    platform::info(&app).await
}

/// Checks for an update (Tauri updater) or opens Sparkle's window.
#[tauri::command]
pub async fn update_check<R: Runtime>(app: AppHandle<R>) -> Result<UpdateCheck, CmdError> {
    platform::check(&app).await
}

/// Tauri updater: downloads the update the last check found.
#[tauri::command]
pub async fn update_download<R: Runtime>(app: AppHandle<R>) -> Result<(), CmdError> {
    #[cfg(not(target_os = "macos"))]
    return platform::download(&app).await;
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        Err(not_available("Downloading through the app"))
    }
}

/// Tauri updater: installs the downloaded update and restarts. The UI calls
/// it only after the unsaved-work check.
#[tauri::command]
pub async fn update_install<R: Runtime>(app: AppHandle<R>) -> Result<(), CmdError> {
    #[cfg(not(target_os = "macos"))]
    return platform::install(&app).await;
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        Err(not_available("Installing through the app"))
    }
}

/// Turns automatic update checks on or off.
#[tauri::command]
pub async fn set_update_auto_check<R: Runtime>(
    app: AppHandle<R>,
    auto_check: bool,
) -> Result<(), CmdError> {
    platform::set_auto_check(&app, auto_check).await
}

/// The page listens for `varve://update-relaunch-requested` from now on.
#[tauri::command]
pub fn update_listening(listener: tauri::State<'_, UpdateListener>) {
    listener.set(true);
}

/// Sparkle: continues (after the unsaved-work check) or declines a held-back
/// relaunch; declined, it stays held.
#[tauri::command]
pub async fn update_relaunch<R: Runtime>(app: AppHandle<R>, proceed: bool) -> Result<(), CmdError> {
    platform::relaunch(&app, proceed).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_installs_do_not_update_in_app() {
        use tauri::utils::config::BundleType;
        for bundle in [BundleType::Deb, BundleType::Rpm, BundleType::Msi] {
            assert!(!updates_in_app(Some(bundle.clone())), "{bundle:?}");
        }
        for bundle in [BundleType::AppImage, BundleType::Nsis, BundleType::App] {
            assert!(updates_in_app(Some(bundle.clone())), "{bundle:?}");
        }
        assert!(updates_in_app(None));
    }

    /// Every signature the feed hands out must name its version, or the
    /// updater rejects it (`requireSignedVersion`).
    #[test]
    fn the_updater_requires_signed_versions() -> Result<(), serde_json::Error> {
        let config: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))?;
        assert_eq!(
            config["plugins"]["updater"]["requireSignedVersion"],
            serde_json::Value::Bool(true)
        );
        Ok(())
    }

    #[test]
    fn the_ui_reads_the_update_shapes() -> Result<(), serde_json::Error> {
        assert_eq!(serde_json::to_string(&Mechanism::Sparkle)?, "\"sparkle\"");
        assert_eq!(
            serde_json::to_value(UpdateInfo {
                mechanism: Mechanism::Tauri,
                current_version: "0.1.6".into(),
                auto_check: true,
                relaunch_pending: false,
            })?,
            serde_json::json!({"mechanism": "tauri", "currentVersion": "0.1.6", "autoCheck": true, "relaunchPending": false})
        );
        assert_eq!(
            serde_json::to_value(UpdateCheck::Available {
                version: "0.1.7".into(),
                notes: None,
                date: Some("2026-10-28T12:00:00Z".into()),
            })?,
            serde_json::json!({"kind": "available", "version": "0.1.7", "notes": null, "date": "2026-10-28T12:00:00Z"})
        );
        assert_eq!(
            serde_json::to_value(UpdateCheck::UpToDate)?,
            serde_json::json!({"kind": "upToDate"})
        );
        assert_eq!(
            serde_json::to_value(UpdateCheck::Sparkle)?,
            serde_json::json!({"kind": "sparkle"})
        );
        assert_eq!(
            serde_json::to_value(UpdateProgress {
                downloaded: 10,
                total: None
            })?,
            serde_json::json!({"downloaded": 10, "total": null})
        );
        Ok(())
    }
}
