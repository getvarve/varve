//! Current server status (design doc 4.2 and 5): what is running now and
//! how the server is configured, never history. Snapshots, history and
//! dashboards are a closed feature built on top of this crate.
//!
//! - [`sql`]: PostgreSQL, MySQL and MariaDB process lists with cancel and
//!   kill, server variables (browse and edit), server status summary and
//!   MariaDB Galera status.
//! - [`redis`]: Redis server info, slow log, latency monitor, client list
//!   with kill, configuration, memory analysis and cluster or sentinel
//!   topology.
//!
//! Everything runs through [`varve_driver::Session::execute`].

pub mod redis;
pub mod sql;
