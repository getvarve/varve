//! Object browser commands (design doc 4.2 and 10): capabilities, the lazy
//! object tree, object details and DDL, plus per-database PostgreSQL
//! sessions.
//!
//! The commands take the user's session id but run on the metadata
//! session of its profile and database ([`super::metadata`]), never on the
//! user's session itself.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use varve_catalog::{
    Capabilities, Catalog, CatalogError, CatalogResult, Container, KindCount, ObjectDetails,
    ObjectKind, ObjectRef, ObjectSummary, open_catalog,
};
use varve_core::Engine;
use varve_driver::Session;

use crate::commands::{CmdError, SecretInput, Services};
use crate::sessions::{SessionError, SessionInfo, SessionLock};

type CmdResult<T> = Result<T, CmdError>;

/// The future of one catalog call.
type CatalogFuture<'a, T> = Pin<Box<dyn Future<Output = CatalogResult<T>> + Send + 'a>>;

impl Services {
    /// Locks a metadata session and returns it with its catalog, opening
    /// the catalog on first use. The catalog is opened under the lock because opening a
    /// MySQL or MariaDB catalog probes the server.
    async fn catalog_session(
        &self,
        session_id: &str,
    ) -> CmdResult<(SessionLock, Arc<dyn Catalog>)> {
        let lock = self.lock(session_id).await?;
        if let Some(catalog) = self.catalogs.get(session_id) {
            return Ok((lock, catalog));
        }
        let session = Self::locked(&lock, session_id)?;
        let catalog: Arc<dyn Catalog> = Arc::from(open_catalog(session).await?);
        self.catalogs.insert(session_id, Arc::clone(&catalog));
        Ok((lock, catalog))
    }

    /// `include_system` as asked, or the profile's "show system objects"
    /// option when the UI leaves it out.
    fn include_system(&self, session_id: &str, asked: Option<bool>) -> CmdResult<bool> {
        if let Some(asked) = asked {
            return Ok(asked);
        }
        let info = self.session_info(session_id)?;
        Ok(self
            .records
            .connection(&info.connection_id)
            .is_ok_and(|p| p.options.show_system_objects))
    }

    /// Runs a catalog call on the metadata session serving `session_id`.
    /// When it fails with a server error and the metadata connection no
    /// longer answers (lost, killed, timed out), the session is reopened
    /// and the call repeated once.
    async fn with_catalog<T, F>(&self, session_id: &str, call: F) -> CmdResult<T>
    where
        F: for<'a> Fn(&'a dyn Catalog, &'a dyn Session) -> CatalogFuture<'a, T>,
    {
        let mut retried = false;
        loop {
            let meta = self.metadata_session(session_id).await?;
            let (lock, catalog) = match self.catalog_session(&meta).await {
                Ok(found) => found,
                Err(CmdError::Session {
                    error: SessionError::NotFound { .. },
                }) if !retried => {
                    retried = true;
                    self.discard_metadata(&meta).await;
                    continue;
                }
                Err(other) => return Err(other),
            };
            let session = Self::locked(&lock, &meta)?;
            let result = call(catalog.as_ref(), session).await;
            if let Err(CatalogError::Driver { .. }) = &result
                && !retried
                && session.ping().await.is_err()
            {
                drop(lock);
                retried = true;
                self.discard_metadata(&meta).await;
                continue;
            }
            return Ok(result?);
        }
    }

    /// What the server supports: object kinds, editable kinds and named
    /// features (design doc 4.1).
    ///
    /// # Errors
    /// Unknown session, Redis (`catalog` / `unsupported`), errors opening
    /// the metadata session, driver errors.
    pub async fn catalog_capabilities(&self, session_id: &str) -> CmdResult<Capabilities> {
        self.with_catalog(session_id, |catalog, _| {
            let capabilities = catalog.capabilities().clone();
            Box::pin(async move { Ok(capabilities) })
        })
        .await
    }

    /// Databases on the server.
    ///
    /// # Errors
    /// Unknown session or catalog errors.
    pub async fn catalog_databases(&self, session_id: &str) -> CmdResult<Vec<ObjectSummary>> {
        self.with_catalog(session_id, |catalog, session| catalog.databases(session))
            .await
    }

    /// PostgreSQL schemas of a database (empty for MySQL and MariaDB).
    ///
    /// # Errors
    /// Unknown session or catalog errors.
    pub async fn catalog_schemas(
        &self,
        session_id: &str,
        database: &str,
    ) -> CmdResult<Vec<ObjectSummary>> {
        self.with_catalog(session_id, |catalog, session| {
            let database = database.to_string();
            Box::pin(async move { catalog.schemas(session, &database).await })
        })
        .await
    }

    /// The object kinds in a container, with counts.
    ///
    /// # Errors
    /// Unknown session or catalog errors.
    pub async fn catalog_kinds(
        &self,
        session_id: &str,
        container: &Container,
        include_system: Option<bool>,
    ) -> CmdResult<Vec<KindCount>> {
        let include_system = self.include_system(session_id, include_system)?;
        self.with_catalog(session_id, |catalog, session| {
            let container = container.clone();
            Box::pin(async move { catalog.kinds(session, &container, include_system).await })
        })
        .await
    }

    /// The objects of one kind in a container.
    ///
    /// # Errors
    /// Unknown session or catalog errors.
    pub async fn catalog_list(
        &self,
        session_id: &str,
        container: &Container,
        kind: ObjectKind,
        include_system: Option<bool>,
    ) -> CmdResult<Vec<ObjectSummary>> {
        let include_system = self.include_system(session_id, include_system)?;
        self.with_catalog(session_id, |catalog, session| {
            let container = container.clone();
            Box::pin(async move {
                catalog
                    .list(session, &container, kind, include_system)
                    .await
            })
        })
        .await
    }

    /// Full details of one object.
    ///
    /// # Errors
    /// Unknown session or catalog errors (`notFound` for a dropped object).
    pub async fn catalog_describe(
        &self,
        session_id: &str,
        object: &ObjectRef,
    ) -> CmdResult<ObjectDetails> {
        self.with_catalog(session_id, |catalog, session| {
            let object = object.clone();
            Box::pin(async move { catalog.describe(session, &object).await })
        })
        .await
    }

    /// The DDL that creates an object.
    ///
    /// # Errors
    /// Unknown session or catalog errors.
    pub async fn catalog_ddl(&self, session_id: &str, object: &ObjectRef) -> CmdResult<String> {
        self.with_catalog(session_id, |catalog, session| {
            let object = object.clone();
            Box::pin(async move { catalog.ddl(session, &object).await })
        })
        .await
    }

    /// The storage engines a MySQL or MariaDB server offers for new tables
    /// (`information_schema.ENGINES`, design doc 4.2), for the table
    /// designer's engine picker. Runs on the metadata session.
    ///
    /// # Errors
    /// Unknown session, PostgreSQL and Redis (`catalog` / `unsupported`),
    /// errors opening the metadata session, driver errors.
    pub async fn catalog_engines(
        &self,
        session_id: &str,
    ) -> CmdResult<Vec<varve_catalog::mysql::MyEngine>> {
        self.with_catalog(session_id, |_, session| {
            Box::pin(async move { varve_catalog::mysql::engines(session).await })
        })
        .await
    }

    /// The exact number of rows of a MySQL or MariaDB table, counted on the
    /// metadata session (not the user's tab, not in its query history) under
    /// a server-side cap of [`varve_catalog::mysql::ROW_COUNT_CAP_MS`];
    /// `None` when the server stopped the count at the cap. The table
    /// designer shows it with an engine change that loses rows.
    ///
    /// # Errors
    /// Unknown session, PostgreSQL and Redis (`catalog` / `unsupported`),
    /// errors opening the metadata session, driver errors other than the
    /// timeout.
    pub async fn catalog_row_count(
        &self,
        session_id: &str,
        database: &str,
        table: &str,
    ) -> CmdResult<Option<u64>> {
        self.with_catalog(session_id, |_, session| {
            let database = database.to_owned();
            let table = table.to_owned();
            Box::pin(async move {
                varve_catalog::mysql::row_count(
                    session,
                    &database,
                    &table,
                    varve_catalog::mysql::ROW_COUNT_CAP_MS,
                )
                .await
            })
        })
        .await
    }

    /// What dropping an object affects ([`super::impact::DropImpact`]):
    /// its dependents, what it holds, and for a database the other
    /// sessions connected to it and what makes the server refuse the drop.
    /// Runs on the metadata session, like the other catalog calls.
    ///
    /// # Errors
    /// Unknown session, Redis (`catalog` / `unsupported`), a reference
    /// without its schema or database (`catalog` / `invalid`), driver
    /// errors.
    pub async fn catalog_drop_impact(
        &self,
        session_id: &str,
        object: &ObjectRef,
    ) -> CmdResult<super::impact::DropImpact> {
        let info = self.session_info(session_id)?;
        let server = info.server.clone();
        let mut impact = self
            .with_catalog(session_id, |_, session| {
                let object = object.clone();
                let server = server.clone();
                Box::pin(async move { super::impact::drop_impact(session, &server, &object).await })
            })
            .await?;
        if object.kind == ObjectKind::Database {
            // This app's tabs on the database: the drop (WITH FORCE) ends
            // them, or leaves them without their database. Matched on the
            // server each session reached when it connected, never on what
            // the profiles say now (two tunnels on one local port, an
            // edited connection).
            impact.app_sessions = self
                .sessions
                .list()
                .into_iter()
                .filter(|s| s.id != info.id && s.database.as_deref() == Some(object.name.as_str()))
                .filter_map(|s| {
                    let relation = on_same_server(&info, &s)?;
                    let state = self.sessions.transaction_state_now(&s.id);
                    Some(super::impact::AppSession {
                        session_id: s.id,
                        busy: state.is_none(),
                        open_transaction: state.flatten(),
                        certain: relation != ServerMatch::Possible,
                        standby: relation == ServerMatch::Standby,
                    })
                })
                .collect();
        }
        // The drop runs on the user's session: with `sql_log_bin` off there
        // it is not written to the binary log and does not replicate.
        if matches!(info.engine, Engine::Mysql | Engine::Mariadb)
            && impact.notes.iter().any(|n| n == super::impact::BINLOG_NOTE)
            && self
                .sessions
                .int_now(&info.id, "SELECT @@session.sql_log_bin AS v")
                .await
                == Some(0)
        {
            impact.notes.retain(|n| n != super::impact::BINLOG_NOTE);
        }
        Ok(impact)
    }

    /// Extensions the PostgreSQL server can install, for the install
    /// picker: name, default and available versions, description, and the
    /// installed version and schema in the tab's database
    /// ([`varve_catalog::postgres::programs::available_extensions`]). Runs on
    /// the metadata session of the tab's database.
    ///
    /// # Errors
    /// Unknown session, a MySQL, MariaDB or Redis session (`catalog` /
    /// `unsupported`), driver errors (for example when the role may not read
    /// the control files).
    pub async fn catalog_available_extensions(
        &self,
        session_id: &str,
    ) -> CmdResult<Vec<varve_catalog::postgres::programs::PgAvailableExtension>> {
        let info = self.session_info(session_id)?;
        if info.engine != Engine::Postgres {
            return Err(CatalogError::Unsupported {
                message: "only PostgreSQL has extensions".into(),
            }
            .into());
        }
        self.with_catalog(session_id, |_, session| {
            Box::pin(varve_catalog::postgres::programs::available_extensions(
                session,
            ))
        })
        .await
    }

    /// Opens a session for a saved profile with another database: a
    /// PostgreSQL session is bound to one database, so the browser opens
    /// one per database it shows (for MySQL and MariaDB it sets the default
    /// schema). The secret and tunnel flow is the one of `open_session`;
    /// [`SessionInfo::database`] names the database.
    ///
    /// # Errors
    /// An empty name or a Redis profile (`invalid`), and everything
    /// `open_session` returns.
    pub async fn open_database_session(
        &self,
        id: &str,
        database: &str,
        password: Option<SecretInput>,
        ssh_secret: Option<SecretInput>,
        tls_passphrase: Option<SecretInput>,
    ) -> CmdResult<SessionInfo> {
        let database = database.trim();
        if database.is_empty() {
            return Err(CmdError::Invalid {
                message: "Name the database to open.".into(),
            });
        }
        let mut profile = self.records.connection(id)?;
        if profile.engine == Engine::Redis {
            return Err(CmdError::Invalid {
                message: "Redis databases are numbered; switch them with the database selector."
                    .into(),
            });
        }
        profile.database = Some(database.to_string());
        self.open_profile_session(&profile, password, ssh_secret, tls_passphrase)
            .await
    }
}

/// How another session relates to the server a drop runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerMatch {
    /// Known to be the same server.
    Same,
    /// A PostgreSQL streaming standby of it (or the primary of a standby):
    /// same system identifier and timeline, other role. The drop replays
    /// there and ends its sessions on the database, with or without FORCE.
    Standby,
    /// May be the same server: an identity is unknown and the endpoint
    /// matches, or only a weak identity (MariaDB) matches.
    Possible,
}

/// The parts of a PostgreSQL identity (`pg:<system id>:tl<timeline>:<role>`).
fn pg_identity(identity: &str) -> Option<(&str, &str, &str)> {
    let mut parts = identity.strip_prefix("pg:")?.split(':');
    let (system, timeline, role) = (parts.next()?, parts.next()?, parts.next()?);
    parts.next().is_none().then_some((system, timeline, role))
}

/// Whether `other` is on the same server as `this` (see [`ServerMatch`]),
/// `None` when it is not (identities differ, a promoted standby on another
/// timeline, or nothing ties them together).
pub(crate) fn on_same_server(this: &SessionInfo, other: &SessionInfo) -> Option<ServerMatch> {
    if this.engine != other.engine {
        return None;
    }
    let same_setup = this.connection_id == other.connection_id && this.identity == other.identity;
    match (&this.server_identity, &other.server_identity) {
        (Some(a), Some(b)) => {
            if let (Some((sa, ta, ra)), Some((sb, tb, rb))) = (pg_identity(a), pg_identity(b)) {
                return match (sa == sb && ta == tb, ra == rb) {
                    (true, true) => Some(ServerMatch::Same),
                    (true, false) => Some(ServerMatch::Standby),
                    (false, _) => None,
                };
            }
            if a != b {
                return None;
            }
            // A weak identity (MariaDB) can collide: certain only when the
            // same profile opened both the same way.
            if this.engine == Engine::Mariadb && !same_setup {
                Some(ServerMatch::Possible)
            } else {
                Some(ServerMatch::Same)
            }
        }
        _ if same_setup => Some(ServerMatch::Same),
        _ => crate::sessions::same_endpoint(&this.identity, &other.identity)
            .then_some(ServerMatch::Possible),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionPurpose;
    use varve_driver::ServerInfo;
    use varve_records::Environment;

    fn session(
        id: &str,
        connection: &str,
        host: &str,
        ssh: &str,
        server: Option<&str>,
    ) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            connection_id: connection.into(),
            engine: Engine::Postgres,
            server: ServerInfo {
                engine: Engine::Postgres,
                version: "18".into(),
                major: 18,
                minor: 0,
                patch: None,
            },
            environment: Environment::Development,
            database: Some("shop".into()),
            read_only: false,
            confirm_destructive: false,
            purpose: SessionPurpose::User,
            opened_at: 0,
            encryption: crate::sessions::Encryption {
                tls: Some(false),
                mode: varve_driver::TlsMode::Disable,
                password_unencrypted: false,
            },
            identity: serde_json::json!({ "engine": "postgres", "host": host, "port": 5432, "ssh": ssh, "username": "u" })
                .to_string(),
            server_identity: server.map(str::to_owned),
        }
    }

    #[test]
    fn sessions_match_on_the_server_they_reached() {
        use ServerMatch::{Possible, Same, Standby};
        let a = session("a", "c1", "localhost", "jump-a", Some("pg:1:tl1:primary"));
        // Same saved endpoint, another server (a second tunnel on the same local port, or an edited connection).
        assert_eq!(
            on_same_server(
                &a,
                &session("b", "c2", "localhost", "jump-a", Some("pg:2:tl1:primary"))
            ),
            None
        );
        // Another endpoint, same server (an alias): certain.
        assert_eq!(
            on_same_server(
                &a,
                &session("c", "c3", "127.0.0.1", "x", Some("pg:1:tl1:primary"))
            ),
            Some(Same)
        );
        // Its streaming standby: same system and timeline, other role.
        assert_eq!(
            on_same_server(
                &a,
                &session("s", "c4", "replica", "x", Some("pg:1:tl1:standby"))
            ),
            Some(Standby)
        );
        // A promoted standby (new timeline) is another server now.
        assert_eq!(
            on_same_server(
                &a,
                &session("p", "c5", "old", "x", Some("pg:1:tl2:primary"))
            ),
            None
        );
        // Unknown identity: the same profile and connect identity is certain, the same endpoint only possible.
        let unknown = session("d", "c1", "localhost", "jump-a", None);
        assert_eq!(
            on_same_server(&unknown, &session("e", "c1", "localhost", "jump-a", None)),
            Some(Same)
        );
        assert_eq!(
            on_same_server(&unknown, &session("f", "c9", "localhost", "jump-a", None)),
            Some(Possible)
        );
        assert_eq!(
            on_same_server(&unknown, &session("g", "c9", "localhost", "jump-b", None)),
            None
        );
        assert_eq!(
            on_same_server(&a, &session("h", "c9", "localhost", "jump-a", None)),
            Some(Possible)
        );
    }

    #[test]
    fn weak_mariadb_identities_match_only_as_possible() {
        let maria = |id: &str, conn: &str, server: &str| SessionInfo {
            engine: Engine::Mariadb,
            ..session(id, conn, "db", "x", Some(server))
        };
        let a = maria("a", "c1", "mariadb:0:1:h:3306:/var/lib/mysql/");
        assert_eq!(
            on_same_server(&a, &maria("b", "c2", "mariadb:0:1:h:3306:/var/lib/mysql/")),
            Some(ServerMatch::Possible)
        );
        assert_eq!(
            on_same_server(&a, &maria("c", "c1", "mariadb:0:1:h:3306:/var/lib/mysql/")),
            Some(ServerMatch::Same)
        );
        assert_eq!(
            on_same_server(
                &a,
                &maria("d", "c2", "mariadb:0:1:other:3306:/var/lib/mysql/")
            ),
            None
        );
    }
}
