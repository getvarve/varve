//! Errors of the activity recorder.

use varve_store::StoreError;

/// Errors from recording, querying, pruning or exporting activity.
#[derive(Debug, thiserror::Error)]
pub enum ActivityError {
    /// The local database failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The action is not a dotted lowercase identifier.
    #[error(
        "invalid activity action `{0}`: use dotted lowercase segments such as `query.executed`"
    )]
    InvalidAction(String),
    /// The event breaks another rule of the event model.
    #[error("invalid activity event: {0}")]
    InvalidEvent(String),
    /// A `before_id` cursor names an event that does not exist (any more).
    #[error("unknown activity cursor `{0}`")]
    UnknownCursor(String),
    /// The retention setting is not usable.
    #[error("invalid retention: {0}")]
    InvalidRetention(String),
    /// A stored row could not be read back.
    #[error("stored activity event `{id}` is unreadable: {reason}")]
    Corrupt {
        /// Id of the row.
        id: String,
        /// What is wrong with it.
        reason: String,
    },
    /// Details could not be encoded or decoded as JSON.
    #[error("activity details are not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Writing an export failed.
    #[error("could not write the activity export: {0}")]
    Io(#[from] std::io::Error),
}
