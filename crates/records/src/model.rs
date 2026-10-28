//! Record types. Field names follow design doc 3.1.

use serde::{Deserialize, Serialize};
use varve_core::Engine;
use varve_driver::TlsMode;
use varve_secrets::SecretKind;

/// The workspace every record belongs to in M1 (team workspaces arrive with the cloud backend).
pub const PERSONAL_WORKSPACE: &str = "personal";

/// Environment tag of a connection (design doc 3.2). Production is red
/// everywhere and gets destructive-statement confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    /// Local or personal development.
    #[default]
    Development,
    /// Test.
    Test,
    /// Staging.
    Staging,
    /// Production.
    Production,
}

impl Environment {
    /// Stored text value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Environment::Development => "development",
            Environment::Test => "test",
            Environment::Staging => "staging",
            Environment::Production => "production",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "development" => Environment::Development,
            "test" => Environment::Test,
            "staging" => Environment::Staging,
            "production" => Environment::Production,
            _ => return None,
        })
    }
}

/// A folder in the workspace tree. Folders nest to any depth and hold any
/// mix of connections and saved queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    /// Id (UUID v7).
    pub id: String,
    /// Workspace.
    pub workspace_id: String,
    /// Parent folder; `None` for the top level.
    pub parent_id: Option<String>,
    /// Name.
    pub name: String,
    /// Optional color token.
    pub color: Option<String>,
    /// Manual order among siblings.
    pub sort_order: i64,
    /// Created, unix ms.
    pub created_at: i64,
    /// Last change, unix ms.
    pub updated_at: i64,
    /// Increments on every change (used by sync later).
    pub version: i64,
}

/// TLS settings of a profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TlsSettings {
    /// Mode.
    pub mode: TlsMode,
    /// CA certificate path (device specific).
    pub ca_path: Option<String>,
    /// Client certificate path (device specific).
    pub client_cert_path: Option<String>,
    /// Client key path (device specific).
    pub client_key_path: Option<String>,
}

/// How the SSH tunnel authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshAuthMethod {
    /// Password from the keychain.
    #[default]
    Password,
    /// Private key file, passphrase from the keychain.
    KeyFile,
    /// The SSH agent.
    Agent,
}

/// SSH tunnel settings of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SshSettings {
    /// Tunnel through SSH.
    pub enabled: bool,
    /// SSH host.
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// SSH user.
    pub username: String,
    /// Authentication.
    pub auth: SshAuthMethod,
    /// Private key path (device specific).
    pub key_path: Option<String>,
}

impl Default for SshSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 22,
            username: String::new(),
            auth: SshAuthMethod::Password,
            key_path: None,
        }
    }
}

/// Behavior options of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConnectionOptions {
    /// Open sessions read-only.
    pub read_only: bool,
    /// Ask before destructive statements (always on for production).
    pub confirm_destructive: bool,
    /// Server-side statement timeout in milliseconds.
    pub statement_timeout_ms: Option<u64>,
    /// TCP keepalive in seconds.
    pub keepalive_s: Option<u32>,
    /// SQL or Redis commands run after connecting.
    pub init_sql: Option<String>,
    /// Show system schemas and objects in the tree.
    pub show_system_objects: bool,
    /// Redis logical database.
    pub redis_db: u16,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            confirm_destructive: true,
            statement_timeout_ms: None,
            keepalive_s: None,
            init_sql: None,
            show_system_objects: false,
            redis_db: 0,
        }
    }
}

/// A connection profile. It never contains secrets; `saved_secrets` only
/// says which secret kinds this device keeps in its keychain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionProfile {
    /// Id (UUID v7).
    pub id: String,
    /// Workspace.
    pub workspace_id: String,
    /// Folder; `None` for the top level.
    pub folder_id: Option<String>,
    /// Name.
    pub name: String,
    /// Optional color token.
    pub color: Option<String>,
    /// Manual order among siblings.
    pub sort_order: i64,
    /// Engine.
    pub engine: Engine,
    /// Host.
    pub host: String,
    /// Port.
    pub port: u16,
    /// Database (PostgreSQL) or default schema (MySQL, MariaDB).
    pub database: Option<String>,
    /// User name.
    pub username: Option<String>,
    /// Environment tag.
    pub environment: Environment,
    /// TLS.
    pub tls: TlsSettings,
    /// SSH tunnel.
    pub ssh: SshSettings,
    /// Options.
    pub options: ConnectionOptions,
    /// Secret kinds saved in this device's keychain.
    pub saved_secrets: Vec<SecretKindName>,
    /// Created, unix ms.
    pub created_at: i64,
    /// Last change, unix ms.
    pub updated_at: i64,
    /// Increments on every change.
    pub version: i64,
}

/// Serializable name of a [`SecretKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKindName {
    /// Database password.
    DbPassword,
    /// SSH password.
    SshPassword,
    /// SSH key passphrase.
    SshKeyPassphrase,
    /// TLS key passphrase.
    TlsKeyPassphrase,
}

impl From<SecretKindName> for SecretKind {
    fn from(value: SecretKindName) -> Self {
        match value {
            SecretKindName::DbPassword => SecretKind::DbPassword,
            SecretKindName::SshPassword => SecretKind::SshPassword,
            SecretKindName::SshKeyPassphrase => SecretKind::SshKeyPassphrase,
            SecretKindName::TlsKeyPassphrase => SecretKind::TlsKeyPassphrase,
        }
    }
}

/// Fields a user edits for a connection. Ids, timestamps and versions are
/// managed by the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionInput {
    /// Folder.
    pub folder_id: Option<String>,
    /// Name.
    pub name: String,
    /// Color token.
    pub color: Option<String>,
    /// Engine.
    pub engine: Engine,
    /// Host.
    pub host: String,
    /// Port; the engine default when `None`.
    pub port: Option<u16>,
    /// Database or default schema.
    pub database: Option<String>,
    /// User name.
    pub username: Option<String>,
    /// Environment.
    #[serde(default)]
    pub environment: Environment,
    /// TLS.
    #[serde(default)]
    pub tls: TlsSettings,
    /// SSH tunnel.
    #[serde(default)]
    pub ssh: SshSettings,
    /// Options.
    #[serde(default)]
    pub options: ConnectionOptions,
}

/// A saved query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedQuery {
    /// Id (UUID v7).
    pub id: String,
    /// Workspace.
    pub workspace_id: String,
    /// Folder; `None` for the top level.
    pub folder_id: Option<String>,
    /// Connection it is attached to, if any.
    pub connection_id: Option<String>,
    /// Database it runs in.
    pub database: Option<String>,
    /// Name.
    pub name: String,
    /// SQL or Redis commands.
    pub sql: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Manual order among siblings.
    pub sort_order: i64,
    /// Created, unix ms.
    pub created_at: i64,
    /// Last change, unix ms.
    pub updated_at: i64,
    /// Increments on every change.
    pub version: i64,
}

/// Fields a user edits for a saved query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedQueryInput {
    /// Folder.
    pub folder_id: Option<String>,
    /// Attached connection.
    pub connection_id: Option<String>,
    /// Database.
    pub database: Option<String>,
    /// Name.
    pub name: String,
    /// SQL text.
    pub sql: String,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// What to do with a folder's contents when it is deleted (design doc 3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderDeleteMode {
    /// Move subfolders, connections and queries to the parent folder (default).
    MoveContentsToParent,
    /// Delete everything inside, recursively.
    DeleteContents,
}

/// Counts of what a folder contains, recursively (for the delete dialog).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderContents {
    /// Subfolders.
    pub folders: u64,
    /// Connections.
    pub connections: u64,
    /// Saved queries.
    pub queries: u64,
}

/// Default port of an engine.
#[must_use]
pub fn default_port(engine: Engine) -> u16 {
    match engine {
        Engine::Postgres => 5432,
        Engine::Mysql | Engine::Mariadb => 3306,
        Engine::Redis => 6379,
    }
}

pub(crate) fn engine_str(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "postgres",
        Engine::Mysql => "mysql",
        Engine::Mariadb => "mariadb",
        Engine::Redis => "redis",
    }
}

pub(crate) fn parse_engine(value: &str) -> Option<Engine> {
    Some(match value {
        "postgres" => Engine::Postgres,
        "mysql" => Engine::Mysql,
        "mariadb" => Engine::Mariadb,
        "redis" => Engine::Redis,
        _ => return None,
    })
}
