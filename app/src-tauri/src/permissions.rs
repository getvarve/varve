//! The Tauri permissions the main window needs.
//!
//! Every app built on `varve-app` has its own `capabilities/default.json`. It
//! must grant at least these permissions, or parts of the UI stop working:
//! dragging the window by its title bar, the open and save dialogs, and
//! writing exports. Each app checks its capability file against this list in
//! a test.

/// Permissions the main window's capability file must grant.
pub const REQUIRED_PERMISSIONS: &[&str] = &[
    "core:default",
    "core:window:allow-start-dragging",
    "dialog:allow-open",
    "dialog:allow-save",
    "fs:allow-write-text-file",
    "fs:allow-write-file",
];

/// Returns the required permissions missing from a capability file's JSON.
///
/// # Errors
///
/// Returns an error when the JSON cannot be parsed or has no `permissions`
/// array of strings.
pub fn missing_permissions(capability_json: &str) -> Result<Vec<&'static str>, String> {
    let value: serde_json::Value =
        serde_json::from_str(capability_json).map_err(|e| format!("invalid JSON: {e}"))?;
    let granted: Vec<&str> = value
        .get("permissions")
        .and_then(serde_json::Value::as_array)
        .ok_or("no permissions array")?
        .iter()
        .map(|p| p.as_str().ok_or("a permission is not a string"))
        .collect::<Result<_, _>>()?;
    Ok(REQUIRED_PERMISSIONS
        .iter()
        .copied()
        .filter(|required| !granted.contains(required))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capability_file_grants_every_required_permission() -> Result<(), String> {
        let json = include_str!("../capabilities/default.json");
        assert_eq!(missing_permissions(json)?, Vec::<&str>::new());
        Ok(())
    }

    #[test]
    fn a_missing_permission_is_reported() -> Result<(), String> {
        let json = r#"{"permissions": ["core:default", "dialog:allow-open"]}"#;
        assert_eq!(
            missing_permissions(json)?,
            vec![
                "core:window:allow-start-dragging",
                "dialog:allow-save",
                "fs:allow-write-text-file",
                "fs:allow-write-file",
            ]
        );
        Ok(())
    }
}
