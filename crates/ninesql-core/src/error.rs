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
    /// A driver property reserved by the driver API rule (TLS, endpoint, credentials, read-only,
    /// init SQL, session modes…), see [`crate::properties::reserved`].
    #[error("the driver property `{key}` is reserved: {use_instead}")]
    ReservedProperty {
        /// Property key.
        key: String,
        /// What to do instead, as a sentence fragment ([`crate::properties::Reserved::model_field`]).
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

/// Error of the driver runtime, the connect flow and sessions (P1-D). User-presentable: no
/// variant carries a secret, a parameter value, a result value or query text.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoreError {
    /// No registered driver serves this engine.
    #[error("no driver is available for the engine `{0}`")]
    NoDriver(String),
    /// The connection is disabled and cannot be opened.
    #[error("the connection is disabled")]
    ConnectionDisabled,
    /// The connection data is invalid, or uses a property the driver reserves.
    #[error(transparent)]
    InvalidConnection(#[from] ConnectionError),
    /// The endpoint kind is not supported by the driver API yet.
    #[error("{0}")]
    UnsupportedEndpoint(&'static str),
    /// A TCP endpoint without port, for an engine without a known default port.
    #[error("the connection needs a port: the engine `{0}` has no default port")]
    PortRequired(String),
    /// A saved secret could not be read.
    #[error("cannot read the saved secret for {slot:?}: {source}")]
    Secret {
        /// Credential field.
        slot: SecretSlot,
        /// What failed (never the value).
        #[source]
        source: ninesql_secrets::SecretError,
    },
    /// The SSH tunnel could not be opened.
    #[error(transparent)]
    Ssh(#[from] ninesql_ssh::SshError),
    /// The driver failed.
    #[error(transparent)]
    Driver(#[from] ninesql_driver_api::DriverError),
    /// The connect did not finish in time.
    #[error("the connection timed out after {0:?}")]
    ConnectTimeout(std::time::Duration),
    /// The connection must be read-only but the session is not (I3): it was closed at once.
    #[error(
        "the connection must be read-only, but the driver did not report a read-only session; it was closed"
    )]
    ReadOnlyNotEnforced,
    /// The previous statement was cancelled and the server has not confirmed its end yet.
    #[error(
        "the previous statement is still being cancelled; try again when the server confirms it stopped"
    )]
    StillCancelling,
    /// A confirmation token that does not match this statement, this session, or was used.
    #[error("the confirmation does not match this statement; confirm it again")]
    InvalidConfirmation,
    /// The session is closed or its task ended.
    #[error("the session is closed")]
    SessionClosed,
    /// The driver runtime is shut down or failed to start.
    #[error("driver runtime: {0}")]
    Runtime(String),
}
