//! Rules for free-form driver properties and init SQL (I1, I3, I5).
//!
//! Driver properties must not carry what the typed model already owns: TLS settings (they would
//! bypass `TlsConfig` and its warnings, I5), the endpoint, credentials (I1), the read-only mode
//! (I3), init SQL, proxies or multi-statement / lexical modes. They must not carry a secret value
//! either.
//!
//! **Reserved keys have a single source of truth**:
//! [`ninesql_driver_api::is_reserved_property`], the rule every driver applies through
//! `check_properties` at connect time. The core reuses it for validation (connection dialog,
//! DBeaver import) and for sanitize-on-load, so it never stores a connection a driver would later
//! refuse. No benign list overrides it: a key the driver API reserves is always refused here.
//! This module only adds:
//! - a classification of reserved keys ([`Reserved`]) to tell the user which typed field to use;
//! - [`PENDING_RESERVED_KEYS`], keys the core already refuses while the driver API catches up
//!   (the core may only be *stricter* than the driver API, never looser);
//! - the secret-**value** detectors ([`value_carries_secret`], [`sql_carries_secret`]), layered on
//!   top of the key rules.
//!
//! The secret-like **key** heuristic is [`ninesql_storage::hygiene::is_secret_like_key`] (with
//! its own benign list, which only affects that heuristic).

/// What a reserved property key collides with, to tell the user which typed field to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reserved {
    /// TLS / certificate verification / transport encryption: use `tls` (`TlsConfig`).
    Tls,
    /// Host, port, URL, socket, service file or fail-over hosts: use `endpoint`.
    Endpoint,
    /// User name, password, token or authentication options: use `auth`.
    Credentials,
    /// Read-only / access mode: use the connection type or `read_only`.
    ReadOnly,
    /// Statements run at connect time: use `init_sql`.
    InitSql,
    /// HTTP / SOCKS proxy: use the SSH / proxy settings.
    Proxy,
    /// GSSAPI / Kerberos options (`gssencmode`, `gsslib`): no typed field exists yet, so they are
    /// refused (fails closed) until one is added.
    Gssapi,
    /// Anything else the driver API reserves (session variables, `options`, `sql_mode`,
    /// multi-statements, local-infile…): not settable at all.
    Other,
}

impl Reserved {
    /// What to do instead, as shown to the user after "the driver property `x` is reserved: ".
    #[must_use]
    pub const fn model_field(self) -> &'static str {
        match self {
            Self::Tls => "set it in the `tls` settings",
            Self::Endpoint => "set it in the `endpoint` settings",
            Self::Credentials => "set it in the `auth` settings (secrets go to the keyring)",
            Self::ReadOnly => "set it with the connection type or `read_only`",
            Self::InitSql => "put the statements in `init_sql`",
            Self::Proxy => "set it in the SSH / proxy settings",
            Self::Gssapi => "GSSAPI options cannot be set as driver properties yet",
            Self::Other => {
                "it controls security or session behavior and cannot be set as a driver property"
            }
        }
    }
}

/// Normalized keys the core refuses on top of [`ninesql_driver_api::is_reserved_property`],
/// until the driver API reserves them too (then the entry is redundant and should be removed).
///
/// - `gssencmode` (PostgreSQL): GSSAPI transport encryption, which would bypass the TLS settings
///   and their warnings; being added to the driver API's reserved keys.
pub const PENDING_RESERVED_KEYS: &[&str] = &["gssencmode"];

/// Normalization of the driver API (lower case, ASCII alphanumerics only).
fn normalize(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a property key is reserved: [`ninesql_driver_api::is_reserved_property`] or one of
/// [`PENDING_RESERVED_KEYS`].
#[must_use]
pub fn is_reserved(key: &str) -> bool {
    ninesql_driver_api::is_reserved_property(key)
        || PENDING_RESERVED_KEYS.contains(&normalize(key).as_str())
}

/// Whether a property key is reserved, and what it collides with (see [`is_reserved`]).
///
/// The classification only picks the hint shown to the user; whether a key is reserved is
/// decided by [`is_reserved`] alone.
#[must_use]
pub fn reserved(key: &str) -> Option<Reserved> {
    if !is_reserved(key) {
        return None;
    }
    let k = normalize(key);
    let has = |fragments: &[&str]| fragments.iter().any(|f| k.contains(f));
    let class = if k.starts_with("gss") {
        // `gsslib` contains `ssl` and is reserved by the driver API through that fragment, though
        // it is not a TLS setting: refused (fails closed) until GSSAPI has a typed field.
        Reserved::Gssapi
    } else if has(&["readonly", "readwrite", "accessmode"]) {
        Reserved::ReadOnly
    } else if has(&["initsql", "initcommand", "initscript", "initstatement"]) {
        Reserved::InitSql
    } else if has(&["proxy", "socks"]) {
        Reserved::Proxy
    } else if has(&[
        "ssl",
        "tls",
        "cert",
        "trust",
        "verif",
        "insecure",
        "encrypt",
        "secure",
        "crypt",
        "keystore",
        "channelbinding",
    ]) {
        Reserved::Tls
    } else if has(&[
        "password",
        "passwd",
        "passfile",
        "secret",
        "token",
        "apikey",
        "auth",
        "credential",
    ]) || matches!(k.as_str(), "user" | "username" | "uid" | "userid" | "pwd")
    {
        Reserved::Credentials
    } else if has(&["host", "url", "failover", "reconnect"])
        || matches!(
            k.as_str(),
            "uri"
                | "nodes"
                | "contactpoints"
                | "connectionstring"
                | "port"
                | "server"
                | "servername"
                | "service"
                | "socket"
                | "unixsocket"
                | "socketfactory"
        )
    {
        Reserved::Endpoint
    } else {
        Reserved::Other
    };
    Some(class)
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
    fn reserved_keys_are_classified() {
        for (key, want) in [
            ("sslmode", Reserved::Tls),
            ("useSSL", Reserved::Tls),
            ("verifyServerCertificate", Reserved::Tls),
            ("hostNameInCertificate", Reserved::Tls),
            ("encrypt", Reserved::Tls),
            ("host", Reserved::Endpoint),
            ("PORT", Reserved::Endpoint),
            ("jdbcUrl", Reserved::Endpoint),
            ("socket", Reserved::Endpoint),
            ("user", Reserved::Credentials),
            ("authSource", Reserved::Credentials),
            ("passwordCharacterEncoding", Reserved::Credentials),
            ("readOnly", Reserved::ReadOnly),
            ("default_transaction_read_only", Reserved::ReadOnly),
            ("init_command", Reserved::InitSql),
            ("socksProxyHost", Reserved::Proxy),
            ("gssencmode", Reserved::Gssapi),
            ("gsslib", Reserved::Gssapi),
            ("options", Reserved::Other),
            ("sql_mode", Reserved::Other),
            ("allowMultiQueries", Reserved::Other),
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

    /// The core is never looser than the driver API, and only stricter by
    /// [`PENDING_RESERVED_KEYS`].
    #[test]
    fn driver_api_rule_is_the_source_of_truth() {
        for key in [
            "sslmode",
            "options",
            "sessionVariables",
            "passfile",
            "allowPublicKeyRetrieval",
            "gsslib",
            "application_name",
            "fetchSize",
            "gssencmode",
        ] {
            let api = ninesql_driver_api::is_reserved_property(key);
            let pending = PENDING_RESERVED_KEYS.contains(&normalize(key).as_str());
            assert_eq!(reserved(key).is_some(), api || pending, "{key}");
        }
        // `gsslib` is reserved by the driver API through the `ssl` fragment (fails closed).
        assert!(ninesql_driver_api::is_reserved_property("gsslib"));
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
