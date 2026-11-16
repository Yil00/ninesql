//! Export of NineSQL connections to a new DBeaver `data-sources.json` (ROADMAP P5-D, F-CON-15,
//! ADR-0006 §7, docs/06-dbeaver-interop.md "Export to DBeaver").
//!
//! [`export_connections`] turns connections into the bytes of a `data-sources.json`, with no I/O;
//! [`write_export`] writes them to a **new** file or project folder
//! the user chose (never into an existing DBeaver workspace, I4).
//!
//! # Rules
//! - **I1**: the input holds no secret value ([`ExportConnection`]); nothing is ever written to a
//!   `credentials-config.json`. Driver properties, init statements and descriptions that look
//!   like they hold a secret are dropped, and the generated document is checked once more with
//!   `ninesql-storage`'s secret-key detector before it is returned.
//! - **Never weaker** (I3, I5): an effective read-only connection (its own flag or its type's
//!   default) is written `read-only`; TLS is written explicitly for the engines whose DBeaver TLS
//!   settings NineSQL's importer reads back, in a form that re-imports to the same mode; a
//!   production type's manual commit is never turned off; a connection whose route NineSQL could
//!   not reproduce (`DisabledKind::Route`) is not exported.
//! - **Nothing dropped silently**: every setting not written, or written approximately, has an
//!   entry in the [`ExportReport`].
//!
//! The written format is the subset of keys common to the five P0-E fixture versions (DBeaver
//! CE 22.3.5 to 26.2.4), plus a few keys taken from DBeaver's source and marked as such in
//! docs/06; `tests/export_format.rs` checks it against the fixtures.

mod model;
mod write;

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::Path;

use ninesql_driver_api::dialect::classify_with_rules;
use ninesql_driver_api::hygiene::text_may_hold_credentials;
use ninesql_driver_api::{EngineId, LexicalRules, StatementClass};
use ninesql_storage::hygiene::{find_secret_like_keys, is_secret_like_entry, is_secret_like_key};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

pub use model::{
    ConnectionExport, Export, ExportAuth, ExportClientCertificate, ExportConnection,
    ExportConnectionType, ExportDisabled, ExportEndpoint, ExportHostKeyPolicy, ExportIssue,
    ExportIssueKind, ExportOptions, ExportReport, ExportSshAuth, ExportSshHop, ExportSshTunnel,
    ExportTls, Rgb, Severity, TlsMode,
};
pub use write::{ExportTarget, ExportWriteError, WrittenExport, write_export};

use model::Issues;

/// Why an export could not be generated.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExportError {
    /// The generated document failed the final secret check (a bug: nothing is returned).
    #[error("the generated file failed the secret check and was not produced")]
    SecretCheck,
    /// JSON serialization failed.
    #[error("the DBeaver file could not be generated")]
    Serialize,
}

/// Generates a DBeaver `data-sources.json` for `connections`. No I/O: write the result with
/// [`write_export`]. Connections that cannot be exported are skipped and reported; the call
/// fails only on an internal error.
///
/// # Errors
/// [`ExportError`]: serialization failure or a failed final secret check.
pub fn export_connections(
    connections: &[ExportConnection],
    options: &ExportOptions,
) -> Result<Export, ExportError> {
    let mut types: BTreeMap<&str, &ExportConnectionType> = BTreeMap::new();
    let builtins = ExportConnectionType::builtins();
    for t in &options.connection_types {
        types.entry(t.id.as_str()).or_insert(t);
    }
    for t in &builtins {
        types.entry(t.id.as_str()).or_insert(t);
    }
    let millis = options.timestamp_millis.unwrap_or_else(now_millis);
    let random = std::hash::RandomState::new();

    let mut report = ExportReport::default();
    let mut used_ids = BTreeSet::new();
    let mut used_types: BTreeSet<&str> = BTreeSet::new();
    let mut folders: BTreeSet<String> = BTreeSet::new();
    let mut out_connections: Vec<(String, J)> = Vec::new();
    for (index, conn) in connections.iter().enumerate() {
        let mut issues = Issues::default();
        let ty = types.get(conn.connection_type.as_str()).copied();
        let result = ty
            .ok_or("its connection type is not known, so its read-only default is not either")
            .and_then(|ty| export_one(conn, ty, &mut issues));
        let dbeaver_id = match result {
            Ok(written)
                if serde_json::to_value(written.json.as_ref())
                    .map_or(true, |v| !find_secret_like_keys(&v).is_empty()) =>
            {
                // A user-chosen key (property, folder…) looks like it names a secret: this
                // connection is skipped, the rest of the export goes on.
                issues.push(
                    Severity::Error,
                    ExportIssueKind::Skipped,
                    None,
                    "not exported: one of its setting names looks like it holds a secret",
                );
                None
            }
            Ok(mut written) => {
                let id = unique_id(
                    written.driver,
                    millis,
                    &random,
                    (index, &conn.id),
                    &mut used_ids,
                );
                if let Some(path) = &written.folder {
                    let mut prefix = String::new();
                    for segment in path.split('/') {
                        if !prefix.is_empty() {
                            prefix.push('/');
                        }
                        prefix.push_str(segment);
                        folders.insert(prefix.clone());
                    }
                }
                used_types.insert(conn.connection_type.as_str());
                out_connections.push((id.clone(), written.take_json()));
                Some(id)
            }
            Err(reason) => {
                issues.push(
                    Severity::Error,
                    ExportIssueKind::Skipped,
                    None,
                    format!("not exported: {reason}"),
                );
                None
            }
        };
        report.connections.push(ConnectionExport {
            id: conn.id.clone(),
            name: conn.name.clone(),
            dbeaver_id,
            issues: issues.0,
        });
    }

    let mut global = Issues::default();
    let mut out_types = Vec::new();
    for id in used_types {
        if let Some(ty) = types.get(id) {
            out_types.push((dbeaver_type_id(id), connection_type_json(ty, &mut global)));
        }
    }
    report.issues = global.0;

    let doc = J::Obj(vec![
        (
            "folders".to_owned(),
            J::Obj(
                folders
                    .into_iter()
                    .map(|f| (f, J::Obj(Vec::new())))
                    .collect(),
            ),
        ),
        ("connections".to_owned(), J::Obj(out_connections)),
        ("connection-types".to_owned(), J::Obj(out_types)),
    ]);
    let value = serde_json::to_value(&doc).map_err(|_| ExportError::Serialize)?;
    if !find_secret_like_keys(&value).is_empty() {
        return Err(ExportError::SecretCheck);
    }
    let mut json = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut ser = serde_json::Serializer::with_formatter(&mut json, formatter);
    doc.serialize(&mut ser)
        .map_err(|_| ExportError::Serialize)?;
    json.push(b'\n');
    tracing::debug!(
        connections = report.exported_count(),
        skipped = report.connections.len() - report.exported_count(),
        "DBeaver export generated"
    );
    Ok(Export { json, report })
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A DBeaver-style data source id, `<driver>-<hex millis>-<hex random>` (DBeaver's
/// `DataSourceDescriptor.generateNewId`), unique in the file.
fn unique_id(
    driver: &str,
    millis: u64,
    random: &std::hash::RandomState,
    seed: (usize, &str),
    used: &mut BTreeSet<String>,
) -> String {
    let mut attempt = 0u64;
    loop {
        let mut h = random.build_hasher();
        seed.hash(&mut h);
        attempt.hash(&mut h);
        let id = format!("{driver}-{millis:x}-{:x}", h.finish());
        if used.insert(id.clone()) {
            return id;
        }
        attempt += 1;
    }
}

// ---------------------------------------------------------------------------------------------
// Ordered JSON
// ---------------------------------------------------------------------------------------------

/// A JSON value that keeps the key order (DBeaver's layout), without `serde_json`'s
/// `preserve_order` feature.
#[derive(Debug, Clone)]
enum J {
    Str(String),
    Bool(bool),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    fn s(text: impl Into<String>) -> Self {
        Self::Str(text.into())
    }
}

impl Serialize for J {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Str(v) => s.serialize_str(v),
            Self::Bool(b) => s.serialize_bool(*b),
            Self::Arr(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Self::Obj(entries) => {
                let mut map = s.serialize_map(Some(entries.len()))?;
                for (k, v) in entries {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Engines
// ---------------------------------------------------------------------------------------------

/// The NineSQL driver family whose DBeaver settings the export writes (and the importer reads
/// back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// pgjdbc: PostgreSQL, TimescaleDB, YugabyteDB (and QuestDB through it, approximated).
    Pg,
    /// QuestDB: PostgreSQL driver in DBeaver, no driver property in NineSQL.
    QuestDb,
    /// MySQL Connector/J: MySQL, TiDB.
    MySql,
    /// MariaDB Connector/J.
    MariaDb,
    /// sqlite-jdbc.
    Sqlite,
    /// An engine without a NineSQL driver yet: properties kept as DBeaver stored them.
    Other,
}

struct EngineOut {
    provider: &'static str,
    driver: &'static str,
    family: Family,
    file: bool,
    /// Covered by a P0-E fixture (the import mapping is *verified*).
    verified: bool,
}

/// NineSQL engine → DBeaver `provider` / `driver`: the reverse of the import table
/// (docs/06 "Engine mapping"), using the driver DBeaver writes today for each engine.
fn engine_out(engine: &EngineId) -> Option<EngineOut> {
    let e = |provider, driver, family, file, verified| EngineOut {
        provider,
        driver,
        family,
        file,
        verified,
    };
    Some(match engine.as_str() {
        "postgresql" => e("postgresql", "postgres-jdbc", Family::Pg, false, true),
        "timescaledb" => e(
            "postgresql",
            "postgres-timescale-jdbc",
            Family::Pg,
            false,
            true,
        ),
        "yugabytedb" => e(
            "postgresql",
            "postgres-yugabytedb-jdbc",
            Family::Pg,
            false,
            false,
        ),
        "questdb" => e("postgresql", "postgres-jdbc", Family::QuestDb, false, true),
        "mysql" => e("mysql", "mysql8", Family::MySql, false, true),
        "mariadb" => e("mysql", "mariaDB", Family::MariaDb, false, true),
        "tidb" => e("tidb", "tidb", Family::MySql, false, false),
        "sqlite" => e("sqlite", "sqlite_jdbc", Family::Sqlite, true, true),
        "duckdb" => e("duckdb", "duckdb_jdbc", Family::Other, true, false),
        "clickhouse" => e("clickhouse", "com_clickhouse", Family::Other, false, true),
        "trino" => e("generic", "trino_jdbc", Family::Other, false, false),
        "firebird" => e("jaybird", "jaybird", Family::Other, false, false),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// One connection
// ---------------------------------------------------------------------------------------------

struct Written {
    driver: &'static str,
    folder: Option<String>,
    json: Option<J>,
}

impl Written {
    fn take_json(&mut self) -> J {
        self.json.take().unwrap_or(J::Obj(Vec::new()))
    }
}

/// A path as UTF-8 text, or `None` (JSON cannot hold other bytes; DBeaver is Java).
fn path_text(path: &Path) -> Option<String> {
    path.to_str().map(str::to_owned).filter(|p| !p.is_empty())
}

#[allow(clippy::too_many_lines)] // One linear pass over the connection's fields, in file order.
fn export_one(
    conn: &ExportConnection,
    ty: &ExportConnectionType,
    issues: &mut Issues,
) -> Result<Written, &'static str> {
    if conn.disabled == Some(ExportDisabled::Route) {
        return Err(
            "NineSQL disabled it because the route DBeaver used (network profile, proxy, other \
             SSH forwarding target) could not be reproduced; exporting it would let DBeaver \
             connect without that route",
        );
    }
    let engine = engine_out(&conn.engine).ok_or("no DBeaver driver is known for its engine")?;
    if !engine.verified {
        issues.info(
            ExportIssueKind::NotVerified,
            "engine",
            format!(
                "written with DBeaver driver `{}` / `{}`, taken from DBeaver's source: not \
                 verified against a DBeaver-written workspace",
                engine.provider, engine.driver
            ),
        );
    }
    if engine.family == Family::QuestDb {
        issues.warn(
            ExportIssueKind::Approximated,
            "engine",
            "DBeaver Community has no QuestDB driver: written with the PostgreSQL driver; \
             importing the file back into NineSQL gives a PostgreSQL connection",
        );
    }
    match conn.disabled {
        Some(ExportDisabled::ByUser | ExportDisabled::NoDriver | ExportDisabled::Invalid) => {
            issues.info(
                ExportIssueKind::Dropped,
                "enabled",
                "the connection is disabled in NineSQL; DBeaver has no disabled state, so it is \
                 exported as a normal connection",
            );
        }
        Some(ExportDisabled::Route) | None => {}
    }

    let mut c: Vec<(String, J)> = vec![
        ("provider".to_owned(), J::s(engine.provider)),
        ("driver".to_owned(), J::s(engine.driver)),
        ("name".to_owned(), J::s(&conn.name)),
    ];
    if let Some(d) = conn.description.as_deref().filter(|d| !d.trim().is_empty()) {
        if text_may_hold_credentials(d) {
            issues.warn(
                ExportIssueKind::LooksLikeSecret,
                "description",
                "the description mentions a credential: not exported",
            );
        } else {
            c.push(("description".to_owned(), J::s(d)));
        }
    }
    // Never weaker (I3): the type's read-only default is folded into the connection flag.
    let read_only = conn.read_only.unwrap_or(ty.read_only);
    if read_only {
        c.push(("read-only".to_owned(), J::Bool(true)));
        issues.warn(
            ExportIssueKind::Approximated,
            "read_only",
            "DBeaver enforces read-only in its JDBC driver and its UI (`setReadOnly`, editors), \
             not with an engine-side read-only transaction re-checked after every statement as \
             NineSQL does: a statement can still write on some engines",
        );
        if conn.read_only.is_none() {
            issues.info(
                ExportIssueKind::Approximated,
                "read_only",
                "read-only through the connection type: written as the connection's read-only \
                 flag (DBeaver connection types have no read-only setting)",
            );
        }
    }
    let folder = folder_path(&conn.folder, issues);
    if let Some(f) = &folder {
        c.push(("folder".to_owned(), J::s(f)));
    }

    // Configuration.
    let mut cfg: Vec<(String, J)> = Vec::new();
    match &conn.endpoint {
        ExportEndpoint::Tcp {
            host,
            port,
            database,
        } => {
            if engine.file {
                return Err("its engine uses a database file but the connection has a host");
            }
            check_url_safe_host(host)?;
            cfg.push(("host".to_owned(), J::s(host)));
            if let Some(p) = port {
                cfg.push(("port".to_owned(), J::s(p.to_string())));
            }
            if let Some(db) = database.as_deref().filter(|d| !d.is_empty()) {
                if !url_safe_database(db) {
                    return Err(URL_UNSAFE_DATABASE);
                }
                cfg.push(("database".to_owned(), J::s(db)));
            }
        }
        ExportEndpoint::UnixSocket { .. } => {
            return Err("DBeaver has no generic setting for a Unix socket endpoint");
        }
        ExportEndpoint::File { path } => {
            if !engine.file {
                return Err("its engine needs a host but the connection has a database file");
            }
            let p = path_text(path).ok_or("the database file path is not valid UTF-8")?;
            if !path.is_absolute() {
                return Err("the database file path is not absolute");
            }
            if !url_safe_database(&p) {
                return Err(URL_UNSAFE_DATABASE);
            }
            cfg.push(("database".to_owned(), J::s(p)));
        }
    }
    cfg.push(("configurationType".to_owned(), J::s("MANUAL")));
    cfg.push(("type".to_owned(), J::s(dbeaver_type_id(&ty.id))));

    // Authentication: the user name only, never a secret.
    let username = match &conn.auth {
        ExportAuth::None => None,
        ExportAuth::Username { username } => {
            issues.info(
                ExportIssueKind::Approximated,
                "auth",
                "user name without a password: DBeaver asks for a password at connect (leave \
                 it empty); NineSQL imports it back as user name and password",
            );
            Some(username.as_str())
        }
        ExportAuth::Password { username, saved } => {
            if *saved {
                issues.info(
                    ExportIssueKind::SecretOmitted,
                    "auth.password",
                    "the saved password is not exported: DBeaver asks for it",
                );
            }
            if username.is_empty() {
                issues.info(
                    ExportIssueKind::Approximated,
                    "auth.username",
                    "password without a user name: DBeaver asks for the credentials; NineSQL \
                     imports it back without credentials",
                );
            }
            Some(username.as_str())
        }
        ExportAuth::Token { username, saved } => {
            issues.warn(
                ExportIssueKind::Approximated,
                "auth",
                "token authentication has no DBeaver equivalent in the native authentication \
                 model: the user name is written, the token is not",
            );
            if *saved {
                issues.info(
                    ExportIssueKind::SecretOmitted,
                    "auth.token",
                    "the saved token is not exported",
                );
            }
            username.as_deref()
        }
    };
    if engine.file && username.is_some_and(|u| !u.is_empty()) {
        issues.info(
            ExportIssueKind::Dropped,
            "auth.username",
            "a file database has no user name in DBeaver: not exported",
        );
    } else if let Some(u) = username.filter(|u| !u.is_empty()) {
        cfg.push(("user".to_owned(), J::s(u)));
        issues.info(
            ExportIssueKind::NotVerified,
            "auth.username",
            "written as `configuration.user` (DBeaver keeps it in credentials-config.json, \
             which NineSQL never writes); DBeaver's reading of this key is not verified against \
             a DBeaver-written workspace",
        );
    }
    if conn.scram_only {
        issues.warn(
            ExportIssueKind::Dropped,
            "password_exchange",
            "\"SCRAM only\" has no DBeaver equivalent: not exported",
        );
    }

    // Driver properties and TLS settings expressed as driver properties.
    let mut props = export_properties(engine.family, &conn.engine, &conn.properties, issues);
    if read_only && matches!(engine.family, Family::Pg | Family::QuestDb) {
        // pgjdbc: `setReadOnly(true)` also in auto-commit mode (its default, `transaction`,
        // only applies it inside explicit transactions).
        props.insert("readOnlyMode".to_owned(), "always".to_owned());
    }
    let mut handlers: Vec<(String, J)> = Vec::new();
    if matches!(conn.endpoint, ExportEndpoint::Tcp { .. }) {
        export_tls(engine.family, &conn.tls, &mut props, &mut handlers, issues);
    }
    if !props.is_empty() {
        cfg.push((
            "properties".to_owned(),
            J::Obj(props.into_iter().map(|(k, v)| (k, J::Str(v))).collect()),
        ));
    }

    // Bootstrap.
    let mut boot: Vec<(String, J)> = Vec::new();
    if let Some(s) = conn
        .default_schema
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        boot.push(("defaultSchema".to_owned(), J::s(s)));
    }
    if let Some(s) = conn
        .default_catalog
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        boot.push(("defaultCatalog".to_owned(), J::s(s)));
        issues.info(
            ExportIssueKind::NotVerified,
            "default_catalog",
            "written as `bootstrap.defaultCatalog`, taken from DBeaver's source (no fixture)",
        );
    }
    let mut queries = Vec::new();
    let mut secret_queries = 0usize;
    let mut writing_queries = 0usize;
    let init_sql: &[String] =
        if conn.init_sql_needs_confirmation && conn.init_sql.iter().any(|q| !q.trim().is_empty()) {
            issues.warn(
                ExportIssueKind::Dropped,
                "init_sql",
                "the init statements came from an import and are not confirmed yet: not exported \
             (confirm them by opening the connection in NineSQL, then export again)",
            );
            &[]
        } else {
            &conn.init_sql
        };
    for q in init_sql.iter().filter(|q| !q.trim().is_empty()) {
        if text_may_hold_credentials(q) {
            secret_queries += 1;
        } else if read_only && !read_only_safe(q, engine.family) {
            writing_queries += 1;
        } else {
            queries.push(J::s(q));
        }
    }
    if writing_queries > 0 {
        issues.warn(
            ExportIssueKind::Dropped,
            "init_sql",
            format!(
                "{writing_queries} init statement(s) of this read-only connection are not plain \
                 reads (or touch the access mode) and are not exported"
            ),
        );
    }
    if secret_queries > 0 {
        issues.warn(
            ExportIssueKind::LooksLikeSecret,
            "init_sql",
            format!("{secret_queries} init statement(s) mention a credential and are not exported"),
        );
    }
    if !queries.is_empty() {
        boot.push(("query".to_owned(), J::Arr(queries)));
    }
    match conn.auto_commit {
        Some(true) if ty.is_production() => issues.warn(
            ExportIssueKind::Dropped,
            "auto_commit",
            "auto-commit on is never applied to a production connection type in NineSQL: not \
             exported, so DBeaver keeps the type's manual commit",
        ),
        Some(auto) => boot.push(("autocommit".to_owned(), J::Bool(auto))),
        None => {}
    }
    if !boot.is_empty() {
        cfg.push(("bootstrap".to_owned(), J::Obj(boot)));
    }

    // SSH.
    if let Some(ssh) = &conn.ssh {
        let tunnel = ssh_handler(ssh, issues)?;
        handlers.insert(0, ("ssh_tunnel".to_owned(), tunnel));
    }
    if !handlers.is_empty() {
        cfg.push(("handlers".to_owned(), J::Obj(handlers)));
    }
    c.push(("configuration".to_owned(), J::Obj(cfg)));
    Ok(Written {
        driver: engine.driver,
        folder,
        json: Some(J::Obj(c)),
    })
}

/// DBeaver folder path (`Team A/Production`). A `/` inside a NineSQL folder name would split
/// it, so it becomes `∕` (U+2215), reported.
fn folder_path(folder: &[String], issues: &mut Issues) -> Option<String> {
    let mut changed = false;
    let parts: Vec<String> = folder
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            if s.contains('/') {
                changed = true;
                s.replace('/', "\u{2215}")
            } else {
                s.clone()
            }
        })
        .collect();
    if changed {
        issues.info(
            ExportIssueKind::Approximated,
            "folder",
            "a folder name contains `/`, which separates folders in DBeaver: written with `∕` \
             (U+2215) instead",
        );
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

// ---------------------------------------------------------------------------------------------
// Values DBeaver pastes into a JDBC URL
// ---------------------------------------------------------------------------------------------

const URL_UNSAFE_DATABASE: &str = "the database name or file path contains `?`, `&`, `;`, `#` \
     or `%`, which DBeaver would paste into the JDBC URL as parameters (they could override \
     the TLS and read-only settings)";

/// DBeaver builds the JDBC URL from a template (`jdbc:postgresql://{host}[:{port}]/{database}`)
/// without escaping: a host or database holding URL syntax would add parameters that win over
/// the TLS handler and read-only (pgjdbc and MariaDB Connector/J: URL parameters win).
fn check_url_safe_host(host: &str) -> Result<(), &'static str> {
    if host.contains(':') {
        // An IPv6 address: whether DBeaver brackets it in the URL is not verified (L5).
        return Err(
            "IPv6 address hosts are not exported (DBeaver's URL bracketing is not \
                    verified); use a host name",
        );
    }
    if host
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || "?&;#/\\%".contains(c))
    {
        return Err(
            "the host contains a character DBeaver would paste into the JDBC URL as \
                    syntax (`? & ; # / \\ %` or whitespace)",
        );
    }
    Ok(())
}

fn url_safe_database(db: &str) -> bool {
    !db.chars().any(|c| c.is_control() || "?&;#%".contains(c))
}

/// Init statements kept on a read-only connection: plain reads (classifier `Read`) that do not
/// touch the access mode (`SET … TRANSACTION … WRITE`, `read_only`, `query_only`).
fn read_only_safe(sql: &str, family: Family) -> bool {
    let rules = match family {
        Family::Pg | Family::QuestDb => LexicalRules::POSTGRES,
        Family::MySql | Family::MariaDb => LexicalRules::MYSQL,
        Family::Sqlite => LexicalRules::SQLITE,
        Family::Other => LexicalRules::ANSI,
    };
    if classify_with_rules(sql, rules) != StatementClass::Read {
        return false;
    }
    let norm: String = sql
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    !(norm.contains("readonly")
        || norm.contains("queryonly")
        || (norm.contains("transaction") && norm.contains("write")))
}

/// DBeaver id of a connection type: built-ins keep theirs, custom types get a `ninesql-` prefix
/// so that they do not collide with a type the user already has in DBeaver (DBeaver keeps an
/// existing type of the same id). At most 64 bytes, so NineSQL imports it back unchanged.
fn dbeaver_type_id(id: &str) -> String {
    const PREFIX: &str = "ninesql-";
    if matches!(id, "dev" | "test" | "prod") || id.starts_with(PREFIX) {
        return id.to_owned();
    }
    let mut base = id.to_owned();
    let max = 64 - PREFIX.len();
    while base.len() > max {
        base.pop();
    }
    format!("{PREFIX}{base}")
}

// ---------------------------------------------------------------------------------------------
// Connection types
// ---------------------------------------------------------------------------------------------

fn connection_type_json(ty: &ExportConnectionType, issues: &mut Issues) -> J {
    let field = format!("connection_types.{}", ty.id);
    let production = ty.is_production();
    if !matches!(ty.id.as_str(), "dev" | "test" | "prod") {
        issues.warn(
            ExportIssueKind::Approximated,
            &field,
            format!(
                "written as the DBeaver type `{}`; if the DBeaver workspace already has a type \
                 of that id, DBeaver keeps its own settings, not these",
                dbeaver_type_id(&ty.id)
            ),
        );
    } else {
        issues.info(
            ExportIssueKind::NotVerified,
            &field,
            "a DBeaver built-in type: DBeaver may keep its own settings for it (not verified)",
        );
    }
    let confirm_execute = ty.confirm_every_write;
    let confirm_change = ty.confirm_destructive || ty.confirm_every_write || production;
    if production && ty.id != "prod" {
        issues.warn(
            ExportIssueKind::Approximated,
            &field,
            "DBeaver has no production flag for a custom connection type: written with \
             confirmation of data changes and the type's commit mode; NineSQL imports it back \
             without production rules",
        );
    }
    if ty.read_only {
        issues.info(
            ExportIssueKind::Approximated,
            &field,
            "read-only default: DBeaver connection types have none, so each connection of this \
             type is written read-only",
        );
    }
    if !ty.log_query_text && !production {
        issues.info(
            ExportIssueKind::Dropped,
            &field,
            "\"do not log query text\" has no DBeaver equivalent: not exported",
        );
    }
    let description = match ty.id.as_str() {
        "dev" => "Regular development database",
        "test" => "Test (QA) database",
        "prod" => "Production database",
        _ => "Exported from NineSQL",
    };
    J::Obj(vec![
        ("name".to_owned(), J::s(&ty.name)),
        (
            "color".to_owned(),
            J::s(format!("{},{},{}", ty.color.r, ty.color.g, ty.color.b)),
        ),
        ("description".to_owned(), J::s(description)),
        ("auto-commit".to_owned(), J::Bool(ty.auto_commit)),
        ("confirm-execute".to_owned(), J::Bool(confirm_execute)),
        ("confirm-data-change".to_owned(), J::Bool(confirm_change)),
    ])
}

// ---------------------------------------------------------------------------------------------
// Driver properties
// ---------------------------------------------------------------------------------------------

/// pgwire property → `options` setting (pgjdbc passes `options` as the startup parameter).
const PG_OPTIONS: &[&str] = &[
    "TimeZone",
    "statement_timeout",
    "lock_timeout",
    "work_mem",
    "client_min_messages",
];
const MYSQL_VARIABLES: &[&str] = &[
    "time_zone",
    "lock_wait_timeout",
    "innodb_lock_wait_timeout",
    "group_concat_max_len",
    "max_execution_time",
];
const MARIADB_VARIABLES: &[&str] = &[
    "time_zone",
    "lock_wait_timeout",
    "innodb_lock_wait_timeout",
    "group_concat_max_len",
    "max_statement_time",
];
const SQLITE_PROPERTIES: &[&str] = &["busy_timeout", "cache_size", "foreign_keys"];

/// Escapes a value for PostgreSQL's `options` splitting: whitespace and `\` get a backslash.
fn pg_option_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_whitespace() || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// NineSQL driver properties → JDBC properties of the DBeaver driver (the reverse of
/// docs/06 "Driver properties"). Anything without an equivalent is dropped and reported.
fn export_properties(
    family: Family,
    engine: &EngineId,
    properties: &BTreeMap<String, String>,
    issues: &mut Issues,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut options: Vec<String> = Vec::new();
    let mut variables: Vec<String> = Vec::new();
    let mut redacted = 0usize;
    for (raw_key, value) in properties {
        // A key that looks like a secret, or holds non-printable characters, is never echoed.
        if is_secret_like_key(raw_key)
            || raw_key.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            redacted += 1;
            let field = format!("properties.<redacted #{redacted}>");
            if is_secret_like_key(raw_key) {
                issues.warn(
                    ExportIssueKind::LooksLikeSecret,
                    &field,
                    "the key looks like it names a secret: not exported",
                );
            } else {
                issues.warn(
                    ExportIssueKind::Dropped,
                    &field,
                    "the key has whitespace or control characters: not exported",
                );
            }
            continue;
        }
        let field = format!("properties.{raw_key}");
        // NineSQL property names are matched ignoring ASCII case, as the drivers do.
        let canonical = |list: &[&'static str]| {
            list.iter()
                .find(|n| n.eq_ignore_ascii_case(raw_key))
                .map(|n| (*n).to_owned())
        };
        let key = canonical(&["search_path"])
            .or_else(|| canonical(PG_OPTIONS))
            .or_else(|| canonical(MARIADB_VARIABLES))
            .or_else(|| canonical(MYSQL_VARIABLES))
            .or_else(|| canonical(SQLITE_PROPERTIES))
            .filter(|_| family != Family::Other)
            .unwrap_or_else(|| raw_key.clone());
        let key = &key;
        if is_secret_like_key(key) || is_secret_like_entry(key, value) {
            issues.warn(
                ExportIssueKind::LooksLikeSecret,
                &field,
                "looks like a secret: not exported",
            );
            continue;
        }
        if value.chars().any(char::is_control) {
            issues.warn(
                ExportIssueKind::Dropped,
                &field,
                "the value has control characters: not exported",
            );
            continue;
        }
        let no_equivalent = |issues: &mut Issues| {
            issues.warn(
                ExportIssueKind::Dropped,
                &field,
                format!(
                    "no equivalent JDBC property for `{engine}` in DBeaver's driver: not exported"
                ),
            );
        };
        match family {
            Family::Pg if key == "search_path" => {
                out.insert("currentSchema".to_owned(), value.clone());
            }
            Family::Pg if PG_OPTIONS.contains(&key.as_str()) => {
                options.push(format!("-c {key}={}", pg_option_escape(value)));
            }
            Family::MySql | Family::MariaDb => {
                let allowed = if family == Family::MariaDb {
                    MARIADB_VARIABLES
                } else {
                    MYSQL_VARIABLES
                };
                if !allowed.contains(&key.as_str()) {
                    no_equivalent(issues);
                } else if key == "time_zone" {
                    if value.is_empty() || value.contains(['\'', '\\', '"']) {
                        issues.warn(
                            ExportIssueKind::Dropped,
                            &field,
                            "a time zone with a quote or backslash cannot be written in \
                             `sessionVariables`: not exported",
                        );
                    } else {
                        variables.push(format!("time_zone='{value}'"));
                    }
                } else if !value.is_empty()
                    && value.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                {
                    variables.push(format!("{key}={value}"));
                } else {
                    issues.warn(
                        ExportIssueKind::Dropped,
                        &field,
                        "not a number: not exported",
                    );
                }
            }
            Family::Sqlite if SQLITE_PROPERTIES.contains(&key.as_str()) => {
                out.insert(key.clone(), value.clone());
            }
            Family::Other => {
                if ninesql_driver_api::is_reserved_property(key) {
                    no_equivalent(issues);
                } else {
                    out.insert(key.clone(), value.clone());
                }
            }
            Family::Pg | Family::QuestDb | Family::Sqlite => no_equivalent(issues),
        }
    }
    if !options.is_empty() {
        out.insert("options".to_owned(), options.join(" "));
    }
    if !variables.is_empty() {
        out.insert("sessionVariables".to_owned(), variables.join(","));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------------------------

fn tls_mode_name(mode: TlsMode) -> &'static str {
    match mode {
        TlsMode::Disable => "disable",
        TlsMode::Require => "require",
        TlsMode::VerifyCa => "verify-ca",
        TlsMode::VerifyFull => "verify-full",
    }
}

fn handler(kind: &str, save_password: bool, extra: Vec<(String, J)>, props: Vec<(String, J)>) -> J {
    let mut h = vec![
        ("type".to_owned(), J::s(kind)),
        ("enabled".to_owned(), J::Bool(true)),
        ("save-password".to_owned(), J::Bool(save_password)),
    ];
    h.extend(extra);
    h.push(("properties".to_owned(), J::Obj(props)));
    J::Obj(h)
}

/// Properties of DBeaver's `mysql_ssl` handler (TLS required).
fn mysql_handler_props(
    verify: bool,
    ca: Option<String>,
    cert: Option<(String, String)>,
) -> Vec<(String, J)> {
    let mut p = vec![
        ("ssl.require".to_owned(), J::Bool(true)),
        ("ssl.verify.server".to_owned(), J::Bool(verify)),
    ];
    if let Some(ca) = ca {
        p.push(("ssl.ca.cert".to_owned(), J::s(ca)));
    }
    if let Some((c, k)) = cert {
        p.push(("ssl.client.cert".to_owned(), J::s(c)));
        p.push(("ssl.client.key".to_owned(), J::s(k)));
    }
    p
}

/// TLS settings in the form NineSQL's importer reads back to the same mode (never weaker).
fn export_tls(
    family: Family,
    tls: &ExportTls,
    props: &mut BTreeMap<String, String>,
    handlers: &mut Vec<(String, J)>,
    issues: &mut Issues,
) {
    let ca = tls.ca_file.as_deref().map(|p| (path_text(p), p));
    let cert = tls
        .client_certificate
        .as_ref()
        .map(|c| (path_text(&c.cert_file), path_text(&c.key_file), c));
    if let Some((None, _)) = ca {
        issues.warn(
            ExportIssueKind::Dropped,
            "tls.ca_file",
            "the CA file path is not valid UTF-8: not exported",
        );
    }
    if let Some((a, b, _)) = &cert
        && (a.is_none() || b.is_none())
    {
        issues.warn(
            ExportIssueKind::Dropped,
            "tls.client_certificate",
            "a client certificate path is not valid UTF-8: not exported",
        );
    }
    if let Some((_, _, c)) = &cert
        && c.has_passphrase
    {
        issues.info(
            ExportIssueKind::SecretOmitted,
            "tls.client_certificate.passphrase",
            "the client key passphrase is not exported",
        );
    }
    let ca_text = ca.and_then(|(t, _)| t);
    let cert_paths = cert.and_then(|(a, b, _)| a.zip(b));
    let mode = tls_mode_name(tls.mode);
    match family {
        Family::Pg | Family::QuestDb => {
            let mut p = vec![
                ("sslMode".to_owned(), J::s(mode)),
                ("ssl.method".to_owned(), J::s("CERTIFICATES")),
            ];
            if let Some(ca) = ca_text {
                p.push(("ssl.ca.cert".to_owned(), J::s(ca)));
            }
            if let Some((c, k)) = cert_paths {
                p.push(("ssl.client.cert".to_owned(), J::s(c)));
                p.push(("ssl.client.key".to_owned(), J::s(k)));
            }
            handlers.push((
                "postgre_ssl".to_owned(),
                handler("CONFIG", false, vec![], p),
            ));
        }
        Family::MySql | Family::MariaDb => {
            let mariadb = family == Family::MariaDb;
            match tls.mode {
                TlsMode::Disable => {
                    // No `mysql_ssl` form disables TLS: a driver property does.
                    if mariadb {
                        props.insert("sslMode".to_owned(), "disable".to_owned());
                    } else {
                        props.insert("useSSL".to_owned(), "false".to_owned());
                    }
                    if ca_text.is_some() || cert_paths.is_some() {
                        issues.info(
                            ExportIssueKind::Dropped,
                            "tls",
                            "certificate paths of a connection with TLS disabled are not \
                             exported",
                        );
                    }
                }
                TlsMode::VerifyFull => {
                    // DBeaver's `mysql_ssl` handler verifies the chain, not the host name
                    // (Connector/J `VERIFY_CA`), so the host name check goes in the `sslMode`
                    // driver property. The handler is written too, so that DBeaver never sees
                    // a connection without TLS settings (it may then add `useSSL=false`) and a
                    // driver that ignores `sslMode` still verifies the chain: the worst case is
                    // `verify-ca`, never plaintext. NineSQL's importer keeps the stricter of
                    // the two (`verify-full`).
                    let value = if mariadb {
                        "verify-full"
                    } else {
                        "VERIFY_IDENTITY"
                    };
                    props.insert("sslMode".to_owned(), value.to_owned());
                    if mariadb && let Some(ca) = &ca_text {
                        props.insert("serverSslCert".to_owned(), ca.clone());
                    }
                    handlers.push((
                        "mysql_ssl".to_owned(),
                        handler(
                            "CONFIG",
                            false,
                            vec![],
                            mysql_handler_props(true, ca_text, cert_paths),
                        ),
                    ));
                    issues.warn(
                        ExportIssueKind::Approximated,
                        "tls.mode",
                        if mariadb {
                            "`verify-full` needs MariaDB Connector/J 3.0 or later in DBeaver \
                             (`sslMode=verify-full`); an older driver ignores `sslMode` and \
                             only applies DBeaver's TLS handler, which does not verify the \
                             host name (`verify-ca`)"
                        } else {
                            "`verify-full` needs MySQL Connector/J 8.0.13 or later in DBeaver \
                             (`sslMode=VERIFY_IDENTITY`); an older driver ignores `sslMode` \
                             and only applies DBeaver's TLS handler, which does not verify the \
                             host name (`verify-ca`). How DBeaver merges its handler settings \
                             with this property is not verified"
                        },
                    );
                }
                TlsMode::Require | TlsMode::VerifyCa => {
                    let verify = tls.mode == TlsMode::VerifyCa;
                    let p = mysql_handler_props(verify, ca_text, cert_paths);
                    handlers.push(("mysql_ssl".to_owned(), handler("CONFIG", false, vec![], p)));
                }
            }
        }
        Family::Sqlite => {}
        Family::Other => {
            let default = tls.mode == TlsMode::VerifyFull
                && tls.ca_file.is_none()
                && tls.client_certificate.is_none();
            issues.push(
                if default {
                    Severity::Info
                } else {
                    Severity::Warning
                },
                ExportIssueKind::Dropped,
                Some("tls"),
                format!(
                    "TLS settings are not exported for this engine (mode `{mode}`): set them in \
                     DBeaver's driver settings; NineSQL imports the connection back with \
                     `verify-full`"
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// SSH
// ---------------------------------------------------------------------------------------------

fn auth_type(auth: &ExportSshAuth) -> &'static str {
    match auth {
        ExportSshAuth::Agent => "AGENT",
        ExportSshAuth::Password { .. } => "PASSWORD",
        ExportSshAuth::PublicKey { .. } => "PUBLIC_KEY",
    }
}

/// Whether a hop's secret is asked for (`save-password: false`). An unencrypted key is written
/// as saved with nothing saved, so DBeaver uses no passphrase instead of asking for one.
fn hop_asks(auth: &ExportSshAuth) -> bool {
    !matches!(
        auth,
        ExportSshAuth::PublicKey {
            passphrase: None,
            ..
        }
    )
}

fn hop_checks(hop: &ExportSshHop, field: &str, issues: &mut Issues) -> Result<(), &'static str> {
    match &hop.auth {
        ExportSshAuth::Password { saved: true } => issues.info(
            ExportIssueKind::SecretOmitted,
            field,
            "the saved SSH password is not exported: DBeaver asks for it",
        ),
        ExportSshAuth::PublicKey {
            passphrase: Some(true),
            ..
        } => issues.info(
            ExportIssueKind::SecretOmitted,
            field,
            "the saved key passphrase is not exported: DBeaver asks for it",
        ),
        _ => {}
    }
    if let ExportSshAuth::PublicKey { key_path, .. } = &hop.auth
        && path_text(key_path).is_none()
    {
        return Err("an SSH key path is not valid UTF-8");
    }
    if hop.host_key_policy != ExportHostKeyPolicy::Strict {
        issues.info(
            ExportIssueKind::Dropped,
            field,
            "the host key policy of this hop is not exported: DBeaver applies its own host key \
             checks",
        );
    }
    Ok(())
}

fn ssh_handler(ssh: &ExportSshTunnel, issues: &mut Issues) -> Result<J, &'static str> {
    if ssh.jump_hosts.len() > 5 {
        return Err("more SSH jump hosts than DBeaver supports (5)");
    }
    let server = &ssh.server;
    hop_checks(server, "ssh", issues)?;
    let mut p = vec![
        ("host".to_owned(), J::s(&server.host)),
        ("port".to_owned(), J::s(server.port.to_string())),
        ("authType".to_owned(), J::s(auth_type(&server.auth))),
    ];
    if let ExportSshAuth::PublicKey { key_path, .. } = &server.auth {
        p.push((
            "keyPath".to_owned(),
            J::s(path_text(key_path).unwrap_or_default()),
        ));
    }
    for (i, hop) in ssh.jump_hosts.iter().enumerate() {
        let field = format!("ssh.jump_hosts[{i}]");
        hop_checks(hop, &field, issues)?;
        let k = |name: &str| format!("jumpServer{i}.{name}");
        p.push((k("enabled"), J::Bool(true)));
        p.push((k("host"), J::s(&hop.host)));
        p.push((k("port"), J::s(hop.port.to_string())));
        p.push((k("name"), J::s(&hop.username)));
        p.push((k("authType"), J::s(auth_type(&hop.auth))));
        if let ExportSshAuth::PublicKey { key_path, .. } = &hop.auth {
            p.push((k("keyPath"), J::s(path_text(key_path).unwrap_or_default())));
        }
    }
    if !ssh.jump_hosts.is_empty() {
        p.push((
            "jumpServer.count".to_owned(),
            J::s(ssh.jump_hosts.len().to_string()),
        ));
        issues.warn(
            ExportIssueKind::NotVerified,
            "ssh.jump_hosts",
            "jump hosts are written as `jumpServer<i>.*` properties, taken from DBeaver's \
             source (no fixture)",
        );
        if ssh.jump_hosts.len() > 1 {
            issues.warn(
                ExportIssueKind::Approximated,
                "ssh.jump_hosts",
                "route change in DBeaver before 24.3: it reads only the first jump host and \
                 would connect through a shorter route than NineSQL; use DBeaver 24.3 or later",
            );
        }
    }
    if ssh.jump_hosts.iter().any(|h| hop_asks(&h.auth)) && !hop_asks(&server.auth) {
        issues.info(
            ExportIssueKind::Approximated,
            "ssh.jump_hosts",
            "DBeaver has one \"save password\" flag per tunnel: the jump hosts' secrets follow \
             the SSH server's setting",
        );
    }
    issues.warn(
        ExportIssueKind::Dropped,
        "ssh.host_key_policy",
        "DBeaver applies its own SSH host key checks, not NineSQL's known_hosts and per-hop \
         policies: check the fingerprint DBeaver shows on the first connection",
    );
    issues.info(
        ExportIssueKind::NotVerified,
        "ssh.username",
        "the SSH user name is written as the handler's `user` (DBeaver keeps it in \
         credentials-config.json, which NineSQL never writes); not verified against a \
         DBeaver-written workspace",
    );
    Ok(handler(
        "TUNNEL",
        !hop_asks(&server.auth),
        vec![("user".to_owned(), J::s(&server.username))],
        p,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_option_escaping() {
        assert_eq!(pg_option_escape("a b\\c"), "a\\ b\\\\c");
        assert_eq!(pg_option_escape("UTC"), "UTC");
    }

    #[test]
    fn ids_are_unique_and_dbeaver_shaped() {
        let random = std::hash::RandomState::new();
        let mut used = BTreeSet::new();
        let a = unique_id("postgres-jdbc", 0x1a1d, &random, (0, "x"), &mut used);
        let b = unique_id("postgres-jdbc", 0x1a1d, &random, (0, "x"), &mut used);
        assert_ne!(a, b);
        assert!(a.starts_with("postgres-jdbc-1a1d-"));
    }

    #[test]
    fn every_import_mapped_engine_has_a_driver() {
        for e in [
            "postgresql",
            "timescaledb",
            "yugabytedb",
            "questdb",
            "mysql",
            "mariadb",
            "tidb",
            "sqlite",
            "duckdb",
            "clickhouse",
            "trino",
            "firebird",
        ] {
            assert!(engine_out(&EngineId::new(e.to_owned())).is_some(), "{e}");
        }
        assert!(engine_out(&EngineId::new("oracle".to_owned())).is_none());
    }
}
