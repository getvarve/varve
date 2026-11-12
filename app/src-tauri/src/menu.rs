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

/// Query: the actions of the query tab in front.
const QUERY: &[Option<UiItem>] = &[
    Some(item("query-run", "Run", Some("CmdOrCtrl+Enter"))),
    Some(item(
        "query-cancel",
        "Cancel Statement",
        Some("CmdOrCtrl+."),
    )),
    None,
    Some(item("query-save", "Save", Some("CmdOrCtrl+S"))),
    Some(item("query-save-as", "Save As…", Some("CmdOrCtrl+Shift+S"))),
    None,
    Some(item("query-history", "Show or Hide History", None)),
];

/// Connection: the connection of the tab in front, or one the UI asks for.
const CONNECTION: &[Option<UiItem>] = &[
    Some(item("connection-overview", "Open Overview", None)),
    Some(item("connection-edit", "Edit Connection…", None)),
    Some(item("connection-duplicate", "Duplicate Connection", None)),
    None,
    Some(item(
        "tab-reconnect",
        "Connect Tab Again",
        Some("CmdOrCtrl+Shift+R"),
    )),
];

/// Help: pages on the web. `(key, title, address)`; the backend opens them in the browser.
const HELP: &[(&str, &str, &str)] = &[
    ("website", "Varve Website", "https://getvarve.com"),
    (
        "docs",
        "Documentation",
        "https://github.com/getvarve/varve#readme",
    ),
    (
        "releases",
        "Release Notes",
        "https://github.com/getvarve/varve/releases",
    ),
    (
        "issue",
        "Report a Problem…",
        "https://github.com/getvarve/varve/issues/new",
    ),
];

/// Start of the id of a Help item that opens a web page.
pub const OPEN_PREFIX: &str = "varve-open:";

/// The web address a Help item id stands for.
#[must_use]
pub fn help_address(id: &str) -> Option<&'static str> {
    let key = id.strip_prefix(OPEN_PREFIX)?;
    HELP.iter()
        .find(|(name, _, _)| *name == key)
        .map(|(_, _, address)| *address)
}

/// Opens a web address in the user's browser. Only the menu bar calls this, and macOS is the
/// only system with a menu bar, so it uses the system's own `open`.
pub fn open_in_browser(address: &str) {
    #[cfg(target_os = "macos")]
    if let Err(error) = std::process::Command::new("/usr/bin/open")
        .arg(address)
        .spawn()
    {
        tracing::warn!(%error, address, "could not open the page");
    }
    #[cfg(not(target_os = "macos"))]
    tracing::debug!(
        address,
        "no menu bar on this system, so nothing asks to open a page"
    );
}

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

/// Turns menu items on or off as the window says: `enabled` holds, per action, whether it
/// can be carried out now. An action that is not listed keeps its state. The UI calls this
/// whenever the tab in front, the page or the connections change.
#[tauri::command]
pub(crate) fn set_menu_enabled(
    app: AppHandle,
    enabled: std::collections::HashMap<String, bool>,
) -> bool {
    bar::set_enabled(&app, &enabled)
}

#[cfg(not(target_os = "macos"))]
mod bar {
    /// No menu bar on this system.
    pub(super) fn set_enabled(
        _app: &tauri::AppHandle,
        _enabled: &std::collections::HashMap<String, bool>,
    ) -> bool {
        false
    }
}

#[cfg(target_os = "macos")]
mod bar {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use tauri::menu::{IsMenuItem, MenuItem, PredefinedMenuItem, Submenu};
    use tauri::{AppHandle, Manager, Runtime};

    use super::{
        CONNECTION, FILE, HELP, OPEN_PREFIX, QUERY, SETTINGS, TOOLS, UI_ACTION_PREFIX, UiItem, VIEW,
    };

    /// The menu items the UI carries out, by action, so they can be turned on and off. An
    /// action can sit in two menus (Activity is in View and in Tools).
    pub struct Items<R: Runtime>(Mutex<HashMap<&'static str, Vec<MenuItem<R>>>>);

    impl<R: Runtime> Default for Items<R> {
        fn default() -> Self {
            Self(Mutex::new(HashMap::new()))
        }
    }

    pub(super) fn set_enabled(app: &AppHandle, enabled: &HashMap<String, bool>) -> bool {
        let Some(items) = app.try_state::<Items<tauri::Wry>>() else {
            return false;
        };
        let items = items
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (action, on) in enabled {
            for item in items.get(action.as_str()).into_iter().flatten() {
                if let Err(error) = item.set_enabled(*on) {
                    tracing::warn!(%error, action, "could not change a menu item");
                }
            }
        }
        true
    }

    fn menu_item<R: Runtime>(app: &AppHandle<R>, item: &UiItem) -> tauri::Result<MenuItem<R>> {
        let made = MenuItem::with_id(
            app,
            format!("{UI_ACTION_PREFIX}{}", item.action),
            item.title,
            true,
            item.shortcut,
        )?;
        if app.try_state::<Items<R>>().is_none() {
            app.manage(Items::<R>::default());
        }
        if let Some(items) = app.try_state::<Items<R>>() {
            items
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(item.action)
                .or_default()
                .push(made.clone());
        }
        Ok(made)
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

    /// The Query menu.
    pub fn query<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Submenu<R>> {
        submenu(app, "Query", QUERY, &[])
    }

    /// The Connection menu.
    pub fn connection<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Submenu<R>> {
        submenu(app, "Connection", CONNECTION, &[])
    }

    /// The Help menu: pages on the web.
    pub fn help<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Submenu<R>> {
        let menu = Submenu::new(app, "Help", true)?;
        for (key, title, _) in HELP {
            menu.append(&MenuItem::with_id(
                app,
                format!("{OPEN_PREFIX}{key}"),
                *title,
                true,
                None::<&str>,
            )?)?;
        }
        Ok(menu)
    }
}

#[cfg(target_os = "macos")]
pub use bar::{connection, file, help, query, settings, tools, view};

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        CONNECTION, FILE, HELP, QUERY, SETTINGS, TOOLS, UI_ACTION_PREFIX, UiItem, VIEW,
        help_address, ui_action,
    };

    fn all() -> Vec<&'static UiItem> {
        [FILE, VIEW, QUERY, CONNECTION, TOOLS]
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
    fn help_items_open_their_own_https_pages() {
        for (key, _, address) in HELP {
            assert!(address.starts_with("https://"), "{address}");
            assert_eq!(help_address(&format!("varve-open:{key}")), Some(*address));
        }
        assert_eq!(help_address("varve-open:elsewhere"), None);
        assert_eq!(help_address("varve-do:website"), None);
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
