//! Engine catalogs for the object browser and editors (design doc 4.2,
//! milestone M2): catalog extraction, DDL, editors that turn typed changes
//! into SQL, `EXPLAIN` plan trees, and SQL for grid data editing. The schema
//! diff engine is not part of this crate.
//!
//! Everything runs through [`varve_driver::Session::execute`], so the
//! catalog works the same over direct, TLS and tunnelled connections and
//! never needs a driver-specific API. Redis is not here: its key browser
//! lives in `varve-driver-redis`.
//!
//! Layout:
//! - [`model`]: engine-neutral references, tree entries and the [`Catalog`]
//!   trait the app calls.
//! - [`sql`]: identifier and literal quoting per dialect.
//! - [`rows`]: reading catalog query results by column name.
//! - [`postgres`], [`mysql`]: per-engine catalogs (MariaDB is a dialect of
//!   the MySQL module), with `edit` submodules for the editors.
//! - [`explain`]: plan trees from each engine's `EXPLAIN` output.
//! - [`data_edit`]: SQL for inserting, updating and deleting grid rows.

pub mod data_edit;
pub mod explain;
pub mod model;
pub mod mysql;
pub mod postgres;
pub mod rows;
pub mod sql;

pub use model::{
    Capabilities, Catalog, CatalogError, CatalogResult, Container, EditPlan, KindCount,
    ObjectDetails, ObjectKind, ObjectRef, ObjectSummary, catalog_for, open_catalog,
};
