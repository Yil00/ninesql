//! `ninesql-driver-pgwire`: the PostgreSQL wire protocol family (ADR-0003). PostgreSQL is the
//! only engine served for now; TimescaleDB, YugabyteDB and QuestDB will be added with their own
//! capability overrides once their integration tests prove them (I8).
//!
//! ROADMAP: P1-B. Built on the message codec of `postgres-protocol` (rust-postgres), tokio and
//! rustls with the `ring` provider: no OpenSSL, no C library (ADR-0002, ADR-0011).
//!
//! # Contract highlights (docs/08-engine-capabilities.md)
//! - **Connect**: typed options only (startup parameters, never a DSN); reserved properties
//!   refused before any network traffic ([`PgWireDriver::extra_reserved_properties`] on top of
//!   the global rule) and an allowlist of accepted ones ([`ACCEPTED_PROPERTIES`]); TLS
//!   `disable` / `require` / `verify-ca` / `verify-full`, never a fallback to plain text;
//!   `verify-ca` requires and trusts only `TlsConfig::ca_file`, `verify-full` trusts only the CA
//!   file when set, the platform store otherwise (libpq `sslrootcert` semantics);
//!   SCRAM-SHA-256 (server signature required) or md5; cleartext passwords only over verified
//!   TLS or a host-key-verified SSH tunnel to a loopback endpoint; with
//!   `PasswordExchange::ScramOnly`, SCRAM-SHA-256 only (md5, cleartext and a server that skips
//!   authentication are refused, as libpq's `require_auth=scram-sha-256`); the password is dropped
//!   (zeroized) before `connect` returns: cancel requests use the backend key. Remaining
//!   password remnants outside the driver's control are listed in the `auth` module.
//! - **Internal queries** never depend on the session's `search_path`: operators are
//!   `OPERATOR(pg_catalog.…)`, functions, types and relations schema-qualified.
//! - **Read-only** (I3): `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` before init
//!   statements, verified by a read-back; after every statement the driver checks
//!   `default_transaction_read_only` (`ParameterStatus` on PostgreSQL ≥ 14, a read-back before)
//!   and closes the session if it was lifted.
//! - **Execute**: extended protocol, one statement per call; results streamed as Arrow batches
//!   through a portal fetched `batch_rows` rows at a time, values cut to the budget while they
//!   are read.
//! - **Cancel**: `CancelRequest` with the backend key, execution-scoped; an idle portal is closed
//!   instead of fetched (idle-cursor rule), and the driver-implicit read transaction sets
//!   `idle_in_transaction_session_timeout` ([`IDLE_CURSOR_TIMEOUT`]).
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod actor;
mod auth;
mod cancel;
mod conn;
mod dialect;
mod metadata;
mod session;
mod tls;
mod types;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ninesql_driver_api::{
    Auth, AuthKind, CancelSupport, Capabilities, ConnectCapabilities, ConnectParams, Dialect,
    Driver, DriverError, DriverResult, EditingCapabilities, Endpoint, EngineId, ExecOptions,
    ExplainFormat, HostKeyVerification, IsolationLevel, MetadataCapabilities, ObjectKind,
    PasswordExchange, PropertyValue, ReadOnlyMechanism, Session, Statement, TlsMode,
    TransactionCapabilities, echo_property_key, redact_connection_string, redact_secret,
};
use tokio::sync::mpsc;

pub use actor::IDLE_CURSOR_TIMEOUT;
pub use dialect::PgDialect;
pub use metadata::PgMetadata;
pub use session::PgSession;

use crate::actor::{Actor, Shared};
use crate::cancel::CancelTarget;
use crate::conn::{Conn, Startup, open_transport};

/// Family name.
pub const FAMILY: &str = "pgwire";

/// Driver property keys the pgwire driver reserves on top of the global rule
/// ([`ninesql_driver_api::is_reserved_property`]): an authentication target (`krbsrvname`,
/// `gsslib`, `gssdelegation`, `requirepeer`), server selection among several hosts
/// (`target_session_attrs`), the database (typed field), lexical and session modes the driver
/// owns (`client_encoding`, `idle_in_transaction_session_timeout`) and privilege switches
/// (`role`, `session_authorization`, `session_replication_role`).
pub const EXTRA_RESERVED_PROPERTIES: &[&str] = &[
    "krbsrvname",
    "target_session_attrs",
    "gsslib",
    "gssdelegation",
    "requirepeer",
    "dbname",
    "client_encoding",
    "idle_in_transaction_session_timeout",
    "role",
    "session_authorization",
    "session_replication_role",
];

/// Driver properties the pgwire driver accepts, sent as startup parameters (normalized key,
/// server parameter name). Any other key is refused at connect (fails closed).
pub const ACCEPTED_PROPERTIES: &[(&str, &str)] = &[
    ("searchpath", "search_path"),
    ("timezone", "TimeZone"),
    ("statementtimeout", "statement_timeout"),
    ("locktimeout", "lock_timeout"),
    ("workmem", "work_mem"),
    ("clientminmessages", "client_min_messages"),
];

/// The pgwire driver family.
#[derive(Debug, Default)]
pub struct PgWireDriver {
    _private: (),
}

const ENGINES: &[EngineId] = &[EngineId::POSTGRESQL];

impl PgWireDriver {
    /// `Driver::connect` with the concrete session type (crate-internal tests use it).
    async fn open(&self, params: ConnectParams) -> DriverResult<PgSession> {
        Self::check_engine(&params.engine)?;
        // Before any network traffic: properties, endpoint, credentials, TLS files.
        let declared = postgres_capabilities(None);
        declared.check_properties(&params.properties)?;
        declared.connect.check_connect_params(&params)?;
        let extra = accepted_properties(&params.properties)?;
        let log_statements = params.may_log_statements();
        let scram_only = params.password_exchange == PasswordExchange::ScramOnly;
        let ConnectParams {
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
            return Err(DriverError::InvalidParams(
                "PostgreSQL needs a host and a port".into(),
            ));
        };
        let (user, password) = match auth {
            Auth::Password { user, password, .. } => (user, password),
            Auth::None => {
                return Err(DriverError::InvalidParams(
                    "PostgreSQL needs a user name".into(),
                ));
            }
            _ => {
                return Err(DriverError::InvalidParams(
                    "PostgreSQL authenticates with a user name and a password".into(),
                ));
            }
        };
        if application_name.contains('\0') || user.contains('\0') {
            return Err(DriverError::InvalidParams(
                "the user or application name holds a NUL character".into(),
            ));
        }
        let tls_setup = tls::setup(&tls, &host)?;
        if tls.mode.is_downgrade() {
            tracing::warn!(mode = ?tls.mode, "TLS below verify-full for this connection");
        }
        let scs = Arc::new(AtomicBool::new(true));
        let verified_tls = matches!(tls.mode, TlsMode::VerifyCa | TlsMode::VerifyFull);
        let strict_tunnel = tunnel
            .as_ref()
            .is_some_and(|t| t.weakest_host_key() == HostKeyVerification::Strict)
            && is_loopback_host(&host);
        let handshake = async {
            let (io, peer) = open_transport(&host, port, tls_setup.as_ref()).await?;
            // Protected: verified TLS, or an SSH tunnel with verified host keys whose local end
            // really is on this machine. `require` (unverified TLS) is not protected.
            let startup = Startup {
                user: &user,
                database: database.as_deref(),
                application_name: &application_name,
                extra: &extra,
                protected_transport: verified_tls || (strict_tunnel && peer.ip().is_loopback()),
                scram_only,
            };
            let conn = Conn::startup(io, Arc::clone(&scs), &startup, password.as_ref()).await?;
            DriverResult::Ok((conn, peer))
        };
        let outcome = tokio::time::timeout(connect_timeout, handshake).await;
        // Errors may echo what the client sent: mask the password before it is dropped.
        let redact = |e: DriverError| -> DriverError {
            let clean = |m: String| {
                let m = match &password {
                    Some(p) => redact_secret(&m, p),
                    None => m,
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
        let (conn, peer) = outcome?;
        if conn
            .server
            .get("client_encoding")
            .is_some_and(|e| !e.eq_ignore_ascii_case("UTF8"))
        {
            return Err(DriverError::Connect(
                "the server did not accept the UTF8 client encoding".into(),
            ));
        }
        let version = conn.server.get("server_version").map(str::to_owned);
        let shared = Arc::new(Shared::new(conn.server.major_version(), conn.txn_status));
        let cancel_target = Arc::new(CancelTarget {
            addr: peer,
            tls: tls_setup,
            process_id: conn.process_id,
            secret_key: conn.secret_key,
        });
        let (tx, rx) = mpsc::channel(8);
        let actor = Actor::new(conn, Arc::clone(&shared), Arc::clone(&cancel_target));
        tokio::spawn(actor.run(rx));
        let mut session = PgSession {
            link: session::Link {
                tx,
                shared,
                tracker: ninesql_driver_api::ExecutionTracker::new(),
            },
            capabilities: postgres_capabilities(version.as_deref()),
            dialect: Arc::new(PgDialect::tracking(scs)),
            cancel_target,
            auto_commit,
            log_statements,
            current_exec: std::sync::Mutex::new(Arc::default()),
        };
        tracing::debug!(
            server_version = version.as_deref().unwrap_or("unknown"),
            read_only,
            "pgwire session opened"
        );
        // Read-only before anything else, init statements after it (I3).
        let setup = async {
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
        debug_assert_eq!(
            session.link.shared.read_only.load(Ordering::SeqCst),
            read_only
        );
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
                "engine `{engine}` is not served by the pgwire driver"
            )))
        }
    }
}

/// Capabilities of PostgreSQL through this driver (mirrored in docs/08).
fn postgres_capabilities(server_version: Option<&str>) -> Capabilities {
    let mut c = Capabilities::minimal(EngineId::POSTGRESQL);
    c.server_version = server_version.map(str::to_owned);
    let mut metadata = MetadataCapabilities::default();
    metadata.kinds = BTreeSet::from([
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Column,
        ObjectKind::Constraint,
        ObjectKind::ForeignKey,
        ObjectKind::Index,
        ObjectKind::Trigger,
        ObjectKind::Partition,
        ObjectKind::Routine,
        ObjectKind::Sequence,
        ObjectKind::Type,
    ]);
    // Every listed kind but `Column` has DDL (P4-A, F-NAV-07).
    metadata.ddl = metadata
        .kinds
        .iter()
        .copied()
        .filter(|k| *k != ObjectKind::Column)
        .collect();
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
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
    ]);
    c.transactions = tx;
    c.read_only = ReadOnlyMechanism::SessionSetting { liftable: true };
    c.cancel = CancelSupport::ServerSide;
    c.explain = BTreeSet::from([
        ExplainFormat::Text,
        ExplainFormat::Json,
        ExplainFormat::Analyze,
    ]);
    c.multiple_result_sets = false;
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
            .map(|(_, name)| (*name).to_owned())
            .collect(),
    );
    connect.auth = BTreeSet::from([AuthKind::Password]);
    connect.password_exchanges =
        BTreeSet::from([PasswordExchange::Any, PasswordExchange::ScramOnly]);
    connect.verify_ca_requires_ca_file = true;
    connect.client_certificates = true;
    connect.encrypted_client_keys = false;
    c.connect = connect;
    c
}

/// `true` when a `search_path` value leaves `pg_catalog` first: not listed (PostgreSQL then
/// searches it first implicitly) or listed first. The driver's own queries are fully
/// qualified anyway; this keeps user statements from resolving built-in names to user
/// objects by default.
fn pg_catalog_first(value: &str) -> bool {
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
        None | Some(0) => true,
        Some(_) => false,
    }
}

/// `localhost` or a loopback IP address (the local end of an SSH tunnel).
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(|c| c == '[' || c == ']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn normalize(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Maps accepted properties to startup parameters; refuses unknown keys (never their values).
fn accepted_properties(
    properties: &std::collections::BTreeMap<String, PropertyValue>,
) -> DriverResult<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (key, value) in properties {
        let norm = normalize(key);
        let Some((_, name)) = ACCEPTED_PROPERTIES.iter().find(|(k, _)| *k == norm) else {
            return Err(DriverError::InvalidParams(format!(
                "property `{}` is not supported by the PostgreSQL driver; accepted: \
                 search_path, TimeZone, statement_timeout, lock_timeout, work_mem, \
                 client_min_messages",
                echo_property_key(key)
            )));
        };
        let v = value.expose();
        if v.contains('\0') {
            return Err(DriverError::InvalidParams(format!(
                "property `{name}` holds a NUL character"
            )));
        }
        if *name == "search_path" && !pg_catalog_first(v) {
            return Err(DriverError::InvalidParams(format!(
                "property `{name}` lists pg_catalog after another schema; the driver keeps \
                 pg_catalog first (leave it out, or list it first)"
            )));
        }
        out.push(((*name).to_owned(), v.to_owned()));
    }
    Ok(out)
}

#[async_trait::async_trait]
impl Driver for PgWireDriver {
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
        Ok(postgres_capabilities(server_version))
    }

    fn dialect(&self, engine: &EngineId) -> DriverResult<Arc<dyn Dialect>> {
        Self::check_engine(engine)?;
        Ok(Arc::new(PgDialect::new()))
    }

    async fn connect(&self, params: ConnectParams) -> DriverResult<Box<dyn Session>> {
        Ok(Box::new(self.open(params).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn capabilities_are_declared() {
        let d = PgWireDriver::new();
        let c = d.capabilities(&EngineId::POSTGRESQL, None).unwrap();
        assert_eq!(
            c.read_only,
            ReadOnlyMechanism::SessionSetting { liftable: true }
        );
        assert!(c.read_only.is_liftable());
        assert_eq!(c.cancel, CancelSupport::ServerSide);
        assert!(!c.retains_secret_after_connect);
        assert!(!c.multiple_result_sets);
        for key in ["krbsrvname", "target_session_attrs", "TargetSessionAttrs"] {
            assert!(c.is_reserved_property(key), "{key}");
        }
        assert!(!c.is_reserved_property("search_path"));
        assert!(d.capabilities(&EngineId::TIMESCALEDB, None).is_err());
        assert!(d.dialect(&EngineId::MYSQL).is_err());
        assert_eq!(d.engines(), &[EngineId::POSTGRESQL]);
    }

    #[test]
    fn properties_allowlist() {
        let mut p = BTreeMap::new();
        p.insert("search_path".into(), PropertyValue::Plain("app".into()));
        p.insert("TimeZone".into(), PropertyValue::Plain("UTC".into()));
        let out = accepted_properties(&p).unwrap();
        assert!(out.contains(&("search_path".into(), "app".into())));
        assert!(out.contains(&("TimeZone".into(), "UTC".into())));
        let mut bad = BTreeMap::new();
        bad.insert(
            "prepareThreshold".into(),
            PropertyValue::Plain("secret-v".into()),
        );
        let err = accepted_properties(&bad).unwrap_err().to_string();
        assert!(!err.contains("secret-v"));
        for (v, ok) in [
            ("app, public", true),
            ("pg_catalog, app", true),
            ("app, pg_catalog", false),
            ("app, \"pg_catalog\"", false),
            ("app, PG_CATALOG", false),
            ("\"$user\", public", true),
        ] {
            let mut sp = BTreeMap::new();
            sp.insert("search_path".into(), PropertyValue::Plain(v.into()));
            assert_eq!(accepted_properties(&sp).is_ok(), ok, "{v}");
        }
        assert!(is_loopback_host("localhost") && is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1") && !is_loopback_host("db.example.test"));
        let mut nul = BTreeMap::new();
        nul.insert("search_path".into(), PropertyValue::Plain("a\0b".into()));
        assert!(accepted_properties(&nul).is_err());
    }

    #[tokio::test]
    async fn connect_refuses_before_network() {
        let d = PgWireDriver::new();
        // Port 9 on a reserved address: nothing must be attempted for these.
        let base = || {
            let mut p = ConnectParams::new(EngineId::POSTGRESQL, Endpoint::tcp("192.0.2.1", 9));
            p.auth = Auth::password("nine", None);
            p.connect_timeout = std::time::Duration::from_millis(50);
            p
        };
        let mut p = base();
        p.properties.insert(
            "target_session_attrs".into(),
            PropertyValue::Plain("any".into()),
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
        p.engine = EngineId::QUESTDB;
        assert!(matches!(
            d.connect(p).await,
            Err(DriverError::InvalidParams(_))
        ));
        // M2: what the capabilities refuse, connect refuses before any traffic.
        let mut p = base();
        p.auth = Auth::token(None, ninesql_driver_api::Secret::new("tok-value".into()));
        let err = d.connect(p).await.err().unwrap();
        assert!(matches!(err, DriverError::InvalidParams(_)), "{err:?}");
        assert!(!err.to_string().contains("tok-value"));
        let mut p = base();
        p.properties
            .insert("fetch_size".into(), PropertyValue::Plain("10".into()));
        assert!(matches!(
            d.connect(p).await,
            Err(DriverError::InvalidParams(_))
        ));
        let mut p = base();
        p.tls.mode = TlsMode::VerifyCa;
        assert!(matches!(d.connect(p).await, Err(DriverError::Tls(_))));
        let mut p = base();
        let mut cc = ninesql_driver_api::ClientCertificate::new("/c.pem", "/k.pem");
        cc.key_passphrase = Some(ninesql_driver_api::Secret::new("kp".into()));
        p.tls.client_certificate = Some(cc);
        assert!(matches!(
            d.connect(p).await,
            Err(DriverError::InvalidParams(_))
        ));
    }

    /// Review L-a: a key normalization accepts is named by its canonical form; an unknown
    /// key is escaped. No control character reaches an error message.
    #[test]
    fn errors_never_echo_control_characters() {
        let mut p = BTreeMap::new();
        p.insert(
            "search\u{1b}[\n_path".to_owned(),
            PropertyValue::Plain("app, pg_catalog".into()),
        );
        let msg = accepted_properties(&p).unwrap_err().to_string();
        assert!(msg.contains("`search_path`"), "{msg}");
        assert!(!msg.chars().any(char::is_control), "{msg:?}");
        let mut p = BTreeMap::new();
        p.insert(
            "fetch\u{1b}[2J\r\nsize".to_owned(),
            PropertyValue::Plain("1".into()),
        );
        let msg = accepted_properties(&p).unwrap_err().to_string();
        assert!(!msg.chars().any(char::is_control), "{msg:?}");
        assert!(msg.contains("\\u{1b}"), "{msg}");
    }

    #[test]
    fn connect_capabilities_match_the_driver() {
        let c = postgres_capabilities(None).connect;
        for (_, name) in ACCEPTED_PROPERTIES {
            assert!(c.accepts_property(name), "{name}");
        }
        assert!(c.accepts_property("searchPath"));
        assert!(!c.accepts_property("fetch_size"));
        assert_eq!(c.auth, BTreeSet::from([AuthKind::Password]));
        assert!(c.accepts_password_exchange(PasswordExchange::ScramOnly));
        assert!(c.verify_ca_requires_ca_file);
        assert!(c.client_certificates && !c.encrypted_client_keys);
    }
}
