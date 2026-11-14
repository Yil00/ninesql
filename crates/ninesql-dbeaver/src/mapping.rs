//! Mapping parsed DBeaver data to NineSQL's import model (ADR-0006 §3–§8,
//! docs/06-dbeaver-interop.md "Engine mapping" and "Field mapping rules").
//!
//! The output mirrors `ninesql_core::Connection` (same field names and shapes) because the core
//! depends on this crate. Differences, all on purpose:
//! - credentials are a [`SecretState`] (`Imported`: a matching [`ImportedSecret`] is in
//!   [`ImportedConnection::secrets`]; `Prompt`: nothing saved) instead of a `SecretSource`, since
//!   secrets are not in a store yet; P2-C moves each secret and sets the reference;
//! - [`ImportedConnection::default_schema`], [`ImportedConnection::default_catalog`] and
//!   [`ImportedConnection::auto_commit`] carry DBeaver settings the core model has no field for
//!   yet (each also has a report entry);
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
use ninesql_storage::hygiene::is_secret_like_key;

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
use crate::report::{ImportIssue, IssueKind, Issues, MAX_IDENT, ident};

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
    /// Jump hosts (none found in the verified DBeaver formats yet).
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
    /// Driver properties kept (non-reserved, non-secret keys).
    pub properties: BTreeMap<String, String>,
    /// Init statements (`bootstrap.query`).
    pub init_sql: Vec<String>,
    /// `bootstrap.defaultSchema` (no core field yet).
    pub default_schema: Option<String>,
    /// `bootstrap.defaultCatalog` (no core field yet).
    pub default_catalog: Option<String>,
    /// `bootstrap.autocommit` (no per-connection core field yet).
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
            .field("init_sql_count", &self.init_sql.len())
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
/// DBeaver workspace (I4: NineSQL must never modify a file under a DBeaver directory).
fn check_file_endpoints(out: &mut ProjectImport, workspace: &std::path::Path) {
    let workspace_canon = fsread::canonicalize(workspace);
    for entry in &mut out.connections {
        let Outcome::Imported(c) = &mut entry.outcome else {
            continue;
        };
        let Endpoint::File { path } = &mut c.endpoint else {
            continue;
        };
        let lexical_inside = path.starts_with(workspace);
        if let Some(canon) = fsread::canonicalize(path) {
            *path = canon;
        }
        let inside = lexical_inside
            || path.starts_with(workspace)
            || workspace_canon
                .as_ref()
                .is_some_and(|w| path.starts_with(w));
        if inside {
            c.read_only = Some(true);
            entry.issues.push(ImportIssue::new(
                crate::report::Severity::Warning,
                IssueKind::FileInsideDbeaver,
                Some("configuration.database".to_owned()),
                "the database file is inside the DBeaver workspace: imported read-only, and the \
                 driver must open it in read-only mode (NineSQL never modifies DBeaver files)",
            ));
        }
    }
}

/// Maps already parsed files of one project. `credentials` is `None` when the project has no
/// `credentials-config.json`.
pub fn map_sources(
    project: &str,
    sources: &[DataSources],
    credentials: Option<Result<Credentials, CredentialsError>>,
) -> ProjectImport {
    let mut issues = Issues::default();
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
    cap_issues(&mut out);
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
}

/// Most data-sources files read per project.
pub(crate) const MAX_DATA_SOURCE_FILES: usize = 64;
/// Most bytes of data-sources files read per project.
pub(crate) const MAX_PROJECT_BYTES: u64 = 64 * 1024 * 1024;
/// Most connections imported per project.
pub(crate) const MAX_CONNECTIONS: usize = 10_000;
/// Most folders imported per project.
pub(crate) const MAX_FOLDERS: usize = 10_000;
/// Most report entries per project (project and connection issues together).
pub(crate) const MAX_ISSUES: usize = 50_000;

/// Keeps at most [`MAX_ISSUES`] report entries, then adds one "truncated" entry. Connection
/// state (disabled, read-only, TLS mode) is not affected: only the explanations are cut.
fn cap_issues(out: &mut ProjectImport) {
    let mut left = MAX_ISSUES;
    let mut cut = false;
    let mut take = |v: &mut Vec<ImportIssue>| {
        if v.len() > left {
            v.truncate(left);
            cut = true;
        }
        left -= v.len();
    };
    take(&mut out.issues);
    for c in &mut out.connections {
        take(&mut c.issues);
    }
    if cut {
        out.issues.push(ImportIssue::new(
            crate::report::Severity::Warning,
            IssueKind::Truncated,
            None,
            format!("more than {MAX_ISSUES} report entries: the others are not shown"),
        ));
    }
}

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
    for (dbeaver_id, def) in defs {
        let prefix = format!("connection-types.{dbeaver_id}");
        let builtin = BUILTIN_TYPES.contains(dbeaver_id);
        let mut id = sanitize_type_id(dbeaver_id);
        let base = id.clone();
        let mut n = 2;
        while used.contains(&id) || (!builtin && BUILTIN_TYPES.contains(&id.as_str())) {
            id = format!("{}-{n}", truncate_bytes(&base, 60));
            n += 1;
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
        ("postgresql", "postgres-timescale-jdbc") => supported(EngineId::TIMESCALEDB, true),
        ("postgresql", d) if d.contains("yugabyte") => supported(EngineId::YUGABYTEDB, false),
        ("mysql", "mysql8") => supported(EngineId::MYSQL, true),
        ("mysql", "mysql5") => supported(EngineId::MYSQL, false),
        ("mysql", "mariadb") => supported(EngineId::MARIADB, true),
        ("mysql" | "generic", "tidb") => supported(EngineId::TIDB, false),
        ("generic" | "questdb", "questdb") => supported(EngineId::QUESTDB, false),
        ("sqlite", "sqlite_jdbc") => supported(EngineId::SQLITE, true),
        ("generic", "sqlite_jdbc") => supported(EngineId::SQLITE, false),
        ("duckdb", _) | ("generic", "duckdb_jdbc") => supported(EngineId::DUCKDB, false),
        ("clickhouse", "com_clickhouse") => supported(EngineId::CLICKHOUSE, true),
        ("clickhouse" | "generic", "yandex_clickhouse") => supported(EngineId::CLICKHOUSE, false),
        ("generic" | "trino", "trino_jdbc") => supported(EngineId::TRINO, false),
        ("jaybird" | "generic", "jaybird" | "firebird") => supported(EngineId::FIREBIRD, false),
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
}

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
        issues: Issues::default(),
        disabled: Vec::new(),
        secrets: Vec::new(),
        tls: TlsHints::default(),
        username_hint: None,
        read_only: None,
        properties: BTreeMap::new(),
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

    // Endpoint (and URL parameters, which go through the property rules).
    let endpoint = match map_endpoint(def, file_engine, &mut st) {
        Ok(e) => e,
        Err(reason) => return failed(id, name, reason, st.issues),
    };

    // Driver properties.
    for key in &cfg.properties.non_scalar {
        st.issues.warn(
            IssueKind::InvalidValue,
            Some(format!("configuration.properties.{key}")),
            "not imported: not a scalar value",
        );
    }
    for (key, value) in &cfg.properties.entries {
        let text = value.as_text();
        apply_property("configuration.properties", key, &text, &mut st);
    }
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
    for hid in &cfg.invalid_handlers {
        st.issues.warn(
            IssueKind::InvalidValue,
            Some(format!("configuration.handlers.{hid}")),
            "not imported: not a JSON object",
        );
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
    let mut scopes: Vec<(String, &crate::model::Extra)> = vec![
        (String::new(), &def.extra),
        ("configuration".to_owned(), &cfg.extra),
    ];
    for h in &cfg.handlers {
        scopes.push((format!("configuration.handlers.{}", h.id), &h.extra));
    }
    for (scope, extra) in scopes {
        for key in extra.keys() {
            let norm = normalize_key(key);
            let references_profile = is_network_profile_key(&norm)
                || (ctx.network_profiles
                    && (norm.starts_with("network") || norm.starts_with("profile")));
            if references_profile {
                st.issues.warn(
                    IssueKind::Disabled,
                    Some(join(&scope, key)),
                    "the connection uses a DBeaver network profile, which is not supported yet \
                     (F-CON-17): imported disabled so that it never bypasses the profile's \
                     tunnel or proxy",
                );
                st.disabled
                    .push("DBeaver network profile not supported yet".to_owned());
            }
        }
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

/// Normalized key naming a network profile (`network-profile`, `networkProfile`,
/// `network_profile_id`…).
fn is_network_profile_key(norm: &str) -> bool {
    norm.contains("networkprofile")
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

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// One driver property or URL parameter: TLS keys feed the TLS settings, `user` the user name,
/// `readOnly=true` the read-only flag; secret-like and reserved keys are dropped and reported;
/// the rest is kept.
fn apply_property(prefix: &str, key: &str, value: &str, st: &mut ConnState) {
    let path = join(prefix, key);
    if key.trim().is_empty() || !is_valid_short(key) {
        st.issues.warn(
            IssueKind::InvalidValue,
            Some(prefix.to_owned()),
            "a driver property with an empty, too long or invalid key was not imported",
        );
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
    if is_secret_like_key(key) {
        st.issues.warn(
            IssueKind::SecretRemoved,
            Some(path),
            "looks like a secret: removed, not imported (secrets only go to the keyring)",
        );
        return;
    }
    if is_reserved_property(key) {
        st.issues.warn(
            IssueKind::ReservedProperty,
            Some(path),
            "not imported: reserved for NineSQL's typed settings (TLS, endpoint, credentials, \
             read-only, init SQL); set it there",
        );
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
    for (key, _) in &props.entries {
        if CONSUMED.contains(&key.as_str()) {
            continue;
        }
        if is_secret_like_key(key) {
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
    Ok(SshTunnel {
        server: SshHop {
            host,
            port,
            username,
            auth,
        },
        jump_hosts: Vec::new(),
    })
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
            k if is_secret_like_key(k) => st.issues.warn(
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
            k if is_secret_like_key(k) => st.issues.warn(
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
}
