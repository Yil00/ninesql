//! Mapping parsed DBeaver data to NineSQL's import model (ADR-0006 §3–§8,
//! docs/06-dbeaver-interop.md "Engine mapping" and "Field mapping rules").
//!
//! The output mirrors `ninesql_core::Connection` (same field names and shapes) because the core
//! depends on this crate. Differences, all on purpose:
//! - credentials are a [`SecretState`] (`Imported`: a matching [`ImportedSecret`] is in
//!   [`ImportedConnection::secrets`]; `Prompt`: nothing saved) instead of a `SecretSource`, since
//!   secrets are not in a store yet; P2-C moves each secret and sets the reference;
//! - [`ImportedConnection::default_schema`], [`ImportedConnection::default_catalog`] and
//!   [`ImportedConnection::auto_commit`] carry DBeaver's `bootstrap` settings, which the core
//!   model has no field for yet (each also has a report entry until the core persists them);
//! - [`ImportedConnection::properties`] holds NineSQL driver property names for the engines a
//!   NineSQL driver family serves (JDBC properties are converted or dropped, see
//!   [`ImportedConnection::dropped_properties`] and docs/06 "Driver properties");
//! - [`ImportedConnection::dbeaver_provider`] / [`ImportedConnection::dbeaver_driver`] keep the
//!   raw ids (ADR-0006: unknown pairs are kept, never dropped).
//!
//! P2-C must still run `Connection::validate` / `sanitize` on the converted value: this module
//! applies the reserved-key rule of the driver API and the secret-like-key heuristic of
//! `ninesql-storage`, but not the core's secret-value detectors.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use ninesql_driver_api::{EngineId, is_reserved_property};
use ninesql_secrets::{ExposeSecret, SecretKind, SecretString};
use ninesql_storage::hygiene::{is_secret_like_entry, is_secret_like_key};

use crate::coverage::{self, Scope, join};
use crate::credentials::{
    CONNECTION_SECTION, ConnectionCredentials, Credentials, CredentialsError, SSH_SECTION,
    decrypt_credentials,
};
use crate::detect::Project;
use crate::fsread::{self, MAX_CREDENTIALS_BYTES, MAX_DATA_SOURCES_BYTES};
use crate::jdbc;
use crate::model::{
    ConnectionDef, ConnectionTypeDef, DataSources, FolderDef, HandlerDef, Properties, Scalar,
    parse_data_sources,
};
use crate::properties;
use crate::report::{Budget, ImportIssue, IssueKind, Issues, MAX_IDENT, ident};

// ---------------------------------------------------------------------------------------------
// Output model
// ---------------------------------------------------------------------------------------------

/// Result of importing one DBeaver project.
#[derive(Debug)]
#[must_use = "the import result carries secrets to move and issues to show"]
pub struct ProjectImport {
    /// DBeaver project name (`General`…).
    pub project: String,
    /// `data-sources*.json` files that were read, in order.
    pub source_files: Vec<PathBuf>,
    /// What happened to `credentials-config.json`.
    pub credentials: CredentialsStatus,
    /// Folders, sorted by path (defined ones and those only referenced by connections).
    pub folders: Vec<ImportedFolder>,
    /// Connection types defined in the files.
    pub connection_types: Vec<ImportedConnectionType>,
    /// One entry per DBeaver connection, sorted by DBeaver id.
    pub connections: Vec<ConnectionImport>,
    /// Issues not tied to one connection (files, folders, types, credentials).
    pub issues: Vec<ImportIssue>,
}

impl ProjectImport {
    /// Number of secrets to move to the secret store (for the report: counts, never values).
    #[must_use]
    pub fn secret_count(&self) -> usize {
        self.connections
            .iter()
            .filter_map(|c| c.imported())
            .map(|c| c.secrets.len())
            .sum()
    }
}

/// What happened to `credentials-config.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialsStatus {
    /// The project has no `credentials-config.json`.
    NotFound,
    /// Decrypted; `passwords` saved passwords found (all sections).
    Decrypted {
        /// Number of saved passwords in the file.
        passwords: usize,
    },
    /// The file exists but could not be read (I/O, size).
    ReadFailed,
    /// The file is not in DBeaver Community's format (PRO master password, corruption…):
    /// connections are imported without passwords.
    Unreadable(CredentialsError),
}

/// A folder of the navigator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedFolder {
    /// Folder path, outermost first.
    pub path: Vec<String>,
    /// Description.
    pub description: Option<String>,
}

/// An sRGB color, displayed as `#RRGGBB` (the core's `Color` format).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

/// A connection type, mirroring `ninesql_core::ConnectionType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedConnectionType {
    /// NineSQL id (`[a-z0-9][a-z0-9_-]{0,63}`), derived from the DBeaver id.
    pub id: String,
    /// DBeaver id.
    pub dbeaver_id: String,
    /// `dev`, `test` or `prod`: NineSQL's built-in type of the same id applies (ADR-0007).
    pub builtin: bool,
    /// Display name.
    pub name: String,
    /// Color.
    pub color: Rgb,
    /// Auto-commit default.
    pub auto_commit: bool,
    /// Confirm destructive statements (DBeaver `confirm-execute` or `confirm-data-change`).
    pub confirm_destructive: bool,
    /// Confirm every write (DBeaver `confirm-execute`).
    pub confirm_every_write: bool,
    /// Read-only default: always `false` (DBeaver types have no read-only setting; it is per
    /// connection).
    pub read_only: bool,
    /// Query text may be logged (`false` for production).
    pub log_query_text: bool,
    /// Production rules (`prod` only; NineSQL forces them for its built-in Production type).
    pub production: bool,
}

/// Import status of one connection, for the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Imported and enabled.
    Imported,
    /// Imported disabled (engine outside the v1 set, unsupported tunnel or proxy…).
    ImportedDisabled,
    /// Not imported.
    Failed,
}

/// Import of one DBeaver connection.
#[derive(Debug)]
pub struct ConnectionImport {
    /// DBeaver data source id.
    pub dbeaver_id: String,
    /// Connection name (sanitized), for the report.
    pub name: String,
    /// Imported connection, or why it failed.
    pub outcome: Outcome,
    /// Issues of this connection.
    pub issues: Vec<ImportIssue>,
}

impl ConnectionImport {
    /// The imported connection, if any.
    #[must_use]
    pub fn imported(&self) -> Option<&ImportedConnection> {
        match &self.outcome {
            Outcome::Imported(c) => Some(c),
            Outcome::Failed { .. } => None,
        }
    }

    /// Status for the report.
    #[must_use]
    pub fn status(&self) -> Status {
        match &self.outcome {
            Outcome::Imported(c) if c.enabled => Status::Imported,
            Outcome::Imported(_) => Status::ImportedDisabled,
            Outcome::Failed { .. } => Status::Failed,
        }
    }
}

/// Result of mapping one connection.
#[derive(Debug)]
pub enum Outcome {
    /// Mapped (possibly disabled).
    Imported(Box<ImportedConnection>),
    /// Could not be mapped.
    Failed {
        /// Why (no value from the file).
        reason: String,
    },
}

/// Whether a credential was saved in DBeaver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretState {
    /// Saved: the matching [`ImportedSecret`] is in [`ImportedConnection::secrets`].
    Imported,
    /// Not saved (or not readable): asked at connect time.
    Prompt,
}

/// Where the database is (mirrors `ninesql_core::Endpoint`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A server reached over TCP.
    Tcp {
        /// Host name or IP address (IPv6 without brackets).
        host: String,
        /// Port; `None` for the engine's default.
        port: Option<u16>,
        /// Database to open.
        database: Option<String>,
    },
    /// An embedded engine's database file.
    File {
        /// Database file path.
        path: PathBuf,
    },
}

/// Database authentication (mirrors `ninesql_core::Auth`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// No credentials.
    None,
    /// User name and password.
    Password {
        /// User name (may be empty when DBeaver saved a password only).
        username: String,
        /// Whether the password was saved.
        secret: SecretState,
    },
}

/// SSH authentication of one hop (mirrors `ninesql_core::SshAuth`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshAuth {
    /// `ssh-agent`.
    Agent,
    /// Password.
    Password {
        /// Whether the password was saved.
        secret: SecretState,
    },
    /// Private key file.
    PublicKey {
        /// Path of the private key.
        key_path: PathBuf,
        /// Passphrase: `None` for a key DBeaver used without one.
        passphrase: Option<SecretState>,
    },
}

/// One SSH server (mirrors `ninesql_core::SshHop`). Host keys are never trusted on import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHop {
    /// Host.
    pub host: String,
    /// Port.
    pub port: u16,
    /// User name.
    pub username: String,
    /// Authentication.
    pub auth: SshAuth,
}

/// SSH tunnel (mirrors `ninesql_core::SshTunnel`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTunnel {
    /// Server that forwards to the database.
    pub server: SshHop,
    /// Jump hosts, closest to NineSQL first (none found in the P0-E fixtures; key names and
    /// order taken from DBeaver's source).
    pub jump_hosts: Vec<SshHop>,
}

/// TLS mode (mirrors `ninesql_core::TlsMode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TlsMode {
    /// No TLS.
    Disable,
    /// Encrypted, certificate not verified.
    Require,
    /// Chain verified, host name not verified.
    VerifyCa,
    /// Chain and host name verified (default, I5).
    #[default]
    VerifyFull,
}

/// TLS client certificate (mirrors `ninesql_core::ClientCertificate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCertificate {
    /// Certificate file.
    pub cert_file: PathBuf,
    /// Key file.
    pub key_file: PathBuf,
    /// Passphrase of the key (none found in the verified DBeaver formats yet).
    pub passphrase: Option<SecretState>,
}

/// TLS settings (mirrors `ninesql_core::TlsConfig`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsConfig {
    /// Mode.
    pub mode: TlsMode,
    /// Extra CA file.
    pub ca_file: Option<PathBuf>,
    /// Client certificate.
    pub client_certificate: Option<ClientCertificate>,
}

/// Credential slot of a connection (mirrors `ninesql_core::SecretSlot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretSlot {
    /// Database password.
    Auth,
    /// SSH server password or key passphrase.
    SshServer,
    /// Jump host password or key passphrase.
    SshJump(usize),
    /// TLS client key passphrase.
    TlsClientKey,
}

/// A decrypted secret to hand to `ninesql-secrets` (P2-C). Zeroized on drop, redacted in
/// `Debug`; never serialized, logged or stored by this crate (I1).
#[derive(Debug)]
pub struct ImportedSecret {
    /// Field of the connection it belongs to.
    pub slot: SecretSlot,
    /// Kind, for the secret store's attributes.
    pub kind: SecretKind,
    /// The value.
    pub value: SecretString,
}

/// Provenance of an imported connection (mirrors `ninesql_core::Origin::Dbeaver`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbeaverOrigin {
    /// DBeaver data source id: re-imports match on it (ADR-0006 §8).
    pub dbeaver_id: String,
    /// DBeaver project.
    pub project: String,
}

/// A mapped connection (mirrors `ninesql_core::Connection`, see the module documentation).
pub struct ImportedConnection {
    /// Display name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// NineSQL engine id (for disabled connections, derived from the DBeaver provider).
    pub engine: EngineId,
    /// Raw DBeaver `provider`.
    pub dbeaver_provider: String,
    /// Raw DBeaver `driver`.
    pub dbeaver_driver: String,
    /// Where the database is.
    pub endpoint: Endpoint,
    /// Database authentication.
    pub auth: Auth,
    /// SSH tunnel.
    pub ssh: Option<SshTunnel>,
    /// TLS settings (meaningful for TCP endpoints only).
    pub tls: TlsConfig,
    /// NineSQL connection type id.
    pub connection_type: String,
    /// Read-only override (`Some(true)` when DBeaver's `read-only` flag was set).
    pub read_only: Option<bool>,
    /// Folder path, outermost first.
    pub folder: Vec<String>,
    /// Driver properties.
    ///
    /// For engines served by a NineSQL driver family (pgwire: PostgreSQL, TimescaleDB,
    /// YugabyteDB, QuestDB; mysqlwire: MySQL, MariaDB, TiDB; sqlite), only **NineSQL property
    /// names** from that driver's `ConnectCapabilities::accepted_properties`, with values the
    /// driver accepts: JDBC properties and URL parameters with an equivalent are converted
    /// (`currentSchema` → `search_path`…), the others are listed in
    /// [`dropped_properties`](Self::dropped_properties). For other engines (no NineSQL driver
    /// yet), the non-reserved, non-secret JDBC properties as DBeaver stored them.
    pub properties: BTreeMap<String, String>,
    /// Driver properties and URL parameters that were not imported, with the reason (also in
    /// the report, as `PropertyDropped`, `ReservedProperty`, `SecretRemoved` or `InvalidValue`
    /// issues). Keys only, never values (I1).
    pub dropped_properties: Vec<DroppedProperty>,
    /// Init statements (`bootstrap.query`).
    pub init_sql: Vec<String>,
    /// Default schema DBeaver selects after connecting (`bootstrap.defaultSchema`, trimmed;
    /// `None` when absent or blank). No core field yet (P2-C).
    pub default_schema: Option<String>,
    /// Default catalog (database) DBeaver selects after connecting (`bootstrap.defaultCatalog`,
    /// trimmed; `None` when absent or blank). No core field yet (P2-C).
    pub default_catalog: Option<String>,
    /// Per-connection auto-commit (`bootstrap.autocommit`) exactly as DBeaver stored it:
    /// `Some(false)` for manual commit, `Some(true)` for auto-commit, `None` when DBeaver
    /// stored nothing (the connection type's `auto_commit` applies). Kept even when it equals
    /// the type's default. No per-connection core field yet (P2-C).
    pub auto_commit: Option<bool>,
    /// `false` for disabled connections.
    pub enabled: bool,
    /// Why the connection is disabled.
    pub disabled_reason: Option<String>,
    /// Provenance.
    pub origin: DbeaverOrigin,
    /// Secrets to move to the secret store.
    pub secrets: Vec<ImportedSecret>,
}

/// Why a driver property or URL parameter was not imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PropertyDropReason {
    /// The NineSQL driver has no equivalent setting (or the property had no effect in DBeaver).
    NoEquivalent,
    /// A setting with an equivalent, but a value the NineSQL driver would refuse (or none).
    InvalidValue,
    /// Reserved for NineSQL's typed settings (driver API rule: TLS, endpoint, credentials,
    /// read-only, init SQL…).
    Reserved,
    /// The key or value looks like a secret: removed (secrets only go to the keyring, I1).
    LooksLikeSecret,
}

/// A driver property or URL parameter that was not imported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedProperty {
    /// DBeaver key path (`configuration.properties.ApplicationName`,
    /// `configuration.url.connectTimeout`, `configuration.properties.options.geqo`…).
    pub key: String,
    /// Why.
    pub reason: PropertyDropReason,
}

impl ImportedConnection {
    /// The secret of a slot.
    #[must_use]
    pub fn secret(&self, slot: SecretSlot) -> Option<&ImportedSecret> {
        self.secrets.iter().find(|s| s.slot == slot)
    }
}

impl fmt::Debug for ImportedConnection {
    /// Property values and init SQL can carry sensitive text: keys and counts only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportedConnection")
            .field("name", &self.name)
            .field("engine", &self.engine)
            .field("dbeaver_provider", &self.dbeaver_provider)
            .field("dbeaver_driver", &self.dbeaver_driver)
            .field("endpoint", &self.endpoint)
            .field("auth", &self.auth)
            .field("ssh", &self.ssh)
            .field("tls", &self.tls)
            .field("connection_type", &self.connection_type)
            .field("read_only", &self.read_only)
            .field("folder", &self.folder)
            .field("property_keys", &self.properties.keys().collect::<Vec<_>>())
            .field("dropped_properties", &self.dropped_properties)
            .field("init_sql_count", &self.init_sql.len())
            .field("has_default_schema", &self.default_schema.is_some())
            .field("has_default_catalog", &self.default_catalog.is_some())
            .field("auto_commit", &self.auto_commit)
            .field("enabled", &self.enabled)
            .field("origin", &self.origin)
            .field("secrets", &self.secrets)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

pub(crate) fn import_project(project: &Project) -> ProjectImport {
    let mut issues = Issues::default();
    let mut sources = Vec::new();
    let mut source_files = Vec::new();
    let mut budget = MAX_PROJECT_BYTES;
    if project.data_source_files.len() > MAX_DATA_SOURCE_FILES {
        issues.warn(
            IssueKind::Truncated,
            None,
            format!(
                "the project has more than {MAX_DATA_SOURCE_FILES} data-sources files: only the \
                 first {MAX_DATA_SOURCE_FILES} are read"
            ),
        );
    }
    for path in project.data_source_files.iter().take(MAX_DATA_SOURCE_FILES) {
        if budget == 0 {
            issues.warn(
                IssueKind::Truncated,
                None,
                format!(
                    "more than {MAX_PROJECT_BYTES} bytes of data-sources files: the remaining \
                     files are not read"
                ),
            );
            break;
        }
        let limit = budget.min(MAX_DATA_SOURCES_BYTES);
        let parsed = fsread::read_limited(path, limit).map(|bytes| {
            budget = budget.saturating_sub(bytes.len() as u64);
            parse_data_sources(&bytes).map_err(|e| match e {
                crate::model::ParseError::InvalidJson { line, column } => {
                    crate::DbeaverError::InvalidJson {
                        path: path.clone(),
                        line,
                        column,
                    }
                }
                crate::model::ParseError::NotAnObject => {
                    crate::DbeaverError::NotAnObject { path: path.clone() }
                }
            })
        });
        match parsed.and_then(|r| r) {
            Ok(ds) => {
                sources.push(ds);
                source_files.push(path.clone());
            }
            Err(e) => issues.error(
                IssueKind::File,
                None,
                format!("{e}; its connections are not imported"),
            ),
        }
    }
    let (credentials, read_failed) = match &project.credentials_file {
        None => (None, false),
        Some(path) => match fsread::read_limited(path, MAX_CREDENTIALS_BYTES) {
            Ok(bytes) => (Some(decrypt_credentials(&bytes)), false),
            Err(e) => {
                issues.warn(
                    IssueKind::Credentials,
                    None,
                    format!("{e}; connections are imported without saved passwords"),
                );
                (None, true)
            }
        },
    };
    let mut out = map_sources(&project.name, &sources, credentials);
    drop(sources);
    if read_failed {
        out.credentials = CredentialsStatus::ReadFailed;
    }
    out.source_files = source_files;
    check_file_endpoints(&mut out, &project.workspace);
    issues.0.append(&mut out.issues);
    out.issues = issues.0;
    out
}

/// Canonicalizes file endpoints and forces read-only on database files that live inside the
/// DBeaver data directory (I4: NineSQL must never modify a file under a DBeaver directory).
///
/// Protected: the workspace and, when the workspace sits in a `DBeaverData` directory (or is
/// named `workspace6`), that whole parent. A file that does not exist is resolved through its
/// parent directory.
fn check_file_endpoints(out: &mut ProjectImport, workspace: &std::path::Path) {
    let mut roots = vec![workspace.to_path_buf()];
    if let Some(data) = workspace
        .ancestors()
        .skip(1)
        .find(|a| a.file_name().is_some_and(|n| n == "DBeaverData"))
    {
        roots.push(data.to_path_buf());
    } else if workspace.file_name().is_some_and(|n| n == "workspace6")
        && let Some(parent) = workspace.parent()
    {
        roots.push(parent.to_path_buf());
    }
    let canon_roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| fsread::canonicalize(r))
        .collect();
    roots.extend(canon_roots);
    for entry in &mut out.connections {
        let Outcome::Imported(c) = &mut entry.outcome else {
            continue;
        };
        let Endpoint::File { path } = &mut c.endpoint else {
            continue;
        };
        let lexical_inside = roots.iter().any(|r| path.starts_with(r));
        if let Some(canon) = canonicalize_maybe_missing(path) {
            *path = canon;
        }
        if lexical_inside || roots.iter().any(|r| path.starts_with(r)) {
            c.read_only = Some(true);
            entry.issues.push(ImportIssue::new(
                crate::report::Severity::Warning,
                IssueKind::FileInsideDbeaver,
                Some("configuration.database".to_owned()),
                "the database file is inside DBeaver's data directory: imported read-only, and \
                 the driver must open it in read-only mode (NineSQL never modifies DBeaver files)",
            ));
        }
    }
}

/// The canonical path, or for a missing file its canonical parent joined with the file name.
fn canonicalize_maybe_missing(path: &std::path::Path) -> Option<PathBuf> {
    if let Some(c) = fsread::canonicalize(path) {
        return Some(c);
    }
    let parent = fsread::canonicalize(path.parent()?)?;
    Some(parent.join(path.file_name()?))
}

/// Maps already parsed files of one project. `credentials` is `None` when the project has no
/// `credentials-config.json`.
///
/// **Use [`crate::import_project`] for real imports** (P2-C): this function does not know where
/// the DBeaver workspace is, so it skips the file-endpoint check that canonicalizes database
/// file paths and forces read-only on files inside DBeaver's data directory (I4). It exists for
/// tests, fuzzing and callers that run that check themselves.
pub fn map_sources(
    project: &str,
    sources: &[DataSources],
    credentials: Option<Result<Credentials, CredentialsError>>,
) -> ProjectImport {
    let budget = Budget::new(MAX_ISSUES);
    let mut issues = Issues::with_budget(&budget);
    let (credentials_status, mut creds) = match credentials {
        None => (CredentialsStatus::NotFound, None),
        Some(Ok(c)) => (
            CredentialsStatus::Decrypted {
                passwords: c.password_count(),
            },
            Some(c),
        ),
        Some(Err(e)) => {
            issues.warn(
                IssueKind::Credentials,
                Some("credentials-config.json".to_owned()),
                format!("{e}; connections are imported without saved passwords"),
            );
            (CredentialsStatus::Unreadable(e), None)
        }
    };
    if let Some(c) = &creds {
        for id in &c.invalid_entries {
            issues.warn(
                IssueKind::Credentials,
                None,
                format!(
                    "saved credentials of connection `{}` have an unknown structure",
                    ident(id, MAX_IDENT)
                ),
            );
        }
    }

    // Merge the files: first definition wins.
    let mut folder_defs: BTreeMap<&str, &FolderDef> = BTreeMap::new();
    let mut type_defs: BTreeMap<&str, &ConnectionTypeDef> = BTreeMap::new();
    let mut conn_defs: BTreeMap<&str, &ConnectionDef> = BTreeMap::new();
    let mut network_profiles = false;
    let mut truncated_connections = false;
    let mut truncated_folders = false;
    let mut truncated_types = false;
    for ds in sources {
        coverage::report_extra(Scope::TopLevel, "", &ds.extra, &mut issues);
        network_profiles |= ds.extra.get("network-profiles").is_some_and(|v| match v {
            serde_json::Value::Object(m) => !m.is_empty(),
            serde_json::Value::Array(a) => !a.is_empty(),
            serde_json::Value::Null => false,
            _ => true,
        });
        for key in &ds.invalid_entries {
            issues.warn(
                IssueKind::InvalidValue,
                Some(key.clone()),
                "not imported: not a JSON object",
            );
        }
        for f in &ds.folders {
            if folder_defs.len() >= MAX_FOLDERS && !folder_defs.contains_key(f.key.as_str()) {
                truncated_folders = true;
                continue;
            }
            folder_defs.entry(f.key.as_str()).or_insert(f);
        }
        for t in &ds.connection_types {
            if type_defs.len() >= MAX_CONNECTION_TYPES && !type_defs.contains_key(t.id.as_str()) {
                truncated_types = true;
                continue;
            }
            type_defs.entry(t.id.as_str()).or_insert(t);
        }
        for c in &ds.connections {
            if conn_defs.contains_key(c.id.as_str()) {
                issues.warn(
                    IssueKind::InvalidValue,
                    Some(format!("connections.{}", c.id)),
                    "defined in several data-sources files: only the first one is imported",
                );
            } else if conn_defs.len() >= MAX_CONNECTIONS {
                truncated_connections = true;
            } else {
                conn_defs.insert(&c.id, c);
            }
        }
    }
    if truncated_connections {
        issues.warn(
            IssueKind::Truncated,
            None,
            format!("more than {MAX_CONNECTIONS} connections: the others are not imported"),
        );
    }
    if truncated_types {
        issues.warn(
            IssueKind::Truncated,
            None,
            format!(
                "more than {MAX_CONNECTION_TYPES} connection types: the others are not imported \
                 (their connections get production rules)"
            ),
        );
    }
    if truncated_folders {
        issues.warn(
            IssueKind::Truncated,
            None,
            format!("more than {MAX_FOLDERS} folders: the others are not imported"),
        );
    }

    let mut folders = map_folders(&folder_defs, &mut issues);
    let types = map_connection_types(&type_defs, &mut issues);

    let ctx = Ctx {
        project,
        types: &types,
        credentials_unreadable: matches!(
            credentials_status,
            CredentialsStatus::Unreadable(_) | CredentialsStatus::ReadFailed
        ),
        network_profiles,
        budget: &budget,
    };
    let mut folder_paths: BTreeSet<Vec<String>> = folders.iter().map(|f| f.path.clone()).collect();
    let mut connections = Vec::with_capacity(conn_defs.len());
    for (id, def) in conn_defs {
        let saved = creds.as_mut().and_then(|c| c.connections.remove(id));
        let import = map_connection(def, saved, &ctx);
        if let Some(c) = import.imported()
            && !c.folder.is_empty()
            && folder_paths.len() < MAX_FOLDERS
            && folder_paths.insert(c.folder.clone())
        {
            folders.push(ImportedFolder {
                path: c.folder.clone(),
                description: None,
            });
        }
        connections.push(import);
    }
    if let Some(c) = &creds {
        for id in c.connections.keys() {
            issues.info(
                IssueKind::Credentials,
                None,
                format!(
                    "saved credentials for connection id `{}`, which is not in the \
                     data-sources files: not imported",
                    ident(id, MAX_IDENT)
                ),
            );
        }
    }
    folders.sort_by(|a, b| a.path.cmp(&b.path));
    folders.dedup_by(|a, b| a.path == b.path);

    let mut out = ProjectImport {
        project: project.to_owned(),
        source_files: Vec::new(),
        credentials: credentials_status,
        folders,
        connection_types: types.list,
        connections,
        issues: issues.0,
    };
    if budget.exhausted() {
        // One entry beyond the cap, so the report says it is incomplete. Connection state
        // (disabled, read-only, TLS mode) is never affected: only explanations are dropped.
        out.issues.push(ImportIssue::new(
            crate::report::Severity::Warning,
            IssueKind::Truncated,
            None,
            format!("more than {MAX_ISSUES} report entries: the others are not shown"),
        ));
    }
    tracing::debug!(
        connections = out.connections.len(),
        secrets = out.secret_count(),
        issues = out.issues.len(),
        "DBeaver project mapped"
    );
    out
}

struct Ctx<'a> {
    project: &'a str,
    types: &'a TypeTable,
    credentials_unreadable: bool,
    /// The files define DBeaver network profiles (shared SSH / proxy settings).
    network_profiles: bool,
    /// Report entries left for the project.
    budget: &'a Budget,
}

/// Most data-sources files read per project.
pub(crate) const MAX_DATA_SOURCE_FILES: usize = 64;
/// Most bytes of data-sources files read per project.
pub(crate) const MAX_PROJECT_BYTES: u64 = 64 * 1024 * 1024;
/// Most connections imported per project.
pub(crate) const MAX_CONNECTIONS: usize = 10_000;
/// Most connection types imported per project.
pub(crate) const MAX_CONNECTION_TYPES: usize = 1_000;
/// Most folders imported per project.
pub(crate) const MAX_FOLDERS: usize = 10_000;
/// Most report entries per project (project and connection issues together).
pub(crate) const MAX_ISSUES: usize = 50_000;

// ---------------------------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------------------------

const MAX_FOLDER_DEPTH: usize = 32;

fn map_folders(defs: &BTreeMap<&str, &FolderDef>, issues: &mut Issues) -> Vec<ImportedFolder> {
    let mut out = Vec::new();
    for (key, def) in defs {
        let prefix = format!("folders.{key}");
        coverage::report_extra(Scope::Folder, &prefix, &def.extra, issues);
        let path = folder_path(key, defs, 0);
        if path.is_empty() {
            issues.warn(
                IssueKind::InvalidValue,
                Some(prefix),
                "not imported: empty folder name",
            );
            continue;
        }
        out.push(ImportedFolder {
            path,
            description: def.description.clone().filter(|d| !d.trim().is_empty()),
        });
    }
    out
}

/// Full path of a folder: a key with `/` is a full path (26.x); otherwise the parent chain is
/// followed (up to 25.x). Cycles and excessive depth stop the chain.
fn folder_path(key: &str, defs: &BTreeMap<&str, &FolderDef>, depth: usize) -> Vec<String> {
    let own = split_folder(key);
    if key.contains('/') || depth >= MAX_FOLDER_DEPTH {
        return own;
    }
    let Some(parent) = defs.get(key).and_then(|d| d.parent.as_deref()) else {
        return own;
    };
    if parent == key {
        return own;
    }
    let mut path = if defs.contains_key(parent) {
        folder_path(parent, defs, depth + 1)
    } else {
        split_folder(parent)
    };
    path.extend(own);
    path
}

fn split_folder(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.trim().is_empty())
        .map(|s| sanitize_text(s).0)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Connection types
// ---------------------------------------------------------------------------------------------

struct TypeTable {
    list: Vec<ImportedConnectionType>,
    /// DBeaver id → index in `list`.
    by_dbeaver_id: BTreeMap<String, usize>,
}

impl TypeTable {
    fn get(&self, dbeaver_id: &str) -> Option<&ImportedConnectionType> {
        self.by_dbeaver_id.get(dbeaver_id).map(|&i| &self.list[i])
    }
}

const BUILTIN_TYPES: &[&str] = &["dev", "test", "prod"];

/// DBeaver 22.3.5 defaults of the built-in types: (auto-commit, confirm-execute,
/// confirm-data-change). Custom types default to (true, false, false).
fn type_defaults(id: &str) -> (bool, bool, bool) {
    match id {
        "prod" => (false, true, true),
        "test" => (true, false, true),
        _ => (true, false, false),
    }
}

fn map_connection_types(
    defs: &BTreeMap<&str, &ConnectionTypeDef>,
    issues: &mut Issues,
) -> TypeTable {
    let mut table = TypeTable {
        list: Vec::new(),
        by_dbeaver_id: BTreeMap::new(),
    };
    let mut used: BTreeSet<String> = BTreeSet::new();
    // Next suffix to try per base id, so that many types sanitizing to the same base stay
    // linear (each suffix is tried once overall).
    let mut next_suffix: BTreeMap<String, usize> = BTreeMap::new();
    for (dbeaver_id, def) in defs {
        let prefix = format!("connection-types.{dbeaver_id}");
        let builtin = BUILTIN_TYPES.contains(dbeaver_id);
        let mut id = sanitize_type_id(dbeaver_id);
        let base = id.clone();
        while used.contains(&id) || (!builtin && BUILTIN_TYPES.contains(&id.as_str())) {
            let n = next_suffix.entry(base.clone()).or_insert(2);
            id = format!("{}-{n}", truncate_bytes(&base, 60));
            *n += 1;
        }
        if id != *dbeaver_id {
            issues.info(
                IssueKind::InvalidValue,
                Some(prefix.clone()),
                format!("connection type id imported as `{id}` (NineSQL ids are [a-z0-9_-])"),
            );
        }
        used.insert(id.clone());
        let (d_auto, d_exec, d_change) = type_defaults(dbeaver_id);
        let production = *dbeaver_id == "prod";
        let confirm_execute = def.confirm_execute.unwrap_or(d_exec);
        let confirm_change = def.confirm_data_change.unwrap_or(d_change);
        let color = map_color(def.color.as_deref(), dbeaver_id, &prefix, issues);
        if let Some(dark) = def.color_dark.as_deref()
            && Some(dark) != def.color.as_deref()
        {
            issues.info(
                IssueKind::Unsupported,
                Some(join(&prefix, "colorDark")),
                "not imported: NineSQL uses one color per connection type in both themes",
            );
        }
        if def
            .description
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty())
            && !builtin
        {
            issues.info(
                IssueKind::Unsupported,
                Some(join(&prefix, "description")),
                "not imported: connection types have no description in NineSQL",
            );
        }
        report_type_extra(def, &prefix, issues);
        let name = def
            .name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .map_or_else(|| (*dbeaver_id).to_owned(), |n| sanitize_text(n).0);
        table
            .by_dbeaver_id
            .insert((*dbeaver_id).to_owned(), table.list.len());
        table.list.push(ImportedConnectionType {
            id,
            dbeaver_id: (*dbeaver_id).to_owned(),
            builtin,
            name,
            color,
            auto_commit: def.auto_commit.unwrap_or(d_auto),
            confirm_destructive: confirm_execute || confirm_change || production,
            confirm_every_write: confirm_execute,
            read_only: false,
            log_query_text: !production,
            production,
        });
    }
    table
}

/// Unconsumed keys of a connection type: reported only when they enable a behavior NineSQL
/// lacks (a disabled DBeaver feature is faithfully represented by its absence).
fn report_type_extra(def: &ConnectionTypeDef, prefix: &str, issues: &mut Issues) {
    for (key, value) in def.extra.iter() {
        let truthy = || match value {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::String(s) => s.eq_ignore_ascii_case("true"),
            _ => false,
        };
        let reason = match key {
            "smart-commit" | "smart-commit-recover" if truthy() => {
                Some("smart commit is not supported yet (F-CON-03)")
            }
            "auto-close-transactions" | "auto-close-connections" if truthy() => {
                Some("closing idle transactions and connections is not supported yet (F-CON-10)")
            }
            "smart-commit"
            | "smart-commit-recover"
            | "auto-close-transactions"
            | "auto-close-connections"
            | "close-transactions-period"
            | "close-connections-period" => None,
            "security" => {
                let restricted = value
                    .get("permission-restrictions")
                    .and_then(serde_json::Value::as_array)
                    .is_none_or(|a| !a.is_empty());
                restricted.then_some(
                    "permission restrictions are not supported yet (F-CON-11); use the \
                     read-only flag",
                )
            }
            _ => {
                coverage::report_key(Scope::ConnectionType, prefix, key, issues);
                None
            }
        };
        if let Some(reason) = reason {
            issues.warn(
                IssueKind::Unsupported,
                Some(join(prefix, key)),
                format!("not imported: {reason}"),
            );
        }
    }
}

/// DBeaver 26.x RGB values of the built-in types, used for the theme color keys of 22.x–25.x.
fn theme_color(key: &str) -> Option<Rgb> {
    let name = key
        .strip_prefix("org.jkiss.dbeaver.color.connectionType.")?
        .strip_suffix(".background")?;
    match name {
        "prod" => Some(Rgb {
            r: 250,
            g: 207,
            b: 207,
        }),
        "qa" | "test" => Some(Rgb {
            r: 214,
            g: 250,
            b: 207,
        }),
        "dev" => Some(Rgb {
            r: 255,
            g: 255,
            b: 255,
        }),
        _ => None,
    }
}

fn map_color(color: Option<&str>, type_id: &str, prefix: &str, issues: &mut Issues) -> Rgb {
    let fallback = match type_id {
        "prod" => theme_color("org.jkiss.dbeaver.color.connectionType.prod.background"),
        "test" => theme_color("org.jkiss.dbeaver.color.connectionType.qa.background"),
        _ => None,
    }
    .unwrap_or(Rgb {
        r: 255,
        g: 255,
        b: 255,
    });
    let Some(color) = color else {
        return fallback;
    };
    if let Some(rgb) = parse_rgb(color) {
        return rgb;
    }
    if let Some(rgb) = theme_color(color.trim()) {
        return rgb;
    }
    issues.warn(
        IssueKind::InvalidValue,
        Some(join(prefix, "color")),
        format!("unrecognized color: {fallback} used"),
    );
    fallback
}

/// `R,G,B` (spaces allowed); a fourth (alpha) component is ignored.
fn parse_rgb(text: &str) -> Option<Rgb> {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    if !(parts.len() == 3 || parts.len() == 4) {
        return None;
    }
    let c = |i: usize| parts.get(i).and_then(|p| p.parse::<u8>().ok());
    Some(Rgb {
        r: c(0)?,
        g: c(1)?,
        b: c(2)?,
    })
}

/// A NineSQL connection type id from a DBeaver one: lower case, `[a-z0-9_-]`, 64 bytes at most,
/// starting with a letter or digit.
fn sanitize_type_id(id: &str) -> String {
    let mut out: String = id
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out = out
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_owned();
    out.truncate(64);
    if out.is_empty() {
        "imported".to_owned()
    } else {
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Engines
// ---------------------------------------------------------------------------------------------

enum EngineMatch {
    /// In the v1 set. `verified`: covered by a P0-E fixture.
    Supported { engine: EngineId, verified: bool },
    /// Outside the v1 set or unknown.
    Unsupported,
}

/// DBeaver `provider` / `driver` → NineSQL engine (docs/06-dbeaver-interop.md).
fn match_engine(provider: &str, driver: &str) -> EngineMatch {
    let p = provider.to_ascii_lowercase();
    let d = driver.to_ascii_lowercase();
    let supported = |engine: EngineId, verified: bool| EngineMatch::Supported { engine, verified };
    match (p.as_str(), d.as_str()) {
        ("postgresql", "postgres-jdbc") => supported(EngineId::POSTGRESQL, true),
        // "PostgreSQL (Old)" driver of DBeaver's PostgreSQL plugin (pgjdbc too).
        ("postgresql", "postgresql") => supported(EngineId::POSTGRESQL, false),
        ("postgresql", "postgres-timescale-jdbc") => supported(EngineId::TIMESCALEDB, true),
        ("postgresql", d) if d.contains("yugabyte") => supported(EngineId::YUGABYTEDB, false),
        ("mysql", "mysql8") => supported(EngineId::MYSQL, true),
        ("mysql", "mysql5") => supported(EngineId::MYSQL, false),
        ("mysql", "mariadb") => supported(EngineId::MARIADB, true),
        // DBeaver's TiDB plugin declares provider `tidb`, driver `tidb` (Connector/J).
        ("tidb" | "mysql" | "generic", "tidb") => supported(EngineId::TIDB, false),
        ("generic" | "questdb", "questdb") => supported(EngineId::QUESTDB, false),
        ("sqlite", "sqlite_jdbc") => supported(EngineId::SQLITE, true),
        ("generic", "sqlite_jdbc") => supported(EngineId::SQLITE, false),
        // Not `duckdb` / `motherduck_jdbc`: MotherDuck is a cloud service, not a local file.
        ("duckdb" | "generic", "duckdb_jdbc") => supported(EngineId::DUCKDB, false),
        ("clickhouse", "com_clickhouse") => supported(EngineId::CLICKHOUSE, true),
        ("clickhouse" | "generic", "yandex_clickhouse") => supported(EngineId::CLICKHOUSE, false),
        ("generic" | "trino", "trino_jdbc") => supported(EngineId::TRINO, false),
        // Not `jaybird_embedded` (an embedded engine on a local file).
        (
            "jaybird" | "generic",
            "jaybird" | "firebird" | "firebird_jaybird" | "firebird_jaybird3",
        ) => supported(EngineId::FIREBIRD, false),
        _ => EngineMatch::Unsupported,
    }
}

fn is_file_engine(engine: &EngineId) -> bool {
    *engine == EngineId::SQLITE || *engine == EngineId::DUCKDB
}

/// Engine id of a disabled connection, derived from the DBeaver provider (core `EngineId` rule:
/// lower-case letter first, then `[a-z0-9_-]`, 64 bytes at most).
fn fallback_engine_id(provider: &str) -> EngineId {
    let mut id: String = provider
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    id = id
        .trim_start_matches(|c: char| !c.is_ascii_lowercase())
        .to_owned();
    id.truncate(64);
    if id.is_empty() {
        EngineId::from_static("unknown")
    } else {
        EngineId::new(id)
    }
}

// ---------------------------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------------------------

const MAX_SHORT_TEXT: usize = 255;

/// Replaces control characters by spaces and truncates to 255 bytes (core `short_text` rule).
/// Returns whether the text changed.
fn sanitize_text(text: &str) -> (String, bool) {
    let replaced: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let out = truncate_bytes(&replaced, MAX_SHORT_TEXT).to_owned();
    let changed = out != text;
    (out, changed)
}

fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn is_valid_short(text: &str) -> bool {
    text.len() <= MAX_SHORT_TEXT && !text.chars().any(char::is_control)
}

/// Core host rule (non-empty, short, no whitespace, `@`, `/` or `\`), and no leading `-` so a
/// host can never be read as a command-line option by a tool it is passed to.
fn is_valid_host(host: &str) -> bool {
    !host.trim().is_empty()
        && !host.starts_with('-')
        && is_valid_short(host)
        && !host
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '@' | '/' | '\\'))
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// State built while mapping one connection.
struct ConnState {
    issues: Issues,
    disabled: Vec<String>,
    secrets: Vec<ImportedSecret>,
    tls: TlsHints,
    username_hint: Option<String>,
    read_only: Option<bool>,
    properties: BTreeMap<String, String>,
    /// Property rules of the JDBC driver (set once the engine is known).
    rules: properties::Rules,
    /// Converted properties: NineSQL name → (precedence, value, DBeaver key path).
    mapped: BTreeMap<&'static str, (properties::Priority, String, String)>,
    dropped: Vec<DroppedProperty>,
    /// Connector/J `connectionTimeZone` / `serverTimezone` (path, value).
    cj_time_zone: Option<(String, String)>,
    /// Connector/J `forceConnectionTimeZoneToSession` (path, value).
    cj_force_time_zone: Option<(String, bool)>,
    /// Driver property keys (ASCII lower case) and values, read before the URL parameters so
    /// that a URL parameter the JDBC driver would override is skipped.
    property_values: BTreeMap<String, String>,
    /// URL parameter keys (ASCII lower case) and values already applied.
    url_values: BTreeMap<String, String>,
    /// Dropped properties past [`MAX_DROPPED_PROPERTIES`] (counted, not listed).
    dropped_overflow: usize,
}

/// Most entries of [`ImportedConnection::dropped_properties`] per connection.
const MAX_DROPPED_PROPERTIES: usize = 1000;
/// Key path prefix of JDBC URL parameters.
const URL_PREFIX: &str = "configuration.url";

fn failed(dbeaver_id: &str, name: String, reason: &str, mut issues: Issues) -> ConnectionImport {
    issues.error(
        IssueKind::InvalidValue,
        None,
        format!("not imported: {reason}"),
    );
    ConnectionImport {
        dbeaver_id: dbeaver_id.to_owned(),
        name,
        outcome: Outcome::Failed {
            reason: reason.to_owned(),
        },
        issues: issues.0,
    }
}

fn map_connection(
    def: &ConnectionDef,
    mut saved: Option<ConnectionCredentials>,
    ctx: &Ctx<'_>,
) -> ConnectionImport {
    let mut st = ConnState {
        issues: Issues::with_budget(ctx.budget),
        disabled: Vec::new(),
        secrets: Vec::new(),
        tls: TlsHints::default(),
        username_hint: None,
        read_only: None,
        properties: BTreeMap::new(),
        rules: properties::Rules::Keep,
        mapped: BTreeMap::new(),
        dropped: Vec::new(),
        cj_time_zone: None,
        cj_force_time_zone: None,
        property_values: BTreeMap::new(),
        url_values: BTreeMap::new(),
        dropped_overflow: 0,
    };
    let id = def.id.as_str();

    // Name.
    let name = match non_empty(def.name.as_deref()) {
        Some(n) => {
            let (n, changed) = sanitize_text(n);
            if changed {
                st.issues.info(
                    IssueKind::InvalidValue,
                    Some("name".to_owned()),
                    "control characters replaced or name shortened to 255 bytes",
                );
            }
            n
        }
        None => {
            st.issues.warn(
                IssueKind::InvalidValue,
                Some("name".to_owned()),
                "missing name: the DBeaver id is used",
            );
            sanitize_text(id).0
        }
    };
    if id.trim().is_empty() || !is_valid_short(id) {
        return failed(
            id,
            name,
            "the DBeaver connection id is empty or too long",
            st.issues,
        );
    }

    coverage::report_extra(Scope::Connection, "", &def.extra, &mut st.issues);
    let cfg = &def.configuration;
    coverage::report_extra(
        Scope::Configuration,
        "configuration",
        &cfg.extra,
        &mut st.issues,
    );

    // Engine.
    let provider = def.provider.clone().unwrap_or_default();
    let driver = def.driver.clone().unwrap_or_default();
    let (shown_provider, shown_driver) = (ident(&provider, MAX_IDENT), ident(&driver, MAX_IDENT));
    let engine = match match_engine(&provider, &driver) {
        EngineMatch::Supported { engine, verified } => {
            if !verified {
                st.issues.info(
                    IssueKind::UnsupportedEngine,
                    Some("driver".to_owned()),
                    format!(
                        "DBeaver driver `{shown_provider}` / `{shown_driver}` mapped to `{engine}`; this \
                         mapping is not yet verified against a DBeaver-written workspace"
                    ),
                );
            }
            engine
        }
        EngineMatch::Unsupported => {
            st.issues.warn(
                IssueKind::UnsupportedEngine,
                Some("driver".to_owned()),
                format!(
                    "DBeaver driver `{shown_provider}` / `{shown_driver}` is not supported by NineSQL v1: \
                     imported disabled (ADR-0006 §5)"
                ),
            );
            st.disabled
                .push("engine not supported by NineSQL v1 (ADR-0003)".to_owned());
            fallback_engine_id(&provider)
        }
    };
    let file_engine = is_file_engine(&engine);
    st.rules = properties::Rules::for_connection(&engine, &driver);

    // Endpoint (and URL parameters, which go through the property rules).
    st.property_values = cfg
        .properties
        .entries
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.as_text().into_owned()))
        .collect();
    let endpoint = match map_endpoint(def, file_engine, &mut st) {
        Ok(e) => e,
        Err(reason) => return failed(id, name, reason, st.issues),
    };

    // Driver properties.
    for key in &cfg.properties.non_scalar {
        let path = format!("configuration.properties.{key}");
        st.issues.warn(
            IssueKind::InvalidValue,
            Some(path.clone()),
            "not imported: not a scalar value",
        );
        st.drop_property(path, PropertyDropReason::InvalidValue);
    }
    for (key, value) in &cfg.properties.entries {
        let text = value.as_text();
        apply_property("configuration.properties", key, &text, &mut st);
    }
    finish_properties(&mut st);
    for (key, _) in &cfg.provider_properties.entries {
        st.issues.info(
            IssueKind::Unsupported,
            Some(format!("configuration.provider-properties.{key}")),
            "not imported: DBeaver-specific provider setting",
        );
    }
    for key in &cfg.provider_properties.non_scalar {
        st.issues.info(
            IssueKind::Unsupported,
            Some(format!("configuration.provider-properties.{key}")),
            "not imported: DBeaver-specific provider setting",
        );
    }

    // Connection type.
    let dbeaver_type = non_empty(cfg.type_id.as_deref()).unwrap_or("dev");
    let (connection_type, type_auto_commit) = match ctx.types.get(dbeaver_type) {
        Some(t) => (t.id.clone(), Some(t.auto_commit)),
        None => {
            let nine = sanitize_type_id(dbeaver_type);
            if !BUILTIN_TYPES.contains(&nine.as_str()) {
                st.issues.warn(
                    IssueKind::InvalidValue,
                    Some("configuration.type".to_owned()),
                    format!(
                        "connection type `{}` is not defined in the file: NineSQL applies \
                         production rules until a type `{nine}` exists",
                        ident(dbeaver_type, MAX_IDENT)
                    ),
                );
            }
            let auto = BUILTIN_TYPES
                .contains(&nine.as_str())
                .then(|| type_defaults(&nine).0);
            (nine, auto)
        }
    };

    // Bootstrap.
    let boot = &cfg.bootstrap;
    coverage::report_extra(
        Scope::Bootstrap,
        "configuration.bootstrap",
        &boot.extra,
        &mut st.issues,
    );
    let default_schema = non_empty(boot.default_schema.as_deref()).map(str::to_owned);
    let default_catalog = non_empty(boot.default_catalog.as_deref()).map(str::to_owned);
    if default_schema.is_some() {
        st.issues.warn(
            IssueKind::Unsupported,
            Some("configuration.bootstrap.defaultSchema".to_owned()),
            "not applied yet: the NineSQL connection model has no default schema setting",
        );
    }
    if default_catalog.is_some() {
        st.issues.warn(
            IssueKind::Unsupported,
            Some("configuration.bootstrap.defaultCatalog".to_owned()),
            "not applied yet: the NineSQL connection model has no default catalog setting",
        );
    }
    if let Some(auto) = boot.autocommit
        && type_auto_commit != Some(auto)
    {
        st.issues.warn(
            IssueKind::Unsupported,
            Some("configuration.bootstrap.autocommit".to_owned()),
            format!(
                "not applied yet: per-connection auto-commit is not supported; the connection \
                 type's default applies (DBeaver had auto-commit {})",
                if auto { "on" } else { "off" }
            ),
        );
    }
    if boot.invalid_queries > 0 {
        st.issues.warn(
            IssueKind::InvalidValue,
            Some("configuration.bootstrap.query".to_owned()),
            format!(
                "{} init queries are not strings: not imported",
                boot.invalid_queries
            ),
        );
    }
    let init_sql: Vec<String> = boot
        .queries
        .iter()
        .filter(|q| !q.trim().is_empty())
        .cloned()
        .collect();
    if !init_sql.is_empty() {
        st.issues.warn(
            IssueKind::InitSql,
            Some("configuration.bootstrap.query".to_owned()),
            format!(
                "{} init statement(s) imported: they run on every connect; review them before \
                 connecting",
                init_sql.len()
            ),
        );
    }

    // Database credentials.
    let auth = map_auth(def, file_engine, saved.as_mut(), ctx, &mut st);

    // Handlers.
    // Handler data that is not an object cannot be read: it may configure a tunnel or proxy,
    // so the connection is disabled (fail closed, I2).
    let mut unreadable_handlers: Vec<String> = cfg
        .invalid_handlers
        .iter()
        .map(|hid| format!("configuration.handlers.{hid}"))
        .collect();
    if cfg.extra.get("handlers").is_some_and(|v| !is_empty_json(v)) {
        unreadable_handlers.push("configuration.handlers".to_owned());
    }
    for key in &unreadable_handlers {
        st.issues.warn(
            IssueKind::Disabled,
            Some(key.clone()),
            "network handler data that is not a JSON object: imported disabled so that it never \
             bypasses a tunnel or proxy it may configure",
        );
    }
    if !unreadable_handlers.is_empty() {
        st.disabled
            .push("unreadable network handler settings".to_owned());
    }
    let mut ssh = None;
    for h in &cfg.handlers {
        let prefix = format!("configuration.handlers.{}", h.id);
        coverage::report_extra(Scope::Handler, &prefix, &h.extra, &mut st.issues);
        // An `enabled` value of an unexpected type counts as enabled (fail closed).
        let enabled = h
            .enabled
            .unwrap_or_else(|| h.extra.get("enabled").is_some());
        if !enabled {
            // Saved but switched off in DBeaver: not used, so not imported.
            continue;
        }
        match h.id.as_str() {
            "ssh_tunnel" => {
                let section = saved.as_mut().and_then(|s| s.sections.remove(SSH_SECTION));
                match map_ssh(h, section, &prefix, &endpoint, &mut st) {
                    Ok(t) => ssh = Some(t),
                    Err(reason) => return failed(id, name, reason, st.issues),
                }
            }
            "postgre_ssl" => tls_from_postgres_handler(h, &prefix, &mut st),
            "mysql_ssl" => tls_from_mysql_handler(h, &prefix, &mut st),
            other if is_harmless_tls_handler(other, h.kind.as_deref()) => {
                st.issues.warn(
                    IssueKind::Unsupported,
                    Some(prefix),
                    "TLS handler not supported yet: its settings are not imported",
                );
            }
            other => {
                // Fail closed (I2): any other enabled handler may route traffic (tunnel, proxy,
                // cloud forwarding…); connecting without it would bypass it.
                st.issues.warn(
                    IssueKind::Disabled,
                    Some(prefix),
                    "network handler not supported yet: the connection is imported disabled so \
                     that it never bypasses the tunnel, proxy or forwarding it configures",
                );
                st.disabled.push(format!(
                    "network handler `{}` not supported yet",
                    ident(other, MAX_IDENT)
                ));
            }
        }
    }

    // Network profiles (shared SSH / proxy settings, F-CON-17): not supported yet, so a
    // connection that may use one is disabled rather than imported without its hops (I2).
    // DBeaver writes `configuration."config-profile"` (and `"config-profile-source"`, e.g.
    // `global`: the profile may live outside this file), so any non-empty value under a key
    // containing `profile` disables, whether or not the file defines `network-profiles`.
    // The same value test applies in every scope (connection, `configuration`, handler, handler
    // properties): see `is_profile_reference`.
    let profile_like = |key: &str| {
        let norm = normalize_key(key);
        norm.contains("profile") || (ctx.network_profiles && norm.starts_with("network"))
    };
    let mut profile_keys: Vec<String> = Vec::new();
    let mut scopes: Vec<(String, &crate::model::Extra)> = vec![
        (String::new(), &def.extra),
        ("configuration".to_owned(), &cfg.extra),
    ];
    for h in &cfg.handlers {
        scopes.push((format!("configuration.handlers.{}", h.id), &h.extra));
        let props_prefix = format!("configuration.handlers.{}.properties", h.id);
        for (key, value) in &h.properties.entries {
            if profile_like(key) && is_profile_reference_scalar(value) {
                profile_keys.push(join(&props_prefix, key));
            }
        }
        for key in &h.properties.non_scalar {
            if profile_like(key) && !h.properties.empty_non_scalar.contains(key) {
                profile_keys.push(join(&props_prefix, key));
            }
        }
    }
    for (scope, extra) in scopes {
        for (key, value) in extra.iter() {
            if profile_like(key) && is_profile_reference(value) {
                profile_keys.push(join(&scope, key));
            }
        }
    }
    for key in &profile_keys {
        st.issues.warn(
            IssueKind::Disabled,
            Some(key.clone()),
            "the connection uses a DBeaver network profile, which is not supported yet \
             (F-CON-17): imported disabled so that it never bypasses the profile's tunnel or \
             proxy",
        );
    }
    if !profile_keys.is_empty() {
        st.disabled
            .push("DBeaver network profile not supported yet".to_owned());
    }

    // Saved credentials left over (sections NineSQL does not use).
    if let Some(saved) = saved {
        for section in saved.invalid_sections {
            st.issues.warn(
                IssueKind::Credentials,
                Some(section),
                "saved credentials with an unknown structure: not imported",
            );
        }
        for (section, values) in saved.sections {
            if values.user.is_some() || values.password.is_some() || !values.other_keys.is_empty() {
                st.issues.warn(
                    IssueKind::Credentials,
                    Some(section),
                    "saved credentials of a handler NineSQL does not import: not imported",
                );
            }
        }
    }

    // TLS.
    let tls = if matches!(endpoint, Endpoint::Tcp { .. }) {
        resolve_tls(&mut st)
    } else {
        TlsConfig::default()
    };

    let read_only = if def.read_only == Some(true) || st.read_only == Some(true) {
        Some(true)
    } else {
        None
    };
    let folder = def.folder.as_deref().map(split_folder).unwrap_or_default();
    let description = non_empty(def.description.as_deref()).map(str::to_owned);
    let enabled = st.disabled.is_empty();
    let disabled_reason = (!enabled).then(|| st.disabled.join("; "));

    let conn = ImportedConnection {
        name: name.clone(),
        description,
        engine,
        dbeaver_provider: provider,
        dbeaver_driver: driver,
        endpoint,
        auth,
        ssh,
        tls,
        connection_type,
        read_only,
        folder,
        properties: st.properties,
        dropped_properties: st.dropped,
        init_sql,
        default_schema,
        default_catalog,
        auto_commit: boot.autocommit,
        enabled,
        disabled_reason,
        origin: DbeaverOrigin {
            dbeaver_id: id.to_owned(),
            project: ctx.project.to_owned(),
        },
        secrets: st.secrets,
    };
    ConnectionImport {
        dbeaver_id: id.to_owned(),
        name,
        outcome: Outcome::Imported(Box::new(conn)),
        issues: st.issues.0,
    }
}

fn map_endpoint(
    def: &ConnectionDef,
    file_engine: bool,
    st: &mut ConnState,
) -> Result<Endpoint, &'static str> {
    let cfg = &def.configuration;
    let url_mode = cfg
        .configuration_type
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case("URL"));
    let host_field = non_empty(cfg.host.as_deref());
    let database_field = non_empty(cfg.database.as_deref());
    // The URL is used in URL mode, or when the fields it would duplicate are empty: in manual
    // mode DBeaver derives the URL from the host / database fields, so it is ignored then.
    let use_url = url_mode
        || if file_engine {
            database_field.is_none()
        } else {
            host_field.is_none()
        };
    let url = non_empty(cfg.url.as_ref().map(|u| u.expose_secret())).filter(|_| use_url);
    let parsed = match url {
        Some(u) => match jdbc::parse(u) {
            Some(p) => Some(p),
            None => {
                st.issues.warn(
                    IssueKind::InvalidValue,
                    Some("configuration.url".to_owned()),
                    "not a JDBC URL: not imported",
                );
                None
            }
        },
        None => None,
    };
    if let Some(p) = &parsed {
        if p.had_password {
            st.issues.warn(
                IssueKind::SecretRemoved,
                Some("configuration.url".to_owned()),
                "the URL contained a password: removed, not imported (enter it again in NineSQL)",
            );
        }
        if p.extra_hosts {
            st.issues.warn(
                IssueKind::Unsupported,
                Some("configuration.url".to_owned()),
                "fail-over host list: only the first host is imported",
            );
        }
        if let Some(u) = &p.user {
            st.username_hint = Some(u.clone());
        }
        for (k, v) in &p.params {
            apply_property("configuration.url", k, v, st);
        }
    }
    if non_empty(cfg.server.as_deref()).is_some() {
        st.issues.warn(
            IssueKind::Unsupported,
            Some("configuration.server".to_owned()),
            "not imported: server / instance names are not supported",
        );
    }

    if file_engine {
        let path = if url_mode {
            parsed.as_ref().and_then(|p| p.file_path.clone())
        } else {
            None
        }
        .or_else(|| database_field.map(str::to_owned))
        .or_else(|| parsed.as_ref().and_then(|p| p.file_path.clone()));
        let Some(path) = path.filter(|p| !p.trim().is_empty()) else {
            return Err("no database file path");
        };
        if path.trim() == ":memory:" || path.trim_start().starts_with(":memory:") {
            return Err("in-memory databases cannot be imported");
        }
        let path = path.trim();
        if path.contains("${") {
            return Err("the database file path contains a DBeaver variable (`${…}`)");
        }
        if !std::path::Path::new(path).is_absolute() {
            return Err("the database file path is not absolute");
        }
        return Ok(Endpoint::File {
            path: PathBuf::from(path),
        });
    }

    let from_url = parsed.as_ref().filter(|p| p.host.is_some());
    let (host, port_text, database) =
        if let Some(p) = from_url.filter(|_| url_mode || host_field.is_none()) {
            (
                p.host.clone().unwrap_or_default(),
                p.port.as_ref().map(|t| (t.clone(), t.parse::<u16>().ok())),
                p.database
                    .clone()
                    .or_else(|| database_field.map(str::to_owned)),
            )
        } else if let Some(h) = host_field {
            (
                h.to_owned(),
                cfg.port
                    .as_ref()
                    .map(|p| (p.as_text().into_owned(), p.as_u16())),
                database_field.map(str::to_owned),
            )
        } else if parsed.is_some() {
            return Err("no host (neither a host field nor a host in the JDBC URL)");
        } else {
            return Err("no host and no JDBC URL");
        };
    let host = host.trim().to_owned();
    if !is_valid_host(&host) {
        return Err("the host is not a valid host name or IP address");
    }
    let port = match port_text {
        None => None,
        Some((text, _)) if text.trim().is_empty() => None,
        Some((_, Some(p))) if p != 0 => Some(p),
        Some(_) => {
            st.issues.warn(
                IssueKind::InvalidValue,
                Some("configuration.port".to_owned()),
                "not a valid port: the engine's default port is used",
            );
            None
        }
    };
    let database = match database {
        Some(db) if is_valid_short(&db) => Some(db),
        Some(_) => {
            st.issues.warn(
                IssueKind::InvalidValue,
                Some("configuration.database".to_owned()),
                "database name too long or with control characters: not imported",
            );
            None
        }
        None => None,
    };
    Ok(Endpoint::Tcp {
        host,
        port,
        database,
    })
}

/// Whether a JSON value is empty (`null`, blank string, `[]`, `{}`).
fn is_empty_json(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.trim().is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Object(m) => m.is_empty(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => false,
    }
}

/// Whether the value of a profile-like key may reference a DBeaver network profile. Fail closed:
/// only `null`, the empty string and an empty array or object do not. A blank string (`" "`)
/// counts, because DBeaver does not trim the value it looks a profile up by.
fn is_profile_reference(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(m) => !m.is_empty(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => true,
    }
}

/// [`is_profile_reference`] for a scalar property value.
fn is_profile_reference_scalar(value: &Scalar) -> bool {
    match value {
        Scalar::Text(s) => !s.is_empty(),
        Scalar::Bool(_) | Scalar::Number(_) => true,
    }
}

/// A TLS-only handler that cannot route traffic: `type` exactly `CONFIG`, an id that names TLS,
/// and nothing in the id that suggests a tunnel, proxy or forwarding.
fn is_harmless_tls_handler(id: &str, kind: Option<&str>) -> bool {
    let lower = id.to_ascii_lowercase();
    kind == Some("CONFIG")
        && (lower.contains("ssl") || lower.contains("tls"))
        && !["tunnel", "ssh", "proxy", "socks", "ssm", "iap", "forward"]
            .iter()
            .any(|w| lower.contains(w))
}

/// Whether a property looks like it holds a secret: a secret-like key, or a benign
/// password-named key (`passwordCharacterEncoding`…) whose value does not have the expected
/// shape (a misplaced password).
fn is_secret_like(key: &str, value: &str) -> bool {
    is_secret_like_key(key) || is_secret_like_entry(key, value)
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// One driver property or URL parameter: TLS keys feed the TLS settings, `user` the user name,
/// `readOnly=true` the read-only flag; secret-like keys are dropped; for engines a NineSQL
/// driver family serves, the property is converted to NineSQL property names or dropped
/// ([`properties`]); otherwise reserved keys are dropped and the rest is kept.
fn apply_property(prefix: &str, key: &str, value: &str, st: &mut ConnState) {
    let path = join(prefix, key);
    if key.trim().is_empty() || !is_valid_short(key) {
        st.issues.warn(
            IssueKind::InvalidValue,
            Some(prefix.to_owned()),
            "a driver property with an empty, too long or invalid key was not imported",
        );
        st.drop_property(prefix.to_owned(), PropertyDropReason::InvalidValue);
        return;
    }
    let norm = normalize_key(key);
    if st.tls.absorb_property(&norm, value, &path, &mut st.issues) {
        return;
    }
    match norm.as_str() {
        "user" | "username" => {
            if !value.trim().is_empty() {
                st.username_hint
                    .get_or_insert_with(|| value.trim().to_owned());
            }
            return;
        }
        "readonly" => {
            if value.trim().eq_ignore_ascii_case("true") {
                st.read_only = Some(true);
            }
            return;
        }
        _ => {}
    }
    if is_secret_like(key, value) {
        st.secret_removed(path);
        return;
    }
    if st.rules != properties::Rules::Keep && same_key_overridden(prefix, key, value, st) {
        return;
    }
    if is_reserved_property(key) && !properties::handles_reserved(st.rules, key) {
        st.issues.warn(
            IssueKind::ReservedProperty,
            Some(path.clone()),
            "not imported: reserved for NineSQL's typed settings (TLS, endpoint, credentials, \
             read-only, init SQL); set it there",
        );
        st.drop_property(path, PropertyDropReason::Reserved);
        return;
    }
    if let Some(actions) = properties::map(st.rules, &path, key, value) {
        for action in actions {
            st.apply_action(action);
        }
        return;
    }
    if st
        .properties
        .insert(key.to_owned(), value.to_owned())
        .is_some()
    {
        st.issues.info(
            IssueKind::InvalidValue,
            Some(path),
            "set both in the URL and in the driver properties: the driver property is kept",
        );
    }
}

/// The same JDBC key set both as a URL parameter and as a driver property: the one the JDBC
/// driver ignores is dropped here, entirely (also for `options` / `sessionVariables`). Returns
/// `true` when `key` from `prefix` is the ignored one.
fn same_key_overridden(prefix: &str, key: &str, value: &str, st: &mut ConnState) -> bool {
    let lower = key.to_ascii_lowercase();
    let from_url = prefix == URL_PREFIX;
    let url_wins = st.rules.url_wins();
    let other = if from_url {
        st.url_values.insert(lower.clone(), value.to_owned());
        if url_wins {
            return false;
        }
        st.property_values.get(&lower)
    } else {
        if !url_wins {
            return false;
        }
        st.url_values.get(&lower)
    };
    let Some(other) = other else {
        return false;
    };
    let differs = other != value;
    let (winner, jdbc) = (
        if url_wins {
            "the URL parameter"
        } else {
            "the driver property"
        },
        st.rules.jdbc_driver(),
    );
    let message = if differs {
        format!(
            "not imported: set to a different value in the URL and in the driver properties; \
             {winner} is kept, as {jdbc} does"
        )
    } else {
        format!("not imported: also set as {winner}, which {jdbc} uses")
    };
    st.dropped_property(join(prefix, key), properties::Drop::NoEquivalent, message);
    true
}

impl ConnState {
    fn drop_property(&mut self, key: String, reason: PropertyDropReason) {
        if self.dropped.len() < MAX_DROPPED_PROPERTIES {
            self.dropped.push(DroppedProperty { key, reason });
        } else {
            self.dropped_overflow += 1;
        }
    }

    fn secret_removed(&mut self, path: String) {
        self.issues.warn(
            IssueKind::SecretRemoved,
            Some(path.clone()),
            "looks like a secret: removed, not imported (secrets only go to the keyring)",
        );
        self.drop_property(path, PropertyDropReason::LooksLikeSecret);
    }

    fn dropped_property(&mut self, path: String, reason: properties::Drop, message: String) {
        self.issues
            .warn(IssueKind::PropertyDropped, Some(path.clone()), message);
        self.drop_property(
            path,
            match reason {
                properties::Drop::NoEquivalent => PropertyDropReason::NoEquivalent,
                properties::Drop::InvalidValue => PropertyDropReason::InvalidValue,
            },
        );
    }

    fn apply_action(&mut self, action: properties::Action) {
        use properties::Action;
        match action {
            Action::Map {
                path,
                target,
                value,
                priority,
            } => self.map_property(path, target, value, priority),
            Action::Drop {
                path,
                reason,
                message,
            } => self.dropped_property(path, reason, message),
            Action::Secret { path } => self.secret_removed(path),
            Action::ReadOnly { path } => {
                self.read_only = Some(true);
                self.issues.info(
                    IssueKind::PropertyMapped,
                    Some(path),
                    "makes the session read-only: imported as the connection's read-only flag",
                );
            }
            Action::ConnectionTimeZone { path, value } => {
                if let Some((old, _)) = self.cj_time_zone.replace((path, value)) {
                    self.dropped_property(
                        old,
                        properties::Drop::NoEquivalent,
                        "not imported: the connection time zone is set more than once; the \
                         last one is kept"
                            .to_owned(),
                    );
                }
            }
            Action::ForceTimeZone { path, on } => {
                if let Some((old, _)) = self.cj_force_time_zone.replace((path, on)) {
                    self.dropped_property(
                        old,
                        properties::Drop::NoEquivalent,
                        "not imported: forceConnectionTimeZoneToSession is set more than once; \
                         the last one is kept"
                            .to_owned(),
                    );
                }
            }
            Action::ReadOnlyUnclear { path } => {
                self.read_only = Some(true);
                self.issues.warn(
                    IssueKind::PropertyMapped,
                    Some(path),
                    "an entry that may make the session read-only could not be read: the \
                     connection is imported read-only (the safe direction); check it before \
                     clearing the flag",
                );
            }
        }
    }

    fn map_property(
        &mut self,
        path: String,
        target: &'static str,
        value: String,
        priority: properties::Priority,
    ) {
        // Precedence: a dedicated property over an embedded setting, then the source the JDBC
        // driver lets win (URL or driver property), then the later one.
        let url_wins = self.rules.url_wins();
        let rank = |p: &str| priority_of(p, url_wins);
        if let Some((old_priority, old_value, old_path)) = self.mapped.get(target) {
            let different_sources =
                old_path.starts_with(URL_PREFIX) != path.starts_with(URL_PREFIX);
            let conflict = if different_sources && *old_value != value {
                " (the URL and the driver properties set different values)"
            } else {
                ""
            };
            if (*old_priority, rank(old_path)) > (priority, rank(&path)) {
                let message = format!(
                    "not imported: `{target}` is already set by `{}`, which takes \
                     precedence{conflict}",
                    ident(old_path, MAX_IDENT)
                );
                self.dropped_property(path, properties::Drop::NoEquivalent, message);
                return;
            }
            let (old_path, message) = (
                old_path.clone(),
                format!(
                    "not imported: `{target}` is set again by `{}`, which takes \
                     precedence{conflict}",
                    ident(&path, MAX_IDENT)
                ),
            );
            self.dropped_property(old_path, properties::Drop::NoEquivalent, message);
        }
        self.issues.info(
            IssueKind::PropertyMapped,
            Some(path.clone()),
            format!("imported as the NineSQL driver property `{target}`"),
        );
        self.mapped.insert(target, (priority, value, path));
    }
}

/// 1 for the source the JDBC driver lets win (URL parameter or driver property), 0 otherwise.
fn priority_of(path: &str, url_wins: bool) -> u8 {
    u8::from(path.starts_with(URL_PREFIX) == url_wins)
}

/// Resolves the properties that depend on each other and moves the converted ones into
/// `properties`.
fn finish_properties(st: &mut ConnState) {
    let force = st.cj_force_time_zone.take();
    let force_on = force.as_ref().is_some_and(|(_, on)| *on);
    let time_zone_set = st.cj_time_zone.is_some();
    match st.cj_time_zone.take() {
        Some((path, value)) if force_on => match properties::connection_time_zone(&value) {
            Some(tz) => st.map_property(path, "time_zone", tz, properties::Priority::Direct),
            None => st.dropped_property(
                path,
                properties::Drop::InvalidValue,
                "not imported: `LOCAL`, `SERVER` or an empty value depends on the machine or \
                 the server, not a fixed time zone"
                    .to_owned(),
            ),
        },
        Some((path, _)) => st.dropped_property(
            path,
            properties::Drop::NoEquivalent,
            "not imported: without `forceConnectionTimeZoneToSession=true`, Connector/J only \
             uses it to convert dates on the client; it does not set the session time zone"
                .to_owned(),
        ),
        None => {}
    }
    if let Some((path, on)) = force {
        // Consumed with the time zone above; on its own it has no effect.
        if !on || !time_zone_set {
            st.dropped_property(
                path,
                properties::Drop::NoEquivalent,
                "not imported: no NineSQL equivalent on its own (it only matters with a \
                 connection time zone)"
                    .to_owned(),
            );
        }
    }
    for (target, (_, value, _)) in std::mem::take(&mut st.mapped) {
        st.properties.insert(target.to_owned(), value);
    }
    if st.dropped_overflow > 0 {
        st.issues.warn(
            IssueKind::Truncated,
            Some("configuration.properties".to_owned()),
            format!(
                "{} more dropped properties are not listed (at most {MAX_DROPPED_PROPERTIES} \
                 per connection)",
                st.dropped_overflow
            ),
        );
    }
}

fn map_auth(
    def: &ConnectionDef,
    file_engine: bool,
    saved: Option<&mut ConnectionCredentials>,
    ctx: &Ctx<'_>,
    st: &mut ConnState,
) -> Auth {
    let cfg = &def.configuration;
    if let Some(model) = non_empty(cfg.auth_model.as_deref())
        && !model.eq_ignore_ascii_case("native")
    {
        st.issues.warn(
            IssueKind::Unsupported,
            Some("configuration.auth-model".to_owned()),
            format!(
                "authentication model `{}` is not supported: imported as user / password",
                ident(model, MAX_IDENT)
            ),
        );
    }
    let section = saved.and_then(|s| s.sections.remove(CONNECTION_SECTION));
    let (mut user, mut password, other_keys) = match section {
        Some(s) => (s.user, s.password, s.other_keys),
        None => (None, None, Vec::new()),
    };
    for key in other_keys {
        st.issues.warn(
            IssueKind::Credentials,
            Some(format!("{CONNECTION_SECTION}.{key}")),
            "unknown saved credential: not imported",
        );
    }
    if user.as_deref().is_none_or(|u| u.trim().is_empty()) {
        user = non_empty(cfg.user.as_deref())
            .map(str::to_owned)
            .or_else(|| st.username_hint.clone());
    }
    if password.is_none()
        && let Some(p) = &cfg.password
    {
        if !p.expose_secret().is_empty() {
            st.issues.warn(
                IssueKind::PlaintextSecret,
                Some("configuration.password".to_owned()),
                "DBeaver stored this password in clear in data-sources.json: it is moved to \
                 the keyring; consider removing it from DBeaver",
            );
        }
        password = Some(crate::credentials::into_secret(
            p.expose_secret().to_owned(),
        ));
    }
    let password = password.filter(|p| !p.expose_secret().is_empty());
    if file_engine {
        if password.is_some() {
            st.issues.info(
                IssueKind::Credentials,
                Some(CONNECTION_SECTION.to_owned()),
                "a saved password of a file database is not used by NineSQL: not imported",
            );
        }
        return Auth::None;
    }
    let username = match user {
        Some(u) if is_valid_short(&u) => Some(u),
        Some(_) => {
            st.issues.warn(
                IssueKind::InvalidValue,
                Some("user".to_owned()),
                "user name too long or with control characters: not imported",
            );
            None
        }
        None => None,
    };
    let secret = match password {
        Some(value) => {
            st.secrets.push(ImportedSecret {
                slot: SecretSlot::Auth,
                kind: SecretKind::Password,
                value,
            });
            SecretState::Imported
        }
        None => {
            if ctx.credentials_unreadable && def.save_password == Some(true) {
                st.issues.info(
                    IssueKind::Credentials,
                    None,
                    "the saved password could not be read: NineSQL will ask for it",
                );
            }
            SecretState::Prompt
        }
    };
    match (username, secret) {
        (None, SecretState::Prompt) if def.save_password != Some(true) => Auth::None,
        (username, secret) => Auth::Password {
            username: username.unwrap_or_default(),
            secret,
        },
    }
}

fn map_ssh(
    h: &HandlerDef,
    section: Option<crate::credentials::Section>,
    prefix: &str,
    endpoint: &Endpoint,
    st: &mut ConnState,
) -> Result<SshTunnel, &'static str> {
    let props_prefix = join(prefix, "properties");
    let props = &h.properties;
    for key in &props.non_scalar {
        st.issues.warn(
            IssueKind::InvalidValue,
            Some(join(&props_prefix, key)),
            "not imported: not a scalar value",
        );
    }
    const CONSUMED: &[&str] = &[
        "host",
        "port",
        "authType",
        "keyPath",
        "user",
        "remoteHost",
        "remotePort",
    ];
    for (key, value) in &props.entries {
        if CONSUMED.contains(&key.as_str()) || key.starts_with("jumpServer") {
            continue;
        }
        if is_inline_key(key) {
            st.issues.warn(
                IssueKind::SecretRemoved,
                Some(join(&props_prefix, key)),
                "an inline private key is never imported: save it to a file and set its path",
            );
            continue;
        }
        if is_secret_like(key, &value.as_text()) {
            st.issues.warn(
                IssueKind::SecretRemoved,
                Some(join(&props_prefix, key)),
                "looks like a secret: removed, not imported",
            );
            continue;
        }
        coverage::report_key(Scope::SshProperties, &props_prefix, key, &mut st.issues);
    }

    let host = text_prop(props, "host").ok_or("SSH tunnel without a host")?;
    if !is_valid_host(&host) {
        return Err("the SSH host is not a valid host name or IP address");
    }
    let port = match props.get("port") {
        None => 22,
        Some(p) => match p.as_u16() {
            Some(port) if port != 0 => port,
            _ => {
                st.issues.warn(
                    IssueKind::InvalidValue,
                    Some(join(&props_prefix, "port")),
                    "not a valid port: 22 is used",
                );
                22
            }
        },
    };
    check_forwarding_target(props, &props_prefix, endpoint, st);
    let (sec_user, sec_password, other_keys) = match section {
        Some(s) => (s.user, s.password, s.other_keys),
        None => (None, None, Vec::new()),
    };
    for key in other_keys {
        st.issues.warn(
            IssueKind::Credentials,
            Some(format!("{SSH_SECTION}.{key}")),
            "unknown saved credential: not imported",
        );
    }
    let username = sec_user
        .filter(|u| !u.trim().is_empty())
        .or_else(|| non_empty(h.user.as_deref()).map(str::to_owned))
        .or_else(|| text_prop(props, "user"))
        .ok_or("SSH tunnel without a user name")?;
    if !is_valid_short(&username) || username.starts_with('-') {
        return Err("the SSH user name is too long, starts with `-` or has control characters");
    }
    let mut password = sec_password;
    if password.is_none()
        && let Some(p) = &h.password
    {
        if !p.expose_secret().is_empty() {
            st.issues.warn(
                IssueKind::PlaintextSecret,
                Some(join(prefix, "password")),
                "DBeaver stored this SSH secret in clear in data-sources.json: it is moved to \
                 the keyring",
            );
        }
        password = Some(crate::credentials::into_secret(
            p.expose_secret().to_owned(),
        ));
    }
    let password = password.filter(|p| !p.expose_secret().is_empty());

    let auth_type = text_prop(props, "authType").unwrap_or_else(|| "PASSWORD".to_owned());
    let auth = match auth_type.to_ascii_uppercase().as_str() {
        "PASSWORD" => {
            let secret = push_secret(st, SecretSlot::SshServer, SecretKind::SshPassword, password);
            SshAuth::Password { secret }
        }
        "PUBLIC_KEY" => {
            let key_path =
                text_prop(props, "keyPath").ok_or("SSH public-key auth without a key path")?;
            check_key_type(&key_path, join(&props_prefix, "keyPath"), st);
            let passphrase = match password {
                Some(p) => Some(push_secret(
                    st,
                    SecretSlot::SshServer,
                    SecretKind::SshKeyPassphrase,
                    Some(p),
                )),
                // DBeaver asks for the passphrase when it is not saved.
                None if h.save_password == Some(false) => Some(SecretState::Prompt),
                None => None,
            };
            SshAuth::PublicKey {
                key_path: PathBuf::from(key_path),
                passphrase,
            }
        }
        "AGENT" => {
            if password.is_some() {
                st.issues.info(
                    IssueKind::Credentials,
                    Some(SSH_SECTION.to_owned()),
                    "a saved SSH password is not used with ssh-agent: not imported",
                );
            }
            SshAuth::Agent
        }
        _ => return Err("unknown SSH authentication type"),
    };
    st.issues.info(
        IssueKind::SshHostKeyToConfirm,
        Some(prefix.to_owned()),
        "the SSH host key is not trusted on import: the first connection shows its fingerprint \
         and asks (ADR-0011)",
    );
    let jump_hosts = map_jump_hosts(props, &props_prefix, st)?;
    Ok(SshTunnel {
        server: SshHop {
            host,
            port,
            username,
            auth,
        },
        jump_hosts,
    })
}

/// DBeaver's `SSHConstants.MAX_JUMP_SERVERS`: the most jump servers its dialog creates, and the
/// default of `jumpServer.count` when the key is absent (`SSHUtils.loadHostConfigurations`,
/// DBeaver 24.3 and later; 22.x and 23.x read `jumpServer0` only).
const MAX_JUMP_SERVERS: usize = 5;

/// `keyValue` / `jumpServer<i>.keyValue`: an inline private key, a secret.
fn is_inline_key(key: &str) -> bool {
    normalize_key(key).ends_with("keyvalue")
}

/// A jump server index as DBeaver writes it (`jumpServer` + Java `int` + `.`): `0`, or a
/// non-zero digit followed by digits. `00`, `01`, `+0`, `-1` and the empty string are refused.
fn parse_jump_index(index: &str) -> Option<usize> {
    let canonical = index == "0"
        || (index
            .as_bytes()
            .first()
            .is_some_and(|b| (b'1'..=b'9').contains(b))
            && index.bytes().all(|b| b.is_ascii_digit()));
    if canonical { index.parse().ok() } else { None }
}

/// `jumpServer.count`: absent or blank means [`MAX_JUMP_SERVERS`] (DBeaver's default); a negative
/// count means none. Anything that is not an integer fails (DBeaver would fall back to a default
/// or truncate a fraction: NineSQL does not guess).
fn parse_jump_count(value: Option<&Scalar>) -> Result<usize, &'static str> {
    const ERR: &str = "unreadable SSH jump server count (`jumpServer.count`)";
    let n: i64 = match value {
        None => return Ok(MAX_JUMP_SERVERS),
        Some(Scalar::Text(s)) if s.trim().is_empty() => return Ok(MAX_JUMP_SERVERS),
        Some(Scalar::Text(s)) => s.parse().map_err(|_| ERR)?,
        Some(Scalar::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|_| i64::MAX))
            .ok_or(ERR)?,
        Some(Scalar::Bool(_)) => return Err(ERR),
    };
    Ok(usize::try_from(n.max(0)).unwrap_or(usize::MAX))
}

/// `jumpServer<i>.enabled`: absent means not enabled (DBeaver's `getBooleanProperty` default).
/// A present value must be a boolean or the string `true` / `false` (any case, not trimmed);
/// anything else fails rather than being read as `false`.
fn parse_jump_enabled(value: Option<&Scalar>) -> Result<bool, &'static str> {
    match value {
        None => Ok(false),
        Some(Scalar::Bool(b)) => Ok(*b),
        Some(Scalar::Text(s)) if s.eq_ignore_ascii_case("true") => Ok(true),
        Some(Scalar::Text(s)) if s.eq_ignore_ascii_case("false") => Ok(false),
        Some(_) => Err("an SSH jump server `enabled` flag is not a boolean"),
    }
}

/// DBeaver SSH jump servers: `jumpServer.count`, then `jumpServer<i>.enabled`, `.host`, `.port`,
/// `.name` (the user name), `.authType`, `.keyPath`… Selection follows DBeaver's
/// `SSHUtils.loadHostConfigurations`: indices `0..count`, stopping at the first one that is not
/// enabled. Index 0 is the hop closest to NineSQL (DBeaver connects through jump #1, then #2…,
/// then the SSH server), which is the [`SshTunnel::jump_hosts`] order. Their saved secrets are
/// not imported (storage format not verified): NineSQL asks for them.
///
/// Fail closed (I2), the connection fails rather than losing a hop: unreadable settings, a
/// non-canonical index, a non-boolean `enabled`, an enabled hop after a disabled or missing one
/// (DBeaver would skip it silently), more than [`MAX_JUMP_SERVERS`] hops in the chain, and a hop
/// without a valid host, port, user name or authentication type. Hops at an index at or past
/// `jumpServer.count` are not used by DBeaver either: they are reported and not imported.
fn map_jump_hosts(
    props: &Properties,
    props_prefix: &str,
    st: &mut ConnState,
) -> Result<Vec<SshHop>, &'static str> {
    if props.non_scalar.iter().any(|k| k.starts_with("jumpServer")) {
        return Err("unreadable SSH jump server settings");
    }
    let mut count = None;
    let mut servers: BTreeMap<usize, Vec<(&str, &Scalar)>> = BTreeMap::new();
    for (key, value) in &props.entries {
        let Some(rest) = key.strip_prefix("jumpServer") else {
            continue;
        };
        let Some((index, field)) = rest.split_once('.') else {
            return Err("unreadable SSH jump server settings");
        };
        if index.is_empty() {
            if field == "count" {
                count = Some(value);
            } else {
                coverage::report_key(Scope::SshProperties, props_prefix, key, &mut st.issues);
            }
            continue;
        }
        let Some(i) = parse_jump_index(index) else {
            return Err("unreadable SSH jump server settings (index not written by DBeaver)");
        };
        servers.entry(i).or_default().push((field, value));
    }
    let count = parse_jump_count(count)?;
    let mut hops = Vec::new();
    // Whether every index before the current one was enabled (DBeaver's loop has not stopped).
    let mut chain_open = true;
    for (i, fields) in servers {
        let get = |name: &str| fields.iter().find(|(f, _)| *f == name).map(|(_, v)| *v);
        let text = |name: &str| {
            get(name)
                .map(|v| v.as_text().trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let hop_prefix = join(props_prefix, &format!("jumpServer{i}"));
        let enabled = parse_jump_enabled(get("enabled"))?;
        if i >= count || i != hops.len() || !enabled {
            // DBeaver's loop does not reach this hop (past the count, after a missing index) or
            // stops at it (not enabled).
            let past_count = i >= count;
            if !past_count {
                chain_open = false;
            }
            if past_count || !enabled {
                report_skipped_hop(&fields, &hop_prefix, enabled, past_count, st);
                continue;
            }
        }
        if !chain_open {
            return Err(
                "an enabled SSH jump server follows a disabled or missing one (DBeaver would \
                 skip it silently)",
            );
        }
        if i >= MAX_JUMP_SERVERS {
            return Err("more SSH jump servers than DBeaver supports (5)");
        }
        for (field, value) in &fields {
            let path = join(&hop_prefix, field);
            match *field {
                "enabled" | "host" | "port" | "name" | "authType" | "keyPath" => {}
                f if is_inline_key(f) => st.issues.warn(
                    IssueKind::SecretRemoved,
                    Some(path),
                    "an inline private key is never imported: save it to a file and set its path",
                ),
                f if is_secret_like(f, &value.as_text()) => st.issues.warn(
                    IssueKind::SecretRemoved,
                    Some(path),
                    "looks like a secret: removed, not imported",
                ),
                f => coverage::report_key(Scope::SshProperties, &hop_prefix, f, &mut st.issues),
            }
        }
        let host = text("host").ok_or("SSH jump server without a host")?;
        if !is_valid_host(&host) {
            return Err("an SSH jump server host is not a valid host name or IP address");
        }
        let port = match get("port") {
            None => 22,
            Some(p) => match p.as_u16() {
                Some(port) if port != 0 => port,
                _ => return Err("an SSH jump server port is not valid"),
            },
        };
        // DBeaver keeps a jump server's user name under `name` (`RegistryConstants.ATTR_NAME`).
        let username = text("name").ok_or("SSH jump server without a user name")?;
        if !is_valid_short(&username) || username.starts_with('-') {
            return Err(
                "an SSH jump server user name is too long, starts with `-` or has \
                        control characters",
            );
        }
        let auth = match text("authType")
            .unwrap_or_else(|| "PASSWORD".to_owned())
            .to_ascii_uppercase()
            .as_str()
        {
            "PASSWORD" => SshAuth::Password {
                secret: SecretState::Prompt,
            },
            "PUBLIC_KEY" => {
                let key_path =
                    text("keyPath").ok_or("SSH jump server public-key auth without a key path")?;
                check_key_type(&key_path, join(&hop_prefix, "keyPath"), st);
                SshAuth::PublicKey {
                    key_path: PathBuf::from(key_path),
                    passphrase: Some(SecretState::Prompt),
                }
            }
            "AGENT" => SshAuth::Agent,
            _ => return Err("unknown SSH jump server authentication type"),
        };
        hops.push(SshHop {
            host,
            port,
            username,
            auth,
        });
    }
    if !hops.is_empty() {
        st.issues.info(
            IssueKind::Credentials,
            Some(props_prefix.to_owned()),
            format!(
                "{} SSH jump host(s) imported; their saved secrets are not imported (format not \
                 verified): NineSQL will ask for them",
                hops.len()
            ),
        );
    }
    Ok(hops)
}

/// A jump server DBeaver does not use: reported, never imported. Its secret-like fields still
/// get a `SecretRemoved` entry (their values are dropped like the others); a configured host
/// gets a warning so the user knows a hop written in the file is not used.
fn report_skipped_hop(
    fields: &[(&str, &Scalar)],
    hop_prefix: &str,
    enabled: bool,
    past_count: bool,
    st: &mut ConnState,
) {
    for (field, value) in fields {
        if is_inline_key(field) {
            st.issues.warn(
                IssueKind::SecretRemoved,
                Some(join(hop_prefix, field)),
                "an inline private key is never imported: save it to a file and set its path",
            );
        } else if *field != "enabled" && is_secret_like(field, &value.as_text()) {
            st.issues.warn(
                IssueKind::SecretRemoved,
                Some(join(hop_prefix, field)),
                "looks like a secret: removed, not imported",
            );
        }
    }
    let has_host = fields
        .iter()
        .any(|(f, v)| *f == "host" && !v.as_text().trim().is_empty());
    let message = match (past_count, enabled, has_host) {
        (true, true, _) => {
            "enabled SSH jump server at or past `jumpServer.count`: DBeaver does not use it, so \
             it is not imported"
        }
        (true, false, true) => {
            "configured SSH jump host not used, as in DBeaver (at or past `jumpServer.count`, \
             not enabled): not imported"
        }
        (false, _, true) => {
            "configured SSH jump host not used, as in DBeaver (not enabled): not imported"
        }
        (_, _, false) => {
            if fields.iter().all(|(f, _)| *f == "enabled") {
                return;
            }
            st.issues.info(
                IssueKind::Unsupported,
                Some(hop_prefix.to_owned()),
                "SSH jump server settings not used by DBeaver (not enabled or at or past \
                 `jumpServer.count`): not imported",
            );
            return;
        }
    };
    st.issues
        .warn(IssueKind::Unsupported, Some(hop_prefix.to_owned()), message);
}

/// What the file name of an SSH private key says about its type. Key files are **never read**
/// during import (I4 spirit, privacy), so this is a heuristic on the name only: a key named
/// `id_ed25519` may still be RSA, and a key named `id_rsa` may not be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyNameHint {
    /// `id_rsa*`, or a name with an `rsa` word (`deploy-rsa.pem`).
    Rsa,
    /// `id_ed25519*`, `id_ecdsa*`, or a name with an `ed25519` / `ecdsa` word.
    NotRsa,
    /// Nothing in the name.
    Unknown,
}

fn key_name_hint(path: &str) -> KeyNameHint {
    // Both separators: DBeaver workspaces may come from Windows.
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_word = |w: &str| {
        name.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| t == w)
    };
    if name.starts_with("id_rsa") || has_word("rsa") {
        KeyNameHint::Rsa
    } else if name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || has_word("ed25519")
        || has_word("ecdsa")
    {
        KeyNameHint::NotRsa
    } else {
        KeyNameHint::Unknown
    }
}

/// ADR-0016 §1.2: NineSQL refuses RSA private key files. Flags a key path whose name says RSA,
/// or whose type cannot be told from the name. DBeaver stores no key type, so the name is the
/// only hint available without reading the file. The message never contains the path.
fn check_key_type(path: &str, key: String, st: &mut ConnState) {
    match key_name_hint(path) {
        KeyNameHint::NotRsa => {}
        KeyNameHint::Rsa => st.issues.warn(
            IssueKind::SshKeyType,
            Some(key),
            "RSA key files are not supported (ADR-0016); use an Ed25519/ECDSA key or the SSH \
             agent",
        ),
        KeyNameHint::Unknown => st.issues.warn(
            IssueKind::SshKeyType,
            Some(key),
            "the type of this key file is not checked on import (key files are never read): \
             RSA key files are not supported (ADR-0016); if it is an RSA key, use an \
             Ed25519/ECDSA key or the SSH agent",
        ),
    }
}

/// `remoteHost` / `remotePort`: the address the SSH server forwards to. NineSQL forwards to the
/// connection's own host and port and has no field for another target, so a different target
/// disables the connection (connecting elsewhere than DBeaver did would be wrong).
fn check_forwarding_target(
    props: &Properties,
    props_prefix: &str,
    endpoint: &Endpoint,
    st: &mut ConnState,
) {
    let Endpoint::Tcp { host, port, .. } = endpoint else {
        return;
    };
    let mut differs = Vec::new();
    if let Some(remote) = text_prop(props, "remoteHost")
        && !remote.eq_ignore_ascii_case(host)
    {
        differs.push("remoteHost");
    }
    if let Some(remote) = props
        .get("remotePort")
        .filter(|v| !v.as_text().trim().is_empty())
    {
        // Without an explicit connection port the target cannot be confirmed: fail closed.
        if remote.as_u16().is_none() || remote.as_u16() != *port {
            differs.push("remotePort");
        }
    }
    for key in differs {
        st.issues.warn(
            IssueKind::Disabled,
            Some(join(props_prefix, key)),
            "the SSH tunnel forwards to another address than the connection's host and port, \
             which NineSQL does not support yet: imported disabled",
        );
        st.disabled
            .push("SSH forwarding target not supported yet".to_owned());
    }
}

fn push_secret(
    st: &mut ConnState,
    slot: SecretSlot,
    kind: SecretKind,
    value: Option<SecretString>,
) -> SecretState {
    match value {
        Some(value) => {
            st.secrets.push(ImportedSecret { slot, kind, value });
            SecretState::Imported
        }
        None => SecretState::Prompt,
    }
}

fn text_prop(props: &Properties, key: &str) -> Option<String> {
    props
        .get(key)
        .map(|v| v.as_text().trim().to_owned())
        .filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------------------------

/// TLS settings gathered from driver properties, URL parameters and TLS handlers.
#[derive(Default)]
struct TlsHints {
    /// Explicit mode from properties / URL (`sslmode`, `sslMode`).
    prop_mode: Option<TlsMode>,
    /// Explicit mode from a TLS handler (wins over properties).
    handler_mode: Option<TlsMode>,
    ssl: Option<bool>,
    use_ssl: Option<bool>,
    require_ssl: Option<bool>,
    verify_server: Option<bool>,
    trust_server: Option<bool>,
    /// A converted mode (`prefer`…) was seen: reported once.
    converted: Option<&'static str>,
    ca_file: Option<String>,
    cert_file: Option<String>,
    key_file: Option<String>,
}

/// Engine-neutral reading of `sslmode` / `sslMode` values (PostgreSQL, MySQL, MariaDB,
/// ClickHouse spellings). The second value tells why a conversion happened.
fn ssl_mode_value(value: &str) -> Option<(TlsMode, Option<&'static str>)> {
    let v = value.trim().to_ascii_lowercase().replace('_', "-");
    Some(match v.as_str() {
        "disable" | "disabled" => (TlsMode::Disable, None),
        "allow" | "prefer" | "preferred" => (
            TlsMode::Require,
            Some(
                "opportunistic TLS (`prefer` / `allow`) has no NineSQL equivalent: imported as \
                 `require` (encrypted, certificate not verified)",
            ),
        ),
        "require" | "required" => (TlsMode::Require, None),
        "trust" | "none" => (TlsMode::Require, None),
        "verify-ca" => (TlsMode::VerifyCa, None),
        "verify-full" | "verify-identity" | "strict" => (TlsMode::VerifyFull, None),
        _ => return None,
    })
}

impl TlsHints {
    /// Takes a TLS-related property (normalized key). Returns `false` when the key is not one.
    fn absorb_property(
        &mut self,
        norm: &str,
        value: &str,
        path: &str,
        issues: &mut Issues,
    ) -> bool {
        let as_bool = || Scalar::Text(value.to_owned()).as_bool();
        let invalid = |issues: &mut Issues| {
            issues.warn(
                IssueKind::InvalidValue,
                Some(path.to_owned()),
                "unrecognized TLS setting value: not imported",
            );
        };
        match norm {
            "sslmode" => match ssl_mode_value(value) {
                Some((mode, note)) => {
                    self.prop_mode = Some(mode);
                    self.converted = self.converted.or(note);
                }
                None => invalid(issues),
            },
            "ssl" => match as_bool() {
                Some(b) => self.ssl = Some(b),
                None => invalid(issues),
            },
            "usessl" => match as_bool() {
                Some(b) => self.use_ssl = Some(b),
                None => invalid(issues),
            },
            "requiressl" => match as_bool() {
                Some(b) => self.require_ssl = Some(b),
                None => invalid(issues),
            },
            "verifyservercertificate" => match as_bool() {
                Some(b) => self.verify_server = Some(b),
                None => invalid(issues),
            },
            "trustservercertificate" => match as_bool() {
                Some(b) => self.trust_server = Some(b),
                None => invalid(issues),
            },
            "sslrootcert" | "serversslcert" => {
                if value.trim_start().starts_with("-----BEGIN") {
                    issues.warn(
                        IssueKind::Unsupported,
                        Some(path.to_owned()),
                        "an inline PEM certificate is not supported: save it to a file and set \
                         it as the CA file",
                    );
                } else if !value.trim().is_empty() {
                    self.ca_file = Some(value.trim().to_owned());
                }
            }
            "sslcert" => {
                if !value.trim().is_empty() {
                    self.cert_file = Some(value.trim().to_owned());
                }
            }
            "sslkey" => {
                if !value.trim().is_empty() {
                    self.key_file = Some(value.trim().to_owned());
                }
            }
            _ => return false,
        }
        true
    }

    /// Mode implied by the boolean properties, when no explicit mode was given.
    fn implied_mode(&mut self) -> Option<TlsMode> {
        if self.ssl == Some(false) || self.use_ssl == Some(false) {
            return Some(TlsMode::Disable);
        }
        if self.verify_server == Some(true) {
            return Some(TlsMode::VerifyCa);
        }
        if self.ssl == Some(true) {
            // pgjdbc / ClickHouse: `ssl=true` without a mode verifies the full chain and host.
            return Some(TlsMode::VerifyFull);
        }
        if self.require_ssl == Some(true)
            || self.verify_server == Some(false)
            || self.trust_server == Some(true)
        {
            return Some(TlsMode::Require);
        }
        if self.use_ssl == Some(true) {
            self.converted = self.converted.or(Some(
                "`useSSL=true` alone is opportunistic TLS in MySQL Connector/J: imported as \
                 `require` (encrypted, certificate not verified)",
            ));
            return Some(TlsMode::Require);
        }
        None
    }
}

fn tls_from_postgres_handler(h: &HandlerDef, prefix: &str, st: &mut ConnState) {
    let props_prefix = join(prefix, "properties");
    for (key, value) in &h.properties.entries {
        let path = join(&props_prefix, key);
        let text = value.as_text();
        match key.as_str() {
            "sslMode" | "ssl.mode" => match ssl_mode_value(&text) {
                Some((mode, note)) => {
                    st.tls.handler_mode = Some(mode);
                    st.tls.converted = st.tls.converted.or(note);
                }
                None => st.issues.warn(
                    IssueKind::InvalidValue,
                    Some(path),
                    "unrecognized TLS mode: not imported",
                ),
            },
            "ssl.ca.cert" => set_path(&mut st.tls.ca_file, &text),
            "ssl.client.cert" => set_path(&mut st.tls.cert_file, &text),
            "ssl.client.key" => set_path(&mut st.tls.key_file, &text),
            k if is_secret_like(k, &text) => st.issues.warn(
                IssueKind::SecretRemoved,
                Some(path),
                "looks like a secret: removed, not imported",
            ),
            k => coverage::report_key(
                Scope::PostgresSslProperties,
                &props_prefix,
                k,
                &mut st.issues,
            ),
        }
    }
    report_non_scalar(&h.properties, &props_prefix, st);
    if st.tls.handler_mode.is_none() {
        st.tls.handler_mode = Some(TlsMode::VerifyFull);
        st.issues.info(
            IssueKind::TlsConverted,
            Some(prefix.to_owned()),
            "TLS handler without a mode: `verify-full` applied",
        );
    }
}

fn tls_from_mysql_handler(h: &HandlerDef, prefix: &str, st: &mut ConnState) {
    let props_prefix = join(prefix, "properties");
    let mut require = None;
    let mut verify = None;
    for (key, value) in &h.properties.entries {
        let path = join(&props_prefix, key);
        let text = value.as_text();
        match key.as_str() {
            "ssl.require" => require = value.as_bool(),
            "ssl.verify.server" => verify = value.as_bool(),
            "ssl.ca.cert" => set_path(&mut st.tls.ca_file, &text),
            "ssl.client.cert" => set_path(&mut st.tls.cert_file, &text),
            "ssl.client.key" => set_path(&mut st.tls.key_file, &text),
            "ssl.public.key.retrieve" => {
                if value.as_bool() == Some(true) {
                    st.issues.warn(
                        IssueKind::Unsupported,
                        Some(path),
                        "not imported: NineSQL never enables RSA public key retrieval \
                         (reserved driver option)",
                    );
                }
            }
            k if is_secret_like(k, &text) => st.issues.warn(
                IssueKind::SecretRemoved,
                Some(path),
                "looks like a secret: removed, not imported",
            ),
            k => coverage::report_key(Scope::MysqlSslProperties, &props_prefix, k, &mut st.issues),
        }
    }
    report_non_scalar(&h.properties, &props_prefix, st);
    // Connector/J: verifyServerCertificate=true is VERIFY_CA (chain, not host name).
    st.tls.handler_mode = Some(match (require, verify) {
        (_, Some(true)) => TlsMode::VerifyCa,
        (Some(true), _) => TlsMode::Require,
        _ => {
            st.tls.converted = st.tls.converted.or(Some(
                "TLS was optional in DBeaver (not required, not verified): imported as \
                 `require` (encrypted, certificate not verified)",
            ));
            TlsMode::Require
        }
    });
}

fn report_non_scalar(props: &Properties, prefix: &str, st: &mut ConnState) {
    for key in &props.non_scalar {
        st.issues.warn(
            IssueKind::InvalidValue,
            Some(join(prefix, key)),
            "not imported: not a scalar value",
        );
    }
}

fn set_path(slot: &mut Option<String>, value: &str) {
    if !value.trim().is_empty() {
        *slot = Some(value.trim().to_owned());
    }
}

/// Final TLS settings of a network connection, with the I5 report entries.
fn resolve_tls(st: &mut ConnState) -> TlsConfig {
    let mut mode = st.tls.handler_mode.or(st.tls.prop_mode);
    if mode.is_none() {
        mode = st.tls.implied_mode();
    }
    if st.tls.verify_server == Some(false) || st.tls.trust_server == Some(true) {
        // An explicit "do not verify" flag caps any mode (explicit or implied, such as
        // `ssl=true`) at `require`.
        mode = mode.map(|m| m.min(TlsMode::Require));
    }
    let mode = match mode {
        Some(m) => m,
        None => {
            st.issues.warn(
                IssueKind::TlsConverted,
                None,
                "DBeaver had no TLS setting: depending on the JDBC driver it connected with \
                 opportunistic TLS without certificate verification (PostgreSQL, MySQL) or \
                 without TLS at all (MariaDB, ClickHouse…). NineSQL applies `verify-full`: if \
                 the server has no TLS or no trusted certificate, choose a weaker mode explicitly",
            );
            TlsMode::VerifyFull
        }
    };
    if let Some(note) = st.tls.converted {
        st.issues.info(IssueKind::TlsConverted, None, note);
    }
    match mode {
        TlsMode::Disable => st.issues.warn(
            IssueKind::TlsNotVerified,
            None,
            "TLS is disabled: traffic is not encrypted (imported as an explicit, flagged choice)",
        ),
        TlsMode::Require => st.issues.warn(
            IssueKind::TlsNotVerified,
            None,
            "the server certificate is not verified: the connection can be intercepted \
             (imported as an explicit, flagged downgrade)",
        ),
        TlsMode::VerifyCa => st.issues.warn(
            IssueKind::TlsNotVerified,
            None,
            "the server host name is not verified (imported as an explicit, flagged downgrade)",
        ),
        TlsMode::VerifyFull => {}
    }
    let client_certificate = match (st.tls.cert_file.take(), st.tls.key_file.take()) {
        (Some(cert), Some(key)) => Some(ClientCertificate {
            cert_file: PathBuf::from(cert),
            key_file: PathBuf::from(key),
            passphrase: None,
        }),
        (None, None) => None,
        _ => {
            st.issues.warn(
                IssueKind::InvalidValue,
                None,
                "a TLS client certificate needs both a certificate and a key file: not imported",
            );
            None
        }
    };
    TlsConfig {
        mode,
        ca_file: st.tls.ca_file.take().map(PathBuf::from),
        client_certificate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_ids_and_colors() {
        assert_eq!(sanitize_type_id("staging"), "staging");
        assert_eq!(sanitize_type_id("My Type!"), "my-type-");
        assert_eq!(sanitize_type_id("--"), "imported");
        assert_eq!(
            parse_rgb("0,128,128"),
            Some(Rgb {
                r: 0,
                g: 128,
                b: 128
            })
        );
        assert_eq!(parse_rgb(" 1, 2 ,3,255"), Some(Rgb { r: 1, g: 2, b: 3 }));
        assert_eq!(parse_rgb("1,2"), None);
        assert_eq!(parse_rgb("1,2,300"), None);
        assert_eq!(
            theme_color("org.jkiss.dbeaver.color.connectionType.prod.background"),
            Some(Rgb {
                r: 250,
                g: 207,
                b: 207
            })
        );
        assert_eq!(
            Rgb {
                r: 0,
                g: 128,
                b: 255
            }
            .to_string(),
            "#0080FF"
        );
    }

    #[test]
    fn engine_ids() {
        assert_eq!(fallback_engine_id("oracle").as_str(), "oracle");
        assert_eq!(fallback_engine_id("9 SQL Server").as_str(), "sql-server");
        assert_eq!(fallback_engine_id("").as_str(), "unknown");
        assert!(matches!(
            match_engine("mysql", "mariaDB"),
            EngineMatch::Supported { verified: true, .. }
        ));
        assert!(matches!(
            match_engine("oracle", "oracle_thin"),
            EngineMatch::Unsupported
        ));
        // Provider / driver pairs from DBeaver's plugin descriptors (no fixture yet).
        let engine = |p: &str, d: &str| match match_engine(p, d) {
            EngineMatch::Supported { engine, .. } => Some(engine),
            EngineMatch::Unsupported => None,
        };
        assert_eq!(engine("tidb", "tidb"), Some(EngineId::TIDB));
        assert_eq!(
            engine("postgresql", "postgres-yugabytedb-jdbc"),
            Some(EngineId::YUGABYTEDB)
        );
        assert_eq!(
            engine("postgresql", "postgresql"),
            Some(EngineId::POSTGRESQL)
        );
        assert_eq!(engine("duckdb", "duckdb_jdbc"), Some(EngineId::DUCKDB));
        assert_eq!(engine("duckdb", "motherduck_jdbc"), None);
        assert_eq!(
            engine("generic", "firebird_jaybird3"),
            Some(EngineId::FIREBIRD)
        );
        assert_eq!(engine("jaybird", "jaybird_embedded"), None);
        assert_eq!(
            engine("clickhouse", "yandex_clickhouse"),
            Some(EngineId::CLICKHOUSE)
        );
        assert_eq!(engine("generic", "trino_jdbc"), Some(EngineId::TRINO));
        assert_eq!(engine("sqlite", "libsql_jdbc"), None);
        assert_eq!(engine("mysql", "mysql_ndb"), None);
    }

    #[test]
    fn ssl_modes() {
        assert_eq!(
            ssl_mode_value("verify-full").map(|m| m.0),
            Some(TlsMode::VerifyFull)
        );
        assert_eq!(
            ssl_mode_value("VERIFY_IDENTITY").map(|m| m.0),
            Some(TlsMode::VerifyFull)
        );
        assert_eq!(
            ssl_mode_value("VERIFY_CA").map(|m| m.0),
            Some(TlsMode::VerifyCa)
        );
        assert_eq!(
            ssl_mode_value("PREFERRED").map(|m| m.0),
            Some(TlsMode::Require)
        );
        assert!(ssl_mode_value("PREFERRED").and_then(|m| m.1).is_some());
        assert_eq!(
            ssl_mode_value("disable").map(|m| m.0),
            Some(TlsMode::Disable)
        );
        assert_eq!(ssl_mode_value("bogus"), None);
    }

    #[test]
    fn text_sanitizing() {
        assert_eq!(sanitize_text("a\tb"), ("a b".to_owned(), true));
        let long = "é".repeat(200);
        let (t, changed) = sanitize_text(&long);
        assert!(changed && t.len() <= 255 && t.is_char_boundary(t.len()));
        assert!(is_valid_host("db.example.test"));
        assert!(!is_valid_host("user@db.example.test"));
        assert!(!is_valid_host(" "));
    }

    #[test]
    fn jump_indices_counts_and_flags() {
        for (index, expected) in [
            ("0", Some(0)),
            ("4", Some(4)),
            ("10", Some(10)),
            ("00", None),
            ("01", None),
            ("+0", None),
            ("-1", None),
            ("", None),
            ("1 ", None),
            ("٣", None),
            ("99999999999999999999999999", None),
        ] {
            assert_eq!(parse_jump_index(index), expected, "{index:?}");
        }
        let text = |s: &str| Scalar::Text(s.to_owned());
        assert_eq!(parse_jump_count(None), Ok(MAX_JUMP_SERVERS));
        assert_eq!(parse_jump_count(Some(&text(" "))), Ok(MAX_JUMP_SERVERS));
        assert_eq!(parse_jump_count(Some(&text("2"))), Ok(2));
        assert_eq!(parse_jump_count(Some(&text("-2"))), Ok(0));
        assert!(parse_jump_count(Some(&text("2.0"))).is_err());
        assert!(parse_jump_count(Some(&Scalar::Bool(true))).is_err());
        assert_eq!(parse_jump_enabled(None), Ok(false));
        assert_eq!(parse_jump_enabled(Some(&text("TRUE"))), Ok(true));
        assert_eq!(parse_jump_enabled(Some(&Scalar::Bool(false))), Ok(false));
        assert!(parse_jump_enabled(Some(&text("on"))).is_err());
    }

    #[test]
    fn key_name_hints() {
        use KeyNameHint::{NotRsa, Rsa, Unknown};
        for (path, hint) in [
            ("~/.ssh/id_rsa", Rsa),
            ("/k/id_rsa.old", Rsa),
            ("/k/prod.RSA.key", Rsa),
            (r"C:\k\id_rsa", Rsa),
            ("/k/id_ed25519", NotRsa),
            ("/k/id_ecdsa_sk", NotRsa),
            ("/k/bastion-ed25519", NotRsa),
            ("/k/rsa-to-ed25519", Rsa),
            ("/rsa/key", Unknown),
            ("/k/universal", Unknown),
            ("/k/key.ppk", Unknown),
            ("", Unknown),
        ] {
            assert_eq!(key_name_hint(path), hint, "{path}");
        }
    }
}
