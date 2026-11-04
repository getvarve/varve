//! The `varve mcp` bridge builds its HTTP client in a fresh process.
//!
//! On Windows and Linux tauri-plugin-updater builds reqwest with rustls but
//! without a crypto provider, and `reqwest::Client` then panics on build.
//! Inside the bridge that panic killed the request's task, so the MCP client
//! waited for an answer forever. This file is its own test binary, so no
//! other test has installed a provider before this one runs.
//!
//! ```sh
//! cargo test -p varve-app --test mcp_http_client
//! ```

#[test]
fn the_bridge_client_builds_without_a_provider_installed_first() {
    assert!(
        rustls::crypto::CryptoProvider::get_default().is_none(),
        "this test needs a process where no provider was installed yet"
    );
    assert!(varve_app::mcp::http_client().is_ok());
}
