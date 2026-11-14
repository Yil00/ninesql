//! Minimal JDBC URL parser for the forms DBeaver writes (docs/06-dbeaver-interop.md, "JDBC URL").
//!
//! Supported:
//! - `jdbc:<sub>://[user[:password]@]host[:port][,host2…][/database][?k=v&…]` (PostgreSQL,
//!   MySQL, MariaDB, ClickHouse with `jdbc:clickhouse:http://…` too, generic);
//! - `jdbc:<sub>:<path>[?…]` for file engines (SQLite, DuckDB), with an optional `file:` prefix;
//! - `jdbc:<sub>:<database>` (PostgreSQL short form, no host).
//!
//! The URL itself is never kept or printed (it can carry `user:password@`, I1): only the parts
//! below are, and a password found in the user info or the query string is dropped and flagged.

/// Parsed JDBC URL.
#[derive(Default, PartialEq, Eq)]
pub(crate) struct JdbcUrl {
    /// Sub-protocol after `jdbc:` (`postgresql`, `mysql`, `clickhouse:http`…), lower case.
    pub(crate) subprotocol: String,
    /// First host, without IPv6 brackets.
    pub(crate) host: Option<String>,
    /// Port text as written (validated by the caller).
    pub(crate) port: Option<String>,
    /// Further hosts of a fail-over list were present (not imported).
    pub(crate) extra_hosts: bool,
    /// Path after the authority (database name), percent-decoded, without the leading `/`.
    pub(crate) database: Option<String>,
    /// File path of a file engine URL.
    pub(crate) file_path: Option<String>,
    /// User name from the user info.
    pub(crate) user: Option<String>,
    /// The user info had a password (dropped, never stored).
    pub(crate) had_password: bool,
    /// Query parameters, percent-decoded, in order.
    pub(crate) params: Vec<(String, String)>,
}

impl Drop for JdbcUrl {
    /// Parameters and the user info can carry credentials: wipe them.
    fn drop(&mut self) {
        use zeroize::Zeroize;
        for (k, v) in &mut self.params {
            k.zeroize();
            v.zeroize();
        }
        if let Some(u) = &mut self.user {
            u.zeroize();
        }
        if let Some(d) = &mut self.database {
            d.zeroize();
        }
    }
}

impl std::fmt::Debug for JdbcUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JdbcUrl")
            .field("subprotocol", &self.subprotocol)
            .field(
                "param_keys",
                &self.params.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// File-engine sub-protocols whose URL is `jdbc:<sub>:<path>`.
const FILE_SUBPROTOCOLS: &[&str] = &["sqlite", "duckdb"];

/// Parses a JDBC URL; `None` when it does not start with `jdbc:`.
pub(crate) fn parse(url: &str) -> Option<JdbcUrl> {
    let url = url.trim();
    let rest = strip_prefix_ci(url, "jdbc:")?;
    let mut out = JdbcUrl::default();
    if let Some(pos) = rest.find("://") {
        out.subprotocol = rest[..pos].to_ascii_lowercase();
        parse_hierarchical(&rest[pos + 3..], &mut out);
        return Some(out);
    }
    let (sub, tail) = rest.split_once(':')?;
    out.subprotocol = sub.to_ascii_lowercase();
    let (body, query) = split_query(tail);
    out.params = parse_query(query);
    if FILE_SUBPROTOCOLS.contains(&out.subprotocol.as_str()) {
        let path = body.strip_prefix("file:").unwrap_or(body);
        if !path.is_empty() {
            out.file_path = Some(percent_decode(path));
        }
    } else if !body.is_empty() && !body.starts_with('/') {
        out.database = Some(percent_decode(body));
    }
    Some(out)
}

fn parse_hierarchical(s: &str, out: &mut JdbcUrl) {
    let authority_end = s.find(['/', '?', ';']).unwrap_or(s.len());
    let (authority, after) = s.split_at(authority_end);
    let host_part = match authority.rsplit_once('@') {
        Some((userinfo, host)) => {
            let (user, password) = match userinfo.split_once(':') {
                Some((u, _)) => (u, true),
                None => (userinfo, false),
            };
            if !user.is_empty() {
                out.user = Some(percent_decode(user));
            }
            out.had_password = password;
            host
        }
        None => authority,
    };
    let mut hosts = host_part.split(',');
    if let Some(first) = hosts.next() {
        let (host, port) = split_host_port(first);
        if !host.is_empty() {
            out.host = Some(host.to_owned());
        }
        out.port = port.filter(|p| !p.is_empty()).map(str::to_owned);
    }
    out.extra_hosts = hosts.any(|h| !h.is_empty());
    let (path, query) = split_query(after);
    let path = path.strip_prefix('/').unwrap_or(path);
    // `;k=v` parameters (SQL Server style) are not parsed: kept out of the database name.
    let path = path.split(';').next().unwrap_or_default();
    if !path.is_empty() {
        out.database = Some(percent_decode(path));
    }
    out.params = parse_query(query);
}

/// `host[:port]` or `[v6]:port`.
fn split_host_port(s: &str) -> (&str, Option<&str>) {
    if let Some(rest) = s.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((host, tail)) => (host, tail.strip_prefix(':')),
            None => (rest, None),
        };
    }
    match s.rsplit_once(':') {
        // More than one colon without brackets: a bare IPv6 address, no port.
        Some((h, _)) if h.contains(':') => (s, None),
        Some((h, p)) => (h, Some(p)),
        None => (s, None),
    }
}

fn split_query(s: &str) -> (&str, &str) {
    s.split_once('?').unwrap_or((s, ""))
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Decodes `%XX` sequences; invalid sequences are kept as written, invalid UTF-8 is replaced.
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|c| hex(*c)),
                bytes.get(i + 2).and_then(|c| hex(*c)),
            )
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_url() {
        let u = parse("jdbc:postgresql://pg-url.example.invalid:5433/nine_url?sslmode=verify-full")
            .unwrap_or_default();
        assert_eq!(u.subprotocol, "postgresql");
        assert_eq!(u.host.as_deref(), Some("pg-url.example.invalid"));
        assert_eq!(u.port.as_deref(), Some("5433"));
        assert_eq!(u.database.as_deref(), Some("nine_url"));
        assert_eq!(u.params, [("sslmode".to_owned(), "verify-full".to_owned())]);
    }

    #[test]
    fn userinfo_ipv6_failover() {
        let u =
            parse("JDBC:mysql://nine_user:fixture-pw@[2001:db8::1]:3307,h2.example.invalid/d%20b")
                .unwrap_or_default();
        assert_eq!(u.user.as_deref(), Some("nine_user"));
        assert!(u.had_password);
        assert_eq!(u.host.as_deref(), Some("2001:db8::1"));
        assert_eq!(u.port.as_deref(), Some("3307"));
        assert!(u.extra_hosts);
        assert_eq!(u.database.as_deref(), Some("d b"));
        assert!(!format!("{u:?}").contains("fixture"));
    }

    #[test]
    fn file_and_short_forms() {
        let u = parse("jdbc:sqlite:/home/nine_user/data/x.sqlite").unwrap_or_default();
        assert_eq!(
            u.file_path.as_deref(),
            Some("/home/nine_user/data/x.sqlite")
        );
        let u = parse("jdbc:sqlite:file:/home/nine_user/x.db?mode=ro").unwrap_or_default();
        assert_eq!(u.file_path.as_deref(), Some("/home/nine_user/x.db"));
        assert_eq!(u.params.len(), 1);
        let u = parse("jdbc:postgresql:nine").unwrap_or_default();
        assert_eq!(u.host, None);
        assert_eq!(u.database.as_deref(), Some("nine"));
        let u = parse("jdbc:clickhouse:http://ch.example.invalid:8123/default").unwrap_or_default();
        assert_eq!(u.subprotocol, "clickhouse:http");
        assert_eq!(u.host.as_deref(), Some("ch.example.invalid"));
        assert!(parse("postgresql://x").is_none());
        assert!(parse("jdbc:").is_none());
    }

    proptest::proptest! {
        #[test]
        fn parse_never_panics(s in "\\PC{0,80}", t in "(jdbc:)?[a-z:]{0,12}(//)?[\\[\\]@:,/?&=%;a-z0-9]{0,40}") {
            let _ = parse(&s);
            let _ = parse(&t);
            let _ = percent_decode(&s);
        }

        #[test]
        fn host_never_keeps_userinfo(user in "[a-z]{1,8}", pw in "[a-z0-9]{1,8}", host in "[a-z]{1,8}\\.example\\.test") {
            let u = parse(&format!("jdbc:postgresql://{user}:{pw}@{host}:5432/db")).unwrap_or_default();
            proptest::prop_assert_eq!(u.host.as_deref(), Some(host.as_str()));
            proptest::prop_assert!(u.had_password);
            proptest::prop_assert_eq!(u.user.as_deref(), Some(user.as_str()));
        }
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%3Db%zz%4"), "a=b%zz%4");
        assert_eq!(percent_decode("%ff"), "\u{fffd}");
    }
}
