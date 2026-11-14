//! Connection model (ROADMAP P0-D): what `connections.json` holds for each saved connection.
//!
//! Invariants carried by the types:
//! - **I1**: no field can hold a secret value. Credentials are a [`SecretSource`]: a
//!   [`SecretRef`] into the keyring / vault, or "prompt at connect time". Driver properties whose
//!   key looks like a secret are rejected by [`Connection::validate`].
//! - **I5**: TLS defaults to [`TlsMode::VerifyFull`], also when the `tls` key is absent from the
//!   file; any weaker mode is an explicit value and is reported by
//!   [`Connection::security_warnings`]. SSH host key checking cannot be turned off: there is no
//!   field for it (ADR-0011 §6).
//! - **I3**: every connection names its [`ConnectionTypeId`] explicitly (no default), and its
//!   effective read-only mode comes from the type unless overridden.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use ninesql_secrets::{SecretKind, SecretRef};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
        /// Host name or IP address (IPv6 without brackets).
        host: String,
        /// Port; `None` means the engine's default port.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        /// Database, schema or keyspace to open, if the engine has the notion.
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

/// One SSH server: the tunnel endpoint or a jump host.
///
/// There is deliberately no option to skip host key verification: unknown keys are confirmed by
/// the user, changed keys block the connection (ADR-0011 §6, I5).
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
pub enum SecurityWarning {
    /// TLS is off: traffic is readable on the network.
    TlsDisabled,
    /// TLS without certificate verification: open to man-in-the-middle.
    TlsCertificateNotVerified,
    /// Certificate verified, host name not verified.
    TlsHostnameNotVerified,
}

impl fmt::Display for SecurityWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TlsDisabled => "TLS is disabled: traffic is not encrypted",
            Self::TlsCertificateNotVerified => {
                "the server certificate is not verified: the connection can be intercepted"
            }
            Self::TlsHostnameNotVerified => "the server host name is not verified",
        })
    }
}

/// Identifies one credential field of a connection.
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
    /// secret, properties that shadow a typed field (TLS, endpoint, user, read-only), init SQL
    /// with a password literal — and returns one issue per removal. Used on load so that one bad
    /// connection does not block the whole project and the next save drops the offending data.
    pub fn sanitize(&mut self) -> Vec<ConnectionError> {
        let mut issues = Vec::new();
        self.properties
            .retain(|key, value| match property_problem(key, value) {
                Some(problem) => {
                    issues.push(problem);
                    false
                }
                None => true,
            });
        let mut index = 0;
        self.init_sql.retain(|sql| {
            let keep = !crate::properties::sql_carries_secret(sql);
            if !keep {
                issues.push(ConnectionError::SecretInInitSql { index });
            }
            index += 1;
            keep
        });
        issues
    }

    /// Weakened security settings of this connection, for the warning badge (I5).
    #[must_use]
    pub fn security_warnings(&self) -> Vec<SecurityWarning> {
        if !self.endpoint.is_network() {
            return Vec::new();
        }
        match self.tls.mode {
            TlsMode::Disable => vec![SecurityWarning::TlsDisabled],
            TlsMode::Require => vec![SecurityWarning::TlsCertificateNotVerified],
            TlsMode::VerifyCa => vec![SecurityWarning::TlsHostnameNotVerified],
            TlsMode::VerifyFull => Vec::new(),
        }
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
fn property_problem(key: &str, value: &str) -> Option<ConnectionError> {
    // Known-benign keys skip the key heuristics; their value is still checked.
    let benign = crate::properties::is_benign(key);
    if !benign && ninesql_storage::hygiene::is_secret_like_key(key) {
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
