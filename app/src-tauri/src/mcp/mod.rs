//! The local MCP server: MCP clients such as Claude Code read and change
//! folders, connection profiles, saved queries and settings of the running
//! app.
//!
//! The server lives in the app process, not beside it, because a change to
//! a connection is more than a row in `app.db`: the app takes its record and
//! tree locks, moves saved secrets out of the keychain when a profile points
//! at another server, and queues keychain removals. Every tool goes through
//! [`Services`], the same code the UI calls, and the window refreshes after
//! each change ([`McpHost`]).
//!
//! It serves Streamable HTTP on `127.0.0.1` only ([`server`]), off by default
//! ([`McpSettings`]). Every request needs the bearer token in
//! [`TOKEN_FILE`], readable only by the user, and a request that carries an
//! `Origin` header (any browser page) is refused, so a web page cannot reach
//! it. `varve mcp` ([`bridge`]) offers the same tools over stdio to clients
//! that start a command; it reads the token and [`ENDPOINT_FILE`] and needs
//! the app to run. Secrets never pass through MCP: no tool reads or saves a
//! password or passphrase.

pub mod bridge;
pub mod server;
pub mod tools;

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use varve_store::Store;

use crate::commands::{CmdError, Services, run_blocking};
use crate::settings::{self, McpSettings};

/// File in the data directory with the bearer token (64 hex characters).
/// Created on first use with mode 0600; kept across restarts so configured
/// clients keep working, replaced by [`McpController::rotate_token`].
pub const TOKEN_FILE: &str = "mcp-token";

/// File in the data directory with the URL of the running server, for
/// [`bridge`]. Written when the server starts, removed when it stops.
pub const ENDPOINT_FILE: &str = "mcp-endpoint";

/// Path where the server proves it holds the token, without the client
/// sending it: `GET /mcp/identity?nonce=<hex>` answers [`identity_proof`].
/// The bridge checks it before it sends the token, so a program that took
/// the port while Varve was not running never gets the token.
pub const IDENTITY_PATH: &str = "/mcp/identity";

/// HMAC-SHA256 with `token` as the key over `nonce` and the address the
/// server listens on (`127.0.0.1:7465`), in hex. The address is part of it so
/// a relay on another port (a local proxy) cannot pass the real server's
/// proof off as its own.
#[must_use]
pub fn identity_proof(token: &str, nonce: &str, authority: &str) -> String {
    use hmac::{KeyInit, Mac};
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(token.as_bytes())
        .unwrap_or_else(|_| unreachable!("HMAC takes keys of any length"));
    mac.update(b"varve-mcp-identity:");
    mac.update(authority.as_bytes());
    mac.update(b":");
    mac.update(nonce.as_bytes());
    to_hex(&mac.finalize().into_bytes())
}

/// Whether `nonce` is what [`IDENTITY_PATH`] accepts: 32 to 128 hex digits.
#[must_use]
pub fn valid_nonce(nonce: &str) -> bool {
    (32..=128).contains(&nonce.len()) && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `len` random bytes in hex.
///
/// # Errors
/// The OS has no randomness to give.
pub fn random_hex(len: usize) -> std::io::Result<String> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(to_hex(&bytes))
}

/// What answers at an MCP URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// A server that proved it holds the token: Varve with this data
    /// directory.
    Varve,
    /// Something answered without the proof.
    NotVarve,
    /// Nothing answered.
    Nothing,
}

/// The HTTP client for talking to the server: straight to the loopback
/// address, never through a proxy from `HTTP_PROXY` or `ALL_PROXY` (which
/// would see the token), and following no redirects.
///
/// # Errors
/// reqwest cannot set up its client.
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    crate::ensure_crypto_provider();
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(0)
        .build()
}

/// Asks the server at `url` (`http://127.0.0.1:7465/mcp`) to prove it holds
/// `token`, without sending the token. The proof covers the address, so a
/// relay cannot pass on Varve's.
pub async fn check_identity(client: &reqwest::Client, url: &str, token: &str) -> Identity {
    use subtle::ConstantTimeEq;
    let Ok(nonce) = random_hex(32) else {
        return Identity::Nothing;
    };
    let base = url.strip_suffix("/mcp").unwrap_or(url);
    let authority = base
        .strip_prefix("http://")
        .unwrap_or(base)
        .trim_end_matches('/');
    let answer = match client
        .get(format!("{base}{IDENTITY_PATH}?nonce={nonce}"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            tracing::debug!(%error, %url, "no MCP server answered");
            return Identity::Nothing;
        }
    };
    let proof = if answer.status().is_success() {
        answer.text().await.unwrap_or_default()
    } else {
        String::new()
    };
    let expected = identity_proof(token, &nonce, authority);
    if bool::from(proof.trim().as_bytes().ct_eq(expected.as_bytes())) {
        Identity::Varve
    } else {
        Identity::NotVarve
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Event the window gets after a tool changed folders, connections or saved
/// queries: it reloads the tree.
pub const RECORDS_CHANGED_EVENT: &str = "varve://records-changed";

/// Event the window gets after a tool changed settings: the settings page
/// reloads them.
pub const SETTINGS_CHANGED_EVENT: &str = "varve://settings-changed";

/// What the tools need from the app around [`Services`]: telling the window
/// about changes, and the update setting, which belongs to the updater
/// (Sparkle on macOS) rather than to `app.db`.
#[async_trait]
pub trait McpHost: Send + Sync + 'static {
    /// A tool changed folders, connections or saved queries.
    fn records_changed(&self);
    /// A tool changed settings.
    fn settings_changed(&self);
    /// Whether updates are checked for automatically; `None` when this build
    /// does not update itself.
    ///
    /// # Errors
    /// The updater failed to answer.
    async fn update_auto_check(&self) -> Result<Option<bool>, CmdError>;
    /// Turns automatic update checks on or off.
    ///
    /// # Errors
    /// This build does not update itself, or the updater refused.
    async fn set_update_auto_check(&self, auto_check: bool) -> Result<(), CmdError>;
}

/// [`McpHost`] of the running app.
pub struct TauriHost<R: tauri::Runtime>(pub tauri::AppHandle<R>);

#[async_trait]
impl<R: tauri::Runtime> McpHost for TauriHost<R> {
    fn records_changed(&self) {
        use tauri::Emitter;
        if let Err(error) = self.0.emit(RECORDS_CHANGED_EVENT, ()) {
            tracing::warn!(%error, "could not tell the window about an MCP change");
        }
    }

    fn settings_changed(&self) {
        use tauri::Emitter;
        if let Err(error) = self.0.emit(SETTINGS_CHANGED_EVENT, ()) {
            tracing::warn!(%error, "could not tell the window about an MCP settings change");
        }
    }

    async fn update_auto_check(&self) -> Result<Option<bool>, CmdError> {
        let info = crate::updates::update_info(self.0.clone()).await?;
        Ok(match info.mechanism {
            crate::updates::Mechanism::None => None,
            _ => Some(info.auto_check),
        })
    }

    async fn set_update_auto_check(&self, auto_check: bool) -> Result<(), CmdError> {
        crate::updates::set_update_auto_check(self.0.clone(), auto_check).await
    }
}

/// The bearer token every request must carry, shared with the running
/// server so a new token applies to the next request.
#[derive(Debug, Default)]
pub struct TokenCell(RwLock<String>);

impl TokenCell {
    /// A cell holding `token`.
    #[must_use]
    pub fn new(token: String) -> Self {
        Self(RwLock::new(token))
    }

    /// Whether `presented` is the token, compared in constant time.
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        use subtle::ConstantTimeEq;
        let Ok(token) = self.0.read() else {
            return false;
        };
        !token.is_empty() && bool::from(token.as_bytes().ct_eq(presented.as_bytes()))
    }

    /// [`identity_proof`] of `nonce` and `authority` with the current token;
    /// `None` while there is no token.
    #[must_use]
    pub fn prove(&self, nonce: &str, authority: &str) -> Option<String> {
        let token = self.0.read().ok()?;
        (!token.is_empty()).then(|| identity_proof(&token, nonce, authority))
    }

    fn replace(&self, token: String) {
        match self.0.write() {
            Ok(mut current) => *current = token,
            Err(poisoned) => *poisoned.into_inner() = token,
        }
    }
}

/// Reads the token in `dir`, creating it when it is missing or damaged.
///
/// # Errors
/// The file cannot be read or written.
pub fn load_or_create_token(dir: &Path) -> std::io::Result<String> {
    match read_token(dir) {
        Ok(Some(token)) => Ok(token),
        Ok(None) => write_new_token(dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => write_new_token(dir),
        Err(error) => Err(error),
    }
}

/// Reads the token in `dir`; `None` when the file holds no valid token.
///
/// # Errors
/// The file cannot be read (`NotFound` when there is none).
pub fn read_token(dir: &Path) -> std::io::Result<Option<String>> {
    let text = std::fs::read_to_string(dir.join(TOKEN_FILE))?;
    let token = text.trim();
    let valid = token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit());
    Ok(valid.then(|| token.to_ascii_lowercase()))
}

/// Writes a new random token to `dir` and returns it.
///
/// # Errors
/// No randomness, or the file cannot be written.
pub fn write_new_token(dir: &Path) -> std::io::Result<String> {
    let token = random_hex(32)?;
    write_private(&dir.join(TOKEN_FILE), &token)?;
    Ok(token)
}

/// Writes `contents` to `path` through a temporary file, readable only by
/// the user on Unix (the data directory already is on Windows).
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    #[cfg(unix)]
    {
        // `mode` only applies to a new file: an old temporary file keeps its own.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// What the settings page shows about the MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatus {
    /// The stored settings.
    pub settings: McpSettings,
    /// The URL clients connect to while the server runs.
    pub url: Option<String>,
    /// Why the server is not running although it is enabled (the port is
    /// taken, the token file cannot be written).
    pub error: Option<String>,
    /// Path of [`TOKEN_FILE`].
    pub token_file: String,
    /// The command that starts the stdio bridge: this executable and `mcp`.
    /// Empty when the executable's path is unknown.
    pub bridge_command: Vec<String>,
}

struct ControllerState {
    running: Option<server::RunningServer>,
    error: Option<String>,
}

/// The URL this instance's server writes to [`ENDPOINT_FILE`] while it runs,
/// for [`McpController::forget_endpoint`] at exit, outside the async lock.
type ServingUrl = std::sync::Mutex<Option<String>>;

/// Starts, stops and reconfigures the server; managed by Tauri.
pub struct McpController {
    services: Arc<Services>,
    host: Arc<dyn McpHost>,
    dir: PathBuf,
    token: Arc<TokenCell>,
    state: tokio::sync::Mutex<ControllerState>,
    serving: ServingUrl,
}

impl McpController {
    /// A stopped controller for the app whose data directory is `dir`.
    #[must_use]
    pub fn new(services: Arc<Services>, host: Arc<dyn McpHost>, dir: PathBuf) -> Self {
        Self {
            services,
            host,
            dir,
            token: Arc::new(TokenCell::default()),
            state: tokio::sync::Mutex::new(ControllerState {
                running: None,
                error: None,
            }),
            serving: ServingUrl::default(),
        }
    }

    fn store(&self) -> &Store {
        &self.services.store
    }

    /// Starts the server when the stored settings enable it. The settings
    /// are read under the controller's lock, so a change the user makes
    /// while the app starts is never undone by older settings.
    pub async fn start_from_settings(&self) -> McpStatus {
        let mut state = self.state.lock().await;
        match settings::load_mcp(self.store()) {
            Ok(settings) => self.apply(&mut state, settings).await,
            Err(error) => {
                state.error = Some(format!("Could not read the MCP settings: {error}"));
                self.status_of(&state, McpSettings::default())
            }
        }
    }

    /// The current status.
    ///
    /// # Errors
    /// Storage errors.
    pub async fn status(&self) -> Result<McpStatus, CmdError> {
        let state = self.state.lock().await;
        let settings = settings::load_mcp(self.store())?;
        Ok(self.status_of(&state, settings))
    }

    /// Stores new settings and starts, stops or moves the server to match.
    ///
    /// # Errors
    /// Invalid settings or storage errors. A server that cannot start is
    /// reported in [`McpStatus::error`], not as an error: the settings are
    /// stored either way.
    pub async fn set_settings(&self, new: McpSettings) -> Result<McpStatus, CmdError> {
        settings::validate_mcp(&new).map_err(|message| CmdError::Invalid { message })?;
        let mut state = self.state.lock().await;
        let services = Arc::clone(&self.services);
        run_blocking(move || Ok(settings::save_mcp(&services.store, &new)?)).await?;
        Ok(self.apply(&mut state, new).await)
    }

    /// Replaces the token: clients using the old one are refused from their
    /// next request on. Under the controller's lock, so a server starting at
    /// the same moment cannot put the old token back.
    ///
    /// # Errors
    /// The token file cannot be written.
    pub async fn rotate_token(&self) -> Result<McpStatus, CmdError> {
        let state = self.state.lock().await;
        let dir = self.dir.clone();
        let token = run_blocking(move || {
            write_new_token(&dir).map_err(|error| CmdError::Internal {
                message: format!("Could not write a new MCP token: {error}"),
            })
        })
        .await?;
        self.token.replace(token);
        let settings = settings::load_mcp(self.store())?;
        Ok(self.status_of(&state, settings))
    }

    /// Stops the server.
    pub async fn stop(&self) {
        let mut state = self.state.lock().await;
        if let Some(running) = state.running.take() {
            running.stop().await;
        }
        self.set_serving(None);
    }

    /// Removes [`ENDPOINT_FILE`] when the app exits, so the bridge does not
    /// try a server that is gone; only when it names this instance's server.
    /// Synchronous: the runtime is shutting down.
    pub fn forget_endpoint(&self) {
        let url = match self.serving.lock() {
            Ok(url) => url.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if let Some(url) = url {
            server::remove_endpoint(&self.dir, &url);
        }
    }

    fn set_serving(&self, url: Option<String>) {
        match self.serving.lock() {
            Ok(mut serving) => *serving = url,
            Err(poisoned) => *poisoned.into_inner() = url,
        }
    }

    async fn apply(&self, state: &mut ControllerState, settings: McpSettings) -> McpStatus {
        let unchanged = state
            .running
            .as_ref()
            .is_some_and(|r| settings.enabled && r.addr().port() == settings.port);
        if !unchanged {
            if let Some(running) = state.running.take() {
                running.stop().await;
            }
            self.set_serving(None);
            state.error = None;
            if settings.enabled {
                match self.start(settings.port).await {
                    Ok(running) => {
                        self.set_serving(Some(running.url()));
                        state.running = Some(running);
                    }
                    Err((kind, message)) => {
                        let message = if kind == std::io::ErrorKind::AddrInUse {
                            self.port_taken(settings.port).await.unwrap_or(message)
                        } else {
                            message
                        };
                        tracing::warn!(%message, "the MCP server did not start");
                        state.error = Some(message);
                    }
                }
            }
        }
        self.status_of(state, settings)
    }

    /// `port` is taken. When another Varve with this data directory serves
    /// there (a second instance), its endpoint file stays and the message
    /// says so. Otherwise an endpoint file naming the port is left by a
    /// killed run: it is removed, so the bridge does not go to whatever
    /// holds the port now. `None` keeps the plain "in use" message.
    async fn port_taken(&self, port: u16) -> Option<String> {
        let url = format!("http://127.0.0.1:{port}/mcp");
        let named = std::fs::read_to_string(self.dir.join(ENDPOINT_FILE))
            .is_ok_and(|current| current.trim() == url);
        let client = http_client().ok()?;
        let token = read_token(&self.dir).ok().flatten()?;
        if check_identity(&client, &url, &token).await == Identity::Varve {
            return Some(format!(
                "Another Varve window already serves MCP on port {port}. MCP clients reach it there."
            ));
        }
        if named {
            server::remove_endpoint(&self.dir, &url);
        }
        None
    }

    /// Call it under the controller's lock.
    async fn start(
        &self,
        port: u16,
    ) -> Result<server::RunningServer, (std::io::ErrorKind, String)> {
        let dir = self.dir.clone();
        let token = run_blocking(move || {
            load_or_create_token(&dir).map_err(|error| CmdError::Internal {
                message: format!("Could not create the token file: {error}"),
            })
        })
        .await
        .map_err(|error| (std::io::ErrorKind::Other, error.to_string()))?;
        self.token.replace(token);
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        server::start(
            addr,
            Arc::clone(&self.services),
            Arc::clone(&self.host),
            Arc::clone(&self.token),
            &self.dir,
        )
        .await
        .map_err(|error| {
            let message = match error.kind() {
                std::io::ErrorKind::AddrInUse => {
                    format!("Port {port} is in use by another program. Pick another port.")
                }
                _ => format!("Could not listen on 127.0.0.1:{port}: {error}"),
            };
            (error.kind(), message)
        })
    }

    fn status_of(&self, state: &ControllerState, settings: McpSettings) -> McpStatus {
        McpStatus {
            settings,
            url: state.running.as_ref().map(server::RunningServer::url),
            error: state.error.clone(),
            token_file: self.dir.join(TOKEN_FILE).display().to_string(),
            bridge_command: std::env::current_exe()
                .map(|exe| vec![exe.display().to_string(), "mcp".to_string()])
                .unwrap_or_default(),
        }
    }
}
