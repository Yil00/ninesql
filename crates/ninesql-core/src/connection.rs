//! Connection model (ROADMAP P0-D): what `connections.json` holds for each saved connection.
//!
//! Invariants carried by the types:
//! - **I1**: no field can hold a secret value. Credentials are a [`SecretSource`]: a
//!   [`SecretRef`] into the keyring / vault, or "prompt at connect time". Driver properties whose
//!   key looks like a secret, or that the driver API reserves
//!   ([`ninesql_driver_api::is_reserved_property`]), are rejected by [`Connection::validate`]
//!   and removed by [`Connection::sanitize`] on load.
//! - **I5**: TLS defaults to [`TlsMode::VerifyFull`], also when the `tls` key is absent from the
//!   file; any weaker mode is an explicit value and is reported by
//!   [`Connection::security_warnings`]. SSH host keys are checked strictly by default on every
//!   hop; a weaker per-hop [`HostKeyPolicy`] is an explicit value, also reported by
//!   [`Connection::security_warnings`] (ADR-0011 §6 as amended by ADR-0016 §2).
//! - **I3**: every connection names its [`ConnectionTypeId`] explicitly (no default), and its
//!   effective read-only mode comes from the type unless overridden.
//!
//! **Contract for drivers (review item L6)**: host names, user names and database names are
//! checked for length and control characters only (host names also refuse whitespace, `@`, `/`
//! and `\`). They may legitimately contain characters that are significant in connection strings
//! and URLs: `;`, `=`, `'`, `"`, `?`, `&`, `%`, `:`, `{`, `}`, spaces (in user and database names).
//! Drivers must pass them as **structured configuration** (typed config fields of the client
//! library, or properly escaped / percent-encoded values when a library only takes a URL), and
//! never interpolate them into a DSN, a URL or a `key=value` connection string as they are.
//! Otherwise a database named `app;sslmode=disable` could inject options.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use ninesql_secrets::{SecretKind, SecretRef};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::connection_type::{ConnectionTypeId, ConnectionTypes, SafetyPolicy};
use crate::error::ConnectionError;

/// Stable identifier of a connection (UUID v4), also used to name its secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionId(Uuid);

impl ConnectionId {
    /// A new random identifier.
    #[must_use]
    pub fn new_random() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wraps an existing UUID.
    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

/// Identifier of a database engine (`postgresql`, `mysql`, `sqlite`…), as declared by drivers.
///
/// Lower-case ASCII letter first, then letters, digits, `-` or `_`; at most 64 bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EngineId(String);

impl EngineId {
    /// Validates an engine identifier.
    ///
    /// # Errors
    /// [`ConnectionError::InvalidEngineId`].
    pub fn new(id: impl Into<String>) -> Result<Self, ConnectionError> {
        let id = id.into();
        let mut bytes = id.bytes();
        let valid = id.len() <= 64
            && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if valid {
            Ok(Self(id))
        } else {
            Err(ConnectionError::InvalidEngineId)
        }
    }

    /// The identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EngineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for EngineId {
    type Error = ConnectionError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<EngineId> for String {
    fn from(value: EngineId) -> Self {
        value.0
    }
}

/// Where the database is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Endpoint {
    /// A server reached over TCP (possibly through an SSH tunnel or a proxy).
    Tcp {
        /// Host name or IP address (IPv6 without brackets). Never contains whitespace, `@`, `/`
        /// or `\`, but may contain other DSN-significant characters: drivers pass it as
        /// structured configuration (see the module documentation).
        host: String,
        /// Port; `None` means the engine's default port.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        /// Database, schema or keyspace to open, if the engine has the notion. May contain any
        /// printable character, including DSN-significant ones (`;`, `=`, `'`, `?`, spaces):
        /// drivers pass it as structured configuration (see the module documentation).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        database: Option<String>,
    },
    /// A server reached through a local Unix socket.
    UnixSocket {
        /// Socket path, or the directory holding it for engines that expect one (PostgreSQL).
        path: PathBuf,
        /// Database to open.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        database: Option<String>,
    },
    /// An embedded engine's database file (SQLite, DuckDB…).
    File {
        /// Database file path.
        path: PathBuf,
    },
}

impl Endpoint {
    /// Whether traffic leaves the machine over a network socket (TLS settings apply).
    #[must_use]
    pub const fn is_network(&self) -> bool {
        matches!(self, Self::Tcp { .. })
    }
}

/// Where a credential comes from. Never the credential itself (I1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum SecretSource {
    /// Saved in the keyring or the vault.
    Stored(SecretRef),
    /// Not saved: asked at connect time and kept in memory for the session only (ADR-0005 §3).
    Prompt,
}

impl SecretSource {
    /// The reference, when the secret is saved.
    #[must_use]
    pub const fn stored(&self) -> Option<&SecretRef> {
        match self {
            Self::Stored(r) => Some(r),
            Self::Prompt => None,
        }
    }
}

/// Database authentication.
///
/// User names may contain any printable character, including DSN-significant ones (`;`, `=`,
/// `'`, `@`, `:`, spaces): drivers pass them as structured configuration, never inside a URL or a
/// connection string as they are (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum Auth {
    /// No credentials (embedded engines, unauthenticated servers).
    None,
    /// A user name without a password (`trust`, `peer`, `ident`, socket authentication).
    Username {
        /// User name.
        username: String,
    },
    /// User name and password.
    Password {
        /// User name.
        username: String,
        /// Where the password comes from.
        secret: SecretSource,
    },
    /// A token (API key, access token), with an optional user name.
    Token {
        /// User name, if the engine wants one with the token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        /// Where the token comes from.
        secret: SecretSource,
    },
}

impl Auth {
    /// The user name, if any.
    #[must_use]
    pub fn username(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Username { username } | Self::Password { username, .. } => Some(username),
            Self::Token { username, .. } => username.as_deref(),
        }
    }
}

fn default_ssh_port() -> u16 {
    22
}

/// SSH authentication of one hop (ADR-0011 §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum SshAuth {
    /// Keys from `ssh-agent` (`SSH_AUTH_SOCK`).
    Agent,
    /// Password.
    Password {
        /// Where the password comes from.
        secret: SecretSource,
    },
    /// Private key file (OpenSSH format). NineSQL stores only the path (ADR-0005 §6).
    PublicKey {
        /// Path of the private key.
        key_path: PathBuf,
        /// Passphrase of the key; `None` for an unencrypted key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase: Option<SecretSource>,
    },
}

/// Host key policy of one SSH hop (ADR-0016 §2). Defaults to [`HostKeyPolicy::Strict`], also
/// when the key is absent from the file.
///
/// Anything but `Strict` is an explicit, per-connection, per-hop downgrade, always reported by
/// [`Connection::security_warnings`] (I5). Neither weaker policy is ever a default, and the
/// DBeaver import never sets one.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum HostKeyPolicy {
    /// Known key: accepted. Unknown key: the user confirms its fingerprint. Changed or revoked
    /// key: the connection is blocked. The default.
    #[default]
    Strict,
    /// Unknown keys are recorded in NineSQL's `known_hosts` without asking (trust on first use);
    /// changed and revoked keys still block, and a host known only under another key type still
    /// needs confirmation.
    AcceptNew,
    /// The host key is not checked (revoked keys are still refused): no protection against a
    /// man in the middle on this hop.
    Disabled,
}

impl HostKeyPolicy {
    /// `true` for anything weaker than [`HostKeyPolicy::Strict`].
    #[must_use]
    pub const fn is_downgrade(self) -> bool {
        !matches!(self, Self::Strict)
    }

    /// The `ninesql-ssh` policy.
    #[must_use]
    pub const fn to_ssh(self) -> ninesql_ssh::HostKeyPolicy {
        match self {
            Self::Strict => ninesql_ssh::HostKeyPolicy::Strict,
            Self::AcceptNew => ninesql_ssh::HostKeyPolicy::AcceptNew,
            Self::Disabled => ninesql_ssh::HostKeyPolicy::Disabled,
        }
    }
}

fn is_strict(policy: &HostKeyPolicy) -> bool {
    !policy.is_downgrade()
}

/// One SSH server: the tunnel endpoint or a jump host.
///
/// Host keys are verified on every hop (ADR-0011 §6, I5): under the default
/// [`HostKeyPolicy::Strict`], unknown keys are confirmed by the user and changed keys block the
/// connection. A hop may carry an explicit weaker policy ([`HostKeyPolicy::AcceptNew`],
/// [`HostKeyPolicy::Disabled`]) per ADR-0016 §2; it is always reported as a security warning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHop {
    /// SSH server host.
    pub host: String,
    /// SSH server port.
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// SSH user name.
    pub username: String,
    /// Authentication.
    pub auth: SshAuth,
    /// Host key policy of this hop; strict when absent (ADR-0016).
    #[serde(default, skip_serializing_if = "is_strict")]
    pub host_key_policy: HostKeyPolicy,
}

impl SshHop {
    /// A hop with the strict host key policy.
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        auth: SshAuth,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            auth,
            host_key_policy: HostKeyPolicy::Strict,
        }
    }
}

impl SshTunnel {
    /// Every hop in connection order: jump hosts first, the tunnel server last (the hop indices
    /// of `ninesql-ssh` errors and warnings).
    pub fn hops(&self) -> impl Iterator<Item = &SshHop> {
        self.jump_hosts.iter().chain(std::iter::once(&self.server))
    }
}

/// SSH tunnel: local port forwarding through `jump_hosts` (in order) then `server` (ADR-0011 §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTunnel {
    /// Server that forwards to the database endpoint.
    #[serde(flatten)]
    pub server: SshHop,
    /// Jump hosts crossed before `server`, first one first (ProxyJump order).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jump_hosts: Vec<SshHop>,
}

/// TLS mode, in the engine-neutral terms of ADR-0011 §2.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    /// No TLS.
    Disable,
    /// Encrypted, certificate not verified.
    Require,
    /// Certificate chain verified, host name not verified.
    VerifyCa,
    /// Certificate chain and host name verified. The default.
    #[default]
    VerifyFull,
}

/// Which password exchanges the driver may use for this connection (`connections.json` schema 3).
/// Defaults to [`PasswordExchange::Any`], also when the key is absent from the file.
///
/// [`PasswordExchange::ScramOnly`] is accepted at save only by drivers that declare it
/// ([`ninesql_driver_api::ConnectCapabilities::password_exchanges`], checked by
/// [`Connection::validate_for_driver`]); today `ninesql-driver-pgwire`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum PasswordExchange {
    /// Every exchange the driver supports, under its own transport rules. The default.
    #[default]
    Any,
    /// Only a SCRAM exchange in which the server proves it knows the password verifier
    /// (PostgreSQL `SCRAM-SHA-256`): md5, cleartext and trust are refused. It stops method
    /// downgrades, **not** a man in the middle that relays the exchange: there is no channel
    /// binding, so only TLS `verify-full` (or `verify-ca` with a private CA) protects against
    /// that ([`Connection::scram_without_verified_tls`]).
    ScramOnly,
}

impl PasswordExchange {
    /// `true` for [`PasswordExchange::Any`].
    #[must_use]
    pub const fn is_any(self) -> bool {
        matches!(self, Self::Any)
    }

    /// The driver API policy.
    #[must_use]
    pub const fn to_api(self) -> ninesql_driver_api::PasswordExchange {
        match self {
            Self::Any => ninesql_driver_api::PasswordExchange::Any,
            Self::ScramOnly => ninesql_driver_api::PasswordExchange::ScramOnly,
        }
    }
}

fn is_any_exchange(p: &PasswordExchange) -> bool {
    p.is_any()
}

/// TLS client certificate (mTLS, ADR-0011 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCertificate {
    /// PEM certificate (chain) file.
    pub cert_file: PathBuf,
    /// PEM private key file.
    pub key_file: PathBuf,
    /// Passphrase of the key; `None` for an unencrypted key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<SecretSource>,
}

/// TLS settings of a network connection. `Default` is `verify-full` with the platform roots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Verification mode.
    #[serde(default)]
    pub mode: TlsMode,
    /// Extra CA bundle trusted for this connection, on top of the platform roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
    /// Client certificate, if the server asks for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_certificate: Option<ClientCertificate>,
}

/// Where a connection comes from (ADR-0006 §8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Origin {
    /// Created in NineSQL.
    #[default]
    Native,
    /// Imported from a DBeaver workspace; re-imports match on `dbeaver_id`.
    Dbeaver {
        /// The DBeaver data source id (`postgres-jdbc-18c…`).
        dbeaver_id: String,
        /// DBeaver project the connection came from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
}

/// A weakened security setting, shown on the connection and its tabs (ADR-0011 §2, I5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecurityWarning {
    /// TLS is off: traffic is readable on the network.
    TlsDisabled,
    /// TLS without certificate verification: open to man-in-the-middle.
    TlsCertificateNotVerified,
    /// Certificate verified, host name not verified.
    TlsHostnameNotVerified,
    /// Unknown SSH host keys of this hop are accepted without confirmation (ADR-0016). `hop`
    /// counts jump hosts first, the tunnel server last.
    SshAcceptNewHostKeys {
        /// Hop index.
        hop: usize,
    },
    /// The SSH host key of this hop is not verified (ADR-0016).
    SshHostKeyNotVerified {
        /// Hop index.
        hop: usize,
    },
}

impl fmt::Display for SecurityWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TlsDisabled => "TLS is disabled: traffic is not encrypted",
            Self::TlsCertificateNotVerified => {
                "the server certificate is not verified: the connection can be intercepted"
            }
            Self::TlsHostnameNotVerified => "the server host name is not verified",
            Self::SshAcceptNewHostKeys { .. } => {
                "unknown SSH host keys are accepted without confirmation on an SSH hop"
            }
            Self::SshHostKeyNotVerified { .. } => {
                "the SSH host key of an SSH hop is not verified: the tunnel can be intercepted"
            }
        })
    }
}

/// Identifies one credential field of a connection.
///
/// The suffix `proxy` is reserved for the proxy password ([`SecretKind::ProxyPassword`]) of the
/// proxy hop (ADR-0011 §8), added with the proxy field of the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretSlot {
    /// Database password or token ([`Connection::auth`]).
    Auth,
    /// Password or key passphrase of the SSH server.
    SshServer,
    /// Password or key passphrase of a jump host (index in `jump_hosts`).
    SshJump(usize),
    /// Passphrase of the TLS client key.
    TlsClientKey,
}

impl SecretSlot {
    /// Suffix used in the [`ninesql_secrets::SecretId`] of the slot (`<connection id>:<suffix>`).
    #[must_use]
    pub fn id_suffix(self) -> String {
        match self {
            Self::Auth => "auth".to_owned(),
            Self::SshServer => "ssh".to_owned(),
            Self::SshJump(i) => format!("ssh-jump-{i}"),
            Self::TlsClientKey => "tls-client-key".to_owned(),
        }
    }
}

fn is_true(b: &bool) -> bool {
    *b
}

fn default_true() -> bool {
    true
}

/// A saved connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    /// Stable identifier.
    pub id: ConnectionId,
    /// Display name.
    pub name: String,
    /// Free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Engine.
    pub engine: EngineId,
    /// Where the database is.
    pub endpoint: Endpoint,
    /// Database authentication.
    pub auth: Auth,
    /// Password exchanges the driver may use; [`PasswordExchange::Any`] when absent
    /// (`connections.json` schema 3).
    #[serde(default, skip_serializing_if = "is_any_exchange")]
    pub password_exchange: PasswordExchange,
    /// SSH tunnel, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshTunnel>,
    /// TLS settings; `verify-full` when absent (I5). Ignored for non-network endpoints.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Connection type (Dev / Test / Production or user-defined). Required (ADR-0007 consequences).
    pub connection_type: ConnectionTypeId,
    /// Overrides the type's read-only default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// Folder path in the navigator, outermost first; empty for the root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folder: Vec<String>,
    /// Driver properties (non-secret engine options).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
    /// Statements run right after connecting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub init_sql: Vec<String>,
    /// Disabled connections are listed but cannot be opened (for example engines outside the v1
    /// set imported from DBeaver, ADR-0006 §5).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Where the connection comes from.
    #[serde(default)]
    pub origin: Origin,
}

/// Maximum length of short text fields (names, hosts, user names), in bytes.
const MAX_SHORT_TEXT: usize = 255;

impl Connection {
    /// A native connection with a fresh id, default TLS and no SSH.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        engine: EngineId,
        endpoint: Endpoint,
        auth: Auth,
        connection_type: ConnectionTypeId,
    ) -> Self {
        Self {
            id: ConnectionId::new_random(),
            name: name.into(),
            description: None,
            engine,
            endpoint,
            auth,
            password_exchange: PasswordExchange::Any,
            ssh: None,
            tls: TlsConfig::default(),
            connection_type,
            read_only: None,
            folder: Vec::new(),
            properties: BTreeMap::new(),
            init_sql: Vec::new(),
            enabled: true,
            origin: Origin::Native,
        }
    }

    /// Checks the fields that the type system does not.
    ///
    /// # Errors
    /// The first invalid field found.
    pub fn validate(&self) -> Result<(), ConnectionError> {
        short_text("name", &self.name, false)?;
        match &self.endpoint {
            Endpoint::Tcp {
                host,
                port,
                database,
            } => {
                host_name("endpoint.host", host)?;
                if *port == Some(0) {
                    return Err(self.invalid("endpoint.port", "must be between 1 and 65535"));
                }
                if let Some(db) = database {
                    short_text("endpoint.database", db, true)?;
                }
            }
            Endpoint::UnixSocket { path, database } => {
                non_empty_path("endpoint.path", path)?;
                if let Some(db) = database {
                    short_text("endpoint.database", db, true)?;
                }
            }
            Endpoint::File { path } => non_empty_path("endpoint.path", path)?,
        }
        if let Some(username) = self.auth.username() {
            short_text("auth.username", username, true)?;
        }
        if let Some(ssh) = &self.ssh {
            hop("ssh", &ssh.server)?;
            for (i, jump) in ssh.jump_hosts.iter().enumerate() {
                hop(&format!("ssh.jump_hosts[{i}]"), jump)?;
            }
        }
        if let Some(cert) = &self.tls.client_certificate {
            non_empty_path("tls.client_certificate.cert_file", &cert.cert_file)?;
            non_empty_path("tls.client_certificate.key_file", &cert.key_file)?;
        }
        if let Some(ca) = &self.tls.ca_file {
            non_empty_path("tls.ca_file", ca)?;
        }
        for segment in &self.folder {
            short_text("folder", segment, false)?;
            if segment.contains('/') {
                return Err(self.invalid("folder", "segments cannot contain `/`"));
            }
        }
        for (key, value) in &self.properties {
            short_text("properties", key, false)?;
            if let Some(problem) = property_problem(key, value) {
                return Err(problem);
            }
        }
        for (index, sql) in self.init_sql.iter().enumerate() {
            if crate::properties::sql_carries_secret(sql) {
                return Err(ConnectionError::SecretInInitSql { index });
            }
        }
        if let Origin::Dbeaver { dbeaver_id, .. } = &self.origin {
            short_text("origin.dbeaver_id", dbeaver_id, false)?;
        }
        Ok(())
    }

    fn invalid(&self, field: &'static str, reason: &'static str) -> ConnectionError {
        ConnectionError::InvalidField {
            field: field.to_owned(),
            reason,
        }
    }

    /// Removes what must never be kept in a workspace file — properties that hold or look like a
    /// secret, properties reserved by the driver API rule ([`crate::properties::reserved`]: TLS,
    /// endpoint, credentials, read-only, init SQL, session modes…), init SQL with a password
    /// literal — and returns one issue per removal. Removed values are zeroized. Used on load so
    /// that one bad connection does not block the whole project; the loader then rewrites the
    /// file at once without the offending data.
    pub fn sanitize(&mut self) -> Vec<ConnectionError> {
        let mut issues = Vec::new();
        // Removed values may be secrets: zeroized before they are dropped (best effort, N7.4).
        self.properties
            .retain(|key, value| match property_problem(key, value) {
                Some(problem) => {
                    issues.push(problem);
                    value.zeroize();
                    false
                }
                None => true,
            });
        let mut index = 0;
        self.init_sql.retain_mut(|sql| {
            let keep = !crate::properties::sql_carries_secret(sql);
            if !keep {
                issues.push(ConnectionError::SecretInInitSql { index });
                sql.zeroize();
            }
            index += 1;
            keep
        });
        issues
    }

    /// `true` when [`PasswordExchange::ScramOnly`] is set on a network endpoint whose TLS mode
    /// is below `verify-ca`: SCRAM without channel binding does not stop a relaying man in the
    /// middle there, so the UI flags the combination (ROADMAP, driver-API PR, P1-C row).
    #[must_use]
    pub fn scram_without_verified_tls(&self) -> bool {
        self.password_exchange == PasswordExchange::ScramOnly
            && self.endpoint.is_network()
            && matches!(self.tls.mode, TlsMode::Disable | TlsMode::Require)
    }

    /// Weakened security settings of this connection, for the warning badge (I5).
    #[must_use]
    pub fn security_warnings(&self) -> Vec<SecurityWarning> {
        let mut out = Vec::new();
        if self.endpoint.is_network() {
            match self.tls.mode {
                TlsMode::Disable => out.push(SecurityWarning::TlsDisabled),
                TlsMode::Require => out.push(SecurityWarning::TlsCertificateNotVerified),
                TlsMode::VerifyCa => out.push(SecurityWarning::TlsHostnameNotVerified),
                TlsMode::VerifyFull => {}
            }
        }
        if let Some(ssh) = &self.ssh {
            for (hop, h) in ssh.hops().enumerate() {
                match h.host_key_policy {
                    HostKeyPolicy::Strict => {}
                    HostKeyPolicy::AcceptNew => {
                        out.push(SecurityWarning::SshAcceptNewHostKeys { hop });
                    }
                    HostKeyPolicy::Disabled => {
                        out.push(SecurityWarning::SshHostKeyNotVerified { hop });
                    }
                }
            }
        }
        out
    }

    /// Effective safety rules of this connection: its type resolved through
    /// [`ConnectionTypes::resolve`] (an unknown type gets production rules), then the
    /// connection's `read_only` override. On a production type the override can only tighten:
    /// `read_only: false` never weakens a read-only production type (I3).
    #[must_use]
    pub fn policy(&self, types: &ConnectionTypes) -> SafetyPolicy {
        let mut policy = types.resolve(&self.connection_type).policy();
        policy.read_only = match self.read_only {
            Some(true) => true,
            Some(false) if !policy.production => false,
            Some(false) | None => policy.read_only,
        };
        policy
    }

    /// Whether sessions of this connection are read-only (enforced engine-side by drivers, I3).
    /// Shortcut for `self.policy(types).read_only`.
    #[must_use]
    pub fn effective_read_only(&self, types: &ConnectionTypes) -> bool {
        self.policy(types).read_only
    }

    /// Every saved secret reference of this connection, with its kind.
    #[must_use]
    pub fn secret_refs(&self) -> Vec<(SecretSlot, SecretKind, &SecretRef)> {
        let mut refs = Vec::new();
        match &self.auth {
            Auth::Password { secret, .. } => {
                if let Some(r) = secret.stored() {
                    refs.push((SecretSlot::Auth, SecretKind::Password, r));
                }
            }
            Auth::Token { secret, .. } => {
                if let Some(r) = secret.stored() {
                    refs.push((SecretSlot::Auth, SecretKind::Token, r));
                }
            }
            Auth::None | Auth::Username { .. } => {}
        }
        if let Some(ssh) = &self.ssh {
            let hops = std::iter::once((SecretSlot::SshServer, &ssh.server)).chain(
                ssh.jump_hosts
                    .iter()
                    .enumerate()
                    .map(|(i, h)| (SecretSlot::SshJump(i), h)),
            );
            for (slot, hop) in hops {
                match &hop.auth {
                    SshAuth::Password { secret } => {
                        if let Some(r) = secret.stored() {
                            refs.push((slot, SecretKind::SshPassword, r));
                        }
                    }
                    SshAuth::PublicKey {
                        passphrase: Some(secret),
                        ..
                    } => {
                        if let Some(r) = secret.stored() {
                            refs.push((slot, SecretKind::SshKeyPassphrase, r));
                        }
                    }
                    SshAuth::PublicKey {
                        passphrase: None, ..
                    }
                    | SshAuth::Agent => {}
                }
            }
        }
        if let Some(ClientCertificate {
            passphrase: Some(secret),
            ..
        }) = &self.tls.client_certificate
            && let Some(r) = secret.stored()
        {
            refs.push((SecretSlot::TlsClientKey, SecretKind::ClientKeyPassphrase, r));
        }
        refs
    }

    /// Points a credential field at a saved secret and returns the secret's kind.
    ///
    /// # Errors
    /// [`ConnectionError::NoSuchSecretSlot`] when the connection has no credential at `slot`
    /// (for example an SSH password slot on an agent-authenticated hop).
    pub fn set_secret_ref(
        &mut self,
        slot: SecretSlot,
        reference: SecretRef,
    ) -> Result<SecretKind, ConnectionError> {
        let stored = SecretSource::Stored(reference);
        let no_slot = || ConnectionError::NoSuchSecretSlot(slot);
        match slot {
            SecretSlot::Auth => match &mut self.auth {
                Auth::Password { secret, .. } => {
                    *secret = stored;
                    Ok(SecretKind::Password)
                }
                Auth::Token { secret, .. } => {
                    *secret = stored;
                    Ok(SecretKind::Token)
                }
                Auth::None | Auth::Username { .. } => Err(no_slot()),
            },
            SecretSlot::SshServer | SecretSlot::SshJump(_) => {
                let ssh = self.ssh.as_mut().ok_or_else(no_slot)?;
                let hop = match slot {
                    SecretSlot::SshJump(i) => ssh.jump_hosts.get_mut(i).ok_or_else(no_slot)?,
                    _ => &mut ssh.server,
                };
                match &mut hop.auth {
                    SshAuth::Password { secret } => {
                        *secret = stored;
                        Ok(SecretKind::SshPassword)
                    }
                    SshAuth::PublicKey { passphrase, .. } => {
                        *passphrase = Some(stored);
                        Ok(SecretKind::SshKeyPassphrase)
                    }
                    SshAuth::Agent => Err(no_slot()),
                }
            }
            SecretSlot::TlsClientKey => {
                let cert = self.tls.client_certificate.as_mut().ok_or_else(no_slot)?;
                cert.passphrase = Some(stored);
                Ok(SecretKind::ClientKeyPassphrase)
            }
        }
    }
}

/// Why a driver property cannot be kept, if it cannot. Never includes the value.
///
/// Order: secret-like key or key / value pair (I1), then the reserved-key rule shared with the
/// drivers ([`crate::properties::reserved`], which no benign list overrides), then the value
/// detectors.
pub(crate) fn property_problem(key: &str, value: &str) -> Option<ConnectionError> {
    if ninesql_storage::hygiene::is_secret_like_key(key)
        || ninesql_storage::hygiene::is_secret_like_entry(key, value)
    {
        return Some(ConnectionError::SecretInProperties {
            key: key.to_owned(),
        });
    }
    if let Some(reserved) = crate::properties::reserved(key) {
        return Some(ConnectionError::ReservedProperty {
            key: key.to_owned(),
            use_instead: reserved.model_field(),
        });
    }
    if crate::properties::value_carries_secret(value) {
        return Some(ConnectionError::SecretInPropertyValue {
            key: key.to_owned(),
        });
    }
    None
}

fn short_text(field: &str, value: &str, allow_empty: bool) -> Result<(), ConnectionError> {
    let reason = if !allow_empty && value.trim().is_empty() {
        Some("cannot be empty")
    } else if value.len() > MAX_SHORT_TEXT {
        Some("is too long (255 bytes at most)")
    } else if value.chars().any(char::is_control) {
        Some("cannot contain control characters")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ConnectionError::InvalidField {
            field: field.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

fn host_name(field: &str, host: &str) -> Result<(), ConnectionError> {
    short_text(field, host, false)?;
    // `@` and `/` would let a URL with user info (`user:password@host`) hide in the host field.
    if host
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '@' | '/' | '\\'))
    {
        return Err(ConnectionError::InvalidField {
            field: field.to_owned(),
            reason: "must be a host name or an IP address",
        });
    }
    Ok(())
}

fn non_empty_path(field: &str, path: &std::path::Path) -> Result<(), ConnectionError> {
    if path.as_os_str().is_empty() {
        return Err(ConnectionError::InvalidField {
            field: field.to_owned(),
            reason: "cannot be empty",
        });
    }
    Ok(())
}

fn hop(field: &str, hop: &SshHop) -> Result<(), ConnectionError> {
    host_name(&format!("{field}.host"), &hop.host)?;
    short_text(&format!("{field}.username"), &hop.username, false)?;
    if hop.port == 0 {
        return Err(ConnectionError::InvalidField {
            field: format!("{field}.port"),
            reason: "must be between 1 and 65535",
        });
    }
    if let SshAuth::PublicKey { key_path, .. } = &hop.auth {
        non_empty_path(&format!("{field}.key_path"), key_path)?;
    }
    Ok(())
}
