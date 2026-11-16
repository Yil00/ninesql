//! Application core: connection model, workspace services, Tokio driver runtime and the bridge to
//! the UI (ADR-0004). ROADMAP: P0-D (connection model, workspace), P1-D, P3-C (execution
//! service [`execution`], query history [`history`], preferences [`preferences`]).
//!
//! The application starts it with [`CoreServices::start`] ([`services`]): workspace, secret
//! backend (keyring → vault → ask every time), session manager, sleep / screen-lock locking and
//! the idle timer, all off the UI thread.
#![forbid(unsafe_code)]

pub mod connect;
pub mod connection;
pub mod connection_type;
pub mod dbeaver_roots;
mod error;
pub mod execution;
pub mod history;
pub mod import;
pub mod metadata;
pub mod preferences;
pub mod properties;
pub mod registry;
pub mod runtime;
pub mod services;
pub mod session;
pub mod transfer;
pub mod workspace;

pub use connect::{FlaggedInitStatement, SecretPrompt};
pub use connection::{
    Auth, ClientCertificate, Connection, ConnectionId, DisabledKind, DisabledReason, Endpoint,
    EngineId, HostKeyPolicy, Origin, PasswordExchange, SecretSlot, SecretSource, SecurityWarning,
    SshAuth, SshHop, SshTunnel, TlsConfig, TlsMode,
};
pub use connection_type::{Color, ConnectionType, ConnectionTypeId, ConnectionTypes, SafetyPolicy};
pub use dbeaver_roots::{DbeaverRoots, FileCheck};
pub use error::{ConnectionError, CoreError, WorkspaceError};
pub use execution::{
    ErrorPosition, ExecutionService, FetchMode, FirstError, MAX_PAUSED_RESULT_IDLE,
    MAX_STATEMENT_OUTCOMES, MIN_PAUSED_RESULT_IDLE, Run, RunControl, RunEnd, RunEvent, RunKind,
    RunRequest, RunSummary, StatementOutcome, StatementStatus, TimeoutChoice,
    paused_result_idle_limit,
};
pub use history::{History, HistoryError, Recorded, StatementRecord};
pub use import::{
    Change, ConnectionPlan, FolderPlan, ImportCancel, ImportChoices, ImportError, ImportOutcome,
    ImportPreview, ImportReportFile, ImportSource, ImportStatus, Importer, PlanAction,
    PreviewWorkspace, ProjectPreview, SecretChange, SecretChangeKind, SecretToMove, TlsChange,
    TypeAction, TypePlan, UpdateDiff,
};
pub use metadata::{
    CacheStats, CompletionSnapshot, ConnectionFilters, InvalidationReason, InvalidationScope,
    KindFilter, MetadataConfig, MetadataError, MetadataEvent, MetadataEvents, MetadataRequest,
    MetadataService, NodeChildren, NodeKey, ObjectDdl, Prefetch, PrefetchLimits, PrefetchSummary,
    SearchHit, SearchRequest, SearchResults,
};
pub use preferences::{
    ConfirmationPreferences, ExecutionPreferences, HistoryPreferences, OnError, PreferenceChange,
    PreferenceIssue, Preferences, PreferencesError,
};
pub use registry::{DriverRegistry, EnableError};
pub use runtime::DriverRuntime;
pub use services::{
    CoreConfig, CoreServices, DEFAULT_LOCK_CHECK_INTERVAL, DEFAULT_VAULT_IDLE_TIMEOUT,
    LockWatcherStatus, SecretBackend, StartupError,
};
pub use session::{
    CloseReason, ConfirmReason, ConfirmationRequest, ConfirmationToken, CoreEvent, CoreEvents,
    ExecEvent, ExecuteRequest, Execution, HostKeyConfirmation, HostKeyToken, InitSqlConfirmation,
    InitSqlToken, KnownHostsFiles, MAX_RECONNECT_ATTEMPTS, MAX_RECONNECT_DELAY, MAX_RECONNECT_TIME,
    ManagerConfig, Next, OpenOutcome, OpenRequest, Reconnect, ReconnectEvent, ReconnectPolicy,
    Reply, SecretsLockReason, SessionHandle, SessionId, SessionInfo, SessionManager,
    SessionWarning, TransactionEnd, TransactionState,
};
pub use transfer::{
    CsvPreviewRequest, ExportEvent, ExportJob, ExportRequest, ExportSource, ExportSummary,
    ImportConfirmation, ImportEnd, ImportEvent, ImportJob, ImportRequest, ImportSummary,
    ImportToken, ImportTransaction, Job, TransferJobError, TransferService,
};
pub use workspace::{
    ConnectionTypesFile, Connections, ConnectionsFile, LoadIssue, LoadedConnections, MarkerClear,
    ProjectFile, ProjectId, Workspace,
};

// Dependency graph of docs/02-architecture.md, made explicit until real code uses these crates.
pub use ninesql_dbeaver as dbeaver;
pub use ninesql_driver_api as driver_api;
pub use ninesql_secrets as secrets;
pub use ninesql_ssh as ssh;
pub use ninesql_storage as storage;
