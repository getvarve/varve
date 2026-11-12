//! The actions of the macOS menu bar that the window carries out.
//!
//! A menu item whose id starts with [`UI_ACTION_PREFIX`] does nothing here:
//! the rest of its id is sent to the main window as a [`UI_ACTION_EVENT`]
//! event, and the UI runs the action with what it knows (the active tab, the
//! page, the connection to ask about). Items the backend handles itself (quit,
//! updates, cached secrets) keep their own ids in `quit.rs` and `updates.rs`.
//!
//! Windows and Linux show no menu bar: there the same actions are reached
//! from the window, and the shortcuts are the UI's own.

use tauri::{AppHandle, Emitter, Manager, Runtime};

/// Event the main window gets for a menu action, with the action's name as payload.
pub const UI_ACTION_EVENT: &str = "menu://action";

/// Start of the id of every menu item the UI carries out.
pub const UI_ACTION_PREFIX: &str = "varve-do:";

/// One menu item the UI carries out: the action's name, the title in the
/// menu and the shortcut, if it has one.
struct UiItem {
    action: &'static str,
    title: &'static str,
    shortcut: Option<&'static str>,
}

const fn item(action: &'static str, title: &'static str, shortcut: Option<&'static str>) -> UiItem {
    UiItem {
        action,
        title,
        shortcut,
    }
}

/// Settings sits in the app menu, where macOS users look for it.
const SETTINGS: UiItem = item("show-settings", "Settings…", Some("CmdOrCtrl+,"));

/// File: making things, and closing the tab in front.
const FILE: &[Option<UiItem>] = &[
    Some(item(
        "new-connection",
        "New Connection…",
        Some("CmdOrCtrl+N"),
    )),
    Some(item("new-folder", "New Folder", Some("CmdOrCtrl+Shift+N"))),
    Some(item("new-query", "New Query", Some("CmdOrCtrl+T"))),
    None,
    Some(item("close-tab", "Close Tab", Some("CmdOrCtrl+W"))),
];

/// View: the three pages and the sidebar.
const VIEW: &[Option<UiItem>] = &[
    Some(item("show-workspace", "Workspace", Some("CmdOrCtrl+1"))),
    Some(item("show-activity", "Activity", Some("CmdOrCtrl+2"))),
    None,
    Some(item(
        "toggle-sidebar",
        "Show or Hide Sidebar",
        Some("Ctrl+Cmd+S"),
    )),
    Some(item("fit-sidebar", "Fit Sidebar to Its Text", None)),
];

/// Tools: what there is to look at on a server. The UI asks which connection
/// when no tab is in front.
const TOOLS: &[Option<UiItem>] = &[
    Some(item("live:status", "Server Status", None)),
    Some(item("live:processes", "Process List", None)),
    Some(item("live:variables", "Server Variables", None)),
    None,
    Some(item(
        "show-activity",
        "Activity Log",
        Some("CmdOrCtrl+Shift+H"),
    )),
];

/// The action a menu item id stands for, when the UI carries it out.
#[must_use]
pub fn ui_action(id: &str) -> Option<&str> {
    id.strip_prefix(UI_ACTION_PREFIX)
        .filter(|action| !action.is_empty())
}

/// Sends a menu action to the main window. Returns whether a window got it.
pub fn send_ui_action<R: Runtime>(app: &AppHandle<R>, action: &str) -> bool {
    app.get_webview_window("main")
        .is_some_and(|window| window.emit(UI_ACTION_EVENT, action).is_ok())
}

#[cfg(target_os = "macos")]
mod bar {
    use tauri::menu::{IsMenuItem, MenuItem, PredefinedMenuItem, Submenu};
    use tauri::{AppHandle, Runtime};

    use super::{FILE, SETTINGS, TOOLS, UI_ACTION_PREFIX, UiItem, VIEW};

    fn menu_item<R: Runtime>(app: &AppHandle<R>, item: &UiItem) -> tauri::Result<MenuItem<R>> {
        MenuItem::with_id(
            app,
            format!("{UI_ACTION_PREFIX}{}", item.action),
            item.title,
            true,
            item.shortcut,
        )
    }

    /// A submenu of UI actions; `None` in the list is a separator. `last`
    /// items come after them, behind a separator.
    fn submenu<R: Runtime>(
        app: &AppHandle<R>,
        title: &str,
        items: &[Option<UiItem>],
        last: &[&dyn IsMenuItem<R>],
    ) -> tauri::Result<Submenu<R>> {
        let menu = Submenu::new(app, title, true)?;
        for item in items {
            match item {
                Some(item) => menu.append(&menu_item(app, item)?)?,
                None => menu.append(&PredefinedMenuItem::separator(app)?)?,
            }
        }
        if !last.is_empty() {
            menu.append(&PredefinedMenuItem::separator(app)?)?;
            for item in last {
                menu.append(*item)?;
            }
        }
        Ok(menu)
    }

    /// The Settings item of the app menu.
    pub fn settings<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<MenuItem<R>> {
        menu_item(app, &SETTINGS)
    }

    /// The File menu.
    pub fn file<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Submenu<R>> {
        submenu(app, "File", FILE, &[])
    }

    /// The View menu, ending with the system's full screen item.
    pub fn view<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Submenu<R>> {
        submenu(
            app,
            "View",
            VIEW,
            &[&PredefinedMenuItem::fullscreen(app, None)?],
        )
    }

    /// The Tools menu.
    pub fn tools<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Submenu<R>> {
        submenu(app, "Tools", TOOLS, &[])
    }
}

#[cfg(target_os = "macos")]
pub use bar::{file, settings, tools, view};

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{FILE, SETTINGS, TOOLS, UI_ACTION_PREFIX, UiItem, VIEW, ui_action};

    fn all() -> Vec<&'static UiItem> {
        [FILE, VIEW, TOOLS]
            .into_iter()
            .flatten()
            .flatten()
            .chain([&SETTINGS])
            .collect()
    }

    #[test]
    fn only_prefixed_ids_are_ui_actions() {
        assert_eq!(ui_action("varve-do:new-query"), Some("new-query"));
        assert_eq!(ui_action("varve-do:live:status"), Some("live:status"));
        assert_eq!(ui_action("varve-do:"), None);
        assert_eq!(ui_action("varve-quit"), None);
        assert_eq!(ui_action("new-query"), None);
        assert!(UI_ACTION_PREFIX.ends_with(':'));
    }

    #[test]
    fn no_two_items_share_a_shortcut() {
        let mut seen = HashSet::new();
        for item in all() {
            if let Some(shortcut) = item.shortcut {
                assert!(seen.insert(shortcut), "{shortcut} is used twice");
            }
        }
        // Quit keeps its own shortcut in quit.rs.
        assert!(!seen.contains("CmdOrCtrl+Q"));
    }

    #[test]
    fn the_ui_knows_every_action() {
        // The list the UI handles (packages/ui/src/composables/useAppMenu.ts, MENU_ACTIONS).
        let source = include_str!("../../../packages/ui/src/composables/useAppMenu.ts");
        for item in all() {
            assert!(
                source.contains(&format!("'{}'", item.action)),
                "the UI has no handler for {}",
                item.action
            );
        }
    }
}
