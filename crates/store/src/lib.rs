//! Local storage for Varve: the `app.db` SQLite file.
//!
//! Migrations are grouped in **namespaces**. The app owns the `core`
//! namespace; every extension owns exactly one namespace equal to its id
//! (design doc section 2.3). Each namespace keeps its own migration history
//! table, so one `app.db` holds them all: an extension's tables stay on disk,
//! unused, when that extension is no longer installed.
//!
//! Tables created by an extension must be prefixed with the extension id in
//! SQL form (`acme.stats` creates `acme_stats_*` tables). [`MigrationSet::validate`]
//! enforces this before anything touches the database.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use refinery::Migration;
use rusqlite::Connection;
use varve_core::DottedId;

/// The namespace owned by the app itself.
pub const CORE_NAMESPACE: &str = "core";

/// `PRAGMA auto_vacuum` values.
const AUTO_VACUUM_NONE: i64 = 0;
const AUTO_VACUUM_INCREMENTAL: i64 = 2;

/// Errors from the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite returned an error.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A migration failed to apply.
    #[error("migration error in namespace `{namespace}`: {source}")]
    Migration {
        /// Namespace whose migration failed.
        namespace: String,
        /// Underlying refinery error.
        #[source]
        source: Box<refinery::Error>,
    },
    /// A migration set broke the namespacing rules.
    #[error("invalid migration set `{namespace}`: {reason}")]
    InvalidMigrationSet {
        /// Namespace of the offending set.
        namespace: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The same namespace was registered twice.
    #[error("migration namespace `{0}` is registered more than once")]
    DuplicateNamespace(String),
    /// The connection mutex was poisoned by a panic in another thread.
    #[error("the database connection is unusable after a panic in another thread")]
    Poisoned,
}

/// One schema migration: a version, a name and the SQL to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaMigration {
    /// Version number, unique within its namespace, starting at 1.
    pub version: u32,
    /// Short snake_case name, for example `create_meta`.
    pub name: String,
    /// SQL to execute. May contain several statements.
    pub sql: String,
}

impl SchemaMigration {
    /// Creates a migration.
    #[must_use]
    pub fn new(version: u32, name: impl Into<String>, sql: impl Into<String>) -> Self {
        Self {
            version,
            name: name.into(),
            sql: sql.into(),
        }
    }
}

/// All migrations of one namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationSet {
    namespace: DottedId,
    migrations: Vec<SchemaMigration>,
}

impl MigrationSet {
    /// Creates a set for `namespace`.
    #[must_use]
    pub fn new(namespace: DottedId, migrations: Vec<SchemaMigration>) -> Self {
        Self {
            namespace,
            migrations,
        }
    }

    /// The namespace this set belongs to.
    #[must_use]
    pub fn namespace(&self) -> &DottedId {
        &self.namespace
    }

    /// The migrations in this set.
    #[must_use]
    pub fn migrations(&self) -> &[SchemaMigration] {
        &self.migrations
    }

    /// Name of the history table that tracks this namespace.
    #[must_use]
    pub fn history_table(&self) -> String {
        format!("_migrations_{}", self.namespace.to_sql_prefix())
    }

    /// Checks versions and table naming before the set is applied.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidMigrationSet`] when versions are not unique
    /// and positive, a name is not snake_case, or (outside the core namespace)
    /// a created table, index, view or trigger is not prefixed with the
    /// namespace in SQL form.
    pub fn validate(&self) -> Result<(), StoreError> {
        let invalid = |reason: String| StoreError::InvalidMigrationSet {
            namespace: self.namespace.to_string(),
            reason,
        };
        let mut versions: Vec<u32> = self.migrations.iter().map(|m| m.version).collect();
        versions.sort_unstable();
        if versions.first() == Some(&0) {
            return Err(invalid("migration versions start at 1".into()));
        }
        if versions.windows(2).any(|w| w[0] == w[1]) {
            return Err(invalid("migration versions must be unique".into()));
        }
        for migration in &self.migrations {
            let name_ok = !migration.name.is_empty()
                && migration
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if !name_ok {
                return Err(invalid(format!(
                    "migration name `{}` must be snake_case",
                    migration.name
                )));
            }
            if self.namespace.as_str() != CORE_NAMESPACE {
                let prefix = format!("{}_", self.namespace.to_sql_prefix());
                for object in created_objects(&migration.sql) {
                    if !object.starts_with(&prefix) {
                        return Err(invalid(format!(
                            "migration V{} creates `{object}`, which must start with `{prefix}`",
                            migration.version
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn to_refinery(&self) -> Result<Vec<Migration>, StoreError> {
        self.migrations
            .iter()
            .map(|m| {
                Migration::unapplied(&format!("V{}__{}", m.version, m.name), &m.sql).map_err(|e| {
                    StoreError::Migration {
                        namespace: self.namespace.to_string(),
                        source: Box::new(e),
                    }
                })
            })
            .collect()
    }
}

/// Returns the lowercase names of tables, indexes, views and triggers that
/// `sql` creates. Quotes and `IF NOT EXISTS` are handled; schema-qualified
/// names keep only the object name.
fn created_objects(sql: &str) -> Vec<String> {
    let tokens: Vec<String> = sql
        .split(|c: char| c.is_whitespace() || c == '(' || c == ';')
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut names = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == "create" {
            let mut j = i + 1;
            while j < tokens.len()
                && matches!(
                    tokens[j].as_str(),
                    "temp" | "temporary" | "unique" | "virtual"
                )
            {
                j += 1;
            }
            if j < tokens.len()
                && matches!(tokens[j].as_str(), "table" | "index" | "view" | "trigger")
            {
                j += 1;
                if j + 2 < tokens.len()
                    && tokens[j] == "if"
                    && tokens[j + 1] == "not"
                    && tokens[j + 2] == "exists"
                {
                    j += 3;
                }
                if let Some(raw) = tokens.get(j) {
                    let unquoted = raw.trim_matches(|c| matches!(c, '"' | '`' | '[' | ']'));
                    let name = unquoted.rsplit('.').next().unwrap_or(unquoted);
                    names.push(
                        name.trim_matches(|c| matches!(c, '"' | '`' | '[' | ']'))
                            .to_string(),
                    );
                }
                i = j;
            }
        }
        i += 1;
    }
    names
}

/// The local activity log (design doc 9.5). Kept forever by default.
const CORE_V2_ACTIVITY: &str = "
CREATE TABLE core_activity (
    id            TEXT    PRIMARY KEY NOT NULL,
    occurred_at   INTEGER NOT NULL,
    actor_id      TEXT    NOT NULL,
    device_id     TEXT,
    workspace_id  TEXT,
    connection_id TEXT,
    server_key    TEXT,
    category      TEXT    NOT NULL,
    action        TEXT    NOT NULL,
    outcome       TEXT    NOT NULL CHECK (outcome IN ('success', 'failure', 'cancelled')),
    duration_ms   INTEGER,
    target        TEXT,
    details       TEXT    NOT NULL DEFAULT '{}',
    upload_state  TEXT    NOT NULL DEFAULT 'local_only'
                  CHECK (upload_state IN ('local_only', 'pending', 'uploaded'))
);
CREATE INDEX core_activity_time ON core_activity (occurred_at);
CREATE INDEX core_activity_conn_time ON core_activity (connection_id, occurred_at);
CREATE INDEX core_activity_cat_time ON core_activity (category, occurred_at);
";

/// Folders, connection profiles and saved queries (design doc 3.1 and 3.4).
/// Secrets are never stored here; `saved_secrets` only lists which secret
/// kinds this device has in its keychain.
const CORE_V3_RECORDS: &str = "
CREATE TABLE core_folders (
    id           TEXT    PRIMARY KEY NOT NULL,
    workspace_id TEXT    NOT NULL DEFAULT 'personal',
    parent_id    TEXT    REFERENCES core_folders (id) ON DELETE CASCADE,
    name         TEXT    NOT NULL CHECK (length(trim(name)) > 0),
    color        TEXT,
    sort_order   INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    version      INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX core_folders_parent ON core_folders (workspace_id, parent_id, sort_order);

CREATE TABLE core_connections (
    id            TEXT    PRIMARY KEY NOT NULL,
    workspace_id  TEXT    NOT NULL DEFAULT 'personal',
    folder_id     TEXT    REFERENCES core_folders (id) ON DELETE SET NULL,
    name          TEXT    NOT NULL CHECK (length(trim(name)) > 0),
    color         TEXT,
    sort_order    INTEGER NOT NULL DEFAULT 0,
    engine        TEXT    NOT NULL CHECK (engine IN ('postgres', 'mysql', 'mariadb', 'redis')),
    host          TEXT    NOT NULL,
    port          INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    database      TEXT,
    username      TEXT,
    environment   TEXT    NOT NULL DEFAULT 'development'
                  CHECK (environment IN ('development', 'test', 'staging', 'production')),
    tls           TEXT    NOT NULL DEFAULT '{}',
    ssh           TEXT    NOT NULL DEFAULT '{}',
    options       TEXT    NOT NULL DEFAULT '{}',
    saved_secrets TEXT    NOT NULL DEFAULT '[]',
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    version       INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX core_connections_folder ON core_connections (workspace_id, folder_id, sort_order);

CREATE TABLE core_saved_queries (
    id            TEXT    PRIMARY KEY NOT NULL,
    workspace_id  TEXT    NOT NULL DEFAULT 'personal',
    folder_id     TEXT    REFERENCES core_folders (id) ON DELETE SET NULL,
    connection_id TEXT    REFERENCES core_connections (id) ON DELETE SET NULL,
    database      TEXT,
    name          TEXT    NOT NULL CHECK (length(trim(name)) > 0),
    sql           TEXT    NOT NULL,
    tags          TEXT    NOT NULL DEFAULT '[]',
    sort_order    INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    version       INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX core_saved_queries_folder ON core_saved_queries (workspace_id, folder_id, sort_order);
CREATE INDEX core_saved_queries_connection ON core_saved_queries (connection_id);
";

/// Migrations of the core namespace, owned by the app.
#[must_use]
pub fn core_migrations() -> MigrationSet {
    let namespace =
        DottedId::new(CORE_NAMESPACE).unwrap_or_else(|_| unreachable!("`core` is a valid id"));
    MigrationSet::new(
        namespace,
        vec![
            SchemaMigration::new(
                1,
                "create_meta",
                "CREATE TABLE core_meta (
                key   TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO core_meta (key, value) VALUES ('created_at', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));",
            ),
            SchemaMigration::new(2, "create_activity", CORE_V2_ACTIVITY),
            SchemaMigration::new(3, "create_folders_connections_queries", CORE_V3_RECORDS),
        ],
    )
}

/// Handle to `app.db`.
///
/// The connection is guarded by a mutex; keep closures passed to
/// [`Store::with_conn`] short and never hold the guard across an `.await`.
#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
}

/// How far [`Store::reclaim_free_space`] may go to shrink the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reclaim {
    /// Only work that takes time in proportion to the freed pages. A file
    /// from before incremental auto-vacuum keeps its free pages.
    Quick,
    /// Also rebuild a file from before incremental auto-vacuum with a
    /// one-time `VACUUM`, which holds the store for as long as copying all
    /// live data takes. For actions the user started and waits for.
    Rebuild,
}

impl Store {
    /// Opens (or creates) the database file at `path`.
    ///
    /// # Errors
    /// Returns [`StoreError::Sqlite`] when the file cannot be opened or the
    /// pragmas fail.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::configure(Connection::open(path)?)
    }

    /// Opens a private in-memory database, for tests.
    ///
    /// # Errors
    /// Returns [`StoreError::Sqlite`] when SQLite fails to initialise.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::configure(Connection::open_in_memory()?)
    }

    fn configure(conn: Connection) -> Result<Self, StoreError> {
        // rusqlite already opens with this timeout; stated first so every
        // pragma below waits for a lock another process holds on the file
        // instead of failing at once with "database is locked".
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Deleted rows are overwritten with zeros, so text the user removed
        // (a cleared activity log) cannot be read back from free pages.
        conn.pragma_update(None, "secure_delete", "ON")?;
        // Free pages can be handed back to the file system without a full
        // VACUUM. A new file gets this mode at once; an existing file keeps
        // its mode until its next VACUUM (see [`Store::reclaim_free_space`]).
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Hands the pages freed by a large delete back to the file system and
    /// empties the write-ahead log, so `app.db` shrinks and no old page
    /// versions stay behind in `app.db-wal`. Call it with the connection
    /// from [`Store::with_conn`], after the delete was committed and outside
    /// any transaction.
    ///
    /// A file in incremental auto-vacuum mode releases its free pages at
    /// once, in time that grows with the number of freed pages.
    ///
    /// A file created before that mode was set can only be shrunk by a
    /// `VACUUM`, which copies all live data (and switches the file to that
    /// mode, so it happens at most once per file). The store has one
    /// connection behind one lock, so for as long as the copy takes (seconds
    /// for a log of several hundred megabytes) every other store call waits:
    /// recording events, loading profiles. That is why the caller chooses:
    /// with [`Reclaim::Rebuild`] (an action the user started and waits for,
    /// such as clearing the activity log) the file is rebuilt when at least
    /// a quarter of its pages are free, so a small delete never pays for a
    /// full rebuild; with [`Reclaim::Quick`] (background work such as the
    /// retention prune) it is never rebuilt and its free pages, already
    /// zeroed, stay in the file until a later call with
    /// [`Reclaim::Rebuild`].
    ///
    /// # Errors
    /// Returns [`StoreError::Sqlite`] when a pragma or the `VACUUM` fails,
    /// for example inside an open transaction or on a full disk. The delete
    /// before it is committed either way: treat a failure as "space not
    /// released yet", not as a failed delete.
    pub fn reclaim_free_space(conn: &Connection, reclaim: Reclaim) -> Result<(), StoreError> {
        let auto_vacuum: i64 = conn.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
        match auto_vacuum {
            AUTO_VACUUM_INCREMENTAL => {
                // Each step of the pragma frees one page: run it to the end.
                let mut stmt = conn.prepare("PRAGMA incremental_vacuum")?;
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
            }
            AUTO_VACUUM_NONE if reclaim == Reclaim::Rebuild => {
                let free: i64 = conn.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
                let pages: i64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
                if free > 0 && free.saturating_mul(4) >= pages {
                    conn.execute_batch("VACUUM")?;
                }
            }
            // Full auto-vacuum already truncates the file at every commit;
            // an older file is left as it is by a quick reclaim.
            _ => {}
        }
        let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        if busy != 0 {
            // Another process is reading an older snapshot; the log is
            // emptied by a later checkpoint once that reader is done.
            tracing::warn!(
                "the write-ahead log could not be emptied while another reader is active"
            );
        }
        Ok(())
    }

    /// Validates every set, then applies the core set first and the others in
    /// the given order. Already applied migrations are skipped.
    ///
    /// # Errors
    /// Returns an error when a set is invalid, a namespace repeats, or a
    /// migration fails. Validation runs for all sets before any SQL executes.
    pub fn migrate(&self, sets: &[MigrationSet]) -> Result<(), StoreError> {
        let mut seen = std::collections::BTreeSet::new();
        for set in sets {
            set.validate()?;
            if !seen.insert(set.namespace().clone()) {
                return Err(StoreError::DuplicateNamespace(set.namespace().to_string()));
            }
        }
        let mut ordered: Vec<&MigrationSet> = sets.iter().collect();
        ordered.sort_by_key(|s| s.namespace().as_str() != CORE_NAMESPACE);

        let mut conn = self.lock()?;
        for set in ordered {
            let migrations = set.to_refinery()?;
            let mut runner = refinery::Runner::new(&migrations).set_abort_divergent(true);
            runner.set_migration_table_name(set.history_table());
            let report = runner.run(&mut *conn).map_err(|e| StoreError::Migration {
                namespace: set.namespace().to_string(),
                source: Box::new(e),
            })?;
            tracing::info!(
                namespace = %set.namespace(),
                applied = report.applied_migrations().len(),
                "migrations up to date"
            );
        }
        Ok(())
    }

    /// Runs `f` with the connection.
    ///
    /// # Errors
    /// Returns [`StoreError::Poisoned`] if another thread panicked while
    /// holding the connection, or whatever `f` returns.
    pub fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let conn = self.lock()?;
        f(&conn)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.conn.lock().map_err(|_| StoreError::Poisoned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> DottedId {
        DottedId::new(value).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn core_migrations_apply_and_are_idempotent() -> Result<(), StoreError> {
        let store = Store::open_in_memory()?;
        store.migrate(&[core_migrations()])?;
        store.migrate(&[core_migrations()])?;
        let created: String = store.with_conn(|c| {
            Ok(c.query_row(
                "SELECT value FROM core_meta WHERE key = 'created_at'",
                [],
                |r| r.get(0),
            )?)
        })?;
        assert!(created.ends_with('Z'));
        Ok(())
    }

    #[test]
    fn extension_set_gets_its_own_history_table() -> Result<(), StoreError> {
        let store = Store::open_in_memory()?;
        let ext = MigrationSet::new(
            id("acme.test"),
            vec![SchemaMigration::new(
                1,
                "create_pings",
                "CREATE TABLE acme_test_pings (id INTEGER PRIMARY KEY)",
            )],
        );
        store.migrate(&[ext, core_migrations()])?;
        let tables: Vec<String> = store.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
            let rows = stmt
                .query_map([], |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })?;
        for expected in [
            "_migrations_core",
            "_migrations_acme_test",
            "core_meta",
            "acme_test_pings",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing {expected} in {tables:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn extension_tables_must_be_prefixed() {
        let set = MigrationSet::new(
            id("acme.test"),
            vec![SchemaMigration::new(
                1,
                "bad",
                "CREATE TABLE IF NOT EXISTS \"pings\" (id INTEGER)",
            )],
        );
        let err = set
            .validate()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("must start with `acme_test_`"), "{err}");
    }

    #[test]
    fn prefix_rule_covers_indexes_views_and_triggers() {
        for sql in [
            "CREATE UNIQUE INDEX other_idx ON acme_test_pings (id)",
            "CREATE VIEW v AS SELECT 1",
            "CREATE TEMP TRIGGER t AFTER INSERT ON acme_test_pings BEGIN SELECT 1; END",
        ] {
            let set = MigrationSet::new(id("acme.test"), vec![SchemaMigration::new(1, "x", sql)]);
            assert!(set.validate().is_err(), "{sql} should be rejected");
        }
    }

    #[test]
    fn versions_must_be_unique_and_positive() {
        let dup = MigrationSet::new(
            id("acme.test"),
            vec![
                SchemaMigration::new(1, "a", "SELECT 1"),
                SchemaMigration::new(1, "b", "SELECT 1"),
            ],
        );
        assert!(dup.validate().is_err());
        let zero = MigrationSet::new(
            id("acme.test"),
            vec![SchemaMigration::new(0, "a", "SELECT 1")],
        );
        assert!(zero.validate().is_err());
    }

    #[test]
    fn duplicate_namespaces_are_rejected_before_any_sql_runs() -> Result<(), StoreError> {
        let store = Store::open_in_memory()?;
        let result = store.migrate(&[core_migrations(), core_migrations()]);
        assert!(matches!(result, Err(StoreError::DuplicateNamespace(_))));
        let tables: i64 = store.with_conn(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )?)
        })?;
        assert_eq!(tables, 0);
        Ok(())
    }

    /// A directory of its own under the system temp dir, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> std::io::Result<Self> {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "varve-store-{label}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pragma(store: &Store, name: &str) -> Result<i64, StoreError> {
        store.with_conn(|c| Ok(c.pragma_query_value(None, name, |r| r.get(0))?))
    }

    #[test]
    fn new_files_delete_securely_and_vacuum_incrementally() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = TempDir::new("pragmas")?;
        let store = Store::open(&dir.0.join("app.db"))?;
        store.migrate(&[core_migrations()])?;
        assert_eq!(pragma(&store, "busy_timeout")?, 5000);
        assert_eq!(pragma(&store, "foreign_keys")?, 1);
        assert_eq!(pragma(&store, "secure_delete")?, 1);
        assert_eq!(pragma(&store, "auto_vacuum")?, AUTO_VACUUM_INCREMENTAL);
        let mode: String =
            store.with_conn(|c| Ok(c.pragma_query_value(None, "journal_mode", |r| r.get(0))?))?;
        assert_eq!(mode, "wal");
        Ok(())
    }

    /// Fills `t` with rows big enough to take many pages, then deletes
    /// them all.
    fn fill_and_empty(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS t (v TEXT);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
             INSERT INTO t SELECT 'reclaim-marker-' || hex(randomblob(256)) FROM n;",
        )?;
        conn.execute_batch("DELETE FROM t")
    }

    #[test]
    fn reclaim_shrinks_new_files_incrementally() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new("incremental")?;
        let path = dir.0.join("app.db");
        let store = Store::open(&path)?;
        store.with_conn(|c| Ok(fill_and_empty(c)?))?;
        assert!(pragma(&store, "freelist_count")? > 100);
        // Releasing the pages of a file in incremental mode is quick work.
        store.with_conn(|c| Store::reclaim_free_space(c, Reclaim::Quick))?;
        assert_eq!(pragma(&store, "freelist_count")?, 0);
        assert!(pragma(&store, "page_count")? < 10);
        assert_eq!(std::fs::metadata(dir.0.join("app.db-wal"))?.len(), 0);
        Ok(())
    }

    #[test]
    fn reclaim_converts_an_older_file_once_enough_is_free() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = TempDir::new("convert")?;
        let path = dir.0.join("app.db");
        {
            // A file from before incremental auto-vacuum.
            let old = Connection::open(&path)?;
            old.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE keep (v TEXT);")?;
        }
        let store = Store::open(&path)?;
        assert_eq!(pragma(&store, "auto_vacuum")?, AUTO_VACUUM_NONE);

        // Little free space: no rebuild, the mode stays.
        store.with_conn(|c| {
            c.execute_batch(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
                 INSERT INTO keep SELECT hex(randomblob(256)) FROM n;
                 DELETE FROM keep WHERE rowid <= 10;",
            )?;
            Ok(())
        })?;
        store.with_conn(|c| Store::reclaim_free_space(c, Reclaim::Rebuild))?;
        assert_eq!(pragma(&store, "auto_vacuum")?, AUTO_VACUUM_NONE);
        assert!(pragma(&store, "freelist_count")? > 0);

        // Most of the file free, but a quick reclaim never rebuilds: the
        // free pages stay and only the write-ahead log is emptied.
        store.with_conn(|c| Ok(c.execute_batch("DELETE FROM keep")?))?;
        let free = pragma(&store, "freelist_count")?;
        store.with_conn(|c| Store::reclaim_free_space(c, Reclaim::Quick))?;
        assert_eq!(pragma(&store, "auto_vacuum")?, AUTO_VACUUM_NONE);
        assert_eq!(pragma(&store, "freelist_count")?, free);
        assert_eq!(std::fs::metadata(dir.0.join("app.db-wal"))?.len(), 0);

        // With a rebuild allowed: one VACUUM, which also switches the mode.
        store.with_conn(|c| Store::reclaim_free_space(c, Reclaim::Rebuild))?;
        assert_eq!(pragma(&store, "auto_vacuum")?, AUTO_VACUUM_INCREMENTAL);
        assert_eq!(pragma(&store, "freelist_count")?, 0);
        assert_eq!(std::fs::metadata(dir.0.join("app.db-wal"))?.len(), 0);
        Ok(())
    }

    #[test]
    fn opening_waits_for_a_lock_held_by_another_process() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = TempDir::new("busy")?;
        let path = dir.0.join("app.db");
        // Another instance holds an exclusive lock on a file that is not
        // in WAL mode yet, so switching the journal mode has to wait.
        let other = Connection::open(&path)?;
        other.execute_batch(
            "CREATE TABLE t (v INTEGER); BEGIN EXCLUSIVE; INSERT INTO t VALUES (1);",
        )?;
        let opener = std::thread::spawn({
            let path = path.clone();
            move || Store::open(&path)
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        other.execute_batch("COMMIT")?;
        let store = opener.join().map_err(|_| "the opening thread panicked")??;
        let mode: String =
            store.with_conn(|c| Ok(c.pragma_query_value(None, "journal_mode", |r| r.get(0))?))?;
        assert_eq!(mode, "wal");
        Ok(())
    }

    #[test]
    fn created_objects_parses_common_forms() {
        let sql =
            "create table if not exists main.\"acme_a\" (x int); CREATE INDEX acme_b ON t(x);";
        assert_eq!(
            created_objects(sql),
            vec!["acme_a".to_string(), "acme_b".to_string()]
        );
    }
}
