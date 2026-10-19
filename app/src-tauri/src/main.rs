//! Varve Community: the app with no extensions.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use varve_core::Edition;

fn main() -> std::process::ExitCode {
    match varve_app::run(tauri::generate_context!(), Edition::Community, Vec::new()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Varve could not start: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
