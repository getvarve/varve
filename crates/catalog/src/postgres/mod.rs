//! PostgreSQL catalog.
//!
//! Split over two modules so they can be built in parallel:
//! [`relations`] (databases, schemas, tables and everything that hangs off
//! them, views, sequences, tablespaces) and [`programs`] (routines, types,
//! extensions, replication, foreign data, collations, casts, operators,
//! event triggers). This file only dispatches by kind.
//!
//! A PostgreSQL session is connected to one database, so every call works
//! on the session's current database; [`Container::database`] must name it
//! (the app opens one session per browsed database).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use varve_driver::{ServerInfo, Session};

use crate::model::{
    Capabilities, Catalog, CatalogError, CatalogResult, Container, KindCount, ObjectDetails,
    ObjectKind, ObjectRef, ObjectSummary,
};

pub mod edit;
pub mod programs;
pub mod relations;

/// Details of one PostgreSQL object, tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PgObject {
    /// An object of [`relations::KINDS`].
    Relation(relations::PgRelationObject),
    /// An object of [`programs::KINDS`].
    Program(programs::PgProgramObject),
}

/// The PostgreSQL catalog.
#[derive(Debug, Clone)]
pub struct PgCatalog {
    server: ServerInfo,
    capabilities: Capabilities,
}

impl PgCatalog {
    /// The catalog for a server.
    #[must_use]
    pub fn new(server: &ServerInfo) -> Self {
        let mut kinds = relations::kinds(server);
        kinds.extend(programs::kinds(server));
        let mut editable = relations::editable(server);
        editable.extend(programs::editable(server));
        let mut features = relations::features(server);
        features.extend(programs::features(server));
        kinds.sort();
        kinds.dedup();
        editable.sort();
        editable.dedup();
        features.sort();
        features.dedup();
        Self {
            server: server.clone(),
            capabilities: Capabilities {
                engine: server.engine,
                kinds,
                editable,
                features,
            },
        }
    }
}

fn owner_of(kind: ObjectKind) -> Option<Owner> {
    if relations::KINDS.contains(&kind) {
        Some(Owner::Relations)
    } else if programs::KINDS.contains(&kind) {
        Some(Owner::Programs)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy)]
enum Owner {
    Relations,
    Programs,
}

fn unsupported(kind: ObjectKind) -> CatalogError {
    CatalogError::Unsupported {
        message: format!("PostgreSQL has no {kind:?} objects"),
    }
}

#[async_trait]
impl Catalog for PgCatalog {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn databases(&self, session: &dyn Session) -> CatalogResult<Vec<ObjectSummary>> {
        relations::databases(session, &self.server).await
    }

    async fn schemas(
        &self,
        session: &dyn Session,
        database: &str,
    ) -> CatalogResult<Vec<ObjectSummary>> {
        relations::schemas(session, &self.server, database).await
    }

    async fn kinds(
        &self,
        session: &dyn Session,
        container: &Container,
        include_system: bool,
    ) -> CatalogResult<Vec<KindCount>> {
        let mut counts =
            relations::counts(session, &self.server, container, include_system).await?;
        counts.extend(programs::counts(session, &self.server, container, include_system).await?);
        counts.retain(|c| c.count > 0);
        counts.sort_by_key(|c| c.kind);
        Ok(counts)
    }

    async fn list(
        &self,
        session: &dyn Session,
        container: &Container,
        kind: ObjectKind,
        include_system: bool,
    ) -> CatalogResult<Vec<ObjectSummary>> {
        match owner_of(kind) {
            Some(Owner::Relations) => {
                relations::list(session, &self.server, container, kind, include_system).await
            }
            Some(Owner::Programs) => {
                programs::list(session, &self.server, container, kind, include_system).await
            }
            None => Err(unsupported(kind)),
        }
    }

    async fn describe(
        &self,
        session: &dyn Session,
        object: &ObjectRef,
    ) -> CatalogResult<ObjectDetails> {
        let details = match owner_of(object.kind) {
            Some(Owner::Relations) => {
                PgObject::Relation(relations::describe(session, &self.server, object).await?)
            }
            Some(Owner::Programs) => {
                PgObject::Program(programs::describe(session, &self.server, object).await?)
            }
            None => return Err(unsupported(object.kind)),
        };
        Ok(ObjectDetails::Postgres(Box::new(details)))
    }

    async fn ddl(&self, session: &dyn Session, object: &ObjectRef) -> CatalogResult<String> {
        match owner_of(object.kind) {
            Some(Owner::Relations) => relations::ddl(session, &self.server, object).await,
            Some(Owner::Programs) => programs::ddl(session, &self.server, object).await,
            None => Err(unsupported(object.kind)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_postgres_kind_has_exactly_one_owner() {
        for kind in relations::KINDS {
            assert!(
                !programs::KINDS.contains(kind),
                "{kind:?} is in both modules"
            );
        }
    }
}
