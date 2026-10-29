//! Varve: the desktop app.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    match varve_app::run(tauri::generate_context!(), Vec::new()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Varve could not start: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
