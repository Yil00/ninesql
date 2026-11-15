//! Connection URLs (F-CON-12): `postgresql://user@host:5432/db?sslmode=verify-full`,
//! `mysql://…`, `mariadb://…`, `sqlite:///path/to/file.db`, with an optional `jdbc:` prefix.
//!
//! A URL is only a way to fill the dialog: it is parsed into fields and never stored. A URL
//! that carries a password (`user:secret@host`) is refused, and the error never repeats it (I1):
//! the password goes into the password field, which saves it to the keyring or the vault.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ninesql_core::TlsMode;
use ninesql_core::driver_api::echo_property_key;

/// Fragments of a normalized query key that name a credential: the credential fragments of the
/// driver API's reserved-property rule (`RESERVED_PROPERTY_FRAGMENTS`) plus the short forms
/// `pass` and `pwd`. Such a parameter is refused outright instead of becoming a driver property,
/// where its value would sit in a plain text field until validation refused it.
const CREDENTIAL_FRAGMENTS: &[&str] = &[
    "password",
    "passwd",
    "pass",
    "pwd",
    "secret",
    "token",
    "apikey",
    "auth",
    "credential",
];

/// Whether a query key may carry a credential. Fails closed: a key the normalization cannot
/// judge (any character other than printable ASCII) counts as one.
fn is_credential_key(key: &str) -> bool {
    if key.chars().any(|c| !c.is_ascii_graphic()) {
        return true;
    }
    let norm: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    CREDENTIAL_FRAGMENTS.iter().any(|f| norm.contains(f))
}

/// What a URL filled in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedUrl {
    /// Engine id from the scheme.
    pub engine: String,
    /// Host (IPv6 without brackets), for server engines.
    pub host: Option<String>,
    /// Port, when given.
    pub port: Option<u16>,
    /// Database, when given.
    pub database: Option<String>,
    /// User name, when given.
    pub username: Option<String>,
    /// File path, for file engines.
    pub file: Option<PathBuf>,
    /// TLS mode from `sslmode` / `ssl-mode`, when given.
    pub tls_mode: Option<TlsMode>,
    /// CA file from `sslrootcert` / `ssl-ca`, when given.
    pub ca_file: Option<PathBuf>,
    /// Other query parameters, as driver properties (checked against the driver's allowlist
    /// when the connection is validated).
    pub properties: BTreeMap<String, String>,
}

/// Why a URL was refused. Messages never contain the password.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    /// No `scheme://`.
    #[error("not a connection URL: expected scheme://host… (for example postgresql://host/db)")]
    NoScheme,
    /// Scheme of an engine NineSQL does not know (echoed bounded and escaped).
    #[error("unknown URL scheme `{0}`")]
    UnknownScheme(String),
    /// The URL carries a password.
    #[error(
        "the URL contains a password: remove it from the URL and type it in the password field, which saves it to the keyring"
    )]
    ContainsPassword,
    /// Host missing or malformed.
    #[error("the URL has no valid host")]
    BadHost,
    /// Port not a number in 1..=65535.
    #[error("the URL port must be a number between 1 and 65535")]
    BadPort,
    /// Bad percent-encoding.
    #[error("the URL has an invalid %-escape")]
    BadEscape,
    /// A TLS mode with no NineSQL equivalent (echoed bounded and escaped).
    #[error("the TLS mode `{0}` is not supported: use disable, require, verify-ca or verify-full")]
    UnsupportedTlsMode(String),
    /// A query parameter that carries a credential (its key, bounded and escaped; never the
    /// value).
    #[error("the URL parameter `{0}` carries a credential; type it in the password field instead")]
    CredentialParameter(String),
    /// No file path.
    #[error("the URL has no database file path")]
    NoFile,
}

fn engine_of(scheme: &str) -> Option<&'static str> {
    Some(match scheme {
        "postgresql" | "postgres" | "pgsql" => "postgresql",
        "mysql" => "mysql",
        "mariadb" => "mariadb",
        "sqlite" | "sqlite3" => "sqlite",
        _ => return None,
    })
}

fn percent_decode(s: &str) -> Result<String, UrlError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3).ok_or(UrlError::BadEscape)?;
                let hex = std::str::from_utf8(hex).map_err(|_| UrlError::BadEscape)?;
                let v = u8::from_str_radix(hex, 16).map_err(|_| UrlError::BadEscape)?;
                out.push(v);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| UrlError::BadEscape)
}

/// Percent-encodes what [`parse`] decodes in user names and database names.
#[must_use]
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn tls_mode(value: &str) -> Result<TlsMode, UrlError> {
    Ok(
        match value.to_ascii_lowercase().replace('_', "-").as_str() {
            "disable" | "disabled" => TlsMode::Disable,
            "require" | "required" => TlsMode::Require,
            "verify-ca" => TlsMode::VerifyCa,
            "verify-full" | "verify-identity" => TlsMode::VerifyFull,
            other => return Err(UrlError::UnsupportedTlsMode(echo_property_key(other))),
        },
    )
}

/// Parses a connection URL.
///
/// # Errors
/// [`UrlError`]; a URL with a password is always refused with [`UrlError::ContainsPassword`].
pub fn parse(text: &str) -> Result<ParsedUrl, UrlError> {
    let text = text.trim();
    let text = text.strip_prefix("jdbc:").unwrap_or(text);
    let (scheme, rest) = text.split_once(':').ok_or(UrlError::NoScheme)?;
    let scheme = scheme.to_ascii_lowercase();
    let engine =
        engine_of(&scheme).ok_or_else(|| UrlError::UnknownScheme(echo_property_key(&scheme)))?;
    let mut parsed = ParsedUrl {
        engine: engine.to_owned(),
        ..ParsedUrl::default()
    };
    if engine == "sqlite" {
        // sqlite:///abs/path, sqlite://relative (rare) or sqlite:path.
        let path = rest.strip_prefix("//").unwrap_or(rest);
        let path = path.split(['?', '#']).next().unwrap_or_default();
        if path.is_empty() {
            return Err(UrlError::NoFile);
        }
        parsed.file = Some(PathBuf::from(percent_decode(path)?));
        return Ok(parsed);
    }
    let rest = rest.strip_prefix("//").ok_or(UrlError::NoScheme)?;
    // Checked on the whole remainder, before splitting on `/`, `?` or `#`: a password holding
    // one of them must not leak, even partly, into the database or a property. A raw `@` after
    // a `:` anywhere (for example in a query value) is refused too; encode it as `%40`.
    if let Some(at) = rest.rfind('@')
        && rest[..at].contains(':')
    {
        return Err(UrlError::ContainsPassword);
    }
    let rest = rest.split('#').next().unwrap_or_default();
    let (before_query, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };
    let (authority, path) = match before_query.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (before_query, None),
    };
    // User info ends at the last `@` (a user name may hold an encoded `@`, never a raw one).
    let host_port = match authority.rsplit_once('@') {
        Some((userinfo, host_port)) => {
            if userinfo.contains(':') {
                return Err(UrlError::ContainsPassword);
            }
            let user = percent_decode(userinfo)?;
            if !user.is_empty() {
                parsed.username = Some(user);
            }
            host_port
        }
        None => authority,
    };
    let (host, port) = if let Some(v6) = host_port.strip_prefix('[') {
        let (host, after) = v6.split_once(']').ok_or(UrlError::BadHost)?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or(UrlError::BadHost)?),
        };
        (host, port)
    } else {
        match host_port.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        }
    };
    if host.is_empty()
        || host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | '/' | '\\'))
    {
        return Err(UrlError::BadHost);
    }
    parsed.host = Some(host.to_owned());
    if let Some(p) = port {
        let p: u16 = p.parse().map_err(|_| UrlError::BadPort)?;
        if p == 0 {
            return Err(UrlError::BadPort);
        }
        parsed.port = Some(p);
    }
    if let Some(db) = path {
        let db = percent_decode(db)?;
        if !db.is_empty() {
            parsed.database = Some(db);
        }
    }
    for pair in query.into_iter().flat_map(|q| q.split('&')) {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(k)?;
        let value = percent_decode(v)?;
        if is_credential_key(&key) {
            return Err(UrlError::CredentialParameter(echo_property_key(&key)));
        }
        let norm = key.to_ascii_lowercase().replace('_', "-");
        match norm.as_str() {
            "sslmode" | "ssl-mode" => parsed.tls_mode = Some(tls_mode(&value)?),
            "sslrootcert" | "ssl-ca" => parsed.ca_file = Some(PathBuf::from(value)),
            "user" | "username" => parsed.username = Some(value),
            _ => {
                parsed.properties.insert(key, value);
            }
        }
    }
    Ok(parsed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn postgres_url_with_everything() {
        let p = parse(
            "postgresql://app%40corp@db.example.test:6543/sales%20db?sslmode=verify-ca&sslrootcert=/etc/ca.pem&application_name=nine",
        )
        .unwrap();
        assert_eq!(p.engine, "postgresql");
        assert_eq!(p.username.as_deref(), Some("app@corp"));
        assert_eq!(p.host.as_deref(), Some("db.example.test"));
        assert_eq!(p.port, Some(6543));
        assert_eq!(p.database.as_deref(), Some("sales db"));
        assert_eq!(p.tls_mode, Some(TlsMode::VerifyCa));
        assert_eq!(p.ca_file, Some(PathBuf::from("/etc/ca.pem")));
        assert_eq!(
            p.properties.get("application_name").map(String::as_str),
            Some("nine")
        );
    }

    #[test]
    fn jdbc_mysql_ipv6_and_mysql_tls_names() {
        let p = parse("jdbc:mysql://[::1]:3307/shop?ssl-mode=VERIFY_IDENTITY").unwrap();
        assert_eq!(p.engine, "mysql");
        assert_eq!(p.host.as_deref(), Some("::1"));
        assert_eq!(p.port, Some(3307));
        assert_eq!(p.tls_mode, Some(TlsMode::VerifyFull));
        let p = parse("mariadb://h").unwrap();
        assert_eq!(
            (p.engine.as_str(), p.port, p.database),
            ("mariadb", None, None)
        );
    }

    #[test]
    fn sqlite_urls() {
        let p = parse("sqlite:///var/lib/nine.db").unwrap();
        assert_eq!(p.file, Some(PathBuf::from("/var/lib/nine.db")));
        assert_eq!(
            parse("sqlite:nine.db").unwrap().file,
            Some(PathBuf::from("nine.db"))
        );
        assert_eq!(parse("sqlite://"), Err(UrlError::NoFile));
    }

    #[test]
    fn passwords_are_refused_and_never_echoed() {
        for url in [
            "postgresql://app:hunter2@db/x",
            "mysql://:hunter2@db",
            "postgres://u:hunter2@[::1]:5432",
            "postgresql://u:hun/ter2@db/x",
            "postgresql://u:hun?ter2@db/x",
            "postgresql://u:hun#ter2@db/x",
        ] {
            let err = parse(url).unwrap_err();
            assert_eq!(err, UrlError::ContainsPassword, "{url}");
            assert!(!err.to_string().contains("hun"));
        }
        let err = parse("postgresql://db/x?password=hunter2").unwrap_err();
        assert!(matches!(err, UrlError::CredentialParameter(_)));
        assert!(!err.to_string().contains("hunter2"));
    }

    #[test]
    fn every_credential_like_parameter_is_refused() {
        for key in [
            "password",
            "sslpassword",
            "PGPASSWORD",
            "db_passwd",
            "pass",
            "pwd",
            "api_key",
            "apiKey",
            "access-token",
            "client_secret",
            "authSource",
            "credentials",
            "pass\u{200b}word",
        ] {
            let url = format!("postgresql://h/db?{}=s3cr3t", percent_encode(key));
            assert!(
                matches!(parse(&url), Err(UrlError::CredentialParameter(_))),
                "{key}"
            );
            let msg = parse(&url).unwrap_err().to_string();
            assert!(!msg.contains("s3cr3t"), "{msg}");
        }
        // Ordinary parameters still become properties.
        let p = parse("postgresql://h/db?application_name=x&connect_timeout=5").unwrap();
        assert_eq!(p.properties.len(), 2);
    }

    #[test]
    fn echoed_text_is_bounded_and_escaped() {
        let long = "x".repeat(500);
        let msg = parse(&format!("{long}://h")).unwrap_err().to_string();
        assert!(msg.len() < 200, "{msg}");
        let msg = parse("postgresql://h/db?sslmode=evil%0Aforged%20line")
            .unwrap_err()
            .to_string();
        assert!(!msg.contains('\n'), "{msg}");
        let key = format!("{}token", "k".repeat(300));
        let msg = parse(&format!("postgresql://h/db?{key}=v"))
            .unwrap_err()
            .to_string();
        assert!(msg.len() < 200, "{msg}");
    }

    #[test]
    fn malformed_urls() {
        assert_eq!(parse("db.example.test"), Err(UrlError::NoScheme));
        assert!(matches!(
            parse("oracle://h"),
            Err(UrlError::UnknownScheme(_))
        ));
        assert_eq!(parse("postgresql://h:0"), Err(UrlError::BadPort));
        assert_eq!(parse("postgresql://h:99999"), Err(UrlError::BadPort));
        assert_eq!(parse("postgresql://"), Err(UrlError::BadHost));
        assert_eq!(parse("postgresql://h/%zz"), Err(UrlError::BadEscape));
        assert!(matches!(
            parse("postgresql://h?sslmode=prefer"),
            Err(UrlError::UnsupportedTlsMode(_))
        ));
    }

    proptest! {
        #[test]
        fn never_panics(text in ".{0,200}") {
            let _ = parse(&text);
        }

        #[test]
        fn round_trips_structured_fields(
            user in "[^\\x00-\\x1f]{1,20}",
            host in "[a-z][a-z0-9.-]{0,30}",
            port in 1u16..,
            db in "[^\\x00-\\x1f]{1,20}",
        ) {
            let url = format!(
                "postgresql://{}@{host}:{port}/{}",
                percent_encode(&user),
                percent_encode(&db)
            );
            let p = parse(&url).unwrap();
            prop_assert_eq!(p.username.as_deref(), Some(user.as_str()));
            prop_assert_eq!(p.host.as_deref(), Some(host.as_str()));
            prop_assert_eq!(p.port, Some(port));
            prop_assert_eq!(p.database.as_deref(), Some(db.as_str()));
        }

        #[test]
        fn any_password_is_refused_without_echo(
            user in "[a-z]{0,8}",
            secret in "[A-Za-z0-9]{6,20}",
        ) {
            let url = format!("mysql://{user}:{secret}@db.example.test/x");
            let err = parse(&url).unwrap_err();
            prop_assert_eq!(&err, &UrlError::ContainsPassword);
            prop_assert!(!err.to_string().contains(&secret));
        }
    }
}
