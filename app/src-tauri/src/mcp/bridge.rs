//! `varve mcp`: the tools of the running app over stdio, for MCP clients
//! that start a command.
//!
//! The bridge holds no records or settings itself. It reads the token and
//! the server URL from the data directory and forwards `tools/list` and
//! `tools/call` to the app's local server, so every change still goes
//! through the app ([`super`] says why). It connects on the first request
//! and connects again when the app was restarted in between.
//!
//! The URL may be stale: when Varve was killed, [`ENDPOINT_FILE`] stays, and
//! another program (another user's, on a shared computer) may listen on the
//! port by now. So before it sends the token, the bridge asks the server to
//! prove it holds it ([`IDENTITY_PATH`]).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt};
use tokio::sync::Mutex;

use super::{ENDPOINT_FILE, Identity, check_identity, http_client, read_token};

/// What a client is told while the app or its server is not running.
const NOT_RUNNING: &str = "Varve is not running, or its local MCP server is off. Open Varve, \
turn on Settings > Local MCP server, and try again.";

type Remote = RunningService<RoleClient, ()>;

/// The stdio side of the bridge.
pub struct Bridge {
    dir: PathBuf,
    remote: Mutex<Option<Remote>>,
}

impl Bridge {
    /// A bridge to the app whose data directory is `dir`.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            remote: Mutex::new(None),
        }
    }

    /// Runs `f` against the app's server, connecting first when needed;
    /// after a broken connection the next request connects again (rmcp
    /// itself starts a new session when the app restarted on the same port).
    /// A failed request is sent once more on a new connection when that
    /// cannot do anything twice: `idempotent` requests, and requests that
    /// never ran ([`never_ran`]). Otherwise the client hears that the call
    /// may have been carried out.
    async fn with_remote<T, F, Fut>(&self, idempotent: bool, f: F) -> Result<T, ErrorData>
    where
        F: Fn(rmcp::Peer<RoleClient>) -> Fut,
        Fut: Future<Output = Result<T, ServiceError>>,
    {
        let mut remote = self.remote.lock().await;
        let mut attempt = 0;
        loop {
            attempt += 1;
            if remote.as_ref().is_none_or(RunningService::is_closed) {
                *remote = Some(connect(&self.dir).await?);
            }
            let Some(peer) = remote.as_ref().map(|running| running.peer().clone()) else {
                return Err(ErrorData::internal_error(NOT_RUNNING, None));
            };
            let error = match f(peer).await {
                Ok(value) => return Ok(value),
                Err(ServiceError::McpError(error)) => return Err(error),
                Err(error) => error,
            };
            tracing::debug!(%error, attempt, "the MCP connection to Varve failed");
            if let Some(stale) = remote.take() {
                let _ = stale.cancel().await;
            }
            let safe = idempotent || never_ran(&error);
            if safe && attempt < 2 {
                continue;
            }
            return Err(ErrorData::internal_error(
                if safe {
                    NOT_RUNNING.to_string()
                } else {
                    format!(
                        "Lost the connection to Varve during the call ({error}): Varve is not \
                         running any more, or it restarted. The call may or may not have been \
                         carried out; check with a get_ tool before you try again."
                    )
                },
                None,
            ));
        }
    }
}

/// Connects to the server named in [`ENDPOINT_FILE`] with the token, once
/// the server proved it is Varve.
async fn connect(dir: &Path) -> Result<Remote, ErrorData> {
    let not_running = || ErrorData::internal_error(NOT_RUNNING, None);
    let url = std::fs::read_to_string(dir.join(ENDPOINT_FILE)).map_err(|_| not_running())?;
    let url = url.trim().to_string();
    let token = read_token(dir).ok().flatten().ok_or_else(not_running)?;
    let client =
        http_client().map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    verify_identity(&client, &url, &token).await?;
    let transport = StreamableHttpClientTransport::with_client(
        client,
        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
    );
    ().serve(transport).await.map_err(|error| {
        tracing::debug!(%error, "could not connect to Varve's MCP server");
        not_running()
    })
}

/// Checks that the server at `url` holds `token`, without sending it.
async fn verify_identity(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<(), ErrorData> {
    match check_identity(client, url, token).await {
        Identity::Varve => Ok(()),
        Identity::Nothing => Err(ErrorData::internal_error(NOT_RUNNING, None)),
        Identity::NotVarve => Err(ErrorData::internal_error(
            format!(
                "Another program answers at {url}, not Varve (it does not know the token), so the \
                 token was not sent. Open Varve, or pick another port in Settings > Local MCP server."
            ),
            None,
        )),
    }
}

/// Whether `error` means the request never ran: the server refused the
/// token (it was replaced) or nothing listened. Such a request can be sent
/// again on a new connection without doing anything twice.
fn never_ran(error: &ServiceError) -> bool {
    use rmcp::transport::streamable_http_client::StreamableHttpError;
    let ServiceError::TransportSend(error) = error else {
        return false;
    };
    match error
        .error
        .downcast_ref::<StreamableHttpError<reqwest::Error>>()
    {
        Some(StreamableHttpError::AuthRequired(_)) => true,
        Some(StreamableHttpError::Client(error)) => error.is_connect(),
        _ => false,
    }
}

impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        let mut implementation = Implementation::new("varve", env!("CARGO_PKG_VERSION"));
        implementation.title = Some("Varve".into());
        implementation.website_url = Some("https://getvarve.com".into());
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(implementation)
            .with_instructions(super::tools::INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.with_remote(true, |peer| {
            let request = request.clone();
            async move { peer.list_tools(request).await }
        })
        .await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.with_remote(false, |peer| {
            let request = request.clone();
            async move { peer.call_tool(request).await }
        })
        .await
        .map(Into::into)
    }
}

/// Serves the bridge on `transport` until the client hangs up.
///
/// # Errors
/// The client did not complete the handshake, or the stream failed.
pub async fn serve<T, E, A>(transport: T, dir: PathBuf) -> Result<(), Box<dyn std::error::Error>>
where
    T: rmcp::transport::IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let bridge = Bridge::new(dir);
    let running = bridge.serve(transport).await?;
    running.waiting().await?;
    Ok(())
}

/// The data directory the app uses: [`crate::DATA_DIR_ENV`], or `Varve` in
/// the OS data directory (the same folder Tauri's `data_dir` gives the app).
fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(crate::DATA_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    dirs::data_dir().map(|base| base.join("Varve"))
}

/// Entry point of `varve mcp`. Logs go to stderr: stdout is the protocol.
pub fn main() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_env("VARVE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
    let Some(dir) = data_dir() else {
        eprintln!(
            "varve mcp: cannot find the data directory; set {}",
            crate::DATA_DIR_ENV
        );
        return ExitCode::FAILURE;
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("varve mcp: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve(rmcp::transport::io::stdio_pipes(), dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("varve mcp: {error}");
            ExitCode::FAILURE
        }
    }
}
