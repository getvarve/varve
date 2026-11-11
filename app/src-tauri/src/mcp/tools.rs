//! The MCP tools. Each one calls [`Services`] exactly as the UI does, so
//! validation, locks and the keychain rules of a change are the same.
//!
//! Records come back in the JSON the UI gets (camelCase fields). Updates
//! take `changes`, merged into the record's current editable fields like a
//! JSON merge patch (RFC 7386): nested objects merge, `null` clears an
//! optional field, and a field the record does not have is refused rather
//! than ignored.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use varve_activity::TextLevel;
use varve_core::Engine;
use varve_records::{
    ConnectionInput, ConnectionProfile, Environment, FolderDeleteMode, PERSONAL_WORKSPACE, Records,
    ReorderKind, SavedQuery, SavedQueryInput,
};

use super::McpHost;
use crate::commands::{CmdError, Services, run_blocking};
use crate::settings::{ActivitySettings, SecretSettings};

pub(crate) const INSTRUCTIONS: &str = "Varve is a desktop manager for PostgreSQL, MySQL, MariaDB and \
Redis. These tools read and change the user's folders, connection profiles, saved queries and \
settings in the running app; the window shows every change at once. Start with \
get_workspace. Passwords and passphrases stay in the system keychain and never pass through \
these tools: after a host, port, user or tunnel change the user may have to enter them again \
in Varve. Init SQL and the safety options (`options.readOnly`, `options.confirmDestructive`, \
`environment`) can only be made stricter here: setting or changing init SQL, or loosening a \
safety option, is refused and the user does it in Varve. Connections tagged production \
deserve extra care: confirm with the user before changing or deleting one.";

/// The tool set; one per MCP session.
#[derive(Clone)]
pub struct VarveTools {
    services: Arc<Services>,
    host: Arc<dyn McpHost>,
    tool_router: ToolRouter<Self>,
}

/// A record id.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IdParams {
    /// Id of the record (a UUID from get_workspace).
    pub id: String,
}

/// Arguments of `create_folder`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateFolderParams {
    /// Parent folder id; leave out for the top level.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Folder name.
    pub name: String,
    /// Color token, for example `red`; leave out for none.
    #[serde(default)]
    pub color: Option<String>,
}

/// Arguments of `update_folder`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateFolderParams {
    /// Folder id.
    pub id: String,
    /// New name; leave out to keep it.
    #[serde(default)]
    pub name: Option<String>,
    /// New color token, or `null` to remove the color; leave out to keep it.
    #[serde(default, deserialize_with = "present")]
    pub color: Option<Option<String>>,
}

/// Arguments of `move_folder`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(transform = target_required)]
pub struct MoveFolderParams {
    /// Folder id.
    pub id: String,
    /// New parent folder id, or `null` for the top level. Required: a
    /// left out target is refused rather than read as the top level.
    #[serde(default, deserialize_with = "present")]
    pub parent_id: Option<Option<String>>,
}

/// What happens to what a deleted folder holds.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    /// Subfolders, connections and saved queries move to the parent folder.
    MoveContentsToParent,
    /// Everything inside is deleted too, including connections and their
    /// saved keychain entries.
    DeleteContents,
}

/// Arguments of `delete_folder`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteFolderParams {
    /// Folder id.
    pub id: String,
    /// What happens to the folder's contents.
    pub mode: DeleteMode,
}

/// Arguments of `create_connection`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateConnectionParams {
    /// The profile. Required: `name`, `engine` (`postgres`, `mysql`,
    /// `mariadb`, `redis`) and `host`. Optional: `folderId`, `color`, `port`
    /// (engine default when left out), `database`, `username`,
    /// `environment` (`development`, `test`, `staging`, `production`),
    /// `tls` {`mode`: `disable`, `prefer`, `require`, `verify_ca`,
    /// `verify_full`; `caPath`, `clientCertPath`, `clientKeyPath`}, `ssh`
    /// {`enabled`, `host`, `port`, `username`, `auth`: `password`,
    /// `key_file`, `agent`; `keyPath`} and `options` {`readOnly`,
    /// `confirmDestructive`, `statementTimeoutMs`, `keepaliveS`,
    /// `showSystemObjects`, `redisDb`}. No passwords: the user saves those
    /// in Varve. `options.initSql` cannot be set here, and
    /// `options.confirmDestructive` cannot be `false`: the user sets those
    /// in Varve.
    pub connection: Map<String, Value>,
}

/// Arguments of the `update_*` tools of connections and saved queries.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateParams {
    /// Id of the record.
    pub id: String,
    /// Fields to change, merged into the current ones: nested objects such
    /// as `tls`, `ssh` and `options` merge too, and `null` clears an
    /// optional field (one that can be empty, such as `database` or `port`).
    /// Same field names as the record returns.
    pub changes: Map<String, Value>,
}

/// Arguments of `move_connection`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(transform = target_required)]
pub struct MoveConnectionParams {
    /// Connection id.
    pub id: String,
    /// Folder id, or `null` for the top level. Required: a left out
    /// target is refused rather than read as the top level.
    #[serde(default, deserialize_with = "present")]
    pub folder_id: Option<Option<String>>,
}

/// Arguments of `create_saved_query`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateQueryParams {
    /// The query. Required: `name` and `sql` (SQL, or Redis commands for a
    /// Redis connection). Optional: `folderId`, `connectionId` (the
    /// connection it opens with), `database` and `tags` (strings).
    pub query: Map<String, Value>,
}

/// The list `reorder` works on.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReorderList {
    /// Folders.
    Folders,
    /// Connections.
    Connections,
    /// Saved queries.
    Queries,
}

/// Arguments of `reorder`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReorderParams {
    /// Which list.
    pub kind: ReorderList,
    /// Ids of siblings (same parent folder) in their new order.
    pub ids: Vec<String>,
}

/// How much statement text the activity log keeps.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TextLevelParam {
    /// Statement kind, duration and row counts only.
    None,
    /// Text with literal values replaced by `?` (the default).
    Normalized,
    /// The full text, still passed through the secret redactor.
    Full,
}

/// Changes to the activity log settings.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivityChanges {
    /// How much statement text is kept.
    #[serde(default)]
    pub text_level: Option<TextLevelParam>,
    /// Days to keep events (1 to 36500), or `null` to keep them forever.
    /// Shortening it removes older events at once.
    #[serde(default, deserialize_with = "present")]
    pub retention_days: Option<Option<u32>>,
}

/// Arguments of `update_settings`; leave out what stays.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateSettingsParams {
    /// Activity log settings.
    #[serde(default)]
    pub activity: Option<ActivityChanges>,
    /// Keep each saved secret in memory after its first keychain read until
    /// Varve quits. Turning it off forgets the cached secrets.
    #[serde(default)]
    pub remember_secrets_for_session: Option<bool>,
    /// Check for updates automatically. Only for builds that update
    /// themselves.
    #[serde(default)]
    pub auto_check_updates: Option<bool>,
}

/// What `get_settings` returns.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    activity: ActivitySettings,
    remember_secrets_for_session: bool,
    /// `null` when this build does not update itself.
    auto_check_updates: Option<bool>,
}

/// Lists the target of a move (`parentId`, `folderId`) as required in the
/// tool's schema. The field has a serde default, so that a call without it
/// reaches the tool and gets [`missing_target`] as its answer; without
/// this, the default would make the schema call the field optional.
fn target_required(schema: &mut schemars::Schema) {
    for field in ["parentId", "folderId"] {
        let Some(property) = schema
            .get_mut("properties")
            .and_then(|properties| properties.get_mut(field))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        property.remove("default");
        let required = schema
            .ensure_object()
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(required) = required
            && !required.iter().any(|name| name == field)
        {
            required.push(Value::String(field.into()));
        }
    }
}

/// Tells a field given as `null` (`Some(None)`) from a missing one (`None`).
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[tool_router]
impl VarveTools {
    /// A tool set over `services`.
    #[must_use]
    pub fn new(services: Arc<Services>, host: Arc<dyn McpHost>) -> Self {
        Self {
            services,
            host,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "All folders, connection profiles and saved queries of the workspace. Profiles never contain passwords; `savedSecrets` lists which ones the keychain holds.",
        annotations(read_only_hint = true)
    )]
    async fn get_workspace(&self) -> Result<CallToolResult, ErrorData> {
        let services = Arc::clone(&self.services);
        Ok(outcome(run_blocking(move || services.tree()).await))
    }

    #[tool(
        description = "A folder and how many subfolders, connections and saved queries it holds, recursively.",
        annotations(read_only_hint = true)
    )]
    async fn get_folder(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let services = Arc::clone(&self.services);
        Ok(outcome(
            run_blocking(move || {
                let folder = services.records.folder(&p.id)?;
                let contents = services.folder_contents(&p.id)?;
                Ok(serde_json::json!({ "folder": folder, "contents": contents }))
            })
            .await,
        ))
    }

    #[tool(description = "Creates a folder.")]
    async fn create_folder(
        &self,
        Parameters(p): Parameters<CreateFolderParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let services = Arc::clone(&self.services);
        let result = run_blocking(move || {
            services.create_folder(p.parent_id.as_deref(), &p.name, p.color.as_deref())
        })
        .await;
        Ok(self.changed(result))
    }

    #[tool(
        description = "Renames a folder or changes its color.",
        annotations(idempotent_hint = true)
    )]
    async fn update_folder(
        &self,
        Parameters(p): Parameters<UpdateFolderParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let services = Arc::clone(&self.services);
        let result = run_blocking(move || {
            let current = services.records.folder(&p.id)?;
            let name = p.name.unwrap_or(current.name);
            let color = p.color.unwrap_or(current.color);
            services.update_folder(&p.id, &name, color.as_deref())
        })
        .await;
        Ok(self.changed(result))
    }

    #[tool(
        description = "Moves a folder into another folder or to the top level.",
        annotations(idempotent_hint = true)
    )]
    async fn move_folder(
        &self,
        Parameters(p): Parameters<MoveFolderParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Some(parent_id) = p.parent_id else {
            return Ok(failure(&missing_target("parentId")));
        };
        let services = Arc::clone(&self.services);
        let result = run_blocking(move || services.move_folder(&p.id, parent_id.as_deref())).await;
        Ok(self.changed(result))
    }

    #[tool(
        description = "Deletes a folder. `move_contents_to_parent` keeps what it holds; `delete_contents` also deletes every connection and saved query inside, with the connections' saved keychain entries, and closes the open sessions and tabs of those connections. Confirm with the user first.",
        annotations(destructive_hint = true)
    )]
    async fn delete_folder(
        &self,
        Parameters(p): Parameters<DeleteFolderParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let mode = match p.mode {
            DeleteMode::MoveContentsToParent => FolderDeleteMode::MoveContentsToParent,
            DeleteMode::DeleteContents => FolderDeleteMode::DeleteContents,
        };
        let result = self.services.delete_folder(&p.id, mode).await;
        Ok(self.changed(result.map(|()| serde_json::json!({ "deleted": p.id }))))
    }

    #[tool(
        description = "A connection profile, without passwords.",
        annotations(read_only_hint = true)
    )]
    async fn get_connection(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let services = Arc::clone(&self.services);
        Ok(outcome(
            run_blocking(move || Ok(services.records.connection(&p.id)?)).await,
        ))
    }

    #[tool(
        description = "Creates a connection profile. The user saves its password in Varve when connecting. Init SQL (`options.initSql`) and `options.confirmDestructive: false` are refused: the user sets those in Varve."
    )]
    async fn create_connection(
        &self,
        Parameters(p): Parameters<CreateConnectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let input = match parse_record::<ConnectionInput>(
            Value::Object(p.connection),
            &connection_template(),
        )
        .and_then(|input| {
            let created = Records::preview_connection(PERSONAL_WORKSPACE, &input)?;
            refuse_unsafe_change(&Safety::new_connection(), &Safety::of(&created))?;
            Ok(input)
        }) {
            Ok(input) => input,
            Err(error) => return Ok(failure(&error)),
        };
        let services = Arc::clone(&self.services);
        let result = run_blocking(move || services.create_connection(&input)).await;
        Ok(self.changed(result))
    }

    #[tool(
        description = "Changes fields of a connection profile. When the host, port, user or route (TLS, SSH) changes, Varve removes the saved passwords that no longer apply, exactly as when the user edits it. Init SQL (`options.initSql`) cannot be set, changed or cleared here, and the safety options can only be made stricter: turning `options.readOnly` or `options.confirmDestructive` off, or moving `environment` to a less strict one (production, staging, test, development), is refused. The user changes those in Varve.",
        annotations(idempotent_hint = true)
    )]
    async fn update_connection(
        &self,
        Parameters(p): Parameters<UpdateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let services = Arc::clone(&self.services);
        let id = p.id.clone();
        let current = match run_blocking(move || Ok(services.records.connection(&id)?)).await {
            Ok(current) => current,
            Err(error) => return Ok(failure(&error)),
        };
        let input = match merge_changes(
            &input_from_profile(&current),
            p.changes,
            &connection_template(),
        ) {
            Ok(input) => input,
            Err(error) => return Ok(failure(&error)),
        };
        // Checked again against the profile as stored, under its record
        // lock, so an edit in the window meanwhile cannot be undone around
        // the check.
        let result = self
            .services
            .update_connection_checked(&p.id, &input, |stored, edited| {
                refuse_unsafe_change(&Safety::of(stored), &Safety::of(edited))
            })
            .await;
        Ok(self.changed(result))
    }

    #[tool(
        description = "Moves a connection into a folder or to the top level.",
        annotations(idempotent_hint = true)
    )]
    async fn move_connection(
        &self,
        Parameters(p): Parameters<MoveConnectionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Some(folder_id) = p.folder_id else {
            return Ok(failure(&missing_target("folderId")));
        };
        let services = Arc::clone(&self.services);
        let result =
            run_blocking(move || services.move_connection(&p.id, folder_id.as_deref())).await;
        Ok(self.changed(result))
    }

    #[tool(description = "Copies a connection profile, without its saved passwords.")]
    async fn duplicate_connection(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let services = Arc::clone(&self.services);
        let result = run_blocking(move || services.duplicate_connection(&p.id)).await;
        Ok(self.changed(result))
    }

    #[tool(
        description = "Deletes a connection profile and its saved keychain entries. Its open sessions are closed and the window closes their tabs (asking about unsaved editor text first). Confirm with the user first.",
        annotations(destructive_hint = true)
    )]
    async fn delete_connection(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self.services.delete_connection(&p.id).await;
        Ok(self.changed(result.map(|()| serde_json::json!({ "deleted": p.id }))))
    }

    #[tool(
        description = "A saved query with its text.",
        annotations(read_only_hint = true)
    )]
    async fn get_saved_query(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let services = Arc::clone(&self.services);
        Ok(outcome(
            run_blocking(move || Ok(services.records.query(&p.id)?)).await,
        ))
    }

    #[tool(description = "Saves a query. It is stored, not run.")]
    async fn create_saved_query(
        &self,
        Parameters(p): Parameters<CreateQueryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let input = match parse_record::<SavedQueryInput>(Value::Object(p.query), &query_template())
        {
            Ok(input) => input,
            Err(error) => return Ok(failure(&error)),
        };
        let services = Arc::clone(&self.services);
        let result = run_blocking(move || services.create_query(&input)).await;
        Ok(self.changed(result))
    }

    #[tool(
        description = "Changes fields of a saved query.",
        annotations(idempotent_hint = true)
    )]
    async fn update_saved_query(
        &self,
        Parameters(p): Parameters<UpdateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let services = Arc::clone(&self.services);
        let result = run_blocking(move || {
            let current = services.records.query(&p.id)?;
            let input = merge_changes(&input_from_query(&current), p.changes, &query_template())?;
            services.update_query(&p.id, &input)
        })
        .await;
        Ok(self.changed(result))
    }

    #[tool(
        description = "Deletes a saved query.",
        annotations(destructive_hint = true)
    )]
    async fn delete_saved_query(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let services = Arc::clone(&self.services);
        let id = p.id.clone();
        let result = run_blocking(move || services.delete_query(&id)).await;
        Ok(self.changed(result.map(|()| serde_json::json!({ "deleted": p.id }))))
    }

    #[tool(
        description = "Sets the manual order of sibling folders, connections or saved queries.",
        annotations(idempotent_hint = true)
    )]
    async fn reorder(
        &self,
        Parameters(p): Parameters<ReorderParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let kind = match p.kind {
            ReorderList::Folders => ReorderKind::Folders,
            ReorderList::Connections => ReorderKind::Connections,
            ReorderList::Queries => ReorderKind::Queries,
        };
        let services = Arc::clone(&self.services);
        let result = run_blocking(move || services.reorder(kind, &p.ids)).await;
        Ok(self.changed(result.map(|()| serde_json::json!({ "reordered": true }))))
    }

    #[tool(
        description = "Varve's settings: the activity log (`activity.textLevel`, `activity.retentionDays`), whether saved secrets are remembered for the session, and automatic update checks.",
        annotations(read_only_hint = true)
    )]
    async fn get_settings(&self) -> Result<CallToolResult, ErrorData> {
        Ok(outcome(self.settings().await))
    }

    #[tool(
        description = "Changes settings; returns them all afterwards. Leave out what stays. Shortening `activity.retentionDays` deletes older activity events at once: confirm that with the user first.",
        annotations(destructive_hint = true, idempotent_hint = true)
    )]
    async fn update_settings(
        &self,
        Parameters(p): Parameters<UpdateSettingsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self.apply_settings(p).await;
        self.host.settings_changed();
        Ok(outcome(result))
    }

    #[tool(
        description = "Forgets the secrets Varve keeps in memory, so the next connection reads the keychain again. Returns how many were forgotten."
    )]
    async fn forget_cached_secrets(&self) -> Result<CallToolResult, ErrorData> {
        let forgotten = self.services.forget_cached_secrets();
        Ok(outcome(Ok(serde_json::json!({ "forgotten": forgotten }))))
    }
}

impl VarveTools {
    fn changed<T: Serialize>(&self, result: Result<T, CmdError>) -> CallToolResult {
        // Also after an error: a partly applied change (stored, keychain
        // cleanup pending) must show too.
        self.host.records_changed();
        outcome(result)
    }

    async fn settings(&self) -> Result<SettingsView, CmdError> {
        let services = Arc::clone(&self.services);
        let (activity, secrets) =
            run_blocking(move || Ok((services.activity_settings()?, services.secret_settings()?)))
                .await?;
        Ok(SettingsView {
            activity,
            remember_secrets_for_session: secrets.remember_for_session,
            auto_check_updates: self.host.update_auto_check().await?,
        })
    }

    async fn apply_settings(&self, p: UpdateSettingsParams) -> Result<SettingsView, CmdError> {
        if p.auto_check_updates.is_some() && self.host.update_auto_check().await?.is_none() {
            return Err(CmdError::Invalid {
                message: "This build of Varve does not update itself, so it has no automatic update checks."
                    .into(),
            });
        }
        let services = Arc::clone(&self.services);
        let (activity, remember) = (p.activity, p.remember_secrets_for_session);
        run_blocking(move || {
            if let Some(changes) = activity {
                let mut settings = services.activity_settings()?;
                if let Some(level) = changes.text_level {
                    settings.text_level = match level {
                        TextLevelParam::None => TextLevel::None,
                        TextLevelParam::Normalized => TextLevel::Normalized,
                        TextLevelParam::Full => TextLevel::Full,
                    };
                }
                if let Some(days) = changes.retention_days {
                    settings.retention_days = days;
                }
                services.set_activity_settings(settings)?;
            }
            if let Some(remember_for_session) = remember {
                services.set_secret_settings(SecretSettings {
                    remember_for_session,
                })?;
            }
            Ok(())
        })
        .await?;
        if let Some(auto_check) = p.auto_check_updates {
            self.host.set_update_auto_check(auto_check).await?;
        }
        self.settings().await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for VarveTools {
    fn get_info(&self) -> ServerConfig {
        let mut implementation = Implementation::new("varve", env!("CARGO_PKG_VERSION"));
        implementation.title = Some("Varve".into());
        implementation.website_url = Some("https://getvarve.com".into());
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(implementation)
            .with_instructions(INSTRUCTIONS)
    }
}

/// The JSON of `value` as text, and as structured content when it is an
/// object (MCP wants an object there).
fn outcome<T: Serialize>(result: Result<T, CmdError>) -> CallToolResult {
    let value = match result.map(|value| serde_json::to_value(value)) {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            return failure(&CmdError::Internal {
                message: format!("Could not encode the result: {error}"),
            });
        }
        Err(error) => return failure(&error),
    };
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    if value.is_object() {
        result.structured_content = Some(value);
    }
    result
}

/// A tool error the model reads: the error's kind and message, as the UI
/// gets them.
fn failure(error: &CmdError) -> CallToolResult {
    let value = serde_json::to_value(error).unwrap_or_else(
        |_| serde_json::json!({ "kind": "internal", "message": error.to_string() }),
    );
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| error.to_string());
    CallToolResult::error(vec![ContentBlock::text(text)])
}

/// The error for a move without its target field.
fn missing_target(field: &str) -> CmdError {
    CmdError::Invalid {
        message: format!(
            "Give `{field}`: a folder id, or `null` for the top level. It cannot be left out."
        ),
    }
}

/// What a connection runs on connect and how carefully it treats
/// statements: only the user may set init SQL or loosen these, in Varve.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Safety {
    init_sql: Option<String>,
    read_only: bool,
    confirm_destructive: bool,
    environment: Environment,
}

impl Safety {
    /// The values of a stored (validated, normalized) profile.
    fn of(profile: &ConnectionProfile) -> Self {
        Self {
            init_sql: profile.options.init_sql.clone(),
            read_only: profile.options.read_only,
            confirm_destructive: profile.options.confirm_destructive,
            environment: profile.environment,
        }
    }

    /// What a new connection starts from (the defaults of the input).
    fn new_connection() -> Self {
        let options = varve_records::ConnectionOptions::default();
        Self {
            init_sql: options.init_sql,
            read_only: options.read_only,
            confirm_destructive: options.confirm_destructive,
            environment: Environment::default(),
        }
    }
}

/// How strict an environment is: production is the strictest.
fn strictness(environment: Environment) -> u8 {
    match environment {
        Environment::Development => 0,
        Environment::Test => 1,
        Environment::Staging => 2,
        Environment::Production => 3,
    }
}

/// Refuses an MCP change from `before` to `after` that sets, changes or
/// clears init SQL (it runs on every connect, with the saved password and
/// without the destructive-statement confirmation), or that loosens a
/// safety option. Making them stricter is allowed.
fn refuse_unsafe_change(before: &Safety, after: &Safety) -> Result<(), CmdError> {
    if before.init_sql != after.init_sql {
        return Err(CmdError::Invalid {
            message: "Init SQL runs every time Varve connects, so MCP clients cannot set, change \
                      or clear it. Change it in Varve, in the connection's settings."
                .into(),
        });
    }
    let mut loosened = Vec::new();
    if before.read_only && !after.read_only {
        loosened.push("turning read-only off".to_string());
    }
    if before.confirm_destructive && !after.confirm_destructive {
        loosened.push("turning off the confirmation of destructive statements".to_string());
    }
    if strictness(after.environment) < strictness(before.environment) {
        loosened.push(format!(
            "changing the environment from {} to {}",
            before.environment.as_str(),
            after.environment.as_str()
        ));
    }
    if loosened.is_empty() {
        return Ok(());
    }
    Err(CmdError::Invalid {
        message: format!(
            "MCP clients can only make a connection's safety options stricter, so {} was \
             refused. Change it in Varve, in the connection's settings.",
            loosened.join(" and ")
        ),
    })
}

/// The editable fields of a profile, as the edit form starts from them.
fn input_from_profile(profile: &ConnectionProfile) -> ConnectionInput {
    ConnectionInput {
        folder_id: profile.folder_id.clone(),
        name: profile.name.clone(),
        color: profile.color.clone(),
        engine: profile.engine,
        host: profile.host.clone(),
        port: Some(profile.port),
        database: profile.database.clone(),
        username: profile.username.clone(),
        environment: profile.environment,
        tls: profile.tls.clone(),
        ssh: profile.ssh.clone(),
        options: profile.options.clone(),
    }
}

/// The editable fields of a saved query.
fn input_from_query(query: &SavedQuery) -> SavedQueryInput {
    SavedQueryInput {
        folder_id: query.folder_id.clone(),
        connection_id: query.connection_id.clone(),
        database: query.database.clone(),
        name: query.name.clone(),
        sql: query.sql.clone(),
        tags: query.tags.clone(),
    }
}

/// Every field a connection input can have, for the unknown-field check.
fn connection_template() -> Value {
    serde_json::to_value(ConnectionInput {
        folder_id: None,
        name: String::new(),
        color: None,
        engine: Engine::Postgres,
        host: String::new(),
        port: None,
        database: None,
        username: None,
        environment: varve_records::Environment::default(),
        tls: varve_records::TlsSettings::default(),
        ssh: varve_records::SshSettings::default(),
        options: varve_records::ConnectionOptions::default(),
    })
    .unwrap_or(Value::Null)
}

/// Every field a saved query input can have.
fn query_template() -> Value {
    serde_json::to_value(input_from_query(&SavedQuery {
        id: String::new(),
        workspace_id: String::new(),
        folder_id: None,
        connection_id: None,
        database: None,
        name: String::new(),
        sql: String::new(),
        tags: Vec::new(),
        sort_order: 0,
        created_at: 0,
        updated_at: 0,
        version: 0,
    }))
    .unwrap_or(Value::Null)
}

/// Merges `changes` into `current` (both the same input type) and reads
/// the result back. `template` is the type's empty input
/// ([`connection_template`]): it names the fields and says which may be
/// `null`.
fn merge_changes<T: Serialize + DeserializeOwned>(
    current: &T,
    changes: Map<String, Value>,
    template: &Value,
) -> Result<T, CmdError> {
    let mut merged = serde_json::to_value(current).map_err(|error| CmdError::Internal {
        message: format!("Could not encode the record: {error}"),
    })?;
    let changes = Value::Object(changes);
    check_known_fields(&changes, template, "")?;
    merge_patch(&mut merged, changes);
    serde_json::from_value(merged).map_err(|error| CmdError::Invalid {
        message: format!("Invalid changes: {error}"),
    })
}

/// Reads a new record, refusing fields `template` does not have.
fn parse_record<T: DeserializeOwned>(value: Value, template: &Value) -> Result<T, CmdError> {
    check_known_fields(&value, template, "")?;
    serde_json::from_value(value).map_err(|error| CmdError::Invalid {
        message: format!("Invalid record: {error}"),
    })
}

/// Refuses a key of `value` that `template` lacks, at any depth where the
/// template has an object, and `null` for a field that has no empty value
/// (`null` there would quietly become the default, such as a production
/// connection turning into a development one).
fn check_known_fields(value: &Value, template: &Value, path: &str) -> Result<(), CmdError> {
    let (Value::Object(fields), Value::Object(known)) = (value, template) else {
        return Ok(());
    };
    for (key, field) in fields {
        let Some(known_field) = known.get(key) else {
            let mut names: Vec<&str> = known.keys().map(String::as_str).collect();
            names.sort_unstable();
            return Err(CmdError::Invalid {
                message: format!(
                    "Unknown field `{path}{key}`. Known fields here: {}.",
                    names.join(", ")
                ),
            });
        };
        if field.is_null() && !known_field.is_null() {
            return Err(CmdError::Invalid {
                message: format!(
                    "`{path}{key}` cannot be null: give a value, or leave it out to keep the current one."
                ),
            });
        }
        check_known_fields(field, known_field, &format!("{path}{key}."))?;
    }
    Ok(())
}

/// JSON merge patch (RFC 7386).
fn merge_patch(target: &mut Value, patch: Value) {
    let Value::Object(patch) = patch else {
        *target = patch;
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(fields) = target {
        for (key, value) in patch {
            if value.is_null() {
                fields.remove(&key);
            } else {
                merge_patch(fields.entry(key).or_insert(Value::Null), value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_patch_merges_nested_objects_and_clears_nulls() {
        let mut target = serde_json::json!({ "a": 1, "b": { "c": 2, "d": 3 }, "e": "x" });
        merge_patch(
            &mut target,
            serde_json::json!({ "b": { "c": 9 }, "e": null, "f": [1] }),
        );
        assert_eq!(
            target,
            serde_json::json!({ "a": 1, "b": { "c": 9, "d": 3 }, "f": [1] })
        );
    }

    #[test]
    fn unknown_fields_are_named_with_their_path() {
        let template = serde_json::json!({ "name": "", "options": { "readOnly": false } });
        let error = check_known_fields(
            &serde_json::json!({ "options": { "readonly": true } }),
            &template,
            "",
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(error.contains("`options.readonly`"), "{error}");
        assert!(error.contains("readOnly"), "{error}");
    }

    fn safety(environment: Environment, read_only: bool, confirm: bool) -> Safety {
        Safety {
            init_sql: None,
            read_only,
            confirm_destructive: confirm,
            environment,
        }
    }

    #[test]
    fn init_sql_is_left_to_the_user() {
        let plain = Safety::new_connection();
        let with_sql = Safety {
            init_sql: Some("SET search_path = app".into()),
            ..plain.clone()
        };
        for (before, after) in [(&plain, &with_sql), (&with_sql, &plain)] {
            let error = refuse_unsafe_change(before, after)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(error.contains("Init SQL"), "{error}");
            assert!(error.contains("in Varve"), "{error}");
        }
        let changed = Safety {
            init_sql: Some("SET search_path = other".into()),
            ..plain.clone()
        };
        assert!(refuse_unsafe_change(&with_sql, &changed).is_err());
        assert!(refuse_unsafe_change(&with_sql, &with_sql.clone()).is_ok());
    }

    #[test]
    fn safety_options_only_get_stricter() {
        let prod = safety(Environment::Production, true, true);
        for looser in [
            safety(Environment::Production, false, true),
            safety(Environment::Production, true, false),
            safety(Environment::Staging, true, true),
            safety(Environment::Development, true, true),
        ] {
            let error = refuse_unsafe_change(&prod, &looser)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(error.contains("stricter"), "{looser:?}: {error}");
        }
        let error = refuse_unsafe_change(&prod, &safety(Environment::Test, false, true))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("read-only off"), "{error}");
        assert!(error.contains("from production to test"), "{error}");

        let dev = safety(Environment::Development, false, false);
        for stricter in [
            safety(Environment::Test, false, false),
            safety(Environment::Production, true, true),
            safety(Environment::Development, false, true),
        ] {
            assert!(
                refuse_unsafe_change(&dev, &stricter).is_ok(),
                "{stricter:?}"
            );
        }
        // A new connection may not start without the confirmation.
        assert!(
            refuse_unsafe_change(
                &Safety::new_connection(),
                &safety(Environment::Development, false, false)
            )
            .is_err()
        );
    }

    #[test]
    fn a_move_needs_its_target_and_known_fields() {
        let to_top: Result<MoveFolderParams, _> =
            serde_json::from_value(serde_json::json!({ "id": "f", "parentId": null }));
        assert!(matches!(
            to_top,
            Ok(MoveFolderParams {
                parent_id: Some(None),
                ..
            })
        ));
        let into: Result<MoveConnectionParams, _> =
            serde_json::from_value(serde_json::json!({ "id": "c", "folderId": "f" }));
        assert!(matches!(
            into,
            Ok(MoveConnectionParams { folder_id: Some(Some(ref id)), .. }) if id == "f"
        ));
        // A left out target reaches the tool as `None`, which answers with
        // `missing_target`; it is never read as the top level.
        let missing: Result<MoveFolderParams, _> =
            serde_json::from_value(serde_json::json!({ "id": "f" }));
        assert!(matches!(
            missing,
            Ok(MoveFolderParams {
                parent_id: None,
                ..
            })
        ));
        let missing: Result<MoveConnectionParams, _> =
            serde_json::from_value(serde_json::json!({ "id": "c" }));
        assert!(matches!(
            missing,
            Ok(MoveConnectionParams {
                folder_id: None,
                ..
            })
        ));
        assert!(matches!(
            missing_target("parentId"),
            CmdError::Invalid { message } if message.contains("`parentId`")
        ));
        // A misspelled target is an unknown field.
        assert!(
            serde_json::from_value::<MoveFolderParams>(
                serde_json::json!({ "id": "f", "parent_id": null })
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<IdParams>(serde_json::json!({ "id": "c", "force": true }))
                .is_err()
        );
        assert!(
            serde_json::from_value::<UpdateSettingsParams>(
                serde_json::json!({ "activity": { "retentionDay": 3 } })
            )
            .is_err()
        );
    }

    #[test]
    fn the_connection_template_lists_every_editable_field() {
        let template = connection_template();
        for key in [
            "folderId", "name", "engine", "host", "port", "tls", "ssh", "options",
        ] {
            assert!(template.get(key).is_some(), "missing {key}");
        }
        assert!(template["options"].get("readOnly").is_some());
    }
}
