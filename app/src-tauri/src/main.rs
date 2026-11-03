//! Varve: the desktop app.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    // `varve mcp`: the stdio bridge to the running app's MCP server.
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "mcp") {
        return varve_app::mcp::bridge::main();
    }
    match varve_app::run(tauri::generate_context!(), Vec::new()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Varve could not start: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
