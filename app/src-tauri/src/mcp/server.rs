//! Streamable HTTP on the loopback address, at `/mcp`.
//!
//! Every request, on any path, is refused when its `Host` is not a loopback
//! name (DNS rebinding) or when it carries an `Origin` header: MCP clients
//! send none, browsers send one with every cross-site request. rmcp checks
//! both again. [`IDENTITY_PATH`] then answers without a token: the server
//! proves it holds one. Every other request must carry
//! `Authorization: Bearer <token>` before rmcp sees it.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode, header};
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::tools::VarveTools;
use super::{ENDPOINT_FILE, IDENTITY_PATH, McpHost, TokenCell, valid_nonce};
use crate::commands::Services;

/// Largest request body accepted (a saved query with a long script fits).
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// How long a client may take to send a request's headers, and how long an
/// idle kept-alive connection stays open. A connection that sends nothing
/// is closed after it, so it cannot hold a file descriptor for ever.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Connections served at once. One more is closed as soon as it is
/// accepted, so no local process can use up Varve's file descriptors
/// (which its database connections, tunnels and `app.db` need too). MCP
/// clients use one or two each.
pub const MAX_CONNECTIONS: usize = 64;

type Mcp = StreamableHttpService<VarveTools, LocalSessionManager>;

/// A server that accepts connections until [`RunningServer::stop`].
pub struct RunningServer {
    addr: SocketAddr,
    cancel: CancellationToken,
    task: JoinHandle<()>,
    dir: PathBuf,
}

impl RunningServer {
    /// The address it listens on.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The URL clients connect to.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    /// Stops accepting connections, ends open streams and removes
    /// [`ENDPOINT_FILE`].
    pub async fn stop(self) {
        let url = self.url();
        self.cancel.cancel();
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "the MCP server task failed");
        }
        remove_endpoint(&self.dir, &url);
    }
}

/// Removes [`ENDPOINT_FILE`] from `dir` when it names `url`: a file that
/// names another server belongs to another Varve with the same data
/// directory (a second instance), which still runs.
pub fn remove_endpoint(dir: &Path, url: &str) {
    let path = dir.join(ENDPOINT_FILE);
    match std::fs::read_to_string(&path) {
        Ok(current) if current.trim() == url => {}
        Ok(_) => return,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(%error, "could not read the MCP endpoint file");
            return;
        }
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, "could not remove the MCP endpoint file"),
    }
}

/// Listens on `addr` (port 0 picks a free port) and writes the URL to
/// [`ENDPOINT_FILE`] in `dir`.
///
/// # Errors
/// The address cannot be bound or the endpoint file cannot be written.
pub async fn start(
    addr: SocketAddr,
    services: Arc<Services>,
    host: Arc<dyn McpHost>,
    token: Arc<TokenCell>,
    dir: &Path,
) -> std::io::Result<RunningServer> {
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(["localhost", "127.0.0.1"])
        .enforce_origin_validation()
        .with_max_request_body_bytes(MAX_BODY_BYTES)
        .with_cancellation_token(cancel.child_token());
    let mcp: Mcp = StreamableHttpService::new(
        move || Ok(VarveTools::new(Arc::clone(&services), Arc::clone(&host))),
        Arc::default(),
        config,
    );
    let running = RunningServer {
        addr,
        cancel: cancel.clone(),
        task: tokio::spawn(accept_loop(listener, addr, mcp, token, cancel)),
        dir: dir.to_path_buf(),
    };
    let url = running.url();
    let endpoint = dir.join(ENDPOINT_FILE);
    if let Err(error) = tokio::task::spawn_blocking(move || super::write_private(&endpoint, &url))
        .await
        .map_err(std::io::Error::other)
        .and_then(|written| written)
    {
        running.stop().await;
        return Err(error);
    }
    tracing::info!(%addr, "MCP server listening");
    Ok(running)
}

async fn accept_loop(
    listener: TcpListener,
    addr: SocketAddr,
    mcp: Mcp,
    token: Arc<TokenCell>,
    cancel: CancellationToken,
) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let accepted = tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!(%error, "MCP accept failed");
                continue;
            }
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            tracing::warn!(
                limit = MAX_CONNECTIONS,
                "MCP connection refused: too many open connections"
            );
            drop(stream);
            continue;
        };
        let (mcp, token, cancel) = (mcp.clone(), Arc::clone(&token), cancel.clone());
        tokio::spawn(async move {
            // Held while the connection is served.
            let _slot = slot;
            let service = hyper::service::service_fn(move |request| {
                let (mcp, token) = (mcp.clone(), Arc::clone(&token));
                async move { Ok::<_, Infallible>(handle(request, addr, &mcp, &token).await) }
            });
            // The header timeout only works with a timer; without one
            // hyper silently waits for ever.
            let connection = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service);
            tokio::pin!(connection);
            let served = tokio::select! {
                served = connection.as_mut() => served,
                () = cancel.cancelled() => {
                    connection.as_mut().graceful_shutdown();
                    connection.await
                }
            };
            if let Err(error) = served {
                tracing::debug!(%error, "MCP connection ended with an error");
            }
        });
    }
}

async fn handle(
    request: Request<Incoming>,
    addr: SocketAddr,
    mcp: &Mcp,
    token: &TokenCell,
) -> Response<BoxBody<Bytes, Infallible>> {
    if !loopback_host(request.headers()) {
        return plain(
            StatusCode::FORBIDDEN,
            "Only localhost and 127.0.0.1 are served.",
        );
    }
    if request.headers().contains_key(header::ORIGIN) {
        return plain(StatusCode::FORBIDDEN, "Browsers are not allowed here.");
    }
    if request.uri().path() == IDENTITY_PATH {
        return identity(&request, addr, token);
    }
    if request.uri().path() != "/mcp" {
        return plain(
            StatusCode::NOT_FOUND,
            "Not found. The MCP endpoint is /mcp.",
        );
    }
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer").then_some(token));
    if !presented.is_some_and(|presented| token.matches(presented.trim())) {
        let mut response = plain(
            StatusCode::UNAUTHORIZED,
            "Send the token from Varve's mcp-token file as `Authorization: Bearer <token>`.",
        );
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer realm=\"varve\""),
        );
        return response;
    }
    mcp.handle(request).await
}

/// Whether the `Host` header names `localhost` or `127.0.0.1`, with or
/// without a port.
fn loopback_host(headers: &header::HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1"
}

/// `GET /mcp/identity?nonce=<hex>`: the proof that this server, at `addr`,
/// holds the token.
fn identity(
    request: &Request<Incoming>,
    addr: SocketAddr,
    token: &TokenCell,
) -> Response<BoxBody<Bytes, Infallible>> {
    if request.method() != hyper::Method::GET {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "Use GET.");
    }
    let nonce = request
        .uri()
        .query()
        .and_then(|query| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("nonce="))
        })
        .filter(|nonce| valid_nonce(nonce));
    let Some(nonce) = nonce else {
        return plain(
            StatusCode::BAD_REQUEST,
            "Send ?nonce= with 32 to 128 hex digits.",
        );
    };
    match token.prove(nonce, &addr.to_string()) {
        Some(proof) => text(StatusCode::OK, proof),
        None => plain(StatusCode::SERVICE_UNAVAILABLE, "No token yet."),
    }
}

fn plain(status: StatusCode, message: &'static str) -> Response<BoxBody<Bytes, Infallible>> {
    text(status, message.to_string())
}

fn text(status: StatusCode, message: String) -> Response<BoxBody<Bytes, Infallible>> {
    let mut response = Response::new(Full::new(Bytes::from(message)).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
