//! Rules for free-form driver properties and init SQL (I1, I3, I5).
//!
//! Driver properties must not carry what the typed model already owns: TLS settings (they would
//! bypass `TlsConfig` and its warnings, I5), the endpoint, credentials (I1), the read-only mode
//! (I3), init SQL, proxies or multi-statement / lexical modes. They must not carry a secret value
//! either.
//!
//! **Reserved keys have a single source of truth**:
//! [`ninesql_driver_api::is_reserved_property`], the rule every driver applies through
//! `check_properties` at connect time (it reserves `gssencmode` and `replication` too). The core
//! reuses it, unchanged, for validation (connection dialog, DBeaver import) and for
//! sanitize-on-load, so it never stores a connection a driver would later refuse, and never
//! refuses one a driver accepts. No benign list overrides it: a key the driver API reserves is
//! always refused here. This module only adds:
//! - a classification of reserved keys ([`Reserved`]) to tell the user which typed field to use;
//! - the secret-**value** detectors ([`value_carries_secret`], [`sql_carries_secret`]), layered on
//!   top of the key rules.
//!
//! Extra keys a single driver reserves on top of the shared rule are not visible here yet; they
//! are refused by that driver at connect time (ROADMAP integration-review follow-up).
//!
//! The secret-like **key** heuristic is [`ninesql_storage::hygiene::is_secret_like_entry`] (with
//! its own benign list and value-shape checks, which only affect that heuristic).

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

/// Normalization of the driver API (lower case, ASCII alphanumerics only).
fn normalize(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a property key is reserved: exactly [`ninesql_driver_api::is_reserved_property`].
#[must_use]
pub fn is_reserved(key: &str) -> bool {
    ninesql_driver_api::is_reserved_property(key)
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

/// Whether a property value embeds a credential:
/// - a URL with `user:password@` user info (`postgres://u:p@h/db`, also after `jdbc:`);
/// - user info without a scheme, as in Go DSNs (`user:pass@tcp(h:3306)/db`,
///   `user:pass@unix(/s)/db`, `user:pass@/db`) and Oracle thin URLs with credentials
///   (`jdbc:oracle:thin:scott/tiger@h:1521/s`): a word with a `:`, then at least one character,
///   then `@` and something after it. Words are separated by whitespace, quotes and `;`. Two
///   narrow look-alikes are exempt: the MySQL assignment `@a:=@b` (`:=` right before `@`) and
///   an Oracle thin URL without credentials (`…:thin:@h`). Others, such as `mailto:a@b`, fail
///   closed;
/// - a `key=value` or `key: value` pair (also `"key":"value"`) whose key looks like a secret per
///   [`ninesql_storage::hygiene::is_secret_like_entry`]: `password`, `pwd`, `pass`, `passphrase`,
///   `token` (`access_token`, `csrftoken`…), `apikey`, `secret` (`client_secret`,
///   `aws_secret_access_key`…), `private_key`, `credential`… with a non-empty value. This is the
///   same fail-closed fragment rule as for property keys, so `token_type=bearer` is refused too.
#[must_use]
pub fn value_carries_secret(value: &str) -> bool {
    let lower = value.to_lowercase();
    url_userinfo(&lower) || schemeless_userinfo(&lower) || secret_pair(&lower)
}

fn url_userinfo(lower: &str) -> bool {
    let mut rest = lower;
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
    false
}

fn schemeless_userinfo(lower: &str) -> bool {
    lower
        .split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`' | ';'))
        .filter(|word| !word.contains("://"))
        .any(|word| {
            word.match_indices('@').any(|(at, _)| {
                let user_info = &word[..at];
                let exempt = user_info.ends_with(":=") || user_info.ends_with(":thin:");
                !exempt
                    && at + 1 < word.len()
                    && user_info
                        .find(':')
                        .is_some_and(|colon| colon + 1 < user_info.len())
            })
        })
}

/// Characters of a key in a `key=value` / `key: value` pair.
fn is_key_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Whether a lower-cased value holds a `key=value` / `key: value` pair with a secret-like key and
/// a non-empty value. `==`, `!=`, `<=`, `>=`, `:=`, `::` and `://` are not pair separators.
fn secret_pair(lower: &str) -> bool {
    let chars: Vec<char> = lower.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != '=' && c != ':' {
            continue;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let not_a_separator = match c {
            '=' => matches!(prev, Some('=' | '!' | '<' | '>' | ':')) || next == Some('='),
            _ => matches!(next, Some(':' | '/' | '=')) || prev == Some(':'),
        };
        if not_a_separator {
            continue;
        }
        // Key: back over spaces and one closing quote, then key characters.
        let mut k = i;
        while k > 0 && chars[k - 1] == ' ' {
            k -= 1;
        }
        if k > 0 && matches!(chars[k - 1], '"' | '\'') {
            k -= 1;
        }
        let key_end = k;
        while k > 0 && is_key_char(chars[k - 1]) {
            k -= 1;
        }
        let key: String = chars[k..key_end].iter().collect();
        // Value: over spaces and one opening quote, up to a delimiter.
        let mut v = i + 1;
        while v < chars.len() && chars[v] == ' ' {
            v += 1;
        }
        if v < chars.len() && matches!(chars[v], '"' | '\'') {
            v += 1;
        }
        let value: String = chars[v..]
            .iter()
            .take_while(|c| {
                !c.is_whitespace() && !matches!(c, '"' | '\'' | ';' | '&' | ',' | '}' | ')')
            })
            .collect();
        if !key.is_empty() && ninesql_storage::hygiene::is_secret_like_entry(&key, &value) {
            return true;
        }
    }
    false
}

/// Whether an init SQL statement contains an obvious credential:
/// - `IDENTIFIED BY …`, `IDENTIFIED WITH <plugin> BY …` and `IDENTIFIED WITH <plugin> AS
///   '<hash>'` (MySQL / MariaDB / Oracle; a password hash is credential material too);
/// - `SET PASSWORD [FOR 'u'@'h'] = <literal>` (MySQL);
/// - `PASSWORD` followed (after an optional `=`, `TO` and opening parentheses) by a string
///   literal, including prefixed literals: `'…'`, `"…"`, `$$…$$`, `E'…'`, `N'…'`, `U&'…'`,
///   `B'…'`, `X'…'`, charset introducers (`_utf8mb4'…'`) and hexadecimal hashes (`0x…`, SQL
///   Server `WITH PASSWORD = 0x… HASHED`). This covers
///   `PASSWORD '…'`, `SET PASSWORD = PASSWORD('…')`, `ALTER LOGIN … WITH PASSWORD = N'…'` and
///   `IDENTIFIED VIA … USING PASSWORD('…')`;
/// - anything [`value_carries_secret`] reports (`dblink_connect('… password=…')`, DSNs with user
///   info).
///
/// The statement is checked as written and again with its comments (`--`, `#`, `/* */`)
/// replaced by spaces, so a comment cannot split a keyword from its literal and commented-out or
/// executable (`/*! … */`) text is still seen.
#[must_use]
pub fn sql_carries_secret(sql: &str) -> bool {
    sql_text_carries_secret(sql) || sql_text_carries_secret(&strip_sql_comments(sql))
}

fn sql_text_carries_secret(sql: &str) -> bool {
    if value_carries_secret(sql) {
        return true;
    }
    let words: Vec<String> = sql.split_whitespace().map(str::to_lowercase).collect();
    let flat = words.join(" ");
    if flat.contains("identified by") {
        return true;
    }
    let mut rest = flat.as_str();
    while let Some(pos) = rest.find("identified with") {
        let after = &rest[pos + "identified with".len()..];
        let statement = after.split(';').next().unwrap_or_default();
        if [" by ", " by'", " as ", " as'"]
            .iter()
            .any(|w| statement.contains(w))
        {
            return true;
        }
        rest = after;
    }
    let mut rest = flat.as_str();
    while let Some(pos) = rest.find("set password") {
        let after = &rest[pos + "set password".len()..];
        let statement = after.split(';').next().unwrap_or_default();
        // Word boundary: not `set password_encryption`.
        let whole_word = !statement.starts_with(is_key_char);
        if whole_word && let Some(eq) = statement.find('=') {
            let mut value = statement[eq + 1..].trim_start();
            while let Some(inner) = value.strip_prefix('(') {
                value = inner.trim_start();
            }
            if starts_with_literal(value) {
                return true;
            }
        }
        rest = after;
    }
    let mut rest = flat.as_str();
    while let Some(pos) = rest.find("password") {
        let mut after = rest[pos + "password".len()..].trim_start();
        after = after.strip_prefix('=').map_or(after, str::trim_start);
        after = after.strip_prefix("to ").map_or(after, str::trim_start);
        while let Some(inner) = after.strip_prefix('(') {
            after = inner.trim_start();
        }
        if starts_with_literal(after) {
            return true;
        }
        rest = &rest[pos + "password".len()..];
    }
    false
}

/// Whether lower-cased SQL text starts with a string literal, possibly prefixed.
fn starts_with_literal(s: &str) -> bool {
    let quoted = |t: &str| t.starts_with(['\'', '"', '$']);
    if quoted(s) || s.starts_with("0x") {
        return true;
    }
    if ["u&", "e", "n", "b", "x"]
        .iter()
        .any(|p| s.strip_prefix(p).is_some_and(quoted))
    {
        return true;
    }
    // Charset introducer: `_utf8mb4'…'`.
    s.strip_prefix('_').is_some_and(|t| {
        let name_len = t
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(t.len());
        name_len > 0 && quoted(&t[name_len..])
    })
}

/// Replaces SQL comments (`-- …`, `# …` to the end of the line, `/* … */`) by a space, outside
/// single- and double-quoted text.
fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                out.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        break;
                    }
                }
                out.push(' ');
            }
            '#' => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        break;
                    }
                }
                out.push(' ');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for d in chars.by_ref() {
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
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
            ("replication", Reserved::Other),
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

    /// The core is exactly as strict as the driver API.
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
            "replication",
        ] {
            let api = ninesql_driver_api::is_reserved_property(key);
            assert_eq!(reserved(key).is_some(), api, "{key}");
        }
        assert!(ninesql_driver_api::is_reserved_property("gssencmode"));
        assert!(ninesql_driver_api::is_reserved_property("replication"));
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
            // N3: Go DSNs without a scheme.
            "user:Hunter2@tcp(db:3306)/app",
            "user:Hunter2@unix(/run/mysqld.sock)/app",
            "user:Hunter2@/app?charset=utf8mb4",
            "dsn=user:Hunter2@tcp(db)/app",
            ":Hunter2@tcp(db)/app",
            "user:p@ss@tcp(db)/app",
            // N3: token / API key / secret pairs.
            "token=abc123",
            "access_token=abc123",
            "Refresh_Token = abc123",
            "apikey=abc123",
            "api_key=abc123",
            "api-key=abc123",
            "secret=abc123",
            "client_secret=abc123",
            "a=1;Password=Hunter2",
            // Security review L3: JSON / YAML-style pairs and more secret key names.
            r#"{"password":"Hunter2"}"#,
            "password: Hunter2",
            "secret_key=abc123",
            "aws_secret_access_key=abc123",
            "private_key=abc123",
            "pass=Hunter2",
            "passphrase = Hunter2",
            "x=1&client_secret=abc",
            // Fails closed like the key rule (the key `token_type` is refused as a property too).
            "token_type=bearer",
            // Oracle thin URL with credentials.
            "jdbc:oracle:thin:scott/tiger@db:1521/orcl",
        ] {
            assert!(value_carries_secret(bad), "{bad}");
        }
        for ok in [
            "postgres://db/x",
            "postgres://app@db/x",
            "10",
            "https://example.test/a:b@c",
            "password_encryption",
            "user@db.example.test",
            "db.example.test:5432",
            "12:30",
            "user:@tcp(db)/app",
            "UTF-8",
            "Europe/Paris",
            // Security review L5: narrow look-alikes that are not credentials.
            "jdbc:oracle:thin:@db:1521/orcl",
            "jdbc:oracle:thin:@//db:1521/orcl",
            "@a:=@b",
            "password_encryption=scram-sha-256",
            "passwordCharacterEncoding: UTF-8",
            "a==b",
            "x::text",
            "token=",
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
            // N3: MySQL / MariaDB / SQL Server forms.
            "SET PASSWORD = PASSWORD('Hunter2')",
            "SET PASSWORD FOR app = PASSWORD ( 'Hunter2' )",
            "ALTER LOGIN app WITH PASSWORD = N'Hunter2'",
            "CREATE USER app IDENTIFIED WITH mysql_native_password BY 'Hunter2'",
            "ALTER USER app IDENTIFIED WITH caching_sha2_password BY'Hunter2'",
            "CREATE USER app IDENTIFIED VIA ed25519 USING PASSWORD('Hunter2')",
            // N3: prefixed literals.
            "ALTER ROLE app PASSWORD U&'Hunter\\0032'",
            "ALTER ROLE app PASSWORD E'Hunter2'",
            "ALTER USER app PASSWORD _utf8mb4'Hunter2'",
            // N3: comments cannot split the keyword from its literal, nor hide the statement.
            "ALTER ROLE app PASSWORD/**/'Hunter2'",
            "ALTER ROLE app PASSWORD -- note\n 'Hunter2'",
            "ALTER ROLE app PASSWORD # note\n 'Hunter2'",
            "ALTER USER app IDENTIFIED/* x */BY 'Hunter2'",
            "/*!80000 ALTER USER app IDENTIFIED BY 'Hunter2' */",
            "SELECT 1 -- ALTER USER app IDENTIFIED BY 'Hunter2'",
            // Connection strings inside SQL.
            "SELECT dblink_connect('host=h user=u password=Hunter2')",
            // Security review L3.
            "SET PASSWORD FOR 'app'@'%' = 'Hunter2'",
            "SET PASSWORD FOR app@localhost = _utf8mb4'Hunter2'",
            "CREATE USER app IDENTIFIED WITH mysql_native_password AS '*2470C0C06DEE42FD1618BB99005ADCA2EC9D1E19'",
            "ALTER LOGIN app WITH PASSWORD = 0x0200A1B2C3D4 HASHED",
            "SET @cfg = '{\"api_key\": \"abc123\"}'",
        ] {
            assert!(sql_carries_secret(bad), "{bad}");
        }
        for ok in [
            "SET search_path TO app",
            "SET password_encryption = 'scram-sha-256'",
            "SELECT 1",
            "SET NAMES utf8mb4",
            "SELECT '--' AS dashes, 'a#b' AS hash",
            "CREATE USER app IDENTIFIED WITH auth_socket",
            "SET application_name = 'NineSQL /* x */'",
            "SET @a := @b",
            "SET PASSWORD_ENCRYPTION = 'scram-sha-256'",
        ] {
            assert!(!sql_carries_secret(ok), "{ok}");
        }
    }
}
