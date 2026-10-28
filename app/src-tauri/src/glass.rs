//! Native glass behind the transparent main window.
//!
//! The window and its webview are transparent (`tauri.conf.json`); this puts
//! the platform's own material behind them: Liquid Glass on macOS 26, the
//! window vibrancy material on older macOS and Mica on Windows 11. Windows 10
//! and Linux get none: Windows 10 Acrylic is an untinted blur that lags while
//! dragging, and Linux has no material. The UI turns its surfaces translucent
//! only for the glass that was actually applied, so a window without glass
//! keeps the opaque design instead of showing the desktop through.

use serde::Serialize;

/// The glass behind the main window, as the UI reads it from `app_info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Glass {
    /// macOS 26 and newer: an `NSGlassEffectView`.
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

#[cfg(target_os = "macos")]
mod platform {
    use super::Glass;
    use window_vibrancy::{
        Error, LiquidGlassOptions, NSGlassEffectViewStyle, NSVisualEffectMaterial,
        NSVisualEffectState,
    };

    pub(super) fn apply(window: &tauri::WebviewWindow) -> Glass {
        match window_vibrancy::apply_liquid_glass(
            window,
            LiquidGlassOptions::new(NSGlassEffectViewStyle::Regular),
        ) {
            Ok(()) => return Glass::LiquidGlass,
            // Before macOS 26: fall back to the classic material.
            Err(Error::UnsupportedPlatformVersion(_)) => {}
            Err(error) => {
                tracing::warn!(?error, "could not apply Liquid Glass");
                return Glass::None;
            }
        }
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
