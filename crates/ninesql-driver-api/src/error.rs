//! Typed, user-presentable driver errors.
//!
//! Every message in a [`DriverError`] may be shown in the UI and written to the log, so drivers
//! must build it from engine messages that carry no secret: pass client error text through
//! [`redact_connection_string`] (or [`redact_secret`]) when it might echo a connection string.
//! Errors never contain query parameter values or result values.

use std::time::Duration;

use crate::capabilities::Feature;
use crate::secret::Secret;

/// Result alias used across the API.
pub type DriverResult<T> = Result<T, DriverError>;

/// Error returned by drivers, sessions, metadata and result streams.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DriverError {
    /// Parameters rejected before any network activity (missing host, wrong engine id…).
    #[error("invalid connection parameters: {0}")]
    InvalidParams(String),
    /// The server could not be reached or the handshake failed.
    #[error("connection failed: {0}")]
    Connect(String),
    /// The server rejected the credentials. The message never contains the secret.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// TLS negotiation or certificate verification failed.
    #[error("TLS error: {0}")]
    Tls(String),
    /// The engine-side read-only mechanism could not be applied, or the engine refused a write in
    /// a read-only session (I3).
    #[error("read-only session: {0}")]
    ReadOnly(String),
    /// The engine reported an error for a statement.
    #[error("{message}")]
    Query {
        /// Engine error code (SQLSTATE, MySQL error number, …) when available.
        code: Option<String>,
        /// Engine message, free of parameter and result values.
        message: String,
        /// 1-based character offset in the statement, when the engine reports one.
        position: Option<u32>,
    },
    /// The statement exceeded [`ExecOptions::timeout`](crate::ExecOptions::timeout) and was
    /// cancelled.
    #[error("query timed out after {0:?}")]
    Timeout(Duration),
    /// The statement was cancelled by the user.
    #[error("query cancelled")]
    Cancelled,
    /// The driver does not offer this feature for this engine (should have been hidden, I8).
    #[error("not supported by this engine: {0:?}")]
    Unsupported(Feature),
    /// The session is still streaming a previous result.
    #[error("session busy: a result is still streaming")]
    Busy,
    /// The session is closed or the connection was lost.
    #[error("session closed: {0}")]
    Closed(String),
    /// Unexpected data from the server or a client library bug.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// A local I/O error (file engines, sockets).
    #[error("I/O error: {0}")]
    Io(String),
}

impl DriverError {
    /// `true` when the session is unusable and must be reconnected.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Closed(_) | Self::Protocol(_))
    }
}

/// Replaces every occurrence of `secret` in `message` with `***`.
///
/// Use it on client-library error text that may echo the password it was given.
#[must_use]
pub fn redact_secret(message: &str, secret: &Secret) -> String {
    let value = secret.expose();
    if value.is_empty() {
        return message.to_owned();
    }
    message.replace(value, "***")
}

/// Masks credentials in text that may contain a connection string or URL:
/// `scheme://user:password@host` user-info passwords and `key=value` pairs whose key looks like a
/// secret (`password`, `pwd`, `passwd`, `sslpassword`, `token`, `secret`, `apikey`…), in both
/// space-separated (libpq) and `;`/`&`-separated (ODBC, URL query) forms.
#[must_use]
pub fn redact_connection_string(text: &str) -> String {
    redact_kv_pairs(&redact_userinfo(text))
}

fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("://") {
        let (head, tail) = rest.split_at(idx + 3);
        out.push_str(head);
        // The authority ends at the first '/', '?', '#' or whitespace.
        let end = tail
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        if let Some(at) = authority.rfind('@') {
            let userinfo = &authority[..at];
            if let Some(colon) = userinfo.find(':') {
                out.push_str(&userinfo[..colon]);
                out.push_str(":***");
                out.push_str(&authority[at..]);
            } else {
                out.push_str(authority);
            }
        } else {
            out.push_str(authority);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "pass",
    "sslpassword",
    "token",
    "secret",
    "apikey",
    "api_key",
    "access_token",
    "auth_token",
];

fn is_secret_key(key: &str) -> bool {
    let k = key
        .trim()
        .trim_start_matches(['?', '&', ';'])
        .to_ascii_lowercase();
    SECRET_KEYS
        .iter()
        .any(|s| k == *s || k.ends_with(&format!("_{s}")))
}

fn redact_kv_pairs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut token_start = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'=' {
            let key = &text[token_start..i];
            out.push_str(&text[token_start..=i]);
            i += 1;
            // Value: quoted ('…' or "…") or up to the next separator.
            let value_end = if i < bytes.len() && (bytes[i] == b'\'' || bytes[i] == b'"') {
                let q = bytes[i];
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != q {
                    if bytes[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                (j + 1).min(bytes.len())
            } else {
                let mut j = i;
                while j < bytes.len() && !matches!(bytes[j], b' ' | b'\t' | b'\n' | b';' | b'&') {
                    j += 1;
                }
                j
            };
            if is_secret_key(key) {
                out.push_str("***");
            } else {
                out.push_str(&text[i..value_end]);
            }
            i = value_end;
            token_start = i;
        } else if matches!(c, b' ' | b'\t' | b'\n' | b';' | b'&' | b'?' | b',') {
            out.push_str(&text[token_start..=i]);
            i += 1;
            token_start = i;
        } else {
            i += 1;
        }
    }
    if token_start < text.len() {
        out.push_str(&text[token_start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn redacts_url_userinfo() {
        let s = "error connecting to postgres://nine:Pa55word@db.example.test:5432/app";
        let r = redact_connection_string(s);
        assert!(!r.contains("Pa55word"));
        assert!(r.contains("postgres://nine:***@db.example.test:5432/app"));
    }

    #[test]
    fn redacts_kv_forms() {
        for s in [
            "host=db user=nine password=Pa55word dbname=app",
            "Server=db;Uid=nine;Pwd=Pa55word;Database=app",
            "mysql://db/app?user=nine&password=Pa55word&ssl=true",
            "host=db password='Pa55 word' port=1",
            "token=Pa55word",
        ] {
            let r = redact_connection_string(s);
            assert!(!r.contains("Pa55"), "{s} -> {r}");
            assert!(r.contains("***"), "{s} -> {r}");
        }
    }

    #[test]
    fn keeps_harmless_text() {
        let s = "relation \"users\" does not exist; host=db port=5432";
        assert_eq!(redact_connection_string(s), s);
    }

    #[test]
    fn redact_secret_replaces_all() {
        let secret = Secret::new("abc123".to_owned());
        assert_eq!(redact_secret("x abc123 y abc123", &secret), "x *** y ***");
        let empty = Secret::new(String::new());
        assert_eq!(redact_secret("x", &empty), "x");
    }

    #[test]
    fn error_messages_are_presentable() {
        let e = DriverError::Query {
            code: Some("42P01".into()),
            message: "relation does not exist".into(),
            position: Some(15),
        };
        assert_eq!(e.to_string(), "relation does not exist");
        assert!(DriverError::Closed("eof".into()).is_fatal());
        assert!(!DriverError::Cancelled.is_fatal());
    }

    proptest! {
        #[test]
        fn url_password_never_survives(
            user in "[a-z]{1,8}",
            pw in "[A-Za-z0-9!$%*+.~-]{6,24}",
            host in "[a-z]{1,10}\\.test",
            prefix in "[ -~]{0,20}",
        ) {
            let s = format!("{prefix} mysql://{user}:{pw}@{host}:3306/db");
            let r = redact_connection_string(&s);
            let needle = format!(":{pw}@");
            prop_assert!(!r.contains(&needle));
        }

        #[test]
        fn kv_password_never_survives(
            pw in "[A-Za-z0-9!$%*+.~-]{6,24}",
            key in prop::sample::select(vec!["password", "PASSWORD", "pwd", "Pwd", "sslpassword", "token"]),
            sep in prop::sample::select(vec![" ", ";", "&"]),
        ) {
            let s = format!("host=db{sep}user=nine{sep}{key}={pw}{sep}dbname=app");
            let r = redact_connection_string(&s);
            prop_assert!(!r.contains(&pw), "{} -> {}", s, r);
        }

        #[test]
        fn never_panics(s in "\\PC{0,200}") {
            let _ = redact_connection_string(&s);
        }
    }
}
