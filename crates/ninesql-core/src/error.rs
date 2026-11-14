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
    /// A confirmation token (statement, init SQL or SSH host key) that does not match what it
    /// is used for, was already used or expired.
    #[error("the confirmation does not match what is about to run, or expired; confirm it again")]
    InvalidConfirmation,
    /// The session is closed or its task ended.
    #[error("the session is closed")]
    SessionClosed,
    /// The driver runtime is shut down or failed to start.
    #[error("driver runtime: {0}")]
    Runtime(String),
}

impl CoreError {
    /// A short, stable name of the error kind, safe to log at any level: no engine message, no
    /// host, no path (AGENTS.md logging rules). For engine query errors, the engine code is
    /// available through [`Self::engine_code`].
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoDriver(_) => "no-driver",
            Self::ConnectionDisabled => "connection-disabled",
            Self::InvalidConnection(_) => "invalid-connection",
            Self::UnsupportedEndpoint(_) => "unsupported-endpoint",
            Self::PortRequired(_) => "port-required",
            Self::Secret { .. } => "secret",
            Self::Ssh(e) => ssh_error_kind(e),
            Self::Driver(e) => driver_error_kind(e),
            Self::ConnectTimeout(_) => "connect-timeout",
            Self::ReadOnlyNotEnforced => "read-only-not-enforced",
            Self::StillCancelling => "still-cancelling",
            Self::InvalidConfirmation => "invalid-confirmation",
            Self::SessionClosed => "session-closed",
            Self::Runtime(_) => "runtime",
        }
    }

    /// The engine error code (SQLSTATE, MySQL error number…) of a query error, if any. Safe to
    /// log.
    #[must_use]
    pub fn engine_code(&self) -> Option<&str> {
        match self {
            Self::Driver(ninesql_driver_api::DriverError::Query { code, .. }) => code.as_deref(),
            _ => None,
        }
    }
}

/// A short, stable name of a driver error kind, safe to log at any level.
#[must_use]
pub(crate) fn driver_error_kind(e: &ninesql_driver_api::DriverError) -> &'static str {
    use ninesql_driver_api::DriverError as E;
    match e {
        E::InvalidParams(_) => "driver-invalid-params",
        E::Connect(_) => "driver-connect",
        E::Auth(_) => "driver-auth",
        E::Tls(_) => "driver-tls",
        E::ReadOnly(_) => "driver-read-only",
        E::Query { .. } => "driver-query",
        E::Timeout(_) => "driver-timeout",
        E::Cancelled => "driver-cancelled",
        E::Unsupported(_) => "driver-unsupported",
        E::MultipleStatements(_) => "driver-multiple-statements",
        E::Busy => "driver-busy",
        E::Closed(_) => "driver-closed",
        E::Protocol(_) => "driver-protocol",
        E::Io(_) => "driver-io",
        _ => "driver-other",
    }
}

fn ssh_error_kind(e: &ninesql_ssh::SshError) -> &'static str {
    use ninesql_ssh::SshError as E;
    match e {
        E::HostKeyNeedsConfirmation(_) => "ssh-host-key-unknown",
        E::HostKeyChanged { .. } => "ssh-host-key-changed",
        E::HostKeyRevoked { .. } => "ssh-host-key-revoked",
        E::UnsupportedHostKey { .. } | E::InvalidHostKey => "ssh-host-key-invalid",
        E::Connect { .. } => "ssh-connect",
        E::Timeout { .. } => "ssh-timeout",
        E::Protocol { .. } => "ssh-protocol",
        E::AuthFailed { .. } | E::PartialAuth { .. } => "ssh-auth",
        E::KeyFile { .. } => "ssh-key-file",
        E::Agent(_) => "ssh-agent",
        _ => "ssh-other",
    }
}
