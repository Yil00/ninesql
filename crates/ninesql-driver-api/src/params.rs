//! Connection parameters handed to [`Driver::connect`](crate::Driver::connect).
//!
//! Drivers never read workspace files or the secret store (docs/02-architecture.md): the core
//! resolves everything, including the secret and the SSH tunnel, and passes a fully resolved
//! [`ConnectParams`] by value. Security-relevant defaults follow ADR-0011 (I5): TLS defaults to
//! [`TlsMode::VerifyFull`] and SSH host keys to [`HostKeyVerification::Strict`]. Every downgrade
//! is listed by [`ConnectParams::security_warnings`] so the UI can flag it visibly.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::{DriverError, DriverResult};
use crate::secret::Secret;

/// Identifier of one engine (not one family): `postgresql`, `timescaledb`, `mysql`…
///
/// The authoritative list is the table in docs/08-engine-capabilities.md; constants are provided
/// for the v1 set (ADR-0003).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EngineId(Cow<'static, str>);

impl EngineId {
    /// PostgreSQL (pgwire family).
    pub const POSTGRESQL: Self = Self::from_static("postgresql");
    /// TimescaleDB (pgwire family).
    pub const TIMESCALEDB: Self = Self::from_static("timescaledb");
    /// YugabyteDB YSQL (pgwire family).
    pub const YUGABYTEDB: Self = Self::from_static("yugabytedb");
    /// QuestDB (pgwire family).
    pub const QUESTDB: Self = Self::from_static("questdb");
    /// MySQL (mysqlwire family).
    pub const MYSQL: Self = Self::from_static("mysql");
    /// MariaDB (mysqlwire family).
    pub const MARIADB: Self = Self::from_static("mariadb");
    /// TiDB (mysqlwire family).
    pub const TIDB: Self = Self::from_static("tidb");
    /// SQLite.
    pub const SQLITE: Self = Self::from_static("sqlite");
    /// DuckDB.
    pub const DUCKDB: Self = Self::from_static("duckdb");
    /// ClickHouse.
    pub const CLICKHOUSE: Self = Self::from_static("clickhouse");
    /// Trino.
    pub const TRINO: Self = Self::from_static("trino");
    /// Firebird.
    pub const FIREBIRD: Self = Self::from_static("firebird");
    /// InfluxDB 3 Core.
    pub const INFLUXDB: Self = Self::from_static("influxdb");
    /// MongoDB.
    pub const MONGODB: Self = Self::from_static("mongodb");
    /// Redis.
    pub const REDIS: Self = Self::from_static("redis");
    /// Valkey.
    pub const VALKEY: Self = Self::from_static("valkey");
    /// Elasticsearch.
    pub const ELASTICSEARCH: Self = Self::from_static("elasticsearch");
    /// OpenSearch.
    pub const OPENSEARCH: Self = Self::from_static("opensearch");
    /// Apache Cassandra.
    pub const CASSANDRA: Self = Self::from_static("cassandra");
    /// Neo4j Community.
    pub const NEO4J: Self = Self::from_static("neo4j");

    /// Builds an id from a static string.
    #[must_use]
    pub const fn from_static(id: &'static str) -> Self {
        Self(Cow::Borrowed(id))
    }

    /// Builds an id from an owned string (e.g. read from a workspace file).
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(Cow::Owned(id.into()))
    }

    /// The id as a string slice.
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

/// Where the engine lives.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Endpoint {
    /// A network server. When an SSH tunnel is used, this is the tunnel's local end
    /// (`127.0.0.1:<ephemeral>`), and [`TlsConfig::server_name`] carries the real host name.
    Tcp {
        /// Host name or IP address.
        host: String,
        /// TCP port.
        port: u16,
    },
    /// A file engine (SQLite, DuckDB).
    File {
        /// Path of the database file.
        path: PathBuf,
        /// Create the file if it does not exist. Ignored for read-only sessions.
        create_if_missing: bool,
    },
    /// An in-memory database (file engines only).
    InMemory,
}

/// How the session authenticates. Build with [`Auth::password`] / [`Auth::token`].
#[derive(Debug, Default)]
#[non_exhaustive]
pub enum Auth {
    /// No authentication (or authentication by TLS client certificate only).
    #[default]
    None,
    /// User name and password. `password: None` means an empty password was explicitly chosen.
    #[non_exhaustive]
    Password {
        /// Database user.
        user: String,
        /// Password, resolved by `ninesql-secrets` at connect time.
        password: Option<Secret>,
    },
    /// Bearer / API token (InfluxDB, Elasticsearch, Trino…).
    #[non_exhaustive]
    Token {
        /// Optional user name sent with the token.
        user: Option<String>,
        /// Token, resolved by `ninesql-secrets` at connect time.
        token: Secret,
    },
}

/// TLS mode, in the engine-neutral terms of ADR-0011. Defaults to [`TlsMode::VerifyFull`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum TlsMode {
    /// Plain text. Flagged as a downgrade.
    Disable,
    /// Encrypted, certificate not verified. Flagged as a downgrade.
    Require,
    /// Certificate chain verified, host name not verified. Flagged as a downgrade.
    VerifyCa,
    /// Certificate chain and host name verified (default, I5).
    #[default]
    VerifyFull,
}

impl TlsMode {
    /// `true` for every mode below [`TlsMode::VerifyFull`].
    #[must_use]
    pub fn is_downgrade(self) -> bool {
        self != Self::VerifyFull
    }
}

/// Client certificate for mutual TLS (ADR-0011 §3). Build with [`ClientCertificate::new`].
#[derive(Debug)]
#[non_exhaustive]
pub struct ClientCertificate {
    /// PEM certificate (chain) file.
    pub cert_file: PathBuf,
    /// PEM private key file.
    pub key_file: PathBuf,
    /// Passphrase of the key, if encrypted; resolved from the secret store.
    pub key_passphrase: Option<Secret>,
}

impl Endpoint {
    /// A network server.
    #[must_use]
    pub fn tcp(host: impl Into<String>, port: u16) -> Self {
        Self::Tcp {
            host: host.into(),
            port,
        }
    }

    /// An existing database file (not created when missing).
    #[must_use]
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File {
            path: path.into(),
            create_if_missing: false,
        }
    }
}

/// Kind of [`Auth`], without the credentials: what
/// [`ConnectCapabilities::auth`](crate::ConnectCapabilities::auth) lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum AuthKind {
    /// [`Auth::None`].
    None,
    /// [`Auth::Password`] (with or without a password).
    Password,
    /// [`Auth::Token`].
    Token,
}

/// Which password exchanges a driver may use for one connection. Defaults to
/// [`PasswordExchange::Any`].
///
/// A driver that cannot enforce a policy other than `Any` does not list it in
/// [`ConnectCapabilities::password_exchanges`](crate::ConnectCapabilities::password_exchanges)
/// and refuses it at connect ([`ConnectCapabilities::check_connect_params`](crate::ConnectCapabilities::check_connect_params)),
/// so the core refuses it when the connection is saved (I8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum PasswordExchange {
    /// Every exchange the driver supports, under the driver's own transport rules (default).
    #[default]
    Any,
    /// Only a SCRAM exchange in which the server proves it knows the password verifier
    /// (PostgreSQL `SCRAM-SHA-256`, as libpq's `require_auth=scram-sha-256`): md5, cleartext and
    /// a server that skips authentication are refused, whatever the transport.
    ///
    /// This stops **method downgrades** only. Without channel binding (the client sends the
    /// GS2 header `n,,`; `SCRAM-SHA-256-PLUS` is not implemented), a man in the middle that
    /// relays the exchange to the real server still passes when TLS is `require` or `disable`.
    /// Only TLS `verify-full` (or `verify-ca` with a private CA) protects against a man in
    /// the middle; the UI should flag `ScramOnly` combined with TLS below `verify-ca`.
    ScramOnly,
}

impl Auth {
    /// The kind of authentication, without the credentials.
    #[must_use]
    pub fn kind(&self) -> AuthKind {
        match self {
            Self::None => AuthKind::None,
            Self::Password { .. } => AuthKind::Password,
            Self::Token { .. } => AuthKind::Token,
        }
    }

    /// User name and password (`None` = explicitly empty password).
    #[must_use]
    pub fn password(user: impl Into<String>, password: Option<Secret>) -> Self {
        Self::Password {
            user: user.into(),
            password,
        }
    }

    /// Bearer / API token, with an optional user name.
    #[must_use]
    pub fn token(user: Option<String>, token: Secret) -> Self {
        Self::Token { user, token }
    }
}

impl ClientCertificate {
    /// A client certificate whose key is not encrypted.
    #[must_use]
    pub fn new(cert_file: impl Into<PathBuf>, key_file: impl Into<PathBuf>) -> Self {
        Self {
            cert_file: cert_file.into(),
            key_file: key_file.into(),
            key_passphrase: None,
        }
    }
}

/// TLS settings for one connection. Drivers implement them with rustls only (ADR-0002).
///
/// Non-exhaustive: start from `TlsConfig::default()` (`verify-full`) and set fields.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct TlsConfig {
    /// Verification mode; default [`TlsMode::VerifyFull`].
    pub mode: TlsMode,
    /// CA bundle (PEM) of the server's certificate authority (ADR-0019, libpq `sslrootcert`
    /// semantics):
    /// - when set, it is the **only** trust anchor: the platform trust store is not added;
    /// - [`TlsMode::VerifyCa`] **requires** it: without one the connect fails with a TLS error
    ///   (no fallback to the platform store);
    /// - [`TlsMode::VerifyFull`] without one uses the platform trust store;
    /// - an unreadable file, or one without a valid PEM certificate, is a TLS error (fails
    ///   closed, never ignored);
    /// - [`TlsMode::Require`] and [`TlsMode::Disable`] do not use it.
    pub ca_file: Option<PathBuf>,
    /// Client certificate for mTLS.
    pub client_certificate: Option<ClientCertificate>,
    /// Host name to verify instead of the endpoint host. Set by the core when the endpoint is an
    /// SSH tunnel's local end, so `verify-full` still checks the real server name.
    pub server_name: Option<String>,
}

/// SSH host key policy (ADR-0011 §6). Defaults to [`HostKeyVerification::Strict`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HostKeyVerification {
    /// Check `known_hosts`; unknown keys need user confirmation; changed keys block (default).
    #[default]
    Strict,
    /// Unknown hosts are recorded without asking (trust on first use); known keys are still
    /// verified and changed or revoked keys block (ADR-0016 §2). An explicit, per-connection,
    /// per-hop, visibly flagged choice (I5).
    AcceptNew,
    /// Host key not checked: an explicit, per-connection, visibly flagged choice (I5).
    Disabled,
}

impl HostKeyVerification {
    /// Ordering by strength: lower is weaker (`Disabled` < `AcceptNew` < `Strict`).
    fn strength(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::AcceptNew => 1,
            Self::Strict => 2,
        }
    }

    /// The weaker of two policies.
    #[must_use]
    pub fn weakest(self, other: Self) -> Self {
        if other.strength() < self.strength() {
            other
        } else {
            self
        }
    }
}

/// One SSH hop of a tunnel (a jump host or the last hop) with the host key policy applied to
/// it (ADR-0011 §5–6). Build with [`SshHop::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshHop {
    /// Host name of the SSH server.
    pub host: String,
    /// Port of the SSH server.
    pub port: u16,
    /// Host key policy applied when connecting to this hop.
    pub host_key: HostKeyVerification,
}

impl SshHop {
    /// A hop with the given host key policy.
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16, host_key: HostKeyVerification) -> Self {
        Self {
            host: host.into(),
            port,
            host_key,
        }
    }
}

/// Description of the SSH tunnel the core opened before calling the driver.
///
/// The driver does not open tunnels (`ninesql-ssh` does). This is informational: it lets the
/// session report the full route and lets [`ConnectParams::security_warnings`] flag a disabled
/// host key check on **any** hop (L1 of the P0-B security review).
///
/// Build with [`TunnelInfo::new`] for a direct tunnel or [`TunnelInfo::with_hops`] for a
/// ProxyJump chain; `with_hops` keeps the summary fields consistent with the hop list.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TunnelInfo {
    /// SSH server (last hop) host name.
    pub ssh_host: String,
    /// SSH server port.
    pub ssh_port: u16,
    /// Number of jump hosts before the last hop.
    pub jump_hosts: u8,
    /// The **weakest** host key policy applied to any hop of the tunnel (jump hosts included):
    /// `Strict` only when every hop verified its host key.
    pub host_key: HostKeyVerification,
    /// Every hop in connection order, jump hosts first and the last hop (`ssh_host`) last, each
    /// with its own policy. Empty when the core reports only the summary fields.
    pub hops: Vec<SshHop>,
    /// Proxy used to reach the first SSH hop, if any (ADR-0011 §8).
    pub proxy: Option<ProxyInfo>,
}

impl TunnelInfo {
    /// A direct tunnel (no jump host, no proxy) with strict host key checking.
    #[must_use]
    pub fn new(ssh_host: impl Into<String>, ssh_port: u16) -> Self {
        Self {
            ssh_host: ssh_host.into(),
            ssh_port,
            jump_hosts: 0,
            host_key: HostKeyVerification::Strict,
            hops: Vec::new(),
            proxy: None,
        }
    }

    /// Sets the hop list (jump hosts first, last hop last) and derives the summary fields from
    /// it: `ssh_host` / `ssh_port` from the last hop, `jump_hosts` from the count, `host_key`
    /// as the weakest policy across hops. An empty list leaves the tunnel unchanged.
    #[must_use]
    pub fn with_hops(mut self, hops: Vec<SshHop>) -> Self {
        let Some(last) = hops.last() else {
            return self;
        };
        self.ssh_host.clone_from(&last.host);
        self.ssh_port = last.port;
        self.jump_hosts = u8::try_from(hops.len() - 1).unwrap_or(u8::MAX);
        self.host_key = hops
            .iter()
            .fold(HostKeyVerification::Strict, |w, h| w.weakest(h.host_key));
        self.hops = hops;
        self
    }

    /// The weakest host key policy across `host_key` and every entry of `hops`. Used by
    /// [`ConnectParams::security_warnings`], so a hop list that disagrees with the summary
    /// still fails safe.
    #[must_use]
    pub fn weakest_host_key(&self) -> HostKeyVerification {
        self.hops
            .iter()
            .fold(self.host_key, |w, h| w.weakest(h.host_key))
    }
}

/// Kind of proxy (ADR-0011 §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProxyKind {
    /// HTTP `CONNECT`.
    HttpConnect,
    /// SOCKS5.
    Socks5,
}

/// Description of a proxy hop opened by the core (ADR-0011 §8). Informational for drivers,
/// like [`TunnelInfo`]: the driver receives an already proxied endpoint. Proxy credentials are
/// resolved and used by the component that opens the proxy, never passed here.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProxyInfo {
    /// Proxy kind.
    pub kind: ProxyKind,
    /// Proxy host.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// User name sent to the proxy, if it authenticates.
    pub user: Option<String>,
}

impl ProxyInfo {
    /// A proxy without authentication.
    #[must_use]
    pub fn new(kind: ProxyKind, host: impl Into<String>, port: u16) -> Self {
        Self {
            kind,
            host: host.into(),
            port,
            user: None,
        }
    }
}

/// A security downgrade the UI must show on the connection and its tabs (I5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SecurityWarning {
    /// TLS is below `verify-full`.
    TlsDowngraded(TlsMode),
    /// TLS is disabled although credentials are sent over the network.
    CredentialsInClear,
    /// An SSH hop does not verify its host key (any hop of the tunnel).
    SshHostKeyNotVerified,
    /// The hop at this index of [`TunnelInfo::hops`] (0 = first hop) does not verify its host
    /// key. Reported in addition to [`SecurityWarning::SshHostKeyNotVerified`] so the UI can
    /// name the hop.
    SshHopHostKeyNotVerified {
        /// Index in [`TunnelInfo::hops`].
        hop: u8,
    },
    /// The weakest SSH hop accepts unknown host keys without asking
    /// ([`HostKeyVerification::AcceptNew`], ADR-0016 §2); no hop has verification disabled.
    SshHostKeyAcceptNew,
    /// The hop at this index of [`TunnelInfo::hops`] uses
    /// [`HostKeyVerification::AcceptNew`]. Reported in addition to
    /// [`SecurityWarning::SshHostKeyAcceptNew`] or [`SecurityWarning::SshHostKeyNotVerified`].
    SshHopHostKeyAcceptNew {
        /// Index in [`TunnelInfo::hops`].
        hop: u8,
    },
}

/// Value of a driver property. `Debug` never shows the value.
#[non_exhaustive]
pub enum PropertyValue {
    /// A non-secret value (still never logged: properties are free-form).
    Plain(String),
    /// A secret value resolved from the secret store (I1).
    Secret(Secret),
}

impl PropertyValue {
    /// Reads the value (plain or secret). Never format, log or store the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        match self {
            Self::Plain(s) => s,
            Self::Secret(s) => s.expose(),
        }
    }

    /// `true` for [`PropertyValue::Secret`].
    #[must_use]
    pub fn is_secret(&self) -> bool {
        matches!(self, Self::Secret(_))
    }
}

impl fmt::Debug for PropertyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(_) => f.write_str("PropertyValue::Plain(<redacted>)"),
            Self::Secret(_) => f.write_str("PropertyValue::Secret(<redacted>)"),
        }
    }
}

/// Normalized property keys (lower-case, ASCII alphanumerics only) that are always reserved.
///
/// Notes on some entries:
/// - `allowpublickeyretrieval` (MySQL): lets the server send its RSA key unauthenticated, so a
///   man in the middle on a non-TLS connection can obtain the password;
/// - MongoDB `authSource` / `authMechanism` are reserved through the `auth` fragment until typed
///   fields exist (fails closed);
/// - `replication` (PostgreSQL): switches the backend to a walsender, which accepts replication
///   commands instead of ordinary SQL and so bypasses the session's read-only transaction;
/// - `gssencmode` (PostgreSQL): GSSAPI encryption is negotiated before, and instead of, TLS, so
///   it could replace the verified TLS channel chosen in the connection's TLS settings.
pub const RESERVED_PROPERTY_KEYS: &[&str] = &[
    "uri",
    "nodes",
    "contactpoints",
    "connectionstring",
    "userid",
    "host",
    "hostaddr",
    "hostname",
    "port",
    "server",
    "servername",
    "user",
    "username",
    "uid",
    "pwd",
    "passfile",
    "service",
    "options",
    "socket",
    "unixsocket",
    "socketfactory",
    "encrypt",
    "sessionvariables",
    "sqlmode",
    "standardconformingstrings",
    "allowpublickeyretrieval",
    "allowloadlocalinfile",
    "allowlocalinfile",
    "localinfile",
    "replication",
    "gssencmode",
];

/// Fragments that make a normalized property key reserved wherever they appear: TLS and
/// certificate verification, credentials, access mode, multi-statements, init SQL.
pub const RESERVED_PROPERTY_FRAGMENTS: &[&str] = &[
    "encrypt",
    "secure",
    "crypt",
    "keystore",
    "channelbinding",
    "proxy",
    "socks",
    "host",
    "url",
    "reconnect",
    "failover",
    "ssl",
    "tls",
    "cert",
    "trust",
    "verif",
    "insecure",
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "auth",
    "credential",
    "readonly",
    "readwrite",
    "accessmode",
    "multistatement",
    "multiquer",
    "initsql",
    "initcommand",
    "initscript",
    "initstatement",
];

/// Normalization used by the reserved-key rules: ASCII letters lower-cased, digits kept, every
/// other character dropped.
pub(crate) fn normalize_property_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Longest property key echoed in an error message, in characters.
const MAX_ECHOED_KEY_CHARS: usize = 64;

/// A property key as error messages show it: control and other non-printable characters
/// escaped (`\u{…}`), cut to [`MAX_ECHOED_KEY_CHARS`] characters with `…`, so a hostile or
/// misplaced key cannot forge log lines or flood the UI.
#[must_use]
pub(crate) fn echo_key(key: &str) -> String {
    let mut out = String::new();
    for (n, c) in key.chars().enumerate() {
        if n == MAX_ECHOED_KEY_CHARS {
            out.push('…');
            break;
        }
        if c == ' ' || c.is_ascii_graphic() || (!c.is_ascii() && c.is_alphanumeric()) {
            out.push(c);
        } else {
            out.extend(c.escape_unicode());
        }
    }
    out
}

/// `true` when `key` controls a security-relevant setting that only the typed
/// [`ConnectParams`] fields may set (H3 / N3 of the P0-B security review).
///
/// This is the **canonical** reserved-property rule: drivers apply it through
/// [`check_properties`], and `ninesql-core` (connection dialog, DBeaver import) must reuse this
/// function rather than keep its own list, so the UI and the drivers always agree. A driver's
/// extra keys are in [`Capabilities::extra_reserved_properties`](crate::Capabilities::extra_reserved_properties);
/// [`Capabilities::is_reserved_property`](crate::Capabilities::is_reserved_property) applies
/// both.
///
/// Matching is on the normalized key (ASCII letters lower-cased, digits kept, everything else
/// dropped), so separators and case never hide a reserved key. Keys the normalization cannot
/// judge are reserved too (fails closed): keys holding a non-ASCII or control character
/// (homoglyphs, zero-width or full-width characters that a client might fold into a reserved
/// key) and keys with no ASCII letter or digit at all.
#[must_use]
pub fn is_reserved_property(key: &str) -> bool {
    if !key.chars().all(|c| c == ' ' || c.is_ascii_graphic()) {
        return true;
    }
    let k = normalize_property_key(key);
    k.is_empty()
        || RESERVED_PROPERTY_KEYS.contains(&k.as_str())
        || RESERVED_PROPERTY_FRAGMENTS.iter().any(|f| k.contains(f))
}

/// Rejects properties that would override TLS / verification, host / port, credentials,
/// read-only / access mode, multi-statements, init SQL or lexical modes. `extra_reserved` adds
/// engine-specific keys (compared after the same normalization).
///
/// Drivers pass accepted properties to the client library as separate typed options; they
/// **never splice property values into a connection string or URL**, where a value could inject
/// further keys (`x;sslmode=disable`, `a&useSSL=false`).
///
/// Every driver calls it at the start of [`Driver::connect`](crate::Driver::connect) and reuses
/// the conformance test of the `mock` feature (`conformance::assert_reserved_properties_rejected`).
///
/// # Errors
/// [`DriverError::InvalidParams`] naming the first reserved key (never its value).
pub fn check_properties(
    properties: &BTreeMap<String, PropertyValue>,
    extra_reserved: &[&str],
) -> DriverResult<()> {
    let extra: Vec<String> = extra_reserved
        .iter()
        .map(|k| normalize_property_key(k))
        .collect();
    for key in properties.keys() {
        if is_reserved_property(key) || extra.contains(&normalize_property_key(key)) {
            return Err(DriverError::InvalidParams(format!(
                "property `{}` cannot be set as a driver property; use the connection's \
                 TLS, authentication, read-only or endpoint settings",
                echo_key(key)
            )));
        }
    }
    Ok(())
}

/// Fully resolved parameters for one session.
///
/// Built by `ninesql-core`, moved into [`Driver::connect`](crate::Driver::connect). The `Debug`
/// output never contains secrets, and only property keys (not values), since DBeaver-style
/// driver properties sometimes carry credentials.
///
/// Non-exhaustive: build with [`ConnectParams::new`] and set fields.
#[non_exhaustive]
pub struct ConnectParams {
    /// Engine served by the driver (one family serves several engines).
    pub engine: EngineId,
    /// Where to connect.
    pub endpoint: Endpoint,
    /// Credentials.
    pub auth: Auth,
    /// Default database / catalog / keyspace / index, if the engine has one.
    pub database: Option<String>,
    /// TLS settings (ignored by file engines).
    pub tls: TlsConfig,
    /// SSH tunnel description, if the endpoint is a tunnel.
    pub tunnel: Option<TunnelInfo>,
    /// Proxy description, if the endpoint (or the first SSH hop, see [`TunnelInfo::proxy`]) is
    /// reached through a proxy (ADR-0011 §8). Informational.
    pub proxy: Option<ProxyInfo>,
    /// Open the session read-only. The driver **must** apply the engine-side mechanism declared
    /// in [`Capabilities::read_only`](crate::Capabilities::read_only) before returning the
    /// session (I3, ADR-0007), or fail the connect.
    pub read_only: bool,
    /// Initial auto-commit mode (connection type default, ADR-0007 §5).
    pub auto_commit: bool,
    /// Timeout for TCP connect + handshake + session setup.
    pub connect_timeout: Duration,
    /// Statements run after connect (connection "bootstrap queries").
    pub init_statements: Vec<String>,
    /// Engine-specific options (driver properties). Values may be sensitive: never logged.
    ///
    /// Drivers **must** pass them through [`check_properties`] before use: properties can never
    /// override TLS / certificate verification, host / port, credentials, read-only / access
    /// mode, multi-statement support, init SQL or lexical modes, which only come from the typed
    /// fields of [`ConnectParams`].
    pub properties: BTreeMap<String, PropertyValue>,
    /// `true` for connections typed "production": the driver must never log query text, even at
    /// `trace` level (ADR-0007 §6).
    pub production: bool,
    /// The connection type allows query text in logs (`log_query_text` of the connection type,
    /// ADR-0007 §1). Drivers log statement text (at `trace` level only) when
    /// [`ConnectParams::may_log_statements`] is `true`, i.e. this flag is set **and** the
    /// connection is not [`production`](Self::production). Defaults to `true`; the core sets it
    /// from the connection type (L1 of the wave 2 integration review).
    pub log_statements: bool,
    /// Which password exchanges the driver may use (default [`PasswordExchange::Any`]). A driver
    /// refuses a policy it does not list in
    /// [`ConnectCapabilities::password_exchanges`](crate::ConnectCapabilities::password_exchanges).
    pub password_exchange: PasswordExchange,
    /// The database file of an [`Endpoint::File`] lies inside a DBeaver workspace
    /// (`DBeaverData`, I4, ADR-0006). File drivers then open it read-only whatever
    /// [`read_only`](Self::read_only) says, never create it and never create or modify a file
    /// next to it (journal, `-wal`, `-shm`); network drivers ignore the flag. Set by the core
    /// for imported connections whose file is in the DBeaver workspace.
    pub inside_dbeaver_workspace: bool,
    /// Application name reported to the server where supported (`application_name`…).
    pub application_name: String,
}

impl ConnectParams {
    /// Default connect timeout.
    pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

    /// Parameters with secure defaults: `verify-full` TLS, no tunnel, read-write, auto-commit on,
    /// non-production. Callers set the remaining fields.
    #[must_use]
    pub fn new(engine: EngineId, endpoint: Endpoint) -> Self {
        Self {
            engine,
            endpoint,
            auth: Auth::None,
            database: None,
            tls: TlsConfig::default(),
            tunnel: None,
            proxy: None,
            read_only: false,
            auto_commit: true,
            connect_timeout: Self::DEFAULT_CONNECT_TIMEOUT,
            init_statements: Vec::new(),
            properties: BTreeMap::new(),
            production: false,
            log_statements: true,
            password_exchange: PasswordExchange::Any,
            inside_dbeaver_workspace: false,
            application_name: "NineSQL".to_owned(),
        }
    }

    /// `true` when the driver may log statement text at `trace` level:
    /// [`log_statements`](Self::log_statements) and not [`production`](Self::production)
    /// (ADR-0007 §6).
    #[must_use]
    pub fn may_log_statements(&self) -> bool {
        self.log_statements && !self.production
    }

    /// Downgrades the UI must display (I5). Empty for the secure defaults.
    #[must_use]
    pub fn security_warnings(&self) -> Vec<SecurityWarning> {
        let mut out = Vec::new();
        if matches!(self.endpoint, Endpoint::Tcp { .. }) {
            if self.tls.mode.is_downgrade() {
                out.push(SecurityWarning::TlsDowngraded(self.tls.mode));
            }
            let sends_secret = !matches!(self.auth, Auth::None);
            if self.tls.mode == TlsMode::Disable && sends_secret && self.tunnel.is_none() {
                out.push(SecurityWarning::CredentialsInClear);
            }
        }
        if let Some(t) = &self.tunnel {
            match t.weakest_host_key() {
                HostKeyVerification::Strict => {}
                HostKeyVerification::AcceptNew => out.push(SecurityWarning::SshHostKeyAcceptNew),
                // `Disabled`, and any weaker policy added later: fails safe.
                _ => out.push(SecurityWarning::SshHostKeyNotVerified),
            }
            for (i, hop) in t.hops.iter().enumerate() {
                let hop_index = u8::try_from(i).unwrap_or(u8::MAX);
                match hop.host_key {
                    HostKeyVerification::Strict => {}
                    HostKeyVerification::AcceptNew => {
                        out.push(SecurityWarning::SshHopHostKeyAcceptNew { hop: hop_index });
                    }
                    _ => out.push(SecurityWarning::SshHopHostKeyNotVerified { hop: hop_index }),
                }
            }
        }
        out
    }
}

impl fmt::Debug for ConnectParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectParams")
            .field("engine", &self.engine)
            .field("endpoint", &self.endpoint)
            .field("auth", &self.auth)
            .field("database", &self.database)
            .field("tls", &self.tls)
            .field("tunnel", &self.tunnel)
            .field("proxy", &self.proxy)
            .field("read_only", &self.read_only)
            .field("auto_commit", &self.auto_commit)
            .field("connect_timeout", &self.connect_timeout)
            .field("init_statements", &self.init_statements.len())
            .field("property_keys", &self.properties.keys().collect::<Vec<_>>())
            .field("production", &self.production)
            .field("log_statements", &self.log_statements)
            .field("password_exchange", &self.password_exchange)
            .field("inside_dbeaver_workspace", &self.inside_dbeaver_workspace)
            .field("application_name", &self.application_name)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// A reserved fragment survives any separators, digits-free decoration and case.
        #[test]
        fn reserved_fragments_survive_decoration(
            fragment in prop::sample::select(RESERVED_PROPERTY_FRAGMENTS.to_vec()),
            prefix in "[a-z0-9]{0,6}",
            sep in "[-_. :/]{0,3}",
            upper in any::<bool>(),
        ) {
            let mut decorated = String::new();
            for (i, c) in fragment.chars().enumerate() {
                if i > 0 {
                    decorated.push_str(&sep);
                }
                decorated.push(if upper { c.to_ascii_uppercase() } else { c });
            }
            let key = format!("{prefix}{sep}{decorated}");
            prop_assert!(is_reserved_property(&key), "{:?}", key);
        }

        /// Any key with a non-ASCII or control character is reserved.
        #[test]
        fn non_ascii_keys_are_reserved(
            head in "[a-z0-9]{0,8}",
            odd in "[^\x20-\x7e]",
            tail in "[a-z0-9]{0,8}",
        ) {
            let key = format!("{head}{odd}{tail}");
            prop_assert!(is_reserved_property(&key), "{:?}", key);
        }

        /// Printable ASCII keys: reserved exactly when the normalized key matches a rule.
        #[test]
        fn ascii_keys_follow_the_normalized_rule(key in "[ -~]{0,24}") {
            let k = normalize_property_key(&key);
            let expected = k.is_empty()
                || RESERVED_PROPERTY_KEYS.contains(&k.as_str())
                || RESERVED_PROPERTY_FRAGMENTS.iter().any(|f| k.contains(f));
            prop_assert_eq!(is_reserved_property(&key), expected);
        }
    }

    fn tcp() -> Endpoint {
        Endpoint::Tcp {
            host: "db.example.test".to_owned(),
            port: 5432,
        }
    }

    #[test]
    fn defaults_are_secure() {
        let p = ConnectParams::new(EngineId::POSTGRESQL, tcp());
        assert_eq!(p.tls.mode, TlsMode::VerifyFull);
        assert_eq!(HostKeyVerification::default(), HostKeyVerification::Strict);
        assert!(p.security_warnings().is_empty());
        assert!(!p.read_only);
    }

    #[test]
    fn downgrades_are_reported() {
        let mut p = ConnectParams::new(EngineId::MYSQL, tcp());
        p.tls.mode = TlsMode::Disable;
        p.auth = Auth::Password {
            user: "nine".to_owned(),
            password: Some(Secret::new("pw".to_owned())),
        };
        p.tunnel = None;
        let w = p.security_warnings();
        assert!(w.contains(&SecurityWarning::TlsDowngraded(TlsMode::Disable)));
        assert!(w.contains(&SecurityWarning::CredentialsInClear));

        let mut tunnel = TunnelInfo::new("bastion.example.test", 22);
        tunnel.host_key = HostKeyVerification::Disabled;
        p.tunnel = Some(tunnel);
        let w = p.security_warnings();
        assert!(w.contains(&SecurityWarning::SshHostKeyNotVerified));
        assert!(!w.contains(&SecurityWarning::CredentialsInClear));
    }

    #[test]
    fn every_ssh_hop_is_checked() {
        use HostKeyVerification::{Disabled, Strict};
        let mut p = ConnectParams::new(EngineId::POSTGRESQL, tcp());
        // A jump host without verification, the last hop strict.
        let t = TunnelInfo::new("ignored", 1).with_hops(vec![
            SshHop::new("jump1.example.test", 22, Strict),
            SshHop::new("jump2.example.test", 2222, Disabled),
            SshHop::new("db-bastion.example.test", 22, Strict),
        ]);
        assert_eq!(t.ssh_host, "db-bastion.example.test");
        assert_eq!(t.jump_hosts, 2);
        assert_eq!(t.host_key, Disabled, "summary is the weakest hop");
        p.tunnel = Some(t);
        let w = p.security_warnings();
        assert!(w.contains(&SecurityWarning::SshHostKeyNotVerified));
        assert!(w.contains(&SecurityWarning::SshHopHostKeyNotVerified { hop: 1 }));
        assert!(!w.contains(&SecurityWarning::SshHopHostKeyNotVerified { hop: 0 }));

        // A summary that claims Strict while a hop is Disabled still warns (fails safe).
        let mut t = TunnelInfo::new("db-bastion.example.test", 22);
        t.hops = vec![
            SshHop::new("jump.example.test", 22, Disabled),
            SshHop::new("db-bastion.example.test", 22, Strict),
        ];
        assert_eq!(t.host_key, Strict);
        assert_eq!(t.weakest_host_key(), Disabled);
        p.tunnel = Some(t);
        assert!(
            p.security_warnings()
                .contains(&SecurityWarning::SshHostKeyNotVerified)
        );

        // All strict: no warning.
        p.tunnel = Some(TunnelInfo::new("x", 22).with_hops(vec![
            SshHop::new("a.example.test", 22, Strict),
            SshHop::new("b.example.test", 22, Strict),
        ]));
        assert!(p.security_warnings().is_empty());
        assert_eq!(
            TunnelInfo::new("x", 22).with_hops(Vec::new()),
            TunnelInfo::new("x", 22)
        );
    }

    /// ADR-0016: `AcceptNew` has its own warnings and ranks between `Disabled` and `Strict`.
    #[test]
    fn accept_new_is_reported_as_such() {
        use HostKeyVerification::{AcceptNew, Disabled, Strict};
        assert_eq!(Strict.weakest(AcceptNew), AcceptNew);
        assert_eq!(AcceptNew.weakest(Strict), AcceptNew);
        assert_eq!(AcceptNew.weakest(Disabled), Disabled);
        let mut p = ConnectParams::new(EngineId::POSTGRESQL, tcp());
        p.tunnel = Some(TunnelInfo::new("x", 22).with_hops(vec![
            SshHop::new("jump.example.test", 22, AcceptNew),
            SshHop::new("bastion.example.test", 22, Strict),
        ]));
        assert_eq!(p.tunnel.as_ref().unwrap().host_key, AcceptNew);
        let w = p.security_warnings();
        assert!(w.contains(&SecurityWarning::SshHostKeyAcceptNew));
        assert!(w.contains(&SecurityWarning::SshHopHostKeyAcceptNew { hop: 0 }));
        assert!(!w.contains(&SecurityWarning::SshHostKeyNotVerified));
        assert!(!w.iter().any(|x| matches!(
            x,
            SecurityWarning::SshHopHostKeyNotVerified { .. }
                | SecurityWarning::SshHopHostKeyAcceptNew { hop: 1 }
        )));

        // A disabled hop wins the summary; each hop keeps its own warning.
        p.tunnel = Some(TunnelInfo::new("x", 22).with_hops(vec![
            SshHop::new("jump.example.test", 22, AcceptNew),
            SshHop::new("bastion.example.test", 22, Disabled),
        ]));
        let w = p.security_warnings();
        assert!(w.contains(&SecurityWarning::SshHostKeyNotVerified));
        assert!(!w.contains(&SecurityWarning::SshHostKeyAcceptNew));
        assert!(w.contains(&SecurityWarning::SshHopHostKeyAcceptNew { hop: 0 }));
        assert!(w.contains(&SecurityWarning::SshHopHostKeyNotVerified { hop: 1 }));
    }

    #[test]
    fn statement_logging_follows_type_and_production() {
        let mut p = ConnectParams::new(EngineId::POSTGRESQL, tcp());
        assert!(p.log_statements && p.may_log_statements());
        assert_eq!(p.password_exchange, PasswordExchange::Any);
        assert!(!p.inside_dbeaver_workspace);
        p.log_statements = false;
        assert!(!p.may_log_statements());
        p.log_statements = true;
        p.production = true;
        assert!(!p.may_log_statements());
    }

    #[test]
    fn auth_kinds() {
        assert_eq!(Auth::None.kind(), AuthKind::None);
        assert_eq!(Auth::password("u", None).kind(), AuthKind::Password);
        assert_eq!(
            Auth::token(None, Secret::new("t".to_owned())).kind(),
            AuthKind::Token
        );
    }

    #[test]
    fn file_engines_have_no_tls_warning() {
        let mut p = ConnectParams::new(EngineId::SQLITE, Endpoint::InMemory);
        p.tls.mode = TlsMode::Disable;
        assert!(p.security_warnings().is_empty());
    }

    #[test]
    fn debug_hides_secrets_and_property_values() {
        let mut p = ConnectParams::new(EngineId::POSTGRESQL, tcp());
        p.auth = Auth::Password {
            user: "nine".to_owned(),
            password: Some(Secret::new("s3cr3t-pw".to_owned())),
        };
        p.tls.client_certificate = Some(ClientCertificate {
            cert_file: "/c.pem".into(),
            key_file: "/k.pem".into(),
            key_passphrase: Some(Secret::new("key-pass-123".to_owned())),
        });
        p.properties.insert(
            "applicationLabel".to_owned(),
            PropertyValue::Plain("prop-value-xyz".to_owned()),
        );
        p.properties.insert(
            "kerberosKey".to_owned(),
            PropertyValue::Secret(Secret::new("prop-secret-xyz".to_owned())),
        );
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("s3cr3t-pw"));
        assert!(!dbg.contains("key-pass-123"));
        assert!(!dbg.contains("prop-value-xyz"));
        assert!(!dbg.contains("prop-secret-xyz"));
        assert!(dbg.contains("applicationLabel"));
        let v = PropertyValue::Plain("abc".into());
        assert!(!format!("{v:?}").contains("abc"));
    }

    #[test]
    fn reserved_properties_are_rejected() {
        for key in [
            "sslmode",
            "useSSL",
            "verifyServerCertificate",
            "trustServerCertificate",
            "readonly",
            "options",
            "allowMultiQueries",
            "multiStatements",
            "initSql",
            "init_command",
            "sessionVariables",
            "password",
            "trustStorePassword",
            "PGPASSWORD",
            "host",
            "Port",
            "user",
            "sql_mode",
            "standard_conforming_strings",
            "default_transaction_read_only",
            "tls.insecure",
            "sslrootcert",
            "authenticationPlugins",
            "secure",
            "encrypted",
            "useEncryption",
            "keystore",
            "wire_crypt",
            "channel_binding",
            "hosts",
            "alt_hosts",
            "failoverHosts",
            "loadBalanceHosts",
            "contactPoints",
            "nodes",
            "url",
            "uri",
            "jdbcUrl",
            "connectionString",
            "proxy",
            "http_proxy",
            "socksProxyHost",
            "autoReconnect",
            "userid",
            "allowPublicKeyRetrieval",
            "authSource",
            "authMechanism",
            "replication",
            "REPLICATION",
            "gssencmode",
            "gssEncMode",
            "gss_enc_mode",
        ] {
            assert!(is_reserved_property(key), "{key}");
            let mut props = BTreeMap::new();
            props.insert(key.to_owned(), PropertyValue::Plain("x-value".into()));
            let err = check_properties(&props, &[]).unwrap_err();
            assert!(matches!(err, DriverError::InvalidParams(_)));
            assert!(!err.to_string().contains("x-value"));
        }
        let mut props = BTreeMap::new();
        props.insert(
            "connectTimeout".to_owned(),
            PropertyValue::Plain("5".into()),
        );
        props.insert(
            "application_name".to_owned(),
            PropertyValue::Plain("x".into()),
        );
        props.insert("tcpNoDelay".to_owned(), PropertyValue::Plain("true".into()));
        props.insert("fetchSize".to_owned(), PropertyValue::Plain("100".into()));
        assert!(check_properties(&props, &[]).is_ok());
        assert!(check_properties(&props, &["connect_timeout"]).is_err());
    }

    /// Integration review: the normalization keeps digits, drops separators and never lets a
    /// non-ASCII character hide a reserved key.
    #[test]
    fn property_key_normalization_edges() {
        for key in [
            "ssl2mode",
            "SSL_MODE_2",
            "use-ssl-v2",
            "tls13",
            "init_sql_1",
            "x509cert",
            "ssl\u{200b}mode",
            "s\u{0455}lmode",
            "ｓｓｌｍｏｄｅ",
            "ssl\u{0}mode",
            "readonly\t",
            "\u{feff}user",
            "pässword",
            "_",
            "--",
            "",
            "日本語",
        ] {
            assert!(is_reserved_property(key), "{key:?}");
        }
        for key in [
            "fetchSize",
            "connect_timeout_2",
            "tcp.keepalive",
            "Application Name",
            "x1",
        ] {
            assert!(!is_reserved_property(key), "{key:?}");
        }
    }

    #[test]
    fn echoed_keys_are_escaped_and_cut() {
        assert_eq!(echo_key("search_path"), "search_path");
        assert_eq!(echo_key("a\nb"), "a\\u{a}b");
        assert_eq!(echo_key("a\u{1b}[2J"), "a\\u{1b}[2J");
        assert_eq!(echo_key("x\u{200b}y"), "x\\u{200b}y");
        let long = "k".repeat(200);
        let shown = echo_key(&long);
        assert_eq!(shown.chars().count(), MAX_ECHOED_KEY_CHARS + 1);
        assert!(shown.ends_with('…'));
        let mut props = BTreeMap::new();
        props.insert(format!("ssl\n{long}"), PropertyValue::Plain("v".into()));
        let msg = check_properties(&props, &[]).unwrap_err().to_string();
        assert!(!msg.contains('\n') && msg.len() < 300, "{msg}");
    }

    #[test]
    fn engine_id_roundtrip() {
        assert_eq!(EngineId::new("postgresql"), EngineId::POSTGRESQL);
        assert_eq!(EngineId::QUESTDB.to_string(), "questdb");
    }
}
