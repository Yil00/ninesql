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
    ///
    /// `message` is **display-only**: engines may quote values or statement text in it, so logs
    /// record only `code` and the variant, and nothing for production connections.
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
    /// The text holds several statements; drivers execute exactly one per call
    /// ([`ensure_single_statement`](crate::dialect::ensure_single_statement)).
    #[error("{0} statements found: run them one at a time")]
    MultipleStatements(usize),
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

/// Masks credentials in text that may contain a connection string, a URL, JSON, HTTP headers,
/// SQL or a command line:
/// - `scheme://user:password@host` user-info passwords (up to the last `@` before the next
///   whitespace, so passwords containing `/`, `#` or `@` are covered);
/// - `user:password@host` without a scheme (Go DSNs such as `user:pw@tcp(host:3306)/db`);
/// - `key=value`, `key: value`, `"key":"value"` pairs whose key contains `password`, `passwd`,
///   `secret`, `token`, `apikey`, `credential` or `authorization`, or is `pwd` / `pass` / `auth`
///   (case-insensitive, separators ignored: `trustStorePassword`, `PGPASSWORD`, `api-key`),
///   including prefixed literals (`N'…'`, `E'…'`) and `PASSWORD('…')` calls;
/// - the credential after an `Authorization: Bearer|Basic|Token …` scheme;
/// - SQL forms: `PASSWORD '…'`, `IDENTIFIED BY '…'`, `IDENTIFIED WITH plugin BY '…'`,
///   `IDENTIFIED BY PASSWORD '…'`, `SET PASSWORD = PASSWORD('…')`;
/// - command-line forms: `-p secret`, `-psecret`, `--password secret` (`-p` is also the port
///   flag of some tools: masking a port is accepted over-redaction).
///
/// Redaction is best effort over free text: it never replaces passing structured parameters
/// and never makes it acceptable to log a statement or an error that may carry a secret.
#[must_use]
pub fn redact_connection_string(text: &str) -> String {
    let text = redact_userinfo(text);
    let text = redact_bare_userinfo(&text);
    let text = redact_sql_forms(&text);
    let text = redact_kv_pairs(&text);
    redact_cli_flags(&text)
}

fn is_segment_end(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`')
}

/// `user:password@host` without `://`: in each whitespace-delimited segment that has an `@`
/// after a `:`, masks from the first `:` to the last `@`.
fn redact_bare_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let start = rest
            .find(|c: char| !is_segment_end(c))
            .unwrap_or(rest.len());
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(is_segment_end).unwrap_or(rest.len());
        let segment = &rest[..end];
        let masked = (!segment.contains("://"))
            .then(|| {
                let at = segment.rfind('@')?;
                let colon = segment[..at].find(':')?;
                // A user name must precede the colon: `x:y@h`, `DSN=x:y@h`.
                let user = &segment[..colon];
                let user_ok = user
                    .bytes()
                    .last()
                    .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
                (user_ok && at > colon + 1)
                    .then(|| format!("{}:***{}", &segment[..colon], &segment[at..]))
            })
            .flatten();
        out.push_str(masked.as_deref().unwrap_or(segment));
        rest = &rest[end..];
    }
    out
}

fn eq_word(bytes: &[u8], start: usize, end: usize, word: &str) -> bool {
    end - start == word.len() && bytes[start..end].eq_ignore_ascii_case(word.as_bytes())
}

/// Masks the quoted literal starting at `k` (after optional blanks, an optional `(` and an
/// optional one-letter prefix such as `N` / `E`). Returns `(open_quote, close_end)`.
fn literal_at(bytes: &[u8], k: usize) -> Option<(usize, usize)> {
    let mut k = skip_blanks(bytes, k);
    if bytes.get(k) == Some(&b'(') {
        k = skip_blanks(bytes, k + 1);
    }
    if bytes
        .get(k)
        .is_some_and(|b| matches!(b, b'N' | b'n' | b'E' | b'e' | b'B' | b'b'))
        && matches!(bytes.get(k + 1), Some(b'\'' | b'"'))
    {
        k += 1;
    }
    match bytes.get(k) {
        Some(b'\'' | b'"') => Some((k, value_end(bytes, k))),
        _ => None,
    }
}

/// SQL forms that carry a password literal: `PASSWORD '…'`, `PASSWORD('…')`,
/// `IDENTIFIED [WITH plugin] BY [PASSWORD] '…'`.
fn redact_sql_forms(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut i = 0;
    let mut identified_at: Option<usize> = None;
    while i < bytes.len() {
        if !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_')
            || (i > 0 && is_key_byte(bytes[i - 1]))
        {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        let expects_literal = if eq_word(bytes, i, j, "identified") {
            identified_at = Some(j);
            false
        } else if eq_word(bytes, i, j, "by") {
            // `IDENTIFIED BY`, or `IDENTIFIED WITH plugin BY` (a few words in between).
            identified_at.is_some_and(|at| i - at < 64)
        } else {
            eq_word(bytes, i, j, "password") || eq_word(bytes, i, j, "passwd")
        };
        // The literal follows after whitespace or `(`: `"password":"…"` (a JSON key) is a
        // key-value pair, handled later.
        let expects_literal =
            expects_literal && matches!(bytes.get(j), Some(b' ' | b'\t' | b'\n' | b'\r' | b'('));
        let literal_from = if expects_literal && !eq_word(bytes, i, j, "by") {
            skip_password_for(bytes, j)
        } else {
            j
        };
        if expects_literal
            && let Some((open, end)) = literal_at(bytes, literal_from)
            && end > open + 1
        {
            out.push_str(&text[last..=open]);
            out.push_str("***");
            if end - open >= 2 && bytes[end - 1] == bytes[open] {
                out.push_str(&text[end - 1..end]);
            }
            last = end;
            i = end;
            identified_at = None;
            continue;
        }
        i = j;
    }
    out.push_str(&text[last..]);
    out
}

/// After `PASSWORD`: skips `FOR <user> =` (`SET PASSWORD FOR app = '…'`) and a bare `=`, so the
/// literal that follows is found. Returns `j` unchanged otherwise.
fn skip_password_for(bytes: &[u8], j: usize) -> usize {
    let k = skip_blanks(bytes, j);
    let mut e = k;
    while e < bytes.len() && bytes[e].is_ascii_alphabetic() {
        e += 1;
    }
    let mut k = if eq_word(bytes, k, e, "for") {
        // The user: a quoted or bare token, possibly `'u'@'h'`, up to `=`.
        let mut t = skip_blanks(bytes, e);
        while t < bytes.len() && bytes[t] != b'=' && bytes[t] != b'\n' && t - e < 256 {
            t += 1;
        }
        t
    } else {
        k
    };
    if bytes.get(k) == Some(&b'=') {
        k += 1;
    } else if k != skip_blanks(bytes, j) {
        // `FOR` without `=`: not the form we know.
        return j;
    }
    k
}

/// `-p secret`, `-psecret`, `--password secret`, `--pass secret` (the `=` forms are key-value
/// pairs).
fn redact_cli_flags(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut i = 0;
    while i < bytes.len() {
        let at_token = bytes[i] == b'-' && (i == 0 || bytes[i - 1].is_ascii_whitespace());
        if !at_token {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let flag = &text[i..j];
        let value = if flag == "-p" || flag == "--password" || flag == "--pass" {
            let k = skip_blanks(bytes, j);
            let mut e = k;
            while e < bytes.len() && !bytes[e].is_ascii_whitespace() {
                e += 1;
            }
            (e > k && !text[k..e].starts_with('-')).then_some((k, e))
        } else if flag.len() > 2 && flag.starts_with("-p") && !flag.starts_with("-p=") {
            Some((i + 2, j))
        } else {
            None
        };
        match value {
            Some((k, e)) if &text[k..e] != "***" => {
                out.push_str(&text[last..k]);
                out.push_str("***");
                last = e;
                i = e;
            }
            _ => i = j,
        }
    }
    out.push_str(&text[last..]);
    out
}

fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("://") {
        let (head, tail) = rest.split_at(idx + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
            .unwrap_or(tail.len());
        let segment = &tail[..end];
        match segment.rfind('@') {
            Some(at) if segment[..at].contains(':') => {
                let colon = segment.find(':').unwrap_or(0);
                out.push_str(&segment[..colon]);
                out.push_str(":***");
                out.push_str(&segment[at..]);
            }
            _ => out.push_str(segment),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

const SECRET_KEY_FRAGMENTS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "credential",
    "authorization",
];

const SECRET_KEYS_EXACT: &[&str] = &["pwd", "pass", "auth", "pw"];

const AUTH_SCHEMES: &[&str] = &["bearer", "basic", "token", "digest", "negotiate", "apikey"];

fn is_secret_key(key: &str) -> bool {
    let k: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    SECRET_KEYS_EXACT.contains(&k.as_str()) || SECRET_KEY_FRAGMENTS.iter().any(|f| k.contains(f))
}

fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

fn is_value_end(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b';' | b'&' | b',' | b'}' | b')' | b']' | b'"' | b'\'')
}

/// End of a value starting at `k` (quoted or bare).
fn value_end(bytes: &[u8], k: usize) -> usize {
    match bytes.get(k) {
        Some(&q) if q == b'"' || q == b'\'' => {
            let mut j = k + 1;
            while j < bytes.len() && bytes[j] != q {
                if bytes[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            (j + 1).min(bytes.len())
        }
        _ => {
            let mut j = k;
            while j < bytes.len() && !is_value_end(bytes[j]) {
                j += 1;
            }
            j
        }
    }
}

fn skip_blanks(bytes: &[u8], mut k: usize) -> usize {
    while k < bytes.len() && matches!(bytes[k], b' ' | b'\t') {
        k += 1;
    }
    k
}

fn redact_kv_pairs(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut i = 0;
    while i < bytes.len() {
        let starts_key = is_key_byte(bytes[i]) && (i == 0 || !is_key_byte(bytes[i - 1]));
        if !starts_key {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && is_key_byte(bytes[j]) {
            j += 1;
        }
        if !is_secret_key(&text[i..j]) {
            i = j;
            continue;
        }
        // Optional closing quote of a JSON key, blanks, then `=` / `:` / `:=`.
        let mut k = j;
        if matches!(bytes.get(k), Some(b'"' | b'\'')) {
            k += 1;
        }
        k = skip_blanks(bytes, k);
        match bytes.get(k) {
            Some(b'=') => k += 1,
            Some(b':') => {
                k += 1;
                if bytes.get(k) == Some(&b'=') {
                    k += 1;
                }
            }
            _ => {
                i = j;
                continue;
            }
        }
        k = skip_blanks(bytes, k);
        // `N'…'` / `E'…'` (SQL Server, PostgreSQL): the literal starts at the quote.
        if bytes
            .get(k)
            .is_some_and(|b| matches!(b, b'N' | b'n' | b'E' | b'e' | b'B' | b'b'))
            && matches!(bytes.get(k + 1), Some(b'\'' | b'"'))
        {
            out.push_str(&text[last..=k]);
            last = k + 1;
            k += 1;
        }
        let mut end = value_end(bytes, k);
        // `PASSWORD('…')`: a function call whose argument is the secret.
        if end > k && bytes[end - 1] == b'(' && matches!(bytes.get(end), Some(b'\'' | b'"')) {
            out.push_str(&text[last..end]);
            last = end;
            k = end;
            end = value_end(bytes, k);
        }
        // `Authorization: Bearer <credential>`: keep the scheme, mask the credential.
        let first = text[k..end].to_ascii_lowercase();
        if AUTH_SCHEMES.contains(&first.as_str()) {
            let cred = skip_blanks(bytes, end);
            if cred > end {
                k = cred;
                end = value_end(bytes, k);
            }
        }
        if end > k {
            let quoted = matches!(bytes[k], b'"' | b'\'') && end - k >= 2;
            if quoted {
                out.push_str(&text[last..=k]);
                out.push_str("***");
                out.push_str(&text[end - 1..end]);
            } else {
                out.push_str(&text[last..k]);
                out.push_str("***");
            }
            last = end;
        }
        i = end.max(j);
    }
    out.push_str(&text[last..]);
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
    fn redacts_review_m3_forms() {
        for s in [
            "trustStorePassword=Pa55word;x=1",
            "PGPASSWORD=Pa55word psql",
            "{\"password\":\"Pa55word\",\"user\":\"nine\"}",
            "{\"password\": \"Pa55word\"}",
            "password: Pa55word",
            "Authorization: Bearer Pa55word",
            "authorization=Basic Pa55word",
            "api-key=Pa55word",
            "client_secret = 'Pa55word'",
            "postgres://nine:Pa/55#wo@rd@db.example.test:5432/app",
            "jdbc:mysql://nine:Pa55word@db/app",
        ] {
            let r = redact_connection_string(s);
            assert!(!r.contains("Pa55") && !r.contains("55#wo"), "{s} -> {r}");
            assert!(r.contains("***"), "{s} -> {r}");
        }
        assert_eq!(
            redact_connection_string("Authorization: Bearer abc.def"),
            "Authorization: Bearer ***"
        );
        assert_eq!(
            redact_connection_string("password authentication failed for user \"nine\""),
            "password authentication failed for user \"nine\""
        );
    }

    /// F7 of the P0-B security review.
    #[test]
    fn redacts_sql_and_cli_forms() {
        for s in [
            "CREATE ROLE app LOGIN PASSWORD 'Pa55word'",
            "ALTER USER app WITH PASSWORD 'Pa55word' VALID UNTIL 'infinity'",
            "alter role app password \"Pa55word\"",
            "CREATE USER 'app'@'%' IDENTIFIED BY 'Pa55word'",
            "ALTER USER app IDENTIFIED WITH mysql_native_password BY 'Pa55word'",
            "ALTER USER app IDENTIFIED WITH caching_sha2_password BY \"Pa55word\" PASSWORD EXPIRE",
            "GRANT ALL ON *.* TO app IDENTIFIED BY PASSWORD '*Pa55word'",
            "SET PASSWORD = PASSWORD('Pa55word')",
            "SET PASSWORD FOR app = 'Pa55word'",
            "ALTER LOGIN app WITH PASSWORD = N'Pa55word'",
            "dial tcp: nine:Pa55word@tcp(db.example.test:3306)/app",
            "dsn=nine:Pa55word@db.example.test:5432/app failed",
            "mysql -u nine -pPa55word app",
            "mysql -u nine -p Pa55word app",
            "pg_dump --password Pa55word",
            "sshpass -p Pa55word ssh host",
        ] {
            let r = redact_connection_string(s);
            assert!(!r.contains("Pa55word"), "{s} -> {r}");
            assert!(r.contains("***"), "{s} -> {r}");
        }
        assert_eq!(
            redact_connection_string("CREATE USER 'app'@'%' IDENTIFIED BY 'x1'"),
            "CREATE USER 'app'@'%' IDENTIFIED BY '***'"
        );
        assert_eq!(
            redact_connection_string("nine:secret@db.example.test"),
            "nine:***@db.example.test"
        );
        // Not secrets: `user@host:port`, emails, times, plain prose.
        for s in [
            "connect to nine@db.example.test:5432 refused",
            "mail admin@example.test about it",
            "at 12:30 the server restarted",
            "password authentication failed for user \"nine\"",
            "the password is too short",
            "column \"identified\" does not exist",
        ] {
            assert_eq!(redact_connection_string(s), s);
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
        fn url_password_with_reserved_chars_never_survives(
            pw in "[A-Za-z0-9/#@:!$*+.~-]{6,24}",
            host in "[a-z]{1,10}\\.test",
        ) {
            let s = format!("error: postgres://nine:{pw}@{host}:5432/db failed");
            let r = redact_connection_string(&s);
            prop_assert!(!r.contains(&pw), "{} -> {}", s, r);
        }

        #[test]
        fn structured_secrets_never_survive(
            pw in "[A-Za-z0-9!$%*+.~-]{6,24}",
            key in prop::sample::select(vec![
                "password", "trustStorePassword", "PGPASSWORD", "client_secret", "api_key",
                "x-api-key", "accessToken", "Authorization",
            ]),
            form in 0usize..4,
        ) {
            let s = match form {
                0 => format!("{{\"{key}\":\"{pw}\"}}"),
                1 => format!("{key}: {pw}"),
                2 => format!("{key} = '{pw}'"),
                _ => format!("{key}=Bearer {pw}"),
            };
            let r = redact_connection_string(&s);
            prop_assert!(!r.contains(&pw), "{} -> {}", s, r);
        }

        #[test]
        fn never_panics(s in "\\PC{0,200}") {
            let _ = redact_connection_string(&s);
        }

        #[test]
        fn sql_form_passwords_never_survive(
            pw in "[A-Za-z0-9!$%*+.~#@-]{6,24}",
            form in 0usize..6,
            quote in prop::sample::select(vec!['\'', '"']),
        ) {
            let s = match form {
                0 => format!("CREATE ROLE r LOGIN PASSWORD {quote}{pw}{quote}"),
                1 => format!("CREATE USER u IDENTIFIED BY {quote}{pw}{quote}"),
                2 => format!("ALTER USER u IDENTIFIED WITH mysql_native_password BY {quote}{pw}{quote}"),
                3 => format!("SET PASSWORD = PASSWORD({quote}{pw}{quote})"),
                4 => format!("ALTER LOGIN l WITH PASSWORD = N{quote}{pw}{quote}"),
                _ => format!("error: u:{pw}@tcp(db:3306)/x"),
            };
            let r = redact_connection_string(&s);
            prop_assert!(!r.contains(&pw), "{} -> {}", s, r);
        }
    }
}
