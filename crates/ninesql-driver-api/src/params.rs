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

impl Auth {
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
    /// Extra CA bundle (PEM) trusted in addition to the platform trust store.
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
    /// Host key not checked: an explicit, per-connection, visibly flagged choice (I5).
    Disabled,
}

/// Description of the SSH tunnel the core opened before calling the driver.
///
/// The driver does not open tunnels (`ninesql-ssh` does). This is informational: it lets the
/// session report the full route and lets [`ConnectParams::security_warnings`] flag a disabled
/// host key check.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TunnelInfo {
    /// SSH server (last hop) host name.
    pub ssh_host: String,
    /// SSH server port.
    pub ssh_port: u16,
    /// Number of jump hosts before the last hop.
    pub jump_hosts: u8,
    /// Host key policy that was applied when the tunnel was opened.
    pub host_key: HostKeyVerification,
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
            proxy: None,
        }
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
    /// An SSH hop does not verify its host key.
    SshHostKeyNotVerified,
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
///   fields exist (fails closed).
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

fn normalize_property_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// `true` when `key` controls a security-relevant setting that only the typed
/// [`ConnectParams`] fields may set (H3 / N3 of the P0-B security review).
///
/// This is the **canonical** reserved-property rule: drivers apply it through
/// [`check_properties`], and `ninesql-core` (connection dialog, DBeaver import) must reuse this
/// function rather than keep its own list, so the UI and the drivers always agree.
#[must_use]
pub fn is_reserved_property(key: &str) -> bool {
    let k = normalize_property_key(key);
    RESERVED_PROPERTY_KEYS.contains(&k.as_str())
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
                "property `{key}` cannot be set as a driver property; use the connection's \
                 TLS, authentication, read-only or endpoint settings"
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
            application_name: "NineSQL".to_owned(),
        }
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
        if self
            .tunnel
            .as_ref()
            .is_some_and(|t| t.host_key == HostKeyVerification::Disabled)
        {
            out.push(SecurityWarning::SshHostKeyNotVerified);
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
            .field("application_name", &self.application_name)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn engine_id_roundtrip() {
        assert_eq!(EngineId::new("postgresql"), EngineId::POSTGRESQL);
        assert_eq!(EngineId::QUESTDB.to_string(), "questdb");
    }
}
