//! Errors of the core model and services.

use crate::connection::{ConnectionId, SecretSlot};

/// Invalid connection or connection type data.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectionError {
    /// Engine identifier with characters outside `[a-z0-9_-]`.
    #[error("invalid engine identifier `{0}`")]
    InvalidEngineId(String),
    /// Connection type identifier with characters outside `[a-z0-9_-]`.
    #[error("invalid connection type identifier `{0}`")]
    InvalidConnectionTypeId(String),
    /// Color not in `#RRGGBB` form.
    #[error("invalid color `{0}`: `#RRGGBB` expected")]
    InvalidColor(String),
    /// A field breaks a rule.
    #[error("`{field}` {reason}")]
    InvalidField {
        /// Field path.
        field: String,
        /// Rule broken.
        reason: &'static str,
    },
    /// A driver property whose key looks like a secret (I1). The value is never shown.
    #[error(
        "the driver property `{key}` looks like a secret; secrets go to the keyring, not to connection properties"
    )]
    SecretInProperties {
        /// Property key.
        key: String,
    },
    /// The connection has no credential at that slot.
    #[error("the connection has no credential field for {0:?}")]
    NoSuchSecretSlot(SecretSlot),
    /// Two connections share an id.
    #[error("two connections share the id {0}")]
    DuplicateId(ConnectionId),
    /// No connection with that id.
    #[error("no connection with id {0}")]
    NotFound(ConnectionId),
}

/// Error of a workspace operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// Storage failure (I/O, parse, migration).
    #[error(transparent)]
    Storage(#[from] ninesql_storage::StorageError),
    /// Invalid model data.
    #[error("connection `{name}`: {source}")]
    InvalidConnection {
        /// Name of the offending connection.
        name: String,
        /// What is wrong.
        #[source]
        source: ConnectionError,
    },
    /// Invalid model data not tied to one connection.
    #[error(transparent)]
    Model(#[from] ConnectionError),
    /// Secret store failure.
    #[error(transparent)]
    Secrets(#[from] ninesql_secrets::SecretError),
}
