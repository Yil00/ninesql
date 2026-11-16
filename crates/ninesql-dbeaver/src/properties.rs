//! JDBC driver properties and URL parameters → NineSQL driver properties
//! (docs/06-dbeaver-interop.md, "Driver properties").
//!
//! DBeaver hands its driver properties and JDBC URL parameters to a Java JDBC driver (pgjdbc,
//! MySQL Connector/J, MariaDB Connector/J, xerial sqlite-jdbc). NineSQL's drivers take a short
//! allowlist of their own property names instead (`ConnectCapabilities::accepted_properties`),
//! and refuse any other key at connect. For the engines a NineSQL driver family serves, this
//! module turns each JDBC property into either NineSQL properties with the same effect, or a
//! "dropped" entry with the reason. For the other engines (no NineSQL driver yet), properties are
//! kept as they are.
//!
//! Only property **names** and setting names read from `options` / `sessionVariables` (when they
//! are plain identifiers) appear in the report, never a value (I1).

use ninesql_driver_api::EngineId;
use ninesql_storage::hygiene::is_secret_like_key;

/// Which JDBC driver's property semantics apply, from the DBeaver driver and the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rules {
    /// pgjdbc → `ninesql-driver-pgwire` (PostgreSQL, TimescaleDB, YugabyteDB).
    PgJdbc,
    /// pgjdbc → `ninesql-driver-pgwire` for QuestDB, which accepts no driver property
    /// (QuestDB ignores PostgreSQL session settings): every property is dropped with a report
    /// entry, but pgjdbc's semantics still apply to read them (URL precedence, `options` read
    /// for its read-only and secret-like entries).
    QuestDb,
    /// MySQL Connector/J → `ninesql-driver-mysqlwire` (MySQL, TiDB). `mariadb`: the server is
    /// MariaDB (`max_statement_time` instead of `max_execution_time`). `legacy`: DBeaver's
    /// `mysql5` driver (Connector/J 5.1), which has no `forceConnectionTimeZoneToSession`.
    ConnectorJ { mariadb: bool, legacy: bool },
    /// MariaDB Connector/J → `ninesql-driver-mysqlwire`. `mariadb` as above.
    MariaDbJ { mariadb: bool },
    /// xerial sqlite-jdbc → `ninesql-driver-sqlite`.
    SqliteJdbc,
    /// No NineSQL driver for this engine yet: properties are kept unchanged.
    Keep,
}

impl Rules {
    pub(crate) fn for_connection(engine: &EngineId, dbeaver_driver: &str) -> Self {
        let mariadb_driver = dbeaver_driver.eq_ignore_ascii_case("mariadb");
        let legacy = dbeaver_driver.eq_ignore_ascii_case("mysql5");
        if *engine == EngineId::POSTGRESQL
            || *engine == EngineId::TIMESCALEDB
            || *engine == EngineId::YUGABYTEDB
        {
            Self::PgJdbc
        } else if *engine == EngineId::QUESTDB {
            Self::QuestDb
        } else if *engine == EngineId::MYSQL || *engine == EngineId::TIDB {
            if mariadb_driver {
                Self::MariaDbJ { mariadb: false }
            } else {
                Self::ConnectorJ {
                    mariadb: false,
                    legacy,
                }
            }
        } else if *engine == EngineId::MARIADB {
            if mariadb_driver {
                Self::MariaDbJ { mariadb: true }
            } else {
                Self::ConnectorJ {
                    mariadb: true,
                    legacy,
                }
            }
        } else if *engine == EngineId::SQLITE {
            Self::SqliteJdbc
        } else {
            Self::Keep
        }
    }

    /// Why a property with no equivalent is dropped, for messages.
    fn no_equivalent_message(self) -> String {
        match self {
            Self::QuestDb => "not imported: NineSQL's QuestDB driver accepts no driver property \
                              (QuestDB ignores PostgreSQL session settings)"
                .to_owned(),
            _ => format!(
                "not imported: no NineSQL equivalent (the NineSQL driver accepts {})",
                self.accepted()
            ),
        }
    }

    /// The NineSQL driver's accepted property names, for messages.
    fn accepted(self) -> &'static str {
        match self {
            Self::QuestDb => "",
            Self::PgJdbc => {
                "search_path, TimeZone, statement_timeout, lock_timeout, work_mem, \
                 client_min_messages"
            }
            Self::ConnectorJ { mariadb: false, .. } | Self::MariaDbJ { mariadb: false } => {
                "time_zone, lock_wait_timeout, innodb_lock_wait_timeout, group_concat_max_len, \
                 max_execution_time"
            }
            Self::ConnectorJ { mariadb: true, .. } | Self::MariaDbJ { mariadb: true } => {
                "time_zone, lock_wait_timeout, innodb_lock_wait_timeout, group_concat_max_len, \
                 max_statement_time"
            }
            Self::SqliteJdbc => "busy_timeout, cache_size, foreign_keys",
            Self::Keep => "",
        }
    }

    /// Whether a JDBC URL parameter wins over a driver property of the same name, as the JDBC
    /// driver resolves them (checked in the public sources, docs/06 "Driver properties"):
    /// pgjdbc (`Driver.parseURL`: URL values first) and MariaDB Connector/J
    /// (`Configuration.processUrlParameters` writes over the properties) let the URL win;
    /// MySQL Connector/J (`ConnectionUrl.collectProperties` puts the properties last) and
    /// sqlite-jdbc (`extractPragmasFromFilename` keeps a property already set) let the
    /// property win. For engines without a NineSQL driver the property wins.
    pub(crate) fn url_wins(self) -> bool {
        matches!(self, Self::PgJdbc | Self::QuestDb | Self::MariaDbJ { .. })
    }

    pub(crate) fn jdbc_driver(self) -> &'static str {
        match self {
            Self::PgJdbc | Self::QuestDb => "pgjdbc",
            Self::ConnectorJ { .. } => "MySQL Connector/J",
            Self::MariaDbJ { .. } => "MariaDB Connector/J",
            Self::SqliteJdbc => "sqlite-jdbc",
            Self::Keep => "the JDBC driver",
        }
    }

    /// NineSQL property names of this family, by normalized name.
    fn targets(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::PgJdbc => PG_TARGETS,
            Self::ConnectorJ { mariadb: false, .. } | Self::MariaDbJ { mariadb: false } => {
                MYSQL_TARGETS
            }
            Self::ConnectorJ { mariadb: true, .. } | Self::MariaDbJ { mariadb: true } => {
                MARIADB_TARGETS
            }
            Self::SqliteJdbc => SQLITE_TARGETS,
            Self::QuestDb | Self::Keep => &[],
        }
    }
}

/// pgwire's accepted properties (normalized key, name sent), as in `ninesql-driver-pgwire`.
const PG_TARGETS: &[(&str, &str)] = &[
    ("searchpath", "search_path"),
    ("timezone", "TimeZone"),
    ("statementtimeout", "statement_timeout"),
    ("locktimeout", "lock_timeout"),
    ("workmem", "work_mem"),
    ("clientminmessages", "client_min_messages"),
];

/// mysqlwire's accepted properties for MySQL and TiDB.
const MYSQL_TARGETS: &[(&str, &str)] = &[
    ("timezone", "time_zone"),
    ("lockwaittimeout", "lock_wait_timeout"),
    ("innodblockwaittimeout", "innodb_lock_wait_timeout"),
    ("groupconcatmaxlen", "group_concat_max_len"),
    ("maxexecutiontime", "max_execution_time"),
];

/// mysqlwire's accepted properties for MariaDB.
const MARIADB_TARGETS: &[(&str, &str)] = &[
    ("timezone", "time_zone"),
    ("lockwaittimeout", "lock_wait_timeout"),
    ("innodblockwaittimeout", "innodb_lock_wait_timeout"),
    ("groupconcatmaxlen", "group_concat_max_len"),
    ("maxstatementtime", "max_statement_time"),
];

/// sqlite's accepted properties.
const SQLITE_TARGETS: &[(&str, &str)] = &[
    ("busytimeout", "busy_timeout"),
    ("cachesize", "cache_size"),
    ("foreignkeys", "foreign_keys"),
];

/// Most entries read from one `options` / `sessionVariables` value.
const MAX_ENTRIES: usize = 64;
/// Longest setting name echoed in the report.
const MAX_NAME: usize = 64;
/// Longest busy timeout the sqlite driver accepts, in milliseconds.
const SQLITE_MAX_BUSY_TIMEOUT_MS: u64 = 600_000;

/// Precedence of a mapped value: a dedicated JDBC property wins over a setting read from
/// `options` / `sessionVariables` (PostgreSQL applies the startup parameters after `options`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Priority {
    /// Read from `options` (pgjdbc) or `sessionVariables` (Connector/J).
    Embedded,
    /// A dedicated JDBC property (`currentSchema`, `busy_timeout`…).
    Direct,
}

/// Why a property was not imported. Mirrored by the public
/// [`PropertyDropReason`](crate::mapping::PropertyDropReason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drop {
    NoEquivalent,
    InvalidValue,
}

/// What to do with one JDBC property.
#[derive(Debug)]
pub(crate) enum Action {
    /// Set NineSQL property `target`.
    Map {
        path: String,
        target: &'static str,
        value: String,
        priority: Priority,
    },
    /// Not imported.
    Drop {
        path: String,
        reason: Drop,
        message: String,
    },
    /// An entry that reads as a secret: removed (the caller reports it).
    Secret { path: String },
    /// A setting that makes the session read-only: honoured as the read-only flag.
    ReadOnly { path: String },
    /// An entry that may make the session read-only but cannot be read (unparsable, past the
    /// entry limit, or an unknown read-only setting): the read-only flag is set anyway (the safe
    /// direction) and the caller warns.
    ReadOnlyUnclear { path: String },
    /// Connector/J `connectionTimeZone` / `serverTimezone`: maps only with
    /// `forceConnectionTimeZoneToSession=true` (resolved once every property is read).
    ConnectionTimeZone { path: String, value: String },
    /// Connector/J `forceConnectionTimeZoneToSession`.
    ForceTimeZone { path: String, on: bool },
}

fn normalize(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// A plain setting name, safe to show and to use in a key path.
fn is_plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= MAX_NAME
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// The JDBC properties this module handles itself before the reserved-key rule: they are
/// reserved by the driver API because their value embeds arbitrary settings, which are read one
/// by one here and kept only when allowlisted.
pub(crate) fn handles_reserved(rules: Rules, key: &str) -> bool {
    match rules {
        Rules::PgJdbc | Rules::QuestDb => key.eq_ignore_ascii_case("options"),
        Rules::ConnectorJ { .. } | Rules::MariaDbJ { .. } => {
            key.eq_ignore_ascii_case("sessionVariables")
        }
        Rules::SqliteJdbc | Rules::Keep => false,
    }
}

/// Maps one property (already past the TLS, user, read-only and secret-key rules). `None` for
/// [`Rules::Keep`] (the caller keeps it).
pub(crate) fn map(rules: Rules, path: &str, key: &str, value: &str) -> Option<Vec<Action>> {
    let direct = |target: &'static str, value: String| Action::Map {
        path: path.to_owned(),
        target,
        value,
        priority: Priority::Direct,
    };
    let invalid = || Action::Drop {
        path: path.to_owned(),
        reason: Drop::InvalidValue,
        message: "not imported: the value is not one the NineSQL driver accepts".to_owned(),
    };
    let actions = match rules {
        Rules::Keep => return None,
        Rules::PgJdbc => {
            if key.eq_ignore_ascii_case("currentSchema") {
                match pg_search_path(value) {
                    Some(v) => vec![direct("search_path", v)],
                    None => vec![invalid_search_path(path)],
                }
            } else if key.eq_ignore_ascii_case("options") {
                pg_options(rules, path, value)
            } else {
                vec![no_equivalent(rules, path, key)]
            }
        }
        // `options` is still read entry by entry (read-only intent, secret-like entries); every
        // other entry and property is dropped.
        Rules::QuestDb if key.eq_ignore_ascii_case("options") => pg_options(rules, path, value),
        Rules::QuestDb => vec![no_equivalent(rules, path, key)],
        Rules::ConnectorJ { .. } | Rules::MariaDbJ { .. } => {
            // Connector/J 8 and later (`mysql8`, `tidb`): the time zone options below.
            let cj = matches!(rules, Rules::ConnectorJ { legacy: false, .. });
            if key.eq_ignore_ascii_case("sessionVariables") {
                session_variables(rules, path, value)
            } else if cj
                && (key.eq_ignore_ascii_case("connectionTimeZone")
                    || key.eq_ignore_ascii_case("serverTimezone"))
            {
                vec![Action::ConnectionTimeZone {
                    path: path.to_owned(),
                    value: value.to_owned(),
                }]
            } else if cj && key.eq_ignore_ascii_case("forceConnectionTimeZoneToSession") {
                match parse_bool(value) {
                    Some(on) => vec![Action::ForceTimeZone {
                        path: path.to_owned(),
                        on,
                    }],
                    None => vec![invalid()],
                }
            } else {
                vec![no_equivalent(rules, path, key)]
            }
        }
        Rules::SqliteJdbc => {
            let v = value.trim();
            if key.eq_ignore_ascii_case("busy_timeout") {
                match sqlite_busy_timeout(v) {
                    Some(ms) => vec![direct("busy_timeout", ms)],
                    None => vec![invalid()],
                }
            } else if key.eq_ignore_ascii_case("cache_size") {
                match v.parse::<i64>() {
                    Ok(n) if (-1_048_576..=-1).contains(&n) => {
                        vec![direct("cache_size", n.to_string())]
                    }
                    Ok(n) if n >= 0 => vec![Action::Drop {
                        path: path.to_owned(),
                        reason: Drop::InvalidValue,
                        message: "not imported: a page count; NineSQL's SQLite driver takes the \
                                  cache size in KiB only (a negative value, -1 to -1048576)"
                            .to_owned(),
                    }],
                    _ => vec![invalid()],
                }
            } else if key.eq_ignore_ascii_case("foreign_keys") {
                match parse_bool(v) {
                    Some(on) => vec![direct("foreign_keys", on.to_string())],
                    None => vec![invalid()],
                }
            } else {
                vec![no_equivalent(rules, path, key)]
            }
        }
    };
    Some(actions)
}

fn no_equivalent(rules: Rules, path: &str, key: &str) -> Action {
    let norm = normalize(key);
    let message = if norm == "connecttimeout" || norm == "logintimeout" {
        "not imported: NineSQL applies its own connect timeout".to_owned()
    } else if rules.targets().iter().any(|(n, _)| *n == norm) {
        // Named like a NineSQL property, but the JDBC driver does not read it: it had no
        // effect in DBeaver, so importing it would change the connection.
        format!(
            "not imported: not a {} property, so it had no effect in DBeaver",
            rules.jdbc_driver()
        )
    } else {
        rules.no_equivalent_message()
    };
    Action::Drop {
        path: path.to_owned(),
        reason: Drop::NoEquivalent,
        message,
    }
}

fn invalid_search_path(path: &str) -> Action {
    Action::Drop {
        path: path.to_owned(),
        reason: Drop::InvalidValue,
        message: "not imported: empty, or lists pg_catalog after another schema (NineSQL keeps \
                  pg_catalog first)"
            .to_owned(),
    }
}

/// A `search_path` value pgwire accepts: not blank, no NUL, `pg_catalog` absent or first.
fn pg_search_path(value: &str) -> Option<String> {
    if value.trim().is_empty() || value.contains('\0') {
        return None;
    }
    let schemas: Vec<String> = value
        .split(',')
        .map(|s| {
            let t = s.trim();
            match t.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
                Some(quoted) => quoted.replace("\"\"", "\""),
                None => t.to_ascii_lowercase(),
            }
        })
        .filter(|s| !s.is_empty())
        .collect();
    match schemas.iter().position(|s| s == "pg_catalog") {
        None | Some(0) => Some(value.to_owned()),
        Some(_) => None,
    }
}

/// A boolean as PostgreSQL's `parse_bool` reads it: case-insensitive unique prefixes of
/// `true`, `false`, `yes`, `no`, then `on`, `off` (at least `of`), `1`, `0`.
fn pg_bool(value: &str) -> Option<bool> {
    let v = value.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    let prefix_of = |word: &str| word.starts_with(v.as_str());
    if prefix_of("true") || prefix_of("yes") || v == "on" || v == "1" {
        Some(true)
    } else if prefix_of("false")
        || prefix_of("no")
        || (v.len() >= 2 && prefix_of("off"))
        || v == "0"
    {
        Some(false)
    } else {
        None
    }
}

/// An explicit "off" for a MySQL boolean variable (`0`, `OFF`, `FALSE`, any case, quotes
/// removed). Anything else counts as on for read-only settings (fail safe).
fn mysql_explicit_false(value: &str) -> bool {
    matches!(
        unquote(value.trim()).trim().to_ascii_lowercase().as_str(),
        "0" | "off" | "false"
    )
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "1" | "yes" => Some(true),
        "false" | "off" | "0" | "no" => Some(false),
        _ => None,
    }
}

fn sqlite_busy_timeout(v: &str) -> Option<String> {
    if v.is_empty() || v.len() > 7 || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ms: u64 = v.parse().ok()?;
    (ms <= SQLITE_MAX_BUSY_TIMEOUT_MS).then(|| ms.to_string())
}

/// Splits PostgreSQL's `options` like the server does: whitespace separates arguments, a
/// backslash escapes the next character.
fn split_pg_options(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                cur.push(next);
            }
            in_word = true;
        } else if c.is_whitespace() {
            if in_word {
                out.push(std::mem::take(&mut cur));
                in_word = false;
            }
        } else {
            cur.push(c);
            in_word = true;
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// PostgreSQL server switches that take an argument (`postmaster.c` getopt string
/// `B:bC:c:D:d:EeFf:h:ijk:lN:nOPp:r:S:sTt:W:-:`).
const PG_ARG_SWITCHES: &str = "BCcDdfhkNprStW-";

/// Reads `options` words like the server's getopt: combined flags (`-ec name=value`), an
/// argument attached or in the next word. `-c x` and `--x` give settings (`-` in the name read
/// as `_`, as the server does); every other switch or word is `None` (not a setting) and keeps
/// its text for the read-only check.
fn pg_option_settings(words: &[String]) -> Vec<Setting> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        i += 1;
        let Some(flags) = w.strip_prefix('-').filter(|f| !f.is_empty()) else {
            out.push(Setting::Unreadable(w.clone()));
            continue;
        };
        for (pos, ch) in flags.char_indices() {
            if PG_ARG_SWITCHES.contains(ch) {
                let rest = &flags[pos + ch.len_utf8()..];
                let arg = if rest.is_empty() {
                    let next = words.get(i).cloned();
                    if next.is_some() {
                        i += 1;
                    }
                    next
                } else {
                    Some(rest.to_owned())
                };
                match (ch, arg) {
                    ('c' | '-', Some(arg)) => out.push(Setting::Entry(dashes_in_name(&arg))),
                    (_, arg) => out.push(Setting::Unreadable(arg.unwrap_or_default())),
                }
                break;
            }
            // A flag without an argument: a server setting NineSQL does not import.
            out.push(Setting::Unreadable(String::new()));
        }
    }
    out
}

/// `name=value` with `-` in the name read as `_` (PostgreSQL's `ParseLongOption`).
fn dashes_in_name(arg: &str) -> String {
    match arg.split_once('=') {
        Some((name, value)) => format!("{}={value}", name.replace('-', "_")),
        None => arg.replace('-', "_"),
    }
}

/// pgjdbc `options`: `-c name=value`, `-cname=value` and `--name=value` settings. Allowlisted
/// settings are mapped; `default_transaction_read_only=on` becomes the read-only flag; anything
/// else is dropped entry by entry.
fn pg_options(rules: Rules, path: &str, value: &str) -> Vec<Action> {
    let settings = pg_option_settings(&split_pg_options(value));
    embedded_settings(rules, path, settings, |name, v| {
        let norm = normalize(name);
        if norm == "defaulttransactionreadonly" || norm == "transactionreadonly" {
            // Read-only unless explicitly false (fail safe, I3).
            return Some(Embedded::ReadOnly(pg_bool(v) != Some(false)));
        }
        let target = rules.targets().iter().find(|(n, _)| *n == norm)?.1;
        if v.contains('\0') {
            return Some(Embedded::Invalid);
        }
        if target == "search_path" {
            return Some(match pg_search_path(v) {
                Some(sp) => Embedded::Map(target, sp),
                None => Embedded::Invalid,
            });
        }
        Some(Embedded::Map(target, v.to_owned()))
    })
}

/// Splits Connector/J's `sessionVariables` on commas outside quotes and parentheses.
fn split_session_variables(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    for c in value.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    cur.push(c);
                }
                ',' if depth == 0 => out.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            },
        }
    }
    out.push(cur);
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// A value without its surrounding quotes, if it has a matching pair.
fn unquote(v: &str) -> &str {
    for q in ['\'', '"'] {
        if let Some(inner) = v.strip_prefix(q).and_then(|x| x.strip_suffix(q))
            && v.len() >= 2
        {
            return inner;
        }
    }
    v
}

/// Connector/J and MariaDB Connector/J `sessionVariables`: `name=value` pairs. Allowlisted
/// variables are mapped (values checked as mysqlwire checks them); `transaction_read_only` /
/// `tx_read_only` = on becomes the read-only flag; anything else is dropped entry by entry.
fn session_variables(rules: Rules, path: &str, value: &str) -> Vec<Action> {
    let settings = split_session_variables(value)
        .into_iter()
        .map(|s| {
            let s = s.trim();
            // `@@session.x`, `@@x`, `session.x`, `SESSION x`.
            let s = s.strip_prefix("@@").unwrap_or(s);
            let lower = s.to_ascii_lowercase();
            let s = if lower.starts_with("session.") || lower.starts_with("session ") {
                s[8..].trim_start()
            } else {
                s
            };
            Setting::Entry(s.to_owned())
        })
        .collect();
    let targets = rules.targets();
    embedded_settings(rules, path, settings, |name, v| {
        let norm = normalize(name);
        if norm == "transactionreadonly" || norm == "txreadonly" {
            // Read-only unless explicitly false (fail safe, I3).
            return Some(Embedded::ReadOnly(!mysql_explicit_false(v)));
        }
        let target = targets.iter().find(|(n, _)| *n == norm)?.1;
        let v = v.trim();
        Some(match mysql_value(target, v) {
            Some(ok) => Embedded::Map(target, ok),
            None => Embedded::Invalid,
        })
    })
}

/// A value mysqlwire accepts for `var` (its `prop_kind` checks), normalized.
fn mysql_value(var: &str, v: &str) -> Option<String> {
    match var {
        "time_zone" => {
            let inner = unquote(v);
            (!inner.is_empty() && !inner.contains('\0')).then(|| inner.to_owned())
        }
        "max_statement_time" => (!v.is_empty()
            && v.len() <= 32
            && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && v.bytes().filter(|b| *b == b'.').count() <= 1)
            .then(|| v.to_owned()),
        _ => (!v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit()))
            .then(|| v.to_owned()),
    }
}

/// Connector/J `connectionTimeZone` (or `serverTimezone`) with
/// `forceConnectionTimeZoneToSession=true` sets the session `time_zone`; the special values
/// `LOCAL` and `SERVER` depend on the machine or the server and are not imported.
pub(crate) fn connection_time_zone(value: &str) -> Option<String> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("LOCAL") || v.eq_ignore_ascii_case("SERVER") {
        return None;
    }
    mysql_value("time_zone", v)
}

enum Embedded {
    Map(&'static str, String),
    ReadOnly(bool),
    Invalid,
}

/// One entry of `options` / `sessionVariables`.
enum Setting {
    /// A `name=value` candidate.
    Entry(String),
    /// Not a setting (another server switch, a stray word); its text is kept only for the
    /// read-only check, never shown.
    Unreadable(String),
}

/// Whether a name looks like a token rather than a setting name: long, mixing upper case, lower
/// case and digits, with no `_`. Such names are never echoed (they may be misplaced secrets).
fn is_token_like(name: &str) -> bool {
    name.len() >= 16
        && !name.contains('_')
        && name.bytes().any(|b| b.is_ascii_uppercase())
        && name.bytes().any(|b| b.is_ascii_lowercase())
        && name.bytes().any(|b| b.is_ascii_digit())
}

/// Turns the settings of `options` / `sessionVariables` into actions. `classify` returns `None`
/// for a setting with no equivalent. Any entry that cannot be read or is past the entry limit,
/// and any unknown setting whose name contains "read only", sets the read-only flag (the safe
/// direction) with a warning.
fn embedded_settings(
    rules: Rules,
    path: &str,
    settings: Vec<Setting>,
    classify: impl Fn(&str, &str) -> Option<Embedded>,
) -> Vec<Action> {
    let mut out = Vec::new();
    let mut unreadable = 0usize;
    let mut unclear_read_only = false;
    let total = settings.len();
    for (index, setting) in settings.into_iter().enumerate() {
        let text = match &setting {
            Setting::Entry(t) | Setting::Unreadable(t) => t,
        };
        let mentions_read_only = normalize(text).contains("readonly");
        if index >= MAX_ENTRIES {
            unclear_read_only |= mentions_read_only;
            continue;
        }
        let parsed = match &setting {
            Setting::Entry(t) => t
                .split_once('=')
                .map(|(n, v)| (n.trim(), v))
                .filter(|(n, v)| {
                    !v.trim().is_empty()
                        && !v.starts_with('=')
                        && is_plain_name(n)
                        && !is_token_like(n)
                }),
            Setting::Unreadable(_) => None,
        };
        let Some((name, v)) = parsed else {
            unreadable += 1;
            unclear_read_only |= mentions_read_only;
            continue;
        };
        let sub = format!("{path}.{name}");
        if is_secret_like_key(name) {
            out.push(Action::Secret { path: sub });
            continue;
        }
        match classify(name, v) {
            Some(Embedded::Map(target, value)) => out.push(Action::Map {
                path: sub,
                target,
                value,
                priority: Priority::Embedded,
            }),
            Some(Embedded::ReadOnly(true)) => out.push(Action::ReadOnly { path: sub }),
            Some(Embedded::ReadOnly(false)) => out.push(Action::Drop {
                path: sub,
                reason: Drop::NoEquivalent,
                message: "not imported: it explicitly turns read-only off; the read-only mode \
                          is a typed NineSQL setting of the connection"
                    .to_owned(),
            }),
            Some(Embedded::Invalid) => out.push(Action::Drop {
                path: sub,
                reason: Drop::InvalidValue,
                message: "not imported: the value is not one the NineSQL driver accepts".to_owned(),
            }),
            None if normalize(name).contains("readonly") => {
                out.push(Action::ReadOnlyUnclear { path: sub });
            }
            None => out.push(Action::Drop {
                path: sub,
                reason: Drop::NoEquivalent,
                message: rules.no_equivalent_message(),
            }),
        }
    }
    let skipped = total.saturating_sub(MAX_ENTRIES);
    if unreadable + skipped > 0 {
        let n = unreadable + skipped;
        out.push(Action::Drop {
            path: path.to_owned(),
            reason: Drop::InvalidValue,
            message: format!(
                "{n} entr{} that {} not a plain `name=value` setting (or past the first \
                 {MAX_ENTRIES}) not imported",
                if n == 1 { "y" } else { "ies" },
                if n == 1 { "is" } else { "are" },
            ),
        });
    }
    if unclear_read_only {
        out.push(Action::ReadOnlyUnclear {
            path: path.to_owned(),
        });
    }
    if out.is_empty() {
        out.push(Action::Drop {
            path: path.to_owned(),
            reason: Drop::NoEquivalent,
            message: "not imported: empty".to_owned(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(actions: &[Action]) -> Vec<(&str, &str)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Map { target, value, .. } => Some((*target, value.as_str())),
                _ => None,
            })
            .collect()
    }

    fn dropped(actions: &[Action]) -> Vec<&str> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Drop { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pg_options_parsing() {
        assert_eq!(
            split_pg_options(
                r"-c search_path=a,b  -cstatement_timeout=5s --lock_timeout=1s -c x=a\ b"
            ),
            [
                "-c",
                "search_path=a,b",
                "-cstatement_timeout=5s",
                "--lock_timeout=1s",
                "-c",
                "x=a b"
            ]
        );
        let a = pg_options(
            Rules::PgJdbc,
            "p.options",
            "-c search_path=app,public -cstatement_timeout=5000 --work_mem=64MB -c geqo=off -B 8",
        );
        assert_eq!(
            targets(&a),
            [
                ("search_path", "app,public"),
                ("statement_timeout", "5000"),
                ("work_mem", "64MB")
            ]
        );
        assert_eq!(dropped(&a), ["p.options.geqo", "p.options"]);
        // pg_catalog after another schema: refused by pgwire, so dropped here.
        let a = pg_options(Rules::PgJdbc, "p", "-c search_path=app,pg_catalog");
        assert!(targets(&a).is_empty());
        assert_eq!(dropped(&a), ["p.search_path"]);
        // Read-only on is honoured, a password-like setting is a secret.
        let a = pg_options(
            Rules::PgJdbc,
            "p",
            "-c default_transaction_read_only=on -c password=x",
        );
        assert!(matches!(a[0], Action::ReadOnly { .. }));
        assert!(matches!(a[1], Action::Secret { .. }));
        // Nothing usable.
        assert_eq!(dropped(&pg_options(Rules::PgJdbc, "p", "  ")), ["p"]);
    }

    #[test]
    fn pg_booleans_and_switches() {
        for (v, want) in [
            ("t", Some(true)),
            ("TRU", Some(true)),
            ("y", Some(true)),
            ("on", Some(true)),
            ("1", Some(true)),
            ("f", Some(false)),
            ("n", Some(false)),
            ("of", Some(false)),
            ("0", Some(false)),
            ("o", None),
            ("", None),
            ("truex", None),
        ] {
            assert_eq!(pg_bool(v), want, "{v}");
        }
        let words: Vec<String> = ["-ec", "a-b=1", "-B", "8", "--c-d=2", "x", "-Ecname=3"]
            .iter()
            .map(|w| (*w).to_owned())
            .collect();
        let entries: Vec<String> = pg_option_settings(&words)
            .into_iter()
            .filter_map(|s| match s {
                Setting::Entry(e) => Some(e),
                Setting::Unreadable(_) => None,
            })
            .collect();
        assert_eq!(entries, ["a_b=1", "c_d=2", "name=3"]);
        assert!(is_token_like("c2VjcmV0VG9rZW4xMjM0NTY3OA"));
        assert!(!is_token_like("default_transaction_read_only"));
        assert!(!is_token_like("innodbLockWaitTimeout"));
    }

    #[test]
    fn session_variables_parsing() {
        assert_eq!(
            split_session_variables("a=1,b='x,y',c=f(1,2), ,d=\"q\""),
            ["a=1", "b='x,y'", "c=f(1,2)", "d=\"q\""]
        );
        let rules = Rules::ConnectorJ {
            mariadb: false,
            legacy: false,
        };
        let a = session_variables(
            rules,
            "p",
            "time_zone='+00:00',@@session.lock_wait_timeout=10,group_concat_max_len=x,\
             max_statement_time=1,sql_mode='ANSI',tx_read_only=1",
        );
        assert_eq!(
            targets(&a),
            [("time_zone", "+00:00"), ("lock_wait_timeout", "10")]
        );
        assert_eq!(
            dropped(&a),
            [
                "p.group_concat_max_len",
                "p.max_statement_time",
                "p.sql_mode"
            ]
        );
        assert!(matches!(a.last(), Some(Action::ReadOnly { .. })));
        let a = session_variables(
            Rules::MariaDbJ { mariadb: true },
            "p",
            "max_statement_time=1.5,max_execution_time=10",
        );
        assert_eq!(targets(&a), [("max_statement_time", "1.5")]);
        assert_eq!(dropped(&a), ["p.max_execution_time"]);
    }

    #[test]
    fn sqlite_values() {
        let m = |k: &str, v: &str| map(Rules::SqliteJdbc, "p", k, v).unwrap_or_default();
        assert_eq!(
            targets(&m("busy_timeout", "3000")),
            [("busy_timeout", "3000")]
        );
        assert_eq!(dropped(&m("busy_timeout", "600001")), ["p"]);
        assert_eq!(
            targets(&m("cache_size", "-2000")),
            [("cache_size", "-2000")]
        );
        assert_eq!(dropped(&m("cache_size", "2000")), ["p"]);
        assert_eq!(
            targets(&m("foreign_keys", "ON")),
            [("foreign_keys", "true")]
        );
        assert_eq!(dropped(&m("foreign_keys", "maybe")), ["p"]);
        assert_eq!(dropped(&m("enable_load_extension", "true")), ["p"]);
    }

    #[test]
    fn rules_by_driver() {
        assert_eq!(
            Rules::for_connection(&EngineId::MARIADB, "mariaDB"),
            Rules::MariaDbJ { mariadb: true }
        );
        assert_eq!(
            Rules::for_connection(&EngineId::MYSQL, "mysql8"),
            Rules::ConnectorJ {
                mariadb: false,
                legacy: false
            }
        );
        assert_eq!(
            Rules::for_connection(&EngineId::MYSQL, "mysql5"),
            Rules::ConnectorJ {
                mariadb: false,
                legacy: true
            }
        );
        assert_eq!(
            Rules::for_connection(&EngineId::TIMESCALEDB, "x"),
            Rules::PgJdbc
        );
        assert_eq!(
            Rules::for_connection(&EngineId::YUGABYTEDB, "postgres-yugabytedb-jdbc"),
            Rules::PgJdbc
        );
        assert_eq!(
            Rules::for_connection(&EngineId::QUESTDB, "questdb"),
            Rules::QuestDb
        );
        assert_eq!(
            Rules::for_connection(&EngineId::TIDB, "tidb"),
            Rules::ConnectorJ {
                mariadb: false,
                legacy: false
            }
        );
        assert_eq!(
            Rules::for_connection(&EngineId::CLICKHOUSE, "com_clickhouse"),
            Rules::Keep
        );
    }
}
