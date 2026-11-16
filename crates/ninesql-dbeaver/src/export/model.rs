//! Input and report model of the DBeaver export (P5-D, F-CON-15).
//!
//! The input mirrors `ninesql_core::Connection` and `ninesql_core::ConnectionType` (the core
//! depends on this crate, so it converts its own values). **It holds no secret value by
//! construction** (I1): a password, passphrase or token is represented only by whether one is
//! saved in NineSQL, which the export reports and never writes.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use ninesql_driver_api::EngineId;

pub use crate::mapping::{Rgb, TlsMode};
pub use crate::report::Severity;

/// Where the database is (mirrors `ninesql_core::Endpoint`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportEndpoint {
    /// A server reached over TCP.
    Tcp {
        /// Host name or IP address (IPv6 without brackets).
        host: String,
        /// Port; `None` for the engine's default.
        port: Option<u16>,
        /// Database to open.
        database: Option<String>,
    },
    /// A local Unix socket: DBeaver has no generic equivalent, so it is not exported.
    UnixSocket {
        /// Socket path or directory.
        path: PathBuf,
        /// Database to open.
        database: Option<String>,
    },
    /// An embedded engine's database file.
    File {
        /// Database file path.
        path: PathBuf,
    },
}

/// Database authentication (mirrors `ninesql_core::Auth`, without the secret reference).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportAuth {
    /// No credentials.
    None,
    /// A user name without a password.
    Username {
        /// User name.
        username: String,
    },
    /// User name and password.
    Password {
        /// User name.
        username: String,
        /// Whether NineSQL has the password saved (never exported).
        saved: bool,
    },
    /// A token, with an optional user name.
    Token {
        /// User name.
        username: Option<String>,
        /// Whether NineSQL has the token saved (never exported).
        saved: bool,
    },
}

/// SSH authentication of one hop (mirrors `ninesql_core::SshAuth`, without secret references).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportSshAuth {
    /// `ssh-agent`.
    Agent,
    /// Password.
    Password {
        /// Whether NineSQL has it saved (never exported).
        saved: bool,
    },
    /// Private key file.
    PublicKey {
        /// Path of the private key (a path only: the key file is never read).
        key_path: PathBuf,
        /// `None` for an unencrypted key; `Some(saved)` for a key with a passphrase, `saved`
        /// telling whether NineSQL has the passphrase saved (never exported).
        passphrase: Option<bool>,
    },
}

/// Host key policy of one SSH hop (mirrors `ninesql_core::HostKeyPolicy`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExportHostKeyPolicy {
    /// The default.
    #[default]
    Strict,
    /// Trust on first use.
    AcceptNew,
    /// Not checked.
    Disabled,
}

/// One SSH server (mirrors `ninesql_core::SshHop`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSshHop {
    /// Host.
    pub host: String,
    /// Port.
    pub port: u16,
    /// User name.
    pub username: String,
    /// Authentication.
    pub auth: ExportSshAuth,
    /// Host key policy (not exported: DBeaver has no per-hop policy in the verified format).
    pub host_key_policy: ExportHostKeyPolicy,
}

/// SSH tunnel (mirrors `ninesql_core::SshTunnel`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSshTunnel {
    /// Server that forwards to the database.
    pub server: ExportSshHop,
    /// Jump hosts, closest to NineSQL first.
    pub jump_hosts: Vec<ExportSshHop>,
}

/// TLS client certificate (mirrors `ninesql_core::ClientCertificate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportClientCertificate {
    /// Certificate file.
    pub cert_file: PathBuf,
    /// Key file.
    pub key_file: PathBuf,
    /// Whether the key has a passphrase (never exported).
    pub has_passphrase: bool,
}

/// TLS settings (mirrors `ninesql_core::TlsConfig`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportTls {
    /// Mode.
    pub mode: TlsMode,
    /// CA file (a path only).
    pub ca_file: Option<PathBuf>,
    /// Client certificate (paths only).
    pub client_certificate: Option<ExportClientCertificate>,
}

/// Why a connection is disabled (mirrors `ninesql_core::DisabledKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExportDisabled {
    /// Disabled by the user (`enabled = false`, no reason).
    ByUser,
    /// No NineSQL driver for the engine.
    NoDriver,
    /// The imported route could not be reproduced (DBeaver network profile, proxy, other SSH
    /// forwarding target): **never exported**, since DBeaver would then connect without it.
    Route,
    /// The driver refuses the settings.
    Invalid,
}

/// A connection to export (mirrors `ninesql_core::Connection`; see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportConnection {
    /// NineSQL connection id (report only; never written to the DBeaver file).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// Engine.
    pub engine: EngineId,
    /// Where the database is.
    pub endpoint: ExportEndpoint,
    /// Database authentication.
    pub auth: ExportAuth,
    /// `password_exchange = scram-only` (NineSQL-only: reported, not exported).
    pub scram_only: bool,
    /// SSH tunnel.
    pub ssh: Option<ExportSshTunnel>,
    /// TLS settings.
    pub tls: ExportTls,
    /// Connection type id.
    pub connection_type: String,
    /// Read-only override of the type's default.
    pub read_only: Option<bool>,
    /// Folder path, outermost first.
    pub folder: Vec<String>,
    /// NineSQL driver properties.
    pub properties: BTreeMap<String, String>,
    /// Init statements.
    pub init_sql: Vec<String>,
    /// `Connection::init_sql_needs_confirmation`: imported init SQL the user has not confirmed
    /// yet. Such statements are not exported (reported).
    pub init_sql_needs_confirmation: bool,
    /// Default schema.
    pub default_schema: Option<String>,
    /// Default catalog.
    pub default_catalog: Option<String>,
    /// Auto-commit override.
    pub auto_commit: Option<bool>,
    /// `None` when enabled.
    pub disabled: Option<ExportDisabled>,
}

/// A connection type (mirrors `ninesql_core::ConnectionType`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportConnectionType {
    /// Id (`dev`, `test`, `prod` or a user-defined id).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Color.
    pub color: Rgb,
    /// Auto-commit default.
    pub auto_commit: bool,
    /// Confirm destructive statements.
    pub confirm_destructive: bool,
    /// Confirm every write.
    pub confirm_every_write: bool,
    /// Read-only default (folded into each connection's `read-only` flag).
    pub read_only: bool,
    /// Query text may be logged (NineSQL-only).
    pub log_query_text: bool,
    /// Production rules.
    pub production: bool,
}

impl ExportConnectionType {
    /// Whether production rules apply (the built-in `prod` type always counts), as
    /// `ninesql_core::ConnectionType::is_production`.
    #[must_use]
    pub fn is_production(&self) -> bool {
        self.production || self.id == "prod"
    }

    /// NineSQL's three built-in types with their default settings (as
    /// `ninesql_core::ConnectionType::builtins`): used when the caller does not pass them.
    #[must_use]
    pub fn builtins() -> [Self; 3] {
        let t = |id: &str, name: &str, color: (u8, u8, u8), auto: bool, prod: bool| Self {
            id: id.to_owned(),
            name: name.to_owned(),
            color: Rgb {
                r: color.0,
                g: color.1,
                b: color.2,
            },
            auto_commit: auto,
            confirm_destructive: prod,
            confirm_every_write: false,
            read_only: false,
            log_query_text: !prod,
            production: prod,
        };
        [
            t("dev", "Development", (0xFF, 0xFF, 0xFF), true, false),
            t("test", "Test", (0xC4, 0xFF, 0xB5), true, false),
            t("prod", "Production", (0xF7, 0x9F, 0x81), false, true),
        ]
    }
}

/// Options of [`crate::export_connections`].
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    /// The project's connection types. Built-in types missing from the list get NineSQL's
    /// defaults; a connection whose type is in neither is not exported (fail closed: its
    /// read-only default is unknown).
    pub connection_types: Vec<ExportConnectionType>,
    /// Milliseconds since the Unix epoch, for the DBeaver-style ids
    /// (`<driver>-<hex millis>-<hex random>`). `None`: the current time.
    pub timestamp_millis: Option<u64>,
}

/// What an export report entry is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExportIssueKind {
    /// A setting with no DBeaver equivalent: not written.
    Dropped,
    /// A setting written in the nearest DBeaver form; it does not mean exactly the same.
    Approximated,
    /// A saved secret (password, passphrase, token) is not exported; DBeaver asks for it.
    SecretOmitted,
    /// A value that looks like it holds a secret: not written (I1).
    LooksLikeSecret,
    /// The connection is not exported.
    Skipped,
    /// A format detail not verified against a DBeaver-written workspace yet.
    NotVerified,
}

/// One entry of the export report. Names NineSQL fields and DBeaver keys, never a value
/// (no host, user, property value or statement text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportIssue {
    /// Severity.
    pub severity: Severity,
    /// Category.
    pub kind: ExportIssueKind,
    /// NineSQL field or property concerned (`tls.ca_file`, `properties.work_mem`…).
    pub field: Option<String>,
    /// Explanation.
    pub message: String,
}

impl fmt::Display for ExportIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.field {
            Some(field) => write!(f, "{field}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Export result of one connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionExport {
    /// NineSQL connection id.
    pub id: String,
    /// Connection name.
    pub name: String,
    /// The DBeaver id written, or `None` when the connection was skipped.
    pub dbeaver_id: Option<String>,
    /// Entries of this connection.
    pub issues: Vec<ExportIssue>,
}

impl ConnectionExport {
    /// Whether the connection is in the file.
    #[must_use]
    pub fn exported(&self) -> bool {
        self.dbeaver_id.is_some()
    }
}

/// Report of one export.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// One entry per connection given, in order.
    pub connections: Vec<ConnectionExport>,
    /// Entries not tied to one connection (connection types, format notes).
    pub issues: Vec<ExportIssue>,
}

impl ExportReport {
    /// Number of connections written.
    #[must_use]
    pub fn exported_count(&self) -> usize {
        self.connections.iter().filter(|c| c.exported()).count()
    }
}

/// A generated `data-sources.json`, not written anywhere yet.
#[derive(Clone, PartialEq, Eq)]
#[must_use = "the export must be written or shown"]
pub struct Export {
    /// The file content (UTF-8 JSON, tab-indented like DBeaver's).
    pub json: Vec<u8>,
    /// What was dropped or approximated.
    pub report: ExportReport,
}

impl fmt::Debug for Export {
    /// The content names hosts and users: its size only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Export")
            .field("json_bytes", &self.json.len())
            .field("report", &self.report)
            .finish()
    }
}

/// Collects report entries.
#[derive(Default)]
pub(crate) struct Issues(pub(crate) Vec<ExportIssue>);

impl Issues {
    pub(crate) fn push(
        &mut self,
        severity: Severity,
        kind: ExportIssueKind,
        field: Option<&str>,
        message: impl Into<String>,
    ) {
        self.0.push(ExportIssue {
            severity,
            kind,
            field: field.map(str::to_owned),
            message: message.into(),
        });
    }

    pub(crate) fn info(&mut self, kind: ExportIssueKind, field: &str, message: impl Into<String>) {
        self.push(Severity::Info, kind, Some(field), message);
    }

    pub(crate) fn warn(&mut self, kind: ExportIssueKind, field: &str, message: impl Into<String>) {
        self.push(Severity::Warning, kind, Some(field), message);
    }
}
