//! Quitting goes through the UI's unsaved-changes check.
//!
//! Exit requests (closing the main window, the macOS Quit menu item and its
//! Cmd+Q shortcut, `ExitRequested` from the run loop) are stopped
//! and turned into the `varve://quit-requested` event for the main window.
//! The UI asks about unsaved work and then calls `confirm_quit` (quit) or
//! `cancel_quit` (stay).
//!
//! The app must always stay quittable, so the gate only holds an exit back
//! while a UI is known to listen: the UI calls `quit_ready` once its handler
//! is installed (every page load resets this). When the UI starts asking the
//! user it calls `quit_acknowledged`; a request that was never acknowledged
//! within [`ANSWER_TIMEOUT`] means the UI is not responding, and the next
//! request exits directly. An acknowledged question waits for the user as
//! long as needed.
//!
//! Limit: on macOS, Quit from the Dock (and logging out) ends the app
//! through `applicationWillTerminate` without any event the gate can stop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager, RunEvent, Runtime, WindowEvent};

use crate::commands::Services;

/// Event sent to the main window when the user asked to quit.
pub const QUIT_REQUESTED: &str = "varve://quit-requested";

/// Menu item id of the app's own Quit item.
pub const QUIT_MENU_ID: &str = "varve-quit";

/// Menu item id of "Forget Cached Secrets" (drops the secret cache).
pub const FORGET_SECRETS_MENU_ID: &str = "varve-forget-secrets";

/// How long a quit request may stay unanswered before a repeated request
/// exits without asking.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// Quit state shared by the run loop and the commands.
#[derive(Debug, Default)]
pub struct QuitGate {
    confirmed: AtomicBool,
    ready: AtomicBool,
    pending: Mutex<Pending>,
}

/// The quit request the UI is handling.
#[derive(Debug, Default, Clone, Copy)]
struct Pending {
    since: Option<Instant>,
    acknowledged: bool,
}

/// What to do with a quit request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Let it through.
    Exit,
    /// Ask the UI.
    Ask,
    /// A request is already being answered: hold the exit back and remind
    /// the UI.
    Wait,
}

impl QuitGate {
    /// True once the UI confirmed.
    #[must_use]
    pub fn confirmed(&self) -> bool {
        self.confirmed.load(Ordering::SeqCst)
    }

    /// Marks the quit as confirmed.
    pub fn confirm(&self) {
        self.confirmed.store(true, Ordering::SeqCst);
    }

    /// Takes a confirmation back: a restart for an update did not happen
    /// after all, so quitting must ask again.
    pub fn unconfirm(&self) {
        self.confirmed.store(false, Ordering::SeqCst);
    }

    /// The UI installed its quit handler.
    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }

    /// True while a UI with a quit handler can answer questions.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// A page started loading: its quit handler is not installed yet, and
    /// any question the old page was asking is gone.
    pub fn reset_ui(&self) {
        self.ready.store(false, Ordering::SeqCst);
        *self.pending() = Pending::default();
    }

    /// The UI started asking the user. A question the UI started on its
    /// own (Ctrl+Q handled in the page) is recorded too, so a later request
    /// from the window waits for the same answer instead of timing out.
    pub fn acknowledge(&self, now: Instant) {
        let mut pending = self.pending();
        pending.since.get_or_insert(now);
        pending.acknowledged = true;
    }

    /// The UI answered "stay".
    pub fn cancel(&self) {
        *self.pending() = Pending::default();
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Decides about a quit request made at `now`.
    pub fn decide(&self, now: Instant) -> Decision {
        if self.confirmed() || !self.ready.load(Ordering::SeqCst) {
            return Decision::Exit;
        }
        let mut pending = self.pending();
        match pending.since {
            Some(since) if !pending.acknowledged && now.duration_since(since) >= ANSWER_TIMEOUT => {
                Decision::Exit
            }
            Some(_) => Decision::Wait,
            None => {
                *pending = Pending {
                    since: Some(now),
                    acknowledged: false,
                };
                Decision::Ask
            }
        }
    }
}

/// Handles a quit request from any source.
fn request_quit<R: Runtime>(app: &AppHandle<R>) {
    let decision = app
        .try_state::<QuitGate>()
        .map_or(Decision::Exit, |gate| gate.decide(Instant::now()));
    match decision {
        Decision::Exit => confirm(app),
        // Ask again: the UI joins its running question, or starts a new one
        // when the previous answer is still on its way (Cancel in flight).
        Decision::Wait | Decision::Ask => match app.get_webview_window("main") {
            Some(window) => {
                if let Err(error) = window.emit(QUIT_REQUESTED, ()) {
                    tracing::warn!(%error, "could not ask the UI about quitting");
                    confirm(app);
                }
            }
            None => confirm(app),
        },
    }
}

fn confirm<R: Runtime>(app: &AppHandle<R>) {
    if let Some(gate) = app.try_state::<QuitGate>() {
        gate.confirm();
    }
    // Cached secrets are wiped before the process goes away.
    if let Some(services) = app.try_state::<Arc<Services>>() {
        services.secret_cache.forget_all();
        varve_driver::tls_key::forget_cached_keys();
    }
    app.exit(0);
}

/// Handles the run loop events that would quit the app.
pub fn on_run_event<R: Runtime>(app: &AppHandle<R>, event: &RunEvent) {
    // Without a gate (setup failed) nothing may block quitting.
    let confirmed = app.try_state::<QuitGate>().is_none_or(|g| g.confirmed());
    match event {
        RunEvent::ExitRequested { api, .. } if !confirmed => {
            api.prevent_exit();
            request_quit(app);
        }
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::CloseRequested { api, .. },
            ..
        } if label == "main" && !confirmed => {
            api.prevent_close();
            request_quit(app);
        }
        RunEvent::MenuEvent(event) if event.id() == QUIT_MENU_ID => request_quit(app),
        #[cfg(target_os = "macos")]
        RunEvent::MenuEvent(event) if event.id() == crate::updates::CHECK_FOR_UPDATES_MENU_ID => {
            crate::updates::handle_menu_event(app, event.id().as_ref());
        }
        RunEvent::MenuEvent(event) if event.id() == FORGET_SECRETS_MENU_ID => {
            if let Some(services) = app.try_state::<Arc<Services>>() {
                let count = services.forget_cached_secrets();
                tracing::info!(count, "forgot the cached secrets from the menu");
            }
        }
        _ => {}
    }
}

/// The application menu on macOS, with Varve's own Quit item instead of the
/// predefined one (which terminates without an event the gate could stop).
///
/// # Errors
/// Menu construction errors.
#[cfg(target_os = "macos")]
pub fn app_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<tauri::menu::Menu<R>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
    let name = app.package_info().name.clone();
    let quit = MenuItem::with_id(
        app,
        QUIT_MENU_ID,
        format!("Quit {name}"),
        true,
        Some("CmdOrCtrl+Q"),
    )?;
    let forget_secrets = MenuItem::with_id(
        app,
        FORGET_SECRETS_MENU_ID,
        "Forget Cached Secrets",
        true,
        None::<&str>,
    )?;
    let about = PredefinedMenuItem::about(app, None, None)?;
    let check_for_updates = MenuItem::with_id(
        app,
        crate::updates::CHECK_FOR_UPDATES_MENU_ID,
        "Check for Updates…",
        true,
        None::<&str>,
    )?;
    let separators = [
        PredefinedMenuItem::separator(app)?,
        PredefinedMenuItem::separator(app)?,
        PredefinedMenuItem::separator(app)?,
        PredefinedMenuItem::separator(app)?,
    ];
    let services = PredefinedMenuItem::services(app, None)?;
    let hide = PredefinedMenuItem::hide(app, None)?;
    let hide_others = PredefinedMenuItem::hide_others(app, None)?;
    let show_all = PredefinedMenuItem::show_all(app, None)?;
    let mut items: Vec<&dyn tauri::menu::IsMenuItem<R>> = vec![&about];
    // Only when Sparkle runs: a dev build has no updater behind the item.
    if crate::updates::menu_item_enabled() {
        items.push(&check_for_updates);
    }
    items.extend([
        &separators[0] as &dyn tauri::menu::IsMenuItem<R>,
        &forget_secrets,
        &separators[1],
        &services,
        &separators[2],
        &hide,
        &hide_others,
        &show_all,
        &separators[3],
        &quit,
    ]);
    let app_menu = Submenu::with_items(app, &name, true, &items)?;
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;
    let window = Submenu::with_items(
        app,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            &PredefinedMenuItem::close_window(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::fullscreen(app, None)?,
        ],
    )?;
    Menu::with_items(app, &[&app_menu, &edit, &window])
}

/// The UI installed its quit handler; from now on quit requests ask it.
#[tauri::command]
pub fn quit_ready(gate: tauri::State<'_, QuitGate>) {
    gate.set_ready();
}

/// The UI checked for unsaved work and the user chose to quit.
#[tauri::command]
pub fn confirm_quit(app: AppHandle) {
    confirm(&app);
}

/// The UI started asking the user; the request now waits for the answer
/// however long it takes.
#[tauri::command]
pub fn quit_acknowledged(gate: tauri::State<'_, QuitGate>) {
    gate.acknowledge(Instant::now());
}

/// The user chose to stay; the next quit request asks again.
#[tauri::command]
pub fn cancel_quit(gate: tauri::State<'_, QuitGate>) {
    gate.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exits_directly_until_the_ui_is_ready() {
        let gate = QuitGate::default();
        assert_eq!(gate.decide(Instant::now()), Decision::Exit);
    }

    #[test]
    fn asks_once_and_waits_for_the_answer() {
        let gate = QuitGate::default();
        gate.set_ready();
        let start = Instant::now();
        assert_eq!(gate.decide(start), Decision::Ask);
        assert_eq!(gate.decide(start + Duration::from_secs(1)), Decision::Wait);
        gate.cancel();
        assert_eq!(gate.decide(start + Duration::from_secs(2)), Decision::Ask);
    }

    #[test]
    fn an_unanswered_request_lets_a_later_one_through() {
        let gate = QuitGate::default();
        gate.set_ready();
        let start = Instant::now();
        assert_eq!(gate.decide(start), Decision::Ask);
        assert_eq!(gate.decide(start + ANSWER_TIMEOUT), Decision::Exit);
    }

    #[test]
    fn an_acknowledged_question_waits_for_the_user() {
        let gate = QuitGate::default();
        gate.set_ready();
        let start = Instant::now();
        assert_eq!(gate.decide(start), Decision::Ask);
        gate.acknowledge(start);
        assert_eq!(gate.decide(start + ANSWER_TIMEOUT * 6), Decision::Wait);
        gate.cancel();
        assert_eq!(gate.decide(start + ANSWER_TIMEOUT * 7), Decision::Ask);
    }

    #[test]
    fn a_page_load_resets_the_ui_state() {
        let gate = QuitGate::default();
        gate.set_ready();
        assert_eq!(gate.decide(Instant::now()), Decision::Ask);
        gate.acknowledge(Instant::now());
        gate.reset_ui();
        assert_eq!(gate.decide(Instant::now()), Decision::Exit);
    }

    #[test]
    fn a_question_started_by_the_ui_holds_later_requests() {
        let gate = QuitGate::default();
        gate.set_ready();
        let start = Instant::now();
        // Ctrl+Q in the page: the UI asks before the backend saw a request.
        gate.acknowledge(start);
        assert_eq!(gate.decide(start + ANSWER_TIMEOUT * 3), Decision::Wait);
        gate.cancel();
        assert_eq!(gate.decide(start + ANSWER_TIMEOUT * 4), Decision::Ask);
    }

    #[test]
    fn a_confirmed_quit_is_never_held() {
        let gate = QuitGate::default();
        gate.set_ready();
        gate.confirm();
        assert_eq!(gate.decide(Instant::now()), Decision::Exit);
    }
}
