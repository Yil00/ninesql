//! Export of saved connections to a new DBeaver `data-sources.json` (P5-D core wiring,
//! F-CON-15, ADR-0006 §7, docs/06 "Export to DBeaver").
//!
//! The generator and the safe writer are in [`ninesql_dbeaver::export`]; this module converts the
//! core model into its input and runs the export:
//! - [`From<&Connection>`] for [`ExportConnection`] and [`From<&ConnectionType>`] for
//!   [`ExportConnectionType`]. The input holds **no secret value** (I1): a password, passphrase
//!   or token is only a "saved" flag ([`SecretSource::Stored`]), which the export reports and
//!   never writes; secret references are not passed either.
//! - [`export_dbeaver_blocking`] loads the chosen connections and **all** the project's
//!   connection types (a connection whose type is missing would not be exported: fail closed),
//!   generates the file and writes it with [`write_export`] to a new file or project folder,
//!   refusing any target inside a DBeaver location (I4: the crate's own markers plus the
//!   session's [`DbeaverRoots`], which also know the folders picked for imports).
//!   `CoreServices::export_dbeaver` runs it on the blocking pool.
//!
//! When a path changes while the file is written ([`ExportWriteError::Changed`], another process
//! swapped it), nothing is exported and [`DbeaverExportError::Changed`] tells the user to choose
//! the target again; the export is never retried on its own.

use std::path::PathBuf;

use ninesql_dbeaver::export::{
    Export, ExportAuth, ExportClientCertificate, ExportConnection, ExportConnectionType,
    ExportDisabled, ExportEndpoint, ExportError, ExportHostKeyPolicy, ExportOptions, ExportReport,
    ExportSshAuth, ExportSshHop, ExportSshTunnel, ExportTarget, ExportTls, ExportWriteError, Rgb,
    TlsMode as ExportTlsMode, WrittenExport, export_connections, write_export,
};

use crate::connection::{
    Auth, Connection, ConnectionId, DisabledKind, Endpoint, HostKeyPolicy, PasswordExchange,
    SecretSource, SshAuth, SshHop, TlsMode,
};
use crate::connection_type::ConnectionType;
use crate::dbeaver_roots::DbeaverRoots;
use crate::error::WorkspaceError;
use crate::registry::api_engine;
use crate::workspace::Workspace;

/// Why a DBeaver export failed. Names the path the user chose, never file content or a secret.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DbeaverExportError {
    /// No connection was chosen.
    #[error("choose at least one connection to export")]
    NothingSelected,
    /// A chosen connection is no longer saved (deleted meanwhile).
    #[error("a chosen connection no longer exists; refresh the list and try again")]
    UnknownConnection(ConnectionId),
    /// The connections or connection types could not be read.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// The file could not be generated.
    #[error(transparent)]
    Generate(#[from] ExportError),
    /// A path changed while the file was written (another process swapped it): nothing was
    /// exported. Choose the target again.
    #[error(
        "{path} changed while the export was written: nothing was exported; choose the target again"
    )]
    Changed {
        /// The path concerned.
        path: PathBuf,
    },
    /// The file could not be written (an existing target, a DBeaver location, I/O).
    #[error(transparent)]
    Write(ExportWriteError),
    /// The export task failed.
    #[error("the export failed: {0}")]
    Runtime(String),
}

impl DbeaverExportError {
    /// A short, stable kind, safe to log.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NothingSelected => "nothing-selected",
            Self::UnknownConnection(_) => "unknown-connection",
            Self::Workspace(_) => "workspace",
            Self::Generate(_) => "generate",
            Self::Changed { .. } => "changed",
            Self::Write(ExportWriteError::InsideDbeaver { .. }) => "inside-dbeaver",
            Self::Write(ExportWriteError::AlreadyExists { .. }) => "already-exists",
            Self::Write(_) => "write",
            Self::Runtime(_) => "runtime",
        }
    }
}

impl From<ExportWriteError> for DbeaverExportError {
    fn from(e: ExportWriteError) -> Self {
        match e {
            ExportWriteError::Changed { path } => Self::Changed { path },
            other => Self::Write(other),
        }
    }
}

/// A finished DBeaver export.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DbeaverExportOutcome {
    /// What was created.
    pub written: WrittenExport,
    /// Per connection: written or skipped, and what was dropped or approximated (no value).
    pub report: ExportReport,
}

fn saved(secret: &SecretSource) -> bool {
    matches!(secret, SecretSource::Stored(_))
}

fn hop(h: &SshHop) -> ExportSshHop {
    ExportSshHop {
        host: h.host.clone(),
        port: h.port,
        username: h.username.clone(),
        auth: match &h.auth {
            SshAuth::Agent => ExportSshAuth::Agent,
            SshAuth::Password { secret } => ExportSshAuth::Password {
                saved: saved(secret),
            },
            SshAuth::PublicKey {
                key_path,
                passphrase,
            } => ExportSshAuth::PublicKey {
                key_path: key_path.clone(),
                passphrase: passphrase.as_ref().map(saved),
            },
        },
        host_key_policy: match h.host_key_policy {
            HostKeyPolicy::Strict => ExportHostKeyPolicy::Strict,
            HostKeyPolicy::AcceptNew => ExportHostKeyPolicy::AcceptNew,
            HostKeyPolicy::Disabled => ExportHostKeyPolicy::Disabled,
        },
    }
}

impl From<&Connection> for ExportConnection {
    /// The export input of a connection: no secret value and no secret reference (I1). A
    /// [`DisabledReason`](crate::DisabledReason) maps to its kind whatever `enabled` says (a
    /// route NineSQL could not reproduce is never exported); a connection disabled without a
    /// reason is [`ExportDisabled::ByUser`].
    fn from(c: &Connection) -> Self {
        let disabled = match &c.disabled_reason {
            Some(reason) => Some(match reason.kind {
                DisabledKind::NoDriver => ExportDisabled::NoDriver,
                DisabledKind::Route => ExportDisabled::Route,
                DisabledKind::Invalid => ExportDisabled::Invalid,
            }),
            None if !c.enabled => Some(ExportDisabled::ByUser),
            None => None,
        };
        Self {
            id: c.id.to_string(),
            name: c.name.clone(),
            description: c.description.clone(),
            engine: api_engine(&c.engine),
            endpoint: match &c.endpoint {
                Endpoint::Tcp {
                    host,
                    port,
                    database,
                } => ExportEndpoint::Tcp {
                    host: host.clone(),
                    port: *port,
                    database: database.clone(),
                },
                Endpoint::UnixSocket { path, database } => ExportEndpoint::UnixSocket {
                    path: path.clone(),
                    database: database.clone(),
                },
                Endpoint::File { path } => ExportEndpoint::File { path: path.clone() },
            },
            auth: match &c.auth {
                Auth::None => ExportAuth::None,
                Auth::Username { username } => ExportAuth::Username {
                    username: username.clone(),
                },
                Auth::Password { username, secret } => ExportAuth::Password {
                    username: username.clone(),
                    saved: saved(secret),
                },
                Auth::Token { username, secret } => ExportAuth::Token {
                    username: username.clone(),
                    saved: saved(secret),
                },
            },
            scram_only: c.password_exchange == PasswordExchange::ScramOnly,
            ssh: c.ssh.as_ref().map(|t| ExportSshTunnel {
                server: hop(&t.server),
                jump_hosts: t.jump_hosts.iter().map(hop).collect(),
            }),
            tls: ExportTls {
                mode: match c.tls.mode {
                    TlsMode::Disable => ExportTlsMode::Disable,
                    TlsMode::Require => ExportTlsMode::Require,
                    TlsMode::VerifyCa => ExportTlsMode::VerifyCa,
                    TlsMode::VerifyFull => ExportTlsMode::VerifyFull,
                },
                ca_file: c.tls.ca_file.clone(),
                client_certificate: c.tls.client_certificate.as_ref().map(|cc| {
                    ExportClientCertificate {
                        cert_file: cc.cert_file.clone(),
                        key_file: cc.key_file.clone(),
                        has_passphrase: cc.passphrase.is_some(),
                    }
                }),
            },
            connection_type: c.connection_type.as_str().to_owned(),
            read_only: c.read_only,
            folder: c.folder.clone(),
            properties: c.properties.clone(),
            init_sql: c.init_sql.clone(),
            init_sql_needs_confirmation: c.init_sql_needs_confirmation,
            default_schema: c.default_schema.clone(),
            default_catalog: c.default_catalog.clone(),
            auto_commit: c.auto_commit,
            disabled,
        }
    }
}

impl From<&ConnectionType> for ExportConnectionType {
    fn from(t: &ConnectionType) -> Self {
        Self {
            id: t.id.as_str().to_owned(),
            name: t.name.clone(),
            color: Rgb {
                r: t.color.r,
                g: t.color.g,
                b: t.color.b,
            },
            auto_commit: t.auto_commit,
            confirm_destructive: t.confirm_destructive,
            confirm_every_write: t.confirm_every_write,
            read_only: t.read_only,
            log_query_text: t.log_query_text,
            production: t.production,
        }
    }
}

/// Generates the export of `ids` (in that order) with all the project's connection types. No
/// file is written. **Blocking** (reads `connections.json` and `connection-types.json`).
///
/// # Errors
/// [`DbeaverExportError::NothingSelected`], [`DbeaverExportError::UnknownConnection`],
/// [`DbeaverExportError::Workspace`], [`DbeaverExportError::Generate`].
pub fn generate_dbeaver_export(
    workspace: &Workspace,
    ids: &[ConnectionId],
) -> Result<Export, DbeaverExportError> {
    if ids.is_empty() {
        return Err(DbeaverExportError::NothingSelected);
    }
    // Load issues (removed unsafe data) are reported by the UI's own load; the export uses the
    // connections as they are now saved.
    let loaded = workspace.load_connections()?;
    let connections = ids
        .iter()
        .map(|id| {
            loaded
                .connections
                .get(*id)
                .map(ExportConnection::from)
                .ok_or(DbeaverExportError::UnknownConnection(*id))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let types = workspace.load_connection_types()?;
    let options = ExportOptions {
        connection_types: types.all().iter().map(ExportConnectionType::from).collect(),
        timestamp_millis: None,
    };
    Ok(export_connections(&connections, &options)?)
}

/// Exports `ids` to `target` (a new file or a new DBeaver project folder): see the
/// [module documentation](self). **Blocking**: run it on a blocking thread
/// (`CoreServices::export_dbeaver` does).
///
/// # Errors
/// [`DbeaverExportError`]; nothing is left behind on failure.
pub fn export_dbeaver_blocking(
    workspace: &Workspace,
    roots: &DbeaverRoots,
    ids: &[ConnectionId],
    target: &ExportTarget,
) -> Result<DbeaverExportOutcome, DbeaverExportError> {
    let export = generate_dbeaver_export(workspace, ids)?;
    let written = write_export(&export, target, &|p| roots.forbids_new_file(p)).map_err(|e| {
        let e = DbeaverExportError::from(e);
        tracing::warn!(kind = e.kind(), "the DBeaver export was not written");
        e
    })?;
    tracing::info!(
        exported = export.report.exported_count(),
        selected = ids.len(),
        "DBeaver export written"
    );
    Ok(DbeaverExportOutcome {
        written,
        report: export.report,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use ninesql_secrets::SecretRef;

    use super::*;
    use crate::connection::{DisabledReason, EngineId, SshTunnel};
    use crate::connection_type::ConnectionTypeId;

    fn connection() -> Connection {
        Connection::new(
            "C",
            EngineId::new("postgresql").unwrap(),
            Endpoint::Tcp {
                host: "db.example.test".into(),
                port: Some(5432),
                database: None,
            },
            Auth::Password {
                username: "u".into(),
                secret: SecretSource::Prompt,
            },
            ConnectionTypeId::prod(),
        )
    }

    #[test]
    fn converts_flags_without_secrets() {
        let mut c = connection();
        let stored = SecretSource::Stored(SecretRef::new(
            ninesql_secrets::StoreKind::Vault,
            ninesql_secrets::SecretId::new("x:auth").unwrap(),
        ));
        c.auth = Auth::Password {
            username: "u".into(),
            secret: stored.clone(),
        };
        c.password_exchange = PasswordExchange::ScramOnly;
        c.init_sql_needs_confirmation = true;
        c.ssh = Some(SshTunnel {
            server: SshHop {
                host: "b".into(),
                port: 22,
                username: "s".into(),
                auth: SshAuth::PublicKey {
                    key_path: "/k".into(),
                    passphrase: Some(stored),
                },
                host_key_policy: HostKeyPolicy::AcceptNew,
            },
            jump_hosts: vec![],
        });
        let e = ExportConnection::from(&c);
        assert_eq!(
            e.auth,
            ExportAuth::Password {
                username: "u".into(),
                saved: true
            }
        );
        assert!(e.scram_only);
        assert!(e.init_sql_needs_confirmation);
        let ssh = e.ssh.clone().unwrap();
        assert_eq!(ssh.server.host_key_policy, ExportHostKeyPolicy::AcceptNew);
        assert_eq!(
            ssh.server.auth,
            ExportSshAuth::PublicKey {
                key_path: "/k".into(),
                passphrase: Some(true)
            }
        );
        assert_eq!(e.disabled, None);
        assert_eq!(e.connection_type, "prod");
        assert!(!format!("{e:?}").contains("x:auth"));
    }

    #[test]
    fn disabled_states() {
        let mut c = connection();
        c.enabled = false;
        assert_eq!(
            ExportConnection::from(&c).disabled,
            Some(ExportDisabled::ByUser)
        );
        c.disabled_reason = Some(DisabledReason {
            kind: DisabledKind::Route,
            message: "route".into(),
        });
        assert_eq!(
            ExportConnection::from(&c).disabled,
            Some(ExportDisabled::Route)
        );
        // A route reason is never lost, even on an enabled connection.
        c.enabled = true;
        assert_eq!(
            ExportConnection::from(&c).disabled,
            Some(ExportDisabled::Route)
        );
        c.disabled_reason = Some(DisabledReason {
            kind: DisabledKind::NoDriver,
            message: "x".into(),
        });
        assert_eq!(
            ExportConnection::from(&c).disabled,
            Some(ExportDisabled::NoDriver)
        );
    }

    #[test]
    fn types_convert() {
        let t = ExportConnectionType::from(&ConnectionType::production());
        assert!(t.is_production());
        assert!(t.confirm_destructive);
        assert_eq!(t.id, "prod");
    }
}
