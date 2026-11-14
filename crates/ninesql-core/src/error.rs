//! Errors of the core model and services.

use crate::connection::{ConnectionId, SecretSlot};

/// Invalid connection or connection type data.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectionError {
    // The three variants below never echo the rejected text: it comes from a file and could be a
    // misplaced secret (I1).
    /// Engine identifier breaking its rule.
    #[error(
        "invalid engine identifier: a lower-case letter, then up to 63 lower-case letters, digits, hyphens or underscores"
    )]
    InvalidEngineId,
    /// Connection type identifier breaking its rule.
    #[error(
        "invalid connection type identifier: up to 64 lower-case letters, digits, hyphens or underscores, starting with a letter or digit"
    )]
    InvalidConnectionTypeId,
    /// Color not in `#RRGGBB` form.
    #[error("invalid color: #RRGGBB expected")]
    InvalidColor,
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
    /// A driver property that shadows a typed field (TLS, endpoint, user, read-only).
    #[error("the driver property `{key}` is not allowed; set it with `{use_instead}` instead")]
    ReservedProperty {
        /// Property key.
        key: String,
        /// Model field to use instead.
        use_instead: &'static str,
    },
    /// A driver property whose value embeds a credential (URL user info, `password=`). The value
    /// is never shown.
    #[error("the driver property `{key}` contains a credential; secrets go to the keyring")]
    SecretInPropertyValue {
        /// Property key.
        key: String,
    },
    /// An init SQL statement with a password literal. The statement is never shown.
    #[error("init SQL statement #{index} contains a password literal; it cannot be saved")]
    SecretInInitSql {
        /// Index in `init_sql`.
        index: usize,
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
