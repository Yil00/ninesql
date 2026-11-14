//! Application core: connection model, workspace services, Tokio driver runtime and the bridge to
//! the UI (ADR-0004). ROADMAP: P0-D (connection model, workspace), P1-D, P3-C.
#![forbid(unsafe_code)]

pub mod connect;
pub mod connection;
pub mod connection_type;
mod error;
pub mod properties;
pub mod registry;
pub mod runtime;
pub mod session;
pub mod workspace;

pub use connect::{FlaggedInitStatement, SecretPrompt};
pub use connection::{
    Auth, ClientCertificate, Connection, ConnectionId, Endpoint, EngineId, HostKeyPolicy, Origin,
    SecretSlot, SecretSource, SecurityWarning, SshAuth, SshHop, SshTunnel, TlsConfig, TlsMode,
};
pub use connection_type::{Color, ConnectionType, ConnectionTypeId, ConnectionTypes, SafetyPolicy};
pub use error::{ConnectionError, CoreError, WorkspaceError};
pub use registry::DriverRegistry;
pub use runtime::DriverRuntime;
pub use session::{
    CloseReason, ConfirmReason, ConfirmationRequest, ConfirmationToken, CoreEvent, CoreEvents,
    ExecEvent, ExecuteRequest, Execution, HostKeyConfirmation, HostKeyToken, InitSqlConfirmation,
    InitSqlToken, KnownHostsFiles, ManagerConfig, Next, OpenOutcome, OpenRequest, Reply,
    SessionHandle, SessionId, SessionInfo, SessionManager, SessionWarning, TransactionState,
};
pub use workspace::{
    ConnectionTypesFile, Connections, ConnectionsFile, LoadIssue, LoadedConnections, ProjectFile,
    ProjectId, Workspace,
};

// Dependency graph of docs/02-architecture.md, made explicit until real code uses these crates.
pub use ninesql_dbeaver as dbeaver;
pub use ninesql_driver_api as driver_api;
pub use ninesql_secrets as secrets;
pub use ninesql_ssh as ssh;
pub use ninesql_storage as storage;
pub use ninesql_transfer as transfer;
