//! Native glass behind the main window.
//!
//! The window and its webview are transparent (`tauri.conf.json`), and what
//! lies behind them depends on the system:
//!
//! - macOS 26: the window paints an opaque background and each shape the UI
//!   reports (the sidebar card) gets its own Liquid Glass view behind the
//!   webview (`varve-glass`), so only that shape is glass;
//! - older macOS: the window vibrancy material behind the whole window;
//! - Windows 11: Mica behind the whole window;
//! - Windows 10 and Linux: none. Windows 10 Acrylic is an untinted blur that
//!   lags while dragging, and Linux has no material.
//!
//! The UI reads which one applies from `app_info` and turns translucent only
//! where glass really is, so a window without glass keeps the opaque design
//! instead of showing the desktop through.

use serde::{Deserialize, Serialize};
use tauri::Manager;

/// The glass behind the main window, as the UI reads it from `app_info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Glass {
    /// macOS 26 and newer: an `NSGlassEffectView` behind each shape the UI
    /// reports with `set_glass_shapes`; the rest of the window is opaque.
    LiquidGlass,
    /// Older macOS: the window background vibrancy material.
    Vibrancy,
    /// Windows 11.
    Mica,
    /// No native glass: the UI stays opaque.
    None,
}

/// The glass applied to the main window, kept as Tauri state for `app_info`.
#[derive(Clone, Copy, Debug)]
pub struct WindowGlass(pub Glass);

/// Puts the native glass behind `window` and returns which one it is.
/// `transparent` is the window's `transparent` setting: glass behind an opaque
/// webview is invisible, and a UI that turned translucent over it would lose
/// its contrast, so an opaque window gets none.
/// Must run on the main thread, as Tauri's `setup` does.
pub fn apply(window: &tauri::WebviewWindow, transparent: bool) -> Glass {
    if !transparent {
        tracing::info!("window glass off: the window is not transparent");
        return Glass::None;
    }
    let glass = platform::apply(window);
    tracing::info!(?glass, "window glass");
    glass
}

/// One glass shape as the UI reports it: CSS pixels from the top-left corner
/// of the page.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlassShape {
    id: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    radius: f64,
}

/// Shows exactly these glass shapes behind the page: the UI calls this when
/// the sidebar card appears, moves, changes size or goes away. Returns whether
/// the shapes were handed to the system; false where the window has no Liquid
/// Glass, and the UI then keeps the card opaque.
#[tauri::command]
pub(crate) fn set_glass_shapes(
    window: tauri::WebviewWindow,
    window_glass: tauri::State<'_, WindowGlass>,
    shapes: Vec<GlassShape>,
) -> bool {
    if window_glass.0 != Glass::LiquidGlass || window.label() != "main" {
        return false;
    }
    let shapes: Vec<varve_glass_shape::Shape> = shapes
        .into_iter()
        .map(|shape| varve_glass_shape::Shape {
            id: shape.id,
            x: shape.x,
            y: shape.y,
            width: shape.width,
            height: shape.height,
            radius: shape.radius,
        })
        .collect();
    let target = window.clone();
    // The views live on the main thread; a command runs on a worker.
    window
        .app_handle()
        .run_on_main_thread(move || {
            varve_glass_shape::show(&target, &shapes);
        })
        .is_ok()
}

/// `varve-glass` on macOS; elsewhere the same names doing nothing, so the
/// command above is one piece of code for every platform.
#[cfg(target_os = "macos")]
use varve_glass as varve_glass_shape;

#[cfg(not(target_os = "macos"))]
mod varve_glass_shape {
    pub(super) struct Shape {
        pub id: String,
        pub x: f64,
        pub y: f64,
        pub width: f64,
        pub height: f64,
        pub radius: f64,
    }

    pub(super) fn show(_window: &tauri::WebviewWindow, _shapes: &[Shape]) -> bool {
        false
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::Glass;
    use window_vibrancy::{NSVisualEffectMaterial, NSVisualEffectState};

    pub(super) fn apply(window: &tauri::WebviewWindow) -> Glass {
        // macOS 26: glass only behind the shapes the UI reports, over a window that paints its
        // own background again (a clear window would show the desktop through the glass).
        if varve_glass::supported() {
            if varve_glass::opaque_background(window) {
                return Glass::LiquidGlass;
            }
            tracing::warn!("could not give the window a background for Liquid Glass");
        }
        // Before macOS 26: the classic material behind the whole window.
        match window_vibrancy::apply_vibrancy(
            window,
            NSVisualEffectMaterial::UnderWindowBackground,
            Some(NSVisualEffectState::FollowsWindowActiveState),
            None,
        ) {
            Ok(()) => Glass::Vibrancy,
            Err(error) => {
                tracing::warn!(?error, "could not apply window vibrancy");
                Glass::None
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::Glass;

    pub(super) fn apply(window: &tauri::WebviewWindow) -> Glass {
        // Mica needs Windows 11 and reports an unsupported version before it.
        match window_vibrancy::apply_mica(window, None) {
            Ok(()) => Glass::Mica,
            Err(error) => {
                tracing::info!(?error, "no Mica: the window stays opaque");
                Glass::None
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::Glass;

    pub(super) fn apply(_window: &tauri::WebviewWindow) -> Glass {
        Glass::None
    }
}

#[cfg(test)]
mod tests {
    use super::Glass;

    #[test]
    fn the_ui_reads_glass_in_camel_case() -> Result<(), serde_json::Error> {
        let names = [
            (Glass::LiquidGlass, "\"liquidGlass\""),
            (Glass::Vibrancy, "\"vibrancy\""),
            (Glass::Mica, "\"mica\""),
            (Glass::None, "\"none\""),
        ];
        for (glass, json) in names {
            assert_eq!(serde_json::to_string(&glass)?, json);
        }
        Ok(())
    }

    /// Linux has no glass, so `tauri.linux.conf.json` gives it an opaque
    /// window. Tauri replaces the whole windows array with it, so it must
    /// repeat the main window exactly, apart from `transparent`.
    #[test]
    fn the_linux_window_differs_only_in_transparency() -> Result<(), serde_json::Error> {
        let main_window = |config: &str| -> Result<serde_json::Value, serde_json::Error> {
            let config: serde_json::Value = serde_json::from_str(config)?;
            Ok(config["app"]["windows"][0].clone())
        };
        let mut base = main_window(include_str!("../tauri.conf.json"))?;
        let mut linux = main_window(include_str!("../tauri.linux.conf.json"))?;
        assert_eq!(base["transparent"], serde_json::Value::Bool(true));
        assert_eq!(linux["transparent"], serde_json::Value::Bool(false));
        for window in [&mut base, &mut linux] {
            if let Some(fields) = window.as_object_mut() {
                fields.remove("transparent");
            }
        }
        assert_eq!(base, linux);
        Ok(())
    }
}
