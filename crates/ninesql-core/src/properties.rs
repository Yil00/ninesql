//! Rules for free-form driver properties and init SQL (I1, I3, I5).
//!
//! Driver properties must not carry what the typed model already owns: TLS settings (they would
//! bypass `TlsConfig` and its warnings, I5), the endpoint, credentials (I1) or the read-only mode
//! (I3). They must not carry a secret value either. The denylist below is shared by validation
//! and load-time sanitizing. **The P0-B driver boundary (`ninesql-driver-api`) must mirror this
//! denylist and allowlist** for TLS, host, port, credential and read-only keys.

/// What a reserved property key collides with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reserved {
    /// TLS settings: use `tls` (`TlsConfig`).
    Tls,
    /// Host / port: use `endpoint`.
    Endpoint,
    /// User name: use `auth`.
    Credentials,
    /// Read-only mode: use the connection type or `read_only`.
    ReadOnly,
}

impl Reserved {
    /// The model field to use instead.
    #[must_use]
    pub const fn model_field(self) -> &'static str {
        match self {
            Self::Tls => "tls",
            Self::Endpoint => "endpoint",
            Self::Credentials => "auth",
            Self::ReadOnly => "read_only / connection type",
        }
    }
}

/// Known-benign keys (normalized) checked **before** every heuristic below and before the
/// secret-like key check, so that common driver options are never refused or dropped on load.
/// `passwordcharacterencoding` / `passwordencryption` come from
/// [`ninesql_storage::hygiene::BENIGN_SECRET_LIKE_KEYS`].
pub const BENIGN_KEYS: &[&str] = &[
    // MySQL Connector/J: lets the driver fetch the server RSA key for password exchange on
    // non-TLS connections (DBeaver sets it on MySQL 8 connections). Not a TLS setting.
    "allowpublickeyretrieval",
    // PostgreSQL: GSSAPI encryption, independent from TLS.
    "gssencmode",
];

/// TLS keys that do not contain `ssl` or `tls` (normalized). Any key containing `ssl` or `tls`,
/// starting with `verify`, or one of these counts as TLS.
pub const TLS_KEYS: &[&str] = &[
    // ClickHouse / generic: `secure=false` means plaintext.
    "secure",
    "insecure",
    "insecuremode",
    "encrypt",
    "trustservercertificate",
    "hostnameincertificate",
    "servercertificate",
    "truststore",
    "truststorepassword",
    "truststoretype",
    "keystore",
    "keystoretype",
    "trustcertificatekeystoreurl",
    "clientcertificatekeystoreurl",
];
/// Normalized substrings that make a key a TLS setting.
pub const TLS_FRAGMENTS: &[&str] = &["ssl", "tls"];
/// Normalized prefixes that make a key a TLS setting (`verifyServerCertificate`, `verify_ssl`…).
pub const TLS_PREFIXES: &[&str] = &["verify"];
/// Normalized endpoint keys.
pub const ENDPOINT_KEYS: &[&str] = &["host", "hostaddr", "hostname", "port", "server"];
/// Normalized credential keys (password-like keys are caught as secrets).
pub const CREDENTIAL_KEYS: &[&str] = &["user", "username", "uid", "userid"];
/// Normalized read-only keys.
pub const READ_ONLY_KEYS: &[&str] = &["readonly", "defaulttransactionreadonly"];

fn normalize(key: &str) -> String {
    ninesql_storage::hygiene::normalize_key(key)
}

/// Whether a key is a known-benign driver option (see [`BENIGN_KEYS`]).
#[must_use]
pub fn is_benign(key: &str) -> bool {
    let k = normalize(key);
    BENIGN_KEYS.contains(&k.as_str())
        || ninesql_storage::hygiene::BENIGN_SECRET_LIKE_KEYS.contains(&k.as_str())
}

/// Whether a property key collides with a typed model field.
#[must_use]
pub fn reserved(key: &str) -> Option<Reserved> {
    if is_benign(key) {
        return None;
    }
    let k = normalize(key);
    if TLS_KEYS.contains(&k.as_str())
        || TLS_FRAGMENTS.iter().any(|f| k.contains(f))
        || TLS_PREFIXES.iter().any(|p| k.starts_with(p))
    {
        Some(Reserved::Tls)
    } else if ENDPOINT_KEYS.contains(&k.as_str()) {
        Some(Reserved::Endpoint)
    } else if CREDENTIAL_KEYS.contains(&k.as_str()) {
        Some(Reserved::Credentials)
    } else if READ_ONLY_KEYS.contains(&k.as_str()) {
        Some(Reserved::ReadOnly)
    } else {
        None
    }
}

/// Whether a property value embeds a credential: a URL with `user:password@` user info, or a
/// `password=` / `pwd=` / `passwd=` pair (connection-string style).
#[must_use]
pub fn value_carries_secret(value: &str) -> bool {
    let lower = value.to_lowercase();
    let mut rest = lower.as_str();
    while let Some(pos) = rest.find("://") {
        let after = &rest[pos + 3..];
        let authority = after.split(['/', '?', '#']).next().unwrap_or_default();
        if let Some(at) = authority.rfind('@')
            && authority[..at].contains(':')
        {
            return true;
        }
        rest = after;
    }
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    ["password=", "pwd=", "passwd="]
        .iter()
        .any(|p| compact.contains(p))
}

/// Whether an init SQL statement contains an obvious password literal (`IDENTIFIED BY …`,
/// `PASSWORD '…'`, `PASSWORD = "…"`, `PASSWORD $$…$$`).
#[must_use]
pub fn sql_carries_secret(sql: &str) -> bool {
    let words: Vec<String> = sql.split_whitespace().map(str::to_lowercase).collect();
    let flat = words.join(" ");
    if flat.contains("identified by") {
        return true;
    }
    let mut rest = flat.as_str();
    while let Some(pos) = rest.find("password") {
        let after = rest[pos + "password".len()..].trim_start();
        let after = after.strip_prefix('=').map_or(after, str::trim_start);
        let after = after.strip_prefix("to ").map_or(after, str::trim_start);
        if after.starts_with(['\'', '"', '$']) || after.starts_with("e'") {
            return true;
        }
        rest = &rest[pos + "password".len()..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_keys() {
        for (key, want) in [
            ("sslmode", Reserved::Tls),
            ("useSSL", Reserved::Tls),
            ("requireSSL", Reserved::Tls),
            ("verifyServerCertificate", Reserved::Tls),
            ("trustServerCertificate", Reserved::Tls),
            ("ssl", Reserved::Tls),
            ("tls", Reserved::Tls),
            ("sslrootcert", Reserved::Tls),
            ("tlsAllowInvalidCertificates", Reserved::Tls),
            ("encrypt", Reserved::Tls),
            ("host", Reserved::Endpoint),
            ("PORT", Reserved::Endpoint),
            ("user", Reserved::Credentials),
            ("readOnly", Reserved::ReadOnly),
            ("default_transaction_read_only", Reserved::ReadOnly),
        ] {
            assert_eq!(reserved(key), Some(want), "{key}");
        }
        for ok in [
            "application_name",
            "connect_timeout",
            "charset",
            "search_path",
        ] {
            assert_eq!(reserved(ok), None, "{ok}");
        }
    }

    #[test]
    fn secret_values() {
        for bad in [
            "postgres://app:Hunter2@db/x",
            "jdbc:mysql://root:pw@h:3306/db",
            "Server=h;User Id=u;Password=Hunter2;",
            "pwd = x",
            "x://h/?a=1 then mongodb://u:p@h",
        ] {
            assert!(value_carries_secret(bad), "{bad}");
        }
        for ok in [
            "postgres://db/x",
            "postgres://app@db/x",
            "10",
            "https://example.test/a:b@c",
            "password_encryption",
        ] {
            assert!(!value_carries_secret(ok), "{ok}");
        }
    }

    #[test]
    fn secret_sql() {
        for bad in [
            "ALTER USER app IDENTIFIED BY Hunter2",
            "alter role app with password 'Hunter2'",
            "CREATE USER x PASSWORD = \"Hunter2\"",
            "ALTER ROLE a PASSWORD $$x$$",
            "SET PASSWORD TO 'x'",
        ] {
            assert!(sql_carries_secret(bad), "{bad}");
        }
        for ok in [
            "SET search_path TO app",
            "SET password_encryption = 'scram-sha-256'",
            "SELECT 1",
        ] {
            assert!(!sql_carries_secret(ok), "{ok}");
        }
    }
}
