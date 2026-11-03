//! The local MCP server end to end: a real rmcp client over Streamable
//! HTTP, and over the stdio bridge, against the server on a free loopback
//! port. No database servers needed.
//!
//! ```sh
//! cargo test -p varve-app --test mcp
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use varve_app::commands::{CmdError, Services};
use varve_app::mcp::server::{self, RunningServer};
use varve_app::mcp::{self, McpController, McpHost, TokenCell};
use varve_app::settings::McpSettings;
use varve_records::SecretKindName;
use varve_secrets::{MemoryStore, SecretKey, SecretKind};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Client = RunningService<RoleClient, ()>;

/// Records what the tools tell the window, and plays an updater.
#[derive(Default)]
struct FakeHost {
    records_changed: AtomicUsize,
    settings_changed: AtomicUsize,
    /// `None`: this build does not update itself.
    auto_check: Mutex<Option<bool>>,
}

#[async_trait]
impl McpHost for FakeHost {
    fn records_changed(&self) {
        self.records_changed.fetch_add(1, Ordering::SeqCst);
    }

    fn settings_changed(&self) {
        self.settings_changed.fetch_add(1, Ordering::SeqCst);
    }

    async fn update_auto_check(&self) -> Result<Option<bool>, CmdError> {
        Ok(*self.auto_check.lock().map_err(|_| internal())?)
    }

    async fn set_update_auto_check(&self, auto_check: bool) -> Result<(), CmdError> {
        let mut current = self.auto_check.lock().map_err(|_| internal())?;
        if current.is_none() {
            return Err(internal());
        }
        *current = Some(auto_check);
        Ok(())
    }
}

fn internal() -> CmdError {
    CmdError::Internal {
        message: "fake updater".into(),
    }
}

struct Fixture {
    services: Arc<Services>,
    host: Arc<FakeHost>,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(name: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("varve-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let extensions = varve_app::build_host(&[])?;
    let store = Arc::new(varve_app::open_store(&dir, &extensions)?);
    let services =
        varve_app::build_services(store, &dir, &extensions, Arc::new(MemoryStore::default()))?;
    Ok(Fixture {
        services,
        host: Arc::new(FakeHost::default()),
        dir,
    })
}

impl Fixture {
    /// A server on a free port with a new token.
    async fn serve(&self) -> Result<(RunningServer, String), Box<dyn std::error::Error>> {
        let token = mcp::write_new_token(&self.dir)?;
        let running = server::start(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            Arc::clone(&self.services),
            Arc::clone(&self.host) as Arc<dyn McpHost>,
            Arc::new(TokenCell::new(token.clone())),
            &self.dir,
        )
        .await?;
        Ok((running, token))
    }
}

async fn client(url: &str, token: &str) -> Result<Client, Box<dyn std::error::Error>> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.to_string())
            .auth_header(token.to_string()),
    );
    Ok(().serve(transport).await?)
}

/// Calls a tool and returns its result.
async fn call(
    client: &Client,
    name: &'static str,
    arguments: Value,
) -> Result<CallToolResult, Box<dyn std::error::Error>> {
    let Value::Object(arguments) = arguments else {
        return Err("arguments must be an object".into());
    };
    Ok(client
        .peer()
        .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
        .await?)
}

/// Calls a tool that must succeed and returns its JSON.
async fn ok(
    client: &Client,
    name: &'static str,
    arguments: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let result = call(client, name, arguments).await?;
    if result.is_error == Some(true) {
        return Err(format!("{name} failed: {}", text(&result)).into());
    }
    let body: Value = serde_json::from_str(&text(&result))?;
    if let Some(structured) = &result.structured_content {
        assert_eq!(
            structured, &body,
            "{name}: structured content and text differ"
        );
    }
    Ok(body)
}

/// Calls a tool that must fail and returns its error text.
async fn fails(
    client: &Client,
    name: &'static str,
    arguments: Value,
) -> Result<String, Box<dyn std::error::Error>> {
    let result = call(client, name, arguments).await?;
    if result.is_error != Some(true) {
        return Err(format!("{name} succeeded: {}", text(&result)).into());
    }
    Ok(text(&result))
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn id(value: &Value) -> Result<String, Box<dyn std::error::Error>> {
    Ok(value["id"].as_str().ok_or("no id")?.to_string())
}

#[tokio::test]
async fn lists_every_tool_with_hints() -> TestResult {
    let f = fixture("list")?;
    let (running, token) = f.serve().await?;
    let client = client(&running.url(), &token).await?;
    let tools = client.peer().list_all_tools().await?;
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "create_connection",
            "create_folder",
            "create_saved_query",
            "delete_connection",
            "delete_folder",
            "delete_saved_query",
            "duplicate_connection",
            "forget_cached_secrets",
            "get_connection",
            "get_folder",
            "get_saved_query",
            "get_settings",
            "get_workspace",
            "move_connection",
            "move_folder",
            "reorder",
            "update_connection",
            "update_folder",
            "update_saved_query",
            "update_settings",
        ]
    );
    let hint = |name: &str| {
        tools
            .iter()
            .find(|t| t.name == name)
            .and_then(|t| t.annotations.clone())
    };
    assert_eq!(
        hint("get_workspace").and_then(|a| a.read_only_hint),
        Some(true)
    );
    assert_eq!(
        hint("delete_connection").and_then(|a| a.destructive_hint),
        Some(true)
    );
    assert_eq!(
        hint("update_settings").and_then(|a| a.destructive_hint),
        Some(true),
        "it can delete events"
    );
    let info = client.peer_info().ok_or("no server info")?;
    assert_eq!(
        info.server_info.as_ref().map(|s| s.name.as_str()),
        Some("varve")
    );
    assert!(
        info.instructions
            .as_deref()
            .is_some_and(|i| i.contains("keychain"))
    );
    client.cancel().await?;
    running.stop().await;
    Ok(())
}

#[tokio::test]
async fn folders_round_trip_and_the_window_hears_of_it() -> TestResult {
    let f = fixture("folders")?;
    let (running, token) = f.serve().await?;
    let client = client(&running.url(), &token).await?;

    let parent = ok(&client, "create_folder", json!({ "name": "Clients" })).await?;
    let child = ok(
        &client,
        "create_folder",
        json!({ "name": "Acme", "color": "red" }),
    )
    .await?;
    let moved = ok(
        &client,
        "move_folder",
        json!({ "id": id(&child)?, "parentId": id(&parent)? }),
    )
    .await?;
    assert_eq!(moved["parentId"], parent["id"]);
    let renamed = ok(
        &client,
        "update_folder",
        json!({ "id": id(&child)?, "name": "Acme BV" }),
    )
    .await?;
    assert_eq!(renamed["name"], "Acme BV");
    assert_eq!(renamed["color"], "red", "a left-out color stays");
    let cleared = ok(
        &client,
        "update_folder",
        json!({ "id": id(&child)?, "color": null }),
    )
    .await?;
    assert_eq!(cleared["color"], Value::Null);
    assert_eq!(cleared["name"], "Acme BV");

    let folder = ok(&client, "get_folder", json!({ "id": id(&parent)? })).await?;
    assert_eq!(folder["contents"]["folders"], 1);

    ok(
        &client,
        "delete_folder",
        json!({ "id": id(&parent)?, "mode": "move_contents_to_parent" }),
    )
    .await?;
    let workspace = ok(&client, "get_workspace", json!({})).await?;
    let folders = workspace["folders"].as_array().ok_or("no folders")?;
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["parentId"], Value::Null, "the child moved up");

    assert_eq!(f.host.records_changed.load(Ordering::SeqCst), 6);
    client.cancel().await?;
    running.stop().await;
    Ok(())
}

#[tokio::test]
async fn a_new_host_drops_the_saved_password_as_in_the_ui() -> TestResult {
    let f = fixture("secrets")?;
    let (running, token) = f.serve().await?;
    let client = client(&running.url(), &token).await?;

    let created = ok(
        &client,
        "create_connection",
        json!({ "connection": {
            "name": "Shop",
            "engine": "postgres",
            "host": "db.internal",
            "username": "shop",
            "environment": "production",
            "options": { "readOnly": true }
        }}),
    )
    .await?;
    assert_eq!(created["port"], 5432, "the engine default");
    assert_eq!(created["options"]["readOnly"], true);
    assert_eq!(
        created["options"]["confirmDestructive"], true,
        "defaults fill the rest"
    );
    let profile_id = id(&created)?;

    let password = serde_json::from_value(json!("s3cret"))?;
    f.services
        .save_secret(&profile_id, SecretKindName::DbPassword, password)?;
    let key = SecretKey::new(profile_id.clone(), SecretKind::DbPassword)?;
    assert!(f.services.secrets.get(&key)?.is_some());

    // A rename keeps the password: same server, same account.
    let renamed = ok(
        &client,
        "update_connection",
        json!({ "id": profile_id, "changes": { "name": "Shop (live)", "options": { "statementTimeoutMs": 30000 } } }),
    )
    .await?;
    assert_eq!(renamed["savedSecrets"], json!(["db_password"]));
    assert_eq!(renamed["options"]["statementTimeoutMs"], 30000);
    assert_eq!(renamed["options"]["readOnly"], true, "nested fields merge");

    // Another host: the password must not follow the profile there.
    let moved = ok(
        &client,
        "update_connection",
        json!({ "id": profile_id, "changes": { "host": "attacker.example" } }),
    )
    .await?;
    assert_eq!(moved["host"], "attacker.example");
    assert_eq!(moved["savedSecrets"], json!([]));
    assert!(
        f.services.secrets.get(&key)?.is_none(),
        "the old server's password left the keychain"
    );

    client.cancel().await?;
    running.stop().await;
    Ok(())
}

#[tokio::test]
async fn bad_input_is_refused_with_a_reason() -> TestResult {
    let f = fixture("refused")?;
    let (running, token) = f.serve().await?;
    let client = client(&running.url(), &token).await?;

    let typo = fails(
        &client,
        "create_connection",
        json!({ "connection": { "name": "X", "engine": "postgres", "host": "h", "options": { "readonly": true } } }),
    )
    .await?;
    assert!(typo.contains("options.readonly"), "{typo}");

    let engine = fails(
        &client,
        "create_connection",
        json!({ "connection": { "name": "X", "engine": "oracle", "host": "h" } }),
    )
    .await?;
    assert!(engine.contains("oracle"), "{engine}");

    let created = ok(
        &client,
        "create_connection",
        json!({ "connection": { "name": "X", "engine": "redis", "host": "h" } }),
    )
    .await?;
    let unknown = fails(
        &client,
        "update_connection",
        json!({ "id": id(&created)?, "changes": { "password": "nope" } }),
    )
    .await?;
    assert!(unknown.contains("`password`"), "{unknown}");
    let required = fails(
        &client,
        "update_connection",
        json!({ "id": id(&created)?, "changes": { "name": null } }),
    )
    .await?;
    assert!(required.contains("name"), "{required}");
    let production = fails(
        &client,
        "update_connection",
        json!({ "id": id(&created)?, "changes": { "environment": null } }),
    )
    .await?;
    assert!(
        production.contains("`environment` cannot be null"),
        "{production}"
    );
    let nested = fails(
        &client,
        "update_connection",
        json!({ "id": id(&created)?, "changes": { "options": { "confirmDestructive": null } } }),
    )
    .await?;
    assert!(nested.contains("`options.confirmDestructive`"), "{nested}");
    let custom_port = ok(
        &client,
        "update_connection",
        json!({ "id": id(&created)?, "changes": { "port": 6380 } }),
    )
    .await?;
    assert_eq!(custom_port["port"], 6380);
    let default_port = ok(
        &client,
        "update_connection",
        json!({ "id": id(&created)?, "changes": { "port": null } }),
    )
    .await?;
    assert_eq!(
        default_port["port"], 6379,
        "null port means the engine default"
    );

    let missing = fails(
        &client,
        "get_connection",
        json!({ "id": "0190e7d2-0000-7000-8000-000000000000" }),
    )
    .await?;
    assert!(missing.contains("notFound"), "{missing}");

    client.cancel().await?;
    running.stop().await;
    Ok(())
}

#[tokio::test]
async fn connections_and_queries_move_copy_and_go() -> TestResult {
    let f = fixture("crud")?;
    let (running, token) = f.serve().await?;
    let client = client(&running.url(), &token).await?;

    let folder = ok(&client, "create_folder", json!({ "name": "Work" })).await?;
    let conn = ok(
        &client,
        "create_connection",
        json!({ "connection": { "name": "Cache", "engine": "redis", "host": "localhost" } }),
    )
    .await?;
    let conn_id = id(&conn)?;
    let moved = ok(
        &client,
        "move_connection",
        json!({ "id": conn_id, "folderId": id(&folder)? }),
    )
    .await?;
    assert_eq!(moved["folderId"], folder["id"]);
    let copy = ok(&client, "duplicate_connection", json!({ "id": conn_id })).await?;
    assert_ne!(copy["id"], conn["id"]);

    let query = ok(
        &client,
        "create_saved_query",
        json!({ "query": { "name": "Keys", "sql": "SCAN 0", "connectionId": conn_id, "tags": ["ops"] } }),
    )
    .await?;
    let query_id = id(&query)?;
    let edited = ok(
        &client,
        "update_saved_query",
        json!({ "id": query_id, "changes": { "sql": "SCAN 0 COUNT 100" } }),
    )
    .await?;
    assert_eq!(edited["sql"], "SCAN 0 COUNT 100");
    assert_eq!(edited["tags"], json!(["ops"]));
    let fetched = ok(&client, "get_saved_query", json!({ "id": query_id })).await?;
    assert_eq!(fetched["name"], "Keys");

    ok(
        &client,
        "reorder",
        json!({ "kind": "connections", "ids": [id(&copy)?] }),
    )
    .await?;
    ok(&client, "delete_saved_query", json!({ "id": query_id })).await?;
    ok(&client, "delete_connection", json!({ "id": conn_id })).await?;
    ok(
        &client,
        "delete_folder",
        json!({ "id": id(&folder)?, "mode": "delete_contents" }),
    )
    .await?;
    let workspace = ok(&client, "get_workspace", json!({})).await?;
    assert_eq!(workspace["connections"], json!([]));
    assert_eq!(workspace["queries"], json!([]));
    assert_eq!(workspace["folders"], json!([]));

    client.cancel().await?;
    running.stop().await;
    Ok(())
}

#[tokio::test]
async fn settings_read_and_change() -> TestResult {
    let f = fixture("settings")?;
    let (running, token) = f.serve().await?;
    let client = client(&running.url(), &token).await?;

    let before = ok(&client, "get_settings", json!({})).await?;
    assert_eq!(
        before,
        json!({
            "activity": { "textLevel": "normalized", "retentionDays": null },
            "rememberSecretsForSession": true,
            "autoCheckUpdates": null
        })
    );
    let refused = fails(
        &client,
        "update_settings",
        json!({ "autoCheckUpdates": true }),
    )
    .await?;
    assert!(refused.contains("does not update itself"), "{refused}");

    *f.host.auto_check.lock().map_err(|_| "poisoned")? = Some(false);
    let after = ok(
        &client,
        "update_settings",
        json!({
            "activity": { "textLevel": "full", "retentionDays": 30 },
            "rememberSecretsForSession": false,
            "autoCheckUpdates": true
        }),
    )
    .await?;
    assert_eq!(
        after,
        json!({
            "activity": { "textLevel": "full", "retentionDays": 30 },
            "rememberSecretsForSession": false,
            "autoCheckUpdates": true
        })
    );
    assert!(!f.services.secret_settings()?.remember_for_session);

    let forever = ok(
        &client,
        "update_settings",
        json!({ "activity": { "retentionDays": null } }),
    )
    .await?;
    assert_eq!(
        forever["activity"],
        json!({ "textLevel": "full", "retentionDays": null })
    );
    let invalid = fails(
        &client,
        "update_settings",
        json!({ "activity": { "retentionDays": 0 } }),
    )
    .await?;
    assert!(invalid.contains("36500"), "{invalid}");

    let forgotten = ok(&client, "forget_cached_secrets", json!({})).await?;
    assert_eq!(forgotten, json!({ "forgotten": 0 }));
    assert!(f.host.settings_changed.load(Ordering::SeqCst) >= 3);

    client.cancel().await?;
    running.stop().await;
    Ok(())
}

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"1"}}}"#;

async fn post(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<reqwest::StatusCode, Box<dyn std::error::Error>> {
    let mut request = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(INIT);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    Ok(request.send().await?.status())
}

#[tokio::test]
async fn only_the_token_holder_without_a_browser_origin_gets_in() -> TestResult {
    let f = fixture("auth")?;
    let (running, token) = f.serve().await?;
    let url = running.url();
    let bearer = format!("Bearer {token}");

    assert_eq!(post(&url, &[]).await?, 401);
    assert_eq!(post(&url, &[("authorization", "Bearer nope")]).await?, 401);
    assert_eq!(
        post(&url, &[("authorization", &token)]).await?,
        401,
        "without the scheme"
    );
    assert_eq!(
        post(
            &url,
            &[
                ("authorization", &bearer),
                ("origin", "https://evil.example")
            ]
        )
        .await?,
        403
    );
    assert_eq!(
        post(
            &url,
            &[("authorization", &bearer), ("host", "evil.example")]
        )
        .await?,
        403,
        "DNS rebinding"
    );
    let elsewhere = url.replace("/mcp", "/other");
    assert_eq!(post(&elsewhere, &[("authorization", &bearer)]).await?, 404);
    assert_eq!(post(&url, &[("authorization", &bearer)]).await?, 200);
    let lowercase = format!("bearer {token}");
    assert_eq!(
        post(&url, &[("authorization", &lowercase)]).await?,
        200,
        "the scheme is case-insensitive"
    );
    let identity = url.replace(
        "/mcp",
        "/mcp/identity?nonce=00000000000000000000000000000000",
    );
    let rebound = reqwest::Client::new()
        .get(&identity)
        .header("host", "evil.example")
        .send()
        .await?;
    assert_eq!(rebound.status(), 403, "every path checks the Host");

    running.stop().await;
    Ok(())
}

#[tokio::test]
async fn the_stdio_bridge_forwards_to_the_running_app() -> TestResult {
    let f = fixture("bridge")?;
    let (running, _token) = f.serve().await?;
    let (bridge_side, client_side) = tokio::io::duplex(64 * 1024);
    let dir = f.dir.clone();
    let bridge = tokio::spawn(async move {
        mcp::bridge::serve(bridge_side, dir)
            .await
            .map_err(|e| e.to_string())
    });
    let client: Client = ().serve(client_side).await?;

    assert_eq!(client.peer().list_all_tools().await?.len(), 20);
    let folder = ok(&client, "create_folder", json!({ "name": "Via stdio" })).await?;
    assert_eq!(folder["name"], "Via stdio");
    assert_eq!(
        f.services.tree()?.folders.len(),
        1,
        "the change reached the app"
    );

    // The app restarts: the bridge connects again on the next call.
    running.stop().await;
    let not_running = call(&client, "get_workspace", json!({})).await;
    assert!(
        not_running
            .as_ref()
            .err()
            .is_some_and(|e| e.to_string().contains("not running")),
        "{not_running:?}"
    );
    let (again, _token) = f.serve().await?;
    let workspace = ok(&client, "get_workspace", json!({})).await?;
    assert_eq!(workspace["folders"][0]["name"], "Via stdio");

    client.cancel().await?;
    bridge.await??;
    again.stop().await;
    Ok(())
}

/// A port nothing listens on right now.
fn free_port() -> Result<u16, std::io::Error> {
    Ok(std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
        .local_addr()?
        .port())
}

#[tokio::test]
async fn the_controller_starts_moves_rotates_and_stops() -> TestResult {
    let f = fixture("controller")?;
    let controller = McpController::new(
        Arc::clone(&f.services),
        Arc::clone(&f.host) as Arc<dyn McpHost>,
        f.dir.clone(),
    );
    let off = controller.start_from_settings().await;
    assert_eq!(off.settings, McpSettings::default());
    assert_eq!(off.url, None);
    assert!(off.bridge_command.last().is_some_and(|arg| arg == "mcp"));

    let port = free_port()?;
    let on = controller
        .set_settings(McpSettings {
            enabled: true,
            port,
        })
        .await?;
    assert_eq!(on.error, None);
    assert_eq!(
        on.url.as_deref(),
        Some(format!("http://127.0.0.1:{port}/mcp").as_str())
    );
    let token = mcp::read_token(&f.dir)?.ok_or("no token")?;
    assert_eq!(
        std::fs::read_to_string(f.dir.join(mcp::ENDPOINT_FILE))?,
        format!("http://127.0.0.1:{port}/mcp")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(f.dir.join(mcp::TOKEN_FILE))?
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let url = on.url.clone().ok_or("no url")?;
    assert_eq!(
        post(&url, &[("authorization", &format!("Bearer {token}"))]).await?,
        200
    );

    // Restarting the app keeps the token, so configured clients keep working.
    let same = McpController::new(
        Arc::clone(&f.services),
        Arc::clone(&f.host) as Arc<dyn McpHost>,
        f.dir.clone(),
    );
    controller.stop().await;
    assert!(same.start_from_settings().await.url.is_some());
    assert_eq!(mcp::read_token(&f.dir)?.as_deref(), Some(token.as_str()));
    same.stop().await;
    let on = controller.start_from_settings().await;
    assert!(on.url.is_some());

    controller.rotate_token().await?;
    let rotated = mcp::read_token(&f.dir)?.ok_or("no token")?;
    assert_ne!(rotated, token);
    assert_eq!(
        post(&url, &[("authorization", &format!("Bearer {token}"))]).await?,
        401
    );
    assert_eq!(
        post(&url, &[("authorization", &format!("Bearer {rotated}"))]).await?,
        200
    );

    let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let taken_port = taken.local_addr()?.port();
    // As after a killed run: an endpoint file naming the port.
    std::fs::write(
        f.dir.join(mcp::ENDPOINT_FILE),
        format!("http://127.0.0.1:{taken_port}/mcp"),
    )?;
    let busy = controller
        .set_settings(McpSettings {
            enabled: true,
            port: taken_port,
        })
        .await?;
    assert_eq!(busy.url, None);
    assert!(
        busy.error.as_deref().is_some_and(|e| e.contains("in use")),
        "{busy:?}"
    );
    assert!(
        !f.dir.join(mcp::ENDPOINT_FILE).exists(),
        "a failed start removes the old endpoint file"
    );
    drop(taken);

    let stopped = controller
        .set_settings(McpSettings {
            enabled: false,
            port,
        })
        .await?;
    assert_eq!(stopped.url, None);
    assert_eq!(stopped.error, None);
    assert!(!f.dir.join(mcp::ENDPOINT_FILE).exists());
    assert!(post(&url, &[]).await.is_err(), "nothing listens any more");

    let low = controller
        .set_settings(McpSettings {
            enabled: true,
            port: 80,
        })
        .await;
    assert!(matches!(low, Err(CmdError::Invalid { .. })));
    Ok(())
}

#[tokio::test]
async fn the_server_proves_it_holds_the_token_without_receiving_it() -> TestResult {
    let f = fixture("identity")?;
    let (running, token) = f.serve().await?;
    let base = running.url().replace("/mcp", "/mcp/identity");
    let authority = running.addr().to_string();
    let nonce = mcp::random_hex(32)?;
    let http = reqwest::Client::new();

    let answer = http.get(format!("{base}?nonce={nonce}")).send().await?;
    assert_eq!(answer.status(), 200);
    assert_eq!(
        answer.text().await?,
        mcp::identity_proof(&token, &nonce, &authority)
    );
    assert_ne!(
        mcp::identity_proof(&token, &nonce, &authority),
        mcp::identity_proof(&token, &nonce, "127.0.0.1:1"),
        "the proof names the address"
    );

    assert_eq!(
        http.get(format!("{base}?nonce=xyz")).send().await?.status(),
        400
    );
    assert_eq!(http.get(&base).send().await?.status(), 400);
    assert_eq!(
        http.get(format!("{base}?nonce={nonce}"))
            .header("origin", "https://evil.example")
            .send()
            .await?
            .status(),
        403
    );
    assert_eq!(
        http.post(format!("{base}?nonce={nonce}"))
            .send()
            .await?
            .status(),
        405
    );

    running.stop().await;
    Ok(())
}

#[tokio::test]
async fn the_bridge_never_sends_the_token_to_a_program_that_took_the_port() -> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let f = fixture("squatter")?;
    mcp::write_new_token(&f.dir)?;
    // Varve was killed: its endpoint file still names a port another
    // program now listens on, answering 200 to everything.
    let squatter = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = squatter.local_addr()?.port();
    std::fs::write(
        f.dir.join(mcp::ENDPOINT_FILE),
        format!("http://127.0.0.1:{port}/mcp"),
    )?;
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = Arc::clone(&seen);
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = squatter.accept().await {
            let mut buffer = vec![0u8; 16 * 1024];
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            if let Ok(mut log) = log.lock() {
                log.push(String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase());
            }
            let _ = stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\nconnection: close\r\n\r\ngotcha!",
                )
                .await;
        }
    });

    let (bridge_side, client_side) = tokio::io::duplex(64 * 1024);
    let dir = f.dir.clone();
    let bridge = tokio::spawn(async move {
        mcp::bridge::serve(bridge_side, dir)
            .await
            .map_err(|e| e.to_string())
    });
    let client: Client = ().serve(client_side).await?;
    let refused = call(&client, "get_workspace", json!({})).await;
    let message = refused
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(message.contains("Another program"), "{refused:?}");

    let requests = seen.lock().map_err(|_| "poisoned")?.clone();
    assert!(
        !requests.is_empty(),
        "the bridge asked the squatter who it is"
    );
    assert!(
        requests
            .iter()
            .all(|r| r.starts_with("get /mcp/identity?nonce=") && !r.contains("authorization")),
        "{requests:?}"
    );

    client.cancel().await?;
    bridge.await??;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn the_bridge_refuses_a_relay_in_front_of_varve() -> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let f = fixture("relay")?;
    let (running, _token) = f.serve().await?;
    let target = running.addr();
    // A program on another port that forwards every byte to the real
    // server (a local debugging proxy), so it can pass on Varve's answers.
    let relay = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let relay_port = relay.local_addr()?.port();
    let seen = Arc::new(Mutex::new(String::new()));
    let log = Arc::clone(&seen);
    let forwarder = tokio::spawn(async move {
        while let Ok((mut inbound, _)) = relay.accept().await {
            let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await else {
                continue;
            };
            let mut buffer = vec![0u8; 16 * 1024];
            let read = inbound.read(&mut buffer).await.unwrap_or(0);
            if let Ok(mut log) = log.lock() {
                log.push_str(&String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase());
            }
            let _ = outbound.write_all(&buffer[..read]).await;
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
        }
    });
    std::fs::write(
        f.dir.join(mcp::ENDPOINT_FILE),
        format!("http://127.0.0.1:{relay_port}/mcp"),
    )?;

    let (bridge_side, client_side) = tokio::io::duplex(64 * 1024);
    let dir = f.dir.clone();
    let bridge = tokio::spawn(async move {
        mcp::bridge::serve(bridge_side, dir)
            .await
            .map_err(|e| e.to_string())
    });
    let client: Client = ().serve(client_side).await?;
    let refused = call(&client, "get_workspace", json!({})).await;
    let message = refused
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(message.contains("Another program"), "{refused:?}");
    let requests = seen.lock().map_err(|_| "poisoned")?.clone();
    assert!(requests.contains("/mcp/identity"), "{requests}");
    assert!(
        !requests.contains("authorization"),
        "the token never passed the relay: {requests}"
    );

    client.cancel().await?;
    bridge.await??;
    forwarder.abort();
    running.stop().await;
    Ok(())
}

#[tokio::test]
async fn a_call_right_after_a_new_token_still_runs_once() -> TestResult {
    let f = fixture("rotate-bridge")?;
    let controller = McpController::new(
        Arc::clone(&f.services),
        Arc::clone(&f.host) as Arc<dyn McpHost>,
        f.dir.clone(),
    );
    let on = controller
        .set_settings(McpSettings {
            enabled: true,
            port: free_port()?,
        })
        .await?;
    assert!(on.url.is_some(), "{on:?}");

    let (bridge_side, client_side) = tokio::io::duplex(64 * 1024);
    let dir = f.dir.clone();
    let bridge = tokio::spawn(async move {
        mcp::bridge::serve(bridge_side, dir)
            .await
            .map_err(|e| e.to_string())
    });
    let client: Client = ().serve(client_side).await?;
    ok(&client, "create_folder", json!({ "name": "Before" })).await?;

    // The open connection still sends the old token: the server refuses
    // it before anything runs, so the bridge may send the call again.
    controller.rotate_token().await?;
    ok(&client, "create_folder", json!({ "name": "After" })).await?;
    let names: Vec<String> = f
        .services
        .tree()?
        .folders
        .into_iter()
        .map(|folder| folder.name)
        .collect();
    assert_eq!(names.len(), 2, "each call ran exactly once: {names:?}");

    client.cancel().await?;
    bridge.await??;
    controller.stop().await;
    Ok(())
}

#[tokio::test]
async fn a_second_varve_leaves_the_first_ones_server_alone() -> TestResult {
    let f = fixture("two-instances")?;
    let first = McpController::new(
        Arc::clone(&f.services),
        Arc::clone(&f.host) as Arc<dyn McpHost>,
        f.dir.clone(),
    );
    let port = free_port()?;
    let settings = McpSettings {
        enabled: true,
        port,
    };
    let serving = first.set_settings(settings).await?;
    let url = serving.url.clone().ok_or("the first did not start")?;

    let second = McpController::new(
        Arc::clone(&f.services),
        Arc::clone(&f.host) as Arc<dyn McpHost>,
        f.dir.clone(),
    );
    let refused = second.start_from_settings().await;
    assert_eq!(refused.url, None);
    assert!(
        refused
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Another Varve window")),
        "{refused:?}"
    );
    second.forget_endpoint();
    second.stop().await;
    assert_eq!(
        std::fs::read_to_string(f.dir.join(mcp::ENDPOINT_FILE))?,
        url,
        "the first one's endpoint file stays"
    );

    first.forget_endpoint();
    assert!(
        !f.dir.join(mcp::ENDPOINT_FILE).exists(),
        "its own file goes at exit"
    );
    first.stop().await;
    Ok(())
}
