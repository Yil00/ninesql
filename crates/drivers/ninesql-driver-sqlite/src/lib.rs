//! `ninesql-driver-sqlite`: the SQLite driver (ADR-0003), a file engine served through
//! `rusqlite` with the SQLite amalgamation bundled (no system library, no network, I2).
//!
//! ROADMAP: P1-B. Our code keeps `#![forbid(unsafe_code)]`: the only `unsafe` is inside
//! `rusqlite` / `libsqlite3-sys`, which bind the C library (AGENTS.md).
//!
//! # Contract highlights (docs/08-engine-capabilities.md)
//! - **Open**: an absolute path only, canonicalized (symbolic links and `..` resolved) and
//!   opened with `SQLITE_OPEN_NOFOLLOW`, never as a URI; a regular file only; created only for
//!   read-write sessions that ask for it. `Endpoint::InMemory` opens a private in-memory
//!   database, refused for read-only sessions. No authentication, no TLS, no tunnel.
//! - **Read-only** (I3, [`ReadOnlyMechanism::OpenMode`], not liftable): `SQLITE_OPEN_READ_ONLY`,
//!   `PRAGMA query_only = ON`, no attached databases, and an authorizer that denies every write,
//!   `ATTACH` (so `VACUUM INTO`), temporary objects and non-reading pragmas while statements are
//!   prepared; verified and re-asserted after every statement, the session closed if anything
//!   changed.
//! - **Properties**: reserved keys refused ([`EXTRA_RESERVED_PROPERTIES`] on top of the global
//!   rule), then only [`ACCEPTED_PROPERTIES`]: no arbitrary `PRAGMA` through properties.
//! - **Execute**: one statement per call, on a dedicated thread per session (never the GPUI
//!   thread, never the shared blocking pool), one batch per demand under the caller's
//!   `FetchBudget`; cancel and timeouts through `sqlite3_interrupt`.
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod dialect;
mod session;
mod types;
mod worker;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ninesql_driver_api::{
    Auth, CancelSupport, Capabilities, ConnectParams, Dialect, Driver, DriverError, DriverResult,
    EditingCapabilities, Endpoint, EngineId, ExecOptions, ExecutionTracker, ExplainFormat,
    IsolationLevel, MetadataCapabilities, ObjectKind, PropertyValue, ReadOnlyMechanism, Session,
    Statement, TransactionCapabilities,
};

pub use dialect::{SqliteDialect, bracket_idents_as_quotes};
pub use session::{SqliteMetadata, SqliteSession};
pub use worker::PAUSED_RESULT_TIMEOUT;

use crate::worker::{OpenOptions, Shared, Target};

/// Family name.
pub const FAMILY: &str = "sqlite";

/// Default `busy_timeout`: how long a statement waits for a lock held by another connection.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest `busy_timeout` property accepted (milliseconds).
pub const MAX_BUSY_TIMEOUT_MS: u64 = 600_000;

/// Driver property keys the SQLite driver reserves on top of the global rule
/// ([`ninesql_driver_api::is_reserved_property`]): pragmas and open options that change the
/// access mode or the file (`query_only`, `writable_schema`, `trusted_schema`, `journal_mode`,
/// `locking_mode`, `synchronous`, `temp_store`, `temp_store_directory`, `data_store_directory`,
/// `auto_vacuum`, `page_size`, `mmap_size`, `cell_size_check`, `secure_delete`,
/// `legacy_alter_table`, `ignore_check_constraints`, `schema_version`, `user_version`,
/// `application_id`, `recursive_triggers`), lexical modes (`encoding`, `case_sensitive_like`),
/// URI and open parameters (`mode`, `open_mode`, `cache`, `shared_cache`, `immutable`, `nolock`,
/// `modeof`, `vfs`, `psow`), file locations (`database`, `dbname`, `path`, `file`, `filename`),
/// extension loading (`enable_load_extension`, `load_extension`, `extension`, `extensions`),
/// encryption keys of SQLCipher-style builds (`key`, `rekey`, `hexkey`, `hexrekey`, `textkey`,
/// `cipher`) and sqlite-jdbc's `transaction_mode`.
pub const EXTRA_RESERVED_PROPERTIES: &[&str] = &[
    "query_only",
    "writable_schema",
    "trusted_schema",
    "journal_mode",
    "locking_mode",
    "synchronous",
    "temp_store",
    "temp_store_directory",
    "data_store_directory",
    "auto_vacuum",
    "page_size",
    "mmap_size",
    "cell_size_check",
    "secure_delete",
    "legacy_alter_table",
    "ignore_check_constraints",
    "schema_version",
    "user_version",
    "application_id",
    "recursive_triggers",
    "encoding",
    "case_sensitive_like",
    "mode",
    "open_mode",
    "cache",
    "shared_cache",
    "immutable",
    "nolock",
    "modeof",
    "vfs",
    "psow",
    "database",
    "dbname",
    "path",
    "file",
    "filename",
    "enable_load_extension",
    "load_extension",
    "extension",
    "extensions",
    "key",
    "rekey",
    "hexkey",
    "hexrekey",
    "textkey",
    "cipher",
    "transaction_mode",
];

/// Driver properties the SQLite driver accepts (normalized keys). Any other key is refused at
/// connect (fails closed):
/// - `busy_timeout`: milliseconds to wait for a lock, `0..=600000` (default 5000);
/// - `cache_size`: `PRAGMA cache_size`, an integer in `-1048576..=1000000` (pages, or KiB when
///   negative);
/// - `foreign_keys`: `PRAGMA foreign_keys`, `on` / `off` (`true` / `false`, `1` / `0`); on by
///   default in the bundled build.
pub const ACCEPTED_PROPERTIES: &[&str] = &["busytimeout", "cachesize", "foreignkeys"];

/// The SQLite driver.
#[derive(Debug, Default)]
pub struct SqliteDriver {
    _private: (),
}

const ENGINES: &[EngineId] = &[EngineId::SQLITE];

impl SqliteDriver {
    /// The driver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// See [`EXTRA_RESERVED_PROPERTIES`].
    #[must_use]
    pub fn extra_reserved_properties() -> &'static [&'static str] {
        EXTRA_RESERVED_PROPERTIES
    }

    fn check_engine(engine: &EngineId) -> DriverResult<()> {
        if ENGINES.contains(engine) {
            Ok(())
        } else {
            Err(DriverError::InvalidParams(format!(
                "engine `{engine}` is not served by the sqlite driver"
            )))
        }
    }
}

/// Capabilities of SQLite through this driver (mirrored in docs/08).
fn capabilities_for(server_version: Option<&str>) -> Capabilities {
    let mut c = Capabilities::minimal(EngineId::SQLITE);
    c.server_version = server_version.map(str::to_owned);
    let mut metadata = MetadataCapabilities::default();
    metadata.kinds = BTreeSet::from([
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Column,
    ]);
    metadata.ddl = BTreeSet::from([ObjectKind::Table, ObjectKind::View]);
    c.metadata = metadata;
    let mut editing = EditingCapabilities::default();
    editing.insert = true;
    editing.update = true;
    editing.delete = true;
    editing.requires_unique_key = true;
    c.editing = editing;
    let mut tx = TransactionCapabilities::default();
    tx.manual_commit = true;
    tx.savepoints = true;
    tx.isolation_levels = BTreeSet::from([IsolationLevel::Serializable]);
    c.transactions = tx;
    c.read_only = ReadOnlyMechanism::OpenMode;
    c.cancel = CancelSupport::ServerSide;
    c.explain = BTreeSet::from([ExplainFormat::Text]);
    c.multiple_result_sets = false;
    c.server_side_timeout = false;
    c.tls = BTreeSet::new();
    c.parameters = true;
    c.extra_reserved_properties = EXTRA_RESERVED_PROPERTIES
        .iter()
        .map(|k| (*k).to_owned())
        .collect();
    c.retains_secret_after_connect = false;
    c
}

fn normalize(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Validated property values: the busy timeout and the `PRAGMA` statements to run (built by
/// the driver from parsed values, never from the text the user typed).
fn accepted_properties(
    properties: &BTreeMap<String, PropertyValue>,
) -> DriverResult<(Duration, Vec<String>)> {
    let mut busy = DEFAULT_BUSY_TIMEOUT;
    let mut pragmas = Vec::new();
    for (key, value) in properties {
        let invalid =
            || DriverError::InvalidParams(format!("property `{key}` has an invalid value"));
        let v = value.expose().trim();
        match normalize(key).as_str() {
            "busytimeout" => {
                if v.is_empty() || v.len() > 7 || !v.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                let ms: u64 = v.parse().map_err(|_| invalid())?;
                if ms > MAX_BUSY_TIMEOUT_MS {
                    return Err(invalid());
                }
                busy = Duration::from_millis(ms);
            }
            "cachesize" => {
                let n: i64 = v.parse().map_err(|_| invalid())?;
                if !(-1_048_576..=1_000_000).contains(&n) {
                    return Err(invalid());
                }
                pragmas.push(format!("PRAGMA cache_size = {n}"));
            }
            "foreignkeys" => {
                let on = match v.to_ascii_lowercase().as_str() {
                    "on" | "true" | "1" | "yes" => true,
                    "off" | "false" | "0" | "no" => false,
                    _ => return Err(invalid()),
                };
                pragmas.push(format!(
                    "PRAGMA foreign_keys = {}",
                    if on { "ON" } else { "OFF" }
                ));
            }
            _ => {
                return Err(DriverError::InvalidParams(format!(
                    "property `{key}` is not supported by the sqlite driver; accepted: \
                     busy_timeout, cache_size, foreign_keys"
                )));
            }
        }
    }
    Ok((busy, pragmas))
}

#[async_trait::async_trait]
impl Driver for SqliteDriver {
    fn family(&self) -> &'static str {
        FAMILY
    }

    fn engines(&self) -> &[EngineId] {
        ENGINES
    }

    fn capabilities(
        &self,
        engine: &EngineId,
        server_version: Option<&str>,
    ) -> DriverResult<Capabilities> {
        Self::check_engine(engine)?;
        Ok(capabilities_for(server_version))
    }

    fn dialect(&self, engine: &EngineId) -> DriverResult<Arc<dyn Dialect>> {
        Self::check_engine(engine)?;
        Ok(Arc::new(SqliteDialect))
    }

    async fn connect(&self, params: ConnectParams) -> DriverResult<Box<dyn Session>> {
        Self::check_engine(&params.engine)?;
        // Before touching the file system: properties, endpoint, credentials.
        capabilities_for(None).check_properties(&params.properties)?;
        let (busy_timeout, pragmas) = accepted_properties(&params.properties)?;
        let ConnectParams {
            endpoint,
            auth,
            database,
            tunnel,
            proxy,
            read_only,
            auto_commit,
            connect_timeout,
            init_statements,
            production,
            ..
        } = params;
        if !matches!(auth, Auth::None) {
            // Dropped (zeroized) on return: SQLite has no authentication.
            return Err(DriverError::InvalidParams(
                "SQLite has no authentication: remove the user name and password of this \
                 connection"
                    .into(),
            ));
        }
        drop(auth);
        if tunnel.is_some() || proxy.is_some() {
            return Err(DriverError::InvalidParams(
                "a SQLite database is a local file: SSH tunnels and proxies do not apply".into(),
            ));
        }
        if database.as_deref().is_some_and(|d| d != "main") {
            return Err(DriverError::InvalidParams(
                "a SQLite connection opens one file: leave the database name empty".into(),
            ));
        }
        let target = match endpoint {
            Endpoint::File {
                path,
                create_if_missing,
            } => {
                if !path.is_absolute() {
                    return Err(DriverError::InvalidParams(
                        "the SQLite database path must be an absolute file path".into(),
                    ));
                }
                Target::File {
                    path,
                    create: create_if_missing && !read_only,
                }
            }
            Endpoint::InMemory if read_only => {
                return Err(DriverError::InvalidParams(
                    "an in-memory SQLite database cannot be opened read-only (it would be empty)"
                        .into(),
                ));
            }
            Endpoint::InMemory => Target::Memory,
            _ => {
                return Err(DriverError::InvalidParams(
                    "SQLite needs a database file path".into(),
                ));
            }
        };
        let shared = Arc::new(Shared::new(read_only, auto_commit));
        let options = OpenOptions {
            read_only,
            busy_timeout,
            pragmas,
        };
        let (tx, opened) = worker::spawn(target, options, Arc::clone(&shared))?;
        let opened = match tokio::time::timeout(connect_timeout, opened).await {
            Ok(Ok(Ok(o))) => o,
            Ok(Ok(Err(e))) => return Err(e),
            Ok(Err(_)) => {
                return Err(DriverError::Connect(
                    "the SQLite connection thread stopped".into(),
                ));
            }
            Err(_) => {
                shared.mark_closed("connect timed out");
                return Err(DriverError::Connect(format!(
                    "opening the database did not finish within {connect_timeout:?}"
                )));
            }
        };
        let mut session = SqliteSession {
            link: session::Link {
                tx,
                shared,
                tracker: ExecutionTracker::new(),
            },
            capabilities: capabilities_for(Some(&opened.version)),
            interrupt: opened.interrupt,
            production,
            current_exec: Mutex::new(Arc::default()),
        };
        tracing::debug!(sqlite_version = %opened.version, read_only, "sqlite session opened");
        // Read-only is in force since the open: init statements cannot lift it (I3).
        let init = async {
            for sql in &init_statements {
                let mut stream = session
                    .execute(Statement::sql(sql.as_str()), ExecOptions::default())
                    .await?;
                while let Some(item) = stream.next().await {
                    item?;
                }
            }
            DriverResult::Ok(())
        };
        match tokio::time::timeout(connect_timeout, init).await {
            Ok(Ok(())) => Ok(Box::new(session)),
            Ok(Err(e)) => {
                let _ = Box::new(session).close().await;
                Err(e)
            }
            Err(_) => {
                let _ = Box::new(session).close().await;
                Err(DriverError::Connect(
                    "init statements did not finish within the connect timeout".into(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninesql_driver_api::Secret;

    #[test]
    fn capabilities_are_declared() {
        let d = SqliteDriver::new();
        let c = d.capabilities(&EngineId::SQLITE, None).unwrap();
        assert_eq!(c.read_only, ReadOnlyMechanism::OpenMode);
        assert!(!c.read_only.is_liftable());
        assert_eq!(c.cancel, CancelSupport::ServerSide);
        assert!(c.tls.is_empty());
        assert!(!c.retains_secret_after_connect);
        assert!(!c.multiple_result_sets);
        for key in [
            "query_only",
            "journalMode",
            "temp-store",
            "vfs",
            "key",
            "sslmode",
        ] {
            assert!(c.is_reserved_property(key), "{key}");
        }
        for key in ACCEPTED_PROPERTIES {
            assert!(!c.is_reserved_property(key), "{key}");
        }
        assert!(d.capabilities(&EngineId::DUCKDB, None).is_err());
        assert!(d.dialect(&EngineId::POSTGRESQL).is_err());
        assert_eq!(d.engines(), &[EngineId::SQLITE]);
    }

    #[test]
    fn properties_allowlist() {
        let mut p = BTreeMap::new();
        p.insert("busy_timeout".into(), PropertyValue::Plain("250".into()));
        p.insert("cacheSize".into(), PropertyValue::Plain("-2000".into()));
        p.insert("foreign_keys".into(), PropertyValue::Plain("off".into()));
        let (busy, pragmas) = accepted_properties(&p).unwrap();
        assert_eq!(busy, Duration::from_millis(250));
        assert!(pragmas.contains(&"PRAGMA cache_size = -2000".to_owned()));
        assert!(pragmas.contains(&"PRAGMA foreign_keys = OFF".to_owned()));
        for (k, v) in [
            ("busy_timeout", "5; ATTACH 'x' AS y"),
            ("busy_timeout", "+5"),
            ("busy_timeout", "9999999"),
            ("cache_size", "1e9"),
            ("foreign_keys", "maybe-secret"),
            ("temp_store", "MEMORY"),
            ("whatever", "secret-value"),
        ] {
            let mut bad = BTreeMap::new();
            bad.insert(k.to_owned(), PropertyValue::Plain(v.into()));
            let err = accepted_properties(&bad).unwrap_err().to_string();
            assert!(!err.contains(v), "{err}");
        }
    }

    #[tokio::test]
    async fn connect_refuses_before_touching_files() {
        let d = SqliteDriver::new();
        let dir = tempfile::tempdir().unwrap();
        let base = || ConnectParams::new(EngineId::SQLITE, Endpoint::file(dir.path().join("x.db")));
        let refused =
            |r: DriverResult<Box<dyn Session>>| matches!(r, Err(DriverError::InvalidParams(_)));
        let mut p = base();
        p.auth = Auth::password("nine", Some(Secret::new("pw".to_owned())));
        assert!(refused(d.connect(p).await));
        let mut p = base();
        p.endpoint = Endpoint::tcp("db.example.test", 5432);
        assert!(refused(d.connect(p).await));
        let mut p = base();
        p.endpoint = Endpoint::file("relative.db");
        assert!(refused(d.connect(p).await));
        let mut p = base();
        p.endpoint = Endpoint::file(":memory:");
        assert!(refused(d.connect(p).await));
        let mut p = base();
        p.endpoint = Endpoint::InMemory;
        p.read_only = true;
        assert!(refused(d.connect(p).await));
        let mut p = base();
        p.database = Some("other".into());
        assert!(refused(d.connect(p).await));
        let mut p = base();
        p.engine = EngineId::DUCKDB;
        assert!(refused(d.connect(p).await));
        let mut p = base();
        p.tunnel = Some(ninesql_driver_api::TunnelInfo::new(
            "bastion.example.test",
            22,
        ));
        assert!(refused(d.connect(p).await));
        assert!(!dir.path().join("x.db").exists());
    }
}
