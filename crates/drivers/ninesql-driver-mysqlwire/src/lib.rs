//! `ninesql-driver-mysqlwire`: the MySQL wire protocol family (ADR-0003). Serves MySQL,
//! MariaDB and TiDB, each with its own engine id and capabilities (I8); the server must be the
//! declared engine.
//!
//! ROADMAP: P1-B, P4-C (TiDB). The client/server protocol is implemented in the driver on top of tokio and
//! rustls with the `ring` provider: no OpenSSL, no C library, no protocol crate (ADR-0002,
//! ADR-0011, docs/03-tech-stack.md).
//!
//! # Contract highlights (docs/08-engine-capabilities.md)
//! - **Connect**: typed options only (handshake fields and `SET SESSION` with inert literals,
//!   never a DSN); reserved properties refused before any network traffic
//!   ([`EXTRA_RESERVED_PROPERTIES`] on top of the global rule) and an allowlist of accepted ones
//!   ([`ACCEPTED_PROPERTIES`]); TLS `disable` / `require` / `verify-ca` / `verify-full`, never a
//!   fallback to plain text; `verify-ca` requires and trusts only `TlsConfig::ca_file`,
//!   `verify-full` trusts only the CA file when set, the platform store otherwise;
//!   `caching_sha2_password` and `mysql_native_password`; the password itself (full
//!   `caching_sha2_password` exchange, `mysql_clear_password`) only over verified TLS or a
//!   host-key-verified SSH tunnel to a loopback endpoint; the server's RSA key is never asked
//!   for. `PasswordExchange::ScramOnly` is not offered (no MySQL exchange proves that the server
//!   knows the password verifier) and is refused at connect. No `CLIENT_MULTI_STATEMENTS`, no `CLIENT_LOCAL_FILES`.
//! - **Cancel**: `KILL QUERY` on a control connection opened and authenticated during
//!   `connect` (same address, same TLS settings), so the password is dropped before `connect`
//!   returns (`retains_secret_after_connect = false`).
//! - **Read-only** (I3): `SET SESSION TRANSACTION READ ONLY` before init statements, verified
//!   by a read-back; after every statement `transaction_read_only` is read back and the session
//!   is closed if it was lifted. TiDB has no session read-only mode: a read-only session needs
//!   a read-only TiDB instance (`tidb_super_read_only` / `tidb_restricted_read_only`), read
//!   back the same way, and an account that mode covers (no `RESTRICTED_REPLICA_WRITER_ADMIN`
//!   or `SUPER` in `SHOW GRANTS`, and a write probe refused with 1836; `tidb` module), and is
//!   refused otherwise.
//! - **Execute**: text protocol, one statement per call, parameters inlined as inert literals
//!   (hexadecimal, digits, fixed date formats); results streamed as Arrow batches, values cut
//!   to the budget while they are read.
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod actor;
mod auth;
mod cancel;
mod conn;
mod dialect;
mod dml;
mod metadata;
mod oneshot;
mod proto;
mod session;
mod tidb;
mod tls;
mod types;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use ninesql_driver_api::{
    Auth, AuthKind, CancelSupport, Capabilities, ConnectCapabilities, ConnectParams, Dialect,
    Driver, DriverError, DriverResult, EditingCapabilities, Endpoint, EngineId, ExecOptions,
    ExplainFormat, HostKeyVerification, IsolationLevel, MetadataCapabilities, ObjectKind, Param,
    PasswordExchange, PropertyValue, ReadOnlyMechanism, Session, Statement, TlsMode,
    TransactionCapabilities, echo_property_key, redact_connection_string, redact_secret,
};
use tokio::sync::mpsc;

pub use actor::PAUSED_RESULT_TIMEOUT;
pub use dialect::MyDialect;
pub use metadata::MyMetadata;
pub use session::MySession;

use crate::actor::{Actor, Shared};
use crate::cancel::Control;
use crate::conn::{Conn, Startup};
use crate::dialect::Modes;

/// Family name.
pub const FAMILY: &str = "mysqlwire";

/// Driver property keys the mysqlwire driver reserves on top of the global rule
/// ([`ninesql_driver_api::is_reserved_property`]): the database (typed field), the character
/// set and collation (lexical modes the driver owns: `characterEncoding`, `charset`,
/// `collation`, `connectionCollation`, `characterSetResults`, `useUnicode`, `names`), session
/// modes the driver owns or verifies (`autocommit`, `session_track_system_variables`,
/// `net_write_timeout`), an authentication input (`serverRsaPublicKeyFile`), local file and
/// local transport access (`allowLoadLocalInfileInPath`, `localSocket`, `pipe`,
/// `namedPipePath`) and statement rewriting into several statements
/// (`rewriteBatchedStatements`).
pub const EXTRA_RESERVED_PROPERTIES: &[&str] = &[
    "database",
    "dbname",
    "characterEncoding",
    "characterSetResults",
    "connectionCollation",
    "charset",
    "collation",
    "useUnicode",
    "names",
    "autocommit",
    "session_track_system_variables",
    "net_write_timeout",
    "serverRsaPublicKeyFile",
    "allowLoadLocalInfileInPath",
    "localSocket",
    "pipe",
    "namedPipePath",
    "rewriteBatchedStatements",
];

/// Value accepted for an allowlisted property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PropKind {
    /// Digits only.
    Unsigned,
    /// Digits with an optional point.
    Decimal,
    /// Any text (sent as a hexadecimal literal).
    Text,
}

/// Driver properties the mysqlwire driver accepts, applied with `SET SESSION` after connect
/// (normalized key, system variable, value kind, engines). Any other key is refused at connect
/// (fails closed).
pub const ACCEPTED_PROPERTIES: &[(&str, &str)] = &[
    ("timezone", "time_zone"),
    ("lockwaittimeout", "lock_wait_timeout"),
    ("innodblockwaittimeout", "innodb_lock_wait_timeout"),
    ("groupconcatmaxlen", "group_concat_max_len"),
    ("maxexecutiontime", "max_execution_time"),
    ("maxstatementtime", "max_statement_time"),
];

fn prop_kind(var: &str) -> PropKind {
    match var {
        "time_zone" => PropKind::Text,
        "max_statement_time" => PropKind::Decimal,
        _ => PropKind::Unsigned,
    }
}

/// `max_execution_time` is MySQL and TiDB only, `max_statement_time` MariaDB only.
fn prop_engine_ok(var: &str, engine: &EngineId) -> bool {
    match var {
        "max_execution_time" => *engine == EngineId::MYSQL || *engine == EngineId::TIDB,
        "max_statement_time" => *engine == EngineId::MARIADB,
        _ => true,
    }
}

/// The mysqlwire driver family.
#[derive(Debug, Default)]
pub struct MySqlWireDriver {
    _private: (),
}

const ENGINES: &[EngineId] = &[EngineId::MYSQL, EngineId::MARIADB, EngineId::TIDB];

impl MySqlWireDriver {
    #[allow(clippy::too_many_lines)]
    /// `Driver::connect` with the concrete session type (crate-internal tests use it).
    async fn open(&self, params: ConnectParams) -> DriverResult<MySession> {
        Self::check_engine(&params.engine)?;
        // Before any network traffic: properties, endpoint, credentials, TLS files.
        let declared = capabilities_for(&params.engine, None);
        declared.check_properties(&params.properties)?;
        declared.connect.check_connect_params(&params)?;
        let settings = accepted_properties(&params.engine, &params.properties)?;
        let log_statements = params.may_log_statements();
        let ConnectParams {
            engine,
            endpoint,
            auth,
            database,
            tls,
            tunnel,
            read_only,
            auto_commit,
            connect_timeout,
            init_statements,
            application_name,
            ..
        } = params;
        let Endpoint::Tcp { host, port } = endpoint else {
            return Err(DriverError::InvalidParams(format!(
                "{engine} needs a host and a port"
            )));
        };
        let (user, password) = match auth {
            Auth::Password { user, password, .. } => (user, password),
            Auth::None => {
                return Err(DriverError::InvalidParams(format!(
                    "{engine} needs a user name"
                )));
            }
            _ => {
                return Err(DriverError::InvalidParams(format!(
                    "{engine} authenticates with a user name and a password"
                )));
            }
        };
        if user.contains('\0')
            || application_name.contains('\0')
            || database.as_deref().is_some_and(|d| d.contains('\0'))
        {
            return Err(DriverError::InvalidParams(
                "the user, database or application name holds a NUL character".into(),
            ));
        }
        let tls_setup = tls::setup(&tls, &host)?;
        if tls.mode.is_downgrade() {
            tracing::warn!(mode = ?tls.mode, "TLS below verify-full for this connection");
        }
        let verified_tls = matches!(tls.mode, TlsMode::VerifyCa | TlsMode::VerifyFull);
        let strict_tunnel = tunnel
            .as_ref()
            .is_some_and(|t| t.weakest_host_key() == HostKeyVerification::Strict)
            && is_loopback_host(&host);
        let handshake = async {
            let (tcp, peer) = proto::open_tcp(&host, port).await?;
            // Protected: verified TLS, or an SSH tunnel with verified host keys whose local end
            // really is on this machine. `require` (unverified TLS) is not protected.
            let startup = Startup {
                user: &user,
                database: database.as_deref(),
                application_name: &application_name,
                protected_transport: verified_tls || (strict_tunnel && peer.ip().is_loopback()),
            };
            let main = Conn::connect(tcp, tls_setup.as_ref(), &startup, password.as_ref()).await?;
            // The control connection for `KILL QUERY`, authenticated now so the password is not
            // kept (L5): same resolved address, same TLS settings.
            let tcp = proto::open_tcp_addr(peer).await?;
            let control =
                Conn::connect(tcp, tls_setup.as_ref(), &startup, password.as_ref()).await?;
            DriverResult::Ok((main, control))
        };
        let outcome = tokio::time::timeout(connect_timeout, handshake).await;
        // Errors may echo what the client sent: mask the password before it is dropped.
        let redact = |e: DriverError| -> DriverError {
            let clean = |m: String| {
                let m = match &password {
                    Some(p) if !p.is_empty() => redact_secret(&m, p),
                    _ => m,
                };
                redact_connection_string(&m)
            };
            match e {
                DriverError::Connect(m) => DriverError::Connect(clean(m)),
                DriverError::Auth(m) => DriverError::Auth(clean(m)),
                DriverError::Tls(m) => DriverError::Tls(clean(m)),
                DriverError::Protocol(m) => DriverError::Connect(clean(m)),
                DriverError::Closed(m) => DriverError::Connect(clean(m)),
                other => other,
            }
        };
        let outcome = match outcome {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(redact(e)),
            Err(_) => Err(DriverError::Connect(format!(
                "no answer from {host}:{port} within {connect_timeout:?}"
            ))),
        };
        // The password is not needed any more (`retains_secret_after_connect = false`).
        drop(password);
        let (main, mut control_conn) = outcome?;
        let ro_expr = match check_server(&engine, &main.server_version) {
            Ok(v) => v,
            Err(e) => {
                let mut main = main;
                main.quit().await;
                control_conn.quit().await;
                return Err(e);
            }
        };
        let version = display_version(&main.server_version).to_owned();
        let shared = Arc::new(Shared::new(main.status, ro_expr, engine == EngineId::TIDB));
        let modes = Arc::new(Modes::default());
        control_conn.status = 0;
        let control = Control::new(control_conn);
        let (tx, rx) = mpsc::channel(8);
        let actor = Actor::new(
            main,
            Arc::clone(&shared),
            Arc::clone(&modes),
            Arc::clone(&control),
        );
        tokio::spawn(actor.run(rx));
        let mut session = MySession {
            link: session::Link {
                tx,
                shared,
                tracker: ninesql_driver_api::ExecutionTracker::new(),
            },
            capabilities: capabilities_for(&engine, Some(&version)),
            dialect: Arc::new(MyDialect::tracking(
                Arc::clone(&modes),
                engine != EngineId::MARIADB,
                engine == EngineId::TIDB,
            )),
            control: Arc::clone(&control),
            log_statements,
            current_exec: std::sync::Mutex::new(Arc::default()),
        };
        tracing::debug!(server_version = %version, read_only, "mysqlwire session opened");
        // Session modes, then read-only, then init statements (I3).
        let setup = async {
            session.run("SET NAMES utf8mb4").await?;
            let rows = session
                .link
                .internal(
                    "SELECT CONNECTION_ID(), @@SESSION.sql_mode, \
                     @@SESSION.net_write_timeout, @@SESSION.character_set_client, \
                     @@SESSION.character_set_results, VERSION()",
                    false,
                )
                .await?;
            let row = rows.into_iter().next().unwrap_or_default();
            let id = row
                .first()
                .cloned()
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| DriverError::Connect("the server sent no connection id".into()))?;
            control.target.store(id, Ordering::SeqCst);
            // The greeting checked above travels in clear before TLS: check the engine and
            // version again from inside the (possibly verified TLS) session.
            let server_version = row
                .get(5)
                .cloned()
                .flatten()
                .ok_or_else(|| DriverError::Connect("the server sent no version".into()))?;
            if check_server(&engine, &server_version)? != ro_expr {
                return Err(DriverError::Connect(
                    "the server version changed between the handshake and the session".into(),
                ));
            }
            session.capabilities =
                capabilities_for(&engine, Some(display_version(&server_version)));
            if let Some(Some(mode)) = row.get(1) {
                modes.set_sql_mode(mode);
            }
            if !row
                .iter()
                .skip(3)
                .take(2)
                .all(|c| c.as_deref().is_some_and(|c| c.starts_with("utf8")))
            {
                return Err(DriverError::Connect(
                    "the server did not accept the utf8mb4 character set".into(),
                ));
            }
            // Only ever raised: a longer value already in force is kept.
            let secs = PAUSED_RESULT_TIMEOUT.as_secs();
            let current = row
                .get(2)
                .cloned()
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            if current < secs {
                session
                    .run(&format!("SET SESSION net_write_timeout = {secs}"))
                    .await?;
            }
            session
                .run(if auto_commit {
                    "SET SESSION autocommit = 1"
                } else {
                    "SET SESSION autocommit = 0"
                })
                .await?;
            for sql in &settings {
                session.run(sql).await?;
            }
            if read_only {
                session.apply_read_only(true).await?;
            }
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
        match tokio::time::timeout(connect_timeout, setup).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = Box::new(session).close().await;
                return Err(e);
            }
            Err(_) => {
                let _ = Box::new(session).close().await;
                return Err(DriverError::Connect(
                    "session setup did not finish within the connect timeout".into(),
                ));
            }
        }
        Ok(session)
    }

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
                "engine `{engine}` is not served by the mysqlwire driver"
            )))
        }
    }
}

/// Capabilities of MySQL, MariaDB or TiDB through this driver (mirrored in docs/08).
fn capabilities_for(engine: &EngineId, server_version: Option<&str>) -> Capabilities {
    if *engine == EngineId::TIDB {
        return tidb_capabilities(server_version);
    }
    let mut c = Capabilities::minimal(engine.clone());
    c.server_version = server_version.map(str::to_owned);
    let mut metadata = MetadataCapabilities::default();
    metadata.kinds = BTreeSet::from([
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Column,
        ObjectKind::Constraint,
        ObjectKind::ForeignKey,
        ObjectKind::Index,
        ObjectKind::Trigger,
        ObjectKind::Partition,
        ObjectKind::Routine,
        ObjectKind::Event,
    ]);
    // `SHOW CREATE …` (P4-A, F-NAV-07); keys, indexes and partitions are part of the table's.
    metadata.ddl = BTreeSet::from([
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Trigger,
        ObjectKind::Routine,
        ObjectKind::Event,
    ]);
    if *engine == EngineId::MARIADB {
        metadata.kinds.insert(ObjectKind::Sequence);
        metadata.ddl.insert(ObjectKind::Sequence);
    }
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
    tx.isolation_levels = BTreeSet::from([
        IsolationLevel::ReadUncommitted,
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
    ]);
    c.transactions = tx;
    c.read_only = ReadOnlyMechanism::SessionSetting { liftable: true };
    c.cancel = CancelSupport::ServerSide;
    c.explain = if *engine == EngineId::MYSQL {
        BTreeSet::from([
            ExplainFormat::Text,
            ExplainFormat::Json,
            ExplainFormat::Analyze,
        ])
    } else {
        BTreeSet::from([ExplainFormat::Text, ExplainFormat::Json])
    };
    c.multiple_result_sets = true;
    c.server_side_timeout = false;
    c.tls = BTreeSet::from([
        TlsMode::Disable,
        TlsMode::Require,
        TlsMode::VerifyCa,
        TlsMode::VerifyFull,
    ]);
    c.parameters = true;
    c.extra_reserved_properties = EXTRA_RESERVED_PROPERTIES
        .iter()
        .map(|k| (*k).to_owned())
        .collect();
    c.retains_secret_after_connect = false;
    let mut connect = ConnectCapabilities::default();
    connect.accepted_properties = Some(
        ACCEPTED_PROPERTIES
            .iter()
            .filter(|(_, var)| prop_engine_ok(var, engine))
            .map(|(_, var)| (*var).to_owned())
            .collect(),
    );
    connect.auth = BTreeSet::from([AuthKind::Password]);
    // No exchange of the MySQL protocol proves that the server knows the password verifier
    // (`caching_sha2_password` is not SCRAM): `ScramOnly` is not offered.
    connect.password_exchanges = BTreeSet::from([PasswordExchange::Any]);
    connect.verify_ca_requires_ca_file = true;
    connect.client_certificates = true;
    connect.encrypted_client_keys = false;
    c.connect = connect;
    c
}

/// TiDB (P4-C, mirrored in docs/08): MySQL's protocol and `information_schema` without
/// triggers, stored routines or events, with sequences; only `READ COMMITTED` and
/// `REPEATABLE READ`; no session read-only mode (see `MySession::apply_tidb_read_only`); TLS
/// not declared until the dev service has a test CA (I8).
fn tidb_capabilities(server_version: Option<&str>) -> Capabilities {
    let mut c = capabilities_for(&EngineId::MYSQL, server_version);
    c.engine = EngineId::TIDB;
    let mut metadata = MetadataCapabilities::default();
    metadata.kinds = BTreeSet::from([
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Column,
        ObjectKind::Constraint,
        ObjectKind::ForeignKey,
        ObjectKind::Index,
        ObjectKind::Partition,
        ObjectKind::Sequence,
    ]);
    metadata.ddl = BTreeSet::from([
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Sequence,
    ]);
    c.metadata = metadata;
    let mut tx = TransactionCapabilities::default();
    tx.manual_commit = true;
    tx.savepoints = true;
    // TiDB refuses SERIALIZABLE and READ UNCOMMITTED (error 8048).
    tx.isolation_levels = BTreeSet::from([
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
    ]);
    c.transactions = tx;
    // The instance's read-only variables; a privileged user can lift them with `SET GLOBAL`
    // (refused client-side as an access-mode change, and read back after every statement).
    c.read_only = ReadOnlyMechanism::ServerSetting { liftable: true };
    // No stored procedures, so no `CALL` with several result sets.
    c.multiple_result_sets = false;
    c.tls = BTreeSet::from([TlsMode::Disable]);
    // No TLS, so no client certificate either (I8): set back to MySQL's values when TLS is
    // declared.
    c.connect.client_certificates = false;
    c.connect.encrypted_client_keys = false;
    let mut accepted = c.connect.accepted_properties.clone().unwrap_or_default();
    accepted.retain(|k| k != "max_statement_time");
    accepted.insert("max_execution_time".to_owned());
    c.connect.accepted_properties = Some(accepted);
    c
}

fn normalize(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Maps accepted properties to `SET SESSION` statements; refuses unknown keys and malformed
/// values (never echoing them).
fn accepted_properties(
    engine: &EngineId,
    properties: &BTreeMap<String, PropertyValue>,
) -> DriverResult<Vec<String>> {
    let mut out = Vec::new();
    for (key, value) in properties {
        let norm = normalize(key);
        let Some((_, var)) = ACCEPTED_PROPERTIES
            .iter()
            .find(|(k, v)| *k == norm && prop_engine_ok(v, engine))
        else {
            return Err(DriverError::InvalidParams(format!(
                "property `{}` is not supported by the {engine} driver; accepted: time_zone, \
                 lock_wait_timeout, innodb_lock_wait_timeout, group_concat_max_len, {}",
                echo_property_key(key),
                if *engine == EngineId::MARIADB {
                    "max_statement_time"
                } else {
                    "max_execution_time"
                }
            )));
        };
        let v = value.expose().trim();
        let lit = match prop_kind(var) {
            PropKind::Unsigned
                if !v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit()) =>
            {
                v.to_owned()
            }
            PropKind::Decimal
                if !v.is_empty()
                    && v.len() <= 32
                    && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                    && v.bytes().filter(|b| *b == b'.').count() <= 1 =>
            {
                v.to_owned()
            }
            PropKind::Text if !v.contains('\0') => dialect::literal(&Param::Text(v.to_owned()))?,
            _ => {
                return Err(DriverError::InvalidParams(format!(
                    "property `{var}` has an invalid value"
                )));
            }
        };
        out.push(format!("SET SESSION {var} = {lit}"));
    }
    Ok(out)
}

/// `localhost` or a loopback IP address (the local end of an SSH tunnel).
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(|c| c == '[' || c == ']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `(major, minor)` of a version string (`"8.4.13"`, `"5.5.5-11.8.9-MariaDB-…"`).
fn version_of(v: &str) -> (u32, u32) {
    let v = v.strip_prefix("5.5.5-").unwrap_or(v);
    let mut it = v.split(|c: char| !c.is_ascii_digit());
    let major = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let minor = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    (major, minor)
}

/// The server version as shown to users (MariaDB's `5.5.5-` replication prefix removed).
fn display_version(v: &str) -> &str {
    v.strip_prefix("5.5.5-").unwrap_or(v)
}

/// Checks that the server is the engine the connection was declared for (capabilities differ),
/// and returns the expression read back for the access mode.
fn check_server(engine: &EngineId, version: &str) -> DriverResult<&'static str> {
    let is_mariadb = version.contains("MariaDB");
    let is_tidb = version.contains("TiDB");
    let server = if is_tidb {
        EngineId::TIDB
    } else if is_mariadb {
        EngineId::MARIADB
    } else {
        EngineId::MYSQL
    };
    if server != *engine {
        let shown: String = display_version(version)
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect();
        return Err(DriverError::Connect(format!(
            "this server reports version {shown}, which is not {engine}; choose the matching \
             engine"
        )));
    }
    if is_tidb {
        return Ok(actor::TIDB_READ_ONLY_EXPR);
    }
    let (major, minor) = version_of(version);
    let modern = if is_mariadb {
        (major, minor) >= (11, 1)
    } else {
        (major, minor) >= (8, 0)
    };
    Ok(if modern {
        "@@SESSION.transaction_read_only"
    } else {
        "@@SESSION.tx_read_only"
    })
}

#[async_trait::async_trait]
impl Driver for MySqlWireDriver {
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
        Ok(capabilities_for(engine, server_version))
    }

    fn dialect(&self, engine: &EngineId) -> DriverResult<Arc<dyn Dialect>> {
        Self::check_engine(engine)?;
        Ok(Arc::new(if *engine == EngineId::MYSQL {
            MyDialect::mysql()
        } else if *engine == EngineId::TIDB {
            MyDialect::tidb()
        } else {
            MyDialect::mariadb()
        }))
    }

    async fn connect(&self, params: ConnectParams) -> DriverResult<Box<dyn Session>> {
        Ok(Box::new(self.open(params).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_are_declared() {
        let d = MySqlWireDriver::new();
        for engine in [EngineId::MYSQL, EngineId::MARIADB] {
            let c = d.capabilities(&engine, None).unwrap();
            assert_eq!(
                c.read_only,
                ReadOnlyMechanism::SessionSetting { liftable: true }
            );
            assert_eq!(c.cancel, CancelSupport::ServerSide);
            assert!(!c.retains_secret_after_connect);
            assert!(c.multiple_result_sets);
            for key in [
                "characterEncoding",
                "character_encoding",
                "sslMode",
                "useSSL",
            ] {
                assert!(c.is_reserved_property(key), "{key}");
            }
            assert!(!c.is_reserved_property("time_zone"));
        }
        assert!(
            d.capabilities(&EngineId::MYSQL, None)
                .unwrap()
                .explain
                .contains(&ExplainFormat::Analyze)
        );
        assert!(
            !d.capabilities(&EngineId::MARIADB, None)
                .unwrap()
                .explain
                .contains(&ExplainFormat::Analyze)
        );
        let tidb = d.capabilities(&EngineId::TIDB, None).unwrap();
        assert_eq!(
            tidb.read_only,
            ReadOnlyMechanism::ServerSetting { liftable: true }
        );
        assert!(!tidb.metadata.kinds.contains(&ObjectKind::Trigger));
        assert!(tidb.metadata.kinds.contains(&ObjectKind::Sequence));
        assert!(!tidb.multiple_result_sets);
        assert_eq!(tidb.tls, BTreeSet::from([TlsMode::Disable]));
        assert!(
            !tidb
                .transactions
                .isolation_levels
                .contains(&IsolationLevel::Serializable)
        );
        assert!(tidb.connect.accepts_property("max_execution_time"));
        assert!(!tidb.connect.accepts_property("max_statement_time"));
        assert!(d.dialect(&EngineId::POSTGRESQL).is_err());
        assert_eq!(
            d.engines(),
            &[EngineId::MYSQL, EngineId::MARIADB, EngineId::TIDB]
        );
    }

    #[test]
    fn properties_allowlist() {
        let mut p = BTreeMap::new();
        p.insert("time_zone".into(), PropertyValue::Plain("+00:00".into()));
        p.insert("lockWaitTimeout".into(), PropertyValue::Plain("5".into()));
        let out = accepted_properties(&EngineId::MYSQL, &p).unwrap();
        assert!(out.contains(&"SET SESSION lock_wait_timeout = 5".to_owned()));
        assert!(
            out.contains(&"SET SESSION time_zone = CONCAT(_utf8mb4 X'2B30303A3030')".to_owned())
        );
        for (k, v) in [
            ("lock_wait_timeout", "5; DROP TABLE t"),
            ("useCompression", "secret-v"),
            ("max_statement_time", "1"),
        ] {
            let mut bad = BTreeMap::new();
            bad.insert(k.to_owned(), PropertyValue::Plain(v.into()));
            let err = accepted_properties(&EngineId::MYSQL, &bad)
                .unwrap_err()
                .to_string();
            assert!(!err.contains(v), "{err}");
        }
        let mut ok = BTreeMap::new();
        ok.insert(
            "max_statement_time".into(),
            PropertyValue::Plain("1.5".into()),
        );
        assert!(accepted_properties(&EngineId::MARIADB, &ok).is_ok());
    }

    #[test]
    fn server_checks() {
        assert_eq!(
            check_server(&EngineId::MYSQL, "8.4.13").unwrap(),
            "@@SESSION.transaction_read_only"
        );
        assert_eq!(
            check_server(&EngineId::MARIADB, "5.5.5-11.8.9-MariaDB-ubu2404").unwrap(),
            "@@SESSION.transaction_read_only"
        );
        assert_eq!(
            check_server(&EngineId::MARIADB, "5.5.5-10.11.6-MariaDB").unwrap(),
            "@@SESSION.tx_read_only"
        );
        assert!(check_server(&EngineId::MYSQL, "5.5.5-11.8.9-MariaDB").is_err());
        assert!(check_server(&EngineId::MARIADB, "8.4.13").is_err());
        assert!(check_server(&EngineId::MYSQL, "8.0.11-TiDB-v8.5.0").is_err());
        assert!(check_server(&EngineId::MARIADB, "8.0.11-TiDB-v8.5.0").is_err());
        assert!(check_server(&EngineId::TIDB, "8.4.13").is_err());
        assert_eq!(
            check_server(&EngineId::TIDB, "8.0.11-TiDB-v8.5.8").unwrap(),
            actor::TIDB_READ_ONLY_EXPR
        );
        let msg = check_server(&EngineId::TIDB, "8.4\u{1b}[2J")
            .unwrap_err()
            .to_string();
        assert!(!msg.chars().any(char::is_control), "{msg:?}");
        assert_eq!(display_version("5.5.5-11.8.9-MariaDB"), "11.8.9-MariaDB");
        assert!(is_loopback_host("localhost") && is_loopback_host("::1"));
        assert!(!is_loopback_host("db.example.test"));
    }

    #[tokio::test]
    async fn connect_refuses_before_network() {
        let d = MySqlWireDriver::new();
        // Port 9 on a reserved address: nothing must be attempted for these.
        let base = || {
            let mut p = ConnectParams::new(EngineId::MYSQL, Endpoint::tcp("192.0.2.1", 9));
            p.auth = Auth::password("nine", None);
            p.connect_timeout = std::time::Duration::from_millis(50);
            p
        };
        let mut p = base();
        p.properties.insert(
            "characterEncoding".into(),
            PropertyValue::Plain("sjis".into()),
        );
        assert!(matches!(
            d.connect(p).await,
            Err(DriverError::InvalidParams(_))
        ));
        let mut p = base();
        p.auth = Auth::None;
        assert!(matches!(
            d.connect(p).await,
            Err(DriverError::InvalidParams(_))
        ));
        let mut p = base();
        p.endpoint = Endpoint::InMemory;
        assert!(matches!(
            d.connect(p).await,
            Err(DriverError::InvalidParams(_))
        ));
        let mut p = base();
        p.engine = EngineId::POSTGRESQL;
        assert!(matches!(
            d.connect(p).await,
            Err(DriverError::InvalidParams(_))
        ));
        // TiDB: no TLS declared yet, so a TLS mode is refused before any traffic.
        let mut p = base();
        p.engine = EngineId::TIDB;
        p.tls.mode = TlsMode::VerifyFull;
        assert!(d.connect(p).await.is_err());
        let mut p = base();
        p.database = Some("a\0b".into());
        assert!(matches!(
            d.connect(p).await,
            Err(DriverError::InvalidParams(_))
        ));
        // M2 and SCRAM only: refused before any traffic, as declared.
        let mut p = base();
        p.password_exchange = PasswordExchange::ScramOnly;
        let err = d.connect(p).await.err().unwrap();
        assert!(matches!(err, DriverError::InvalidParams(_)), "{err:?}");
        let mut p = base();
        p.auth = Auth::token(None, ninesql_driver_api::Secret::new("tok".into()));
        assert!(matches!(
            d.connect(p).await,
            Err(DriverError::InvalidParams(_))
        ));
        let mut p = base();
        p.tls.mode = TlsMode::VerifyCa;
        assert!(matches!(d.connect(p).await, Err(DriverError::Tls(_))));
        let mut p = base();
        p.engine = EngineId::MARIADB;
        p.properties.insert(
            "max_execution_time".into(),
            PropertyValue::Plain("10".into()),
        );
        assert!(matches!(
            d.connect(p).await,
            Err(DriverError::InvalidParams(_))
        ));
    }

    /// Review L-a: no control character of a key reaches an error message.
    #[test]
    fn errors_never_echo_control_characters() {
        let mut p = BTreeMap::new();
        p.insert(
            "time\u{1b}[\n_zone".to_owned(),
            PropertyValue::Plain("\0".into()),
        );
        let msg = accepted_properties(&EngineId::MYSQL, &p)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("`time_zone`"), "{msg}");
        assert!(!msg.chars().any(char::is_control), "{msg:?}");
        let mut p = BTreeMap::new();
        p.insert("x\u{1b}\ny".to_owned(), PropertyValue::Plain("1".into()));
        let msg = accepted_properties(&EngineId::MYSQL, &p)
            .unwrap_err()
            .to_string();
        assert!(!msg.chars().any(char::is_control), "{msg:?}");
    }

    #[test]
    fn connect_capabilities_match_the_driver() {
        let my = capabilities_for(&EngineId::MYSQL, None).connect;
        let maria = capabilities_for(&EngineId::MARIADB, None).connect;
        for c in [&my, &maria] {
            assert!(c.accepts_property("time_zone") && c.accepts_property("timeZone"));
            assert!(c.accepts_property("lock_wait_timeout"));
            assert!(!c.accepts_property("connectTimeout"));
            assert_eq!(c.auth, BTreeSet::from([AuthKind::Password]));
            assert!(!c.accepts_password_exchange(PasswordExchange::ScramOnly));
            assert!(c.verify_ca_requires_ca_file);
            assert!(c.client_certificates && !c.encrypted_client_keys);
        }
        assert!(my.accepts_property("max_execution_time"));
        assert!(!my.accepts_property("max_statement_time"));
        assert!(maria.accepts_property("max_statement_time"));
        assert!(!maria.accepts_property("max_execution_time"));
    }

    /// TiDB declares no TLS until the dev service has a test CA (ADR-0026 §3.6), so it
    /// declares no client certificate either (I8), before and after connect.
    #[test]
    fn tidb_declares_no_client_certificates_while_it_has_no_tls() {
        for version in [None, Some("8.0.11-TiDB-v8.5.8")] {
            let tidb = capabilities_for(&EngineId::TIDB, version);
            assert_eq!(tidb.tls, BTreeSet::from([TlsMode::Disable]));
            assert!(!tidb.connect.client_certificates);
            assert!(!tidb.connect.encrypted_client_keys);
            // The TLS settings declared together.
            assert_eq!(
                tidb.tls.iter().any(|m| *m != TlsMode::Disable),
                tidb.connect.client_certificates
            );
        }
        // MySQL and MariaDB keep theirs.
        assert!(
            capabilities_for(&EngineId::MYSQL, None)
                .connect
                .client_certificates
        );
        assert!(
            capabilities_for(&EngineId::MARIADB, None)
                .connect
                .client_certificates
        );
    }

    /// A TiDB connection with a client certificate: ignored while TLS is `disable` (as for
    /// every driver, TLS files are not looked at), refused with any other mode.
    #[test]
    fn tidb_connect_checks_ignore_or_refuse_client_certificates() {
        use ninesql_driver_api::{Auth, ClientCertificate, ConnectParams, Endpoint, TlsConfig};
        let connect = capabilities_for(&EngineId::TIDB, None).connect;
        let mut tls = TlsConfig::default();
        tls.mode = TlsMode::Disable;
        tls.client_certificate = Some(ClientCertificate::new("/c.pem", "/k.pem"));
        let mut p = ConnectParams::new(EngineId::TIDB, Endpoint::tcp("127.0.0.1", 4000));
        p.auth = Auth::password("nine", None);
        p.tls = tls;
        connect.check_connect_params(&p).unwrap();
        p.tls.mode = TlsMode::Require;
        let msg = connect.check_connect_params(&p).unwrap_err().to_string();
        assert!(msg.contains("client certificates"), "{msg}");
    }
}
