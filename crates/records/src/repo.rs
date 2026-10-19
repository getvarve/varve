//! The records repository on top of `app.db`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
use varve_core::Engine;
use varve_driver::TlsMode;
use varve_store::{Store, StoreError};

use crate::model::{
    ConnectionInput, ConnectionOptions, ConnectionProfile, Environment, Folder, FolderContents,
    FolderDeleteMode, SavedQuery, SavedQueryInput, SecretKindName, SshAuthMethod, SshSettings,
    TlsSettings, default_port, engine_str, parse_engine,
};

/// Errors from the records repository.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// The record does not exist.
    #[error("{kind} `{id}` does not exist")]
    NotFound {
        /// Record kind.
        kind: &'static str,
        /// Id that was asked for.
        id: String,
    },
    /// Input failed validation. The message is shown to the user.
    #[error("{0}")]
    Invalid(String),
    /// A folder move would put a folder inside itself.
    #[error("a folder cannot be moved into itself or one of its subfolders")]
    Cycle,
    /// Storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A stored JSON column could not be read.
    #[error("stored record is corrupt: {0}")]
    Corrupt(String),
}

impl From<rusqlite::Error> for RecordError {
    fn from(error: rusqlite::Error) -> Self {
        RecordError::Store(StoreError::Sqlite(error))
    }
}

type Result<T> = std::result::Result<T, RecordError>;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn invalid(message: impl Into<String>) -> RecordError {
    RecordError::Invalid(message.into())
}

fn clean_name(name: &str, what: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid(format!("{what} needs a name.")));
    }
    if name.chars().count() > 200 {
        return Err(invalid(format!(
            "{what} names can be at most 200 characters."
        )));
    }
    Ok(name.to_string())
}

fn clean_color(color: Option<&str>) -> Result<Option<String>> {
    match color.map(str::trim).filter(|c| !c.is_empty()) {
        None => Ok(None),
        Some(c)
            if c.len() <= 32
                && c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') =>
        {
            Ok(Some(c.to_string()))
        }
        Some(_) => Err(invalid(
            "Colors are lowercase tokens such as `violet` or `red-500`.",
        )),
    }
}

fn opt_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
}

/// Validates and normalizes connection input.
fn validate_connection(input: &ConnectionInput) -> Result<ConnectionInput> {
    let mut out = input.clone();
    out.name = clean_name(&input.name, "A connection")?;
    out.color = clean_color(input.color.as_deref())?;
    out.host = input.host.trim().to_string();
    if out.host.is_empty() {
        return Err(invalid("Enter the host name or IP address."));
    }
    if out.host.len() > 255 || out.host.chars().any(char::is_whitespace) {
        return Err(invalid(
            "The host name cannot contain spaces and can be at most 255 characters.",
        ));
    }
    let port = input.port.unwrap_or_else(|| default_port(input.engine));
    if port == 0 {
        return Err(invalid("The port must be between 1 and 65535."));
    }
    out.port = Some(port);
    out.database = opt_text(input.database.as_deref());
    out.username = opt_text(input.username.as_deref());
    if input.engine == Engine::Redis && out.database.is_some() {
        return Err(invalid(
            "Redis has no database name; choose the logical database number instead.",
        ));
    }

    let tls = &input.tls;
    out.tls = TlsSettings {
        mode: tls.mode,
        ca_path: opt_text(tls.ca_path.as_deref()),
        client_cert_path: opt_text(tls.client_cert_path.as_deref()),
        client_key_path: opt_text(tls.client_key_path.as_deref()),
    };
    if out.tls.client_cert_path.is_some() != out.tls.client_key_path.is_some() {
        return Err(invalid(
            "A client certificate needs its key, and a key needs its certificate.",
        ));
    }
    if out.tls.mode == TlsMode::Disable
        && (out.tls.ca_path.is_some() || out.tls.client_cert_path.is_some())
    {
        return Err(invalid(
            "TLS is disabled, so remove the certificate files or pick another TLS mode.",
        ));
    }

    let ssh = &input.ssh;
    out.ssh = SshSettings {
        enabled: ssh.enabled,
        host: ssh.host.trim().to_string(),
        port: ssh.port,
        username: ssh.username.trim().to_string(),
        auth: ssh.auth,
        key_path: opt_text(ssh.key_path.as_deref()),
    };
    if out.ssh.enabled {
        if out.ssh.host.is_empty() || out.ssh.host.chars().any(char::is_whitespace) {
            return Err(invalid("Enter the SSH host."));
        }
        if out.ssh.port == 0 {
            return Err(invalid("The SSH port must be between 1 and 65535."));
        }
        if out.ssh.username.is_empty() {
            return Err(invalid("Enter the SSH user."));
        }
        if out.ssh.auth == SshAuthMethod::KeyFile && out.ssh.key_path.is_none() {
            return Err(invalid("Choose the SSH private key file."));
        }
    }

    let options = &input.options;
    if options
        .statement_timeout_ms
        .is_some_and(|ms| ms == 0 || ms > 86_400_000)
    {
        return Err(invalid(
            "The statement timeout must be between 1 ms and 24 hours.",
        ));
    }
    if options.init_sql.as_ref().is_some_and(|s| s.len() > 65_536) {
        return Err(invalid("Init SQL can be at most 64 KB."));
    }
    if input.engine != Engine::Redis && options.redis_db != 0 {
        return Err(invalid(
            "A Redis database number only applies to Redis connections.",
        ));
    }
    if input.engine == Engine::Redis && options.redis_db > 15 {
        return Err(invalid(
            "Redis database numbers go from 0 to 15 on a default server.",
        ));
    }
    out.options = ConnectionOptions {
        init_sql: opt_text(options.init_sql.as_deref()),
        ..options.clone()
    };
    if out.environment == Environment::Production {
        // Production always confirms destructive statements (design doc 3.2).
        out.options.confirm_destructive = true;
    }
    Ok(out)
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| RecordError::Corrupt(e.to_string()))
}

fn from_json<T: serde::de::DeserializeOwned>(text: &str, column: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(CorruptColumn(format!("{column}: {e}"))),
        )
    })
}

#[derive(Debug)]
struct CorruptColumn(String);

impl std::fmt::Display for CorruptColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CorruptColumn {}

const FOLDER_COLUMNS: &str =
    "id, workspace_id, parent_id, name, color, sort_order, created_at, updated_at, version";

fn folder_from_row(row: &Row<'_>) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        parent_id: row.get(2)?,
        name: row.get(3)?,
        color: row.get(4)?,
        sort_order: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        version: row.get(8)?,
    })
}

const CONNECTION_COLUMNS: &str = "id, workspace_id, folder_id, name, color, sort_order, engine, host, port, database, \
     username, environment, tls, ssh, options, saved_secrets, created_at, updated_at, version";

fn connection_from_row(row: &Row<'_>) -> rusqlite::Result<ConnectionProfile> {
    let engine: String = row.get(6)?;
    let environment: String = row.get(11)?;
    let port: i64 = row.get(8)?;
    Ok(ConnectionProfile {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        folder_id: row.get(2)?,
        name: row.get(3)?,
        color: row.get(4)?,
        sort_order: row.get(5)?,
        engine: parse_engine(&engine).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(CorruptColumn(format!("engine `{engine}`"))),
            )
        })?,
        host: row.get(7)?,
        port: u16::try_from(port).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Integer,
                Box::new(e),
            )
        })?,
        database: row.get(9)?,
        username: row.get(10)?,
        environment: Environment::parse(&environment).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                11,
                rusqlite::types::Type::Text,
                Box::new(CorruptColumn(format!("environment `{environment}`"))),
            )
        })?,
        tls: from_json(&row.get::<_, String>(12)?, "tls")?,
        ssh: from_json(&row.get::<_, String>(13)?, "ssh")?,
        options: from_json(&row.get::<_, String>(14)?, "options")?,
        saved_secrets: from_json(&row.get::<_, String>(15)?, "saved_secrets")?,
        created_at: row.get(16)?,
        updated_at: row.get(17)?,
        version: row.get(18)?,
    })
}

const QUERY_COLUMNS: &str = "id, workspace_id, folder_id, connection_id, database, name, sql, tags, sort_order, created_at, updated_at, version";

fn query_from_row(row: &Row<'_>) -> rusqlite::Result<SavedQuery> {
    Ok(SavedQuery {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        folder_id: row.get(2)?,
        connection_id: row.get(3)?,
        database: row.get(4)?,
        name: row.get(5)?,
        sql: row.get(6)?,
        tags: from_json(&row.get::<_, String>(7)?, "tags")?,
        sort_order: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        version: row.get(11)?,
    })
}

/// Which list [`Records::reorder`] works on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorderKind {
    /// Folders.
    Folders,
    /// Connections.
    Connections,
    /// Saved queries.
    Queries,
}

/// Repository for folders, connections and saved queries.
#[derive(Debug, Clone)]
pub struct Records {
    store: Arc<Store>,
}

impl Records {
    /// Creates the repository. The store must have the core migrations applied.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn run<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let mut outcome = None;
        self.store.with_conn(|conn| {
            outcome = Some(f(conn));
            Ok(())
        })?;
        outcome.unwrap_or_else(|| Err(invalid("the database call did not run")))
    }

    fn folder_exists(conn: &Connection, workspace_id: &str, id: &str) -> Result<()> {
        let found: Option<String> = conn
            .query_row(
                "SELECT workspace_id FROM core_folders WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        match found {
            Some(ws) if ws == workspace_id => Ok(()),
            Some(_) => Err(invalid("The folder belongs to another workspace.")),
            None => Err(RecordError::NotFound {
                kind: "folder",
                id: id.to_string(),
            }),
        }
    }

    fn next_sort_order(
        conn: &Connection,
        table: &str,
        parent_column: &str,
        workspace_id: &str,
        parent: Option<&str>,
    ) -> Result<i64> {
        let sql = format!(
            "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM {table} WHERE workspace_id = ?1 AND {parent_column} IS ?2"
        );
        Ok(conn.query_row(&sql, params![workspace_id, parent], |r| r.get(0))?)
    }

    // Folders

    /// Creates a folder.
    ///
    /// # Errors
    /// Invalid name or color, or a parent that does not exist in the workspace.
    pub fn create_folder(
        &self,
        workspace_id: &str,
        parent_id: Option<&str>,
        name: &str,
        color: Option<&str>,
    ) -> Result<Folder> {
        let name = clean_name(name, "A folder")?;
        let color = clean_color(color)?;
        self.run(|conn| {
            if let Some(parent) = parent_id {
                Self::folder_exists(conn, workspace_id, parent)?;
            }
            let now = now_ms();
            let folder = Folder {
                id: new_id(),
                workspace_id: workspace_id.to_string(),
                parent_id: parent_id.map(ToString::to_string),
                name,
                color,
                sort_order: Self::next_sort_order(conn, "core_folders", "parent_id", workspace_id, parent_id)?,
                created_at: now,
                updated_at: now,
                version: 1,
            };
            conn.execute(
                &format!("INSERT INTO core_folders ({FOLDER_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"),
                params![folder.id, folder.workspace_id, folder.parent_id, folder.name, folder.color, folder.sort_order, folder.created_at, folder.updated_at, folder.version],
            )?;
            Ok(folder)
        })
    }

    /// Reads a folder.
    ///
    /// # Errors
    /// [`RecordError::NotFound`] when it does not exist.
    pub fn folder(&self, id: &str) -> Result<Folder> {
        self.run(|conn| {
            conn.query_row(
                &format!("SELECT {FOLDER_COLUMNS} FROM core_folders WHERE id = ?1"),
                [id],
                folder_from_row,
            )
            .optional()?
            .ok_or_else(|| RecordError::NotFound {
                kind: "folder",
                id: id.to_string(),
            })
        })
    }

    /// All folders of a workspace, by parent then manual order.
    ///
    /// # Errors
    /// Storage errors.
    pub fn folders(&self, workspace_id: &str) -> Result<Vec<Folder>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {FOLDER_COLUMNS} FROM core_folders WHERE workspace_id = ?1 ORDER BY parent_id, sort_order, name"
            ))?;
            let rows = stmt.query_map([workspace_id], folder_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Renames a folder and sets its color.
    ///
    /// # Errors
    /// Invalid name or color, or a missing folder.
    pub fn update_folder(&self, id: &str, name: &str, color: Option<&str>) -> Result<Folder> {
        let name = clean_name(name, "A folder")?;
        let color = clean_color(color)?;
        self.run(|conn| {
            let changed = conn.execute(
                "UPDATE core_folders SET name = ?2, color = ?3, updated_at = ?4, version = version + 1 WHERE id = ?1",
                params![id, name, color, now_ms()],
            )?;
            if changed == 0 {
                return Err(RecordError::NotFound { kind: "folder", id: id.to_string() });
            }
            Ok(())
        })?;
        self.folder(id)
    }

    /// Moves a folder under another folder (or to the top level).
    ///
    /// # Errors
    /// [`RecordError::Cycle`] when the target is the folder itself or inside it,
    /// or the target is in another workspace.
    pub fn move_folder(&self, id: &str, new_parent: Option<&str>) -> Result<Folder> {
        let folder = self.folder(id)?;
        self.run(|conn| {
            if let Some(target) = new_parent {
                Self::folder_exists(conn, &folder.workspace_id, target)?;
                let mut cursor = Some(target.to_string());
                while let Some(current) = cursor {
                    if current == id {
                        return Err(RecordError::Cycle);
                    }
                    cursor = conn
                        .query_row("SELECT parent_id FROM core_folders WHERE id = ?1", [&current], |r| r.get::<_, Option<String>>(0))
                        .optional()?
                        .flatten();
                }
            }
            let order = Self::next_sort_order(conn, "core_folders", "parent_id", &folder.workspace_id, new_parent)?;
            conn.execute(
                "UPDATE core_folders SET parent_id = ?2, sort_order = ?3, updated_at = ?4, version = version + 1 WHERE id = ?1",
                params![id, new_parent, order, now_ms()],
            )?;
            Ok(())
        })?;
        self.folder(id)
    }

    /// Counts what a folder contains, recursively.
    ///
    /// # Errors
    /// A missing folder.
    pub fn folder_contents(&self, id: &str) -> Result<FolderContents> {
        self.folder(id)?;
        self.run(|conn| {
            let ids = Self::descendant_folders(conn, id)?;
            let mut contents = FolderContents {
                folders: (ids.len() as u64).saturating_sub(1),
                ..FolderContents::default()
            };
            for folder_id in &ids {
                let connections: i64 = conn.query_row(
                    "SELECT count(*) FROM core_connections WHERE folder_id = ?1",
                    [folder_id],
                    |r| r.get(0),
                )?;
                contents.connections += u64::try_from(connections).unwrap_or(0);
                let queries: i64 = conn.query_row(
                    "SELECT count(*) FROM core_saved_queries WHERE folder_id = ?1",
                    [folder_id],
                    |r| r.get(0),
                )?;
                contents.queries += u64::try_from(queries).unwrap_or(0);
            }
            Ok(contents)
        })
    }

    /// The folder and every folder below it.
    fn descendant_folders(conn: &Connection, id: &str) -> Result<Vec<String>> {
        let mut stmt = conn.prepare(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM core_folders WHERE id = ?1
                 UNION ALL
                 SELECT f.id FROM core_folders f JOIN tree t ON f.parent_id = t.id
             ) SELECT id FROM tree",
        )?;
        let ids = stmt
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(ids)
    }

    /// Deletes a folder. Returns the connections that were deleted, so the
    /// caller can remove their keychain entries (design doc 3.4).
    ///
    /// # Errors
    /// A missing folder or storage errors. Runs in one transaction.
    pub fn delete_folder(
        &self,
        id: &str,
        mode: FolderDeleteMode,
    ) -> Result<Vec<ConnectionProfile>> {
        let folder = self.folder(id)?;
        self.run(|conn| {
            let tx = conn.unchecked_transaction()?;
            let now = now_ms();
            let mut deleted = Vec::new();
            match mode {
                FolderDeleteMode::MoveContentsToParent => {
                    for table in ["core_connections", "core_saved_queries"] {
                        tx.execute(
                            &format!("UPDATE {table} SET folder_id = ?2, updated_at = ?3, version = version + 1 WHERE folder_id = ?1"),
                            params![id, folder.parent_id, now],
                        )?;
                    }
                    tx.execute(
                        "UPDATE core_folders SET parent_id = ?2, updated_at = ?3, version = version + 1 WHERE parent_id = ?1",
                        params![id, folder.parent_id, now],
                    )?;
                }
                FolderDeleteMode::DeleteContents => {
                    let ids = Self::descendant_folders(&tx, id)?;
                    for folder_id in &ids {
                        let mut stmt = tx.prepare(&format!("SELECT {CONNECTION_COLUMNS} FROM core_connections WHERE folder_id = ?1"))?;
                        deleted.extend(stmt.query_map([folder_id], connection_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?);
                        drop(stmt);
                        tx.execute("DELETE FROM core_saved_queries WHERE folder_id = ?1", [folder_id])?;
                        tx.execute("DELETE FROM core_connections WHERE folder_id = ?1", [folder_id])?;
                    }
                }
            }
            tx.execute("DELETE FROM core_folders WHERE id = ?1", [id])?;
            tx.commit()?;
            Ok(deleted)
        })
    }

    // Connections

    /// Creates a connection profile.
    ///
    /// # Errors
    /// Validation errors (shown to the user) or a missing folder.
    pub fn create_connection(
        &self,
        workspace_id: &str,
        input: &ConnectionInput,
    ) -> Result<ConnectionProfile> {
        let input = validate_connection(input)?;
        let id = new_id();
        self.run(|conn| {
            if let Some(folder) = input.folder_id.as_deref() {
                Self::folder_exists(conn, workspace_id, folder)?;
            }
            let now = now_ms();
            let order = Self::next_sort_order(conn, "core_connections", "folder_id", workspace_id, input.folder_id.as_deref())?;
            conn.execute(
                &format!("INSERT INTO core_connections ({CONNECTION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, '[]', ?16, ?16, 1)"),
                params![
                    id, workspace_id, input.folder_id, input.name, input.color, order, engine_str(input.engine), input.host,
                    i64::from(input.port.unwrap_or_else(|| default_port(input.engine))), input.database, input.username,
                    input.environment.as_str(), to_json(&input.tls)?, to_json(&input.ssh)?, to_json(&input.options)?, now
                ],
            )?;
            Ok(())
        })?;
        self.connection(&id)
    }

    /// Reads a connection profile.
    ///
    /// # Errors
    /// [`RecordError::NotFound`] when it does not exist.
    pub fn connection(&self, id: &str) -> Result<ConnectionProfile> {
        self.run(|conn| {
            conn.query_row(
                &format!("SELECT {CONNECTION_COLUMNS} FROM core_connections WHERE id = ?1"),
                [id],
                connection_from_row,
            )
            .optional()?
            .ok_or_else(|| RecordError::NotFound {
                kind: "connection",
                id: id.to_string(),
            })
        })
    }

    /// All connection profiles of a workspace.
    ///
    /// # Errors
    /// Storage errors.
    pub fn connections(&self, workspace_id: &str) -> Result<Vec<ConnectionProfile>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CONNECTION_COLUMNS} FROM core_connections WHERE workspace_id = ?1 ORDER BY folder_id, sort_order, name"
            ))?;
            let rows = stmt.query_map([workspace_id], connection_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Replaces the editable fields of a profile.
    ///
    /// # Errors
    /// Validation errors, a missing profile or folder.
    pub fn update_connection(
        &self,
        id: &str,
        input: &ConnectionInput,
    ) -> Result<ConnectionProfile> {
        let existing = self.connection(id)?;
        let input = validate_connection(input)?;
        self.run(|conn| {
            if let Some(folder) = input.folder_id.as_deref() {
                Self::folder_exists(conn, &existing.workspace_id, folder)?;
            }
            conn.execute(
                "UPDATE core_connections SET folder_id = ?2, name = ?3, color = ?4, engine = ?5, host = ?6, port = ?7, database = ?8,
                     username = ?9, environment = ?10, tls = ?11, ssh = ?12, options = ?13, updated_at = ?14, version = version + 1
                 WHERE id = ?1",
                params![
                    id, input.folder_id, input.name, input.color, engine_str(input.engine), input.host,
                    i64::from(input.port.unwrap_or_else(|| default_port(input.engine))), input.database, input.username,
                    input.environment.as_str(), to_json(&input.tls)?, to_json(&input.ssh)?, to_json(&input.options)?, now_ms()
                ],
            )?;
            Ok(())
        })?;
        self.connection(id)
    }

    /// Moves a connection to a folder (or the top level).
    ///
    /// # Errors
    /// A missing profile or folder.
    pub fn move_connection(&self, id: &str, folder_id: Option<&str>) -> Result<ConnectionProfile> {
        let existing = self.connection(id)?;
        self.run(|conn| {
            if let Some(folder) = folder_id {
                Self::folder_exists(conn, &existing.workspace_id, folder)?;
            }
            let order = Self::next_sort_order(conn, "core_connections", "folder_id", &existing.workspace_id, folder_id)?;
            conn.execute(
                "UPDATE core_connections SET folder_id = ?2, sort_order = ?3, updated_at = ?4, version = version + 1 WHERE id = ?1",
                params![id, folder_id, order, now_ms()],
            )?;
            Ok(())
        })?;
        self.connection(id)
    }

    /// Copies a profile next to the original. Secrets are not copied.
    ///
    /// # Errors
    /// A missing profile.
    pub fn duplicate_connection(&self, id: &str) -> Result<ConnectionProfile> {
        let p = self.connection(id)?;
        let copy_name = format!("{} copy", p.name);
        let input = ConnectionInput {
            folder_id: p.folder_id,
            name: copy_name.chars().take(200).collect(),
            color: p.color,
            engine: p.engine,
            host: p.host,
            port: Some(p.port),
            database: p.database,
            username: p.username,
            environment: p.environment,
            tls: p.tls,
            ssh: p.ssh,
            options: p.options,
        };
        self.create_connection(&p.workspace_id, &input)
    }

    /// Records which secrets this device saved in its keychain.
    ///
    /// # Errors
    /// A missing profile.
    pub fn set_saved_secrets(
        &self,
        id: &str,
        kinds: &[SecretKindName],
    ) -> Result<ConnectionProfile> {
        let mut unique = kinds.to_vec();
        unique.dedup();
        self.run(|conn| {
            let changed = conn.execute(
                "UPDATE core_connections SET saved_secrets = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, to_json(&unique)?, now_ms()],
            )?;
            if changed == 0 {
                return Err(RecordError::NotFound {
                    kind: "connection",
                    id: id.to_string(),
                });
            }
            Ok(())
        })?;
        self.connection(id)
    }

    /// Deletes a profile and returns it, so the caller can remove its
    /// keychain entries. Saved queries attached to it stay, detached.
    ///
    /// # Errors
    /// A missing profile.
    pub fn delete_connection(&self, id: &str) -> Result<ConnectionProfile> {
        let profile = self.connection(id)?;
        self.run(|conn| {
            conn.execute("DELETE FROM core_connections WHERE id = ?1", [id])?;
            Ok(())
        })?;
        Ok(profile)
    }

    // Saved queries

    fn validate_query(
        conn: &Connection,
        workspace_id: &str,
        input: &SavedQueryInput,
    ) -> Result<SavedQueryInput> {
        let mut out = input.clone();
        out.name = clean_name(&input.name, "A saved query")?;
        if input.sql.trim().is_empty() {
            return Err(invalid("A saved query needs some SQL."));
        }
        if input.sql.len() > 1_048_576 {
            return Err(invalid("Saved queries can be at most 1 MB."));
        }
        out.database = opt_text(input.database.as_deref());
        out.tags = input
            .tags
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        out.tags.sort();
        out.tags.dedup();
        if let Some(folder) = input.folder_id.as_deref() {
            Self::folder_exists(conn, workspace_id, folder)?;
        }
        if let Some(connection) = input.connection_id.as_deref() {
            let ws: Option<String> = conn
                .query_row(
                    "SELECT workspace_id FROM core_connections WHERE id = ?1",
                    [connection],
                    |r| r.get(0),
                )
                .optional()?;
            match ws {
                Some(ws) if ws == workspace_id => {}
                Some(_) => return Err(invalid("The connection belongs to another workspace.")),
                None => {
                    return Err(RecordError::NotFound {
                        kind: "connection",
                        id: connection.to_string(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Saves a new query.
    ///
    /// # Errors
    /// Validation errors or missing folder or connection.
    pub fn create_query(&self, workspace_id: &str, input: &SavedQueryInput) -> Result<SavedQuery> {
        let id = new_id();
        self.run(|conn| {
            let input = Self::validate_query(conn, workspace_id, input)?;
            let now = now_ms();
            let order = Self::next_sort_order(conn, "core_saved_queries", "folder_id", workspace_id, input.folder_id.as_deref())?;
            conn.execute(
                &format!("INSERT INTO core_saved_queries ({QUERY_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, 1)"),
                params![id, workspace_id, input.folder_id, input.connection_id, input.database, input.name, input.sql, to_json(&input.tags)?, order, now],
            )?;
            Ok(())
        })?;
        self.query(&id)
    }

    /// Reads a saved query.
    ///
    /// # Errors
    /// [`RecordError::NotFound`] when it does not exist.
    pub fn query(&self, id: &str) -> Result<SavedQuery> {
        self.run(|conn| {
            conn.query_row(
                &format!("SELECT {QUERY_COLUMNS} FROM core_saved_queries WHERE id = ?1"),
                [id],
                query_from_row,
            )
            .optional()?
            .ok_or_else(|| RecordError::NotFound {
                kind: "saved query",
                id: id.to_string(),
            })
        })
    }

    /// All saved queries of a workspace.
    ///
    /// # Errors
    /// Storage errors.
    pub fn queries(&self, workspace_id: &str) -> Result<Vec<SavedQuery>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {QUERY_COLUMNS} FROM core_saved_queries WHERE workspace_id = ?1 ORDER BY folder_id, sort_order, name"
            ))?;
            let rows = stmt.query_map([workspace_id], query_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Updates a saved query.
    ///
    /// # Errors
    /// Validation errors or a missing query.
    pub fn update_query(&self, id: &str, input: &SavedQueryInput) -> Result<SavedQuery> {
        let existing = self.query(id)?;
        self.run(|conn| {
            let input = Self::validate_query(conn, &existing.workspace_id, input)?;
            conn.execute(
                "UPDATE core_saved_queries SET folder_id = ?2, connection_id = ?3, database = ?4, name = ?5, sql = ?6, tags = ?7,
                     updated_at = ?8, version = version + 1 WHERE id = ?1",
                params![id, input.folder_id, input.connection_id, input.database, input.name, input.sql, to_json(&input.tags)?, now_ms()],
            )?;
            Ok(())
        })?;
        self.query(id)
    }

    /// Deletes a saved query.
    ///
    /// # Errors
    /// A missing query.
    pub fn delete_query(&self, id: &str) -> Result<()> {
        self.query(id)?;
        self.run(|conn| {
            conn.execute("DELETE FROM core_saved_queries WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    /// Sets the manual order of siblings: `ids` in their new order. Every id
    /// must exist and share the same parent.
    ///
    /// # Errors
    /// Ids that do not exist or have different parents.
    pub fn reorder(&self, kind: ReorderKind, ids: &[String]) -> Result<()> {
        let (table, parent) = match kind {
            ReorderKind::Folders => ("core_folders", "parent_id"),
            ReorderKind::Connections => ("core_connections", "folder_id"),
            ReorderKind::Queries => ("core_saved_queries", "folder_id"),
        };
        self.run(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut parent_of_first: Option<Option<String>> = None;
            for (position, id) in ids.iter().enumerate() {
                let p: Option<Option<String>> = tx
                    .query_row(
                        &format!("SELECT {parent} FROM {table} WHERE id = ?1"),
                        [id],
                        |r| r.get(0),
                    )
                    .optional()?;
                let Some(p) = p else {
                    return Err(RecordError::NotFound {
                        kind: "record",
                        id: id.clone(),
                    });
                };
                match &parent_of_first {
                    None => parent_of_first = Some(p),
                    Some(first) if *first == p => {}
                    Some(_) => {
                        return Err(invalid(
                            "Only items in the same folder can be reordered together.",
                        ));
                    }
                }
                tx.execute(
                    &format!("UPDATE {table} SET sort_order = ?2 WHERE id = ?1"),
                    params![id, i64::try_from(position).unwrap_or(i64::MAX)],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }
}
