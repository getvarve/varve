//! Folders, connection profiles and saved queries (design doc 3.1 and 3.4).
//!
//! Records live in `app.db` (tables `core_folders`, `core_connections`,
//! `core_saved_queries`). Profiles never contain secrets: passwords and
//! passphrases stay in the OS keychain, and a profile only lists which kinds
//! this device has saved there. [`connect_params`] combines a profile, the
//! keychain and an optional SSH tunnel endpoint into driver parameters.

mod model;
mod repo;
mod resolve;
#[cfg(test)]
mod tests;

pub use model::{
    ConnectionInput, ConnectionOptions, ConnectionProfile, Environment, Folder, FolderContents,
    FolderDeleteMode, PERSONAL_WORKSPACE, SavedQuery, SavedQueryInput, SecretKindName,
    SshAuthMethod, SshSettings, TlsSettings, default_port,
};
pub use repo::{RecordError, Records, ReorderKind};
pub use resolve::{TunnelEndpoint, connect_params};
