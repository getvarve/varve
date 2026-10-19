//! Local storage for Varve: the `app.db` SQLite file.
//!
//! Migrations are grouped in **namespaces**. The app owns the `core`
//! namespace; every extension owns exactly one namespace equal to its id
//! (design doc section 2.3). Each namespace keeps its own migration history
//! table, so Community and Pro can share one `app.db`: Pro tables stay on disk,
//! unused, when the user goes back to Community.
//!
//! Tables created by an extension must be prefixed with the extension id in
//! SQL form (`pro.stats` creates `pro_stats_*` tables). [`MigrationSet::validate`]
//! enforces this before anything touches the database.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use refinery::Migration;
use rusqlite::Connection;
use varve_core::DottedId;

/// The namespace owned by the app itself.
pub const CORE_NAMESPACE: &str = "core";

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

/// Migrations of the core namespace, owned by the app.
#[must_use]
pub fn core_migrations() -> MigrationSet {
    let namespace =
        DottedId::new(CORE_NAMESPACE).unwrap_or_else(|_| unreachable!("`core` is a valid id"));
    MigrationSet::new(
        namespace,
        vec![SchemaMigration::new(
            1,
            "create_meta",
            "CREATE TABLE core_meta (
                key   TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO core_meta (key, value) VALUES ('created_at', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));",
        )],
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
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
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
            id("pro.test"),
            vec![SchemaMigration::new(
                1,
                "create_pings",
                "CREATE TABLE pro_test_pings (id INTEGER PRIMARY KEY)",
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
            "_migrations_pro_test",
            "core_meta",
            "pro_test_pings",
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
            id("pro.test"),
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
        assert!(err.contains("must start with `pro_test_`"), "{err}");
    }

    #[test]
    fn prefix_rule_covers_indexes_views_and_triggers() {
        for sql in [
            "CREATE UNIQUE INDEX other_idx ON pro_test_pings (id)",
            "CREATE VIEW v AS SELECT 1",
            "CREATE TEMP TRIGGER t AFTER INSERT ON pro_test_pings BEGIN SELECT 1; END",
        ] {
            let set = MigrationSet::new(id("pro.test"), vec![SchemaMigration::new(1, "x", sql)]);
            assert!(set.validate().is_err(), "{sql} should be rejected");
        }
    }

    #[test]
    fn versions_must_be_unique_and_positive() {
        let dup = MigrationSet::new(
            id("pro.test"),
            vec![
                SchemaMigration::new(1, "a", "SELECT 1"),
                SchemaMigration::new(1, "b", "SELECT 1"),
            ],
        );
        assert!(dup.validate().is_err());
        let zero = MigrationSet::new(
            id("pro.test"),
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

    #[test]
    fn created_objects_parses_common_forms() {
        let sql = "create table if not exists main.\"pro_a\" (x int); CREATE INDEX pro_b ON t(x);";
        assert_eq!(
            created_objects(sql),
            vec!["pro_a".to_string(), "pro_b".to_string()]
        );
    }
}
